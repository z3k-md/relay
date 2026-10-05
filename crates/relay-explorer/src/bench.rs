//! Benchmark folders for the P0 pass bars: many empty files of mixed types
//! (listing, scrolling, icons) or small PNGs (thumbnails). Built once under
//! the temp folder and reused.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const EXTENSIONS: [&str; 12] = [
    "txt", "pdf", "docx", "xlsx", "png", "jpg", "mp4", "zip", "rs", "json", "md", "csv",
];
const FOLDERS: u32 = 25;
const PROGRESS_EVERY: u32 = 1000;

/// Create `<temp>/relay-explorer-bench/<kind>-<count>` unless an earlier run
/// finished it, reporting progress as items are written.
pub fn make(kind: &str, count: u32, mut progress: impl FnMut(u32)) -> io::Result<PathBuf> {
    make_in(
        &std::env::temp_dir().join("relay-explorer-bench"),
        kind,
        count,
        &mut progress,
    )
}

fn make_in(
    root: &Path,
    kind: &str,
    count: u32,
    progress: &mut dyn FnMut(u32),
) -> io::Result<PathBuf> {
    let dir = root.join(format!("{kind}-{count}"));
    let marker = root.join(format!("{kind}-{count}.done"));
    if marker.exists() && dir.is_dir() {
        return Ok(dir);
    }
    fs::create_dir_all(&dir)?;
    match kind {
        "files" => files(&dir, count, progress)?,
        "images" => images(&dir, count, progress)?,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown benchmark kind {other:?}"),
            ));
        }
    }
    fs::write(&marker, b"")?;
    Ok(dir)
}

fn files(dir: &Path, count: u32, progress: &mut dyn FnMut(u32)) -> io::Result<()> {
    for i in 0..FOLDERS.min(count) {
        fs::create_dir_all(dir.join(format!("folder {i:02}")))?;
    }
    for i in 0..count {
        let ext = EXTENSIONS[i as usize % EXTENSIONS.len()];
        let path = dir.join(format!("file {i:06}.{ext}"));
        if !path.exists() {
            fs::File::create(path)?;
        }
        if (i + 1).is_multiple_of(PROGRESS_EVERY) {
            progress(i + 1);
        }
    }
    progress(count);
    Ok(())
}

fn images(dir: &Path, count: u32, progress: &mut dyn FnMut(u32)) -> io::Result<()> {
    for i in 0..count {
        let path = dir.join(format!("image {i:05}.png"));
        if !path.exists() {
            fs::write(path, gradient_png(i)?)?;
        }
        if (i + 1).is_multiple_of(100) {
            progress(i + 1);
        }
    }
    progress(count);
    Ok(())
}

/// A 256×256 gradient whose colours vary with `seed`, so every thumbnail
/// looks different.
fn gradient_png(seed: u32) -> io::Result<Vec<u8>> {
    const SIDE: u32 = 256;
    let hue = (seed.wrapping_mul(37) % 256) as u8;
    let mut pixels = Vec::with_capacity((SIDE * SIDE * 3) as usize);
    for y in 0..SIDE {
        for x in 0..SIDE {
            pixels.extend_from_slice(&[hue, x as u8, y as u8]);
        }
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, SIDE, SIDE);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(io::Error::other)?;
    writer.write_image_data(&pixels).map_err(io::Error::other)?;
    writer.finish().map_err(io::Error::other)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_once_and_reuses() {
        let root = tempfile::tempdir().unwrap();
        let mut seen = Vec::new();
        let dir = make_in(root.path(), "files", 30, &mut |n| seen.push(n)).unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 30 + 25);
        assert_eq!(seen.last(), Some(&30));

        seen.clear();
        let again = make_in(root.path(), "files", 30, &mut |n| seen.push(n)).unwrap();
        assert_eq!(again, dir);
        assert!(seen.is_empty(), "second run reuses the folder");
    }

    #[test]
    fn writes_decodable_images() {
        let root = tempfile::tempdir().unwrap();
        let dir = make_in(root.path(), "images", 2, &mut |_| {}).unwrap();
        let bytes = fs::read(dir.join("image 00001.png")).unwrap();
        let decoder = png::Decoder::new(io::Cursor::new(bytes));
        let reader = decoder.read_info().unwrap();
        assert_eq!(reader.info().width, 256);
    }

    #[test]
    fn rejects_unknown_kinds() {
        let root = tempfile::tempdir().unwrap();
        let err = make_in(root.path(), "nope", 1, &mut |_| {}).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
