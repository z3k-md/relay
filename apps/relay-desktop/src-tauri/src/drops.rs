//! Files other devices drop here (D50), and sending one from here.
//!
//! A watcher thread follows the host's activity stream over IPC, so drops
//! show whether sync runs in this app or in `relay service`. Each saved drop
//! opens a small always-on-top popup in the corner that never takes focus:
//! a thumbnail to drag into another app, Copy, Open, and Show in folder.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use relay_core::drops::DropSent;
use relay_core::remote::{RemoteError, RemoteErrorCode};
use relay_engine::{DropSettings, Engine};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use relay_ipc::ActivityItem;
use relay_ipc::{Client, DropSendParams};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use tauri::{Emitter, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;

use crate::commands::host_client;
use crate::error::{anyhow_chain, error_chain};
use crate::{AppState, settings};

/// Label of the popup window.
pub const POPUP: &str = "drop";
/// Tells the popup that [`drop_take`] has something new.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const ARRIVED_EVENT: &str = "drop-arrived";
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const POPUP_WIDTH: f64 = 380.0;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const POPUP_HEIGHT: f64 = 124.0;
/// The popup while it plays a video: 16:9 video above the file name and
/// buttons.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const PLAYER_WIDTH: f64 = 480.0;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const PLAYER_HEIGHT: f64 = 392.0;
/// Gap between the popup and the edges of the work area, in logical pixels.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const POPUP_MARGIN: f64 = 16.0;
/// Wait before reconnecting to a host that is not running or went away.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const RECONNECT: Duration = Duration::from_secs(2);
/// Edge of the drag preview, in pixels.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const DRAG_PREVIEW: u32 = 160;

/// One saved drop, as the popup shows it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DropArrival {
    pub path: String,
    pub name: String,
    /// "Pixel sent PXL_1.jpg".
    pub summary: String,
    /// The sending device's name, when the summary names one.
    pub from: Option<String>,
    pub image: bool,
    /// A video the popup can show a frame of and play.
    pub video: bool,
    pub at_ms: u64,
}

/// Drops the popup has not taken yet, and drag previews made ahead of time
/// so a drag starts the moment the thumbnail is pressed.
#[derive(Default)]
pub struct Pending {
    queue: Mutex<VecDeque<DropArrival>>,
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    previews: Mutex<std::collections::HashMap<PathBuf, Vec<u8>>>,
}

/// Previews kept for drags; older ones are dropped.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const MAX_PREVIEWS: usize = 16;

