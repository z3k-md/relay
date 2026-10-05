use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use relay_core::remote::{DirEntryKind, RemoteCall, RemoteReply};
use relay_core::{
    ConfigApplied, ConfigChange, DeviceId, EntryContent, EntryRecord, LogicalPath, PairingCode,
    Sequence, VersionVector,
};
use relay_daemon::{DaemonEvent, DaemonOptions, HostKind};
use relay_engine::{
    ConflictClass, ConflictInfo, DeleteHoldDecision, Engine, EngineError, Resolution, ScanOptions,
    ScanReport, TransportStatus, WatchEvent, WatchOptions, default_home, group_git_conflicts,
    resolve_conflict, resolve_git_conflicts,
};
use relay_ipc::{
    ActivityItem, Client, FolderEnd, FolderPairParams, OpenRemoteParams, PairJoinParams,
    PairStartParams, PairStatus, Status as DaemonStatus,
};

mod output;
mod service;

use service::ServiceCmd;

const DEFAULT_LISTEN: &str = "0.0.0.0:47321";
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("RELAY_GIT_REV"), ")");

const DEV_EXCLUDES: &[&str] = &[
    "**/node_modules/**",
    "**/target/**",
    "**/dist/**",
    "**/build/**",
    "**/.venv/**",
    "**/__pycache__/**",
];

