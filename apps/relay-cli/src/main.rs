use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use relay_core::{DeviceId, EntryContent, EntryRecord, LogicalPath, Sequence, VersionVector};
use relay_engine::{
    Engine, EngineError, ScanOptions, ScanReport, SyncInput, SyncOutput, WatchEvent, WatchOptions,
    default_home,
};
use relay_net::{NetCommand, NetConfig, NetEvent, PeerConfig};

const DEFAULT_LISTEN: &str = "0.0.0.0:47321";

const DEV_EXCLUDES: &[&str] = &[
    "**/node_modules/**",
    "**/target/**",
    "**/dist/**",
    "**/build/**",
    "**/.venv/**",
    "**/__pycache__/**",
];

#[derive(Parser, Debug)]
#[command(name = "relay", version, about = "Local-first multi-device file sync")]
struct Cli {
    /// Relay home directory (database, object store, logs)
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    /// Print machine-readable JSON where applicable
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Initialize a new Relay home and local device
    Init {
        #[arg(long)]
        name: Option<String>,
    },
    /// Print this device's id and name
    Id,
    /// Show device, mount and peer status
    Status,
    /// Peer pairing
    Peer {
        #[command(subcommand)]
        cmd: PeerCmd,
    },
    /// Share a space with a peer
    Share { space: String, peer: String },
    /// Stop sharing a space with a peer
    Unshare { space: String, peer: String },
    /// List live conflict copies
    Conflicts {
        #[arg(long)]
        space: Option<String>,
    },
    /// Space commands
    Space {
        #[command(subcommand)]
        cmd: SpaceCmd,
    },
    /// Mount commands
    Mount {
        #[command(subcommand)]
        cmd: MountCmd,
    },
    /// Scan one mount, every mount in a space, or all mounts
    Scan {
        target: Option<String>,
        #[arg(long)]
        allow_mass_delete: bool,
        /// Plan the scan and print the report without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// List indexed entries
    Ls {
        target: String,
        #[arg(long)]
        deleted: bool,
        #[arg(long)]
        prefix: Option<String>,
    },
    /// Show version history for a path
    History { target: String },
    /// Restore a historical file version onto disk
    Restore {
        target: String,
        #[arg(long)]
        sequence: u64,
    },
    /// Verify every live object in the store
    Verify,
    /// Collect unreferenced objects
    Gc {
        #[arg(long, default_value_t = 3600)]
        grace_secs: u64,
    },
    /// Watch mounts and sync with peers until Ctrl-C. Event times are UTC `HH:MM:SS`.
    Run {
        /// UDP address to accept peer connections on
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: SocketAddr,
        #[arg(long, default_value_t = 200)]
        debounce_ms: u64,
        #[arg(long, default_value_t = 600)]
        full_scan_secs: u64,
        /// Periodic full scans only; do not attach a native filesystem watcher
        #[arg(long)]
        poll: bool,
        #[arg(long)]
        verbose: bool,
    },
    /// Watch mounts and keep the index live without syncing. Event times are UTC `HH:MM:SS`.
    Watch {
        #[arg(long, default_value_t = 200)]
        debounce_ms: u64,
        #[arg(long, default_value_t = 600)]
        full_scan_secs: u64,
        /// Periodic full scans only; do not attach a native filesystem watcher
        #[arg(long)]
        poll: bool,
        #[arg(long)]
        verbose: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SpaceCmd {
    Create {
        name: String,
    },
    List,
    /// Spaces a peer has offered (not yet joined)
    Offers,
    /// Join an offered space and share it back
    Join {
        name_or_id: String,
        #[arg(long = "from")]
        from: String,
    },
}

#[derive(Subcommand, Debug)]
enum PeerCmd {
    Add {
        name: String,
        device_id: String,
        #[arg(long = "addr")]
        addresses: Vec<String>,
    },
    List,
    Remove {
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum MountCmd {
    Add {
        space: String,
        mount: String,
        path: PathBuf,
        #[arg(long = "include")]
        includes: Vec<String>,
        #[arg(long = "exclude")]
        excludes: Vec<String>,
        #[arg(long)]
        dev_excludes: bool,
    },
    List {
        space: Option<String>,
    },
}

fn main() -> ExitCode {
    init_logging();
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            if let Some(engine) = err.downcast_ref::<EngineError>() {
                print_engine_error(engine);
                return ExitCode::from(engine_exit_code(engine));
            }
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}

fn init_logging() {
    let filter = std::env::var("RELAY_LOG").unwrap_or_else(|_| "warn".to_owned());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .try_init();
}

fn run(cli: Cli) -> Result<ExitCode> {
    let home = cli.home.unwrap_or_else(default_home);
    let json = cli.json;
    match cli.command {
        Command::Init { name } => cmd_init(&home, name, json).map(|()| ExitCode::SUCCESS),
        Command::Id => {
            let engine = Engine::open_read_only(&home)?;
            cmd_id(&engine, json).map(|()| ExitCode::SUCCESS)
        }
        Command::Status => {
            let engine = Engine::open_read_only(&home)?;
            cmd_status(&engine, json).map(|()| ExitCode::SUCCESS)
        }
        Command::Peer { cmd } => cmd_peer(&home, cmd, json),
        Command::Share { space, peer } => {
            let mut engine = Engine::open(&home)?;
            engine.share(&space, &peer)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"shared": true, "space": space, "peer": peer})
                    )?
                );
            } else {
                println!("shared {space} with {peer}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Unshare { space, peer } => {
            let mut engine = Engine::open(&home)?;
            engine.unshare(&space, &peer)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"unshared": true, "space": space, "peer": peer})
                    )?
                );
            } else {
                println!("unshared {space} from {peer}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Conflicts { space } => {
            let engine = Engine::open_read_only(&home)?;
            cmd_conflicts(&engine, space.as_deref(), json).map(|()| ExitCode::SUCCESS)
        }
        Command::Space { cmd } => match cmd {
            SpaceCmd::Create { name } => {
                let mut engine = Engine::open(&home)?;
                let space = engine.create_space(&name)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&space)?);
                } else {
                    println!("created space {}", space.name);
                }
                Ok(ExitCode::SUCCESS)
            }
            SpaceCmd::List => {
                let engine = Engine::open_read_only(&home)?;
                let spaces = engine.spaces()?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&spaces)?);
                } else {
                    for space in spaces {
                        println!("{}", space.name);
                    }
                }
                Ok(ExitCode::SUCCESS)
            }
            SpaceCmd::Offers => {
                let engine = Engine::open_read_only(&home)?;
                let offers = engine.offers()?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&offers)?);
                } else if offers.is_empty() {
                    println!("no offers");
                } else {
                    for offer in offers {
                        let mounts: Vec<_> = offer.mounts.iter().map(|m| m.name.as_str()).collect();
                        println!(
                            "{} from {} ({}): {}",
                            offer.name,
                            offer.peer,
                            offer.space_id,
                            mounts.join(", ")
                        );
                        println!(
                            "  hint: relay space join {} --from {}",
                            offer.name, offer.peer
                        );
                    }
                }
                Ok(ExitCode::SUCCESS)
            }
            SpaceCmd::Join { name_or_id, from } => {
                let mut engine = Engine::open(&home)?;
                let space = engine.join_space(&name_or_id, &from)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&space)?);
                } else {
                    println!(
                        "joined space {} ({}); attach mounts with `relay mount add`",
                        space.name, space.id
                    );
                }
                Ok(ExitCode::SUCCESS)
            }
        },
        Command::Mount { cmd } => match cmd {
            MountCmd::Add {
                space,
                mount,
                path,
                mut excludes,
                includes,
                dev_excludes,
            } => {
                if dev_excludes {
                    excludes.extend(DEV_EXCLUDES.iter().map(|s| (*s).to_owned()));
                }
                let mut engine = Engine::open(&home)?;
                let config = engine.add_mount(&space, &mount, &path, &includes, &excludes)?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&mount_json(&space, &config))?
                    );
                } else {
                    let shown = config
                        .local_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    println!("added mount {space}/{mount} at {shown}");
                }
                Ok(ExitCode::SUCCESS)
            }
            MountCmd::List { space } => {
                let engine = Engine::open_read_only(&home)?;
                let mounts = engine.mounts(space.as_deref())?;
                if json {
                    let rows: Vec<_> = mounts
                        .iter()
                        .map(|(sp, cfg)| mount_json(&sp.name, cfg))
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    let rows: Vec<Vec<String>> = mounts
                        .iter()
                        .map(|(sp, cfg)| {
                            vec![
                                sp.name.clone(),
                                cfg.mount.name.clone(),
                                cfg.local_path
                                    .as_ref()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| "-".to_owned()),
                                format_rules("include", &cfg.includes),
                                format_rules("exclude", &cfg.excludes),
                            ]
                        })
                        .collect();
                    print_table(&rows);
                }
                Ok(ExitCode::SUCCESS)
            }
        },
        Command::Scan {
            target,
            allow_mass_delete,
            dry_run,
        } => {
            let mut engine = Engine::open(&home)?;
            cmd_scan(
                &mut engine,
                target.as_deref(),
                allow_mass_delete,
                dry_run,
                json,
            )
            .map(|()| ExitCode::SUCCESS)
        }
        Command::Ls {
            target,
            deleted,
            prefix,
        } => {
            let engine = Engine::open_read_only(&home)?;
            cmd_ls(&engine, &target, deleted, prefix.as_deref(), json).map(|()| ExitCode::SUCCESS)
        }
        Command::History { target } => {
            let engine = Engine::open_read_only(&home)?;
            cmd_history(&engine, &target, json).map(|()| ExitCode::SUCCESS)
        }
        Command::Restore { target, sequence } => {
            let mut engine = Engine::open(&home)?;
            cmd_restore(&mut engine, &target, Sequence(sequence), json).map(|()| ExitCode::SUCCESS)
        }
        Command::Verify => {
            let engine = Engine::open_read_only(&home)?;
            let report = engine.verify_objects()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "checked {} objects, {} missing, {} corrupt",
                    report.checked,
                    report.missing.len(),
                    report.corrupt.len()
                );
                for id in &report.missing {
                    println!("missing  {}", id.short());
                }
                for id in &report.corrupt {
                    println!("corrupt  {}", id.short());
                }
            }
            if report.missing.is_empty() && report.corrupt.is_empty() {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(3))
            }
        }
        Command::Gc { grace_secs } => {
            let mut engine = Engine::open(&home)?;
            let report = engine.gc(Duration::from_secs(grace_secs))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "removed {} objects ({} bytes), kept {}, cleaned {} tmp files",
                    report.removed, report.bytes_freed, report.kept, report.tmp_cleaned
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Watch {
            debounce_ms,
            full_scan_secs,
            poll,
            verbose,
        } => cmd_watch(&home, debounce_ms, full_scan_secs, poll, verbose, json),
        Command::Run {
            listen,
            debounce_ms,
            full_scan_secs,
            poll,
            verbose,
        } => {
            let opts = WatchOptions {
                debounce: Duration::from_millis(debounce_ms),
                full_scan_interval: Duration::from_secs(full_scan_secs),
                use_watcher: !poll,
                ..WatchOptions::default()
            };
            cmd_run(&home, listen, opts, verbose, json)
        }
    }
}

