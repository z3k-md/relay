use std::path::PathBuf;
use std::sync::Mutex;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use tauri_plugin_opener::OpenerExt;

use crate::error::anyhow_chain;
use crate::runner::{self, RunnerState};
use crate::{AppState, updates};

pub struct TrayMenu {
    pub status: MenuItem<tauri::Wry>,
    pub pause: MenuItem<tauri::Wry>,
    pub drops: Submenu<tauri::Wry>,
}

/// Files shown under Recent drops (D50), newest first.
pub const RECENT_DROPS: usize = 8;

/// What the Recent drops entries open: the files by position, and the folder.
#[derive(Default)]
struct RecentDrops(Mutex<(Vec<PathBuf>, Option<PathBuf>)>);

pub fn setup(app: &AppHandle) -> anyhow::Result<()> {
    let status = MenuItem::with_id(app, "status", "Relay — Starting…", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open Relay", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "Pause sync", true, None::<&str>)?;
    let none = MenuItem::with_id(app, "drops-none", "No drops yet", false, None::<&str>)?;
    let drops = Submenu::with_id_and_items(app, "drops", "Recent drops", true, &[&none])?;
    let updates_item = MenuItem::with_id(app, "updates", "Check for updates", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Relay", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;

    let menu = Menu::with_items(
        app,
        &[
            &status,
            &sep,
            &open,
            &drops,
            &pause,
            &updates_item,
            &sep,
            &quit,
        ],
    )?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing window icon for tray"))?;

    TrayIconBuilder::with_id("main")
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("Relay")
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            "pause" => toggle_pause(app),
            "updates" => {
                show_main_window(app);
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = updates::check_and_maybe_install(&app, true).await;
                });
            }
            "quit" => quit_app(app),
            "drops-folder" => open_recent(app, None),
            id if id.starts_with("drop:") => {
                if let Ok(index) = id["drop:".len()..].parse::<usize>() {
                    open_recent(app, Some(index));
                }
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    app.manage(TrayMenu {
        status,
        pause,
        drops,
    });
    app.manage(RecentDrops::default());
    Ok(())
}

pub fn refresh(app: &AppHandle) {
    let Some(tray) = app.try_state::<TrayMenu>() else {
        return;
    };
    let (state, connected) = if let Some(app_state) = app.try_state::<AppState>() {
        (app_state.runner.state(), app_state.runner.connected_count())
    } else {
        (RunnerState::Starting, 0)
    };
    let summary = app
        .try_state::<AppState>()
        .and_then(|state| state.runner.transfer_summary());
    let _ = tray
        .status
        .set_text(runner::status_line(&state, connected, summary.as_deref()));
    let pause_label = match &state {
        RunnerState::Paused | RunnerState::Stopped => "Resume sync",
        _ => "Pause sync",
    };
    let _ = tray.pause.set_text(pause_label);
}

pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn toggle_pause(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let result = if matches!(
        state.runner.state(),
        RunnerState::Paused | RunnerState::Stopped
    ) {
        state.runner.resume(app)
    } else {
        state.runner.pause()
    };
    if let Err(err) = result {
        log::warn!("tray pause/resume: {}", anyhow_chain(err));
    }
}

pub fn quit_app(app: &AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        state.runner.stop_join();
    }
    app.exit(0);
}

/// Show `files` under Recent drops, newest first, with an entry for `inbox`.
pub fn set_recent_drops(app: &AppHandle, files: Vec<PathBuf>, inbox: Option<PathBuf>) {
    let Some(menu) = app.try_state::<TrayMenu>() else {
        return;
    };
    let rebuilt = (|| -> tauri::Result<()> {
        for item in menu.drops.items()? {
            menu.drops.remove(&item)?;
        }
        if files.is_empty() {
            let none = MenuItem::with_id(app, "drops-none", "No drops yet", false, None::<&str>)?;
            menu.drops.append(&none)?;
        }
        for (index, file) in files.iter().enumerate() {
            let name = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let item = MenuItem::with_id(app, format!("drop:{index}"), name, true, None::<&str>)?;
            menu.drops.append(&item)?;
        }
        menu.drops.append(&PredefinedMenuItem::separator(app)?)?;
        let folder = MenuItem::with_id(
            app,
            "drops-folder",
            "Open drops folder",
            inbox.is_some(),
            None::<&str>,
        )?;
        menu.drops.append(&folder)?;
        Ok(())
    })();
    if let Err(err) = rebuilt {
        log::warn!("tray recent drops: {err}");
    }
    if let Some(recent) = app.try_state::<RecentDrops>()
        && let Ok(mut slot) = recent.0.lock()
    {
        *slot = (files, inbox);
    }
}

/// Open a recent drop by position, or the drops folder for `None`.
fn open_recent(app: &AppHandle, index: Option<usize>) {
    let Some(recent) = app.try_state::<RecentDrops>() else {
        return;
    };
    let target = recent.0.lock().ok().and_then(|slot| match index {
        Some(i) => slot.0.get(i).cloned(),
        None => slot.1.clone(),
    });
    if let Some(path) = target
        && let Err(err) = app.opener().open_path(path.to_string_lossy(), None::<&str>)
    {
        log::warn!("open recent drop: {err}");
    }
}
