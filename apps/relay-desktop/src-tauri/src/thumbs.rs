//! Thumbnails for the Files view, served on the `relay-thumb` scheme (D52).
//!
//! `…/t?space=S&mount=M&path=P&px=N` answers with a JPEG (or a PNG when the
//! image has transparency) at most `N` pixels on its long side. Only a file
//! already on this device is read: an online-only file, or a Windows
//! placeholder whose bytes are not here, answers 404 rather than starting a
//! download. Results are cached under `<home>/cache/thumbs`, keyed by the
//! file's path, size and modification time, so an edit makes a new one.
//!
//! Work runs on a few background threads, newest request first: when a
//! long folder is scrolled, the tiles now on screen were asked for last.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::SystemTime;

use relay_engine::Engine;
use tauri::http::{Request, Response, StatusCode, header};
use tauri::{Manager, Runtime, UriSchemeContext, UriSchemeResponder};

use crate::AppState;

pub const SCHEME: &str = "relay-thumb";

/// Long-side sizes a thumbnail is made at; a request rounds up to one.
const SIZES: [u32; 4] = [64, 128, 256, 512];
/// Larger files keep the generic icon: decoding them costs more than a
/// thumbnail is worth.
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
/// Requests beyond this many waiting are refused oldest first (503); the
/// page asks again if the tile is still on screen.
const MAX_QUEUED: usize = 256;
/// The cache is trimmed to `CACHE_KEEP` once it passes `CACHE_MAX`.
const CACHE_MAX: u64 = 512 * 1024 * 1024;
const CACHE_KEEP: u64 = 384 * 1024 * 1024;

pub fn handle<R: Runtime>(
    ctx: UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let Some(home) = ctx
        .app_handle()
        .try_state::<AppState>()
        .map(|state| state.home.clone())
    else {
        return responder.respond(status(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(req) = request.uri().query().and_then(ThumbRequest::parse) else {
        return responder.respond(status(StatusCode::BAD_REQUEST));
    };
    pool(&home).submit(Job {
        req,
        responder,
        home,
    });
}

#[derive(Debug, PartialEq, Eq)]
struct ThumbRequest {
    space: String,
    mount: String,
    path: String,
    px: u32,
}

impl ThumbRequest {
    fn parse(query: &str) -> Option<ThumbRequest> {
        let (mut space, mut mount, mut path, mut px) = (None, None, None, None);
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=')?;
            let value = percent_decode(value)?;
            match key {
                "space" => space = Some(value),
                "mount" => mount = Some(value),
                "path" => path = Some(value),
                "px" => px = value.parse::<u32>().ok(),
                _ => {}
            }
        }
        let px = px?;
        Some(ThumbRequest {
            space: space?,
            mount: mount?,
            path: path.filter(|p| !p.is_empty())?,
            px: SIZES.into_iter().find(|&s| s >= px).unwrap_or(SIZES[3]),
        })
    }
}

/// `encodeURIComponent` undone; `None` for malformed escapes or non-UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

struct Job {
    req: ThumbRequest,
    responder: UriSchemeResponder,
    home: PathBuf,
}

struct Pool {
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
}

impl Pool {
    fn submit(&self, job: Job) {
        let dropped = {
            let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.push_back(job);
            if queue.len() > MAX_QUEUED {
                queue.pop_front()
            } else {
                None
            }
        };
        self.ready.notify_one();
        if let Some(old) = dropped {
            old.responder
                .respond(status(StatusCode::SERVICE_UNAVAILABLE));
        }
    }

    fn work(&self) {
        // Each worker keeps one read-only engine to find files by path.
        let mut engine: Option<Engine> = None;
        loop {
            let job = {
                let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(job) = queue.pop_back() {
                        break job;
                    }
                    queue = self.ready.wait(queue).unwrap_or_else(|e| e.into_inner());
                }
            };
            let response = serve(&job.home, &job.req, &mut engine);
            job.responder.respond(response);
        }
    }
}

fn pool(home: &Path) -> &'static Arc<Pool> {
    static POOL: OnceLock<Arc<Pool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let pool = Arc::new(Pool {
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
        });
        let workers = std::thread::available_parallelism()
            .map_or(2, |n| n.get() / 2)
            .clamp(1, 4);
        for n in 0..workers {
            let pool = Arc::clone(&pool);
            let _ = std::thread::Builder::new()
                .name(format!("relay-thumb-{n}"))
                .spawn(move || pool.work());
        }
        let dir = cache_dir(home);
        let _ = std::thread::Builder::new()
            .name("relay-thumb-trim".into())
            .spawn(move || trim_cache(&dir, CACHE_MAX, CACHE_KEEP));
        pool
    })
}