fn cmd_watch(
    home: &Path,
    debounce_ms: u64,
    full_scan_secs: u64,
    poll: bool,
    verbose: bool,
    json: bool,
) -> Result<ExitCode> {
    let mut engine = Engine::open(home)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
    })
    .context("installing Ctrl-C handler")?;

    let opts = WatchOptions {
        debounce: Duration::from_millis(debounce_ms),
        full_scan_interval: Duration::from_secs(full_scan_secs),
        use_watcher: !poll,
        ..WatchOptions::default()
    };
    let names = HashMap::new();
    engine.watch(opts, &stop, &mut |event| {
        print_watch_event(event, json, verbose, &names);
    })?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_run(
    home: &Path,
    listen: SocketAddr,
    opts: WatchOptions,
    verbose: bool,
    json: bool,
) -> Result<ExitCode> {
    let mut engine = Engine::open(home)?;
    let identity = Arc::new(engine.load_identity()?);
    let peers = engine.peers()?;
    let names: HashMap<String, String> = peers
        .iter()
        .map(|p| (p.id.to_string(), p.name.clone()))
        .collect();
    if peers.is_empty() && !json {
        println!("no peers yet; add one with `relay peer add <name> <device-id> --addr host:port`");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
    })
    .context("installing Ctrl-C handler")?;

    let (tx, rx) = mpsc::channel::<SyncInput>();
    let listen_failed = Arc::clone(&stop);
    let sink = move |event: NetEvent| {
        let input = match event {
            NetEvent::PeerConnected { peer, name, .. } => SyncInput::PeerConnected { peer, name },
            NetEvent::PeerDisconnected { peer, reason } => {
                tracing::info!(%peer, %reason, "peer disconnected");
                SyncInput::PeerDisconnected { peer }
            }
            NetEvent::Frame { peer, body } => SyncInput::Frame { peer, body },
            NetEvent::ObjectFetched { peer, object } => SyncInput::ObjectFetched { peer, object },
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
                eprintln!("error: network listener stopped: {error}");
                listen_failed.store(true, Ordering::SeqCst);
                return;
            }
        };
        let _ = tx.send(input);
    };
    let net = relay_net::start(
        NetConfig {
            identity,
            device_name: engine.device().name.clone(),
            listen,
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
    .with_context(|| format!("starting the network on {listen}"))?;
    if !json {
        println!(
            "{} listening on {} as {} ({})",
            utc_hms(),
            net.local_addr(),
            engine.device().name,
            engine.device().id
        );
    }

    let result = engine.run(
        opts,
        rx,
        |output| {
            net.send(match output {
                SyncOutput::Send { peer, body } => NetCommand::Send { peer, body },
                SyncOutput::FetchObject { peer, object } => {
                    NetCommand::FetchObject { peer, object }
                }
            })
        },
        &stop,
        &mut |event| print_watch_event(event, json, verbose, &names),
    );
    net.shutdown();
    result?;
    Ok(ExitCode::SUCCESS)
}

fn peer_label<'a>(names: &'a HashMap<String, String>, peer: &'a str) -> &'a str {
    names.get(peer).map(String::as_str).unwrap_or(peer)
}

fn print_watch_event(
    event: &WatchEvent,
    json: bool,
    verbose: bool,
    names: &HashMap<String, String>,
) {
    if json {
        match serde_json::to_string(event) {
            Ok(line) => println!("{line}"),
            Err(err) => eprintln!("error: failed to serialize event: {err}"),
        }
        return;
    }
    match event {
        WatchEvent::Started { mounts } => {
            if mounts.is_empty() {
                println!("{} watching (no local mounts)", utc_hms());
            } else {
                println!("{} watching {}", utc_hms(), mounts.join(", "));
            }
        }
        WatchEvent::Scanned {
            space,
            mount,
            full,
            paths,
            report,
        } => {
            if *full && !report.has_changes() && !verbose {
                return;
            }
            if *full && !report.has_changes() {
                println!("{} {space}/{mount}: full scan, 0 changes", utc_hms());
                return;
            }
            let summary = watch_change_summary(report);
            if *full {
                println!("{} {space}/{mount}: full scan, {summary}", utc_hms());
            } else {
                println!("{} {space}/{mount}: {summary} ({paths} paths)", utc_hms());
            }
            for warning in &report.warnings {
                println!("  warning: {warning}");
            }
        }
        WatchEvent::ScanFailed {
            space,
            mount,
            error,
        } => {
            eprintln!("error: {space}/{mount}: {error}");
            print_watch_error_hints(error);
        }
        WatchEvent::WatcherUnavailable {
            space,
            mount,
            error,
        } => {
            eprintln!("error: {space}/{mount}: {error}");
        }
        WatchEvent::Stopped => println!("stopped"),
        WatchEvent::PeerConnected { peer, name } => {
            let label = names.get(peer).unwrap_or(name);
            println!("{} connected to {label}", utc_hms());
        }
        WatchEvent::PeerDisconnected { peer } => {
            println!(
                "{} disconnected from {}",
                utc_hms(),
                peer_label(names, peer)
            );
        }
        WatchEvent::OffersReceived { peer, spaces } => {
            let label = peer_label(names, peer);
            println!("{} {label} offers: {}", utc_hms(), spaces.join(", "));
            for name in spaces {
                println!("  to accept: stop relay, then `relay space join {name} --from {label}`");
            }
        }
        WatchEvent::RemoteApplied {
            peer,
            space,
            mount,
            written,
            deleted,
            conflicts,
            skipped,
        } => {
            if *written + *deleted + *conflicts + *skipped == 0 && !verbose {
                return;
            }
            let peer = peer_label(names, peer);
            println!(
                "{} {space}/{mount} from {peer}: {written} written, {deleted} deleted, {conflicts} conflicts, {skipped} skipped",
                utc_hms()
            );
        }
        WatchEvent::SentChanges {
            peer,
            space,
            entries,
        } => {
            let peer = peer_label(names, peer);
            println!("{} sent {entries} changes of {space} to {peer}", utc_hms());
        }
        WatchEvent::SyncWarning { peer, path, reason } => {
            eprintln!("warning: {} {path}: {reason}", peer_label(names, peer));
        }
    }
}

fn watch_change_summary(report: &ScanReport) -> String {
    let mut parts = vec![
        format!("{} created", report.created),
        format!("{} modified", report.modified),
    ];
    if report.deleted > 0 {
        parts.push(format!("{} deleted", report.deleted));
    }
    parts.join(", ")
}

fn print_watch_error_hints(error: &str) {
    if error.contains("refusing to delete") {
        eprintln!("hint: re-run with --allow-mass-delete if this was intentional");
    } else if error.contains("marker")
        || error.contains("mount root")
        || error.contains("not a directory")
    {
        eprintln!("hint: is the drive mounted / was the folder moved?");
    } else if error.contains("another relay process") {
        eprintln!("hint: stop `relay watch` or wait for the other command to finish");
    }
}

fn utc_hms() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let tod = (secs % 86_400) as u32;
    format!("{:02}:{:02}:{:02}", tod / 3600, (tod % 3600) / 60, tod % 60)
}

