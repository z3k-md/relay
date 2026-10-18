//! One log setup for every host (D56).
//!
//! The CLI's `relay run`, `relay service`, the desktop app, the Android app,
//! and the home server all write `<relay home>/logs/relay.log` through one
//! `tracing` subscriber: UTC timestamps, spans, 10 MiB per file and two
//! rotated files kept. The file's filter can be changed while the host runs
//! (`relay log level`, Settings) and falls back to the default when a time
//! limit runs out, so nobody leaves `debug` on by accident.
//!
//! Library code only emits `tracing` events; the `log` crate (the desktop
//! app, rustls) is bridged in.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use relay_ipc::LogLevel;
use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, fmt, reload};

pub const LOG_DIR: &str = "logs";
pub const LOG_FILE: &str = "relay.log";
/// Level of the log file when nothing else is asked for.
pub const DEFAULT_LEVEL: &str = "info";
const ROTATE_AFTER: u64 = 10 * 1024 * 1024;
/// Rotated files kept next to the live one: `relay.log.1`, `relay.log.2`.
const ROTATED_KEPT: u32 = 2;
/// Writes between size checks, so a host that runs for months rotates too,
/// not only one that restarts.
const ROTATE_CHECK_EVERY: u32 = 256;
/// Chatty dependencies stay at `warn` whatever level Relay itself logs at,
/// unless a full filter names them.
const QUIET: &[&str] = &[
    "quinn",
    "quinn_proto",
    "quinn_udp",
    "rustls",
    "mdns_sd",
    "polling",
    "notify",
    "hyper",
    "hyper_util",
    "reqwest",
    "tao",
    "wry",
    "tauri",
    "h2",
    "tokio_util",
    "netlink",
];

/// Where the subscriber sends lines besides the file.
#[derive(Default)]
pub struct LogOptions {
    /// The log file, when the Relay home is already known. [`set_file`]
    /// sets or moves it later.
    pub file: Option<PathBuf>,
    /// Also write to stderr with this filter (`RELAY_LOG`, or `warn`).
    pub stderr: Option<String>,
    /// Also hand each line to this sink at the file's level (Android
    /// logcat). Lines go here even before the file is set.
    pub mirror: Option<fn(Level, &str)>,
}

struct Control {
    reload: Box<dyn Fn(EnvFilter) -> Result<(), String> + Send + Sync>,
    state: Mutex<LogLevel>,
    generation: AtomicU64,
}

static CONTROL: OnceLock<Control> = OnceLock::new();
static FILE: OnceLock<Mutex<Option<LogFile>>> = OnceLock::new();
static MIRROR: OnceLock<fn(Level, &str)> = OnceLock::new();

/// `<home>/logs/relay.log`.
pub fn log_path(home: &Path) -> PathBuf {
    home.join(LOG_DIR).join(LOG_FILE)
}

/// The live log and its rotated files that exist, oldest first.
pub fn log_files(path: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = (1..=ROTATED_KEPT)
        .rev()
        .map(|n| rotated_name(path, n))
        .collect();
    files.push(path.to_path_buf());
    files.retain(|p| p.is_file());
    files
}

/// The default file filter: `RELAY_LOG` when set, else [`DEFAULT_LEVEL`].
pub fn default_level() -> String {
    std::env::var("RELAY_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_LEVEL.to_owned())
}

/// Install the subscriber once per process. Later calls only set the file
/// (when given), so the desktop app and the Android service can both call it.
/// Returns whether this call installed it.
pub fn init(opts: LogOptions, version: &str) -> bool {
    if let Some(path) = &opts.file {
        let _ = set_file(path, version);
    }
    if CONTROL.get().is_some() {
        return false;
    }
    if let Some(mirror) = opts.mirror {
        let _ = MIRROR.set(mirror);
    }
    let default = default_level();
    let filter = parse_filter(&default)
        .unwrap_or_else(|_| parse_filter(DEFAULT_LEVEL).expect("the default filter parses"));
    let (filter, handle) = reload::Layer::<EnvFilter, Registry>::new(filter);
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_writer(HostWriter)
        .with_filter(filter);
    let stderr_layer = opts.stderr.map(|level| {
        fmt::layer()
            .with_writer(io::stderr)
            .with_filter(parse_filter(&level).unwrap_or_else(|_| EnvFilter::new("warn")))
    });
    let subscriber = Registry::default().with(file_layer).with(stderr_layer);
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        return false;
    }
    // The file's filter can rise later, so let every `log` record through
    // to the subscriber and filter there.
    let _ = tracing_log::LogTracer::builder()
        .with_max_level(tracing_log::log::LevelFilter::Trace)
        .init();
    let control = Control {
        reload: Box::new(move |filter| handle.reload(filter).map_err(|e| e.to_string())),
        state: Mutex::new(LogLevel {
            filter: default.clone(),
            default,
            until_ms: None,
        }),
        generation: AtomicU64::new(0),
    };
    CONTROL.set(control).is_ok()
}

