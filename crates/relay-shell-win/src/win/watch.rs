//! Change notifications for one open directory with `ReadDirectoryChangesW`.
//!
//! Overlapped I/O on a dedicated thread, waiting on the completion event and a
//! stop event, so dropping the watcher returns promptly. A zero-byte
//! completion means the kernel buffer overflowed and the caller must rescan.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects,
};
use windows::core::PCWSTR;

use super::com::{OwnedHandle, wide};

/// What happened to a name in the watched directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Added,
    Removed,
    Modified,
    RenamedFrom,
    RenamedTo,
    /// Events were lost; rescan the directory.
    Overflow,
}

pub type RawEvent = (Action, String);

/// Watches one directory (not recursive) until dropped.
pub struct DirWatch {
    stop: Arc<OwnedHandle>,
    thread: Option<JoinHandle<()>>,
}

const BUFFER_BYTES: usize = 64 * 1024;

impl DirWatch {
    /// Start watching `dir`. `attributes` adds attribute changes, which is
    /// how cloud-file hydration shows up. `on_events` runs on the watcher
    /// thread with each completed batch.
    pub fn start(
        dir: &Path,
        attributes: bool,
        mut on_events: impl FnMut(Vec<RawEvent>) + Send + 'static,
    ) -> io::Result<Self> {
        let path = wide(dir);
        let dir_handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(io::Error::from)?;
        let dir_handle = OwnedHandle(dir_handle);
        let stop = Arc::new(OwnedHandle(
            unsafe { CreateEventW(None, true, false, None) }.map_err(io::Error::from)?,
        ));
        let done =
            OwnedHandle(unsafe { CreateEventW(None, true, false, None) }.map_err(io::Error::from)?);
        let stop_for_thread = Arc::clone(&stop);

        let mut filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_LAST_WRITE
            | FILE_NOTIFY_CHANGE_SIZE;
        if attributes {
            filter |= FILE_NOTIFY_CHANGE_ATTRIBUTES;
        }

        let thread = std::thread::Builder::new()
            .name("relay-dirwatch".into())
            .spawn(move || run(&dir_handle, &done, &stop_for_thread, filter, &mut on_events))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for DirWatch {
    fn drop(&mut self) {
        let _ = unsafe { SetEvent(self.stop.0) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    dir: &OwnedHandle,
    done: &OwnedHandle,
    stop: &OwnedHandle,
    filter: FILE_NOTIFY_CHANGE,
    on_events: &mut dyn FnMut(Vec<RawEvent>),
) {
    // u64 storage keeps the buffer DWORD-aligned, as the API requires.
    let mut buffer = vec![0u64; BUFFER_BYTES / 8];
    loop {
        let mut overlapped = OVERLAPPED {
            hEvent: done.0,
            ..Default::default()
        };
        let _ = unsafe { ResetEvent(done.0) };
        let started = unsafe {
            ReadDirectoryChangesW(
                dir.0,
                buffer.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
                false,
                filter,
                None,
                Some(&mut overlapped),
                None,
            )
        };
        if started.is_err() {
            // The directory went away or the handle broke; tell the caller to
            // look again and stop.
            on_events(vec![(Action::Overflow, String::new())]);
            return;
        }

        let wait = unsafe { WaitForMultipleObjects(&[done.0, stop.0], false, INFINITE) };
        if wait != WAIT_OBJECT_0 {
            unsafe {
                let _ = CancelIoEx(dir.0, Some(&overlapped));
                let mut n = 0;
                // Wait for the cancel so the kernel stops writing into `buffer`.
                let _ = GetOverlappedResult(dir.0, &overlapped, &mut n, true);
            }
            return;
        }

        let mut bytes = 0u32;
        if unsafe { GetOverlappedResult(dir.0, &overlapped, &mut bytes, false) }.is_err() {
            on_events(vec![(Action::Overflow, String::new())]);
            return;
        }
        if bytes == 0 {
            on_events(vec![(Action::Overflow, String::new())]);
            continue;
        }
        let bytes = &bytemuck_u8(&buffer)[..bytes as usize];
        let events = parse(bytes);
        if !events.is_empty() {
            on_events(events);
        }
    }
}

fn bytemuck_u8(buf: &[u64]) -> &[u8] {
    // SAFETY: u64 has no padding and any byte pattern is a valid u8.
    unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len() * 8) }
}

fn parse(bytes: &[u8]) -> Vec<RawEvent> {
    let header = std::mem::size_of::<FILE_NOTIFY_INFORMATION>() - std::mem::size_of::<u16>();
    let mut events = Vec::new();
    let mut offset = 0usize;
    loop {
        if offset + header > bytes.len() {
            break;
        }
        let record = &bytes[offset..];
        let next = u32::from_ne_bytes(record[0..4].try_into().unwrap_or_default()) as usize;
        let action = u32::from_ne_bytes(record[4..8].try_into().unwrap_or_default());
        let name_len = u32::from_ne_bytes(record[8..12].try_into().unwrap_or_default()) as usize;
        let name_bytes = record.get(12..12 + name_len).unwrap_or_default();
        let units: Vec<u16> = name_bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_ne_bytes(*c))
            .collect();
        let name = String::from_utf16_lossy(&units);
        let action = match action {
            a if a == FILE_ACTION_ADDED.0 => Some(Action::Added),
            a if a == FILE_ACTION_REMOVED.0 => Some(Action::Removed),
            a if a == FILE_ACTION_MODIFIED.0 => Some(Action::Modified),
            a if a == FILE_ACTION_RENAMED_OLD_NAME.0 => Some(Action::RenamedFrom),
            a if a == FILE_ACTION_RENAMED_NEW_NAME.0 => Some(Action::RenamedTo),
            _ => None,
        };
        if let Some(action) = action {
            events.push((action, name));
        }
        if next == 0 {
            break;
        }
        offset += next;
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn collect_until(
        rx: &mpsc::Receiver<Vec<RawEvent>>,
        mut done: impl FnMut(&[RawEvent]) -> bool,
    ) -> Vec<RawEvent> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut all = Vec::new();
        while !done(&all) {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(batch) => all.extend(batch),
                Err(_) => break,
            }
        }
        all
    }

