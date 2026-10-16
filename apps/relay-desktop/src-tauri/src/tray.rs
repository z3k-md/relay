use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::error::anyhow_chain;
use crate::runner::{self, RunnerState};
use crate::{AppState, updates};

pub struct TrayMenu {
    pub status: MenuItem<tauri::Wry>,
    pub pause: MenuItem<tauri::Wry>,
}

pub fn setup(app: &AppHandle) -> anyhow::Result<()> {
    let status = MenuItem::with_id(app, "status", "Relay — Starting…", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open Relay", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "Pause sync", true, None::<&str>)?;
    let updates_item = MenuItem::with_id(app, "updates", "Check for updates", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Relay", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;

    let menu = Menu::with_items(
        app,
        &[&status, &sep, &open, &pause, &updates_item, &sep, &quit],
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

    app.manage(TrayMenu { status, pause });
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
