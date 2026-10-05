use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Context;
use relay_daemon::{DaemonEvent, DaemonOptions, HostKind};
use relay_engine::{WatchEvent, WatchOptions};

pub fn run(home: std::path::PathBuf) -> anyhow::Result<()> {
    init_log();
    let stop = AtomicBool::new(false);
    let listen: SocketAddr = "127.0.0.1:0".parse().expect("loopback listen address");
    relay_daemon::run(
        &home,
        DaemonOptions {
            listen,
            watch: WatchOptions {
                debounce: Duration::from_millis(50),
                max_batch_delay: Duration::from_millis(200),
                full_scan_interval: Duration::from_secs(600),
                use_watcher: true,
                max_dirty_paths: 10_000,
                reload_on_external_change: false,
            },
            verbose: false,
            host: HostKind::Cli,
            enable_stun: false,
            loopback_only: true,
            placeholders: false,
        },
        &stop,
        &mut |event| log_event(&home, event),
    )
    .with_context(|| format!("daemon for {}", home.display()))?;
    Ok(())
}

fn init_log() {
    let filter = std::env::var("RELAY_LOG").unwrap_or_else(|_| "warn".to_owned());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(io::stderr)
        .try_init();
}

fn log_event(home: &Path, event: &DaemonEvent) {
    let line = match event {
        DaemonEvent::Started {
            device_name,
            device_id,
            listen,
        } => {
            let peers = relay_engine::Engine::open_read_only(home)
                .ok()
                .and_then(|engine| engine.peers().ok())
                .unwrap_or_default();
            let listed = peers
                .iter()
                .map(|peer| format!("{} {:?}", peer.name, peer.addresses))
                .collect::<Vec<_>>()
                .join(", ");
            format!("listening on {listen} as {device_name} ({device_id}) peers [{listed}]")
        }
        DaemonEvent::Warning(message) => format!("warning: {message}"),
        DaemonEvent::Reloading => "reloading".to_owned(),
        DaemonEvent::Paused => "paused".to_owned(),
        DaemonEvent::Resumed => "resumed".to_owned(),
        DaemonEvent::Watch(event) => match event {
            WatchEvent::Scanned {
                space,
                mount,
                report,
                ..
            } if report.has_changes() => format!(
                "scanned {space}/{mount}: +{} ~{} -{}",
                report.created, report.modified, report.deleted
            ),
            WatchEvent::ScanFailed {
                space,
                mount,
                error,
            } => format!("scan failed {space}/{mount}: {error}"),
            WatchEvent::SyncWarning { reason, path, .. } => {
                if path.is_empty() {
                    format!("sync warning: {reason}")
                } else {
                    format!("sync warning {path}: {reason}")
                }
            }
            WatchEvent::PeerConnected { name, .. } => format!("peer connected: {name}"),
            WatchEvent::PeerDisconnected { peer } => format!("peer disconnected: {peer}"),
            WatchEvent::RemoteApplied {
                space,
                mount,
                written,
                deleted,
                conflicts,
                ..
            } => format!("applied {space}/{mount}: +{written} -{deleted} conflicts {conflicts}"),
            _ => return,
        },
    };
    println!("{} {}", home.display(), line);
    let _ = io::stdout().flush();
}
