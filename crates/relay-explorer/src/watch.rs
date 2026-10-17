//! Live updates for an open folder: raw file system events, coalesced over a
//! short window and resolved to entries.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use serde::Serialize;

use crate::entry::{Entry, stat};

/// A change the UI applies to its listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Change {
    /// Add the entry, or replace the row with the same name.
    Upsert {
        entry: Entry,
    },
    Removed {
        name: String,
    },
    /// Replace the row `from` in place, keeping its position and selection.
    Renamed {
        from: String,
        entry: Entry,
    },
    /// Events were lost; list the folder again.
    Rescan,
}

/// A raw event, before coalescing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawEvent {
    Added(String),
    Removed(String),
    Modified(String),
    RenamedFrom(String),
    RenamedTo(String),
    Overflow,
}

/// What to do about a name once a window of events has been folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    Upsert(String),
    Removed(String),
    Renamed { from: String, to: String },
    Rescan,
}

/// Fold a window of raw events into at most one pending change per name,
/// preserving first-seen order. Pure, so the rules are easy to test.
pub fn coalesce(events: Vec<RawEvent>) -> Vec<Pending> {
    let mut out: Vec<Option<Pending>> = Vec::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    let mut rename_from: Option<String> = None;

    fn set(
        out: &mut Vec<Option<Pending>>,
        by_name: &mut HashMap<String, usize>,
        name: &str,
        change: Pending,
    ) {
        match by_name.get(name) {
            Some(&i) => out[i] = Some(change),
            None => {
                by_name.insert(name.to_string(), out.len());
                out.push(Some(change));
            }
        }
    }

    for event in events {
        // A "from" not followed directly by its "to" is a move out.
        if let Some(from) = rename_from.take() {
            match &event {
                RawEvent::RenamedTo(to) => {
                    if let Some(i) = by_name.remove(&from) {
                        out[i] = None;
                    }
                    set(
                        &mut out,
                        &mut by_name,
                        to,
                        Pending::Renamed {
                            from,
                            to: to.clone(),
                        },
                    );
                    continue;
                }
                _ => set(
                    &mut out,
                    &mut by_name,
                    &from.clone(),
                    Pending::Removed(from),
                ),
            }
        }
        match event {
            RawEvent::Overflow => return vec![Pending::Rescan],
            RawEvent::Added(name) | RawEvent::Modified(name) | RawEvent::RenamedTo(name) => {
                let keep = matches!(
                    by_name.get(&name).and_then(|&i| out[i].as_ref()),
                    Some(Pending::Renamed { .. } | Pending::Upsert(_))
                );
                if !keep {
                    set(&mut out, &mut by_name, &name, Pending::Upsert(name.clone()));
                }
            }
            RawEvent::Removed(name) => {
                let change = match by_name.get(&name).and_then(|&i| out[i].clone()) {
                    // Renamed then deleted: the row the UI has is `from`.
                    Some(Pending::Renamed { from, .. }) => Pending::Removed(from),
                    _ => Pending::Removed(name.clone()),
                };
                set(&mut out, &mut by_name, &name, change);
            }
            RawEvent::RenamedFrom(name) => rename_from = Some(name),
        }
    }
    if let Some(from) = rename_from {
        set(
            &mut out,
            &mut by_name,
            &from.clone(),
            Pending::Removed(from),
        );
    }
    out.into_iter().flatten().collect()
}

/// Stat the names that need an entry. A name gone by now becomes a removal.
pub fn resolve(dir: &Path, pending: Vec<Pending>) -> Vec<Change> {
    pending
        .into_iter()
        .map(|p| match p {
            Pending::Rescan => Change::Rescan,
            Pending::Removed(name) => Change::Removed { name },
            Pending::Upsert(name) => match stat(dir, &name) {
                Ok(entry) => Change::Upsert { entry },
                Err(_) => Change::Removed { name },
            },
            Pending::Renamed { from, to } => match stat(dir, &to) {
                Ok(entry) => Change::Renamed { from, entry },
                Err(_) => Change::Removed { name: from },
            },
        })
        .collect()
}

