//! Image URLs for shell icons and thumbnails.
//!
//! `<scheme>://localhost/<encodeURIComponent(path)>?s=<px>&f=<flags>&m=<mtime>`
//! (`http://<scheme>.localhost/...` on Windows). `s` is device pixels, `f`
//! the entry flags, `m` only busts the browser cache when the file changes.

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

use crate::entry::{FLAG_DIR, FLAG_LINK, FLAG_READONLY, FLAG_SYSTEM};

#[derive(Debug, PartialEq, Eq)]
pub struct ImageRequest {
    pub path: PathBuf,
    /// Device pixels, 16 to 256.
    pub size: u32,
    /// [`crate::entry`] flags of the item.
    pub flags: u32,
}

/// Parse a request's URL path (with its leading `/`) and query.
pub fn parse(path: &str, query: Option<&str>) -> Option<ImageRequest> {
    let raw = path.strip_prefix('/')?;
    let decoded = percent_decode_str(raw).decode_utf8().ok()?;
    if decoded.is_empty() {
        return None;
    }
    let mut size = 32u32;
    let mut flags = 0;
    for pair in query.unwrap_or_default().split('&') {
        match pair.split_once('=') {
            Some(("s", v)) => size = v.parse().ok()?,
            Some(("f", v)) => flags = v.parse().ok()?,
            _ => {}
        }
    }
    Some(ImageRequest {
        path: PathBuf::from(decoded.into_owned()),
        size: size.clamp(16, 256),
        flags,
    })
}

/// Items that share an icon share a cache entry: plain folders, and files by
/// extension. Folders with a custom icon (marked read-only or system, which
/// `desktop.ini` requires), links and files that carry their own icon get
/// none.
pub fn icon_cache_key(path: &Path, flags: u32) -> Option<String> {
    const OWN_ICON: &[&str] = &[
        "exe",
        "ico",
        "lnk",
        "url",
        "cur",
        "ani",
        "scr",
        "msc",
        "appref-ms",
    ];
    if flags & FLAG_LINK != 0 {
        return None;
    }
    if flags & FLAG_DIR != 0 {
        return (flags & (FLAG_READONLY | FLAG_SYSTEM) == 0).then(|| "<dir>".to_string());
    }
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    (!ext.is_empty() && !OWN_ICON.contains(&ext.as_str())).then(|| format!(".{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_windows_paths() {
        assert_eq!(
            parse(
                "/C%3A%5CUsers%5Czach%5Cnote%20one.txt",
                Some("s=48&f=0&m=17")
            ),
            Some(ImageRequest {
                path: PathBuf::from("C:\\Users\\zach\\note one.txt"),
                size: 48,
                flags: 0,
            })
        );
    }

    #[test]
    fn clamps_size_and_rejects_junk() {
        assert_eq!(parse("/%2Ftmp%2Fa.png", Some("s=4000")).unwrap().size, 256);
        assert_eq!(parse("/%2Ftmp%2Fa.png", None).unwrap().size, 32);
        assert_eq!(parse("/", Some("s=32")), None);
        assert_eq!(parse("/a", Some("s=big")), None);
        assert_eq!(parse("/%FF", None), None, "not UTF-8");
    }

    #[test]
    fn icon_keys() {
        assert_eq!(
            icon_cache_key(Path::new("x/Report.PDF"), 0),
            Some(".pdf".into())
        );
        assert_eq!(icon_cache_key(Path::new("x/setup.exe"), 0), None);
        assert_eq!(icon_cache_key(Path::new("x/Makefile"), 0), None);
        assert_eq!(icon_cache_key(Path::new("x/a.txt"), FLAG_LINK), None);
        assert_eq!(
            icon_cache_key(Path::new("x/Docs"), FLAG_DIR),
            Some("<dir>".into())
        );
        assert_eq!(
            icon_cache_key(Path::new("x/Downloads"), FLAG_DIR | FLAG_READONLY),
            None
        );
    }
}
