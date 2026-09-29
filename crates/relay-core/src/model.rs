use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::ids::{DeviceId, MountId, SpaceId};

/// A logical synchronization namespace (e.g. "Personal", "Work").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Space {
    pub id: SpaceId,
    pub name: String,
}

/// One logical root inside a Space (e.g. `code/`). Each device maps it to its
/// own physical directory; that mapping is device-local, not part of identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub id: MountId,
    pub space: SpaceId,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
}

/// Validate a Space or Mount name: non-empty, no path separators, no control
/// characters, at most 64 characters.
pub fn validate_name(name: &str) -> Result<(), CoreError> {
    let invalid = |reason| {
        Err(CoreError::InvalidName {
            name: name.to_owned(),
            reason,
        })
    };
    if name.trim().is_empty() {
        return invalid("name is empty");
    }
    if name.chars().count() > 64 {
        return invalid("name is longer than 64 characters");
    }
    if name.contains(['/', '\\']) {
        return invalid("name contains a path separator");
    }
    if name.chars().any(char::is_control) {
        return invalid("name contains a control character");
    }
    if name != name.trim() {
        return invalid("name has leading or trailing whitespace");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("Personal").is_ok());
        assert!(validate_name("game mods").is_ok());
        for bad in ["", "  ", "a/b", "a\\b", " pad", "tab\t", &"x".repeat(65)] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }
}
