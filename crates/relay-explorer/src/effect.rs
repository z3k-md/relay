//! Which drop effect to show, following Explorer's rules.

use std::path::{Component, Path, Prefix};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Keys {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// Ctrl copies and Shift moves. Alt (or Ctrl+Shift) means "create shortcut",
/// which the preview does not do yet. With no modifier, Explorer moves within
/// a volume and copies across volumes. Dropping items onto the folder they
/// already live in does nothing.
pub fn choose(
    keys: Keys,
    allow_copy: bool,
    allow_move: bool,
    same_volume: bool,
    already_there: bool,
) -> Effect {
    if already_there || keys.alt || (keys.ctrl && keys.shift) {
        return Effect::None;
    }
    let wanted = if keys.ctrl {
        Effect::Copy
    } else if keys.shift || same_volume {
        Effect::Move
    } else {
        Effect::Copy
    };
    match wanted {
        Effect::Move if allow_move => Effect::Move,
        Effect::Copy if allow_copy => Effect::Copy,
        // The source refused the natural choice; offer the other if allowed
        // and no modifier asked for a specific one.
        Effect::Move if allow_copy && !keys.shift => Effect::Copy,
        Effect::Copy if allow_move && !keys.ctrl => Effect::Move,
        _ => Effect::None,
    }
}

/// Whether two paths are on the same drive or share (`C:` vs `D:`,
/// `\\server\share`). Always true on platforms without prefixes.
pub fn same_volume(a: &Path, b: &Path) -> bool {
    fn volume(p: &Path) -> Option<String> {
        match p.components().next()? {
            Component::Prefix(prefix) => Some(match prefix.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                    (d as char).to_ascii_uppercase().to_string()
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
                    "\\\\{}\\{}",
                    server.to_string_lossy().to_lowercase(),
                    share.to_string_lossy().to_lowercase()
                ),
                other => format!("{other:?}"),
            }),
            _ => None,
        }
    }
    volume(a) == volume(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Keys = Keys {
        ctrl: false,
        shift: false,
        alt: false,
    };

    #[test]
    fn defaults_follow_volume() {
        assert_eq!(choose(NONE, true, true, true, false), Effect::Move);
        assert_eq!(choose(NONE, true, true, false, false), Effect::Copy);
    }

    #[test]
    fn modifiers_win() {
        let ctrl = Keys { ctrl: true, ..NONE };
        let shift = Keys {
            shift: true,
            ..NONE
        };
        assert_eq!(choose(ctrl, true, true, true, false), Effect::Copy);
        assert_eq!(choose(shift, true, true, false, false), Effect::Move);
        assert_eq!(
            choose(Keys { alt: true, ..NONE }, true, true, true, false),
            Effect::None
        );
    }

    #[test]
    fn respects_what_the_source_allows() {
        // Outlook allows copy only.
        assert_eq!(choose(NONE, true, false, true, false), Effect::Copy);
        let shift = Keys {
            shift: true,
            ..NONE
        };
        assert_eq!(choose(shift, true, false, true, false), Effect::None);
    }

    #[test]
    fn no_op_onto_own_folder() {
        assert_eq!(choose(NONE, true, true, true, true), Effect::None);
    }

    #[cfg(windows)]
    #[test]
    fn volumes() {
        assert!(same_volume(Path::new(r"C:\a"), Path::new(r"c:\b\c")));
        assert!(!same_volume(Path::new(r"C:\a"), Path::new(r"D:\a")));
        assert!(same_volume(
            Path::new(r"\\Srv\Share\a"),
            Path::new(r"\\srv\share\b")
        ));
        assert!(!same_volume(
            Path::new(r"\\srv\one\a"),
            Path::new(r"\\srv\two\a")
        ));
    }
}
