//! Sending one file to another device (D50).
//!
//! A drop is a push of a single file to one paired device, outside any
//! space. The receiving device saves it in its inbox folder under a name it
//! chooses; nothing is mirrored and nothing is deleted on either side.

use serde::{Deserialize, Serialize};

use crate::reserved::component_issues;

/// Largest file one drop carries: several minutes of 4K phone video.
/// Receivers before 0.1.13 refuse anything over 256 MiB.
pub const DROP_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Longest saved name, in bytes, before the number a collision adds.
pub const DROP_NAME_MAX: usize = 200;

/// What the sending device learns once the receiver has saved the file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropSent {
    /// The file name on the receiving device, which may differ from the
    /// one offered.
    pub saved_name: String,
    pub size: u64,
    /// From opening the stream to the receiver's confirmation.
    pub elapsed_ms: u64,
}

/// A safe single file name for a drop offered as `name`, or `None` when
/// nothing usable is left. Only the last path component counts; characters
/// Windows forbids become `_`, and a Windows device name gets a `_` prefix.
pub fn clean_drop_name(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut cleaned: String = last
        .chars()
        .map(|c| {
            if c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Hidden files stay out of sight in the inbox; a leading dot is dropped.
    cleaned = cleaned
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .to_owned();
    if cleaned.is_empty() {
        return None;
    }
    if !component_issues(&cleaned).is_empty() {
        cleaned.insert(0, '_');
    }
    Some(shorten(&cleaned, DROP_NAME_MAX))
}

/// `name` with ` (n)` before its extension: `IMG.jpg` becomes `IMG (2).jpg`.
pub fn numbered_name(name: &str, n: u32) -> String {
    let (stem, ext) = split_extension(name);
    format!("{stem} ({n}){ext}")
}

/// A media type for `name` by its extension, for the receiver to show.
pub fn mime_for(name: &str) -> &'static str {
    let (_, ext) = split_extension(name);
    match ext.to_ascii_lowercase().as_str() {
        ".jpg" | ".jpeg" => "image/jpeg",
        ".png" => "image/png",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".heic" => "image/heic",
        ".heif" => "image/heif",
        ".avif" => "image/avif",
        ".dng" => "image/x-adobe-dng",
        ".mp4" => "video/mp4",
        ".mov" => "video/quicktime",
        ".3gp" => "video/3gpp",
        ".webm" => "video/webm",
        ".mkv" => "video/x-matroska",
        ".pdf" => "application/pdf",
        ".txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Split at the last dot, keeping it with the extension. A name whose only
/// dot is the first character has no extension.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// Cut `name` to at most `max` bytes on a character boundary, keeping a
/// short extension.
fn shorten(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_owned();
    }
    let (stem, ext) = split_extension(name);
    let ext = if ext.len() <= 16 { ext } else { "" };
    let mut end = max.saturating_sub(ext.len()).min(stem.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ext}", &stem[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_keep_only_the_last_component() {
        assert_eq!(clean_drop_name("PXL_1.jpg").as_deref(), Some("PXL_1.jpg"));
        assert_eq!(
            clean_drop_name("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            clean_drop_name("C:\\Users\\x\\a.png").as_deref(),
            Some("a.png")
        );
        assert_eq!(clean_drop_name("dir/").as_deref(), None);
    }

    #[test]
    fn names_lose_what_windows_cannot_store() {
        assert_eq!(clean_drop_name("a:b?.jpg").as_deref(), Some("a_b_.jpg"));
        assert_eq!(clean_drop_name("CON.txt").as_deref(), Some("_CON.txt"));
        assert_eq!(clean_drop_name("photo. ").as_deref(), Some("photo"));
        assert_eq!(clean_drop_name(".hidden").as_deref(), Some("hidden"));
        assert_eq!(clean_drop_name("..").as_deref(), None);
        assert_eq!(clean_drop_name("  ").as_deref(), None);
        assert_eq!(clean_drop_name("a\u{7}b").as_deref(), Some("a_b"));
    }

    #[test]
    fn long_names_keep_their_extension() {
        let long = format!("{}.jpg", "é".repeat(300));
        let cleaned = clean_drop_name(&long).unwrap();
        assert!(cleaned.len() <= DROP_NAME_MAX, "{}", cleaned.len());
        assert!(cleaned.ends_with(".jpg"));
    }

    #[test]
    fn numbered_names_go_before_the_extension() {
        assert_eq!(numbered_name("IMG.jpg", 2), "IMG (2).jpg");
        assert_eq!(numbered_name("archive.tar.gz", 3), "archive.tar (3).gz");
        assert_eq!(numbered_name("README", 2), "README (2)");
    }

    #[test]
    fn media_types_follow_the_extension() {
        assert_eq!(mime_for("PXL.JPG"), "image/jpeg");
        assert_eq!(mime_for("Screenshot.png"), "image/png");
        assert_eq!(mime_for("notes"), "application/octet-stream");
    }
}