fn serve(home: &Path, req: &ThumbRequest, engine: &mut Option<Engine>) -> Response<Vec<u8>> {
    let source = match find(home, req, engine) {
        Ok(path) => path,
        Err(err) => {
            log::debug!("thumbnail {}/{}/{}: {err}", req.space, req.mount, req.path);
            return status(StatusCode::NOT_FOUND);
        }
    };
    match thumbnail(&cache_dir(home), &source, req.px) {
        Ok(Some(bytes)) => image_response(bytes),
        Ok(None) => status(StatusCode::NOT_FOUND),
        Err(err) => {
            log::debug!("thumbnail {}: {err:#}", source.display());
            status(StatusCode::NOT_FOUND)
        }
    }
}

fn find(home: &Path, req: &ThumbRequest, engine: &mut Option<Engine>) -> Result<PathBuf, String> {
    if engine.is_none() {
        *engine = Some(Engine::open_read_only(home).map_err(|err| err.to_string())?);
    }
    let found = engine
        .as_ref()
        .map(|e| e.local_file_path(&req.space, &req.mount, &req.path));
    match found {
        Some(Ok(path)) => Ok(path),
        Some(Err(err)) => {
            // A database replaced under it (reset, restore) needs a new handle.
            *engine = None;
            Err(err.to_string())
        }
        None => Err("no engine".into()),
    }
}

/// The cached thumbnail of `source`, made first if needed. `None` when the
/// file is not something to read here: not on this device, too large, or
/// not an image this app decodes.
fn thumbnail(cache: &Path, source: &Path, px: u32) -> anyhow::Result<Option<Vec<u8>>> {
    let meta = std::fs::metadata(source)?;
    if !meta.is_file() || meta.len() > MAX_SOURCE_BYTES || !bytes_here(&meta) {
        return Ok(None);
    }
    let cached = cache.join(cache_name(source, &meta, px));
    if let Ok(bytes) = std::fs::read(&cached) {
        return Ok(Some(bytes));
    }
    let bytes = make(source, px)?;
    store(&cached, &bytes);
    Ok(Some(bytes))
}

/// Encode a thumbnail: JPEG unless the image has transparency.
fn make(source: &Path, px: u32) -> anyhow::Result<Vec<u8>> {
    use image::ImageDecoder;

    let mut reader = image::ImageReader::open(source)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut img = image::DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    let small = if img.width() > px || img.height() > px {
        img.thumbnail(px, px)
    } else {
        img
    };
    let mut out = std::io::Cursor::new(Vec::new());
    if small.color().has_alpha() {
        small.write_to(&mut out, image::ImageFormat::Png)?;
    } else {
        let rgb = image::DynamicImage::ImageRgb8(small.to_rgb8());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82).encode_image(&rgb)?;
    }
    Ok(out.into_inner())
}

/// Whether reading the file would not first pull its bytes from elsewhere.
/// A Windows cloud placeholder (D43) reads as a normal file but downloads
/// on open.
fn bytes_here(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const OFFLINE: u32 = 0x0000_1000;
        const RECALL_ON_OPEN: u32 = 0x0004_0000;
        const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
        meta.file_attributes() & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) == 0
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        true
    }
}

fn cache_dir(home: &Path) -> PathBuf {
    home.join("cache").join("thumbs")
}

fn cache_name(source: &Path, meta: &std::fs::Metadata, px: u32) -> String {
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let mut hasher = blake3::Hasher::new();
    hasher.update(source.to_string_lossy().as_bytes());
    hasher.update(&meta.len().to_le_bytes());
    hasher.update(&modified.to_le_bytes());
    hasher.update(&px.to_le_bytes());
    hasher.finalize().to_hex()[..32].to_owned()
}

