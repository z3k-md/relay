//! Names that are valid logical identities but cannot be created on some
//! supported platform.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortabilityIssue {
    WindowsReservedName(String),
    WindowsForbiddenChar { component: String, ch: char },
    WindowsTrailingDotOrSpace(String),
    ComponentTooLong(String),
}

impl fmt::Display for PortabilityIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WindowsReservedName(c) => write!(f, "{c:?} is a reserved name on Windows"),
            Self::WindowsForbiddenChar { component, ch } => {
                write!(f, "{component:?} contains {ch:?}, which Windows forbids")
            }
            Self::WindowsTrailingDotOrSpace(c) => {
                write!(f, "{c:?} ends with a dot or space, which Windows strips")
            }
            Self::ComponentTooLong(c) => write!(f, "{c:?} is longer than 255 UTF-16 units"),
        }
    }
}

const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "COM¹", "COM²", "COM³", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
    "LPT9", "LPT¹", "LPT²", "LPT³",
];

const WINDOWS_FORBIDDEN: &[char] = &['<', '>', ':', '"', '\\', '|', '?', '*'];

pub(crate) fn component_issues(component: &str) -> Vec<PortabilityIssue> {
    let mut issues = Vec::new();

    // Windows treats "CON.txt" and "con .lua" as the device too.
    let stem = component.split('.').next().unwrap_or(component).trim_end();
    if WINDOWS_RESERVED
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(stem))
    {
        issues.push(PortabilityIssue::WindowsReservedName(component.to_owned()));
    }

    if let Some(ch) = component
        .chars()
        .find(|c| WINDOWS_FORBIDDEN.contains(c) || (*c as u32) < 0x20)
    {
        issues.push(PortabilityIssue::WindowsForbiddenChar {
            component: component.to_owned(),
            ch,
        });
    }

    if component.ends_with('.') || component.ends_with(' ') {
        issues.push(PortabilityIssue::WindowsTrailingDotOrSpace(
            component.to_owned(),
        ));
    }

    if component.encode_utf16().count() > 255 {
        issues.push(PortabilityIssue::ComponentTooLong(component.to_owned()));
    }

    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LogicalPath;

    fn issues(p: &str) -> Vec<PortabilityIssue> {
        LogicalPath::new(p).unwrap().portability_issues()
    }

    #[test]
    fn flags_windows_reserved_names() {
        assert!(!issues("src/con.txt").is_empty());
        assert!(!issues("AUX").is_empty());
        assert!(!issues("lpt1.log").is_empty());
        assert!(issues("console.lua").is_empty());
        assert!(issues("icon.png").is_empty());
    }

    #[test]
    fn flags_forbidden_characters_and_trailing_dots() {
        assert!(!issues("notes/what?.md").is_empty());
        assert!(!issues("a:b").is_empty());
        assert!(!issues("dir./file").is_empty());
        assert!(!issues("name ").is_empty());
        assert!(issues("game/inventory/Core.lua").is_empty());
    }
}
