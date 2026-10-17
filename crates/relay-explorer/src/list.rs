//! Streamed folder listings.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::entry::Entry;

#[derive(Debug, Clone, Copy)]
pub struct ListOptions {
    /// Send whatever has been read this long after the start, so the first
    /// rows paint quickly even in a huge folder.
    pub first_batch_after: Duration,
    /// Then send at most this often.
    pub batch_every: Duration,
}

impl Default for ListOptions {
    fn default() -> Self {
        // Files flushes after 25 ms, then every 500 ms; a shorter second
        // interval keeps the scrollbar honest while a big folder loads.
        Self {
            first_batch_after: Duration::from_millis(25),
            batch_every: Duration::from_millis(150),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListStats {
    pub total: usize,
    pub batches: usize,
    pub elapsed: Duration,
    pub cancelled: bool,
}

/// Accumulates entries and decides when a batch is due.
struct Batcher<'a> {
    pending: Vec<Entry>,
    started: Instant,
    next_flush: Instant,
    every: Duration,
    total: usize,
    batches: usize,
    sink: &'a mut dyn FnMut(Vec<Entry>),
}

impl<'a> Batcher<'a> {
    fn new(options: &ListOptions, sink: &'a mut dyn FnMut(Vec<Entry>)) -> Self {
        let started = Instant::now();
        Self {
            pending: Vec::new(),
            started,
            next_flush: started + options.first_batch_after,
            every: options.batch_every,
            total: 0,
            batches: 0,
            sink,
        }
    }

    fn push(&mut self, entry: Entry) {
        self.pending.push(entry);
        self.total += 1;
        // Checking the clock per entry costs more than reading the entry.
        if self.pending.len().is_multiple_of(256) {
            self.maybe_flush();
        }
    }

    fn maybe_flush(&mut self) {
        let now = Instant::now();
        if now >= self.next_flush {
            self.flush();
            self.next_flush = now + self.every;
        }
    }

    fn flush(&mut self) {
        if !self.pending.is_empty() {
            self.batches += 1;
            (self.sink)(std::mem::take(&mut self.pending));
        }
    }

    fn finish(mut self, cancelled: bool) -> ListStats {
        if !cancelled {
            self.flush();
        }
        ListStats {
            total: self.total,
            batches: self.batches,
            elapsed: self.started.elapsed(),
            cancelled,
        }
    }
}

/// Read `dir`, handing entries to `sink` in timed batches. Stops early, and
/// sends nothing further, once `cancel` is set.
pub fn stream_dir(
    dir: &Path,
    options: &ListOptions,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(Vec<Entry>),
) -> io::Result<ListStats> {
    let mut batcher = Batcher::new(options, sink);
    read_dir(dir, cancel, &mut |entry| batcher.push(entry))?;
    let cancelled = cancel.load(Ordering::Relaxed);
    Ok(batcher.finish(cancelled))
}

#[cfg(windows)]
fn read_dir(dir: &Path, cancel: &AtomicBool, push: &mut dyn FnMut(Entry)) -> io::Result<()> {
    use crate::entry::flags_from_windows;
    let mut n = 0u32;
    relay_shell_win::enumerate::for_each(dir, |raw| {
        push(Entry {
            flags: flags_from_windows(raw.attributes, raw.reparse_tag),
            size: if raw.attributes & 0x10 != 0 {
                0
            } else {
                raw.size
            },
            modified_ms: raw.modified_ms,
            name: raw.name,
        });
        n = n.wrapping_add(1);
        !n.is_multiple_of(1024) || !cancel.load(Ordering::Relaxed)
    })
}

#[cfg(not(windows))]
fn read_dir(dir: &Path, cancel: &AtomicBool, push: &mut dyn FnMut(Entry)) -> io::Result<()> {
    for (i, item) in std::fs::read_dir(dir)?.enumerate() {
        if i.is_multiple_of(1024) && cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        // A name that vanished between readdir and stat is just skipped.
        if let Ok(entry) = crate::entry::stat(dir, &name) {
            push(entry);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(n: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..n {
            std::fs::write(dir.path().join(format!("f{i:05}.txt")), b"x").unwrap();
        }
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        dir
    }

    #[test]
    fn lists_everything_once() {
        let dir = fixture(1000);
        let mut names = Vec::new();
        let stats = stream_dir(
            dir.path(),
            &ListOptions::default(),
            &AtomicBool::new(false),
            &mut |batch| names.extend(batch.into_iter().map(|e| e.name)),
        )
        .unwrap();
        assert_eq!(stats.total, 1001);
        assert_eq!(names.len(), 1001);
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 1001);
        assert!(!stats.cancelled);
    }

    #[test]
    fn zero_intervals_send_many_batches() {
        let dir = fixture(2000);
        let options = ListOptions {
            first_batch_after: Duration::ZERO,
            batch_every: Duration::ZERO,
        };
        let mut sizes = Vec::new();
        let stats = stream_dir(dir.path(), &options, &AtomicBool::new(false), &mut |b| {
            sizes.push(b.len())
        })
        .unwrap();
        assert!(stats.batches > 1, "{sizes:?}");
        assert_eq!(sizes.iter().sum::<usize>(), 2001);
    }

    #[test]
    fn cancelled_sends_nothing_more() {
        let dir = fixture(10);
        let mut got = 0;
        let stats = stream_dir(
            dir.path(),
            &ListOptions::default(),
            &AtomicBool::new(true),
            &mut |b| got += b.len(),
        )
        .unwrap();
        assert!(stats.cancelled);
        assert_eq!(got, 0);
    }

    #[test]
    fn missing_dir_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = stream_dir(
            &dir.path().join("gone"),
            &ListOptions::default(),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
