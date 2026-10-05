//! Which mounts the loop watches, and when each is due for a scan.

use super::*;

/// Loop-side follow-ups for an applied config change: mount watches here,
/// then live sessions in [`Syncer::after_config`].
pub(super) fn after_config(
    engine: &mut Engine,
    syncer: &mut Syncer,
    states: &mut Vec<MountWatch>,
    watcher: Option<&mut MountWatcher>,
    applied: &Applied,
    output: &mut dyn FnMut(SyncOutput),
    on_event: &mut dyn FnMut(&WatchEvent),
) {
    match (&applied.change, &applied.result) {
        (
            ConfigChange::AddMount { space, .. },
            ConfigApplied::Mount {
                mount,
                path: Some(root),
            },
        ) => track_mount(states, watcher, space, mount, root, on_event),
        (ConfigChange::RemoveMount { space, mount }, _) => {
            untrack_mount(states, watcher, space, mount, on_event);
        }
        _ => {}
    }
    emit_sync(syncer.after_config(engine, applied, output), on_event);
    // Mounts and rules decide which folders hold placeholders (D43).
    engine.refresh_placeholder_roots();
}

/// Start watching a newly attached mount. Its first full scan is due now.
fn track_mount(
    states: &mut Vec<MountWatch>,
    watcher: Option<&mut MountWatcher>,
    space: &str,
    mount: &Mount,
    root: &Path,
    on_event: &mut dyn FnMut(&WatchEvent),
) {
    let mut state = MountWatch {
        space_id: mount.space,
        mount_id: mount.id,
        space: space.to_owned(),
        mount: mount.name.clone(),
        root: root.to_path_buf(),
        dirty: HashSet::new(),
        first_event: None,
        last_event: Some(Instant::now()),
        full_pending: true,
        last_full: None,
        failed: false,
        watcher_attached: false,
    };
    if let Some(watcher) = watcher {
        attach_watcher(watcher, &mut state, on_event);
    }
    states.push(state);
    on_event(&WatchEvent::Started {
        mounts: vec![format!("{space}/{}", mount.name)],
    });
}

/// Stop watching a detached mount and drop its pending scans.
fn untrack_mount(
    states: &mut Vec<MountWatch>,
    watcher: Option<&mut MountWatcher>,
    space: &str,
    mount: &str,
    on_event: &mut dyn FnMut(&WatchEvent),
) {
    let Some(index) = states
        .iter()
        .position(|state| state.space == space && state.mount == mount)
    else {
        return;
    };
    let state = states.remove(index);
    if state.watcher_attached
        && let Some(watcher) = watcher
        && let Err(err) = watcher.unwatch(&state.root)
    {
        tracing::debug!(path = %state.root.display(), error = %err, "unwatch failed");
    }
    on_event(&WatchEvent::MountRemoved {
        space: space.to_owned(),
        mount: mount.to_owned(),
    });
}

pub(super) fn attach_watcher(
    watcher: &mut MountWatcher,
    state: &mut MountWatch,
    on_event: &mut dyn FnMut(&WatchEvent),
) {
    match watcher.watch(&state.root) {
        Ok(()) => state.watcher_attached = true,
        Err(err) => {
            state.watcher_attached = false;
            on_event(&WatchEvent::WatcherUnavailable {
                space: state.space.clone(),
                mount: state.mount.clone(),
                error: err.to_string(),
            });
        }
    }
}

pub(super) fn retry_watchers(
    mut watcher: Option<&mut MountWatcher>,
    states: &mut [MountWatch],
    now: Instant,
    interval: Duration,
    on_event: &mut dyn FnMut(&WatchEvent),
) {
    for state in states.iter_mut() {
        let periodic = state
            .last_full
            .is_some_and(|t| now.saturating_duration_since(t) >= interval);
        if !periodic {
            continue;
        }
        if !state.watcher_attached
            && let Some(watcher) = watcher.as_deref_mut()
        {
            attach_watcher(watcher, state, on_event);
        }
        state.failed = false;
        state.full_pending = true;
        note_times(state, now);
    }
}