/// Best effort: a thumbnail that fails to cache is still served.
fn store(path: &Path, bytes: &[u8]) {
    let Some(dir) = path.parent() else { return };
    let _ = std::fs::create_dir_all(dir);
    let tmp = path.with_extension("tmp");
    let written = std::fs::File::create(&tmp).and_then(|mut f| f.write_all(bytes));
    if written.is_err() || std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Delete the least recently written thumbnails until the cache holds at
/// most `keep` bytes, if it holds more than `max`.
fn trim_cache(dir: &Path, max: u64, keep: u64) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = read
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| {
                let when = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                (when, meta.len(), entry.path())
            })
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= max {
        return;
    }
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= keep {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

fn image_response(bytes: Vec<u8>) -> Response<Vec<u8>> {
    let kind = if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else {
        "image/jpeg"
    };
    let mut response = Response::new(bytes);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static(kind));
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    allow_fetch(&mut response);
    response
}

fn status(code: StatusCode) -> Response<Vec<u8>> {
    let mut response = Response::new(Vec::new());
    *response.status_mut() = code;
    allow_fetch(&mut response);
    response
}

/// The page fetches thumbnails from its own origin, which differs from the
/// scheme's.
fn allow_fetch(response: &mut Response<Vec<u8>>) {
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::HeaderValue::from_static("*"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_rounds_size_up() {
        let req = ThumbRequest::parse("space=Photos&mount=Cam%20Roll&path=2026%2Fa%20b.jpg&px=200")
            .expect("parses");
        assert_eq!(
            req,
            ThumbRequest {
                space: "Photos".into(),
                mount: "Cam Roll".into(),
                path: "2026/a b.jpg".into(),
                px: 256,
            }
        );
        let big = ThumbRequest::parse("space=a&mount=b&path=c&px=4000").expect("parses");
        assert_eq!(big.px, 512);
    }

    #[test]
    fn rejects_missing_or_malformed_fields() {
        assert!(ThumbRequest::parse("space=a&mount=b&px=64").is_none());
        assert!(ThumbRequest::parse("space=a&mount=b&path=&px=64").is_none());
        assert!(ThumbRequest::parse("space=a&mount=b&path=%zz&px=64").is_none());
        assert!(ThumbRequest::parse("space=a&mount=b&path=c&px=big").is_none());
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("relay-thumb-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn makes_caches_and_remakes_after_an_edit() {
        let dir = scratch("cache");
        let source = dir.join("photo.png");
        let cache = dir.join("cache");
        image::RgbImage::from_pixel(800, 400, image::Rgb([10, 200, 30]))
            .save(&source)
            .expect("write source");

        let first = thumbnail(&cache, &source, 128)
            .expect("thumb")
            .expect("an image");
        let decoded = image::load_from_memory(&first).expect("decodes");
        assert_eq!((decoded.width(), decoded.height()), (128, 64));
        assert!(!first.starts_with(b"\x89PNG"), "opaque images are JPEG");
        assert_eq!(std::fs::read_dir(&cache).expect("cache").count(), 1);

        // A different size of the same file is its own entry.
        thumbnail(&cache, &source, 64)
            .expect("thumb")
            .expect("an image");
        assert_eq!(std::fs::read_dir(&cache).expect("cache").count(), 2);

        // Rewriting the file (new length) misses the cache.
        image::RgbaImage::from_pixel(100, 100, image::Rgba([0, 0, 0, 0]))
            .save(&source)
            .expect("rewrite source");
        let second = thumbnail(&cache, &source, 128)
            .expect("thumb")
            .expect("an image");
        assert!(
            second.starts_with(b"\x89PNG"),
            "transparent images stay PNG"
        );
        let decoded = image::load_from_memory(&second).expect("decodes");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (100, 100),
            "never enlarged"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_images_have_no_thumbnail() {
        let dir = scratch("text");
        let source = dir.join("notes.txt");
        std::fs::write(&source, b"hello").expect("write");
        assert!(thumbnail(&dir.join("cache"), &source, 64).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trims_oldest_first() {
        let dir = scratch("trim");
        for n in 0..4u8 {
            std::fs::write(dir.join(format!("{n}")), vec![n; 100]).expect("write");
            let when =
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + u64::from(n));
            std::fs::File::options()
                .write(true)
                .open(dir.join(format!("{n}")))
                .and_then(|f| f.set_modified(when))
                .expect("set mtime");
        }
        trim_cache(&dir, 300, 250);
        assert!(!dir.join("0").exists() && !dir.join("1").exists());
        assert!(dir.join("2").exists() && dir.join("3").exists());
        trim_cache(&dir, 500, 0);
        assert!(dir.join("2").exists(), "under the limit nothing is trimmed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
