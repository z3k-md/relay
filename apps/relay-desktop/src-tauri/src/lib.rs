mod commands;
mod error;
mod files;
mod pairs;
mod privacy;
mod runner;
mod settings;
#[cfg(not(target_os = "android"))]
mod sidecar;
#[cfg(not(target_os = "android"))]
mod tray;
#[cfg(not(target_os = "android"))]
mod updates;

use std::path::PathBuf;
use std::sync::Arc;

use tauri::Manager;
#[cfg(not(target_os = "android"))]
use tauri::WindowEvent;

use crate::runner::Runner;

pub struct AppState {
    pub home: PathBuf,
    pub runner: Arc<Runner>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default();
    #[cfg(not(target_os = "android"))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            tray::show_main_window(app);
        }));
    }
    builder = builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        );
    #[cfg(not(target_os = "android"))]
    {
        builder = builder
            .plugin(tauri_plugin_process::init())
            .plugin(tauri_plugin_updater::Builder::new().build())
            .plugin(tauri_plugin_autostart::init(
                tauri_plugin_autostart::MacosLauncher::LaunchAgent,
                Some(vec!["--hidden"]),
            ));
    }
    builder = builder
        .invoke_handler(tauri::generate_handler![
            commands::get_overview,
            commands::init_device,
            commands::list_peers,
            commands::add_peer,
            commands::remove_peer,
            commands::set_peer_manage,
            commands::remote_call,
            commands::pair_start,
            commands::pair_status,
            commands::pair_join,
            commands::pair_cancel,
            commands::list_spaces,
            files::list_folder,
            files::download_file,
            files::open_file,
            files::free_up_space,
            files::set_folder_mode,
            pairs::folder_pair_preview,
            pairs::folder_pair,
            pairs::open_remote_file,
            pairs::list_quick_opens,
            pairs::remove_quick_open,
            commands::create_space,
            commands::add_mount,
            commands::remove_mount,
            commands::share,
            commands::unshare,
            commands::list_offers,
            commands::join_space,
            commands::delete_space,
            commands::list_conflicts,
            commands::resolve_conflict,
            commands::resolve_git_conflicts,
            commands::list_delete_holds,
            commands::decide_delete_hold,
            commands::get_activity,
            commands::pause_sync,
            commands::resume_sync,
            commands::check_for_updates,
            commands::pending_update,
            commands::install_update,
            commands::restart_app,
            commands::get_settings,
            commands::set_settings,
            commands::open_logs_folder,
            commands::full_disk_access,
            commands::open_full_disk_access,
            commands::cli_status,
            commands::install_cli,
        ])
        .setup(|app| {
            let home = app_home(app)?;
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

            #[cfg(not(target_os = "android"))]
            {
                if let Err(err) = tray::setup(app.handle()) {
                    log::warn!("tray setup failed: {err:#}");
                }
                commands::apply_autostart(
                    app.handle(),
                    settings::load(app.handle()).start_at_login,
                );
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
            }

            runner.start(app.handle());
            #[cfg(not(target_os = "android"))]
            updates::spawn_periodic_checks(app.handle());
            Ok(())
        });

    let app = match builder.build(tauri::generate_context!()) {
        Ok(app) => app,
        Err(err) => {
            eprintln!("Relay failed to start: {err}");
            std::process::exit(1);
        }
    };
    app.run(|app, event| match event {
        tauri::RunEvent::ExitRequested { .. } => {
            // Cmd-Q / dock Quit / app.exit all land here. Tray Quit also calls
            // stop_join first; a second call is a no-op once the thread is gone.
            if let Some(state) = app.try_state::<AppState>() {
                state.runner.stop_join();
            }
        }
        // Dock click and a notification click both ask the app to reopen.
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => tray::show_main_window(app),
        _ => {}
    });
}

fn app_home(app: &tauri::App) -> Result<PathBuf, Box<dyn std::error::Error>> {
    #[cfg(target_os = "android")]
    {
        Ok(app.path().app_data_dir()?)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(relay_engine::default_home())
    }
}