/// How long to gather events before resolving them. Long enough to fold a
/// save (truncate + write + rename) into one change, short enough to feel live.
pub const WINDOW: Duration = Duration::from_millis(100);

/// Watches one folder until dropped and calls `on_changes` from a background
/// thread with each coalesced, resolved batch.
pub struct DirWatcher {
    _platform: platform::Watch,
}

impl DirWatcher {
    pub fn start(
        dir: PathBuf,
        on_changes: impl Fn(Vec<Change>) + Send + 'static,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel::<Vec<RawEvent>>();
        let platform = platform::Watch::start(&dir, tx)?;
        std::thread::Builder::new()
            .name("relay-explorer-watch".into())
            .spawn(move || {
                // Ends when the platform watcher (the only sender) drops.
                while let Ok(first) = rx.recv() {
                    let mut events = first;
                    std::thread::sleep(WINDOW);
                    while let Ok(more) = rx.try_recv() {
                        events.extend(more);
                    }
                    let changes = resolve(&dir, coalesce(events));
                    if !changes.is_empty() {
                        on_changes(changes);
                    }
                }
            })?;
        Ok(Self {
            _platform: platform,
        })
    }
}

#[cfg(windows)]
mod platform {
    use super::RawEvent;
    use relay_shell_win::watch::{Action, DirWatch};
    use std::io;
    use std::path::Path;
    use std::sync::mpsc::Sender;