/// Follow the host's activity for drops until the app exits.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn spawn_watcher(app: AppHandle, home: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("relay-drops".into())
        .spawn(move || {
            loop {
                if let Ok(Some(client)) = Client::connect(&home)
                    && let Ok(mut feed) = client.subscribe()
                {
                    while let Ok(Some(item)) = feed.next_item() {
                        if item.kind == relay_daemon::DROP_ACTIVITY {
                            arrived(&app, item);
                        }
                    }
                }
                std::thread::sleep(RECONNECT);
            }
        });
    if let Err(err) = spawned {
        log::warn!("drop watcher: {err}");
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn arrived(app: &AppHandle, item: ActivityItem) {
    let Some(path) = item.detail else {
        return;
    };
    let path = PathBuf::from(path);
    // The popup reads the thumbnail through the asset protocol, one file at
    // a time.
    if let Err(err) = app.asset_protocol_scope().allow_file(&path) {
        log::warn!("drop thumbnail scope: {err}");
    }
    let arrival = DropArrival {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        image: is_image(&path),
        video: is_video(&path),
        path: path.to_string_lossy().into_owned(),
        from: path
            .file_name()
            .and_then(|n| {
                item.summary
                    .strip_suffix(&format!(" sent {}", n.to_string_lossy()))
            })
            .map(str::to_owned),
        summary: item.summary,
        at_ms: item.at_ms,
    };
    if settings::drop_auto_copy(app) {
        let path = path.clone();
        std::thread::spawn(move || {
            if let Err(err) = copy_to_clipboard(&path) {
                log::warn!("drop auto-copy: {err:#}");
            }
        });
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if arrival.image {
        let (app, path) = (app.clone(), path.clone());
        std::thread::spawn(move || {
            if let Some(png) = preview_png(&path)
                && let Ok(mut previews) = app.state::<Pending>().previews.lock()
            {
                if previews.len() >= MAX_PREVIEWS {
                    previews.clear();
                }
                previews.insert(path, png);
            }
        });
    }
    if let Ok(mut pending) = app.state::<Pending>().queue.lock() {
        pending.push_back(arrival);
    }
    refresh_tray(app);
    let app = app.clone();
    let shown = app.clone().run_on_main_thread(move || {
        if let Err(err) = show_popup(&app) {
            log::warn!("drop popup: {err}");
        }
    });
    if let Err(err) = shown {
        log::warn!("drop popup: {err}");
    }
}

/// Open the popup in the bottom corner of the main screen's work area
/// without taking focus, or tell an open one there is more.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn show_popup(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(POPUP) {
        window.emit(ARRIVED_EVENT, ())?;
        if !window.is_visible()? {
            window.show()?;
        }
        return Ok(());
    }
    let window =
        WebviewWindowBuilder::new(app, POPUP, WebviewUrl::App("index.html?popup=drop".into()))
            .title("Relay")
            .inner_size(POPUP_WIDTH, POPUP_HEIGHT)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .decorations(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .focused(false)
            .visible(false)
            .shadow(true)
            .build()?;
    place(&window, POPUP_WIDTH, POPUP_HEIGHT)?;
    // The page shows itself once it has drawn, so no blank frame flashes.
    Ok(())
}

/// Size the popup and put it in the bottom corner of the main screen's work
/// area, so it grows up and to the left.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn place(window: &tauri::WebviewWindow, width: f64, height: f64) -> tauri::Result<()> {
    window.set_size(tauri::LogicalSize::new(width, height))?;
    if let Some(monitor) = window.primary_monitor()? {
        let scale = monitor.scale_factor();
        let area = monitor.work_area();
        let x = f64::from(area.position.x) / scale + f64::from(area.size.width) / scale
            - width
            - POPUP_MARGIN;
        let y = f64::from(area.position.y) / scale + f64::from(area.size.height) / scale
            - height
            - POPUP_MARGIN;
        window.set_position(tauri::LogicalPosition::new(x, y))?;
    }
    Ok(())
}

/// Grow the popup into a player for a video, or back to its normal size.
#[tauri::command]
pub fn drop_popup_player(window: tauri::WebviewWindow, open: bool) -> Result<(), String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let (width, height) = if open {
            (PLAYER_WIDTH, PLAYER_HEIGHT)
        } else {
            (POPUP_WIDTH, POPUP_HEIGHT)
        };
        place(&window, width, height).map_err(|err| err.to_string())
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (window, open);
        Ok(())
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn is_image(path: &Path) -> bool {
    relay_core::drops::mime_for(&path.to_string_lossy()).starts_with("image/")
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn is_video(path: &Path) -> bool {
    relay_core::drops::mime_for(&path.to_string_lossy()).starts_with("video/")
}

/// Drops that arrived since the popup last asked, oldest first.
#[tauri::command]
pub fn drop_take(app: AppHandle) -> Vec<DropArrival> {
    app.state::<Pending>()
        .queue
        .lock()
        .map(|mut pending| pending.drain(..).collect())
        .unwrap_or_default()
}

/// Put the file on the clipboard: as a file for file managers and, for an
/// image, as a bitmap for chat apps and editors.
#[tauri::command(async)]
pub fn drop_copy(path: String) -> Result<(), String> {
    let start = std::time::Instant::now();
    let copied = copy_to_clipboard(Path::new(&path)).map_err(anyhow_chain);
    match &copied {
        Ok(()) => log::info!("drop copied in {} ms", start.elapsed().as_millis()),
        Err(err) => log::warn!("drop copy: {err}"),
    }
    copied
}

#[tauri::command(async)]
pub fn drop_open(app: AppHandle, path: String) -> Result<(), String> {
    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|err| anyhow_chain(err.into()))
}

#[tauri::command(async)]
pub fn drop_reveal(app: AppHandle, path: String) -> Result<(), String> {
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|err| anyhow_chain(err.into()))
}

