//! The app's log and diagnostics (D56): the host log every Relay host
//! writes, the Settings switch for detailed logging, and diagnostics bundles.

use std::path::{Path, PathBuf};
use std::time::Duration;

use relay_daemon::diag::{self, Collected, Pull};
use relay_daemon::logging::{self, LogLevel, LogOptions};
use relay_ipc::LogLevelParams;
use tauri::{AppHandle, Manager};

use crate::AppState;
use crate::error::error_chain;

/// Install the subscriber, and point it at `file` once the home is known.
/// Safe to call again: the Android service and the UI both do.
pub fn init(file: Option<PathBuf>) {
    let stderr = cfg!(debug_assertions)
        .then(|| std::env::var("RELAY_LOG").unwrap_or_else(|_| "warn".to_owned()));
    logging::init(
        LogOptions {
            file,
            stderr,
            mirror: mirror(),
        },
        env!("CARGO_PKG_VERSION"),
    );
    logging::install_panic_hook();
}

#[cfg(target_os = "android")]
fn mirror() -> Option<fn(tracing::Level, &str)> {
    Some(logcat)
}

#[cfg(not(target_os = "android"))]
fn mirror() -> Option<fn(tracing::Level, &str)> {
    None
}

/// Every line also goes to logcat, for `adb logcat`.
#[cfg(target_os = "android")]
fn logcat(level: tracing::Level, line: &str) {
    let level = match level {
        tracing::Level::ERROR => log::Level::Error,
        tracing::Level::WARN => log::Level::Warn,
        tracing::Level::INFO => log::Level::Info,
        tracing::Level::DEBUG => log::Level::Debug,
        tracing::Level::TRACE => log::Level::Trace,
    };
    android_logger::log(
        &log::Record::builder()
            .level(level)
            .target("relay")
            .args(format_args!("{line}"))
            .build(),
    );
}

/// The running host's log level: this app's own, or the background
/// service's when that runs sync instead.
#[tauri::command(async)]
pub fn get_log_level(app: AppHandle) -> Result<Option<LogLevel>, String> {
    if let Some(mut client) = crate::commands::host_client(&app)? {
        return client
            .log_level(&LogLevelParams::default())
            .map(Some)
            .map_err(|err| error_chain(&err));
    }
    Ok(logging::level())
}

/// Turn detailed logging on for `minutes`, or back to the default with no
/// `filter`.
#[tauri::command(async)]
pub fn set_log_level(
    app: AppHandle,
    filter: Option<String>,
    minutes: Option<u64>,
) -> Result<LogLevel, String> {
    let for_secs = minutes.map(|m| m * 60);
    if let Some(mut client) = crate::commands::host_client(&app)? {
        return client
            .log_level(&LogLevelParams {
                reset: filter.is_none(),
                filter,
                for_secs,
            })
            .map_err(|err| error_chain(&err));
    }
    logging::set_level(filter.as_deref(), for_secs.map(Duration::from_secs))
}

/// Save this device's diagnostics bundle, and with `all` every connected
/// device's that lets this one manage it, into a new folder in Downloads.
#[tauri::command(async)]
pub fn save_diagnostics(app: AppHandle, all: bool) -> Result<Collected, String> {
    let home = app.state::<AppState>().home.clone();
    let dir =
        diagnostics_dir(&home).join(format!("relay-diag-{}", diag::stamp(logging::unix_ms())));
    let pull = if all { Pull::All } else { Pull::None };
    diag::collect(&home, &dir, None, pull).map_err(|err| format!("{err:#}"))
}

fn diagnostics_dir(home: &Path) -> PathBuf {
    #[cfg(not(target_os = "android"))]
    if let Some(downloads) =
        directories::UserDirs::new().and_then(|d| d.download_dir().map(Path::to_path_buf))
    {
        return downloads;
    }
    home.join("diagnostics")
}
