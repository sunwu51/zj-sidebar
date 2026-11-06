//! Git actions shared by the panel and the log window, modelled on IntelliJ IDEA's Git
//! UI: the branch popup (filterable local/remote branches, fetched in the background),
//! the per-branch and per-commit actions, and a runner that executes them one git
//! command at a time, with IDEA's recoveries (smart checkout, force delete).

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use zellij_tile::prelude::*;

use crate::commit::{CommitDlg, CommitOut};
use crate::git::{GitRaw, GitState};
use crate::gitlog::{self, Branch, OpKind};
use crate::sessions::now_secs;
use crate::ui::{self, Line, BOLD, CYAN, DIM, GREEN, MAGENTA, RED, REVERSE, YELLOW};
use crate::widgets::{plain, Item, Menu, Outcome, Prompt, PromptOutcome};

pub const GIT: &[&str] = &["git", "--no-optional-locks", "-c", "core.quotepath=false", "-c", "color.ui=false"];
/// Don't refetch more often than this when the branch popup is reopened.
const FETCH_EVERY: u64 = 30;

/// Run a read-only git command; the answer comes back as a `RunCommandResult` with `ctx`.
pub fn run_git(cwd: &str, args: &[&str], mut ctx: BTreeMap<String, String>) {
    let mut cmd: Vec<&str> = GIT.to_vec();
    cmd.extend_from_slice(args);
    ctx.insert("cwd".into(), cwd.into());
    run_command_with_env_variables_and_cwd(&cmd, quiet_env(), PathBuf::from(cwd), ctx);
}

/// Never wait for a password, an editor or a pager; messages in English so they can be
/// recognised.
fn quiet_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GIT_EDITOR".into(), "true".into()),
        ("GIT_PAGER".into(), "cat".into()),
        ("LC_ALL".into(), "C".into()),
    ])
}

/// A git command for the log window; always `--`-free, the caller adds paths.
pub fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// What to offer when a job fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Retry {
    None,
    /// a checkout refused because of local changes: (checkout args)
    Checkout(Vec<String>),
    /// `branch -d` refused because the branch isn't merged
    Delete(String),
}

/// One user action: git commands run in order, stopping at the first failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub label: String,
    pub steps: Vec<Vec<String>>,
    pub retry: Retry,
    /// submitted from the commit dialog: close it when done, keep it on failure
    pub commit: bool,
}

impl Job {
    pub fn new(label: impl Into<String>, steps: Vec<Vec<String>>) -> Self {
        Job { label: label.into(), steps, retry: Retry::None, commit: false }
    }
    pub fn one(label: impl Into<String>, step: &[&str]) -> Self {
        Self::new(label, vec![args(step)])
    }
    fn retry(mut self, r: Retry) -> Self {
        self.retry = r;
        self
    }
}

