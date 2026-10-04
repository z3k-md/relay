//! In-process Relay sync runner. The CLI and desktop app both host this.
//!
//! The engine stays free of Tokio and `relay-net` (D19). This crate maps
//! `NetEvent` / `NetCommand` onto `SyncInput` / `SyncOutput` and reopens
//! when another process commits to the database.

mod folder_pair;
mod host;
mod remote;

use std::fs::File;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use host::Host;
use relay_engine::{
    Engine, EngineError, PairedPeer, PeerInfo, RunExit, SyncInput, SyncOutput, WatchEvent,
    WatchOptions,
};
use relay_ipc::{Client, HostState, Server};
use relay_net::{NetCommand, NetConfig, NetEvent, PeerConfig};

const SETTLE_SLICE: Duration = Duration::from_millis(300);
const SETTLE_CAP: Duration = Duration::from_secs(3);
const OPEN_RETRY: Duration = Duration::from_secs(5);
const PAUSE_POLL: Duration = Duration::from_secs(1);
const HOST_LOCK_FILE: &str = "relay.host.lock";

pub use relay_ipc::HostKind;

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub listen: SocketAddr,
    pub watch: WatchOptions,
    pub verbose: bool,
    pub host: HostKind,
    /// Query STUN on the listen socket so peers can hole-punch to the reflexive address.
    pub enable_stun: bool,
    /// Advertise only `127.0.0.1:<port>`. The sim lab sets this so peers dial
    /// loopback instead of another interface on the same machine.
    pub loopback_only: bool,
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
    Paused,
    Resumed,
}

/// Runs until `stop` is set. Reopens itself when another process changes the database.
pub fn run(
    home: &Path,
    opts: DaemonOptions,
    stop: &AtomicBool,
    on_event: &mut dyn FnMut(&DaemonEvent),
) -> Result<()> {
    tracing::debug!(verbose = opts.verbose, listen = %opts.listen, host = %opts.host, "daemon starting");
    let _host_lock = acquire_host_lock(home)?;
    let host = Host::new(home, opts.host, opts.watch.use_watcher);
    let ipc_stop = Arc::new(AtomicBool::new(false));
    let server = Server::bind(home).context("starting the local IPC server")?;
    let ipc_host = Arc::clone(&host);
    let ipc_stop_thread = Arc::clone(&ipc_stop);
    let ipc_thread = thread::Builder::new()
        .name("relay-ipc".to_owned())
        .spawn(move || server.serve(ipc_host, &ipc_stop_thread))
        .context("starting the local IPC thread")?;

    let result = run_loop(home, &opts, stop, on_event, &host);
    ipc_stop.store(true, Ordering::SeqCst);
    let _ = Client::connect(home);
    let _ = ipc_thread.join();
    // Subscriber threads block on their channel; dropping the senders ends them.
    if let Ok(mut subs) = host.subscribers.lock() {
        subs.clear();
    }
    result
}

