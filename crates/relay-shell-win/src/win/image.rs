//! Icons and thumbnails through `IShellItemImageFactory`, returned as PNG.
//!
//! This is the same source Explorer draws from: registered thumbnail
//! providers, the system thumbnail cache and per-type icons. Must run on an
//! STA thread (see [`super::sta::StaPool`]).

use std::path::Path;

use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF, SIIGBF_ICONONLY,
    SIIGBF_RESIZETOFIT, SIIGBF_THUMBNAILONLY,
};
use windows::core::PCWSTR;

use super::com::{Context, Error, wide};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The file type's icon (or the item's own, for .exe, .lnk and friends).
    Icon,
    /// A content thumbnail, falling back to the icon when there is none.
    Thumbnail,
    /// A content thumbnail only; fails when the item has no thumbnail.
    ThumbnailOnly,
}

/// The sizes the shell keeps hand-tuned icon frames and cache buckets for.
/// Requests snap up to one of these, as Files does.
pub const SIZES: [u32; 8] = [16, 24, 32, 48, 64, 96, 128, 256];

pub fn snap_size(px: u32) -> u32 {
    SIZES.iter().copied().find(|&s| s >= px).unwrap_or(256)
}

/// Render `path` at `size`×`size` device pixels as PNG bytes.
pub fn render_png(path: &Path, size: u32, kind: Kind) -> Result<Vec<u8>, Error> {
    let name = wide(path);
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None) }
            .ctx("SHCreateItemFromParsingName")?;
    let flags: SIIGBF = match kind {
        Kind::Icon => SIIGBF_ICONONLY,
        Kind::Thumbnail => SIIGBF_RESIZETOFIT,
        Kind::ThumbnailOnly => SIIGBF_THUMBNAILONLY | SIIGBF_RESIZETOFIT,
    };
    let side = size as i32;
    let bitmap = unsafe { factory.GetImage(SIZE { cx: side, cy: side }, flags) }
        .ctx("IShellItemImageFactory::GetImage")?;
    let result = encode(bitmap);
    let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
    result
}

fn encode(bitmap: HBITMAP) -> Result<Vec<u8>, Error> {
    let (width, height, mut bgra) = read_bitmap(bitmap)?;
    to_straight_rgba(&mut bgra);
    let mut out = Vec::with_capacity(bgra.len() / 2);
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .map_err(|e| png_error("png header", e))?;
        writer
            .write_image_data(&bgra)
            .map_err(|e| png_error("png data", e))?;
    }
    Ok(out)
}

fn png_error(context: &'static str, err: png::EncodingError) -> Error {
    Error::new(
        context,
        windows::core::Error::new(
            windows::core::HRESULT(0x8000_4005u32 as i32),
            err.to_string(),
        ),
    )
}

/// Top-down 32-bit pixels from a DIB section, in BGRA order.
fn read_bitmap(bitmap: HBITMAP) -> Result<(u32, u32, Vec<u8>), Error> {
    let mut info = BITMAP::default();
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut info as *mut BITMAP).cast()),
        )
    };
    if got == 0 || info.bmWidth <= 0 || info.bmHeight == 0 {
        return Err(Error::new(
            "GetObjectW",
            windows::core::Error::from_thread(),
        ));
    }
    let width = info.bmWidth;
    let height = info.bmHeight.abs();
    let mut header = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    let dc = unsafe { GetDC(None) };
    let lines = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut header,
            DIB_RGB_COLORS,
        )
    };
    unsafe { ReleaseDC(None, dc) };
    if lines == 0 {
        return Err(Error::new("GetDIBits", windows::core::Error::from_thread()));
    }
    Ok((width as u32, height as u32, pixels))
}

/// BGRA → RGBA with straight alpha, in place.
///
/// The shell hands back premultiplied alpha for icons, opaque pixels with a
/// zero alpha channel for some thumbnails, and occasionally straight alpha.
/// Decide from the data: all-zero alpha means opaque; any colour channel
/// above its alpha means the data was never premultiplied.
fn to_straight_rgba(px: &mut [u8]) {
    let (pixels, _) = px.as_chunks::<4>();
    let all_zero_alpha = pixels.iter().all(|p| p[3] == 0);
    let premultiplied = !all_zero_alpha
        && pixels
            .iter()
            .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]);
    for p in px.as_chunks_mut::<4>().0 {
        let (b, g, r, a) = (p[0], p[1], p[2], p[3]);
        let a = if all_zero_alpha { 255 } else { a };
        let unmul = |c: u8| -> u8 {
            if premultiplied && a != 0 && a != 255 {
                ((u16::from(c) * 255 + u16::from(a) / 2) / u16::from(a)).min(255) as u8
            } else {
                c
            }
        };
        p[0] = unmul(r);
        p[1] = unmul(g);
        p[2] = unmul(b);
        p[3] = a;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::sta::StaPool;
    use std::sync::mpsc;

    #[test]
    fn snaps_sizes() {
        assert_eq!(snap_size(1), 16);
        assert_eq!(snap_size(16), 16);
        assert_eq!(snap_size(20), 24);
        assert_eq!(snap_size(300), 256);
    }

    #[test]
    fn unpremultiplies() {
        // Half-transparent pure red, premultiplied: B=0 G=0 R=128 A=128.
        let mut px = vec![0, 0, 128, 128, 0, 0, 0, 255];
        to_straight_rgba(&mut px);
        assert_eq!(&px[0..4], &[255, 0, 0, 128]);
        assert_eq!(&px[4..8], &[0, 0, 0, 255]);
    }

    #[test]
    fn zero_alpha_means_opaque() {
        let mut px = vec![10, 20, 30, 0];
        to_straight_rgba(&mut px);
        assert_eq!(px, vec![30, 20, 10, 255]);
    }

    #[test]
    fn renders_a_text_file_icon() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, b"hello").unwrap();
        let pool = StaPool::new("test-img", 1).unwrap();
        let (tx, rx) = mpsc::channel();
        pool.spawn(move || tx.send(render_png(&file, 32, Kind::Icon)).unwrap());
        let png = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap()
            .unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }
}
