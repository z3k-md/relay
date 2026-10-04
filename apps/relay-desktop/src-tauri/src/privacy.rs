//! macOS privacy access a managing device needs to browse this one (D37).
//!
//! Without Full Disk Access, the first remote listing of Desktop, Documents,
//! or Downloads shows a consent dialog on this Mac's screen, where nobody may
//! be sitting. Settings shows the state and opens the right pane.

/// System Settings → Privacy & Security → Full Disk Access.
#[cfg(target_os = "macos")]
pub const FULL_DISK_ACCESS_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

/// `Some` on macOS when the answer is knowable; `None` elsewhere.
#[cfg(target_os = "macos")]
pub fn full_disk_access() -> Option<bool> {
    // Readable only with Full Disk Access, and present on every Mac.
    let probe = std::path::Path::new("/Library/Application Support/com.apple.TCC/TCC.db");
    match std::fs::File::open(probe) {
        Ok(_) => Some(true),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => Some(false),
        Err(_) => None,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn full_disk_access() -> Option<bool> {
    None
}
