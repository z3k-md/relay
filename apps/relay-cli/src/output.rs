//! Human output of `relay run`. Under `--log-file` (the service) it goes into
//! the log file with the host's `tracing` lines (D56); otherwise to the
//! terminal.

use std::sync::atomic::{AtomicBool, Ordering};

use relay_daemon::logging;

static TO_FILE: AtomicBool = AtomicBool::new(false);

/// Send human output to the log file from now on.
pub fn to_file() {
    TO_FILE.store(true, Ordering::SeqCst);
}

pub fn is_configured() -> bool {
    TO_FILE.load(Ordering::SeqCst) && logging::file_is_set()
}

pub fn out_line(line: &str) {
    if is_configured() {
        let _ = logging::write_line(line);
    } else {
        println!("{line}");
    }
}

pub fn err_line(line: &str) {
    if is_configured() {
        let _ = logging::write_line(line);
    } else {
        eprintln!("{line}");
    }
}

pub fn write_line(line: &str) -> std::io::Result<()> {
    logging::write_line(line)
}