fn run_loop(
    home: &Path,
    opts: &DaemonOptions,
    stop: &AtomicBool,
    on_event: &mut dyn FnMut(&DaemonEvent),
    host: &Arc<Host>,
) -> Result<()> {
    while !stop.load(Ordering::Relaxed) {
        if is_paused(home) {
            wait_paused(home, stop, on_event, host)?;
            if stop.load(Ordering::Relaxed) {
                break;
            }
            continue;
        }

        let mut engine = open_writable(home, stop)?;
        if engine.paused()? {
            drop(engine);
            wait_paused(home, stop, on_event, host)?;
            if stop.load(Ordering::Relaxed) {
                break;
            }
            continue;
        }

        host.set_state(HostState::Starting, None);
        host.seed_mounts(&engine);
        let identity = Arc::new(engine.load_identity()?);
        let peers: Vec<_> = engine.peers()?.into_iter().filter(|p| !p.revoked).collect();

        let (tx, rx) = mpsc::channel::<SyncInput>();
        host.set_sync_tx(Some(tx.clone()));
        host.set_known_peers(peers.iter().map(peer_config).collect());
        let (transport_relay, transport_serve) = transport_settings(&engine);
        let listen_error = Arc::new(Mutex::new(None::<String>));
        let sink = {
            let listen_error = Arc::clone(&listen_error);
            let tx = tx.clone();
            let host = Arc::clone(host);
            move |event: NetEvent| {
                let input = match event {
                    NetEvent::PeerConnected {
                        peer,
                        name,
                        supports_control,
                        ..
                    } => {
                        host.note_remote_connected(peer, supports_control);
                        SyncInput::PeerConnected { peer, name }
                    }
                    NetEvent::PeerDisconnected { peer, reason } => {
                        tracing::info!(%peer, %reason, "peer disconnected");
                        host.forget_remote(peer);
                        SyncInput::PeerDisconnected { peer }
                    }
                    NetEvent::PeerGrants {
                        peer,
                        may_manage_you,
                    } => {
                        host.note_remote_grants(peer, may_manage_you);
                        return;
                    }
                    NetEvent::Frame { peer, body } => SyncInput::Frame { peer, body },
                    NetEvent::ObjectFetched { peer, object } => {
                        SyncInput::ObjectFetched { peer, object }
                    }
                    NetEvent::ObjectProgress {
                        peer,
                        object,
                        incoming,
                        bytes,
                    } => SyncInput::ObjectProgress {
                        peer,
                        object,
                        incoming,
                        bytes,
                    },
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
                    NetEvent::Paired {
                        peer,
                        name,
                        addresses,
                        ..
                    } => {
                        let terms = host.take_pair_terms();
                        host.finish_pair(Ok((name.clone(), peer.to_string())));
                        SyncInput::AddPeer(PairedPeer {
                            peer,
                            name,
                            addresses,
                            share: terms.share,
                            may_manage: terms.allow_manage,
                        })
                    }
                    NetEvent::PairFailed { reason } => {
                        host.finish_pair(Err(reason));
                        return;
                    }
                    NetEvent::PeerAddresses { peer, addresses } => {
                        SyncInput::PeerAddresses { peer, addresses }
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
                peers: peers.iter().map(peer_config).collect(),
                store_root: engine.store().root().to_path_buf(),
                enable_stun: opts.enable_stun,
                relay: transport_relay.clone(),
                serve_relay: transport_serve,
                control: Some(Arc::new(remote::Browser::new(home, Arc::clone(host)))),
            },
            Box::new(sink),
        )
        .map_err(|err| {
            anyhow::Error::from(err).context(format!("starting the network on {}", opts.listen))
        })?;

        let listen = net.local_addr();
        let mut nat_addrs = if opts.loopback_only {
            vec![format!("127.0.0.1:{}", listen.port())]
        } else {
            relay_net::advertised_addresses(listen.port())
        };
        if !opts.loopback_only
            && let Some(reflexive) = net.reflexive()
        {
            nat_addrs.insert(0, relay_core::format_socket_addr(reflexive));
        }
        let _ = tx.send(SyncInput::NatHint {
            addresses: nat_addrs,
        });
        host.set_net(Some(net.sender()));
        host.set_listen(Some(listen.to_string()));
        host.set_state(HostState::Running, None);
        on_event(&DaemonEvent::Started {
            device_name: engine.device().name.clone(),
            device_id: engine.device().id.to_string(),
            listen,
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
                |output| match output {
                    SyncOutput::Send { peer, body } => {
                        net.send(NetCommand::Send { peer, body });
                    }
                    SyncOutput::FetchObject { peer, object } => {
                        net.send(NetCommand::FetchObject { peer, object });
                    }
                    SyncOutput::SetRelay(addr) => {
                        net.send(NetCommand::SetRelay(addr));
                    }
                    SyncOutput::SetPeers => {
                        if let Ok(engine) = Engine::open_read_only(home)
                            && let Ok(peers) = engine.peers()
                        {
                            let configs: Vec<PeerConfig> = peers
                                .iter()
                                .filter(|p| !p.revoked)
                                .map(peer_config)
                                .collect();
                            host.set_known_peers(configs.clone());
                            net.send(NetCommand::SetPeers(configs));
                        }
                    }
                },
                &run_stop,
                &mut |event| {
                    host.apply_watch(event);
                    on_event(&DaemonEvent::Watch(event.clone()));
                },
            );
            run_stop.store(true, Ordering::SeqCst);
            result
        })?;

        host.set_sync_tx(None);
        host.set_net(None);
        net.shutdown();
        drop(engine);

        if let Some(error) = listen_error.lock().ok().and_then(|mut g| g.take()) {
            host.set_state(HostState::Error, Some(error.clone()));
            return Err(anyhow::anyhow!("network listener stopped: {error}"));
        }

        match exit {
            RunExit::Stopped => return Ok(()),
            RunExit::ExternalChange => {
                if is_paused(home) {
                    continue;
                }
                host.set_state(HostState::Reloading, None);
                host.push_activity(relay_ipc::ActivityItem {
                    at_ms: host::now_ms(),
                    kind: "reload".into(),
                    summary: "reloading after a database change".into(),
                    detail: None,
                });
                on_event(&DaemonEvent::Reloading);
                tracing::info!("configuration changed; reloading");
                settle(home, stop);
            }
        }
    }
    Ok(())
}

fn wait_paused(
    home: &Path,
    stop: &AtomicBool,
    on_event: &mut dyn FnMut(&DaemonEvent),
    host: &Host,
) -> Result<()> {
    host.set_sync_tx(None);
    host.set_listen(None);
    if let Ok(mut peers) = host.peers.lock() {
        peers.clear();
    }
    host.set_state(HostState::Paused, None);
    host.push_activity(relay_ipc::ActivityItem {
        at_ms: host::now_ms(),
        kind: "paused".into(),
        summary: "paused".into(),
        detail: None,
    });
    on_event(&DaemonEvent::Paused);
    tracing::info!("sync paused");

    let mut version = read_data_version(home);
    while !stop.load(Ordering::Relaxed) {
        if host.wake.take() && !is_paused(home) {
            break;
        }
        if !is_paused(home) {
            break;
        }
        host.wake.wait(PAUSE_POLL, stop);
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if host.wake.take() && !is_paused(home) {
            break;
        }
        let now = read_data_version(home);
        if now != version {
            version = now;
            if !is_paused(home) {
                break;
            }
        }
    }
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    host.wake.take();
    host.set_state(HostState::Starting, None);
    host.push_activity(relay_ipc::ActivityItem {
        at_ms: host::now_ms(),
        kind: "resumed".into(),
        summary: "resumed".into(),
        detail: None,
    });
    on_event(&DaemonEvent::Resumed);
    tracing::info!("sync resumed");
    Ok(())
}

fn acquire_host_lock(home: &Path) -> Result<File> {
    std::fs::create_dir_all(home)?;
    let path = home.join(HOST_LOCK_FILE);
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            let detail = running_host_detail(home);
            bail!("another Relay host is already running{detail}");
        }
        Err(std::fs::TryLockError::Error(err)) => Err(err.into()),
    }
}