fn cmd_init(home: &Path, name: Option<String>, json: bool) -> Result<()> {
    let name = name.unwrap_or_else(default_device_name);
    let engine = Engine::init(home, &name)?;
    let device = engine.device();
    if json {
        println!("{}", serde_json::to_string_pretty(device)?);
    } else {
        println!("initialized device {} ({})", device.name, device.id);
    }
    Ok(())
}

fn cmd_id(engine: &Engine, json: bool) -> Result<()> {
    let device = engine.device();
    if json {
        println!("{}", serde_json::to_string_pretty(device)?);
    } else {
        println!("{} {}", device.id, device.name);
    }
    Ok(())
}

fn cmd_peer(home: &Path, cmd: PeerCmd, json: bool) -> Result<ExitCode> {
    match cmd {
        PeerCmd::Add {
            name,
            device_id,
            addresses,
        } => {
            let id: DeviceId = device_id.parse()?;
            let mut engine = Engine::open(home)?;
            let peer = engine.add_peer(&name, id, &addresses)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&peer)?);
            } else {
                println!("added peer {} ({})", peer.name, peer.id);
            }
        }
        PeerCmd::List => {
            let engine = Engine::open_read_only(home)?;
            let peers = engine.peers()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&peers)?);
            } else if peers.is_empty() {
                println!("no peers");
            } else {
                for peer in peers {
                    let addrs = if peer.addresses.is_empty() {
                        "-".to_owned()
                    } else {
                        peer.addresses.join(", ")
                    };
                    println!("{}  {}  {addrs}", peer.name, peer.id);
                }
            }
        }
        PeerCmd::Remove { name } => {
            let mut engine = Engine::open(home)?;
            engine.remove_peer(&name)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"removed": name}))?
                );
            } else {
                println!("removed peer {name}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_conflicts(engine: &Engine, space: Option<&str>, json: bool) -> Result<()> {
    let entries = engine.conflicts(space)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!("no conflicts");
        return Ok(());
    }
    for entry in entries {
        println!("{}", entry.key.path);
    }
    Ok(())
}