pub(super) fn apply_signal(states: &mut [MountWatch], signal: WatchSignal, now: Instant) {
    match signal {
        WatchSignal::Changed { root, paths } => {
            let Some(state) = find_mount(states, &root) else {
                return;
            };
            if paths.iter().any(|p| forces_full_scan(&state.root, p)) {
                state.full_pending = true;
                note_event(state, now);
                return;
            }
            for os_path in paths {
                match to_logical_path(&state.root, &os_path) {
                    Ok(logical) => {
                        state.dirty.insert(logical);
                    }
                    Err(_) => {
                        state.full_pending = true;
                    }
                }
            }
            note_event(state, now);
        }
        WatchSignal::Rescan { root, .. } => {
            let Some(state) = find_mount(states, &root) else {
                return;
            };
            state.full_pending = true;
            note_event(state, now);
        }
    }
}

fn forces_full_scan(root: &Path, os_path: &Path) -> bool {
    if os_path == root.join(".relayignore") || os_path == root.join(MOUNT_MARKER) {
        return true;
    }
    match to_logical_path(root, os_path) {
        Ok(path) => path.as_str() == ".relayignore" || path.as_str() == MOUNT_MARKER,
        Err(_) => false,
    }
}

fn find_mount<'a>(states: &'a mut [MountWatch], root: &Path) -> Option<&'a mut MountWatch> {
    if let Some(i) = states.iter().position(|s| s.root == root) {
        return Some(&mut states[i]);
    }
    let canon = dunce::canonicalize(root).ok();
    states.iter_mut().find(|s| {
        if let Some(c) = &canon
            && let Ok(other) = dunce::canonicalize(&s.root)
        {
            return *c == other;
        }
        s.root == root
    })
}

fn note_event(state: &mut MountWatch, now: Instant) {
    state.failed = false;
    note_times(state, now);
}

fn note_times(state: &mut MountWatch, now: Instant) {
    if state.first_event.is_none() {
        state.first_event = Some(now);
    }
    state.last_event = Some(now);
}

pub(super) fn mark_rescan(states: &mut [MountWatch], mounts: &[(SpaceId, MountId)], now: Instant) {
    for state in states {
        if mounts
            .iter()
            .any(|(space, mount)| *space == state.space_id && *mount == state.mount_id)
        {
            state.failed = false;
            state.full_pending = true;
            note_times(state, now);
            state.last_event = Some(now.checked_sub(Duration::from_secs(10)).unwrap_or(now));
        }
    }
}

pub(super) struct FlushJob {
    pub(super) index: usize,
    pub(super) full: bool,
    pub(super) paths: Vec<LogicalPath>,
}

pub(super) fn flush_jobs(
    states: &mut [MountWatch],
    now: Instant,
    opts: &WatchOptions,
) -> Vec<FlushJob> {
    let mut jobs = Vec::new();
    for (index, state) in states.iter_mut().enumerate() {
        if state.failed {
            continue;
        }
        let periodic_full = state.full_pending
            && state
                .last_full
                .is_some_and(|t| now.saturating_duration_since(t) >= opts.full_scan_interval);
        if !periodic_full && !due_for_flush(state, now, opts) {
            continue;
        }
        if !state.full_pending && state.dirty.is_empty() {
            continue;
        }
        let full = state.full_pending || state.dirty.len() > opts.max_dirty_paths;
        let paths = if full {
            Vec::new()
        } else {
            state.dirty.iter().cloned().collect()
        };
        jobs.push(FlushJob { index, full, paths });
    }
    jobs
}

fn due_for_flush(state: &MountWatch, now: Instant, opts: &WatchOptions) -> bool {
    let Some(last) = state.last_event else {
        return false;
    };
    now.saturating_duration_since(last) >= opts.debounce
        || state
            .first_event
            .is_some_and(|first| now.saturating_duration_since(first) >= opts.max_batch_delay)
}