/// Write the log to `path` from now on, rotating it first if it is over the
/// cap, and mark the start with a header line.
pub fn set_file(path: &Path, version: &str) -> io::Result<()> {
    let mut file = LogFile::open(path)?;
    writeln!(
        file.file,
        "==== relay {version} started {} pid {} ====",
        format_utc(unix_ms() / 1000),
        std::process::id()
    )?;
    file.file.flush()?;
    *lock_file() = Some(file);
    Ok(())
}

pub fn file_is_set() -> bool {
    FILE.get()
        .is_some_and(|m| m.lock().map(|g| g.is_some()).unwrap_or(false))
}

/// The current file filter, or `None` before [`init`].
pub fn level() -> Option<LogLevel> {
    let control = CONTROL.get()?;
    control.state.lock().ok().map(|s| s.clone())
}

/// Set the file's filter. `None` goes back to the default. With `for_`, the
/// filter reverts to the default after that long. A bare level such as
/// `debug` keeps chatty dependencies at `warn`; a full filter
/// (`info,relay_net=trace`) is taken as written.
pub fn set_level(filter: Option<&str>, for_: Option<Duration>) -> Result<LogLevel, String> {
    let control = CONTROL
        .get()
        .ok_or_else(|| "logging is not set up in this process".to_owned())?;
    let mut state = control.state.lock().map_err(|e| e.to_string())?;
    let wanted = filter
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| state.default.clone());
    (control.reload)(parse_filter(&wanted)?)?;
    let generation = control.generation.fetch_add(1, Ordering::SeqCst) + 1;
    state.filter = wanted.clone();
    state.until_ms = None;
    if let Some(after) = for_.filter(|_| wanted != state.default) {
        state.until_ms = Some(unix_ms() + after.as_millis() as u64);
        let spawned = thread::Builder::new()
            .name("relay-log-level".to_owned())
            .spawn(move || {
                thread::sleep(after);
                let Some(control) = CONTROL.get() else {
                    return;
                };
                if control.generation.load(Ordering::SeqCst) == generation {
                    let _ = set_level(None, None);
                    tracing::info!("log level back to the default");
                }
            });
        if spawned.is_err() {
            state.until_ms = None;
        }
    }
    tracing::info!(filter = %wanted, until_ms = ?state.until_ms, "log level set");
    Ok(state.clone())
}

/// A bare level gets [`QUIET`] appended; anything with `=` or `,` is a full
/// filter.
fn parse_filter(text: &str) -> Result<EnvFilter, String> {
    let text = text.trim();
    let full = if text.contains('=') || text.contains(',') {
        text.to_owned()
    } else {
        let mut directives = vec![text.to_owned()];
        directives.extend(QUIET.iter().map(|target| format!("{target}=warn")));
        directives.join(",")
    };
    EnvFilter::builder()
        .parse(&full)
        .map_err(|e| format!("invalid log filter {text:?}: {e}"))
}

/// Append one line to the log file. Used by `relay run --log-file` for its
/// own messages.
pub fn write_line(line: &str) -> io::Result<()> {
    let mut guard = lock_file();
    let Some(log) = guard.as_mut() else {
        return Err(io::Error::other("log file is not set"));
    };
    log.rotate_periodically();
    writeln!(log.file, "{line}")?;
    log.file.flush()
}

/// Panics go to the log file too, ahead of the default hook.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = write_line(&format!(
            "{} PANIC {info}",
            format_rfc3339(SystemTime::now())
        ));
        previous(info);
    }));
}

struct LogFile {
    path: PathBuf,
    file: File,
    writes_since_check: u32,
}

impl LogFile {
    fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            file: open_log(path)?,
            writes_since_check: 0,
        })
    }

    /// Every `ROTATE_CHECK_EVERY` writes, rotate and reopen once the file has
    /// grown past the cap. A reopen that fails keeps the old handle.
    fn rotate_periodically(&mut self) {
        self.writes_since_check += 1;
        if self.writes_since_check < ROTATE_CHECK_EVERY {
            return;
        }
        self.writes_since_check = 0;
        let oversized = self.file.metadata().is_ok_and(|m| m.len() > ROTATE_AFTER);
        if oversized && let Ok(file) = open_log(&self.path) {
            self.file = file;
        }
    }
}

fn lock_file() -> std::sync::MutexGuard<'static, Option<LogFile>> {
    FILE.get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn open_log(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    rotate_if_needed(path)?;
    OpenOptions::new().create(true).append(true).open(path)
}

/// Shift `relay.log` → `.1` → `.2` once the live file is over the cap; the
/// oldest is dropped.
fn rotate_if_needed(path: &Path) -> io::Result<()> {
    let len = match fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if len <= ROTATE_AFTER {
        return Ok(());
    }
    for n in (1..ROTATED_KEPT).rev() {
        let from = rotated_name(path, n);
        if from.exists() {
            fs::rename(&from, rotated_name(path, n + 1))?;
        }
    }
    fs::rename(path, rotated_name(path, 1))
}

