//! Include/exclude rule evaluation for a Relay mount.
//!
//! Patterns are globs matched against the mount-relative [`LogicalPath`]
//! string (always `/`-separated). `*` does not cross `/`; `**` matches any
//! number of components, including zero.

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use relay_core::{EntryKind, LogicalPath};
use thiserror::Error;

/// Patterns that are always applied and cannot be removed by user rules.
///
/// The mount marker and in-flight temp files use the same names as
/// [`relay_core::MOUNT_MARKER`] and [`relay_core::TEMP_PREFIX`].
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".relay-mount",
    "**/.relay-tmp-*",
    "**/.git/**/*.lock",
    "**/.git/gc.pid",
    "**/.git/gc.log",
    "**/.git/fsmonitor--daemon*",
    "**/.git/fsmonitor--daemon/**",
    "**/.DS_Store",
    "**/Thumbs.db",
    "**/desktop.ini",
    "**/.~lock.*#",
    "**/~$*",
    "**/.*.sw?",
    "**/.#*",
    "**/*___jb_tmp___",
    "**/*___jb_old___",
];

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PolicyError {
    #[error("invalid glob pattern {pattern:?}: {message}")]
    InvalidPattern { pattern: String, message: String },
}

/// Compiled include/exclude rules for one mount.
///
/// User-supplied includes and excludes are stored as given (after empty
/// includes are replaced with `["**"]`). [`DEFAULT_EXCLUDES`] are compiled
/// into the matcher but are not returned by [`MountRules::excludes`].
///
/// Globs are case-insensitive on macOS and Windows (those platforms' default
/// filesystems) and case-sensitive elsewhere. Use
/// [`MountRules::with_case_insensitive`] to override the platform default.
#[derive(Clone, Debug)]
pub struct MountRules {
    includes: Vec<String>,
    excludes: Vec<String>,
    include_set: GlobSet,
    exclude_set: GlobSet,
    /// Globs `P` taken from exclude patterns of the form `P/**`.
    exclude_prefix_set: GlobSet,
    /// Globs `P` taken from include patterns of the form `P/**`.
    include_prefix_set: GlobSet,
    case_insensitive: bool,
}

impl MountRules {
    /// Compile `includes` and `excludes` with the platform default case
    /// sensitivity (insensitive on macOS and Windows).
    ///
    /// Empty `includes` becomes `["**"]`. [`DEFAULT_EXCLUDES`] are always
    /// appended to the compiled exclude set.
    pub fn new(includes: &[String], excludes: &[String]) -> Result<Self, PolicyError> {
        Self::with_case_insensitive(includes, excludes, default_case_insensitive())
    }

    /// Compile `includes` and `excludes`, forcing glob case sensitivity.
    ///
    /// Empty `includes` becomes `["**"]`. [`DEFAULT_EXCLUDES`] are always
    /// appended to the compiled exclude set.
    pub fn with_case_insensitive(
        includes: &[String],
        excludes: &[String],
        case_insensitive: bool,
    ) -> Result<Self, PolicyError> {
        let includes = if includes.is_empty() {
            vec!["**".to_owned()]
        } else {
            includes.to_vec()
        };
        let excludes = excludes.to_vec();

        let include_set = compile_set(&includes, case_insensitive)?;
        let include_prefix_set = compile_prefix_set(&includes, case_insensitive)?;

        let mut all_excludes = Vec::with_capacity(DEFAULT_EXCLUDES.len() + excludes.len());
        all_excludes.extend(DEFAULT_EXCLUDES.iter().map(|s| (*s).to_owned()));
        all_excludes.extend(excludes.iter().cloned());
        let exclude_set = compile_set(&all_excludes, case_insensitive)?;
        let exclude_prefix_set = compile_prefix_set(&all_excludes, case_insensitive)?;

        Ok(Self {
            includes,
            excludes,
            include_set,
            exclude_set,
            exclude_prefix_set,
            include_prefix_set,
            case_insensitive,
        })
    }