/// Start dragging the file out of the popup into another app. Called on
/// mouse down; returns when the drag ends.
#[tauri::command]
pub async fn drop_drag(window: tauri::WebviewWindow, path: String) -> Result<(), String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        log::info!("drop drag requested");
        let path = PathBuf::from(path);
        // Made when the drop arrived, so the drag starts while the button
        // is still down; making it now would take as long as a decode.
        let cached = window
            .state::<Pending>()
            .previews
            .lock()
            .ok()
            .and_then(|previews| previews.get(&path).cloned());
        let preview = match cached {
            Some(png) => drag::Image::Raw(png),
            None => drag::Image::File(path.clone()),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let target = window.clone();
        window
            .run_on_main_thread(move || {
                log::info!("drop drag starting");
                let started = drag::start_drag(
                    &target,
                    drag::DragItem::Files(vec![path]),
                    preview,
                    |result, at| log::info!("drop drag ended: {result:?} at {},{}", at.x, at.y),
                    drag::Options::default(),
                );
                let _ = tx.send(started.map_err(|err| err.to_string()));
            })
            .map_err(|err| err.to_string())?;
        let dragged = rx.recv().map_err(|err| err.to_string())?;
        if let Err(err) = &dragged {
            log::warn!("drop drag: {err}");
        }
        dragged
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (window, path);
        Err("dragging files out is not available here".into())
    }
}

/// A small PNG of the image for the drag cursor, or `None` when it is not an
/// image this app can decode.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn preview_png(path: &Path) -> Option<Vec<u8>> {
    let img = decode(path).ok()?;
    let mut png = std::io::Cursor::new(Vec::new());
    img.thumbnail(DRAG_PREVIEW, DRAG_PREVIEW)
        .write_to(&mut png, image::ImageFormat::Png)
        .ok()?;
    Some(png.into_inner())
}

/// Decode an image upright: phones store portrait photos sideways with an
/// EXIF orientation.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn decode(path: &Path) -> anyhow::Result<image::DynamicImage> {
    use image::ImageDecoder;
    let mut decoder = image::ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut img = image::DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok(img)
}