fn cmd_status(engine: &Engine, json: bool) -> Result<()> {
    let status = engine.status()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    println!(
        "device {} ({})",
        status.device.name,
        status.device.id.short()
    );
    if status.mounts.is_empty() {
        println!("no mounts");
    } else {
        let rows: Vec<Vec<String>> = status
            .mounts
            .iter()
            .map(|m| {
                vec![
                    format!("{}/{}", m.space, m.mount),
                    m.path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "-".to_owned()),
                    m.marker_state.clone(),
                    format!("{} live", m.live_entries),
                    format!("{} tombstones", m.tombstones),
                    match m.last_scan_ms {
                        Some(ms) => format_utc_ms(ms),
                        None => "never".to_owned(),
                    },
                    m.last_error.clone().unwrap_or_else(|| "-".to_owned()),
                ]
            })
            .collect();
        print_table(&rows);
    }
    println!(
        "objects {}  last sequence {}",
        status.object_count, status.last_sequence
    );
    if status.peers.is_empty() {
        println!("no peers");
    } else {
        println!("peers");
        for peer in &status.peers {
            let addrs = if peer.addresses.is_empty() {
                "-".to_owned()
            } else {
                peer.addresses.join(", ")
            };
            println!("  {}  {}  {addrs}", peer.name, peer.id);
            for sp in &peer.spaces {
                println!(
                    "    {}: received {} acked {} local {} last {}",
                    sp.space,
                    sp.received_seq,
                    sp.acked_seq,
                    sp.our_latest_seq,
                    match sp.last_sync_ms {
                        Some(ms) => format_utc_ms(ms),
                        None => "never".to_owned(),
                    }
                );
            }
        }
    }
    Ok(())
}

