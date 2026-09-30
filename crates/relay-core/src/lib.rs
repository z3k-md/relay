//! Pure domain types for Relay.
//!
//! Nothing in this crate touches the filesystem, network, or database. Every
//! other crate speaks in these types.

pub mod addr;
pub mod conflict;
pub mod entry;
pub mod error;
pub mod git;
pub mod ids;
pub mod local;
pub mod model;
pub mod pairing;
pub mod path;
pub mod reserved;
pub mod version;

pub use addr::{
    MAX_PEER_ADDRESSES, collect_peer_addresses, format_ip_port, format_socket_addr,
    is_advertisable_ip, is_tailscale_v4, is_tailscale_v6, merge_peer_addresses,
};
pub use entry::{EntryContent, EntryKey, EntryKind, EntryRecord, StatHint};
pub use error::CoreError;
pub use git::{git_dir_of, is_git_metadata};
pub use ids::{DeviceId, MountId, ObjectId, Sequence, SpaceId};
pub use local::{LocalChange, Observation, derive_local_change, needs_rehash};
pub use model::{Device, Mount, Space, validate_name};
pub use pairing::PairingCode;
pub use path::LogicalPath;
pub use reserved::PortabilityIssue;
pub use version::{VectorOrdering, VersionRelation, VersionVector, compare_versions};

/// File at the root of every materialized mount holding its Space and Mount
/// ids. A scan refuses to run if it is missing, so an unmounted drive or a
/// moved folder is never mistaken for "every file was deleted".
pub const MOUNT_MARKER: &str = ".relay-mount";

/// Prefix for in-flight files written next to their destination so the final
/// rename stays on one volume. Never indexed.
pub const TEMP_PREFIX: &str = ".relay-tmp-";