    /// Whether compiled globs match case-insensitively.
    pub fn case_insensitive(&self) -> bool {
        self.case_insensitive
    }

    /// User include patterns as stored (empty input is normalized to `["**"]`).
    pub fn includes(&self) -> &[String] {
        &self.includes
    }

    /// User exclude patterns only; does not include [`DEFAULT_EXCLUDES`].
    pub fn excludes(&self) -> &[String] {
        &self.excludes
    }

    /// Whether a scan should descend into this directory at all.
    ///
    /// Returns false if an exclude pattern matches `dir` itself, or an exclude
    /// of the form `P/**` has `P` matching `dir`. Includes never prune.
    pub fn should_descend(&self, dir: &LogicalPath) -> bool {
        let s = dir.as_str();
        !self.exclude_set.is_match(s) && !self.exclude_prefix_set.is_match(s)
    }

    /// Whether this entry is part of the mount.
    ///
    /// Exclude wins over include. A path is excluded when an exclude pattern
    /// matches it, or any ancestor fails [`Self::should_descend`] (the path
    /// lies under a pruned directory).
    ///
    /// Directory selection (deterministic, documented rule):
    /// a [`EntryKind::Directory`] is selected if and only if it is not
    /// excluded and at least one of the following holds:
    /// - an include glob matches the directory path
    /// - the includes contain the catch-all pattern `**`
    /// - some include of the form `P/**` literal-prefix-matches the directory
    ///   (`dir == P` or `dir` starts with `P/`) or the glob `P` matches `dir`
    ///   (so `**/docs/**` keeps `a/docs` as well as `docs`)
    ///
    /// Files and symlinks are selected only when an include glob matches.
    pub fn is_selected(&self, path: &LogicalPath, kind: EntryKind) -> bool {
        if self.is_excluded(path) {
            return false;
        }
        match kind {
            EntryKind::File | EntryKind::Symlink => self.include_set.is_match(path.as_str()),
            EntryKind::Directory => self.directory_included(path),
        }
    }

    /// Rebuild these rules with additional user exclude patterns (e.g. from
    /// `.relayignore`). [`DEFAULT_EXCLUDES`] are still applied.
    pub fn with_extra_excludes(&self, extra: &[String]) -> Result<Self, PolicyError> {
        let mut excludes = self.excludes.clone();
        excludes.extend(extra.iter().cloned());
        Self::with_case_insensitive(&self.includes, &excludes, self.case_insensitive)
    }

    fn is_excluded(&self, path: &LogicalPath) -> bool {
        if self.exclude_set.is_match(path.as_str()) {
            return true;
        }
        let mut current = path.parent();
        while let Some(ancestor) = current {
            if !self.should_descend(&ancestor) {
                return true;
            }
            current = ancestor.parent();
        }
        false
    }

    fn directory_included(&self, dir: &LogicalPath) -> bool {
        let s = dir.as_str();
        if self.include_set.is_match(s) {
            return true;
        }
        if self.includes.iter().any(|p| p == "**") {
            return true;
        }
        if self.include_prefix_set.is_match(s) {
            return true;
        }
        for inc in &self.includes {
            if let Some(prefix) = inc.strip_suffix("/**")
                && prefix_matches(prefix, s)
            {
                return true;
            }
        }
        false
    }
}

/// One pattern per line: trim, drop a trailing `\r`, skip blanks and `#` comments.
pub fn parse_relayignore(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

fn default_case_insensitive() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

fn compile_glob(pattern: &str, case_insensitive: bool) -> Result<Glob, PolicyError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|err| PolicyError::InvalidPattern {
            pattern: pattern.to_owned(),
            message: err.to_string(),
        })
}

fn compile_set(patterns: &[String], case_insensitive: bool) -> Result<GlobSet, PolicyError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(compile_glob(pattern, case_insensitive)?);
    }
    builder.build().map_err(|err| PolicyError::InvalidPattern {
        pattern: patterns.join(","),
        message: err.to_string(),
    })
}

