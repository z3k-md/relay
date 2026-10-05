//! Pure domain types for Relay.
//!
//! Nothing in this crate touches the filesystem, network, or database. Every
//! other crate speaks in these types.

pub mod addr;
pub mod config;
pub mod conflict;
pub mod entry;
pub mod error;
pub mod faults;
pub mod git;
pub mod ids;
pub mod local;
pub mod merge;
pub mod model;
pub mod pairing;
pub mod path;
pub mod remote;
pub mod reserved;
pub mod speed;
pub mod version;

pub use addr::{
    MAX_PEER_ADDRESSES, collect_peer_addresses, format_ip_port, format_socket_addr,
    is_advertisable_ip, is_tailscale_v4, is_tailscale_v6, merge_peer_addresses, rank_addresses,
};
pub use config::{ConfigApplied, ConfigChange, DeleteHoldDecision};
pub use entry::{EntryContent, EntryKey, EntryKind, EntryRecord, StatHint};
pub use error::CoreError;
pub use git::{git_dir_of, is_git_metadata};
pub use ids::{DeviceId, MaterializationRuleId, MountId, ObjectId, PolicyId, Sequence, SpaceId};
pub use local::{LocalChange, Observation, derive_local_change, needs_rehash};
pub use merge::{MergeOutcome, merge_text};
pub use model::{Device, Mount, Space, validate_name};
pub use pairing::PairingCode;
pub use path::LogicalPath;
pub use reserved::PortabilityIssue;
pub use version::{VectorOrdering, VersionRelation, VersionVector, compare_versions};

/// File at the root of every materialized mount holding its Space and Mount
/// ids. A scan refuses to run if it is missing, so an unmounted drive or a
/// moved folder is never mistaken for "every file was deleted".
///
/// Local bookkeeping only: never indexed, never replicated, never materialized
/// from a peer as user content.
pub const MOUNT_MARKER: &str = ".relay-mount";

/// Prefix for in-flight files written next to their destination so the final
/// rename stays on one volume. Never indexed.
pub const TEMP_PREFIX: &str = ".relay-tmp-";

/// True when a single path component is Relay bookkeeping (mount marker or
/// in-flight temp), not user content.
pub fn is_bookkeeping_component(name: &str) -> bool {
    name == MOUNT_MARKER || name.starts_with(TEMP_PREFIX)
}

/// True when any component of `path` is Relay bookkeeping.
pub fn is_bookkeeping_path(path: &LogicalPath) -> bool {
    path.components().any(is_bookkeeping_component)
}

#[cfg(test)]
mod bookkeeping_tests {
    use super::*;

    #[test]
    fn bookkeeping_helpers() {
        assert!(is_bookkeeping_component(MOUNT_MARKER));
        assert!(is_bookkeeping_component(&format!("{TEMP_PREFIX}abc")));
        assert!(!is_bookkeeping_component("notes.txt"));
        assert!(is_bookkeeping_path(
            &LogicalPath::new(MOUNT_MARKER).unwrap()
        ));
        assert!(is_bookkeeping_path(
            &LogicalPath::new(&format!("dir/{MOUNT_MARKER}")).unwrap()
        ));
        assert!(is_bookkeeping_path(
            &LogicalPath::new(&format!("dir/{TEMP_PREFIX}x")).unwrap()
        ));
        assert!(!is_bookkeeping_path(
            &LogicalPath::new("dir/notes.txt").unwrap()
        ));
    }
}
