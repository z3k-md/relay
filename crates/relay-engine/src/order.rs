//! Sender and receiver apply the same entry order (D17).

use relay_core::{EntryContent, EntryKind, LogicalPath};

/// How an entry is classified for apply / sequence assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyClass {
    FileTombstone,
    DirTombstone,
    Directory,
    FileOrLink,
}

/// Rank used to sort a batch or a scan's writes.
///
/// Order: file tombstones; directory tombstones deepest-first; directory
/// creations shallowest-first; file/symlink writes outside `.git`; `.git`
/// content that is not a ref; `.git` refs last.
pub fn apply_sort_key(path: &LogicalPath, class: ApplyClass) -> (u8, i32, &str) {
    match class {
        ApplyClass::FileTombstone => (0, 0, path.as_str()),
        ApplyClass::DirTombstone => (1, -(path.depth() as i32), path.as_str()),
        ApplyClass::Directory => (2, path.depth() as i32, path.as_str()),
        ApplyClass::FileOrLink => {
            let band = if is_git_ref(path) {
                5
            } else if is_under_git(path) {
                4
            } else {
                3
            };
            (band, 0, path.as_str())
        }
    }
}

pub fn classify_apply(content: &EntryContent, previous_kind: Option<EntryKind>) -> ApplyClass {
    match content {
        EntryContent::Deleted => match previous_kind {
            Some(EntryKind::Directory) => ApplyClass::DirTombstone,
            _ => ApplyClass::FileTombstone,
        },
        EntryContent::Directory => ApplyClass::Directory,
        EntryContent::File { .. } | EntryContent::Symlink { .. } => ApplyClass::FileOrLink,
    }
}

/// `HEAD`, `ORIG_HEAD`, `FETCH_HEAD`, `MERGE_HEAD`, `packed-refs`, `index`,
/// and `refs/**` relative to any `.git` directory at any depth.
pub fn is_git_ref(path: &LogicalPath) -> bool {
    let Some(rel) = git_relative(path) else {
        return false;
    };
    matches!(
        rel.as_slice(),
        ["HEAD"] | ["ORIG_HEAD"] | ["FETCH_HEAD"] | ["MERGE_HEAD"] | ["packed-refs"] | ["index"]
    ) || rel.first() == Some(&"refs")
}

fn is_under_git(path: &LogicalPath) -> bool {
    git_relative(path).is_some()
}

fn git_relative(path: &LogicalPath) -> Option<Vec<&str>> {
    let comps: Vec<&str> = path.components().collect();
    for (i, c) in comps.iter().enumerate() {
        if *c == ".git" && i + 1 < comps.len() {
            return Some(comps[i + 1..].to_vec());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_core::LogicalPath;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).unwrap()
    }

    fn sorted(items: &[(&str, ApplyClass)]) -> Vec<String> {
        let mut keyed: Vec<_> = items
            .iter()
            .map(|(p, c)| {
                let path = lp(p);
                let key = apply_sort_key(&path, *c);
                ((key.0, key.1, key.2.to_owned()), p.to_string())
            })
            .collect();
        keyed.sort();
        keyed.into_iter().map(|(_, p)| p).collect()
    }

    #[test]
    fn d17_order_tombstones_dirs_files_then_git() {
        let order = sorted(&[
            (".git/HEAD", ApplyClass::FileOrLink),
            (".git/refs/heads/main", ApplyClass::FileOrLink),
            (".git/objects/ab/cd", ApplyClass::FileOrLink),
            ("src/a.rs", ApplyClass::FileOrLink),
            ("src", ApplyClass::Directory),
            ("old/dir", ApplyClass::DirTombstone),
            ("old/dir/nested", ApplyClass::DirTombstone),
            ("gone.txt", ApplyClass::FileTombstone),
            (".git", ApplyClass::Directory),
            (".git/index", ApplyClass::FileOrLink),
            ("nested/.git/FETCH_HEAD", ApplyClass::FileOrLink),
            ("nested/.git/config", ApplyClass::FileOrLink),
        ]);
        assert_eq!(
            order,
            vec![
                "gone.txt",
                "old/dir/nested",
                "old/dir",
                ".git",
                "src",
                "src/a.rs",
                ".git/objects/ab/cd",
                "nested/.git/config",
                ".git/HEAD",
                ".git/index",
                ".git/refs/heads/main",
                "nested/.git/FETCH_HEAD",
            ]
        );
    }

    #[test]
    fn classify_uses_previous_kind_for_tombstones() {
        assert_eq!(
            classify_apply(&EntryContent::Deleted, Some(EntryKind::Directory)),
            ApplyClass::DirTombstone
        );
        assert_eq!(
            classify_apply(&EntryContent::Deleted, Some(EntryKind::File)),
            ApplyClass::FileTombstone
        );
        assert_eq!(
            classify_apply(&EntryContent::Deleted, None),
            ApplyClass::FileTombstone
        );
        assert_eq!(
            classify_apply(&EntryContent::Directory, None),
            ApplyClass::Directory
        );
    }
}