fn rotated_name(path: &Path, n: u32) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new(LOG_FILE))
        .to_os_string();
    name.push(format!(".{n}"));
    path.with_file_name(name)
}

/// Sends formatted lines to the file and the mirror.
struct HostWriter;

struct LineWriter {
    level: Level,
}

impl<'a> MakeWriter<'a> for HostWriter {
    type Writer = LineWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LineWriter { level: Level::INFO }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        LineWriter {
            level: *meta.level(),
        }
    }
}

impl io::Write for LineWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(mirror) = MIRROR.get() {
            let text = String::from_utf8_lossy(buf);
            mirror(self.level, text.trim_end());
        }
        let mut guard = lock_file();
        if let Some(log) = guard.as_mut() {
            log.rotate_periodically();
            log.file.write_all(buf)?;
            log.file.flush()?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `2026-10-18 00:30:01 UTC`.
pub fn format_utc(secs: u64) -> String {
    let (date, h, m, s) = civil(secs as i64);
    format!("{date} {h:02}:{m:02}:{s:02} UTC")
}

/// `2026-10-18T00:30:01.123456Z`, the subscriber's own timestamp format.
pub fn format_rfc3339(at: SystemTime) -> String {
    let d = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (date, h, m, s) = civil(d.as_secs() as i64);
    format!("{date}T{h:02}:{m:02}:{s:02}.{:06}Z", d.subsec_micros())
}

/// Parse the timestamp a log line starts with (`2026-10-18T00:30:01.123456Z`)
/// into Unix microseconds.
pub fn parse_rfc3339_micros(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| text.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut micros = 0i64;
    let mut rest = &text[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        rest = &frac[digits.len()..];
        let padded = format!("{:0<6}", &digits[..digits.len().min(6)]);
        micros = padded.parse().ok()?;
    }
    if !rest.starts_with('Z') {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    Some(((days * 86_400 + h * 3600 + mi * 60 + s) * 1_000_000) + micros)
}

fn civil(secs: i64) -> (String, i64, i64, i64) {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (
        format!("{y:04}-{m:02}-{d:02}"),
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
    )
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_keeps_two_older_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(LOG_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let oversized = vec![b'x'; ROTATE_AFTER as usize + 16];
        for round in 0..3u8 {
            let mut body = oversized.clone();
            body[0] = b'0' + round;
            fs::write(&path, &body).unwrap();
            drop(open_log(&path).unwrap());
        }
        let first = |p: PathBuf| fs::read(p).unwrap()[0];
        assert_eq!(first(rotated_name(&path, 1)), b'2');
        assert_eq!(first(rotated_name(&path, 2)), b'1');
        assert!(!rotated_name(&path, 3).exists());
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(
            log_files(&path),
            vec![rotated_name(&path, 2), rotated_name(&path, 1), path.clone()]
        );
    }

    #[test]
    fn open_file_rotates_once_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);
        let mut log = LogFile::open(&path).unwrap();
        // Something else (an older handle, say) grows the file past the cap.
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&vec![b'x'; ROTATE_AFTER as usize + 16])
            .unwrap();
        for _ in 0..ROTATE_CHECK_EVERY {
            log.rotate_periodically();
            writeln!(log.file, "line").unwrap();
        }
        assert!(rotated_name(&path, 1).exists(), "log was not rotated");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.len() < 64, "{text:?}");
    }

    #[test]
    fn small_file_is_not_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);
        fs::write(&path, b"keep\n").unwrap();
        rotate_if_needed(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep\n");
        assert_eq!(log_files(&path), vec![path.clone()]);
    }

    #[test]
    fn timestamps_round_trip() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(1_704_067_200), "2024-01-01 00:00:00 UTC");
        let at = UNIX_EPOCH + Duration::from_micros(1_792_283_756_524_578);
        let text = format_rfc3339(at);
        assert_eq!(text, "2026-10-18T00:35:56.524578Z");
        assert_eq!(parse_rfc3339_micros(&text), Some(1_792_283_756_524_578));
        assert_eq!(
            parse_rfc3339_micros("2026-10-18T00:35:56.5Z  INFO x"),
            Some(1_792_283_756_500_000)
        );
        assert_eq!(parse_rfc3339_micros("==== relay"), None);
        assert_eq!(
            parse_rfc3339_micros("1969-12-31T23:59:59Z"),
            Some(-1_000_000)
        );
    }

    #[test]
    fn bare_levels_keep_dependencies_quiet() {
        assert!(
            parse_filter("debug")
                .unwrap()
                .to_string()
                .contains("quinn=warn")
        );
        let full = parse_filter("info,relay_net=trace").unwrap().to_string();
        assert!(!full.contains("quinn"), "{full}");
        assert!(parse_filter("info,=x=").is_err());
    }
}
