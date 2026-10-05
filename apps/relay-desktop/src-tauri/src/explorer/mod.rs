//! Relay Explorer preview (P0 spike, docs/proposals/explorer-p0.md): a
//! separate window that lists local folders with streamed batches, live
//! updates, shell icons and thumbnails, the Explorer context menu and OLE
//! drag and drop, plus the measurements that decide whether WebView2 is fast
//! enough to build the full explorer on.

mod images;
#[cfg(windows)]
mod win;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use relay_explorer::Entry;
use relay_explorer::list::{ListOptions, stream_dir};
use relay_explorer::watch::{Change, DirWatcher};
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

pub use images::{handle_icon, handle_thumb};

pub const LABEL: &str = "explorer";

#[derive(Default)]
pub struct ExplorerState {
    listings: Mutex<HashMap<String, Listing>>,
    /// The folder the frontend says is under the pointer during a drag.
    drop_target: Arc<Mutex<Option<PathBuf>>>,
    #[cfg(windows)]
    shell: std::sync::OnceLock<win::Shell>,
}

impl ExplorerState {
    #[cfg(windows)]
    fn shell(&self) -> Result<&win::Shell, String> {
        if let Some(shell) = self.shell.get() {
            return Ok(shell);
        }
        let shell = win::Shell::new().map_err(|e| e.to_string())?;
        Ok(self.shell.get_or_init(|| shell))
    }
}

struct Listing {
    cancel: Arc<AtomicBool>,
    _watcher: Option<DirWatcher>,
}

impl Drop for Listing {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ListEvent {
    Batch {
        entries: Vec<Entry>,
    },
    Done {
        total: usize,
        #[serde(rename = "elapsedMs")]
        elapsed_ms: f64,
    },
    Error {
        message: String,
    },
    Changes {
        changes: Vec<Change>,
    },
}

/// Native drag and drop over the explorer window. Coordinates are physical
/// client pixels; the frontend divides by `devicePixelRatio`.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum DragEvent {
    #[serde(rename_all = "camelCase")]
    Over {
        x: i32,
        y: i32,
        enter: bool,
        /// Real paths on offer.
        count: usize,
        /// Files offered by content only (Outlook attachments, zip entries).
        virtual_count: usize,
        /// Clipboard formats on offer; only sent on enter.
        formats: Vec<String>,
        effect: u32,
    },
    Leave,
    #[serde(rename_all = "camelCase")]
    Dropped {
        effect: u32,
        target: String,
        count: usize,
    },
    /// The copy or move started by a drop finished.
    #[serde(rename_all = "camelCase")]
    Done {
        target: String,
        ok: bool,
        message: Option<String>,
        elapsed_ms: f64,
    },
}

/// Open (or focus) the explorer window.
#[tauri::command]
pub fn explorer_open(app: AppHandle) -> Result<(), String> {
    open_window(&app).map_err(|e| e.to_string())
}

pub fn open_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.unminimize();
        window.show()?;
        return window.set_focus();
    }
    let mut builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
        .title("Relay Explorer (preview)")
        .inner_size(1180.0, 760.0)
        .min_inner_size(640.0, 420.0)
        // Our own IDropTarget replaces wry's path-only handler (Windows).
        .disable_drag_drop_handler();
    if autobench_output().is_some() {
        // WebView2 runs one browser process per profile. An installed Relay
        // running beside the benchmark owns the default profile, so its
        // processes would host this window and fall outside the process
        // tree the memory bar measures. A profile of our own keeps them in.
        builder = builder.data_directory(
            std::env::temp_dir()
                .join("relay-explorer-bench")
                .join("webview"),
        );
    }
    let window = builder.build()?;
    let app = app.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::Destroyed = event {
            if let Some(state) = app.try_state::<ExplorerState>() {
                let prefix = format!("{LABEL}/");
                state
                    .listings
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|key, _| !key.starts_with(&prefix));
            }
            #[cfg(windows)]
            win::forget_drop_target(LABEL);
        }
    });
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Places {
    home: String,
    roots: Vec<String>,
    /// True when the native shell features (icons, menus, drag and drop)
    /// are available on this platform.
    native: bool,
}

// Commands that touch the disk are async: Tauri runs sync commands on the
// main thread, and a disconnected network drive can stall a metadata call.

#[tauri::command]
pub async fn explorer_places() -> Places {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    Places {
        home: home.to_string_lossy().into_owned(),
        roots: roots(),
        native: cfg!(windows),
    }
}

#[cfg(windows)]
fn roots() -> Vec<String> {
    (b'A'..=b'Z')
        .map(|d| format!("{}:\\", d as char))
        .filter(|root| std::fs::metadata(root).is_ok())
        .collect()
}

#[cfg(not(windows))]
fn roots() -> Vec<String> {
    vec!["/".into()]
}

fn listing_key(window: &WebviewWindow, tab: &str) -> String {
    format!("{}/{tab}", window.label())
}