fn compile_prefix_set(patterns: &[String], case_insensitive: bool) -> Result<GlobSet, PolicyError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let Some(prefix) = pattern.strip_suffix("/**") else {
            continue;
        };
        if prefix.is_empty() {
            continue;
        }
        builder.add(compile_glob(prefix, case_insensitive)?);
    }
    builder.build().map_err(|err| PolicyError::InvalidPattern {
        pattern: patterns.join(","),
        message: err.to_string(),
    })
}

fn prefix_matches(prefix: &str, path: &str) -> bool {
    path == prefix
        || (path.len() > prefix.len()
            && path.starts_with(prefix)
            && path.as_bytes()[prefix.len()] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).unwrap()
    }

    fn rules(includes: &[&str], excludes: &[&str]) -> MountRules {
        rules_case(includes, excludes, false)
    }

    fn rules_case(includes: &[&str], excludes: &[&str], case_insensitive: bool) -> MountRules {
        MountRules::with_case_insensitive(
            &includes.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            &excludes.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            case_insensitive,
        )
        .unwrap()
    }

    fn default_rules() -> MountRules {
        rules(&[], &[])
    }

    #[test]
    fn default_excludes_match_core_constants() {
        assert_eq!(DEFAULT_EXCLUDES[0], relay_core::MOUNT_MARKER);
        assert!(
            DEFAULT_EXCLUDES[1].contains(relay_core::TEMP_PREFIX),
            "temp exclude should mention {}",
            relay_core::TEMP_PREFIX
        );
    }

    #[test]
    fn empty_includes_become_globstar() {
        let r = default_rules();
        assert_eq!(r.includes(), &["**".to_owned()]);
        assert!(r.excludes().is_empty());
    }

    #[test]
    fn default_excludes_are_not_listed_as_user_excludes() {
        let r = rules(&[], &["**/secret/**"]);
        assert_eq!(r.excludes(), &["**/secret/**".to_owned()]);
    }

    #[test]
    fn node_modules_and_target_excluded_at_any_depth() {
        let r = rules(&[], &["**/node_modules/**", "**/target/**"]);
        for path in [
            "node_modules/pkg/index.js",
            "a/node_modules/pkg/index.js",
            "target/debug/foo",
            "crates/relay-fs/target/foo",
        ] {
            assert!(
                !r.is_selected(&lp(path), EntryKind::File),
                "{path} should be excluded"
            );
        }
        assert!(!r.should_descend(&lp("node_modules")));
        assert!(!r.should_descend(&lp("a/node_modules")));
        assert!(!r.should_descend(&lp("target")));
        assert!(!r.should_descend(&lp("crates/relay-fs/target")));
    }

    #[test]
    fn git_content_is_selected_but_lock_files_are_not() {
        let r = default_rules();
        for path in [".git/HEAD", ".git/objects/ab/cd", ".git/refs/heads/main"] {
            assert!(
                r.is_selected(&lp(path), EntryKind::File),
                "{path} should be selected"
            );
        }
        assert!(r.is_selected(&lp(".git"), EntryKind::Directory));
        assert!(r.should_descend(&lp(".git")));

        for path in [".git/index.lock", ".git/refs/heads/main.lock"] {
            assert!(
                !r.is_selected(&lp(path), EntryKind::File),
                "{path} should be excluded"
            );
        }
    }

    #[test]
    fn star_does_not_cross_directories() {
        let r = rules(&["*.md"], &[]);
        assert!(r.is_selected(&lp("readme.md"), EntryKind::File));
        assert!(!r.is_selected(&lp("docs/readme.md"), EntryKind::File));
    }

    #[test]
    fn include_subset_selects_only_named_trees() {
        let r = rules(&["Residency/**", "Notes/**"], &[]);
        assert!(r.is_selected(&lp("Residency/a.md"), EntryKind::File));
        assert!(r.is_selected(&lp("Notes/todo.txt"), EntryKind::File));
        assert!(!r.is_selected(&lp("Other/a.md"), EntryKind::File));
        assert!(r.is_selected(&lp("Residency"), EntryKind::Directory));
        assert!(r.is_selected(&lp("Residency/sub"), EntryKind::Directory));
        assert!(r.is_selected(&lp("Notes"), EntryKind::Directory));
        assert!(!r.is_selected(&lp("Other"), EntryKind::Directory));
        // includes do not prune descent into unselected siblings
        assert!(r.should_descend(&lp("Other")));
    }

    #[test]
    fn directory_selection_rule() {
        let catch_all = default_rules();
        assert!(catch_all.is_selected(&lp("any/dir"), EntryKind::Directory));

        let nested = rules(&["**/docs/**"], &[]);
        assert!(nested.is_selected(&lp("docs"), EntryKind::Directory));
        assert!(nested.is_selected(&lp("a/docs"), EntryKind::Directory));
        assert!(nested.is_selected(&lp("a/docs/api"), EntryKind::Directory));
        assert!(!nested.is_selected(&lp("a/other"), EntryKind::Directory));
    }

    #[test]
    fn exclude_wins_over_include() {
        let r = rules(&["**"], &["secret/**"]);
        assert!(!r.is_selected(&lp("secret/key"), EntryKind::File));
        assert!(!r.should_descend(&lp("secret")));
    }

    #[test]
    fn default_junk_and_marker_are_excluded() {
        let r = default_rules();
        assert!(!r.is_selected(&lp(".relay-mount"), EntryKind::File));
        assert!(!r.is_selected(&lp(".DS_Store"), EntryKind::File));
        assert!(!r.is_selected(&lp("foo/.DS_Store"), EntryKind::File));
        assert!(!r.is_selected(&lp("docs/Thumbs.db"), EntryKind::File));
        assert!(!r.is_selected(&lp("docs/desktop.ini"), EntryKind::File));
        assert!(!r.is_selected(&lp(".relay-tmp-abc"), EntryKind::File));
        assert!(!r.is_selected(&lp("dir/.relay-tmp-xyz"), EntryKind::File));
        assert!(!r.is_selected(&lp("~$notes.docx"), EntryKind::File));
        assert!(!r.is_selected(&lp(".git/gc.pid"), EntryKind::File));
        assert!(!r.is_selected(&lp(".git/gc.log"), EntryKind::File));
    }

    #[test]
    fn editor_temp_files_are_excluded() {
        let r = default_rules();
        for path in [".Core.lua.swp", ".#notes.md", "Core.lua___jb_tmp___"] {
            assert!(
                !r.is_selected(&lp(path), EntryKind::File),
                "{path} should be excluded"
            );
        }
        for path in ["Core.lua", "swap.md", "a.swift"] {
            assert!(
                r.is_selected(&lp(path), EntryKind::File),
                "{path} should still be selected"
            );
        }
        assert!(!r.is_selected(&lp("dir/.Core.lua.swo"), EntryKind::File));
        assert!(!r.is_selected(&lp("dir/.#notes.md"), EntryKind::File));
        assert!(!r.is_selected(&lp("dir/Core.lua___jb_old___"), EntryKind::File));
    }

    #[test]
    fn parse_relayignore_skips_comments_blanks_and_crlf() {
        let text = "# header\r\n*.tmp\r\n\r\n  # indented comment\r\nsecret/**\r\nfoo\r\n";
        assert_eq!(parse_relayignore(text), vec!["*.tmp", "secret/**", "foo"]);
        assert_eq!(parse_relayignore(""), Vec::<String>::new());
        assert_eq!(parse_relayignore("# only\n\n"), Vec::<String>::new());
    }

    #[test]
    fn with_extra_excludes_adds_user_patterns() {
        let base = rules(&["**"], &["*.bak"]);
        let extra = parse_relayignore("*.tmp\nsecret/**\n");
        let combined = base.with_extra_excludes(&extra).unwrap();
        assert_eq!(
            combined.excludes(),
            &[
                "*.bak".to_owned(),
                "*.tmp".to_owned(),
                "secret/**".to_owned()
            ]
        );
        assert!(!combined.is_selected(&lp("notes.tmp"), EntryKind::File));
        assert!(!combined.is_selected(&lp("secret/a"), EntryKind::File));
        assert!(combined.is_selected(&lp("notes.md"), EntryKind::File));
        assert_eq!(combined.includes(), base.includes());
    }

    #[test]
    fn with_extra_excludes_preserves_case_insensitivity() {
        let base = rules_case(&["**"], &["*.bak"], true);
        assert!(base.case_insensitive());
        let combined = base.with_extra_excludes(&["*.tmp".to_owned()]).unwrap();
        assert!(combined.case_insensitive());
        assert!(!combined.is_selected(&lp("Notes.BAK"), EntryKind::File));
        assert!(!combined.is_selected(&lp("Notes.TMP"), EntryKind::File));
    }

    #[test]
    fn git_lock_and_junk_defaults_are_case_insensitive_when_asked() {
        let sensitive = default_rules();
        let insensitive = rules_case(&[], &[], true);

        for path in [".GIT/INDEX.LOCK", ".git/refs/heads/Main.LOCK"] {
            assert!(
                sensitive.is_selected(&lp(path), EntryKind::File),
                "{path} should remain selected when case-sensitive"
            );
            assert!(
                !insensitive.is_selected(&lp(path), EntryKind::File),
                "{path} should be excluded when case-insensitive"
            );
        }

        for path in ["thumbs.db", "docs/thumbs.db", ".ds_store", "foo/.ds_store"] {
            assert!(
                sensitive.is_selected(&lp(path), EntryKind::File),
                "{path} should remain selected when case-sensitive"
            );
            assert!(
                !insensitive.is_selected(&lp(path), EntryKind::File),
                "{path} should be excluded when case-insensitive"
            );
        }
    }

    #[test]
    fn user_exclude_prefix_is_case_insensitive_when_asked() {
        let sensitive = rules(&[], &["**/Build/**"]);
        let insensitive = rules_case(&[], &["**/Build/**"], true);
        assert!(sensitive.should_descend(&lp("build")));
        assert!(sensitive.is_selected(&lp("build/a.rs"), EntryKind::File));
        assert!(!insensitive.should_descend(&lp("build")));
        assert!(!insensitive.is_selected(&lp("build/a.rs"), EntryKind::File));
        assert!(!sensitive.should_descend(&lp("Build")));
        assert!(!insensitive.should_descend(&lp("Build")));
    }

    #[test]
    fn include_patterns_are_case_insensitive_when_asked() {
        let sensitive = rules(&["Notes/**"], &[]);
        let insensitive = rules_case(&["Notes/**"], &[], true);
        assert!(sensitive.is_selected(&lp("Notes/a.md"), EntryKind::File));
        assert!(!sensitive.is_selected(&lp("notes/a.md"), EntryKind::File));
        assert!(insensitive.is_selected(&lp("Notes/a.md"), EntryKind::File));
        assert!(insensitive.is_selected(&lp("notes/a.md"), EntryKind::File));
        assert!(insensitive.is_selected(&lp("notes"), EntryKind::Directory));
        assert!(!insensitive.is_selected(&lp("other/a.md"), EntryKind::File));
    }

    #[test]
    fn invalid_glob_is_reported() {
        let err = MountRules::new(&["[".to_owned()], &[]).unwrap_err();
        match err {
            PolicyError::InvalidPattern { pattern, message } => {
                assert_eq!(pattern, "[");
                assert!(!message.is_empty());
            }
        }
    }
}
