//! Android: one runner per process, shared by the sync service and the UI.
//!
//! `RelaySyncService` (Kotlin) keeps the process alive as a foreground
//! service and starts sync through these JNI entries, with or without an
//! activity. The Tauri setup attaches to the same runner when the UI opens.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use jni::JNIEnv;
use jni::objects::{JObject, JString};
use jni::sys::jstring;

use crate::runner::{self, Runner};

static RUNNER: OnceLock<Arc<Runner>> = OnceLock::new();

pub fn runner(home: PathBuf) -> Arc<Runner> {
    let runner = RUNNER.get_or_init(|| Arc::new(Runner::new(home.clone())));
    if runner.home() != home {
        log::warn!(
            "runner already uses {}; ignoring {}",
            runner.home().display(),
            home.display()
        );
    }
    Arc::clone(runner)
}

/// `RelayNative.start(home)`: start sync unless it is already running.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_relay_desktop_RelayNative_start<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    home: JString<'local>,
) {
    let Ok(home) = env.get_string(&home) else {
        return;
    };
    runner(PathBuf::from(String::from(home))).ensure_running();
}

/// `RelayNative.stop()`: stop sync and wait briefly for the engine to exit.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_relay_desktop_RelayNative_stop<'local>(
    _env: JNIEnv<'local>,
    _this: JObject<'local>,
) {
    if let Some(runner) = RUNNER.get() {
        runner.stop_join();
    }
}

/// `RelayNative.status()`: one line for the service notification.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_relay_desktop_RelayNative_status<'local>(
    env: JNIEnv<'local>,
    _this: JObject<'local>,
) -> jstring {
    let line = match RUNNER.get() {
        Some(runner) => runner::status_line(
            &runner.state(),
            runner.connected_count(),
            runner.transfer_summary().as_deref(),
        ),
        None => "Relay — Starting…".to_owned(),
    };
    let line = line.trim_start_matches("Relay — ");
    env.new_string(line)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
