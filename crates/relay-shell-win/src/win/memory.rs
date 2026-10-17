//! Memory use of this process and every process it started (the WebView2
//! browser, renderer and GPU processes), for the explorer's perf panel.

use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
use windows::Win32::Foundation::{CloseHandle, HANDLE, UNICODE_STRING};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::com::{Context, Error, OwnedHandle};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeMemory {
    pub processes: u32,
    /// Sum of private commit, bytes.
    pub private_bytes: u64,
    /// Sum of working sets, bytes (shared pages counted once per process).
    pub working_set_bytes: u64,
    /// Sum of private working sets, bytes: resident pages no other process
    /// shares, which is what Task Manager's Memory column shows.
    pub private_working_set_bytes: u64,
    /// Each process, in tree order (this process first).
    pub each: Vec<ProcessMemory>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessMemory {
    pub pid: u32,
    /// What the process is: its executable, plus Chromium's `--type` and
    /// `--utility-sub-type` for WebView2 children (`msedgewebview2.exe
    /// utility network.mojom.NetworkService`).
    pub kind: String,
    pub private_bytes: u64,
    pub working_set_bytes: u64,
    pub private_working_set_bytes: u64,
}

pub fn process_tree() -> Result<TreeMemory, Error> {
    let snapshot = OwnedHandle(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .ctx("CreateToolhelp32Snapshot")?,
    );
    let mut all: Vec<(u32, u32, String)> = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_ok() {
        loop {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let exe = String::from_utf16_lossy(&entry.szExeFile[..len]);
            all.push((entry.th32ProcessID, entry.th32ParentProcessID, exe));
            if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
                break;
            }
        }
    }

    let mut tree = vec![unsafe { GetCurrentProcessId() }];
    let mut i = 0;
    while i < tree.len() {
        let pid = tree[i];
        for (child, parent, _) in &all {
            if *parent == pid && *child != pid && !tree.contains(child) {
                tree.push(*child);
            }
        }
        i += 1;
    }

    let mut total = TreeMemory::default();
    for pid in tree {
        let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            continue;
        };
        let mut counters = PROCESS_MEMORY_COUNTERS_EX2 {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
            ..Default::default()
        };
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                process,
                (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX2)
                    .cast::<PROCESS_MEMORY_COUNTERS>(),
                counters.cb,
            )
        }
        .as_bool();
        let switches = command_line(process).map(|line| chromium_type(&line));
        let _ = unsafe { CloseHandle(process) };
        if ok {
            let exe = all
                .iter()
                .find(|(p, _, _)| *p == pid)
                .map(|(_, _, exe)| exe.as_str())
                .unwrap_or("?");
            let kind = match switches {
                Some(t) if !t.is_empty() => format!("{exe} {t}"),
                _ => exe.to_string(),
            };
            total.processes += 1;
            total.private_bytes += counters.PrivateUsage as u64;
            total.working_set_bytes += counters.WorkingSetSize as u64;
            total.private_working_set_bytes += counters.PrivateWorkingSetSize as u64;
            total.each.push(ProcessMemory {
                pid,
                kind,
                private_bytes: counters.PrivateUsage as u64,
                working_set_bytes: counters.WorkingSetSize as u64,
                private_working_set_bytes: counters.PrivateWorkingSetSize as u64,
            });
        }
    }
    Ok(total)
}

/// Another process's command line (Windows 8.1+; needs only limited query
/// access).
fn command_line(process: HANDLE) -> Option<String> {
    let mut len = 0u32;
    let _ = unsafe {
        NtQueryInformationProcess(
            process,
            ProcessCommandLineInformation,
            std::ptr::null_mut(),
            0,
            &mut len,
        )
    };
    if (len as usize) < std::mem::size_of::<UNICODE_STRING>() {
        return None;
    }
    // u64s, so the UNICODE_STRING header at the start is aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    unsafe {
        NtQueryInformationProcess(
            process,
            ProcessCommandLineInformation,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    }
    .ok()
    .ok()?;
    // The kernel points Buffer just past the header, inside `buf`.
    let header = unsafe { &*buf.as_ptr().cast::<UNICODE_STRING>() };
    if header.Buffer.is_null() {
        return None;
    }
    let chars =
        unsafe { std::slice::from_raw_parts(header.Buffer.0, usize::from(header.Length) / 2) };
    Some(String::from_utf16_lossy(chars))
}

/// Chromium's process type switches, e.g. `utility
/// network.mojom.NetworkService`; empty when there are none.
fn chromium_type(command_line: &str) -> String {
    let value = |name: &str| {
        command_line
            .split_whitespace()
            .find_map(|arg| arg.trim_matches('"').strip_prefix(name))
            .map(str::to_string)
    };
    [value("--type="), value("--utility-sub-type=")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_this_process() {
        let mem = super::process_tree().unwrap();
        assert!(mem.processes >= 1);
        assert!(mem.working_set_bytes > 0);
        assert!(mem.private_working_set_bytes > 0);
        assert!(mem.private_working_set_bytes <= mem.working_set_bytes);
        let me = &mem.each[0];
        assert_eq!(me.pid, std::process::id());
        assert!(
            me.kind.to_ascii_lowercase().ends_with(".exe"),
            "{}",
            me.kind
        );
    }

    #[test]
    fn reads_chromium_type() {
        let line = r#""C:\x\msedgewebview2.exe" --type=utility --utility-sub-type=network.mojom.NetworkService --lang=en-US"#;
        assert_eq!(
            super::chromium_type(line),
            "utility network.mojom.NetworkService"
        );
        assert_eq!(
            super::chromium_type(r#""C:\x\msedgewebview2.exe" --embedded-browser-webview=1"#),
            ""
        );
    }
}
