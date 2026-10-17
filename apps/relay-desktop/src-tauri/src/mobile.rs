//! Android: one runner per process, shared by the sync service and the UI.
//!
//! `RelaySyncService` (Kotlin) keeps the process alive as a foreground
//! service and starts sync through these JNI entries, with or without an
//! activity. The Tauri setup attaches to the same runner when the UI opens.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use jni::JNIEnv;
use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jstring};

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

/// `RelayNative.running()`: a sync thread is alive in this process.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_relay_desktop_RelayNative_running<'local>(
    _env: JNIEnv<'local>,
    _this: JObject<'local>,
) -> jboolean {
    RUNNER
        .get()
        .is_some_and(|runner| runner.is_running())
        .into()
}

/// `RelayNative.busy()`: starting, transferring or indexing right now.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_relay_desktop_RelayNative_busy<'local>(
    _env: JNIEnv<'local>,
    _this: JObject<'local>,
) -> jboolean {
    RUNNER.get().is_some_and(|runner| runner.is_busy()).into()
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

/// How long a send from the phone waits for sync to start and the computer
/// to connect.
const DROP_CONNECT_WAIT: std::time::Duration = std::time::Duration::from_secs(25);

fn java_string(env: &mut JNIEnv, value: &JString) -> Option<String> {
    env.get_string(value).ok().map(String::from)
}

/// `RelayNative.dropSend(home, peer, path)`: start sync if needed and send
/// one file to `peer` (D50). Blocks; call it off the main thread. Returns
/// null once the computer saved the file, otherwise why it failed.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_relay_desktop_RelayNative_dropSend<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    home: JString<'local>,
    peer: JString<'local>,
    path: JString<'local>,
) -> jstring {
    let (Some(home), Some(peer), Some(path)) = (
        java_string(&mut env, &home),
        java_string(&mut env, &peer),
        java_string(&mut env, &path),
    ) else {
        return env
            .new_string("bad arguments")
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut());
    };
    let home = PathBuf::from(home);
    runner(home.clone()).ensure_running();
    match crate::drops::send_when_connected(&home, &peer, PathBuf::from(path), DROP_CONNECT_WAIT) {
        Ok(sent) => {
            log::info!("dropped {} on {peer}", sent.saved_name);
            std::ptr::null_mut()
        }
        Err(err) => env
            .new_string(err)
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut()),
    }
}

/// `RelayNative.peers(home)`: paired devices a photo can go to, one name
/// per line.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_relay_desktop_RelayNative_peers<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    home: JString<'local>,
) -> jstring {
    let names = java_string(&mut env, &home)
        .and_then(|home| crate::drops::peer_names(std::path::Path::new(&home)).ok())
        .unwrap_or_default();
    env.new_string(names.join("\n"))
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// How long a pairing link waits for sync to start on the phone.
const PAIR_CONNECT_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// `RelayNative.pairLink(home, link)`: pair with the device that showed the
/// `relay://pair` QR code. Blocks; call it off the main thread. Returns a
/// line for the user either way.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_relay_desktop_RelayNative_pairLink<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    home: JString<'local>,
    link: JString<'local>,
) -> jstring {
    let line = match (java_string(&mut env, &home), java_string(&mut env, &link)) {
        (Some(home), Some(link)) => {
            let home = PathBuf::from(home);
            runner(home.clone()).ensure_running();
            match crate::pair_link::join(&home, &link, PAIR_CONNECT_WAIT) {
                Ok(peer) => format!("Paired with {peer}"),
                Err(err) => format!("Couldn't pair: {err}"),
            }
        }
        _ => "Couldn't pair: bad arguments".to_owned(),
    };
    env.new_string(line)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