#[derive(Parser, Debug)]
#[command(name = "relay", version = VERSION, about = "Realtime file sync across your machines")]
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
    /// Pair with another device using a short code
    Pair {
        /// Code shown on the other device (omit to generate one)
        code: Option<String>,
        /// Spaces to share with the new peer (when generating a code)
        #[arg(long)]
        share: Vec<String>,
        /// Address to dial when joining (Tailscale/VPN; skip on the same LAN)
        #[arg(long)]
        addr: Option<String>,
        /// Let the other device browse this one and set up sync on it
        #[arg(long)]
        allow_manage: bool,
        /// UDP listen address when this command starts a temporary host
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: SocketAddr,
    },
    /// Add or remove peers by device id (advanced)
    Peer {
        #[command(subcommand)]
        cmd: PeerCmd,
    },
    /// Share a space with a peer
    Share { space: String, peer: String },
    /// Stop sharing a space with a peer
    Unshare { space: String, peer: String },
    /// Recovery secret for space keys (write it down; Relay cannot restore it)
    Recovery {
        #[command(subcommand)]
        cmd: RecoveryCmd,
    },
    /// Durable mailbox for offline catch-up
    Replica {
        #[command(subcommand)]
        cmd: ReplicaCmd,
    },
    /// UDP relay peers dial when a direct path does not connect
    Transport {
        #[command(subcommand)]
        cmd: TransportCmd,
    },
    /// Device groups for replication policies
    Group {
        #[command(subcommand)]
        cmd: GroupCmd,
    },
    /// Replication policies (which subtrees sync to which devices)
    Policy {
        #[command(subcommand)]
        cmd: PolicyCmd,
    },
    /// Local materialization rules (what this device stores for a path)
    Materialize {
        #[command(subcommand)]
        cmd: MaterializeCmd,
    },
    /// Sync a folder on one device with a folder on another, set up from
    /// here. A device is this one unless named; remote devices must let this
    /// one manage them
    PairFolder {
        /// Folder to sync, in its device's path format
        source: String,
        /// Folder to sync into (or to create a folder in, with --create)
        dest: String,
        /// Device the source folder is on (default: this one)
        #[arg(long = "from")]
        from: Option<String>,
        /// Device the destination is on (default: this one)
        #[arg(long = "to")]
        to: Option<String>,
        /// Create this folder inside DEST and sync into it
        #[arg(long)]
        create: Option<String>,
        /// Leave out a subfolder of the source (repeatable)
        #[arg(long = "exclude")]
        excludes: Vec<String>,
        /// Leave out files matching a name pattern anywhere, such as `~$*`
        /// (repeatable)
        #[arg(long = "exclude-pattern")]
        exclude_patterns: Vec<String>,
        /// The destination downloads files only when opened
        #[arg(long)]
        online_only: bool,
        /// Show what would happen and change nothing
        #[arg(long)]
        check: bool,
    },
    /// Get a file from a paired device: its folder syncs here online-only if
    /// it does not already, and this file downloads. Prints where it is
    Open {
        peer: String,
        /// The file, in that device's path format
        path: String,
        /// Where folders opened this way go (default ~/Relay)
        #[arg(long)]
        into: Option<PathBuf>,
        /// Copy just this file, read-only, without syncing its folder
        #[arg(long, conflicts_with = "into")]
        read_only: bool,
    },
    /// List folders set up by `relay open`, or remove one
    Opened {
        #[command(subcommand)]
        cmd: Option<OpenedCmd>,
    },
    /// List folders on a paired device that lets this one manage it
    Browse {
        peer: String,
        /// Folder on that device, in its own path format (omit for its roots)
        path: Option<String>,
        /// Include dot-files and hidden files
        #[arg(long)]
        all: bool,
    },
    /// Fetch a demand-mode path onto this device
    Fetch {
        /// SPACE/MOUNT/PATH
        target: String,
    },
    /// Drop demand-mode bytes here without deleting the index entry: one file,
    /// or every downloaded one under SPACE/MOUNT[/FOLDER]
    Evict {
        /// SPACE/MOUNT[/PATH]
        target: String,
    },
    /// List live conflict copies, or resolve them
    Conflicts {
        #[arg(long)]
        space: Option<String>,
        #[command(subcommand)]
        cmd: Option<ConflictsCmd>,
    },
    /// Held mass deletes from a peer
    Deletes {
        #[command(subcommand)]
        cmd: Option<DeletesCmd>,
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
    /// Pause syncing (persisted; a running host stops watching and networking)
    Pause,
    /// Resume syncing after pause
    Resume,
    /// Ask a running host to rescan, or scan directly if none is running
    Rescan {
        target: Option<String>,
        /// Return as soon as the scan is queued
        #[arg(long)]
        no_wait: bool,
    },
    /// Recent host activity
    Activity {
        #[arg(short = 'n', default_value_t = 20)]
        n: usize,
        #[arg(long)]
        follow: bool,
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
        /// Append run output to this file instead of the terminal
        #[arg(long)]
        log_file: Option<PathBuf>,
        /// Which host kind this process reports over IPC (used by `relay service`)
        #[arg(long, hide = true, default_value = "cli")]
        host: HostKind,
    },
    /// Manage the background Relay service (macOS LaunchAgent / Windows Scheduled Task)
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
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
enum TransportCmd {
    /// Store the relay address peers should dial
    Set {
        /// host:port
        addr: String,
        /// Bind 0.0.0.0 and forward for that port
        #[arg(long)]
        serve: bool,
    },
    /// Remove the relay address and stop serving
    Clear,
    /// Show the relay address, whether this device serves, and the mailbox copy
    Status,
}

#[derive(Subcommand, Debug)]
enum ReplicaCmd {
    /// Set the durable mailbox directory
    Set { path: PathBuf },
    /// Clear the mailbox path (peer-only sync)
    Clear,
    /// Show the configured mailbox path and push watermarks
    Status,
    /// Garbage-collect acked mailbox entries and objects
    Gc {
        /// Keep the latest live object per path (mirror mode)
        #[arg(long)]
        mirror: bool,
        #[arg(long, default_value_t = 3600)]
        grace_secs: u64,
    },
}

#[derive(Subcommand, Debug)]
enum ConflictsCmd {
    /// Keep the current file or replace it with the conflict copy
    Resolve {
        /// SPACE/MOUNT/conflict-copy-path
        target: String,
        #[arg(long, value_enum)]
        keep: KeepChoice,
    },
    /// Delete Git metadata conflict copies under a repository (branches stay)
    ResolveGit {
        /// SPACE/MOUNT/path-to-.git
        target: String,
        /// Also delete conflicting refs (branches)
        #[arg(long)]
        branches: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum KeepChoice {
    Current,
    Copy,
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
    /// Forget a space on this device. Detach its mounts first. Files on disk are
    /// not touched.
    Delete {
        name: String,
    },
    /// Rotate the space key. Future mailbox objects use the new generation.
    Rotate {
        name: String,
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
    /// Stop syncing with a peer. Does not delete data already on that device.
    Revoke {
        name: String,
    },
    /// Let a peer browse this device and set up sync on it
    AllowManage {
        name: String,
    },
    /// Stop letting a peer manage this device
    DenyManage {
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum OpenedCmd {
    /// Stop syncing a folder opened from another device. Files stay here
    Remove { space: String },
}

#[derive(Subcommand, Debug)]
enum RecoveryCmd {
    /// Print the recovery secret (creates one on first use)
    Show,
    /// Install space keys wrapped by a recovery secret from the mailbox
    Import { key: String },
}

#[derive(Subcommand, Debug)]
enum GroupCmd {
    Create { name: String },
    Add { name: String, peer: String },
    Remove { name: String, peer: String },
    Delete { name: String },
    List,
}

#[derive(Subcommand, Debug)]
enum PolicyCmd {
    Add {
        space: String,
        name: String,
        #[arg(long = "selector", required = true)]
        selectors: Vec<String>,
        #[arg(long = "peer")]
        peers: Vec<String>,
        #[arg(long = "group")]
        groups: Vec<String>,
    },
    Remove {
        space: String,
        name: String,
    },
    List {
        space: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum MatMode {
    Full,
    Metadata,
    Demand,
    Exclude,
}

impl MatMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Metadata => "metadata",
            Self::Demand => "demand",
            Self::Exclude => "exclude",
        }
    }
}

#[derive(Subcommand, Debug)]
enum MaterializeCmd {
    Add {
        space: String,
        name: String,
        #[arg(long, value_enum)]
        mode: MatMode,
        #[arg(long = "selector", required = true)]
        selectors: Vec<String>,
    },
    Remove {
        space: String,
        name: String,
    },
    List {
        space: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum DeletesCmd {
    /// Apply the peer's deletions on this device
    Apply {
        space: String,
        #[arg(long)]
        mount: Option<String>,
        #[arg(long)]
        peer: Option<String>,
    },
    /// Keep the files here (including any already deleted this catch-up) and send them back
    Restore {
        space: String,
        #[arg(long)]
        mount: Option<String>,
        #[arg(long)]
        peer: Option<String>,
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
    /// Stop syncing a mount on this device. Files on disk are not touched.
    Remove {
        space: String,
        mount: String,
    },
    List {
        space: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(err) = prepare_output(&cli) {
        eprintln!("error: {err:#}");
        return ExitCode::from(1);
    }
    init_logging();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            if output::is_configured() {
                let _ = output::write_line(&format!("error: {err:#}"));
            }
            if let Some(engine) = err.downcast_ref::<EngineError>() {
                print_engine_error(engine);
                return ExitCode::from(engine_exit_code(engine));
            }
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn prepare_output(cli: &Cli) -> Result<()> {
    if let Command::Run {
        log_file: Some(path),
        ..
    } = &cli.command
    {
        output::configure(path, VERSION)?;
        output::install_panic_hook();
    }
    Ok(())
}

fn init_logging() {
    let filter = std::env::var("RELAY_LOG").unwrap_or_else(|_| "warn".to_owned());
    let subscriber =
        tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new(filter));
    if output::is_configured() {
        let _ = subscriber
            .with_ansi(false)
            .with_writer(output::TracingWriter)
            .try_init();
    } else {
        let _ = subscriber.with_writer(std::io::stderr).try_init();
    }
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
        Command::Pause => cmd_pause(&home, json),
        Command::Resume => cmd_resume(&home, json),
        Command::Rescan { target, no_wait } => cmd_rescan(&home, target.as_deref(), no_wait, json),
        Command::Activity { n, follow } => cmd_activity(&home, n, follow, json),
        Command::Pair {
            code,
            share,
            addr,
            allow_manage,
            listen,
        } => cmd_pair(&home, code, share, addr, allow_manage, listen, json),
        Command::Peer { cmd } => cmd_peer(&home, cmd, json),
        Command::Share { space, peer } => {
            apply_config(
                &home,
                ConfigChange::Share {
                    space: space.clone(),
                    peer: peer.clone(),
                },
            )?;
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
            apply_config(
                &home,
                ConfigChange::Unshare {
                    space: space.clone(),
                    peer: peer.clone(),
                },
            )?;
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
        Command::Recovery { cmd } => cmd_recovery(&home, cmd, json),
        Command::Replica { cmd } => cmd_replica(&home, cmd, json),
        Command::Transport { cmd } => cmd_transport(&home, cmd, json),
        Command::Group { cmd } => cmd_group(&home, cmd, json),
        Command::Policy { cmd } => cmd_policy(&home, cmd, json),
        Command::Materialize { cmd } => cmd_materialize(&home, cmd, json),
        Command::Browse { peer, path, all } => cmd_browse(&home, &peer, path, all, json),
        Command::Open {
            peer,
            path,
            into,
            read_only,
        } => {
            let opened = running_host(&home)?.open_remote(&OpenRemoteParams {
                peer,
                path,
                root: into,
                read_only,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&opened)?);
            } else {
                println!("{}", opened.path.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Opened { cmd: None } => {
            let opened = running_host(&home)?.quick_opens()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&opened)?);
            } else if opened.is_empty() {
                println!("no folders opened from other devices");
            } else {
                for q in opened {
                    let here = q
                        .local_path
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "-".into());
                    println!("{}  {}: {}  ->  {here}", q.space, q.peer, q.folder);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Opened {
            cmd: Some(OpenedCmd::Remove { space }),
        } => {
            let note = running_host(&home)?.quick_open_remove(&space)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"removed": space, "note": note})
                    )?
                );
            } else {
                println!("stopped syncing {space}; files here were not touched");
                if let Some(note) = note {
                    println!("note: {note}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::PairFolder {
            source,
            dest,
            from,
            to,
            create,
            excludes,
            exclude_patterns,
            online_only,
            check,
        } => {
            let params = FolderPairParams {
                source: FolderEnd {
                    device: from,
                    path: source,
                },
                dest: FolderEnd {
                    device: to,
                    path: dest,
                },
                create_dest: create,
                name: None,
                excludes,
                exclude_patterns,
                dest_online_only: online_only,
            };
            cmd_pair_folder(&home, &params, check, json)
        }
        Command::Fetch { target } => cmd_fetch(&home, &target, json),
        Command::Evict { target } => cmd_evict(&home, &target, json),
        Command::Conflicts { space, cmd } => match cmd {
            None => {
                let engine = Engine::open_read_only(&home)?;
                cmd_conflicts(&engine, space.as_deref(), json).map(|()| ExitCode::SUCCESS)
            }
            Some(ConflictsCmd::Resolve { target, keep }) => {
                cmd_conflicts_resolve(&home, &target, keep, json).map(|()| ExitCode::SUCCESS)
            }
            Some(ConflictsCmd::ResolveGit { target, branches }) => {
                cmd_conflicts_resolve_git(&home, &target, branches, json)
                    .map(|()| ExitCode::SUCCESS)
            }
        },
        Command::Deletes { cmd } => cmd_deletes(&home, cmd, json),
        Command::Space { cmd } => match cmd {
            SpaceCmd::Create { name } => {
                let space = applied_space(apply_config(
                    &home,
                    ConfigChange::CreateSpace { space: name },
                )?)?;
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
                let space = applied_space(apply_config(
                    &home,
                    ConfigChange::JoinSpace {
                        space: name_or_id,
                        from_peer: from,
                        wait_ms: 0,
                    },
                )?)?;
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
            SpaceCmd::Delete { name } => {
                apply_config(
                    &home,
                    ConfigChange::DeleteSpace {
                        space: name.clone(),
                    },
                )?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({"deleted": name}))?
                    );
                } else {
                    println!("deleted space {name}; files on disk were not touched");
                }
                Ok(ExitCode::SUCCESS)
            }
            SpaceCmd::Rotate { name } => {
                let mut engine = Engine::open_for_config(&home)?;
                engine.rotate_space_key(&name)?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({"rotated": name}))?
                    );
                } else {
                    println!("rotated key for space {name}");
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
                apply_config(
                    &home,
                    ConfigChange::AddMount {
                        space: space.clone(),
                        mount: mount.clone(),
                        path: path.clone(),
                        includes,
                        excludes,
                    },
                )?;
                let config = Engine::open_read_only(&home)?
                    .mounts(Some(&space))?
                    .into_iter()
                    .map(|(_, config)| config)
                    .find(|config| config.mount.name == mount)
                    .ok_or_else(|| anyhow::anyhow!("mount {space}/{mount} was not added"))?;
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
            MountCmd::Remove { space, mount } => {
                apply_config(
                    &home,
                    ConfigChange::RemoveMount {
                        space: space.clone(),
                        mount: mount.clone(),
                    },
                )?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &serde_json::json!({"removed": true, "space": space, "mount": mount})
                        )?
                    );
                } else {
                    println!("stopped syncing {space}/{mount}; files on disk were not touched");
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
            log_file: _,
            host,
        } => {
            let opts = WatchOptions {
                debounce: Duration::from_millis(debounce_ms),
                full_scan_interval: Duration::from_secs(full_scan_secs),
                use_watcher: !poll,
                ..WatchOptions::default()
            };
            cmd_run(&home, listen, opts, verbose, json, host)
        }
        Command::Service { cmd } => service::run(&home, cmd, json),
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
    host: HostKind,
) -> Result<ExitCode> {
    let engine = Engine::open_read_only(home)?;
    let peers = engine.peers()?;
    let mut names: HashMap<String, String> = peer_names(&peers);
    if peers.is_empty() && !json {
        output::out_line("no peers yet; pair another device with `relay pair`");
    }
    drop(engine);

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
    })
    .context("installing Ctrl-C handler")?;

    relay_daemon::run(
        home,
        DaemonOptions {
            listen,
            watch: opts,
            verbose,
            host,
            enable_stun: true,
            loopback_only: false,
            placeholders: true,
        },
        &stop,
        &mut |event| match event {
            DaemonEvent::Started {
                device_name,
                device_id,
                listen,
            } => {
                names = load_peer_names(home);
                if !json {
                    output::out_line(&format!(
                        "{} listening on {listen} as {device_name} ({device_id})",
                        utc_hms()
                    ));
                }
            }
            DaemonEvent::Watch(watch) => print_watch_event(watch, json, verbose, &names),
            DaemonEvent::Reloading => {
                if !json {
                    output::out_line(&format!("{} configuration changed; reloading", utc_hms()));
                }
            }
            DaemonEvent::Warning(message) => {
                if !json {
                    output::out_line(&format!("{} warning: {message}", utc_hms()));
                }
            }
            DaemonEvent::Paused => {
                if !json {
                    output::out_line(&format!("{} paused", utc_hms()));
                }
            }
            DaemonEvent::Resumed => {
                if !json {
                    output::out_line(&format!("{} resumed", utc_hms()));
                }
            }
        },
    )
    .map_err(|err| {
        if service::is_addr_in_use(&err) {
            err.context(service::listen_in_use_hint())
        } else {
            err
        }
    })?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_pair(
    home: &Path,
    code: Option<String>,
    share: Vec<String>,
    addr: Option<String>,
    allow_manage: bool,
    listen: SocketAddr,
    json: bool,
) -> Result<ExitCode> {
    if code.is_some() && !share.is_empty() {
        bail!("--share is only valid when starting a session (omit the code)");
    }
    if code.is_none() && addr.is_some() {
        bail!("--addr is only valid when joining (pass the pairing code)");
    }
    let joining = match code.as_deref() {
        Some(raw) => Some(PairingCode::parse(raw)?),
        None => None,
    };
    let _ready = Engine::open_read_only(home)?;

    let existing = Client::connect(home)?;
    let mut host = if existing.is_some() {
        None
    } else {
        Some(start_pair_host(home, listen)?)
    };
    let mut client = wait_pair_client(home, host.as_mut())?;
    let result = if let Some(code) = joining {
        let params = PairJoinParams {
            code: code.format(),
            addr,
            allow_manage,
        };
        pair_join_cli(home, params, json)
    } else {
        let params = PairStartParams {
            share,
            allow_manage,
        };
        pair_start_cli(&mut client, home, &params, json)
    };
    drop(host);
    result
}

struct PairHost {
    home: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<anyhow::Result<()>>>,
}

impl PairHost {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        if !self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            return None;
        }
        match self.thread.take()?.join() {
            Ok(Err(err)) => Some(err),
            Ok(Ok(())) => Some(anyhow::anyhow!("temporary host stopped unexpectedly")),
            Err(_) => Some(anyhow::anyhow!("temporary host thread panicked")),
        }
    }
}

impl Drop for PairHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = Client::connect(&self.home);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn start_pair_host(home: &Path, listen: SocketAddr) -> Result<PairHost> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let home_owned = home.to_path_buf();
    let thread = thread::Builder::new()
        .name("relay-pair-host".into())
        .spawn(move || {
            relay_daemon::run(
                &home_owned,
                DaemonOptions {
                    listen,
                    watch: WatchOptions::default(),
                    verbose: false,
                    host: HostKind::Cli,
                    enable_stun: true,
                    loopback_only: false,
                    placeholders: true,
                },
                &flag,
                &mut |_| {},
            )
        })
        .context("starting a temporary Relay host")?;
    Ok(PairHost {
        home: home.to_path_buf(),
        stop,
        thread: Some(thread),
    })
}

fn wait_pair_client(home: &Path, mut host: Option<&mut PairHost>) -> Result<Client> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = None::<String>;
    while Instant::now() < deadline {
        if let Some(host) = host.as_mut()
            && let Some(err) = host.take_error()
        {
            return Err(err.context("starting a temporary Relay host"));
        }
        match Client::connect(home) {
            Ok(Some(mut client)) => {
                if client.hello().is_ok() {
                    match client.status() {
                        Ok(status) if status.state == relay_ipc::HostState::Paused => {
                            bail!("Relay is paused; run `relay resume` before pairing");
                        }
                        Ok(status) if status.state == relay_ipc::HostState::Running => {
                            return Ok(client);
                        }
                        Ok(status) => {
                            last = Some(format!("host is {}", status.state.as_str()));
                        }
                        Err(_) => return Ok(client),
                    }
                }
            }
            Ok(None) => last = Some("waiting for the host".into()),
            Err(err) => last = Some(err.to_string()),
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "Relay host did not become ready ({})",
        last.unwrap_or_else(|| "timeout".into())
    )
}

fn install_pair_cancel() -> Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    let cancel = Arc::clone(&flag);
    ctrlc::set_handler(move || {
        cancel.store(true, Ordering::SeqCst);
    })
    .context("installing Ctrl-C handler")?;
    Ok(flag)
}

