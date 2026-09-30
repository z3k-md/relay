//! Git path helpers.
//!
//! Relay treats `.git` as ordinary files (D1). These helpers identify which
//! paths belong to a repository's mutable metadata so conflicts in one
//! repository can be grouped (D21).

use crate::path::LogicalPath;

/// Path of the `.git` directory `path` is inside or is, if any.
///
/// The innermost `.git` component wins so a nested repository is not attributed
/// to an outer one. The component must be exactly `.git`.
pub fn git_dir_of(path: &LogicalPath) -> Option<LogicalPath> {
    let comps: Vec<&str> = path.components().collect();
    let i = comps.iter().rposition(|c| *c == ".git")?;
    LogicalPath::from_components(comps[..=i].iter().copied()).ok()
}

/// Mutable Git metadata: inside a `.git` directory, excluding the directory
/// entry itself and anything under `<gitdir>/objects/`.
///
/// Object files are content-addressed and should not take part in
/// repository-level winner grouping. A file named `.gitignore` is not metadata.
pub fn is_git_metadata(path: &LogicalPath) -> bool {
    let Some(git_dir) = git_dir_of(path) else {
        return false;
    };
    if path == &git_dir {
        return false;
    }
    !matches!(git_dir.join("objects"), Ok(objects) if path.starts_with(&objects))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).unwrap()
    }

    #[test]
    fn git_dir_of_nested_and_root() {
        assert_eq!(git_dir_of(&lp(".git")).unwrap().as_str(), ".git");
        assert_eq!(git_dir_of(&lp(".git/HEAD")).unwrap().as_str(), ".git");
        assert_eq!(
            git_dir_of(&lp("repo/.git/refs/heads/main"))
                .unwrap()
                .as_str(),
            "repo/.git"
        );
        assert_eq!(
            git_dir_of(&lp("outer/inner/.git/HEAD")).unwrap().as_str(),
            "outer/inner/.git"
        );
        assert_eq!(
            git_dir_of(&lp("outer/.git/modules/inner/.git/HEAD"))
                .unwrap()
                .as_str(),
            "outer/.git/modules/inner/.git"
        );
        assert!(git_dir_of(&lp("src/foo.go")).is_none());
        assert!(git_dir_of(&lp(".gitignore")).is_none());
        assert!(git_dir_of(&lp("repo/.gitignore")).is_none());
    }

    #[test]
    fn is_git_metadata_excludes_objects_and_dot_git_dir() {
        assert!(!is_git_metadata(&lp(".git")));
        assert!(!is_git_metadata(&lp("repo/.git")));
        assert!(is_git_metadata(&lp(".git/HEAD")));
        assert!(is_git_metadata(&lp(".git/index")));
        assert!(is_git_metadata(&lp("repo/.git/refs/heads/main")));
        assert!(is_git_metadata(&lp("repo/.git/packed-refs")));
        assert!(is_git_metadata(&lp("nested/.git/config")));
        assert!(!is_git_metadata(&lp(".git/objects")));
        assert!(!is_git_metadata(&lp(".git/objects/ab/cd")));
        assert!(!is_git_metadata(&lp("repo/.git/objects/pack/foo.pack")));
        assert!(!is_git_metadata(&lp(".gitignore")));
        assert!(!is_git_metadata(&lp("repo/.gitignore")));
        assert!(!is_git_metadata(&lp("src/foo.go")));
        assert!(is_git_metadata(&lp(
            "repo/.git/refs/heads/main.relay-conflict-abababab-1"
        )));
    }
}