/// List `path` for one tab, streaming batches and then live changes on
/// `on_event` until the tab lists something else or closes.
#[tauri::command]
pub async fn explorer_list(
    window: WebviewWindow,
    state: State<'_, ExplorerState>,
    tab: String,
    path: String,
    on_event: Channel<ListEvent>,
) -> Result<(), String> {
    let dir = PathBuf::from(&path);
    let cancel = Arc::new(AtomicBool::new(false));

    // Watch first so nothing that changes during the listing is missed; the
    // frontend treats an upsert for a name it already has as a replace.
    let watcher = {
        let channel = on_event.clone();
        let cancel = Arc::clone(&cancel);
        DirWatcher::start(dir.clone(), move |changes| {
            if !cancel.load(Ordering::Relaxed) {
                let _ = channel.send(ListEvent::Changes { changes });
            }
        })
        .map_err(|e| log::debug!("explorer watch {path}: {e}"))
        .ok()
    };

    let previous = state
        .listings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            listing_key(&window, &tab),
            Listing {
                cancel: Arc::clone(&cancel),
                _watcher: watcher,
            },
        );
    // Stops the old stream and watcher, outside the lock.
    drop(previous);

    tauri::async_runtime::spawn_blocking(move || {
        let mut send = |entries: Vec<Entry>| {
            let _ = on_event.send(ListEvent::Batch { entries });
        };
        match stream_dir(&dir, &ListOptions::default(), &cancel, &mut send) {
            Ok(stats) if !stats.cancelled => {
                let _ = on_event.send(ListEvent::Done {
                    total: stats.total,
                    elapsed_ms: stats.elapsed.as_secs_f64() * 1000.0,
                });
            }
            Ok(_) => {}
            Err(err) => {
                let _ = on_event.send(ListEvent::Error {
                    message: err.to_string(),
                });
            }
        }
    });
    Ok(())
}

/// Stop a closed tab's listing and watcher.
#[tauri::command]
pub async fn explorer_close_tab(
    window: WebviewWindow,
    state: State<'_, ExplorerState>,
    tab: String,
) -> Result<(), String> {
    let listing = state
        .listings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&listing_key(&window, &tab));
    drop(listing);
    Ok(())
}

#[tauri::command]
pub async fn explorer_open_item(path: String) -> Result<(), String> {
    tauri_plugin_opener::open_path(path, None::<&str>).map_err(|e| e.to_string())
}

/// Show the Explorer context menu for `names` in `folder` (or the folder
/// background when `names` is empty) at client CSS pixel `x`, `y`. Returns
/// the chosen verb when the app should handle it (only "rename" for now).
#[tauri::command]
pub async fn explorer_context_menu(
    window: WebviewWindow,
    folder: String,
    names: Vec<String>,
    x: f64,
    y: f64,
    extended: bool,
) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        win::context_menu(window, folder, names, x, y, extended).await
    }
    #[cfg(not(windows))]
    {
        let _ = (window, folder, names, x, y, extended);
        Ok(None)
    }
}

/// Install our drop target on the window's WebView2 children. Returns false
/// where native drag and drop is not available.
#[tauri::command]
pub async fn explorer_ready(
    window: WebviewWindow,
    state: State<'_, ExplorerState>,
    on_drag: Channel<DragEvent>,
) -> Result<bool, String> {
    #[cfg(windows)]
    {
        let shell = state.shell()?.clone();
        win::register_drop_target(window, shell, Arc::clone(&state.drop_target), on_drag).await
    }
    #[cfg(not(windows))]
    {
        let _ = (window, state, on_drag);
        Ok(false)
    }
}

/// The frontend's answer to "what is under the pointer" during a drag.
#[tauri::command]
pub fn explorer_drop_target(state: State<'_, ExplorerState>, path: Option<String>) {
    *state.drop_target.lock().unwrap_or_else(|e| e.into_inner()) = path.map(PathBuf::from);
}

/// Start a shell drag of `names` in `folder`. Resolves when the drag ends,
/// with the effect the target reported.
#[tauri::command]
pub async fn explorer_start_drag(
    window: WebviewWindow,
    folder: String,
    names: Vec<String>,
) -> Result<u32, String> {
    #[cfg(windows)]
    {
        win::start_drag(window, folder, names).await
    }
    #[cfg(not(windows))]
    {
        let _ = (window, folder, names);
        Ok(0)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryInfo {
    processes: u32,
    private_bytes: u64,
    working_set_bytes: u64,
}

/// Memory of the app and its WebView2 processes (Windows only).
#[tauri::command]
pub async fn explorer_memory() -> Option<MemoryInfo> {
    #[cfg(windows)]
    {
        relay_shell_win::memory::process_tree()
            .ok()
            .map(|m| MemoryInfo {
                processes: m.processes,
                private_bytes: m.private_bytes,
                working_set_bytes: m.working_set_bytes,
            })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Where the one-shot benchmark writes its results, when the app was started
/// with `RELAY_EXPLORER_BENCH` set (to a file path, or `1` for the default).
pub fn autobench_output() -> Option<PathBuf> {
    let value = std::env::var_os("RELAY_EXPLORER_BENCH")?;
    if value.is_empty() || value == "0" {
        return None;
    }
    Some(if value == "1" {
        std::env::temp_dir()
            .join("relay-explorer-bench")
            .join("results.txt")
    } else {
        PathBuf::from(value)
    })
}

/// The results file, when the frontend should run the benchmark by itself.
#[tauri::command]
pub async fn explorer_autobench() -> Option<String> {
    autobench_output().map(|p| p.to_string_lossy().into_owned())
}

/// Write the benchmark results and quit. Only in benchmark mode.
#[tauri::command]
pub async fn explorer_save_results(app: AppHandle, text: String) -> Result<(), String> {
    let path = autobench_output().ok_or("not in benchmark mode")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, &text).map_err(|e| e.to_string())?;
    println!(
        "{text}\n\nRelay Explorer benchmark results written to {}",
        path.display()
    );
    app.exit(0);
    Ok(())
}

/// Create (once) a benchmark folder and return its path. `kind` is "files"
/// (empty files of mixed types) or "images" (small PNGs for thumbnails).
#[tauri::command]
pub async fn explorer_make_bench(
    kind: String,
    count: u32,
    on_progress: Channel<u32>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        relay_explorer::bench::make(&kind, count, |n| {
            let _ = on_progress.send(n);
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map(|p| p.to_string_lossy().into_owned())
    .map_err(|e| e.to_string())
}
