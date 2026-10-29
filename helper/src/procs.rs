//! Process table access: (pid, parent pid, command line) for every process.

pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// executable name, lowercase, without directory
    pub exe: String,
    /// full command line when the listing already has it (Unix); see `cmdline`
    #[cfg_attr(windows, allow(dead_code))]
    cmd: Option<String>,
}

#[cfg(windows)]
mod imp {
    use super::Proc;
    use std::mem::size_of;
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, UNICODE_STRING};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    pub fn list() -> Vec<Proc> {
        let mut out = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e) != 0;
            while ok {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                out.push(Proc {
                    pid: e.th32ProcessID,
                    ppid: e.th32ParentProcessID,
                    exe: String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase(),
                    cmd: None,
                });
                ok = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        out
    }

    pub fn cmdline(p: &Proc) -> Option<String> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, p.pid);
            if h.is_null() {
                return None;
            }
            // u64 backing keeps the UNICODE_STRING header aligned
            let mut buf = vec![0u64; 8192];
            let mut ret = 0u32;
            let status = NtQueryInformationProcess(
                h,
                ProcessCommandLineInformation,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut ret,
            );
            CloseHandle(h);
            if status != 0 {
                return None;
            }
            let us = &*(buf.as_ptr() as *const UNICODE_STRING);
            let chars = std::slice::from_raw_parts(us.Buffer, us.Length as usize / 2);
            Some(String::from_utf16_lossy(chars))
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Proc;
    use std::process::Command;

    /// One `ps` call for everything (`-ww`: never truncate command lines).
    pub fn list() -> Vec<Proc> {
        let out = Command::new("ps")
            .args(["-axww", "-o", "pid=,ppid=,command="])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        out.lines()
            .filter_map(|l| {
                let l = l.trim_start();
                let (pid, rest) = l.split_once(char::is_whitespace)?;
                let (ppid, cmd) = rest.trim_start().split_once(char::is_whitespace)?;
                let cmd = cmd.trim().to_string();
                let first = cmd.split_whitespace().next().unwrap_or("");
                let exe = first.rsplit('/').next().unwrap_or(first).to_lowercase();
                Some(Proc { pid: pid.parse().ok()?, ppid: ppid.trim().parse().ok()?, exe, cmd: Some(cmd) })
            })
            .collect()
    }

    pub fn cmdline(p: &Proc) -> Option<String> {
        p.cmd.clone()
    }
}

pub use imp::{cmdline, list};