fn cmd_scan(
    engine: &mut Engine,
    target: Option<&str>,
    allow_mass_delete: bool,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let opts = ScanOptions {
        allow_mass_delete,
        dry_run,
    };
    let mut rows = Vec::new();
    let mut first_error: Option<EngineError> = None;

    match target {
        None => {
            for (space, mount, result) in engine.scan_all(opts)? {
                push_scan_result(
                    &mut rows,
                    &space.name,
                    &mount.name,
                    result,
                    &mut first_error,
                );
            }
        }
        Some(raw) => {
            let parsed = parse_target(raw)?;
            match parsed.mount {
                None => {
                    let mounts = engine.mounts(Some(&parsed.space))?;
                    if mounts.is_empty() {
                        bail!("no mounts in space {}", parsed.space);
                    }
                    for (space, config) in mounts {
                        let result = engine.scan(&space.name, &config.mount.name, opts);
                        push_scan_result(
                            &mut rows,
                            &space.name,
                            &config.mount.name,
                            result,
                            &mut first_error,
                        );
                    }
                }
                Some(mount) => {
                    if parsed.path.is_some() {
                        bail!("scan target is SPACE or SPACE/MOUNT");
                    }
                    let result = engine.scan(&parsed.space, &mount, opts);
                    push_scan_result(&mut rows, &parsed.space, &mount, result, &mut first_error);
                }
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        for row in &rows {
            print_scan_human(row, dry_run);
        }
    }

    if let Some(err) = first_error {
        return Err(err.into());
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct ScanRow {
    space: String,
    mount: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<ScanReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn push_scan_result(
    rows: &mut Vec<ScanRow>,
    space: &str,
    mount: &str,
    result: Result<ScanReport, EngineError>,
    first_error: &mut Option<EngineError>,
) {
    match result {
        Ok(report) => rows.push(ScanRow {
            space: space.to_owned(),
            mount: mount.to_owned(),
            report: Some(report),
            error: None,
        }),
        Err(err) => {
            rows.push(ScanRow {
                space: space.to_owned(),
                mount: mount.to_owned(),
                report: None,
                error: Some(err.to_string()),
            });
            let prefer_mass = matches!(err, EngineError::MassDeleteRefused { .. })
                && !matches!(first_error, Some(EngineError::MassDeleteRefused { .. }));
            if first_error.is_none() || prefer_mass {
                *first_error = Some(err);
            }
        }
    }
}

fn print_scan_human(row: &ScanRow, dry_run: bool) {
    let prefix = if dry_run { "(dry run) " } else { "" };
    match (&row.report, &row.error) {
        (Some(report), _) => {
            println!(
                "{prefix}{}/{}: {} created, {} modified, {} deleted, {} unchanged",
                row.space,
                row.mount,
                report.created,
                report.modified,
                report.deleted,
                report.unchanged
            );
            if report.stat_only > 0 {
                println!("  stat-only: {}", report.stat_only);
            }
            if report.protected > 0 {
                println!("  protected: {} entries left untouched", report.protected);
            }
            for path in &report.deselected {
                println!("  deselected: {path}");
            }
            for path in &report.unstable {
                println!("  unstable: {path}");
            }
            for warning in &report.warnings {
                println!("  warning: {warning}");
            }
        }
        (_, Some(error)) => {
            println!("{prefix}{}/{}: error: {error}", row.space, row.mount);
        }
        _ => {}
    }
}

fn cmd_ls(
    engine: &Engine,
    target: &str,
    deleted: bool,
    prefix: Option<&str>,
    json: bool,
) -> Result<()> {
    let parsed = parse_target(target)?;
    let mount = parsed
        .mount
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("ls requires SPACE/MOUNT"))?;
    let prefix = match (prefix, parsed.path.as_ref()) {
        (Some(raw), _) => Some(LogicalPath::new(raw)?),
        (None, Some(path)) => Some(path.clone()),
        (None, None) => None,
    };
    let mut entries = engine.entries(&parsed.space, mount, deleted)?;
    if let Some(prefix) = &prefix {
        entries.retain(|e| e.key.path.starts_with(prefix));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    let rows: Vec<Vec<String>> = entries.iter().map(ls_row).collect();
    print_table(&rows);
    Ok(())
}

fn ls_row(record: &EntryRecord) -> Vec<String> {
    let kind = kind_label(&record.content);
    let (size, object) = match &record.content {
        EntryContent::File { object, size, .. } => (human_bytes(*size), object.short()),
        _ => ("-".to_owned(), "-".to_owned()),
    };
    vec![
        kind.to_owned(),
        size,
        object,
        short_vector(&record.vector),
        record.key.path.to_string(),
    ]
}

fn cmd_history(engine: &Engine, target: &str, json: bool) -> Result<()> {
    let parsed = parse_target(target)?;
    let mount = parsed
        .mount
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("history requires SPACE/MOUNT/PATH"))?;
    let path = parsed
        .path
        .ok_or_else(|| anyhow::anyhow!("history requires SPACE/MOUNT/PATH"))?;
    let history = engine.history(&parsed.space, mount, &path)?;
    if json {
        let rows: Vec<_> = history.iter().map(history_json).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let rows: Vec<Vec<String>> = history
        .iter()
        .map(|h| {
            let (size, object) = match &h.content {
                EntryContent::File { object, size, .. } => (human_bytes(*size), object.short()),
                _ => ("-".to_owned(), "-".to_owned()),
            };
            vec![
                h.sequence.to_string(),
                format_utc_ms(h.modified_at_unix_ms),
                h.modified_by.short(),
                kind_label(&h.content).to_owned(),
                size,
                object,
            ]
        })
        .collect();
    print_table(&rows);
    Ok(())
}

fn cmd_restore(engine: &mut Engine, target: &str, sequence: Sequence, json: bool) -> Result<()> {
    let parsed = parse_target(target)?;
    let mount = parsed
        .mount
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("restore requires SPACE/MOUNT/PATH"))?;
    let path = parsed
        .path
        .ok_or_else(|| anyhow::anyhow!("restore requires SPACE/MOUNT/PATH"))?;
    let record = engine.restore(&parsed.space, mount, &path, sequence)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&record)?);
    } else {
        println!(
            "restored {}/{}/{} to sequence {} (now sequence {})",
            parsed.space, mount, path, sequence, record.sequence
        );
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct MountJson {
    space: String,
    mount: String,
    path: Option<PathBuf>,
    includes: Vec<String>,
    excludes: Vec<String>,
}

fn mount_json(space: &str, config: &relay_engine::MountConfig) -> MountJson {
    MountJson {
        space: space.to_owned(),
        mount: config.mount.name.clone(),
        path: config.local_path.clone(),
        includes: config.includes.clone(),
        excludes: config.excludes.clone(),
    }
}

#[derive(serde::Serialize)]
struct HistoryJson {
    sequence: u64,
    modified_at: String,
    device: String,
    kind: String,
    size: Option<u64>,
    object: Option<String>,
}

fn history_json(record: &relay_engine::HistoryRecord) -> HistoryJson {
    let (kind, size, object) = match &record.content {
        EntryContent::File { object, size, .. } => {
            ("file".to_owned(), Some(*size), Some(object.to_hex()))
        }
        EntryContent::Directory => ("dir".to_owned(), None, None),
        EntryContent::Symlink { .. } => ("link".to_owned(), None, None),
        EntryContent::Deleted => ("deleted".to_owned(), None, None),
    };
    HistoryJson {
        sequence: record.sequence.0,
        modified_at: format_utc_ms(record.modified_at_unix_ms),
        device: record.modified_by.short(),
        kind,
        size,
        object,
    }
}

struct Target {
    space: String,
    mount: Option<String>,
    path: Option<LogicalPath>,
}

fn parse_target(raw: &str) -> Result<Target> {
    let raw = raw.strip_suffix('/').unwrap_or(raw);
    let mut parts = raw.split('/');
    let space = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("expected SPACE[/MOUNT[/PATH]]"))?
        .to_owned();
    let mount = parts.next().map(ToOwned::to_owned);
    if mount.as_deref().is_some_and(str::is_empty) {
        bail!("empty mount name in {raw:?}");
    }
    let rest: Vec<&str> = parts.collect();
    let path = if rest.is_empty() {
        None
    } else {
        Some(LogicalPath::from_components(rest).context("invalid logical path")?)
    };
    Ok(Target { space, mount, path })
}

fn default_device_name() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if relay_core::validate_name(trimmed).is_ok() {
                return trimmed.to_owned();
            }
        }
    }
    if let Ok(value) = hostname_fallback()
        && relay_core::validate_name(&value).is_ok()
    {
        return value;
    }
    "this-device".to_owned()
}