#[cfg(windows)]
fn copy_to_clipboard(path: &Path) -> anyhow::Result<()> {
    use clipboard_win::options::NoClear;
    use clipboard_win::{Clipboard, raw};

    let file = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("the path is not valid Unicode"))?;
    // `set_bitmap` reads a 40-byte BITMAPINFOHEADER, which the encoder
    // writes for 24-bit RGB (RGBA gets a larger V4 header).
    let bmp = if is_image(path) {
        decode(path).ok().and_then(|img| {
            let mut out = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(img.to_rgb8())
                .write_to(&mut out, image::ImageFormat::Bmp)
                .ok()?;
            Some(out.into_inner())
        })
    } else {
        None
    };
    let _open = Clipboard::new_attempts(10).map_err(|err| anyhow::anyhow!("{err}"))?;
    raw::empty().map_err(|err| anyhow::anyhow!("{err}"))?;
    raw::set_file_list_with(&[file], NoClear).map_err(|err| anyhow::anyhow!("{err}"))?;
    if let Some(bmp) = bmp {
        raw::set_bitmap_with(&bmp, NoClear).map_err(|err| anyhow::anyhow!("{err}"))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_to_clipboard(path: &Path) -> anyhow::Result<()> {
    let mut clipboard = arboard::Clipboard::new()?;
    if is_image(path)
        && let Ok(img) = decode(path)
    {
        let rgba = img.to_rgba8();
        clipboard.set_image(arboard::ImageData {
            width: rgba.width() as usize,
            height: rgba.height() as usize,
            bytes: rgba.into_raw().into(),
        })?;
        return Ok(());
    }
    clipboard.set().file_list(&[path])?;
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn copy_to_clipboard(path: &Path) -> anyhow::Result<()> {
    let _ = path;
    anyhow::bail!("copying files is not available on this platform yet")
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DropSettingsView {
    pub receive: bool,
    pub inbox: Option<String>,
    pub inbox_set: bool,
    pub auto_copy: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DropSettingsPatch {
    pub receive: Option<bool>,
    /// A folder, or `""` for the default one.
    pub inbox: Option<String>,
    pub auto_copy: Option<bool>,
}

/// Files in the drop folder, newest first, for the tray's Recent drops.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn recent(home: &Path, limit: usize) -> Vec<PathBuf> {
    let Some(inbox) = Engine::open_read_only(home)
        .and_then(|engine| engine.drop_settings())
        .ok()
        .and_then(|settings| settings.inbox)
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&inbox) else {
        return Vec::new();
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            // A drop keeps the photo's modified time; creation is arrival.
            let arrived = meta.created().or_else(|_| meta.modified()).ok()?;
            Some((arrived, e.path()))
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files.into_iter().take(limit).map(|(_, p)| p).collect()
}

/// Rebuild the tray's Recent drops from the drop folder.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn refresh_tray(app: &AppHandle) {
    let home = app.state::<AppState>().home.clone();
    let app = app.clone();
    std::thread::spawn(move || {
        let files = recent(&home, crate::tray::RECENT_DROPS);
        let inbox = Engine::open_read_only(&home)
            .and_then(|engine| engine.drop_settings())
            .ok()
            .and_then(|settings| settings.inbox);
        let tray_app = app.clone();
        let _ = app.run_on_main_thread(move || {
            crate::tray::set_recent_drops(&tray_app, files, inbox);
        });
    });
}

/// The popup closed on its own before these files were used: leave a
/// Windows notification, so they are in the notification history.
#[tauri::command]
pub fn drop_missed(app: AppHandle, names: Vec<String>, from: Option<String>) {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        use tauri_plugin_notification::NotificationExt;
        let Some(first) = names.first() else {
            return;
        };
        let from = from.unwrap_or_else(|| "A device".to_owned());
        let title = if names.len() == 1 {
            format!("{from} sent {first}")
        } else {
            format!("{from} sent {} files", names.len())
        };
        let body = "Open it from Recent drops in the Relay tray menu.";
        if let Err(err) = app.notification().builder().title(title).body(body).show() {
            log::warn!("drop notification: {err}");
        }
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let _ = (app, names, from);
}

fn settings_view(app: &AppHandle, settings: DropSettings) -> DropSettingsView {
    DropSettingsView {
        receive: settings.receive,
        inbox: settings.inbox.map(|p| p.to_string_lossy().into_owned()),
        inbox_set: settings.inbox_set,
        auto_copy: settings::drop_auto_copy(app),
    }
}

#[tauri::command(async)]
pub fn get_drop_settings(app: AppHandle) -> Result<DropSettingsView, String> {
    let home = app.state::<AppState>().home.clone();
    let settings = Engine::open_read_only(&home)
        .and_then(|engine| engine.drop_settings())
        .map_err(|err| error_chain(&err))?;
    Ok(settings_view(&app, settings))
}

#[tauri::command(async)]
pub fn set_drop_settings(
    app: AppHandle,
    patch: DropSettingsPatch,
) -> Result<DropSettingsView, String> {
    if let Some(value) = patch.auto_copy {
        settings::set_drop_auto_copy(&app, value).map_err(anyhow_chain)?;
    }
    let home = app.state::<AppState>().home.clone();
    let mut engine = Engine::open_for_config(&home).map_err(|err| error_chain(&err))?;
    if let Some(receive) = patch.receive {
        engine
            .set_drops_receive(receive)
            .map_err(|err| error_chain(&err))?;
    }
    if let Some(inbox) = patch.inbox {
        let inbox = (!inbox.is_empty()).then(|| PathBuf::from(inbox));
        engine
            .set_drop_inbox(inbox.as_deref())
            .map_err(|err| error_chain(&err))?;
    }
    let settings = engine.drop_settings().map_err(|err| error_chain(&err))?;
    Ok(settings_view(&app, settings))
}

/// Send one file to a connected peer. Blocks until it is saved there.
#[tauri::command(async)]
pub fn drop_send(app: AppHandle, peer: String, path: String) -> Result<DropSent, RemoteError> {
    let mut client = host_client(&app).ok().flatten().ok_or_else(|| {
        RemoteError::new(
            RemoteErrorCode::Failed,
            "Relay is not running on this computer. Resume sync and try again.",
        )
    })?;
    let params = DropSendParams {
        peer,
        path: PathBuf::from(path),
        name: None,
    };
    client.drop_send(&params).map_err(|err| match err {
        relay_ipc::IpcError::Remote { code, message } => {
            RemoteError::new(RemoteErrorCode::parse(&code), message)
        }
        other => RemoteError::new(RemoteErrorCode::Failed, error_chain(&other)),
    })
}

/// Send `path` to `peer` through the host in `home`, waiting up to `wait`
/// for the host to start and the peer to connect. The phone calls this
/// right after starting sync from a notification, when neither may be up.
// Called from the Android JNI entries in mobile.rs, and from tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn send_when_connected(
    home: &Path,
    peer: &str,
    path: PathBuf,
    wait: Duration,
) -> Result<DropSent, String> {
    let deadline = std::time::Instant::now() + wait;
    let params = DropSendParams {
        peer: peer.to_owned(),
        path,
        name: None,
    };
    let mut last = String::from("Relay is not running");
    loop {
        match Client::connect(home) {
            Ok(Some(mut client)) => match client.drop_send(&params) {
                Ok(sent) => return Ok(sent),
                // Not connected yet: discovery and the handshake take a
                // few seconds after sync starts.
                Err(relay_ipc::IpcError::Remote { code, message })
                    if code == RemoteErrorCode::Offline.as_str() =>
                {
                    last = message;
                }
                Err(err) => return Err(error_chain(&err)),
            },
            Ok(None) => {}
            Err(err) => last = error_chain(&err),
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("couldn't reach {peer}: {last}"));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Names of the devices this one may send to, for the phone's buttons.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn peer_names(home: &Path) -> Result<Vec<String>, String> {
    let engine = Engine::open_read_only(home).map_err(|err| error_chain(&err))?;
    let peers = engine.peers().map_err(|err| error_chain(&err))?;
    Ok(peers
        .into_iter()
        .filter(|p| !p.revoked)
        .map(|p| p.name)
        .collect())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Uses the real clipboard, so it runs only when asked:
    /// `cargo test -p relay-desktop -- --ignored clipboard`.
    #[test]
    #[ignore]
    fn clipboard_gets_the_file_and_an_upright_bitmap() {
        use clipboard_win::{Getter, formats, is_format_avail};

        let dir = std::env::temp_dir().join(format!("relay-drop-clip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wide.png");
        image::RgbImage::from_pixel(64, 32, image::Rgb([200, 30, 30]))
            .save(&path)
            .unwrap();
        copy_to_clipboard(&path).unwrap();

        let _open = clipboard_win::Clipboard::new_attempts(10).unwrap();
        assert!(is_format_avail(formats::CF_HDROP), "file list");
        assert!(is_format_avail(formats::CF_BITMAP), "bitmap");
        assert!(
            is_format_avail(formats::CF_DIB),
            "DIB synthesized for chat apps"
        );
        let mut files = Vec::new();
        formats::FileList.read_clipboard(&mut files).unwrap();
        assert_eq!(files, vec![path.to_string_lossy().into_owned()]);
        let mut dib = Vec::new();
        clipboard_win::raw::get_vec(formats::CF_DIB, &mut dib).unwrap();
        let width = i32::from_le_bytes(dib[4..8].try_into().unwrap());
        let height = i32::from_le_bytes(dib[8..12].try_into().unwrap());
        assert_eq!((width, height.abs()), (64, 32));
        drop(_open);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod phone_tests {
    use super::*;

    #[test]
    fn sending_without_a_host_gives_up_after_the_wait() {
        let home = std::env::temp_dir().join(format!("relay-drop-nohost-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let start = std::time::Instant::now();
        let err = send_when_connected(
            &home,
            "Desk",
            home.join("x.jpg"),
            Duration::from_millis(800),
        )
        .unwrap_err();
        assert!(err.starts_with("couldn't reach Desk"), "{err}");
        assert!(start.elapsed() >= Duration::from_millis(800));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn peer_names_skip_revoked_devices() {
        let home = std::env::temp_dir().join(format!("relay-drop-peers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut engine = Engine::init(&home, "Pixel").unwrap();
        engine
            .add_peer("Desk", relay_core::DeviceId::random(), &[])
            .unwrap();
        engine
            .add_peer("Old laptop", relay_core::DeviceId::random(), &[])
            .unwrap();
        engine.revoke_peer("Old laptop").unwrap();
        drop(engine);
        assert_eq!(peer_names(&home).unwrap(), vec!["Desk".to_owned()]);
        let _ = std::fs::remove_dir_all(home);
    }
}