fn running_host_detail(home: &Path) -> String {
    match Client::connect(home) {
        Ok(Some(mut client)) => match client.hello() {
            Ok(hello) => format!(" ({} pid {})", hello.host, hello.pid),
            Err(_) => String::new(),
        },
        _ => String::new(),
    }
}

fn is_paused(home: &Path) -> bool {
    Engine::open_read_only(home)
        .ok()
        .and_then(|engine| engine.paused().ok())
        .unwrap_or(false)
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

fn transport_settings(engine: &Engine) -> (Option<String>, bool) {
    let relay = match engine.transport_relay() {
        Ok(value) => value.filter(|addr| !addr.is_empty()),
        Err(err) => {
            tracing::warn!(error = %err, "could not read transport_relay");
            None
        }
    };
    let serve = match engine.transport_serve() {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(error = %err, "could not read transport_serve");
            false
        }
    };
    (relay, serve)
}

/// The network's view of a peer: where to dial it and what it may do here.
fn peer_config(peer: &PeerInfo) -> PeerConfig {
    PeerConfig {
        id: peer.id,
        name: peer.name.clone(),
        addresses: peer.addresses.clone(),
        may_manage: peer.may_manage,
    }
}

fn read_data_version(home: &Path) -> Option<u32> {
    Engine::open_read_only(home)
        .ok()
        .and_then(|engine| engine.data_version().ok())
}