fn pair_start_cli(
    client: &mut Client,
    home: &Path,
    params: &PairStartParams,
    json: bool,
) -> Result<ExitCode> {
    let started = client.pair_start(params)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&started)?);
    } else {
        println!("Pairing code:  {}", started.code);
        println!();
        println!("On the other device, run:");
        println!("  relay pair {}", started.code);
        println!();
        println!("If the machines cannot see each other on the LAN (Tailscale or another VPN):");
        println!("  relay pair {} --addr HOST:47321", started.code);
        println!();
        println!("Waiting up to 10 minutes. Ctrl-C cancels this code.");
    }
    let cancel = install_pair_cancel()?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = client.pair_cancel();
            bail!("pairing cancelled");
        }
        match client.pair_status() {
            Ok(PairStatus::Paired { peer_name, peer_id }) => {
                print_paired(&peer_name, &peer_id, json)?;
                return Ok(ExitCode::SUCCESS);
            }
            Ok(PairStatus::Failed { reason }) => bail!("{reason}"),
            Ok(PairStatus::Expired) => bail!("pairing code expired"),
            Ok(PairStatus::Idle) => bail!("pairing cancelled"),
            Ok(PairStatus::Waiting) => {}
            Err(err) => {
                if let Ok(Some(next)) = Client::connect(home) {
                    *client = next;
                } else {
                    return Err(err.into());
                }
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn pair_join_cli(home: &Path, params: PairJoinParams, json: bool) -> Result<ExitCode> {
    let cancel = install_pair_cancel()?;
    let home_owned = home.to_path_buf();
    let addr = params.addr.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let mut client = wait_pair_client(&home_owned, None)?;
            client.pair_join(&params).map_err(anyhow::Error::from)
        })();
        let _ = tx.send(result);
    });
    if !json {
        match addr.as_deref() {
            Some(addr) => println!("Joining via {addr}…"),
            None => println!("Looking for the other device on the LAN…"),
        }
    }
    loop {
        if cancel.load(Ordering::Relaxed) {
            if let Ok(Some(mut client)) = Client::connect(home) {
                let _ = client.pair_cancel();
            }
            bail!("pairing cancelled");
        }
        match rx.try_recv() {
            Ok(Ok(joined)) => {
                print_paired(&joined.peer_name, &joined.peer_id, json)?;
                return Ok(ExitCode::SUCCESS);
            }
            Ok(Err(err)) => return Err(err),
            Err(mpsc::TryRecvError::Empty) => thread::sleep(Duration::from_millis(100)),
            Err(mpsc::TryRecvError::Disconnected) => bail!("pairing ended unexpectedly"),
        }
    }
}

