use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use unicode_normalization::UnicodeNormalization;

use crate::error::CoreError;
use crate::reserved::{PortabilityIssue, component_issues};

/// Mount-relative path that identifies an entry independently of any OS.
///
/// Invariants (enforced by every constructor):
/// - relative, `/`-separated, no leading or trailing separator
/// - no empty, `.` or `..` components
/// - no NUL characters
/// - Unicode NFC, so the same name from macOS and Windows compares equal
///
/// Names that are legal here but cannot exist on some platform (Windows
/// reserved names, `:` in a component, ...) are still valid identities; see
/// [`LogicalPath::portability_issues`].
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogicalPath(String);

impl LogicalPath {
    pub fn new(raw: &str) -> Result<Self, CoreError> {
        let normalized: String = raw.nfc().collect();
        validate(&normalized)?;
        Ok(Self(normalized))
    }

    /// Build a path from already-split components (e.g. from a directory walk).
    pub fn from_components<I, S>(components: I) -> Result<Self, CoreError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut joined = String::new();
        for (i, c) in components.into_iter().enumerate() {
            if i > 0 {
                joined.push('/');
            }
            joined.push_str(c.as_ref());
        }
        Self::new(&joined)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    pub fn depth(&self) -> usize {
        self.components().count()
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    pub fn parent(&self) -> Option<LogicalPath> {
        self.0
            .rsplit_once('/')
            .map(|(parent, _)| LogicalPath(parent.to_owned()))
    }

    pub fn join(&self, component: &str) -> Result<LogicalPath, CoreError> {
        LogicalPath::new(&format!("{}/{}", self.0, component))
    }

    /// True if `self` is `prefix` or lies underneath it.
    pub fn starts_with(&self, prefix: &LogicalPath) -> bool {
        self.0 == prefix.0
            || (self.0.len() > prefix.0.len()
                && self.0.starts_with(&prefix.0)
                && self.0.as_bytes()[prefix.0.len()] == b'/')
    }

    /// Key under which two paths collide on a case-insensitive filesystem.
    pub fn case_fold_key(&self) -> String {
        self.0.to_lowercase()
    }

    /// Reasons this path cannot be materialized as-is on some supported platform.
    pub fn portability_issues(&self) -> Vec<PortabilityIssue> {
        self.components().flat_map(component_issues).collect()
    }
}

fn validate(path: &str) -> Result<(), CoreError> {
    let invalid = |reason| {
        Err(CoreError::InvalidPath {
            path: path.to_owned(),
            reason,
        })
    };
    if path.is_empty() {
        return invalid("path is empty");
    }
    if path.contains('\0') {
        return invalid("path contains NUL");
    }
    if path.starts_with('/') {
        return invalid("path must be relative");
    }
    if path.ends_with('/') {
        return invalid("path has a trailing separator");
    }
    for component in path.split('/') {
        match component {
            "" => return invalid("path has an empty component"),
            "." | ".." => return invalid("path has a '.' or '..' component"),
            _ => {}
        }
    }
    Ok(())
}

impl fmt::Display for LogicalPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for LogicalPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LogicalPath({:?})", self.0)
    }
}

impl std::str::FromStr for LogicalPath {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl AsRef<str> for LogicalPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for LogicalPath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for LogicalPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        LogicalPath::new(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn accepts_ordinary_paths() {
        let p = LogicalPath::new("game/inventory/Core.lua").unwrap();
        assert_eq!(p.file_name(), "Core.lua");
        assert_eq!(p.parent().unwrap().as_str(), "game/inventory");
        assert_eq!(p.depth(), 3);
        assert!(LogicalPath::new("single").unwrap().parent().is_none());
    }

    #[test]
    fn rejects_non_canonical_paths() {
        for bad in [
            "",
            "/abs",
            "trailing/",
            "a//b",
            "a/./b",
            "../up",
            "a/..",
            "nul\0x",
        ] {
            assert!(LogicalPath::new(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn nfd_and_nfc_spellings_are_the_same_identity() {
        let nfc = LogicalPath::new("caf\u{e9}.txt").unwrap();
        let nfd = LogicalPath::new("cafe\u{301}.txt").unwrap();
        assert_eq!(nfc, nfd);
    }

    #[test]
    fn starts_with_respects_component_boundaries() {
        let root = LogicalPath::new("code/game").unwrap();
        assert!(LogicalPath::new("code/game").unwrap().starts_with(&root));
        assert!(
            LogicalPath::new("code/game/a.lua")
                .unwrap()
                .starts_with(&root)
        );
        assert!(!LogicalPath::new("code/gamer").unwrap().starts_with(&root));
    }

    #[test]
    fn case_fold_detects_collisions() {
        let a = LogicalPath::new("Foo.lua").unwrap();
        let b = LogicalPath::new("foo.lua").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.case_fold_key(), b.case_fold_key());
    }

    proptest! {
        #[test]
        fn normalization_is_idempotent(raw in "[a-zA-Z0-9é\u{301}_.-]{1,12}(/[a-zA-Z0-9é\u{301}_-]{1,12}){0,4}") {
            if let Ok(p) = LogicalPath::new(&raw) {
                let again = LogicalPath::new(p.as_str()).unwrap();
                prop_assert_eq!(p, again);
            }
        }
    }
}
