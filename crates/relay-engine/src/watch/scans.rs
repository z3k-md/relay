//! Watch-loop scans: reporting, yielding to priority commands, and pushing
//! committed changes.

use super::*;

impl Engine {
    fn scan_reporting(
        &mut self,
        space: &str,
        mount: &str,
        paths: Option<&[LogicalPath]>,
        on_event: &mut dyn FnMut(&WatchEvent),
        poll_priority: &mut dyn FnMut() -> bool,
    ) -> Result<ScanReport, EngineError> {
        let mut files = 0u64;
        let mut bytes = 0u64;
        let mut last = Instant::now()
            .checked_sub(Duration::from_millis(250))
            .unwrap_or_else(Instant::now);
        let space_name = space.to_owned();
        let mount_name = mount.to_owned();
        let mut on_tick = |tick: crate::scan::ScanTick| {
            match tick {
                crate::scan::ScanTick::Visited => files += 1,
                crate::scan::ScanTick::Hashed(n) => bytes = bytes.saturating_add(n),
            }
            let now = Instant::now();
            if now.saturating_duration_since(last) >= Duration::from_millis(250)
                && (files > 0 || bytes > 0)
            {
                last = now;
                on_event(&WatchEvent::ScanProgress {
                    space: space_name.clone(),
                    mount: mount_name.clone(),
                    files_seen: files,
                    bytes_hashed: bytes,
                });
            }
            if poll_priority() {
                crate::scan::request_scan_yield();
            }
        };
        match paths {
            Some(paths) => {
                self.scan_paths_inner(space, mount, paths, ScanOptions::default(), &mut on_tick)
            }
            None => self.scan_mount(space, mount, ScanOptions::default(), &mut on_tick),
        }
    }

    pub(super) fn run_watch_scan(
        &mut self,
        state: &mut MountWatch,
        full: bool,
        on_event: &mut dyn FnMut(&WatchEvent),
        poll_priority: &mut dyn FnMut() -> bool,
    ) -> ScanStep {
        let paths = state.dirty.len();
        let result = self.scan_reporting(&state.space, &state.mount, None, on_event, poll_priority);
        finish_or_yield(state, full, paths, result, on_event)
    }

    pub(super) fn run_watch_scan_paths(
        &mut self,
        state: &mut MountWatch,
        paths: &[LogicalPath],
        on_event: &mut dyn FnMut(&WatchEvent),
        poll_priority: &mut dyn FnMut() -> bool,
    ) -> ScanStep {
        let n = paths.len();
        let result = self.scan_reporting(
            &state.space,
            &state.mount,
            Some(paths),
            on_event,
            poll_priority,
        );
        finish_or_yield(state, false, n, result, on_event)
    }
}

pub(super) enum ScanStep {
    Finished { committed: bool },
    Yielded,
}

fn finish_or_yield(
    state: &mut MountWatch,
    full: bool,
    paths: usize,
    result: Result<ScanReport, EngineError>,
    on_event: &mut dyn FnMut(&WatchEvent),
) -> ScanStep {
    if matches!(result, Err(EngineError::Interrupted)) {
        // Leave the mount pending so the index resumes after the command.
        return ScanStep::Yielded;
    }
    let committed = result.as_ref().is_ok_and(scan_committed);
    finish_watch_scan(state, full, paths, result, on_event);
    ScanStep::Finished { committed }
}

fn scan_committed(report: &ScanReport) -> bool {
    report.created > 0 || report.modified > 0 || report.deleted > 0
}

fn finish_watch_scan(
    state: &mut MountWatch,
    full: bool,
    paths: usize,
    result: Result<ScanReport, EngineError>,
    on_event: &mut dyn FnMut(&WatchEvent),
) {
    match result {
        Ok(report) => {
            state.dirty.clear();
            state.first_event = None;
            state.last_event = None;
            state.full_pending = false;
            state.failed = false;
            if full {
                state.last_full = Some(Instant::now());
            }
            // Always emit, including a partial scan with no changes. A progress
            // tick may already have opened an index row, and the host clears
            // that row on Scanned. Callers hide the no-change partial from logs.
            on_event(&WatchEvent::Scanned {
                space: state.space.clone(),
                mount: state.mount.clone(),
                full,
                paths,
                report,
            });
        }
        Err(err) => {
            state.failed = true;
            if full {
                state.last_full = Some(Instant::now());
            }
            on_event(&WatchEvent::ScanFailed {
                space: state.space.clone(),
                mount: state.mount.clone(),
                error: err.to_string(),
            });
        }
    }
}
