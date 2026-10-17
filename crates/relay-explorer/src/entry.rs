//! One row of a folder listing.

use std::io;
use std::path::Path;

use serde::ser::{Serialize, SerializeTuple, Serializer};

pub const FLAG_DIR: u32 = 1;
pub const FLAG_HIDDEN: u32 = 1 << 1;
pub const FLAG_SYSTEM: u32 = 1 << 2;
pub const FLAG_READONLY: u32 = 1 << 3;
/// Symlink, junction or other name-surrogate reparse point.
pub const FLAG_LINK: u32 = 1 << 4;
/// Online-only placeholder (recall on open or on data access, or offline).
pub const FLAG_CLOUD: u32 = 1 << 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub flags: u32,
    pub size: u64,
    /// Last write time, Unix milliseconds.
    pub modified_ms: i64,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.flags & FLAG_DIR != 0
    }
}

/// Serialized as `[name, flags, size, modified_ms]`: a 200k-entry listing is
/// mostly names, and field keys would double its size on the wire.
impl Serialize for Entry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut tuple = serializer.serialize_tuple(4)?;
        tuple.serialize_element(&self.name)?;
        tuple.serialize_element(&self.flags)?;
        tuple.serialize_element(&self.size)?;
        tuple.serialize_element(&self.modified_ms)?;
        tuple.end()
    }
}

/// Map Windows `FILE_ATTRIBUTE_*` bits (and the reparse tag) to entry flags.
/// Pure bit mapping, so it is testable everywhere.
pub fn flags_from_windows(attributes: u32, reparse_tag: u32) -> u32 {
    const READONLY: u32 = 0x1;
    const HIDDEN: u32 = 0x2;
    const SYSTEM: u32 = 0x4;
    const DIRECTORY: u32 = 0x10;
    const REPARSE_POINT: u32 = 0x400;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    // Name surrogates: symlinks, junctions and the like, not cloud files.
    const NAME_SURROGATE_BIT: u32 = 0x2000_0000;

    let mut flags = 0;
    if attributes & DIRECTORY != 0 {
        flags |= FLAG_DIR;
    }
    if attributes & HIDDEN != 0 {
        flags |= FLAG_HIDDEN;
    }
    if attributes & SYSTEM != 0 {
        flags |= FLAG_SYSTEM;
    }
    if attributes & READONLY != 0 {
        flags |= FLAG_READONLY;
    }
    if attributes & REPARSE_POINT != 0 && reparse_tag & NAME_SURROGATE_BIT != 0 {
        flags |= FLAG_LINK;
    }
    if attributes & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0 {
        flags |= FLAG_CLOUD;
    }
    flags
}

/// Look up one name in `dir`, without following symlinks.
pub fn stat(dir: &Path, name: &str) -> io::Result<Entry> {
    let meta = std::fs::symlink_metadata(dir.join(name))?;
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    Ok(Entry {
        name: name.to_string(),
        flags: platform_flags(name, &meta),
        size: if meta.is_dir() { 0 } else { meta.len() },
        modified_ms,
    })
}

#[cfg(windows)]
fn platform_flags(_name: &str, meta: &std::fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    // std does not expose the reparse tag; a reparse point that std calls a
    // symlink is a name surrogate.
    let tag = if meta.file_type().is_symlink() {
        0x2000_0000
    } else {
        0
    };
    flags_from_windows(meta.file_attributes(), tag)
}

#[cfg(not(windows))]
fn platform_flags(name: &str, meta: &std::fs::Metadata) -> u32 {
    let mut flags = 0;
    if meta.is_dir() {
        flags |= FLAG_DIR;
    }
    if meta.file_type().is_symlink() {
        flags |= FLAG_LINK;
    }
    if name.starts_with('.') {
        flags |= FLAG_HIDDEN;
    }
    if meta.permissions().readonly() {
        flags |= FLAG_READONLY;
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_tuple() {
        let e = Entry {
            name: "a.txt".into(),
            flags: FLAG_HIDDEN,
            size: 5,
            modified_ms: 7,
        };
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"["a.txt",2,5,7]"#);
    }

    #[test]
    fn maps_windows_attributes() {
        assert_eq!(flags_from_windows(0x10, 0), FLAG_DIR);
        assert_eq!(flags_from_windows(0x2 | 0x4, 0), FLAG_HIDDEN | FLAG_SYSTEM);
        // Symlink tag is a name surrogate; the Cloud Files tag is not.
        assert_eq!(flags_from_windows(0x400, 0xA000_000C), FLAG_LINK);
        assert_eq!(
            flags_from_windows(0x400 | 0x40_0000, 0x9000_001A),
            FLAG_CLOUD
        );
    }

    #[test]
    fn stats_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.bin"), [0u8; 3]).unwrap();
        let e = stat(dir.path(), "x.bin").unwrap();
        assert_eq!(e.size, 3);
        assert!(!e.is_dir());
        assert!(e.modified_ms > 0);
        assert_eq!(
            stat(dir.path(), "missing").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