fn print_paired(peer_name: &str, peer_id: &str, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "peer_name": peer_name,
                "peer_id": peer_id,
            }))?
        );
    } else {
        println!("Paired with {peer_name} ({peer_id})");
    }
    Ok(())
}

fn peer_names(peers: &[relay_engine::PeerInfo]) -> HashMap<String, String> {
    peers
        .iter()
        .map(|p| (p.id.to_string(), p.name.clone()))
        .collect()
}

fn load_peer_names(home: &Path) -> HashMap<String, String> {
    Engine::open_read_only(home)
        .ok()
        .and_then(|engine| engine.peers().ok())
        .map(|peers| peer_names(&peers))
        .unwrap_or_default()
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
            Ok(line) => output::out_line(&line),
            Err(err) => output::err_line(&format!("error: failed to serialize event: {err}")),
        }
        return;
    }
    match event {
        WatchEvent::Started { mounts } => {
            if mounts.is_empty() {
                output::out_line(&format!("{} watching (no local mounts)", utc_hms()));
            } else {
                output::out_line(&format!("{} watching {}", utc_hms(), mounts.join(", ")));
            }
        }
        WatchEvent::MountRemoved { space, mount } => {
            output::out_line(&format!("{} stopped syncing {space}/{mount}", utc_hms()));
        }
        WatchEvent::Scanned {
            space,
            mount,
            full,
            paths,
            report,
        } => {
            if !*full && !report.has_changes() && !verbose {
                return;
            }
            if *full && !report.has_changes() && !verbose {
                return;
            }
            if *full && !report.has_changes() {
                output::out_line(&format!(
                    "{} {space}/{mount}: full scan, 0 changes",
                    utc_hms()
                ));
                return;
            }
            let summary = watch_change_summary(report);
            if *full {
                output::out_line(&format!(
                    "{} {space}/{mount}: full scan, {summary}",
                    utc_hms()
                ));
            } else {
                output::out_line(&format!(
                    "{} {space}/{mount}: {summary} ({paths} paths)",
                    utc_hms()
                ));
            }
            for warning in &report.warnings {
                output::out_line(&format!("  warning: {warning}"));
            }
        }
        WatchEvent::ScanFailed {
            space,
            mount,
            error,
        } => {
            output::err_line(&format!("error: {space}/{mount}: {error}"));
            print_watch_error_hints(error);
        }
        WatchEvent::WatcherUnavailable {
            space,
            mount,
            error,
        } => {
            output::err_line(&format!("error: {space}/{mount}: {error}"));
        }
        WatchEvent::Stopped => output::out_line("stopped"),
        WatchEvent::PeerConnected { peer, name } => {
            let label = names.get(peer).unwrap_or(name);
            output::out_line(&format!("{} connected to {label}", utc_hms()));
        }
        WatchEvent::PeerDisconnected { peer } => {
            output::out_line(&format!(
                "{} disconnected from {}",
                utc_hms(),
                peer_label(names, peer)
            ));
        }
        WatchEvent::OffersReceived { peer, spaces } => {
            let label = peer_label(names, peer);
            output::out_line(&format!(
                "{} {label} offers: {}",
                utc_hms(),
                spaces.join(", ")
            ));
            for name in spaces {
                output::out_line(&format!(
                    "  to accept: `relay space join {name} --from {label}`"
                ));
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
            output::out_line(&format!(
                "{} {space}/{mount} from {peer}: {written} written, {deleted} deleted, {conflicts} conflicts, {skipped} skipped",
                utc_hms()
            ));
        }
        WatchEvent::SentChanges {
            peer,
            space,
            entries,
        } => {
            let peer = peer_label(names, peer);
            output::out_line(&format!(
                "{} sent {entries} changes of {space} to {peer}",
                utc_hms()
            ));
        }
        WatchEvent::SyncWarning { peer, path, reason } => {
            output::err_line(&format!(
                "warning: {} {path}: {reason}",
                peer_label(names, peer)
            ));
        }
        WatchEvent::DeletesHeld {
            peer,
            space,
            mount,
            deletions,
            live,
        } => {
            let peer = peer_label(names, peer);
            output::out_line(&format!(
                "{} {peer} wants to delete {deletions} of {live} files in {space}/{mount}; nothing has been deleted. Decide with `relay deletes apply` or `relay deletes restore`",
                utc_hms()
            ));
        }
        WatchEvent::Transfers(_) => {}
        WatchEvent::ScanProgress { .. } => {}
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
        output::err_line("hint: re-run with --allow-mass-delete if this was intentional");
    } else if error.contains("marker")
        || error.contains("mount root")
        || error.contains("not a directory")
    {
        output::err_line("hint: is the drive mounted / was the folder moved?");
    } else if error.contains("another relay process") {
        output::err_line("hint: stop `relay watch` or wait for the other command to finish");
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

/// Apply a config change through the running host so its sessions stay up;
/// with no host running, write it directly.
fn apply_config(home: &Path, change: ConfigChange) -> Result<ConfigApplied> {
    if let Some(mut client) = Client::connect(home)? {
        return Ok(client.config(&change)?);
    }
    Ok(Engine::open_for_config(home)?.apply_config(&change)?)
}

fn applied_space(applied: ConfigApplied) -> Result<relay_core::Space> {
    match applied {
        ConfigApplied::Space { space } => Ok(space),
        other => bail!("unexpected result {other:?}"),
    }
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
            apply_config(
                home,
                ConfigChange::AddPeer {
                    peer: name.clone(),
                    id,
                    addresses,
                },
            )?;
            let peer = Engine::open_read_only(home)?
                .peers()?
                .into_iter()
                .find(|peer| peer.id == id)
                .ok_or_else(|| anyhow::anyhow!("peer {name} was not added"))?;
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
                    let mark = if peer.revoked { " revoked" } else { "" };
                    let manage = if peer.may_manage {
                        " manages this device"
                    } else {
                        ""
                    };
                    println!("{}  {}  {addrs}{mark}{manage}", peer.name, peer.id);
                }
            }
        }
        PeerCmd::Remove { name } => {
            apply_config(home, ConfigChange::RemovePeer { peer: name.clone() })?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"removed": name}))?
                );
            } else {
                println!("removed peer {name}");
            }
        }
        PeerCmd::AllowManage { name } => set_peer_manage(home, name, true, json)?,
        PeerCmd::DenyManage { name } => set_peer_manage(home, name, false, json)?,
        PeerCmd::Revoke { name } => {
            apply_config(home, ConfigChange::RevokePeer { peer: name.clone() })?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"revoked": name}))?
                );
            } else {
                println!("revoked peer {name}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn set_peer_manage(home: &Path, peer: String, allowed: bool, json: bool) -> Result<()> {
    apply_config(
        home,
        ConfigChange::SetPeerManage {
            peer: peer.clone(),
            allowed,
        },
    )?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"peer": peer, "may_manage": allowed})
            )?
        );
    } else if allowed {
        println!("{peer} can now browse this device and set up sync on it");
    } else {
        println!("{peer} can no longer manage this device");
    }
    Ok(())
}