fn hostname_fallback() -> Result<String> {
    let output = std::process::Command::new("hostname")
        .output()
        .context("hostname")?;
    if !output.status.success() {
        bail!("hostname failed");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn engine_exit_code(err: &EngineError) -> u8 {
    match err {
        EngineError::MassDeleteRefused { .. } => 2,
        _ => 1,
    }
}

fn print_engine_error(err: &EngineError) {
    eprintln!("error: {err}");
    match err {
        EngineError::MassDeleteRefused { .. } => {
            eprintln!("hint: re-run with --allow-mass-delete if this was intentional");
        }
        EngineError::Fs(relay_engine::FsError::MarkerMissing(_))
        | EngineError::Fs(relay_engine::FsError::MountRootMissing(_)) => {
            eprintln!("hint: is the drive mounted / was the folder moved?");
        }
        EngineError::DestinationChanged(_) => {
            eprintln!("hint: scan first so Relay sees the on-disk change");
        }
        EngineError::Busy { .. } => {
            eprintln!("hint: stop `relay watch` or wait for the other command to finish");
        }
        _ => {}
    }
}

fn format_rules(kind: &str, rules: &[String]) -> String {
    if rules.is_empty() {
        format!("{kind}: -")
    } else {
        format!("{kind}: {}", rules.join(", "))
    }
}

fn kind_label(content: &EntryContent) -> &'static str {
    match content {
        EntryContent::File { .. } => "file",
        EntryContent::Directory => "dir",
        EntryContent::Symlink { .. } => "link",
        EntryContent::Deleted => "deleted",
    }
}

fn short_vector(vector: &VersionVector) -> String {
    let parts: Vec<String> = vector
        .iter()
        .map(|(device, counter)| format!("{}:{counter}", device.short()))
        .collect();
    if parts.is_empty() {
        "-".to_owned()
    } else {
        parts.join(",")
    }
}

fn human_bytes(n: u64) -> String {
    const K: u64 = 1024;
    if n < K {
        format!("{n}B")
    } else if n < K * K {
        format!("{:.1}K", n as f64 / K as f64)
    } else if n < K * K * K {
        format!("{:.1}M", n as f64 / (K * K) as f64)
    } else {
        format!("{:.1}G", n as f64 / (K * K * K) as f64)
    }
}

fn print_table(rows: &[Vec<String>]) {
    if rows.is_empty() {
        return;
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    for row in rows {
        let mut line = String::new();
        for (i, width) in widths.iter().enumerate() {
            let cell = row.get(i).map(String::as_str).unwrap_or("");
            if i > 0 {
                line.push_str("  ");
            }
            if i + 1 == cols {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:<w$}", w = *width));
            }
        }
        println!("{line}");
    }
}

/// ISO-8601 UTC from Unix milliseconds. Civil conversion is Howard Hinnant's
/// days-from-civil inverse (no extra date crate).
fn format_utc_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000) as u32;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    let hour = tod / 3600;
    let min = (tod % 3600) / 60;
    let sec = tod % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z")
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_trims_one_trailing_slash() {
        let mount = parse_target("Personal/code/").unwrap();
        assert_eq!(mount.space, "Personal");
        assert_eq!(mount.mount.as_deref(), Some("code"));
        assert!(mount.path.is_none());

        let dir = parse_target("Personal/code/foo/").unwrap();
        assert_eq!(dir.space, "Personal");
        assert_eq!(dir.mount.as_deref(), Some("code"));
        assert_eq!(dir.path.as_ref().map(LogicalPath::as_str), Some("foo"));

        let file = parse_target("Personal/code/foo/bar.txt/").unwrap();
        assert_eq!(
            file.path.as_ref().map(LogicalPath::as_str),
            Some("foo/bar.txt")
        );
    }
}