    #[test]
    fn reports_create_rename_delete() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let _watch = DirWatch::start(dir.path(), false, move |batch| {
            let _ = tx.send(batch);
        })
        .unwrap();

        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        std::fs::rename(dir.path().join("a.txt"), dir.path().join("b.txt")).unwrap();
        std::fs::remove_file(dir.path().join("b.txt")).unwrap();

        let events = collect_until(&rx, |e| {
            e.iter().any(|(a, n)| *a == Action::Removed && n == "b.txt")
        });
        assert!(
            events.contains(&(Action::Added, "a.txt".into())),
            "{events:?}"
        );
        assert!(
            events.contains(&(Action::RenamedFrom, "a.txt".into())),
            "{events:?}"
        );
        assert!(
            events.contains(&(Action::RenamedTo, "b.txt".into())),
            "{events:?}"
        );
        assert!(
            events.contains(&(Action::Removed, "b.txt".into())),
            "{events:?}"
        );
    }

    #[test]
    fn drop_stops_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let watch = DirWatch::start(dir.path(), true, |_| {}).unwrap();
        let start = Instant::now();
        drop(watch);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn parses_records() {
        // Two records: Added "ab", Removed "c".
        let mut bytes = Vec::new();
        let rec = |next: u32, action: u32, name: &str| {
            let units: Vec<u16> = name.encode_utf16().collect();
            let mut r = Vec::new();
            r.extend(next.to_ne_bytes());
            r.extend(action.to_ne_bytes());
            r.extend(((units.len() * 2) as u32).to_ne_bytes());
            for u in units {
                r.extend(u.to_ne_bytes());
            }
            while r.len() % 4 != 0 {
                r.push(0);
            }
            r
        };
        let first = rec(0, 1, "ab");
        let first = rec(first.len() as u32, 1, "ab");
        bytes.extend(&first);
        bytes.extend(rec(0, 2, "c"));
        assert_eq!(
            parse(&bytes),
            vec![(Action::Added, "ab".into()), (Action::Removed, "c".into())]
        );
    }
}