fn cmd_recovery(home: &Path, cmd: RecoveryCmd, json: bool) -> Result<ExitCode> {
    match cmd {
        RecoveryCmd::Show => {
            let mut engine = Engine::open_for_config(home)?;
            let key = engine.reveal_recovery_key()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"recovery_key": key}))?
                );
            } else {
                println!("{key}");
            }
        }
        RecoveryCmd::Import { key } => {
            let mut engine = Engine::open_for_config(home)?;
            let installed = engine.import_recovery_key(&key)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"installed": installed}))?
                );
            } else {
                println!("installed {installed} space key generation(s)");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_transport(home: &Path, cmd: TransportCmd, json: bool) -> Result<ExitCode> {
    match cmd {
        TransportCmd::Set { addr, serve } => {
            let mut engine = Engine::open_for_config(home)?;
            engine.set_transport_relay(&addr, serve)?;
            print_transport(&engine.transport_status()?, json)?;
        }
        TransportCmd::Clear => {
            let mut engine = Engine::open_for_config(home)?;
            engine.clear_transport()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"cleared": true}))?
                );
            } else {
                println!("relay cleared");
            }
        }
        TransportCmd::Status => {
            let engine = Engine::open_read_only(home)?;
            print_transport(&engine.transport_status()?, json)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_transport(status: &TransportStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
        return Ok(());
    }
    match &status.relay {
        Some(addr) => println!("relay {addr}"),
        None => println!("relay not set"),
    }
    println!("serve {}", if status.serve { "yes" } else { "no" });
    match &status.mailbox {
        Some(addr) => println!("mailbox {addr}"),
        None => println!("mailbox not set"),
    }
    Ok(())
}

fn cmd_replica(home: &Path, cmd: ReplicaCmd, json: bool) -> Result<ExitCode> {
    match cmd {
        ReplicaCmd::Set { path } => {
            let mut engine = Engine::open_for_config(home)?;
            engine.set_replica_path(&path)?;
            let status = engine.replica_status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!(
                    "replica path {}",
                    status
                        .path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                );
            }
        }
        ReplicaCmd::Clear => {
            let mut engine = Engine::open_for_config(home)?;
            engine.clear_replica_path()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"cleared": true}))?
                );
            } else {
                println!("replica path cleared");
            }
        }
        ReplicaCmd::Status => {
            let engine = Engine::open_read_only(home)?;
            let status = engine.replica_status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                match &status.path {
                    Some(path) => println!("replica path {}", path.display()),
                    None => println!("replica path not set"),
                }
                for row in &status.pushed {
                    println!("  {} pushed_seq {}", row.space, row.pushed_seq);
                }
            }
        }
        ReplicaCmd::Gc { mirror, grace_secs } => {
            let mut engine = Engine::open_for_config(home)?;
            let report = engine.replica_gc(mirror, Duration::from_secs(grace_secs))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "removed {} entries, {} objects ({} bytes)",
                    report.entries_removed, report.objects_removed, report.bytes_freed
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_group(home: &Path, cmd: GroupCmd, json: bool) -> Result<ExitCode> {
    match cmd {
        GroupCmd::Create { name } => {
            apply_config(
                home,
                ConfigChange::GroupCreate {
                    group: name.clone(),
                },
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"created": name}))?
                );
            } else {
                println!("created group {name}");
            }
        }
        GroupCmd::Add { name, peer } => {
            apply_config(
                home,
                ConfigChange::GroupAdd {
                    group: name.clone(),
                    member: peer.clone(),
                },
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"group": name, "added": peer})
                    )?
                );
            } else {
                println!("added {peer} to group {name}");
            }
        }
        GroupCmd::Remove { name, peer } => {
            apply_config(
                home,
                ConfigChange::GroupRemove {
                    group: name.clone(),
                    member: peer.clone(),
                },
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"group": name, "removed": peer})
                    )?
                );
            } else {
                println!("removed {peer} from group {name}");
            }
        }
        GroupCmd::Delete { name } => {
            apply_config(
                home,
                ConfigChange::GroupDelete {
                    group: name.clone(),
                },
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"deleted": name}))?
                );
            } else {
                println!("deleted group {name}");
            }
        }
        GroupCmd::List => {
            let engine = Engine::open_read_only(home)?;
            let groups = engine.groups()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&groups)?);
            } else if groups.is_empty() {
                println!("no groups");
            } else {
                for group in groups {
                    let members = if group.members.is_empty() {
                        "-".to_owned()
                    } else {
                        group.members.join(", ")
                    };
                    println!("{}  {members}", group.name);
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_policy(home: &Path, cmd: PolicyCmd, json: bool) -> Result<ExitCode> {
    match cmd {
        PolicyCmd::Add {
            space,
            name,
            selectors,
            peers,
            groups,
        } => {
            apply_config(
                home,
                ConfigChange::PolicyAdd {
                    space: space.clone(),
                    name: name.clone(),
                    selectors,
                    peers,
                    groups,
                },
            )?;
            let policy = Engine::open_read_only(home)?
                .policies(Some(&space))?
                .into_iter()
                .find(|policy| policy.name == name)
                .ok_or_else(|| anyhow::anyhow!("policy {name} was not added"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&policy)?);
            } else {
                println!(
                    "added policy {name} on {space} ({} selector(s), {} target(s))",
                    policy.selectors.len(),
                    policy.targets.len()
                );
            }
        }
        PolicyCmd::Remove { space, name } => {
            apply_config(
                home,
                ConfigChange::PolicyRemove {
                    space: space.clone(),
                    name: name.clone(),
                },
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"removed": name, "space": space})
                    )?
                );
            } else {
                println!("removed policy {name} from {space}");
            }
        }
        PolicyCmd::List { space } => {
            let engine = Engine::open_read_only(home)?;
            let policies = engine.policies(space.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&policies)?);
            } else if policies.is_empty() {
                println!("no policies");
            } else {
                for policy in policies {
                    let selectors = policy.selectors.join(", ");
                    let mut targets = policy.peer_names.clone();
                    for g in &policy.group_names {
                        targets.push(format!("group:{g}"));
                    }
                    let targets = if targets.is_empty() {
                        "-".to_owned()
                    } else {
                        targets.join(", ")
                    };
                    println!(
                        "{}/{}  selectors=[{selectors}]  targets=[{targets}]",
                        policy.space, policy.name
                    );
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_materialize(home: &Path, cmd: MaterializeCmd, json: bool) -> Result<ExitCode> {
    match cmd {
        MaterializeCmd::Add {
            space,
            name,
            mode,
            selectors,
        } => {
            apply_config(
                home,
                ConfigChange::MaterializeAdd {
                    space: space.clone(),
                    name: name.clone(),
                    mode: mode.as_str().to_owned(),
                    selectors,
                },
            )?;
            let rule = Engine::open_read_only(home)?
                .materialization_rules(Some(&space))?
                .into_iter()
                .find(|rule| rule.name == name)
                .ok_or_else(|| anyhow::anyhow!("materialization {name} was not added"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rule)?);
            } else {
                println!(
                    "added materialization {name} on {space} ({}, {} selector(s))",
                    rule.mode,
                    rule.selectors.len()
                );
            }
        }
        MaterializeCmd::Remove { space, name } => {
            apply_config(
                home,
                ConfigChange::MaterializeRemove {
                    space: space.clone(),
                    name: name.clone(),
                },
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"removed": name, "space": space})
                    )?
                );
            } else {
                println!("removed materialization {name} from {space}");
            }
        }
        MaterializeCmd::List { space } => {
            let engine = Engine::open_read_only(home)?;
            let rules = engine.materialization_rules(space.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rules)?);
            } else if rules.is_empty() {
                println!("no materialization rules");
            } else {
                for rule in rules {
                    let selectors = rule.selectors.join(", ");
                    println!(
                        "{}/{}  {}  selectors=[{selectors}]",
                        rule.space, rule.name, rule.mode
                    );
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A client for commands that only make sense with Relay running.
fn running_host(home: &Path) -> Result<Client> {
    Client::connect(home)?.ok_or_else(|| anyhow::anyhow!("Relay is not running; start it first"))
}

fn cmd_pair_folder(
    home: &Path,
    params: &FolderPairParams,
    check: bool,
    json: bool,
) -> Result<ExitCode> {
    let mut client = running_host(home)?;
    let plan = client.folder_pair_preview(params)?;
    if check || !plan.problems.is_empty() {
        if json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            for line in plan.problems.iter().chain(&plan.warnings) {
                println!("{line}");
            }
            if plan.problems.is_empty() {
                println!("ready: the space will be named {}", plan.space);
            }
        }
        return Ok(if plan.problems.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }
    let made = client.folder_pair(params)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&made)?);
    } else {
        for warning in &plan.warnings {
            println!("note: {warning}");
        }
        println!(
            "syncing {} with {} (space {})",
            made.source_path, made.dest_path, made.space
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_browse(
    home: &Path,
    peer: &str,
    path: Option<String>,
    all: bool,
    json: bool,
) -> Result<ExitCode> {
    let mut client = running_host(home)?;
    let reply = match path {
        None => client.remote(peer, &RemoteCall::Roots)?,
        Some(path) => client.remote(
            peer,
            &RemoteCall::ListDir {
                path,
                cursor: 0,
                limit: relay_core::remote::MAX_LISTING,
            },
        )?,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&reply)?);
        return Ok(ExitCode::SUCCESS);
    }
    match reply {
        RemoteReply::Roots { roots } => {
            for root in roots {
                println!("{}  {}", root.name, root.path);
            }
        }
        RemoteReply::Listing { listing } => {
            println!("{}", listing.path);
            for entry in listing.entries.iter().filter(|e| all || !e.hidden) {
                let slash = if entry.kind == DirEntryKind::Directory {
                    "/"
                } else {
                    ""
                };
                let mut notes = Vec::new();
                if let Some(mount) = &entry.mount {
                    notes.push(format!("synced {}/{}", mount.space, mount.mount));
                } else if entry.contains_mount {
                    notes.push("contains synced folder".to_owned());
                }
                if entry.cloud_only {
                    notes.push("cloud".to_owned());
                }
                let notes = if notes.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", notes.join(", "))
                };
                println!("  {}{slash}{notes}", entry.name);
            }
            if listing.next_cursor.is_some() {
                println!(
                    "  … {} entries in all; showing the first {}",
                    listing.total,
                    listing.entries.len()
                );
            }
        }
        other => println!("{other:?}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_fetch(home: &Path, target: &str, json: bool) -> Result<ExitCode> {
    let (space, mount, path) = require_file_target(target, "fetch")?;
    if let Some(mut client) = Client::connect(home)? {
        client.fetch(&space, &mount, path.as_str())?;
    } else {
        let mut engine = Engine::open_for_config(home)?;
        engine.fetch_path(&space, &mount, path.as_str())?;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "space": space,
                "mount": mount,
                "path": path.as_str(),
                "materialized": true,
            }))?
        );
    } else {
        println!("fetched {space}/{mount}/{path}");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_evict(home: &Path, target: &str, json: bool) -> Result<ExitCode> {
    let parsed = parse_target(target)?;
    let mount = parsed
        .mount
        .ok_or_else(|| anyhow::anyhow!("evict requires SPACE/MOUNT[/PATH]"))?;
    let space = parsed.space;
    let path = parsed
        .path
        .map(|p| p.as_str().to_owned())
        .unwrap_or_default();
    let evicted = match Client::connect(home)? {
        Some(mut client) => client.evict(&space, &mount, &path)?,
        None => Engine::open_for_config(home)?.evict(&space, &mount, &path)?,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "space": space,
                "mount": mount,
                "path": path,
                "evicted": evicted,
            }))?
        );
    } else {
        println!("freed {evicted} file(s) under {target}");
    }
    Ok(ExitCode::SUCCESS)
}

