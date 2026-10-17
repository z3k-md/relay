//! Directory listing with `FindFirstFileExW`, the fastest documented way to
//! read names, attributes, sizes and times in one pass.
//!
//! Basic info level skips the 8.3 short name and LARGE_FETCH asks for bigger
//! directory reads, as Files does. The call can block for a long time on an
//! unreachable SMB share; callers run it off the UI thread.

use std::io;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, GetLastError};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic,
    FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, WIN32_FIND_DATAW,
};
use windows::core::PCWSTR;

use super::com::{from_wide, wide};

/// One directory entry as the file system reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    pub name: String,
    /// `FILE_ATTRIBUTE_*` bits.
    pub attributes: u32,
    /// `IO_REPARSE_TAG_*` when `attributes` has the reparse-point bit, else 0.
    pub reparse_tag: u32,
    pub size: u64,
    /// Last write time, Unix milliseconds.
    pub modified_ms: i64,
}

/// Call `f` for every entry in `dir` (not `.` or `..`). Stops early when `f`
/// returns false. An empty or vanished directory is not an error.
pub fn for_each(dir: &Path, mut f: impl FnMut(RawEntry) -> bool) -> io::Result<()> {
    let pattern = wide(search_pattern(dir));
    let mut data = WIN32_FIND_DATAW::default();
    let handle = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            (&mut data as *mut WIN32_FIND_DATAW).cast(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    let handle = match handle {
        Ok(handle) => handle,
        Err(err) if err.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(()),
        Err(err) => return Err(io::Error::from_raw_os_error(err.code().0 & 0xFFFF)),
    };

    let result = loop {
        let name = from_wide(&data.cFileName);
        if name != "." && name != ".." && !f(convert(name, &data)) {
            break Ok(());
        }
        if unsafe { FindNextFileW(handle, &mut data) }.is_err() {
            let last = unsafe { GetLastError() };
            if last == ERROR_NO_MORE_FILES {
                break Ok(());
            }
            break Err(io::Error::from_raw_os_error(last.0 as i32));
        }
    };
    let _ = unsafe { FindClose(handle) };
    result
}

fn search_pattern(dir: &Path) -> PathBuf {
    // `C:` alone means "current directory on C:", so pin drive roots.
    let mut dir = dir.to_path_buf();
    let text = dir.as_os_str().to_string_lossy();
    if text.len() == 2 && text.ends_with(':') {
        dir.push("\\");
    }
    dir.join("*")
}

fn convert(name: String, data: &WIN32_FIND_DATAW) -> RawEntry {
    let attributes = data.dwFileAttributes;
    let reparse_tag = if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        data.dwReserved0
    } else {
        0
    };
    RawEntry {
        name,
        attributes,
        reparse_tag,
        size: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
        modified_ms: filetime_to_unix_ms(
            (u64::from(data.ftLastWriteTime.dwHighDateTime) << 32)
                | u64::from(data.ftLastWriteTime.dwLowDateTime),
        ),
    }
}

/// FILETIME counts 100 ns ticks since 1601-01-01.
pub(crate) fn filetime_to_unix_ms(ticks: u64) -> i64 {
    const EPOCH_DIFF: i64 = 116_444_736_000_000_000;
    (ticks as i64 - EPOCH_DIFF) / 10_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_files_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut seen = Vec::new();
        for_each(dir.path(), |e| {
            seen.push(e);
            true
        })
        .unwrap();
        seen.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].name, "a.txt");
        assert_eq!(seen[0].size, 5);
        assert!(seen[0].modified_ms > 1_600_000_000_000);
        assert_eq!(seen[1].name, "sub");
        assert_ne!(seen[1].attributes & 0x10, 0, "directory bit");
    }

    #[test]
    fn stops_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("{i}.txt")), b"").unwrap();
        }
        let mut n = 0;
        for_each(dir.path(), |_| {
            n += 1;
            n < 3
        })
        .unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn missing_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = for_each(&dir.path().join("nope"), |_| true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn filetime_epoch() {
        assert_eq!(filetime_to_unix_ms(116_444_736_000_000_000), 0);
    }
}
