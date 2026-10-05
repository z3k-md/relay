//! Memory use of this process and every process it started (the WebView2
//! browser, renderer and GPU processes), for the explorer's perf panel.

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
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
}

pub fn process_tree() -> Result<TreeMemory, Error> {
    let snapshot = OwnedHandle(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .ctx("CreateToolhelp32Snapshot")?,
    );
    let mut parents: Vec<(u32, u32)> = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_ok() {
        loop {
            parents.push((entry.th32ProcessID, entry.th32ParentProcessID));
            if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
                break;
            }
        }
    }

    let mut tree = vec![unsafe { GetCurrentProcessId() }];
    let mut i = 0;
    while i < tree.len() {
        let pid = tree[i];
        for &(child, parent) in &parents {
            if parent == pid && child != pid && !tree.contains(&child) {
                tree.push(child);
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
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                process,
                (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX)
                    .cast::<PROCESS_MEMORY_COUNTERS>(),
                counters.cb,
            )
        }
        .as_bool();
        let _ = unsafe { CloseHandle(process) };
        if ok {
            total.processes += 1;
            total.private_bytes += counters.PrivateUsage as u64;
            total.working_set_bytes += counters.WorkingSetSize as u64;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_this_process() {
        let mem = super::process_tree().unwrap();
        assert!(mem.processes >= 1);
        assert!(mem.working_set_bytes > 0);
    }
}