    pub struct Watch(#[allow(dead_code)] DirWatch);

    impl Watch {
        pub fn start(dir: &Path, tx: Sender<Vec<RawEvent>>) -> io::Result<Self> {
            // Attribute changes are how cloud placeholders report hydration.
            let watch = DirWatch::start(dir, true, move |batch| {
                let events = batch
                    .into_iter()
                    .map(|(action, name)| match action {
                        Action::Added => RawEvent::Added(name),
                        Action::Removed => RawEvent::Removed(name),
                        Action::Modified => RawEvent::Modified(name),
                        Action::RenamedFrom => RawEvent::RenamedFrom(name),
                        Action::RenamedTo => RawEvent::RenamedTo(name),
                        Action::Overflow => RawEvent::Overflow,
                    })
                    .collect();
                let _ = tx.send(events);
            })?;
            Ok(Self(watch))
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::RawEvent;
    use notify::event::{EventKind, ModifyKind, RenameMode};
    use notify::{RecommendedWatcher, RecursiveMode, Watcher};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::Sender;

    pub struct Watch(#[allow(dead_code)] RecommendedWatcher);

    impl Watch {
        pub fn start(dir: &Path, tx: Sender<Vec<RawEvent>>) -> io::Result<Self> {
            // FSEvents reports canonical paths (/private/var/…), inotify the
            // path we passed; accept either as the parent.
            let root: PathBuf = dir.to_path_buf();
            let canonical: PathBuf = dir.canonicalize().unwrap_or_else(|_| root.clone());
            let mut watcher =
                notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    let event = match res {
                        Ok(event) => event,
                        Err(_) => {
                            let _ = tx.send(vec![RawEvent::Overflow]);
                            return;
                        }
                    };
                    if event.need_rescan() {
                        let _ = tx.send(vec![RawEvent::Overflow]);
                        return;
                    }
                    let names: Vec<String> = event
                        .paths
                        .iter()
                        .filter(|p| {
                            p.parent()
                                .is_some_and(|parent| parent == root || parent == canonical)
                        })
                        .filter_map(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .collect();
                    let events: Vec<RawEvent> = match event.kind {
                        EventKind::Create(_) => names.into_iter().map(RawEvent::Added).collect(),
                        EventKind::Remove(_) => names.into_iter().map(RawEvent::Removed).collect(),
                        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                            names.into_iter().map(RawEvent::RenamedFrom).collect()
                        }
                        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                            names.into_iter().map(RawEvent::RenamedTo).collect()
                        }
                        EventKind::Modify(ModifyKind::Name(RenameMode::Both))
                            if names.len() == 2 =>
                        {
                            vec![
                                RawEvent::RenamedFrom(names[0].clone()),
                                RawEvent::RenamedTo(names[1].clone()),
                            ]
                        }
                        // A rename whose sides we cannot pair: re-stat every name.
                        EventKind::Modify(_) | EventKind::Any | EventKind::Other => {
                            names.into_iter().map(RawEvent::Modified).collect()
                        }
                        EventKind::Access(_) => Vec::new(),
                    };
                    if !events.is_empty() {
                        let _ = tx.send(events);
                    }
                })
                .map_err(io::Error::other)?;
            watcher
                .watch(dir, RecursiveMode::NonRecursive)
                .map_err(io::Error::other)?;
            Ok(Self(watcher))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RawEvent::*;

    fn s(x: &str) -> String {
        x.to_string()
    }

    #[test]
    fn dedups_and_keeps_order() {
        let got = coalesce(vec![
            Added(s("a")),
            Modified(s("a")),
            Added(s("b")),
            Modified(s("a")),
        ]);
        assert_eq!(got, vec![Pending::Upsert(s("a")), Pending::Upsert(s("b"))]);
    }

    #[test]
    fn pairs_renames() {
        let got = coalesce(vec![RenamedFrom(s("a")), RenamedTo(s("b"))]);
        assert_eq!(
            got,
            vec![Pending::Renamed {
                from: s("a"),
                to: s("b")
            }]
        );
    }

    #[test]
    fn unpaired_rename_from_is_removal() {
        let got = coalesce(vec![RenamedFrom(s("a")), Added(s("c"))]);
        assert_eq!(got, vec![Pending::Removed(s("a")), Pending::Upsert(s("c"))]);
        let got = coalesce(vec![RenamedFrom(s("a"))]);
        assert_eq!(got, vec![Pending::Removed(s("a"))]);
    }

    #[test]
    fn unpaired_rename_to_is_addition() {
        assert_eq!(
            coalesce(vec![RenamedTo(s("b"))]),
            vec![Pending::Upsert(s("b"))]
        );
    }

    #[test]
    fn rename_then_delete_removes_original_row() {
        let got = coalesce(vec![
            RenamedFrom(s("a")),
            RenamedTo(s("b")),
            Removed(s("b")),
        ]);
        assert_eq!(got, vec![Pending::Removed(s("a"))]);
    }

    #[test]
    fn delete_then_create_is_upsert() {
        let got = coalesce(vec![Removed(s("a")), Added(s("a"))]);
        assert_eq!(got, vec![Pending::Upsert(s("a"))]);
    }

    #[test]
    fn overflow_wins() {
        let got = coalesce(vec![Added(s("a")), Overflow, Added(s("b"))]);
        assert_eq!(got, vec![Pending::Rescan]);
    }

    #[test]
    fn resolve_stats_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("here.txt"), b"1").unwrap();
        let got = resolve(
            dir.path(),
            vec![
                Pending::Upsert(s("here.txt")),
                Pending::Upsert(s("gone.txt")),
                Pending::Renamed {
                    from: s("old"),
                    to: s("here.txt"),
                },
            ],
        );
        assert!(matches!(&got[0], Change::Upsert { entry } if entry.name == "here.txt"));
        assert_eq!(
            got[1],
            Change::Removed {
                name: s("gone.txt")
            }
        );
        assert!(
            matches!(&got[2], Change::Renamed { from, entry } if from == "old" && entry.size == 1)
        );
    }

    #[test]
    fn watcher_reports_live_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let _w = DirWatcher::start(dir.path().to_path_buf(), move |c| {
            let _ = tx.send(c);
        })
        .unwrap();
        // Give FSEvents/inotify a moment to arm.
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(dir.path().join("new.txt"), b"hi").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut seen = false;
        while std::time::Instant::now() < deadline && !seen {
            if let Ok(changes) = rx.recv_timeout(Duration::from_millis(500)) {
                seen = changes.iter().any(|c| match c {
                    Change::Upsert { entry } => entry.name == "new.txt",
                    Change::Rescan => true,
                    _ => false,
                });
            }
        }
        assert!(seen);
    }
}
