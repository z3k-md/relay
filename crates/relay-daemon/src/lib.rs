//! In-process Relay sync runner. The CLI and desktop app both host this.
//!
//! The engine stays free of Tokio and `relay-net` (D19). This crate maps
//! `NetEvent` / `NetCommand` onto `SyncInput` / `SyncOutput` and reopens
//! when another process commits to the database.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use relay_engine::{Engine, EngineError, RunExit, SyncInput, SyncOutput, WatchEvent, WatchOptions};
use relay_net::{NetCommand, NetConfig, NetEvent, PeerConfig};

const SETTLE_SLICE: Duration = Duration::from_millis(300);
const SETTLE_CAP: Duration = Duration::from_secs(3);
const OPEN_RETRY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub listen: SocketAddr,
    pub watch: WatchOptions,
    pub verbose: bool,
}

#[derive(Debug, Clone)]
pub enum DaemonEvent {
    Started {
        device_name: String,
        device_id: String,
        listen: SocketAddr,
    },
    Watch(WatchEvent),
    Reloading,
    Warning(String),
}

/// Runs until `stop` is set. Reopens itself when another process changes the database.
pub fn run(
    home: &Path,
    opts: DaemonOptions,
    stop: &AtomicBool,
    on_event: &mut dyn FnMut(&DaemonEvent),
) -> Result<()> {
    tracing::debug!(verbose = opts.verbose, listen = %opts.listen, "daemon starting");
    while !stop.load(Ordering::Relaxed) {
        let mut engine = open_writable(home, stop)?;
        let identity = Arc::new(engine.load_identity()?);
        let peers = engine.peers()?;

        let (tx, rx) = mpsc::channel::<SyncInput>();
        let listen_error = Arc::new(Mutex::new(None::<String>));
        let sink = {
            let listen_error = Arc::clone(&listen_error);
            move |event: NetEvent| {
                let input = match event {
                    NetEvent::PeerConnected { peer, name, .. } => {
                        SyncInput::PeerConnected { peer, name }
                    }
                    NetEvent::PeerDisconnected { peer, reason } => {
                        tracing::info!(%peer, %reason, "peer disconnected");
                        SyncInput::PeerDisconnected { peer }
                    }
                    NetEvent::Frame { peer, body } => SyncInput::Frame { peer, body },
                    NetEvent::ObjectFetched { peer, object } => {
                        SyncInput::ObjectFetched { peer, object }
                    }
                    NetEvent::ObjectFetchFailed {
                        peer,
                        object,
                        reason,
                        not_found,
                    } => SyncInput::ObjectFetchFailed {
                        peer,
                        object,
                        not_found,
                        reason,
                    },
                    NetEvent::ListenFailed { error } => {
                        tracing::error!(%error, "network listener stopped");
                        if let Ok(mut slot) = listen_error.lock() {
                            *slot = Some(error);
                        }
                        return;
                    }
                };
                let _ = tx.send(input);
            }
        };

        let net = relay_net::start(
            NetConfig {
                identity,
                device_name: engine.device().name.clone(),
                listen: opts.listen,
                peers: peers
                    .iter()
                    .map(|p| PeerConfig {
                        id: p.id,
                        name: p.name.clone(),
                        addresses: p.addresses.clone(),
                    })
                    .collect(),
                store_root: engine.store().root().to_path_buf(),
            },
            Box::new(sink),
        )
        .map_err(|err| {
            anyhow::Error::from(err).context(format!("starting the network on {}", opts.listen))
        })?;

        on_event(&DaemonEvent::Started {
            device_name: engine.device().name.clone(),
            device_id: engine.device().id.to_string(),
            listen: net.local_addr(),
        });

        let mut watch = opts.watch;
        watch.reload_on_external_change = true;

        let run_stop = AtomicBool::new(false);
        let exit = thread::scope(|scope| {
            scope.spawn(|| {
                while !run_stop.load(Ordering::Relaxed) {
                    if stop.load(Ordering::Relaxed)
                        || listen_error.lock().ok().is_some_and(|g| g.is_some())
                    {
                        run_stop.store(true, Ordering::SeqCst);
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            });
            let result = engine.run(
                watch,
                rx,
                |output| {
                    net.send(match output {
                        SyncOutput::Send { peer, body } => NetCommand::Send { peer, body },
                        SyncOutput::FetchObject { peer, object } => {
                            NetCommand::FetchObject { peer, object }
                        }
                    });
                },
                &run_stop,
                &mut |event| on_event(&DaemonEvent::Watch(event.clone())),
            );
            run_stop.store(true, Ordering::SeqCst);
            result
        })?;

        net.shutdown();
        drop(engine);

        if let Some(error) = listen_error.lock().ok().and_then(|mut g| g.take()) {
            return Err(anyhow::anyhow!("network listener stopped: {error}"));
        }

        match exit {
            RunExit::Stopped => return Ok(()),
            RunExit::ExternalChange => {
                on_event(&DaemonEvent::Reloading);
                tracing::info!("configuration changed; reloading");
                settle(home, stop);
            }
        }
    }
    Ok(())
}

fn open_writable(home: &Path, stop: &AtomicBool) -> Result<Engine> {
    let deadline = Instant::now() + OPEN_RETRY;
    loop {
        match Engine::open(home) {
            Ok(engine) => return Ok(engine),
            Err(EngineError::Busy { .. })
                if Instant::now() < deadline && !stop.load(Ordering::Relaxed) =>
            {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn settle(home: &Path, stop: &AtomicBool) {
    let cap = Instant::now() + SETTLE_CAP;
    let mut version = read_data_version(home);
    loop {
        let remaining = cap.saturating_duration_since(Instant::now());
        if remaining.is_zero() || stop.load(Ordering::Relaxed) {
            return;
        }
        let slice = SETTLE_SLICE.min(remaining);
        let until = Instant::now() + slice;
        while Instant::now() < until {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let now = read_data_version(home);
        if now == version || Instant::now() >= cap {
            return;
        }
        version = now;
    }
}

fn read_data_version(home: &Path) -> Option<u32> {
    Engine::open_read_only(home)
        .ok()
        .and_then(|engine| engine.data_version().ok())
}
