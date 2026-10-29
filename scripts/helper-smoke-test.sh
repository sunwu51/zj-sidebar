#!/usr/bin/env bash
# Smoke test for zjsb-helper on Linux/macOS: builds a fake $HOME with a Codex thread
# database and fake `claude`/`codex` processes, then checks what the helper reports.
#   usage: scripts/helper-smoke-test.sh /path/to/zjsb-helper
set -euo pipefail

HELPER=${1:?usage: helper-smoke-test.sh /path/to/zjsb-helper}
T=$(mktemp -d)
cleanup() {
  kill $(jobs -p) 2>/dev/null || true
  pkill -f "$T/bin/" 2>/dev/null || true
  rm -rf "$T"
}
trap cleanup EXIT

export HOME="$T/home"
unset CODEX_HOME USERPROFILE
mkdir -p "$HOME/.codex" "$HOME/.claude/sessions" "$T/bin" "$T/proj/Alpha" "$T/proj/alpha-other"

# ---- codex: thread list -------------------------------------------------------------
python3 - "$HOME/.codex/state_5.sqlite" "$T/proj/Alpha" "$T/proj/alpha-other" <<'PY'
import sqlite3, sys
db, cwd, other = sys.argv[1:4]
c = sqlite3.connect(db)
c.execute("""CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT, source TEXT, name TEXT, title TEXT,
             first_user_message TEXT, updated_at INTEGER, updated_at_ms INTEGER, archived INTEGER)""")
c.executemany("INSERT INTO threads VALUES (?,?,?,?,?,?,?,?,?)", [
    ("1" * 8 + "-1111-1111-1111-" + "1" * 12, cwd, "cli", "Named thread", "t", "m", 100, 100000, 0),
    ("2" * 8 + "-2222-2222-2222-" + "2" * 12, cwd, "vscode", None, "", "First message\nsecond line", 200, None, 0),
    ("3" * 8 + "-3333-3333-3333-" + "3" * 12, cwd, "cli", "Archived", "t", "m", 400, 400000, 1),
    ("4" * 8 + "-4444-4444-4444-" + "4" * 12, cwd, "exec", "Non-interactive", "t", "m", 500, 500000, 0),
    ("5" * 8 + "-5555-5555-5555-" + "5" * 12, other, "cli", "Other directory", "t", "m", 600, 600000, 0),
    ("6" * 8 + "-6666-6666-6666-" + "6" * 12, cwd + "/", "cli", "Trailing slash", "t", "m", 300, 300000, 0),
])
c.commit()
PY

out=$("$HELPER" codex "$T/proj/Alpha")
python3 - "$out" <<'PY'
import json, sys
rows = [json.loads(l) for l in sys.argv[1].splitlines()]
got = [(r["id"][0], r["title"], r["updated_ms"]) for r in rows]
want = [("6", "Trailing slash", 300000), ("2", "First message", 200000), ("1", "Named thread", 100000)]
assert got == want, f"codex threads:\n  got  {got}\n  want {want}"
PY
echo "ok  codex thread list"

# ---- running sessions ------------------------------------------------------------------
# a fake agent shows up as a process named like the agent, with the agent's arguments
cat > "$T/bin/codex" <<'EOF'
#!/usr/bin/env python3
import time
time.sleep(120)
EOF
cp "$T/bin/codex" "$T/bin/claude"
chmod +x "$T/bin/codex" "$T/bin/claude"

A=aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa
B=bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb
C=cccccccc-cccc-cccc-cccc-cccccccccccc

# pane 1: a shell running `codex resume <A>`
bash -c "'$T/bin/codex' resume $A & wait" &
SH1=$!
# pane 2: a shell running plain `claude`, known only through ~/.claude/sessions/<pid>.json
bash -c "'$T/bin/claude' & echo \$! > '$T/claude.pid'; wait" &
SH2=$!
# pane 3: the pane's own process is the agent
"$T/bin/codex" resume $C &
SH3=$!
sleep 1.5

CPID=$(cat "$T/claude.pid")
echo "{\"pid\":$CPID,\"sessionId\":\"$B\"}" > "$HOME/.claude/sessions/$CPID.json"

out=$("$HELPER" running "$SH1" "$SH2" "$SH3" 999999)
python3 - "$out" "$SH1" "$SH2" "$SH3" "$A" "$B" "$C" <<'PY'
import json, sys
rows = {(r["root"], r["kind"], r["id"]) for r in map(json.loads, sys.argv[1].splitlines())}
sh1, sh2, sh3 = map(int, sys.argv[2:5])
a, b, c = sys.argv[5:8]
want = {(sh1, "codex", a), (sh2, "claude", b), (sh3, "codex", c)}
assert rows == want, f"running sessions:\n  got  {sorted(rows)}\n  want {sorted(want)}"
PY
echo "ok  running sessions"
echo "all good"
