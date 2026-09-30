use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const ROTATE_AFTER: u64 = 10 * 1024 * 1024;

static LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<File>> {
    LOG.get_or_init(|| Mutex::new(None))
}

fn lock_log() -> std::sync::MutexGuard<'static, Option<File>> {
    slot().lock().unwrap_or_else(|e| e.into_inner())
}

pub fn is_configured() -> bool {
    LOG.get()
        .is_some_and(|m| m.lock().map(|g| g.is_some()).unwrap_or(false))
}

pub fn configure(path: &Path, version: &str) -> anyhow::Result<()> {
    let mut file = open_log(path)?;
    write_start_header(&mut file, version)?;
    *lock_log() = Some(file);
    Ok(())
}

pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = write_line(&format!("panic: {info}"));
        previous(info);
    }));
}

pub fn out_line(line: &str) {
    if is_configured() {
        let _ = write_line(line);
    } else {
        println!("{line}");
    }
}

pub fn err_line(line: &str) {
    if is_configured() {
        let _ = write_line(line);
    } else {
        eprintln!("{line}");
    }
}

pub fn write_line(line: &str) -> io::Result<()> {
    let mut guard = lock_log();
    if let Some(file) = guard.as_mut() {
        writeln!(file, "{line}")?;
        file.flush()?;
        Ok(())
    } else {
        Err(io::Error::other("log file is not configured"))
    }
}

pub fn write_bytes(buf: &[u8]) -> io::Result<()> {
    let mut guard = lock_log();
    if let Some(file) = guard.as_mut() {
        file.write_all(buf)?;
        file.flush()?;
        Ok(())
    } else {
        io::stderr().write_all(buf)?;
        io::stderr().flush()
    }
}

pub fn flush() -> io::Result<()> {
    let mut guard = lock_log();
    if let Some(file) = guard.as_mut() {
        file.flush()
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct TracingWriter;

impl io::Write for TracingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_bytes(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TracingWriter {
    type Writer = TracingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TracingWriter
    }
}

pub fn open_log(path: &Path) -> anyhow::Result<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    rotate_if_needed(path)?;
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

pub fn rotate_if_needed(path: &Path) -> anyhow::Result<()> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if meta.len() <= ROTATE_AFTER {
        return Ok(());
    }
    let rotated = rotated_name(path);
    if rotated.exists() {
        fs::remove_file(&rotated)?;
    }
    fs::rename(path, &rotated)?;
    Ok(())
}

fn rotated_name(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("relay.log"))
        .to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

pub fn write_start_header(file: &mut File, version: &str) -> io::Result<()> {
    writeln!(
        file,
        "==== relay {version} started {} ====",
        format_started_at()
    )?;
    file.flush()
}

pub fn format_started_at() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_utc_ymdhms(secs)
}

fn format_utc_ymdhms(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    let hour = tod / 3600;
    let min = (tod % 3600) / 60;
    let sec = tod % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02} UTC")
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn rotate_renames_oversized_file_to_dot_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("relay.log");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let oversized = vec![b'x'; (ROTATE_AFTER as usize) + 16];
        fs::write(&path, &oversized).unwrap();

        let mut file = open_log(&path).unwrap();
        writeln!(file, "fresh").unwrap();
        file.flush().unwrap();
        drop(file);

        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.trim(), "fresh");
        let rotated = path.with_file_name("relay.log.1");
        assert_eq!(
            fs::metadata(&rotated).unwrap().len(),
            oversized.len() as u64
        );
    }

    #[test]
    fn rotate_is_noop_for_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.log");
        fs::write(&path, b"keep\n").unwrap();
        rotate_if_needed(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep\n");
        assert!(!path.with_file_name("relay.log.1").exists());
    }

    #[test]
    fn header_uses_utc_civil_date() {
        assert_eq!(format_utc_ymdhms(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc_ymdhms(1_704_067_200), "2024-01-01 00:00:00 UTC");
    }

    #[test]
    fn open_log_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("relay.log");
        let mut file = open_log(&path).unwrap();
        write_start_header(&mut file, "0.1.0 (deadbee)").unwrap();
        drop(file);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("==== relay 0.1.0 (deadbee) started "));
        assert!(text.contains(" UTC ===="));
    }
}