fn require_file_target(raw: &str, cmd: &str) -> Result<(String, String, LogicalPath)> {
    let parsed = parse_target(raw)?;
    let mount = parsed
        .mount
        .ok_or_else(|| anyhow::anyhow!("{cmd} requires SPACE/MOUNT/PATH"))?;
    let path = parsed
        .path
        .ok_or_else(|| anyhow::anyhow!("{cmd} requires SPACE/MOUNT/PATH"))?;
    Ok((parsed.space, mount, path))
}

fn cmd_conflicts(engine: &Engine, space: Option<&str>, json: bool) -> Result<()> {
    let infos = engine.conflict_infos(space)?;
    if json {
        let rows: Vec<_> = infos.iter().map(conflict_json).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if infos.is_empty() {
        println!("no conflicts");
        return Ok(());
    }
    let names = device_names(engine)?;
    let git_groups = group_git_conflicts(&infos);
    for info in &infos {
        if matches!(info.class, ConflictClass::File { .. }) {
            println!("{}", info.record.key.path);
        }
    }
    for ((space_name, mount_name, git_dir), copies) in git_groups {
        print_git_conflict_summary(&space_name, &mount_name, &git_dir, &copies, &names);
    }
    Ok(())
}

fn cmd_conflicts_resolve(home: &Path, target: &str, keep: KeepChoice, json: bool) -> Result<()> {
    let parsed = parse_target(target)?;
    let mount = parsed
        .mount
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("conflicts resolve requires SPACE/MOUNT/PATH"))?;
    let path = parsed
        .path
        .ok_or_else(|| anyhow::anyhow!("conflicts resolve requires SPACE/MOUNT/PATH"))?;
    let resolution = match keep {
        KeepChoice::Current => Resolution::KeepCurrent,
        KeepChoice::Copy => Resolution::UseCopy,
    };
    let report = resolve_conflict(home, &parsed.space, mount, &path, resolution)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        match report.resolution {
            Resolution::KeepCurrent => {
                println!(
                    "kept current {}/{}/{}; deleted {}",
                    report.space, report.mount, report.original, report.copy
                );
            }
            Resolution::UseCopy => {
                println!(
                    "replaced {}/{}/{} with {}; deleted the copy",
                    report.space, report.mount, report.original, report.copy
                );
            }
        }
        println!(
            "previous versions remain in `relay history {}/{}/{}`",
            report.space, report.mount, report.original
        );
        if report.scanned {
            println!("index updated");
        } else {
            println!("a running sync loop will pick up the change");
        }
    }
    Ok(())
}

fn cmd_conflicts_resolve_git(home: &Path, target: &str, branches: bool, json: bool) -> Result<()> {
    let parsed = parse_target(target)?;
    let mount = parsed
        .mount
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("conflicts resolve-git requires SPACE/MOUNT/PATH"))?;
    let path = parsed
        .path
        .ok_or_else(|| anyhow::anyhow!("conflicts resolve-git requires SPACE/MOUNT/PATH"))?;
    let report = resolve_git_conflicts(home, &parsed.space, mount, &path, branches)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "cleaned {}/{}/{}: deleted {} copies, kept {} branches",
            report.space,
            report.mount,
            report.git_dir,
            report.deleted.len(),
            report.kept.len()
        );
        if !branches && !report.kept.is_empty() {
            println!(
                "hint: conflicting branches remain for merging in Git; add --branches to delete them"
            );
        }
        println!(
            "previous versions remain in `relay history {}/{}/<path>`",
            report.space, report.mount
        );
        if report.scanned {
            println!("index updated");
        } else {
            println!("a running sync loop will pick up the change");
        }
    }
    Ok(())
}

fn device_names(engine: &Engine) -> Result<HashMap<DeviceId, String>> {
    let mut names = HashMap::new();
    names.insert(engine.device().id, engine.device().name.clone());
    for peer in engine.peers()? {
        names.insert(peer.id, peer.name);
    }
    Ok(names)
}