/// Actions the host mode carries out itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostAct {
    /// open the log window (optionally showing one branch)
    OpenLog(Option<String>),
    /// in the log window: show only this ref / range
    ShowLog(String),
    /// log window: move into the selected commit's files
    FocusFiles,
    /// log window file actions, by index into the selected commit's files
    DiffFile(usize),
    CompareWithLocal(usize),
    History(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    Nothing,
    Run(Job),
    /// ask first, then run
    Confirm { title: String, message: String, yes: String, job: Job },
    Host(HostAct),
    /// open the action menu of a branch (by name)
    BranchMenu(String),
    // global
    Commit,
    Fetch,
    PullMerge,
    PullRebase,
    Push,
    Stash,
    Unstash,
    NewBranch(Option<String>),
    CheckoutRevision,
    Abort(OpKind),
    Continue(OpKind),
    // branches
    Checkout(String),
    CheckoutRemote(String),
    CheckoutRebase(String),
    RebaseOnto(String),
    Merge(String),
    PushBranch(String),
    Rename(String),
    Delete(String),
    DeleteRemote(String),
    // commits
    CopyText(String),
    CheckoutCommit(String),
    BranchAt(String),
    TagAt(String),
    CherryPick { sha: String, merge: bool },
    Revert { sha: String, merge: bool },
    ResetMenu(String),
    Reset { mode: &'static str, sha: String },
    UndoCommit,
    EditMessage { subject: String, body: String },
    InteractiveRebase(Option<String>),
    PushUpTo(String),
}

#[derive(Clone, Debug)]
enum Ask {
    NewBranch(Option<String>),
    Rename(String),
    CheckoutRevision,
    BranchAt(String),
    TagAt(String),
    EditMessage(String),
}

/// Result of feeding input to the controller.
pub enum Out {
    /// not for us
    Unhandled,
    Handled,
    Host(HostAct),
}

#[derive(Default)]
pub struct Picker {
    pub filter: String,
    sel: usize,
    scroll: usize,
    /// screen rows of the last render -> index into `rows()`
    clicks: Vec<(usize, usize)>,
    area: (usize, usize, usize, usize),
}

enum PRow {
    Act(Act, String, &'static str),
    Head(String),
    Branch(usize),
}

impl PRow {
    fn selectable(&self) -> bool {
        !matches!(self, PRow::Head(_))
    }
}

#[derive(Default)]
pub struct GitCtl {
    pub cwd: String,
    pub branches: Vec<Branch>,
    pub remotes: Vec<String>,
    pub state: Option<OpKind>,
    pub head: Option<String>,
    pub picker: Option<Picker>,
    menus: Vec<Menu<Act>>,
    prompt: Option<Prompt<Ask>>,
    /// the running job, its current step and its id
    job: Option<(Job, usize, u64)>,
    seq: u64,
    pub status: Option<(String, bool)>,
    pub fetching: bool,
    /// why the last background fetch failed
    pub fetch_error: Option<String>,
    last_fetch: u64,
    /// an action changed the repository; the host should reload what it shows
    pub dirty: bool,
    /// the panel offers "Show Git Log" in the branch popup
    pub in_panel: bool,
    commit: Option<CommitDlg>,
    /// the message of a commit dialog closed without committing
    draft: String,
}

impl GitCtl {
    pub fn set_cwd(&mut self, cwd: &str) {
        if self.cwd != cwd {
            *self = GitCtl { cwd: cwd.to_string(), in_panel: self.in_panel, ..Default::default() };
        }
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Something is open that takes the keyboard.
    pub fn modal(&self) -> bool {
        self.prompt.is_some() || !self.menus.is_empty() || self.picker.is_some() || self.commit.is_some()
    }

    pub fn current(&self) -> Option<&Branch> {
        self.branches.iter().find(|b| b.head && !b.remote)
    }

    pub fn current_name(&self) -> String {
        self.current().map(|b| b.name.clone()).unwrap_or_else(|| "HEAD".into())
    }

    pub fn locals(&self) -> HashSet<String> {
        self.branches.iter().filter(|b| !b.remote).map(|b| b.name.clone()).collect()
    }

    fn info(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), false));
    }
    fn error(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), true));
    }

    // -- reading the repository --------------------------------------------------

    pub fn refresh(&mut self) {
        if self.cwd.is_empty() {
            return;
        }
        let ctx = |k: &str| BTreeMap::from([("gitctl".to_string(), k.to_string())]);
        run_git(&self.cwd, &["for-each-ref", gitlog::BRANCH_FORMAT, "refs/heads", "refs/remotes"], ctx("branches"));
        run_git(&self.cwd, &["remote"], ctx("remotes"));
        run_git(&self.cwd, &["rev-parse", "HEAD"], ctx("head"));
        run_git(&self.cwd, &["status", "--no-ahead-behind", "-uno"], ctx("state"));
    }

    pub fn fetch_in_background(&mut self, force: bool) {
        let now = now_secs();
        if self.fetching || (!force && now.saturating_sub(self.last_fetch) < FETCH_EVERY) {
            return;
        }
        self.fetching = true;
        self.last_fetch = now;
        let ctx = BTreeMap::from([("gitctl".to_string(), "fetch".to_string())]);
        run_git(&self.cwd, &["fetch", "--all", "--prune", "--quiet"], ctx);
    }

    /// Feed a command result; true if it was one of ours.
    pub fn on_result(&mut self, ctx: &BTreeMap<String, String>, code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> bool {
        if ctx.get("cwd").map(String::as_str) != Some(self.cwd.as_str()) {
            return false;
        }
        let ok = code == Some(0);
        let out = String::from_utf8_lossy(stdout);
        if let Some(kind) = ctx.get("gitctl") {
            match kind.as_str() {
                "branches" if ok => self.branches = gitlog::parse_branches(&out),
                "remotes" if ok => self.remotes = out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect(),
                "head" => self.head = ok.then(|| out.trim().to_string()),
                "state" => self.state = gitlog::repo_state(&out),
                "commit_files" => {
                    if let Some(d) = self.commit.as_mut() {
                        let raw = GitRaw { status: Some(Ok(out.into_owned())), ..Default::default() };
                        match raw.state() {
                            GitState::Repo(info) => d.set_files(info.files),
                            _ => d.set_files(Vec::new()),
                        }
                    }
                }
                "head_msg" => {
                    if let Some(d) = self.commit.as_mut() {
                        d.head_msg = ok.then(|| out.trim_end().to_string());
                    }
                }
                "fetch" => {
                    self.fetching = false;
                    self.fetch_error = (!ok).then(|| first_error(&String::from_utf8_lossy(stderr)));
                    if let Some(e) = self.fetch_error.clone() {
                        self.error(format!("fetch failed: {e}"));
                    }
                    self.refresh();
                    self.dirty = true;
                }
                _ => {}
            }
            return true;
        }
        let Some(id) = ctx.get("gitop").and_then(|s| s.parse::<u64>().ok()) else { return false };
        let Some((job, step, seq)) = self.job.take() else { return true };
        if seq != id {
            self.job = Some((job, step, seq));
            return true;
        }
        if ok {
            if step + 1 < job.steps.len() {
                self.start_step(job, step + 1);
            } else {
                let tail = summary(&out, &String::from_utf8_lossy(stderr));
                self.info(if tail.is_empty() { format!("✓ {}", job.label) } else { format!("✓ {} · {tail}", job.label) });
                if job.commit {
                    self.commit = None;
                    self.draft.clear();
                }
                self.finish();
            }
            return true;
        }
        let err = String::from_utf8_lossy(stderr).into_owned();
        let err = if err.trim().is_empty() { out.into_owned() } else { err };
        self.finish();
        if job.commit {
            // keep the dialog (and the message) for another try
            if let Some(d) = self.commit.as_mut() {
                d.busy = false;
                d.error = Some(format!("{} failed", job.label));
            }
        }
        self.failed(&job, step, &err);
        true
    }

    fn finish(&mut self) {
        self.dirty = true;
        self.refresh();
    }

    fn failed(&mut self, job: &Job, step: usize, err: &str) {
        let line = first_error(err);
        self.error(format!("✗ {}: {line}", job.label));
        match &job.retry {
            Retry::Checkout(co) if step == 0 && (err.contains("would be overwritten") || err.contains("commit your changes or stash")) => {
                let mut smart = vec![args(&["stash", "push", "--include-untracked", "-m", "zj-sidebar smart checkout"])];
                smart.push(co.clone());
                smart.push(args(&["stash", "pop"]));
                let mut force = co.clone();
                force.insert(1, "--force".into());
                let menu = Menu::new(
                    "Checkout",
                    vec![
                        Item::new("Smart Checkout (stash → checkout → unstash)", Act::Run(Job::new(format!("smart {}", job.label), smart))).key('s'),
                        Item::new("Force Checkout (discard local changes)", Act::Run(Job::new(format!("force {}", job.label), vec![force]))).key('f'),
                        Item::new("Cancel", Act::Nothing).key('c'),
                    ],
                )
                .message("Your local changes would be overwritten by checkout.")
                .message(line.clone());
                self.open_centered(menu);
            }
            Retry::Delete(b) if err.contains("not fully merged") => {
                let menu = Menu::new(
                    "Delete Branch",
                    vec![
                        Item::new("Delete anyway", Act::Run(Job::one(format!("delete {b}"), &["branch", "-D", b]))).key('d'),
                        Item::new("Cancel", Act::Nothing).key('c'),
                    ],
                )
                .message(format!("Branch '{b}' is not fully merged."));
                self.open_centered(menu);
            }
            _ => {
                // anything else: show git's whole message until acknowledged
                let lines: Vec<String> =
                    err.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).take(12).map(String::from).collect();
                self.alert(&format!("{} failed", job.label), if lines.is_empty() { vec![line] } else { lines });
            }
        }
    }

    fn start_step(&mut self, job: Job, step: usize) {
        let mut cmd: Vec<&str> = GIT.to_vec();
        cmd.extend(job.steps[step].iter().map(String::as_str));
        let ctx = BTreeMap::from([
            ("cwd".to_string(), self.cwd.clone()),
            ("gitop".to_string(), self.seq.to_string()),
        ]);
        run_command_with_env_variables_and_cwd(&cmd, quiet_env(), PathBuf::from(&self.cwd), ctx);
        self.info(format!("{}…", job.label));
        self.job = Some((job, step, self.seq));
    }

    pub fn run(&mut self, job: Job) {
        if let Some((j, _, _)) = &self.job {
            let l = j.label.clone();
            self.error(format!("busy: {l}…"));
            return;
        }
        if job.steps.is_empty() {
            return;
        }
        self.seq += 1;
        self.start_step(job, 0);
    }

    // -- actions -----------------------------------------------------------------

    fn ask(&mut self, title: &str, label: &str, initial: &str, ask: Ask) {
        self.prompt = Some(Prompt::new(title, label, initial, ask));
    }

    /// An error or notice the user has to acknowledge.
    fn alert(&mut self, title: &str, lines: Vec<String>) {
        let mut menu = Menu::new(title, vec![Item::new("OK", Act::Nothing).key('o')]);
        menu.message = lines;
        self.open_centered(menu);
    }

    /// Ask before pushing `b`, saying what goes where.
    fn confirm_push(&mut self, b: &Branch) {
        match self.upstream_of(b) {
            Some((r, n)) => {
                let msg = if b.ahead > 0 {
                    format!("Push {} commit{} from '{}' to '{r}/{n}'?", b.ahead, plural(b.ahead), b.name)
                } else {
                    format!("'{}' has nothing new for '{r}/{n}' as of the last fetch. Push anyway?", b.name)
                };
                let refspec = format!("{}:{n}", b.name);
                self.confirm("Push", msg, "Push", Job::one(format!("push {}", b.name), &["push", &r, &refspec]));
            }
            None => match self.default_remote() {
                Some(r) => {
                    let msg = format!("Push '{}' to '{r}' as a new branch and track it?", b.name);
                    self.confirm("Push", msg, "Push", Job::one(format!("push {} → {r}", b.name), &["push", "-u", &r, &b.name]));
                }
                None => self.alert("Push", vec!["No remote is configured for this repository.".into()]),
            },
        }
    }

    fn confirm(&mut self, title: &str, message: String, yes: &str, job: Job) {
        let menu = Menu::new(title, vec![Item::new(yes, Act::Run(job)).key('y'), Item::new("Cancel", Act::Nothing).key('n')])
            .message(message);
        self.open_centered(menu);
    }

    fn open_centered(&mut self, mut menu: Menu<Act>) {
        menu.centered = true;
        self.menus = vec![menu];
    }

    pub fn open_menu(&mut self, menu: Menu<Act>) {
        self.menus = vec![menu];
    }

    /// The commit dialog over the repository's current changes.
    pub fn open_commit(&mut self) {
        if self.commit.is_some() || self.cwd.is_empty() {
            return;
        }
        self.picker = None;
        self.menus.clear();
        let upstream = self.current().and_then(|b| b.upstream.clone());
        self.commit = Some(CommitDlg::new(self.current_name(), std::mem::take(&mut self.draft), upstream));
        let ctx = |k: &str| BTreeMap::from([("gitctl".to_string(), k.to_string())]);
        run_git(&self.cwd, &["status", "--porcelain=v1", "-uall"], ctx("commit_files"));
        run_git(&self.cwd, &["log", "-1", "--format=%B"], ctx("head_msg"));
    }

    /// Commit the checked files: stage them (new files, deletions too), commit exactly
    /// those paths, then push if asked.
    fn submit_commit(&mut self) {
        let Some(d) = self.commit.as_ref() else { return };
        let msg = d.msg.trim().to_string();
        let mut paths: Vec<String> = Vec::new();
        // only what has unstaged changes (or is new) needs `git add`; a staged
        // deletion, for one, is no longer a path `git add` knows
        let mut to_add: Vec<String> = Vec::new();
        for f in d.selected_files() {
            if let Some(o) = &f.old {
                paths.push(o.clone());
            }
            paths.push(f.path.clone());
            if f.xy == "??" || f.xy.as_bytes().get(1).is_some_and(|y| *y != b' ') {
                to_add.push(f.path.clone());
            }
        }
        let (amend, push) = (d.amend, d.push);
        let err = if msg.is_empty() && !amend {
            Some("write a commit message first")
        } else if paths.is_empty() && !amend {
            Some("select at least one file")
        } else if self.busy() {
            Some("another git command is still running")
        } else {
            None
        };
        if let Some(e) = err {
            if let Some(d) = self.commit.as_mut() {
                d.error = Some(e.to_string());
            }
            return;
        }
        let mut steps = Vec::new();
        if !to_add.is_empty() {
            let mut add = args(&["add", "-A", "--"]);
            add.extend(to_add);
            steps.push(add);
        }
        let mut c = args(&["commit"]);
        if amend {
            c.push("--amend".into());
        }
        if msg.is_empty() {
            c.push("--no-edit".into());
        } else {
            c.push("-m".into());
            c.push(msg);
        }
        if paths.is_empty() {
            // amend the message only
            c.push("--only".into());
        } else {
            c.push("--".into());
            c.extend(paths);
        }
        steps.push(c);
        let branch = self.current().cloned();
        if push {
            match branch.as_ref() {
                Some(b) if b.upstream.is_some() => steps.push(args(&["push"])),
                Some(b) => match self.default_remote() {
                    Some(r) => steps.push(args(&["push", "-u", &r, &b.name])),
                    None => {
                        if let Some(d) = self.commit.as_mut() {
                            d.error = Some("no remote to push to".into());
                        }
                        return;
                    }
                },
                None => {
                    if let Some(d) = self.commit.as_mut() {
                        d.error = Some("not on a branch: cannot push".into());
                    }
                    return;
                }
            }
        }
        let label = match (amend, push) {
            (true, true) => "amend & push",
            (true, false) => "amend commit",
            (false, true) => "commit & push",
            (false, false) => "commit",
        };
        let mut job = Job::new(label, steps);
        job.commit = true;
        if let Some(d) = self.commit.as_mut() {
            d.busy = true;
            d.error = None;
        }
        self.run(job);
    }

    pub fn open_picker(&mut self) {
        self.picker = Some(Picker::default());
        self.menus.clear();
        self.refresh();
        self.fetch_in_background(false);
    }

    pub fn close_all(&mut self) {
        self.picker = None;
        self.menus.clear();
        self.prompt = None;
    }

    fn upstream_of(&self, b: &Branch) -> Option<(String, String)> {
        let up = b.upstream.as_ref()?;
        let (r, n) = gitlog::split_remote(up, &self.remotes)?;
        Some((r.to_string(), n.to_string()))
    }

    fn default_remote(&self) -> Option<String> {
        self.remotes.iter().find(|r| *r == "origin").or(self.remotes.first()).cloned()
    }

    /// Carry out a menu choice. Returns what the host has to do, if anything.
    pub fn perform(&mut self, act: Act) -> Option<HostAct> {
        let cur = self.current_name();
        match act {
            // OK / Cancel: just close the dialog (a branch popup under it stays)
            Act::Nothing => self.menus.clear(),
            Act::Host(h) => {
                self.close_all();
                return Some(h);
            }
            Act::Run(job) => {
                self.close_all();
                self.run(job);
            }
            Act::BranchMenu(name) => self.branch_menu(&name),
            Act::Commit => self.open_commit(),
            Act::Fetch => {
                self.close_all();
                self.run(Job::one("fetch", &["fetch", "--all", "--prune"]));
            }
            Act::PullMerge | Act::PullRebase => {
                let rebase = act_is_rebase(&act);
                let Some(b) = self.current().cloned() else {
                    self.alert("Update Project", vec!["Not on a branch (detached HEAD).".into()]);
                    return None;
                };
                let Some(up) = b.upstream.clone() else {
                    self.alert("Update Project", vec![format!("'{}' has no upstream branch to pull from.", b.name)]);
                    return None;
                };
                let how = if rebase { "rebasing" } else { "merging" };
                let mut msg = format!("Pull '{up}' into '{}' by {how}?", b.name);
                if b.behind > 0 {
                    msg.push_str(&format!(" {} incoming commit{} as of the last fetch.", b.behind, plural(b.behind)));
                }
                let job = if rebase {
                    Job::one("pull (rebase)", &["pull", "--rebase"])
                } else {
                    Job::one("pull (merge)", &["pull", "--no-rebase", "--no-edit"])
                };
                self.confirm("Update Project", msg, "Pull", job);
            }
            Act::Push => match self.current().cloned() {
                None => self.alert("Push", vec!["Not on a branch (detached HEAD).".into()]),
                Some(b) => self.confirm_push(&b),
            },
            Act::Stash => {
                let job = Job::one("stash", &["stash", "push", "--include-untracked"]);
                self.confirm("Stash Changes", "Stash all local changes, including untracked files? The working tree becomes clean; Unstash brings them back.".into(), "Stash", job);
            }
            Act::Unstash => {
                let job = Job::one("unstash", &["stash", "pop"]);
                self.confirm("Unstash", "Apply the latest stash (stash@{0}) to the working tree and drop it?".into(), "Unstash", job);
            }
            Act::NewBranch(from) => {
                let title = match &from {
                    Some(f) => format!("New Branch from '{f}'"),
                    None => format!("New Branch from '{cur}'"),
                };
                self.menus.clear();
                self.ask(&title, "name (it is checked out)", "", Ask::NewBranch(from));
            }
            Act::CheckoutRevision => {
                self.menus.clear();
                self.ask("Checkout Tag or Revision", "tag, commit or any revision", "", Ask::CheckoutRevision);
            }
            Act::Abort(op) => {
                let job = Job::one(format!("abort {}", op.name()), &[op.name(), "--abort"]);
                self.confirm(&format!("Abort {}", op.name()), format!("Abort the {} in progress?", op.name()), "Abort", job);
            }
            Act::Continue(op) => {
                self.close_all();
                let job = match op {
                    OpKind::Merge => Job::one("continue merge", &["commit", "--no-edit"]),
                    _ => Job::one(format!("continue {}", op.name()), &[op.name(), "--continue"]),
                };
                self.run(job);
            }
            Act::Checkout(b) => {
                self.close_all();
                let co = args(&["checkout", &b]);
                self.run(Job::new(format!("checkout {b}"), vec![co.clone()]).retry(Retry::Checkout(co)));
            }
            Act::CheckoutRemote(r) => {
                self.close_all();
                let Some((_, name)) = gitlog::split_remote(&r, &self.remotes) else { return None };
                let name = name.to_string();
                let local = self.branches.iter().find(|b| !b.remote && b.name == name).cloned();
                let job = match local {
                    // the local branch already tracks it: switch and fast-forward
                    Some(l) if l.upstream.as_deref() == Some(r.as_str()) => {
                        let co = args(&["checkout", &name]);
                        Job::new(format!("checkout {name}"), vec![co.clone(), args(&["merge", "--ff-only", &r])]).retry(Retry::Checkout(co))
                    }
                    Some(_) => {
                        self.info(format!("local '{name}' exists; checking it out"));
                        let co = args(&["checkout", &name]);
                        Job::new(format!("checkout {name}"), vec![co.clone()]).retry(Retry::Checkout(co))
                    }
                    None => {
                        let co = args(&["checkout", "-b", &name, "--track", &r]);
                        Job::new(format!("checkout {r} as {name}"), vec![co.clone()]).retry(Retry::Checkout(co))
                    }
                };
                self.run(job);
            }
            Act::CheckoutRebase(b) => {
                let job = Job::one(format!("checkout {b} & rebase onto {cur}"), &["rebase", &cur, &b]);
                self.confirm("Checkout and Rebase", format!("Check out '{b}' and rebase it onto '{cur}'?"), "Rebase", job);
            }
            Act::RebaseOnto(b) => {
                let job = Job::one(format!("rebase {cur} onto {b}"), &["rebase", &b]);
                self.confirm("Rebase", format!("Rebase '{cur}' onto '{b}'? Its commits are rewritten on top of '{b}'."), "Rebase", job);
            }
            Act::Merge(b) => {
                let job = Job::one(format!("merge {b} into {cur}"), &["merge", "--no-edit", &b]);
                self.confirm("Merge", format!("Merge '{b}' into '{cur}'?"), "Merge", job);
            }
            Act::PushBranch(b) => {
                if let Some(br) = self.branches.iter().find(|x| !x.remote && x.name == b).cloned() {
                    self.confirm_push(&br);
                }
            }
            Act::Rename(b) => {
                self.menus.clear();
                self.ask(&format!("Rename Branch '{b}'"), "new name", &b.clone(), Ask::Rename(b));
            }
            Act::Delete(b) => {
                let job = Job::one(format!("delete {b}"), &["branch", "-d", &b]).retry(Retry::Delete(b.clone()));
                self.confirm("Delete Branch", format!("Delete the local branch '{b}'?"), "Delete", job);
            }
            Act::Confirm { title, message, yes, job } => self.confirm(&title, message, &yes, job),
            Act::DeleteRemote(r) => {
                let Some((remote, name)) = gitlog::split_remote(&r, &self.remotes) else { return None };
                let job = Job::one(format!("delete {r}"), &["push", remote, "--delete", name]);
                self.confirm("Delete Remote Branch", format!("Delete '{name}' on '{remote}'?"), "Delete", job);
            }
            Act::CopyText(t) => {
                self.close_all();
                copy_to_clipboard(t.clone());
                self.info(format!("copied {}", ui::truncate(&t, 40)));
            }
            Act::CheckoutCommit(sha) => {
                self.close_all();
                let co = args(&["checkout", "--detach", &sha]);
                self.run(Job::new(format!("checkout {}", short(&sha)), vec![co.clone()]).retry(Retry::Checkout(co)));
            }
            Act::BranchAt(sha) => {
                self.menus.clear();
                self.ask(&format!("New Branch at {}", short(&sha)), "name (it is checked out)", "", Ask::BranchAt(sha));
            }
            Act::TagAt(sha) => {
                self.menus.clear();
                self.ask(&format!("New Tag at {}", short(&sha)), "tag name", "", Ask::TagAt(sha));
            }
            Act::CherryPick { sha, merge } => {
                let mut a = vec!["cherry-pick"];
                if merge {
                    a.extend(["-m", "1"]);
                }
                a.push(&sha);
                let job = Job::one(format!("cherry-pick {}", short(&sha)), &a);
                self.confirm("Cherry-Pick", format!("Apply {} onto '{cur}' as a new commit?", short(&sha)), "Cherry-Pick", job);
            }
            Act::Revert { sha, merge } => {
                let mut a = vec!["revert", "--no-edit"];
                if merge {
                    a.extend(["-m", "1"]);
                }
                a.push(&sha);
                let job = Job::one(format!("revert {}", short(&sha)), &a);
                self.confirm("Revert Commit", format!("Create a commit on '{cur}' that undoes {}?", short(&sha)), "Revert", job);
            }
            Act::ResetMenu(sha) => {
                let anchor = self.submenu_anchor();
                let s = short(&sha).to_string();
                let items = vec![
                    Item::new("Soft — keep changes staged", Act::Reset { mode: "--soft", sha: sha.clone() }).key('s'),
                    Item::new("Mixed — keep changes unstaged", Act::Reset { mode: "--mixed", sha: sha.clone() }).key('m'),
                    Item::new("Hard — discard all changes", Act::Reset { mode: "--hard", sha: sha.clone() }).key('h'),
                    Item::new("Keep — keep local changes", Act::Reset { mode: "--keep", sha }).key('k'),
                ];
                self.menus.push(Menu::new(format!("Reset {cur} to {s}"), items).at(anchor.0, anchor.1));
            }
            Act::Reset { mode, sha } => {
                let job = Job::one(format!("reset {mode} {}", short(&sha)), &["reset", mode, &sha]);
                let s = short(&sha);
                let (title, msg) = match mode {
                    "--hard" => ("Hard Reset", format!("Reset '{cur}' to {s} and discard all local changes?")),
                    "--soft" => ("Soft Reset", format!("Reset '{cur}' to {s}? The undone commits' changes stay staged.")),
                    "--keep" => ("Keep Reset", format!("Reset '{cur}' to {s}, keeping your uncommitted changes?")),
                    _ => ("Mixed Reset", format!("Reset '{cur}' to {s}? The undone commits' changes stay in the working tree, unstaged.")),
                };
                self.confirm(title, msg, &format!("Reset {mode}"), job);
            }
            Act::UndoCommit => {
                let job = Job::one("undo last commit", &["reset", "--soft", "HEAD~1"]);
                self.confirm("Undo Commit", format!("Undo the last commit of '{cur}'? Its changes stay staged."), "Undo", job);
            }
            Act::EditMessage { subject, body } => {
                self.menus.clear();
                let label = if body.is_empty() { "subject" } else { "subject (the body is kept)" };
                self.ask("Edit Commit Message (HEAD)", label, &subject, Ask::EditMessage(body));
            }
            Act::InteractiveRebase(base) => {
                self.close_all();
                let mut a = args(&["rebase", "-i"]);
                match base {
                    Some(b) => a.push(b),
                    None => a.push("--root".into()),
                }
                open_command_pane_floating(
                    CommandToRun { path: "git".into(), args: a, cwd: Some(PathBuf::from(&self.cwd)) },
                    FloatingPaneCoordinates::new(Some("8%".into()), Some("8%".into()), Some("84%".into()), Some("80%".into()), None, None),
                    BTreeMap::new(),
                );
                self.info("interactive rebase opened in a floating pane");
            }
            Act::PushUpTo(sha) => match self.current().cloned().and_then(|b| self.upstream_of(&b)) {
                Some((r, n)) => {
                    let job = Job::one(format!("push up to {}", short(&sha)), &["push", &r, &format!("{sha}:refs/heads/{n}")]);
                    self.confirm("Push", format!("Push '{cur}' up to {} to '{r}/{n}'?", short(&sha)), "Push", job);
                }
                None => self.alert("Push", vec![format!("'{cur}' has no upstream branch.")]),
            },
        }
        None
    }

    fn submit(&mut self, ask: Ask, text: String) -> Result<(), String> {
        if text.is_empty() {
            return Err("cannot be empty".into());
        }
        let no_space = |t: &str| if t.chars().any(char::is_whitespace) { Err("no spaces allowed".to_string()) } else { Ok(()) };
        let job = match ask {
            Ask::NewBranch(from) => {
                no_space(&text)?;
                let mut a = vec!["checkout", "-b", text.as_str()];
                if let Some(f) = &from {
                    // a new branch from a remote branch tracks it, as in IDEA
                    a.push(f);
                }
                Job::one(format!("new branch {text}"), &a)
            }
            Ask::Rename(old) => {
                no_space(&text)?;
                Job::one(format!("rename {old} → {text}"), &["branch", "-m", &old, &text])
            }
            Ask::CheckoutRevision => {
                let co = args(&["checkout", &text]);
                Job::new(format!("checkout {text}"), vec![co.clone()]).retry(Retry::Checkout(co))
            }
            Ask::BranchAt(sha) => {
                no_space(&text)?;
                Job::one(format!("new branch {text}"), &["checkout", "-b", &text, &sha])
            }
            Ask::TagAt(sha) => {
                no_space(&text)?;
                Job::one(format!("tag {text}"), &["tag", &text, &sha])
            }
            Ask::EditMessage(body) => {
                let mut a = vec!["commit", "--amend", "--only", "-m", text.as_str()];
                if !body.is_empty() {
                    a.extend(["-m", body.as_str()]);
                }
                Job::one("edit commit message", &a)
            }
        };
        self.close_all();
        self.run(job);
        Ok(())
    }

    fn branch_menu(&mut self, name: &str) {
        let Some(b) = self.branches.iter().find(|b| b.name == name).cloned() else { return };
        let cur = self.current_name();
        let n = ui::truncate(name, 28);
        let c = ui::truncate(&cur, 28);
        let mut items: Vec<Item<Act>> = Vec::new();
        if b.remote {
            items.push(Item::new("Checkout", Act::CheckoutRemote(b.name.clone())).key('c'));
            items.push(Item::new(format!("New Branch from '{n}'…"), Act::NewBranch(Some(b.name.clone()))).key('n'));
            items.push(Item::new(format!("Checkout and Rebase onto '{c}'"), Act::CheckoutRebase(b.name.clone())));
            items.push(Item::sep());
            items.push(Item::new("Show in Log", Act::Host(HostAct::ShowLog(b.name.clone()))).key('l'));
            items.push(Item::new(format!("Compare with '{c}'"), Act::Host(HostAct::ShowLog(format!("{cur}..{}", b.name)))));
            items.push(Item::sep());
            items.push(Item::new(format!("Rebase '{c}' onto '{n}'"), Act::RebaseOnto(b.name.clone())).key('r'));
            items.push(Item::new(format!("Merge '{n}' into '{c}'"), Act::Merge(b.name.clone())).key('m'));
            items.push(Item::sep());
            items.push(Item::new("Delete", Act::DeleteRemote(b.name.clone())).key('d'));
        } else if b.head {
            items.push(Item::new(format!("New Branch from '{n}'…"), Act::NewBranch(None)).key('n'));
            items.push(Item::new("Show in Log", Act::Host(HostAct::ShowLog(b.name.clone()))).key('l'));
            items.push(Item::sep());
            items.push(Item::new("Update (pull, merge)", Act::PullMerge).key('u').enabled(b.upstream.is_some()));
            items.push(Item::new("Update (pull, rebase)", Act::PullRebase).enabled(b.upstream.is_some()));
            items.push(Item::new("Push", Act::Push).key('p'));
            items.push(Item::sep());
            items.push(Item::new("Rename…", Act::Rename(b.name.clone())));
        } else {
            items.push(Item::new("Checkout", Act::Checkout(b.name.clone())).key('c'));
            items.push(Item::new(format!("New Branch from '{n}'…"), Act::NewBranch(Some(b.name.clone()))).key('n'));
            items.push(Item::new(format!("Checkout and Rebase onto '{c}'"), Act::CheckoutRebase(b.name.clone())));
            items.push(Item::sep());
            items.push(Item::new("Show in Log", Act::Host(HostAct::ShowLog(b.name.clone()))).key('l'));
            items.push(Item::new(format!("Compare with '{c}'"), Act::Host(HostAct::ShowLog(format!("{cur}..{}", b.name)))));
            items.push(Item::sep());
            items.push(Item::new(format!("Rebase '{c}' onto '{n}'"), Act::RebaseOnto(b.name.clone())).key('r'));
            items.push(Item::new(format!("Merge '{n}' into '{c}'"), Act::Merge(b.name.clone())).key('m'));
            items.push(Item::sep());
            items.push(Item::new(format!("Push '{n}'"), Act::PushBranch(b.name.clone())).key('p'));
            items.push(Item::new("Rename…", Act::Rename(b.name.clone())));
            items.push(Item::new("Delete", Act::Delete(b.name.clone())).key('d'));
        }
        if self.in_panel {
            // in the panel, "Show in Log" opens the log window on that branch
            for it in &mut items {
                if let Some(Act::Host(HostAct::ShowLog(r))) = &it.act {
                    it.act = Some(Act::Host(HostAct::OpenLog(Some(r.clone()))));
                }
            }
        }
        let menu = Menu::new(format!("Branch '{n}'"), items);
        if self.picker.is_none() && !self.menus.is_empty() {
            // opened from a commit's menu: a submenu
            let (x, y) = self.submenu_anchor();
            self.menus.push(menu.at(x, y));
        } else {
            let (x, y) = self.picker_anchor();
            self.menus = vec![menu.at(x, y)];
        }
    }

    /// Next to the selected row of the top menu.
    fn submenu_anchor(&self) -> (usize, usize) {
        match self.menus.last() {
            Some(m) => {
                let (x, y) = m.origin();
                (x + 6, y + 1 + m.sel)
            }
            None => (2, 2),
        }
    }

    fn picker_anchor(&self) -> (usize, usize) {
        let Some(p) = &self.picker else { return (2, 2) };
        let (x, y, w, _) = p.area;
        let row = p.clicks.iter().find(|(_, i)| *i == p.sel).map(|(l, _)| *l).unwrap_or(y);
        if self.in_panel { (x + 2, row + 1) } else { (x + w.min(40), row) }
    }

    // -- the branch popup ----------------------------------------------------------

    fn picker_rows(&self) -> Vec<PRow> {
        let filter = self.picker.as_ref().map(|p| p.filter.to_lowercase()).unwrap_or_default();
        let mut rows = Vec::new();
        if filter.is_empty() {
            if let Some(op) = self.state {
                rows.push(PRow::Act(Act::Continue(op), format!("Continue {}", op.name()), "▶"));
                rows.push(PRow::Act(Act::Abort(op), format!("Abort {}", op.name()), "■"));
            }
            rows.push(PRow::Act(Act::Commit, "Commit…".into(), "✓"));
            rows.push(PRow::Act(Act::NewBranch(None), "New Branch…".into(), "+"));
            rows.push(PRow::Act(Act::CheckoutRevision, "Checkout Tag or Revision…".into(), "⎇"));
            rows.push(PRow::Act(Act::PullMerge, "Update Project (pull)".into(), "↓"));
            rows.push(PRow::Act(Act::Push, "Push…".into(), "↑"));
            rows.push(PRow::Act(Act::Fetch, "Fetch".into(), "⟳"));
            rows.push(PRow::Act(Act::Stash, "Stash Changes".into(), "≡"));
            rows.push(PRow::Act(Act::Unstash, "Unstash (pop)".into(), "≡"));
            if self.in_panel {
                rows.push(PRow::Act(Act::Host(HostAct::OpenLog(None)), "Show Git Log".into(), "☰"));
            }
        }
        let matches = |b: &Branch| filter.is_empty() || b.name.to_lowercase().contains(&filter);
        let local: Vec<usize> = (0..self.branches.len()).filter(|&i| !self.branches[i].remote && matches(&self.branches[i])).collect();
        let remote: Vec<usize> = (0..self.branches.len()).filter(|&i| self.branches[i].remote && matches(&self.branches[i])).collect();
        if !local.is_empty() {
            rows.push(PRow::Head("Local".into()));
            rows.extend(local.into_iter().map(PRow::Branch));
        }
        if !remote.is_empty() || self.fetching {
            rows.push(PRow::Head(if self.fetching { "Remote  (fetching…)".into() } else { "Remote".into() }));
            rows.extend(remote.into_iter().map(PRow::Branch));
        }
        rows
    }

    fn picker_step(&mut self, delta: isize) {
        let rows = self.picker_rows();
        let Some(p) = self.picker.as_mut() else { return };
        if !rows.iter().any(PRow::selectable) {
            return;
        }
        let n = rows.len() as isize;
        let mut i = (p.sel as isize).clamp(0, n - 1);
        let mut moved = 0;
        loop {
            let next = i + delta.signum();
            if next < 0 || next >= n {
                break;
            }
            i = next;
            if rows[i as usize].selectable() {
                moved += 1;
                p.sel = i as usize;
                if moved >= delta.unsigned_abs() {
                    break;
                }
            }
        }
    }

    fn picker_fix_sel(&mut self) {
        let rows = self.picker_rows();
        let Some(p) = self.picker.as_mut() else { return };
        if p.sel >= rows.len() || !rows[p.sel].selectable() {
            p.sel = rows.iter().position(PRow::selectable).unwrap_or(0);
        }
    }

    fn picker_pick(&mut self, i: usize) -> Option<HostAct> {
        let rows = self.picker_rows();
        match rows.into_iter().nth(i)? {
            PRow::Act(a, _, _) => self.perform(a),
            PRow::Branch(b) => {
                let name = self.branches[b].name.clone();
                self.branch_menu(&name);
                None
            }
            PRow::Head(_) => None,
        }
    }

    // -- input ---------------------------------------------------------------------

    fn host(h: Option<HostAct>) -> Out {
        match h {
            Some(h) => Out::Host(h),
            None => Out::Handled,
        }
    }

    pub fn key(&mut self, key: &KeyWithModifier) -> Out {
        if let Some(p) = self.prompt.as_mut() {
            match p.key(key) {
                PromptOutcome::Handled => {}
                PromptOutcome::Cancel => self.prompt = None,
                PromptOutcome::Submit(text) => {
                    let ask = p.act.clone();
                    if let Err(e) = self.submit(ask, text) {
                        if let Some(p) = self.prompt.as_mut() {
                            p.error = Some(e);
                        }
                    }
                }
            }
            return Out::Handled;
        }
        if let Some(m) = self.menus.last_mut() {
            return match m.key(key) {
                Outcome::Handled => Out::Handled,
                Outcome::Close => {
                    self.menus.clear();
                    Out::Handled
                }
                Outcome::Back => {
                    self.menus.pop();
                    Out::Handled
                }
                Outcome::Pick(a) => {
                    let h = self.perform(a);
                    Self::host(h)
                }
            };
        }
        if let Some(d) = self.commit.as_mut() {
            match d.key(key) {
                CommitOut::Handled => {}
                CommitOut::Close => {
                    self.draft = d.msg.clone();
                    self.commit = None;
                }
                CommitOut::Submit => self.submit_commit(),
            }
            return Out::Handled;
        }
        if self.picker.is_some() {
            return self.picker_key(key);
        }
        Out::Unhandled
    }

    fn picker_key(&mut self, key: &KeyWithModifier) -> Out {
        if !plain(key) {
            return Out::Handled;
        }
        let Some(p) = self.picker.as_mut() else { return Out::Unhandled };
        match key.bare_key {
            BareKey::Esc if !p.filter.is_empty() => p.filter.clear(),
            BareKey::Esc => self.picker = None,
            BareKey::Down => self.picker_step(1),
            BareKey::Up => self.picker_step(-1),
            BareKey::PageDown => self.picker_step(10),
            BareKey::PageUp => self.picker_step(-10),
            BareKey::Backspace => {
                p.filter.pop();
            }
            BareKey::Enter | BareKey::Right => {
                let i = p.sel;
                let h = self.picker_pick(i);
                return Self::host(h);
            }
            BareKey::Char(c) => {
                p.filter.push(c);
                p.sel = 0;
            }
            _ => {}
        }
        self.picker_fix_sel();
        Out::Handled
    }

    /// A mouse click at screen (`line`, `col`).
    pub fn click(&mut self, line: usize, col: usize) -> Out {
        if self.prompt.is_some() {
            return Out::Handled;
        }
        if let Some(m) = self.menus.last_mut() {
            return match m.click(line, col) {
                Outcome::Pick(a) => {
                    let h = self.perform(a);
                    Self::host(h)
                }
                Outcome::Close => {
                    self.menus.clear();
                    Out::Handled
                }
                _ => Out::Handled,
            };
        }
        if let Some(d) = self.commit.as_mut() {
            d.click(line, col);
            return Out::Handled;
        }
        if let Some(p) = self.picker.as_mut() {
            let (x, y, w, h) = p.area;
            if line < y || line >= y + h || col < x || col >= x + w {
                self.picker = None;
                return Out::Handled;
            }
            if let Some(&(_, i)) = p.clicks.iter().find(|(l, _)| *l == line) {
                p.sel = i;
                let h = self.picker_pick(i);
                return Self::host(h);
            }
            return Out::Handled;
        }
        Out::Unhandled
    }

    pub fn hover(&mut self, line: usize, col: usize) -> bool {
        match self.menus.last_mut() {
            Some(m) => m.hover(line, col),
            None => false,
        }
    }

    pub fn scroll(&mut self, delta: isize) -> bool {
        if !self.menus.is_empty() || self.prompt.is_some() {
            return true;
        }
        if let Some(d) = self.commit.as_mut() {
            d.scroll(delta);
            return true;
        }
        if self.picker.is_some() {
            self.picker_step(delta);
            return true;
        }
        false
    }

    // -- drawing ---------------------------------------------------------------

    /// Draw the open popups over `lines`. The branch popup goes into `area`
    /// (x, y, w, h); with `framed` it gets a border.
    pub fn draw(&mut self, lines: &mut Vec<Line>, cols: usize, rows: usize, area: (usize, usize, usize, usize), framed: bool) {
        if self.picker.is_some() {
            self.draw_picker(lines, area, framed);
        }
        if let Some(d) = self.commit.as_mut() {
            d.draw(lines, cols, rows);
        }
        for m in &mut self.menus {
            m.draw(lines, cols, rows);
        }
        if let Some(p) = &self.prompt {
            p.draw(lines, cols, rows);
        }
    }

    fn draw_picker(&mut self, lines: &mut Vec<Line>, area: (usize, usize, usize, usize), framed: bool) {
        let rows = self.picker_rows();
        let (x, y, w, mut h) = area;
        if framed {
            // a popup is only as tall as its content (border + search + rows)
            h = h.min(rows.len() + 3 + usize::from(self.fetch_error.is_some())).max(6);
        }
        let (iy, iw, ih) = if framed { (y + 1, w.saturating_sub(2), h.saturating_sub(2)) } else { (y, w, h) };
        let mut body: Vec<Line> = Vec::new();
        let filter = self.picker.as_ref().map(|p| p.filter.clone()).unwrap_or_default();
        // search field
        let mut s = Line::new();
        s.push(DIM, " ⌕ ");
        if filter.is_empty() {
            s.push(DIM, "type to filter branches");
        } else {
            s.push(BOLD, &filter);
            s.push(REVERSE, " ");
        }
        if self.fetching {
            s.right(DIM, "fetching… ", iw);
        } else if self.fetch_error.is_some() {
            s.right(RED, "fetch failed ", iw);
        }
        body.push(s);
        if let Some(e) = self.fetch_error.as_ref().filter(|_| !self.fetching) {
            // why the remote branches may be stale
            body.push(ui::line(RED, &format!(" {}", ui::truncate(e, iw.saturating_sub(2)))));
        }
        let list_h = ih.saturating_sub(body.len());
        let p = self.picker.as_mut().unwrap();
        if p.sel < p.scroll {
            p.scroll = p.sel.saturating_sub(1);
        } else if list_h > 0 && p.sel >= p.scroll + list_h {
            p.scroll = p.sel + 1 - list_h;
        }
        p.scroll = p.scroll.min(rows.len().saturating_sub(list_h));
        let (sel, scroll) = (p.sel, p.scroll);
        let mut clicks = Vec::new();
        for (i, r) in rows.iter().enumerate().skip(scroll).take(list_h) {
            let mut l = Line::new();
            match r {
                PRow::Head(t) => {
                    l.push(&format!("{BOLD}{DIM}"), &format!(" {t}"));
                }
                PRow::Act(_, label, icon) => {
                    l.push(CYAN, &format!("  {icon} "));
                    l.push("", label);
                }
                PRow::Branch(bi) => {
                    let b = &self.branches[*bi];
                    let mut right = String::new();
                    if b.gone {
                        right.push_str("gone ");
                    }
                    if b.ahead > 0 {
                        right.push_str(&format!("↑{} ", b.ahead));
                    }
                    if b.behind > 0 {
                        right.push_str(&format!("↓{} ", b.behind));
                    }
                    if b.head {
                        l.push(GREEN, "  ● ");
                    } else if b.remote {
                        l.push(DIM, "  ☁ ");
                    } else {
                        l.push(DIM, "  ⎇ ");
                    }
                    let style = if b.head { format!("{BOLD}{GREEN}") } else if b.remote { MAGENTA.to_string() } else { String::new() };
                    l.push(&style, &ui::truncate(&b.name, iw.saturating_sub(5 + ui::width(&right))));
                    let rs = if b.gone { RED } else { YELLOW };
                    l.right(rs, &right, iw);
                }
            }
            let mut l = l.fit(iw);
            if i == sel && r.selectable() {
                l.restyle(REVERSE);
            }
            clicks.push((iy + 1 + body.len() - 1, i));
            body.push(l);
        }
        let p = self.picker.as_mut().unwrap();
        p.clicks = clicks;
        p.area = (x, y, w, h);
        while body.len() < ih {
            body.push(Line::new());
        }
        let out = if framed {
            let title = format!("Branches · {}", self.current_name());
            let hint = "↑↓ move · Enter actions · Esc close";
            ui::boxed(&title, hint, &body, w, CYAN)
        } else {
            body.into_iter().map(|l| l.fit(iw)).collect()
        };
        ui::overlay(lines, x, y, &out);
    }
}

fn act_is_rebase(a: &Act) -> bool {
    matches!(a, Act::PullRebase)
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// One line saying what a successful command did: its output, else the ref updates
/// git reports on stderr (push/fetch), else nothing.
fn summary(stdout: &str, stderr: &str) -> String {
    let out = first_line(stdout);
    if !out.is_empty() && out.chars().count() <= 80 {
        return out;
    }
    let updates: Vec<&str> = stderr.lines().map(str::trim).filter(|l| l.contains("->")).collect();
    match updates.len() {
        0 if stderr.contains("up to date") || stderr.contains("up-to-date") => "up to date".into(),
        0 => String::new(),
        1 => ui::truncate(updates[0], 80),
        n => format!("{n} refs updated"),
    }
}

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

fn first_line(s: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string()
}

/// The most telling line of git's error output.
fn first_error(s: &str) -> String {
    let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines
        .iter()
        .find(|l| l.starts_with("error:") || l.starts_with("fatal:") || l.starts_with("CONFLICT"))
        .or(lines.first())
        .map(|l| l.trim_start_matches("error: ").trim_start_matches("fatal: ").to_string())
        .unwrap_or_else(|| "failed".into())
}
