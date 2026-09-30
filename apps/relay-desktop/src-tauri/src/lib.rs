mod commands;
mod error;
mod runner;
mod settings;
mod sidecar;
mod tray;
mod updates;

use std::path::PathBuf;
use std::sync::Arc;

use tauri::{Manager, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;

use crate::runner::Runner;

pub struct AppState {
    pub home: PathBuf,
    pub runner: Arc<Runner>,
}

pub fn run() {
    let mut builder = tauri::Builder::default();
    builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
        tray::show_main_window(app);
    }));
    builder = builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            commands::get_overview,
            commands::init_device,
            commands::list_peers,
            commands::add_peer,
            commands::remove_peer,
            commands::pair_start,
            commands::pair_status,
            commands::pair_join,
            commands::pair_cancel,
            commands::list_spaces,
            commands::create_space,
            commands::add_mount,
            commands::share,
            commands::unshare,
            commands::list_offers,
            commands::join_space,
            commands::list_conflicts,
            commands::resolve_conflict,
            commands::resolve_git_conflicts,
            commands::list_delete_holds,
            commands::decide_delete_hold,
            commands::get_activity,
            commands::pause_sync,
            commands::resume_sync,
            commands::check_for_updates,
            commands::install_update,
            commands::get_settings,
            commands::set_settings,
            commands::open_logs_folder,
            commands::cli_status,
            commands::install_cli,
        ])
        .setup(|app| {
            let home = relay_engine::default_home();
            let _ = std::fs::create_dir_all(home.join("logs"));

            if let Err(err) = settings::apply_first_run_defaults(app.handle()) {
                log::warn!("settings defaults: {err:#}");
            }
            if let Err(err) = settings::migrate_paused_to_db(app.handle(), &home) {
                log::warn!("paused-flag migration: {err:#}");
            }

            let runner = Arc::new(Runner::new(home.clone()));
            app.manage(AppState {
                home,
                runner: Arc::clone(&runner),
            });

            if let Err(err) = tray::setup(app.handle()) {
                log::warn!("tray setup failed: {err:#}");
            }
            commands::apply_autostart(app.handle(), settings::load(app.handle()).start_at_login);
            commands::maybe_install_cli(app.handle());

            if let Some(window) = app.get_webview_window("main") {
                let window_hide = window.clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = window_hide.hide();
                    }
                });
                if std::env::args().any(|a| a == "--hidden") {
                    let _ = window.hide();
                }
            }

            runner.start(app.handle());
            updates::spawn_periodic_checks(app.handle());
            Ok(())
        });

    if let Err(err) = builder.run(tauri::generate_context!()) {
        eprintln!("Relay failed to start: {err}");
        std::process::exit(1);
    }
}