fn print_git_conflict_summary(
    space: &str,
    mount: &str,
    git_dir: &LogicalPath,
    copies: &[&ConflictInfo],
    names: &HashMap<DeviceId, String>,
) {
    let mut branches = Vec::new();
    let mut metadata = 0usize;
    let mut from: Vec<String> = Vec::new();
    for copy in copies {
        let label = names
            .get(&copy.record.modified_by)
            .cloned()
            .unwrap_or_else(|| copy.record.modified_by.short());
        if !from.contains(&label) {
            from.push(label);
        }
        match &copy.class {
            ConflictClass::Git { is_ref: true, .. } => {
                let shown = copy
                    .record
                    .key
                    .path
                    .as_str()
                    .strip_prefix(git_dir.as_str())
                    .and_then(|s| s.strip_prefix('/'))
                    .unwrap_or(copy.record.key.path.as_str());
                branches.push(shown.to_owned());
            }
            _ => metadata += 1,
        }
    }
    let branch_bit = match branches.len() {
        0 => "0 branches".to_owned(),
        1 => format!("1 branch ({})", branches[0]),
        n => format!("{n} branches ({})", branches.join(", ")),
    };
    let meta_bit = if metadata == 1 {
        "1 metadata copy".to_owned()
    } else {
        format!("{metadata} metadata copies")
    };
    let from_bit = if from.is_empty() {
        String::new()
    } else {
        format!(" (from {})", from.join(", "))
    };
    println!("{space}/{mount}: {git_dir} — {branch_bit}, {meta_bit}{from_bit}");
    println!("  hint: relay conflicts resolve-git {space}/{mount}/{git_dir}");
    println!("        (add --branches to also delete conflicting branches)");
}

#[derive(serde::Serialize)]
struct ConflictJson {
    #[serde(flatten)]
    record: EntryRecord,
    classification: ConflictClass,
    space: String,
    mount: String,
}

fn conflict_json(info: &ConflictInfo) -> ConflictJson {
    ConflictJson {
        record: info.record.clone(),
        classification: info.class.clone(),
        space: info.space.clone(),
        mount: info.mount.clone(),
    }
}

fn cmd_status(engine: &Engine, json: bool) -> Result<()> {
    let status = engine.status()?;
    let holds = engine.delete_holds()?;
    let daemon = query_daemon(engine.home());
    let service = if service::supported() {
        match service::status_info(engine.home()) {
            Ok(info) => Some(info),
            Err(err) => {
                eprintln!("warning: could not query the background service: {err:#}");
                None
            }
        }
    } else {
        None
    };
    if json {
        let mut value = serde_json::to_value(&status)?;
        value["delete_holds"] = serde_json::to_value(&holds)?;
        value["daemon"] = match &daemon {
            Some((hello, live)) => serde_json::json!({
                "host": hello.host,
                "pid": hello.pid,
                "started_at_ms": hello.started_at_ms,
                "protocol": hello.protocol,
                "relay_version": hello.relay_version,
                "state": live.state,
                "message": live.message,
                "listen": live.listen,
                "peers": live.peers,
                "mounts": live.mounts,
                "transfers": live.transfers,
            }),
            None => serde_json::Value::Null,
        };
        if let Some(info) = service {
            value["service"] = serde_json::to_value(info)?;
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
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
    if !holds.is_empty() {
        println!("held deletes");
        for hold in &holds {
            let decision = hold.decision.map_or("pending", DeleteHoldDecision::as_str);
            println!(
                "  {} wants to delete {} of {} files in {}/{} (held {}, {decision})",
                hold.peer_name,
                hold.deletions,
                hold.live,
                hold.space,
                hold.mount,
                format_utc_ms(hold.held_at_ms)
            );
        }
    }
    if let Some(info) = service {
        println!("{}", service::format_status_line(&info));
    }
    print_daemon_human(daemon.as_ref());
    Ok(())
}

fn query_daemon(home: &Path) -> Option<(relay_ipc::Hello, DaemonStatus)> {
    let mut client = Client::connect(home).ok().flatten()?;
    let hello = client.hello().ok()?;
    let status = client.status().ok()?;
    Some((hello, status))
}

fn print_daemon_human(daemon: Option<&(relay_ipc::Hello, DaemonStatus)>) {
    let Some((hello, live)) = daemon else {
        println!("daemon: not running");
        return;
    };
    let state = live.state.as_str();
    println!("daemon: {state} ({} pid {})", hello.host, hello.pid);
    if let Some(listen) = &live.listen {
        println!("  listen {listen}");
    }
    if let Some(message) = &live.message {
        println!("  {message}");
    }
    if live.peers.is_empty() {
        println!("  peers: none connected");
    } else {
        for peer in &live.peers {
            println!("  peer {} {}", peer.name, peer.id);
        }
    }
    for mount in &live.mounts {
        let path = mount
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".to_owned());
        let scan = mount
            .last_scan_summary
            .clone()
            .or_else(|| mount.last_error.clone())
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "  {}/{}  {}  {}  {scan}",
            mount.space,
            mount.mount,
            path,
            mount.watching.as_str()
        );
    }
    if !live.transfers.is_empty() {
        println!("sync:");
        for row in &live.transfers {
            println!("  {}", format_transfer(row));
        }
    }
}

fn format_transfer(row: &relay_ipc::TransferLive) -> String {
    let rate = if row.bytes_per_sec > 0 {
        format!("  {}", format_byte_rate(row.bytes_per_sec))
    } else {
        String::new()
    };
    match row.direction {
        relay_ipc::TransferDirection::Receive => {
            let bytes = match row.bytes_total {
                Some(total) => format!(
                    "{} of {}",
                    format_byte_count(row.bytes_done),
                    format_byte_count(total)
                ),
                None => format_byte_count(row.bytes_done),
            };
            let files = match row.files_total {
                Some(total) => format!("{} of {total} files", row.files_done),
                None => format!("{} files", row.files_done),
            };
            let retry = if row.retries > 0 {
                format!("  {} files waiting to retry", row.retries)
            } else {
                String::new()
            };
            format!(
                "receiving from {}  {}  {files}  {bytes}{rate}{retry}",
                row.peer_name, row.space
            )
        }
        relay_ipc::TransferDirection::Send => format!(
            "sending to {}  {}  {}{rate}",
            row.peer_name,
            row.space,
            format_byte_count(row.bytes_done)
        ),
        relay_ipc::TransferDirection::Index => {
            let mount = row.mount.as_deref().unwrap_or(&row.space);
            format!(
                "indexing {mount}  {} files  {}",
                row.files_done,
                format_byte_count(row.bytes_done)
            )
        }
    }
}

fn format_byte_count(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let kb = n as f64 / 1024.0;
    if kb < 1024.0 {
        return format!("{kb:.1} KB");
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return format!("{mb:.1} MB");
    }
    format!("{:.1} GB", mb / 1024.0)
}

fn format_byte_rate(n: u64) -> String {
    format!("{}/s", format_byte_count(n))
}

fn cmd_pause(home: &Path, json: bool) -> Result<ExitCode> {
    if let Some(mut client) = Client::connect(home)? {
        let status = client.pause()?;
        return print_pause_result(json, true, Some(&status));
    }
    let mut engine = Engine::open_for_config(home)?;
    engine.set_paused(true)?;
    print_pause_result(json, true, None)
}

fn cmd_resume(home: &Path, json: bool) -> Result<ExitCode> {
    if let Some(mut client) = Client::connect(home)? {
        let status = client.resume()?;
        return print_pause_result(json, false, Some(&status));
    }
    let mut engine = Engine::open_for_config(home)?;
    engine.set_paused(false)?;
    print_pause_result(json, false, None)
}

fn print_pause_result(json: bool, paused: bool, live: Option<&DaemonStatus>) -> Result<ExitCode> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "paused": paused,
                "state": live.map(|s| s.state),
                "daemon": live.is_some(),
            }))?
        );
    } else {
        match (paused, live) {
            (true, Some(_)) => println!("paused"),
            (false, Some(_)) => println!("resumed"),
            (true, None) => {
                println!("paused (will take effect the next time Relay starts)")
            }
            (false, None) => {
                println!("resume requested (will take effect the next time Relay starts)")
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_rescan(home: &Path, target: Option<&str>, no_wait: bool, json: bool) -> Result<ExitCode> {
    let (space, mount) = parse_rescan_target(target)?;
    if Client::connect(home)?.is_some() {
        if no_wait {
            let mut client =
                Client::connect(home)?.ok_or_else(|| anyhow::anyhow!("Relay is not running"))?;
            let queued = client.rescan(space.as_deref(), mount.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&queued)?);
            } else {
                println!("queued {}", queued.queued.join(", "));
            }
            return Ok(ExitCode::SUCCESS);
        }
        let sub_client =
            Client::connect(home)?.ok_or_else(|| anyhow::anyhow!("Relay is not running"))?;
        let mut cmd_client =
            Client::connect(home)?.ok_or_else(|| anyhow::anyhow!("Relay is not running"))?;
        let mut pending = wait_rescan_start(sub_client)?;
        let queued = cmd_client.rescan(space.as_deref(), mount.as_deref())?;
        if !json {
            println!("queued {}", queued.queued.join(", "));
        }
        pending.extend(queued.queued);
        return wait_rescan_finish(pending, json);
    }
    let mut engine = Engine::open(home)?;
    cmd_scan(&mut engine, target, false, false, json)?;
    Ok(ExitCode::SUCCESS)
}

fn parse_rescan_target(target: Option<&str>) -> Result<(Option<String>, Option<String>)> {
    let Some(raw) = target else {
        return Ok((None, None));
    };
    let parsed = parse_target(raw)?;
    if parsed.path.is_some() {
        bail!("rescan target is SPACE or SPACE/MOUNT");
    }
    Ok((Some(parsed.space), parsed.mount))
}

struct RescanWait {
    pending: std::collections::HashSet<String>,
    rx: std::sync::mpsc::Receiver<ActivityItem>,
}

fn wait_rescan_start(client: Client) -> Result<RescanWait> {
    let mut sub = client.subscribe()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(Some(item)) = sub.next_item() {
            if tx.send(item).is_err() {
                break;
            }
        }
    });
    Ok(RescanWait {
        pending: std::collections::HashSet::new(),
        rx,
    })
}

impl RescanWait {
    fn extend(&mut self, queued: Vec<String>) {
        self.pending.extend(queued);
    }
}

fn wait_rescan_finish(mut wait: RescanWait, json: bool) -> Result<ExitCode> {
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut failed: Vec<ActivityItem> = Vec::new();
    while !wait.pending.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let left: Vec<String> = wait.pending.into_iter().collect();
            bail!("timed out waiting for rescan of {}", left.join(", "));
        }
        match wait.rx.recv_timeout(remaining.min(Duration::from_secs(1))) {
            Ok(item) => {
                if item.kind != "scan" && item.kind != "scan_failed" {
                    continue;
                }
                let key = item
                    .detail
                    .clone()
                    .or_else(|| item.summary.split(':').next().map(|s| s.trim().to_owned()));
                let Some(key) = key else {
                    continue;
                };
                if !wait.pending.remove(&key) {
                    continue;
                }
                if json {
                    println!("{}", serde_json::to_string(&item)?);
                } else if item.kind == "scan_failed" {
                    eprintln!("error: {}", item.summary);
                    print_watch_error_hints(&item.summary);
                } else {
                    println!("{}", item.summary);
                }
                if item.kind == "scan_failed" {
                    failed.push(item);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("host closed the activity stream before rescan finished");
            }
        }
    }
    Ok(rescan_exit_code(&failed))
}

/// The host ran the scans; exit as `relay scan` would have: 2 for a refused
/// mass delete, 1 for any other failure.
fn rescan_exit_code(failed: &[ActivityItem]) -> ExitCode {
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else if failed
        .iter()
        .any(|item| item.summary.contains("refusing to delete"))
    {
        ExitCode::from(2)
    } else {
        ExitCode::from(1)
    }
}

fn cmd_activity(home: &Path, n: usize, follow: bool, json: bool) -> Result<ExitCode> {
    let Some(mut client) = Client::connect(home)? else {
        bail!("Relay is not running");
    };
    let items = client.activity(Some(n as u32))?;
    for item in &items {
        print_activity_item(item, json);
    }
    if !follow {
        return Ok(ExitCode::SUCCESS);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let _ = ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
    });
    let mut sub = client.subscribe()?;
    while !stop.load(Ordering::Relaxed) {
        match sub.next_item()? {
            Some(item) => print_activity_item(&item, json),
            None => break,
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_activity_item(item: &ActivityItem, json: bool) {
    if json {
        match serde_json::to_string(item) {
            Ok(line) => println!("{line}"),
            Err(err) => eprintln!("error: {err}"),
        }
    } else {
        println!(
            "{}  {}  {}",
            format_utc_ms(item.at_ms as i64),
            item.kind,
            item.summary
        );
    }
}

fn cmd_deletes(home: &Path, cmd: Option<DeletesCmd>, json: bool) -> Result<ExitCode> {
    match cmd {
        None => {
            let engine = Engine::open_read_only(home)?;
            let holds = engine.delete_holds()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&holds)?);
                return Ok(ExitCode::SUCCESS);
            }
            if holds.is_empty() {
                println!("no held deletes");
                return Ok(ExitCode::SUCCESS);
            }
            for hold in holds {
                let decision = hold.decision.map_or("pending", DeleteHoldDecision::as_str);
                println!(
                    "{}  {}/{}  wants to delete {} of {} files  held {}  {decision}",
                    hold.peer_name,
                    hold.space,
                    hold.mount,
                    hold.deletions,
                    hold.live,
                    format_utc_ms(hold.held_at_ms)
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(DeletesCmd::Apply { space, mount, peer }) => decide_delete_hold(
            home,
            &space,
            mount.as_deref(),
            peer.as_deref(),
            DeleteHoldDecision::Apply,
            json,
        ),
        Some(DeletesCmd::Restore { space, mount, peer }) => decide_delete_hold(
            home,
            &space,
            mount.as_deref(),
            peer.as_deref(),
            DeleteHoldDecision::Restore,
            json,
        ),
    }
}

fn decide_delete_hold(
    home: &Path,
    space: &str,
    mount: Option<&str>,
    peer: Option<&str>,
    decision: DeleteHoldDecision,
    json: bool,
) -> Result<ExitCode> {
    let applied = apply_config(
        home,
        ConfigChange::DecideDeleteHold {
            space: space.to_owned(),
            mount: mount.map(str::to_owned),
            peer: peer.map(str::to_owned),
            decision,
        },
    )?;
    let ConfigApplied::Holds { decided: n } = applied else {
        bail!("unexpected result {applied:?}");
    };
    let word = decision.as_str();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "updated": n,
                "decision": word,
                "space": space,
                "mount": mount,
                "peer": peer,
            }))?
        );
    } else {
        println!("marked {n} hold(s) as {word}");
    }
    Ok(ExitCode::SUCCESS)
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
    let kind = if !record.materialized && !record.is_deleted() {
        "meta"
    } else {
        kind_label(&record.content)
    };
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
        EngineError::Running { .. } => {
            eprintln!("hint: use `relay rescan` to scan while Relay is running");
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

    #[test]
    fn rescan_exit_code_distinguishes_mass_delete() {
        let item = |summary: &str| ActivityItem {
            at_ms: 0,
            kind: "scan_failed".to_owned(),
            summary: summary.to_owned(),
            detail: Some("S/m".to_owned()),
        };
        assert_eq!(rescan_exit_code(&[]), ExitCode::SUCCESS);
        assert_eq!(
            rescan_exit_code(&[item("S/m: mount marker missing at /x")]),
            ExitCode::from(1)
        );
        assert_eq!(
            rescan_exit_code(&[
                item("S/m: mount marker missing at /x"),
                item("S/n: refusing to delete 30 of 30 live entries"),
            ]),
            ExitCode::from(2)
        );
    }
}
