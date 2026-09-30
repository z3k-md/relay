//! Local multi-process Relay lab.
//!
//! `relay-sim` starts one daemon process per node, each with its own home and
//! mount, and optionally one shared mailbox directory. Later commands talk to
//! those daemons over the local IPC socket. `wait-converged` is the stable
//! check: every node is idle, each index matches its disk, and the mount
//! trees match.

mod daemon;
mod lab;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "relay-sim", about = "Drive a local multi-process Relay lab")]
struct Cli {
    /// Lab directory (default: ./.relay-sim, or RELAY_SIM_LAB)
    #[arg(long, global = true)]
    lab: Option<PathBuf>,

    /// Print machine-readable JSON where a command has a result
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create homes, start daemons, and pair the nodes
    Up {
        /// Node names, used as device names. At least two.
        #[arg(required = true)]
        nodes: Vec<String>,
        /// Space created on every node
        #[arg(long, default_value = "sim")]
        space: String,
        /// Mount name. `write` paths look like `{mount}/file`.
        #[arg(long, default_value = "mods")]
        mount: String,
        /// Mailbox directory shared by every node. Relative paths live in the lab.
        #[arg(long)]
        mailbox: Option<String>,
    },
    /// Kill every daemon. Deletes the lab directory unless --keep.
    Down {
        /// Leave the lab directory in place
        #[arg(long)]
        keep: bool,
    },
    /// Write a file into one node's mount (`mount/relative/path`)
    Write {
        node: String,
        path: String,
        content: String,
    },
    /// Block until every node is idle and the mount trees match
    WaitConverged {
        /// `20s` or `500ms`
        #[arg(long, default_value = "20s")]
        timeout: String,
    },
    /// SIGKILL a node process (the mailbox crash window when stalled)
    Kill { node: String },
    /// Start a node whose process is not running
    Start { node: String },
    /// Hold this node's next mailbox push after the append, before the watermark
    Stall { node: String },
    /// Release a stall so a restarted node can finish the push
    Unstall { node: String },
    /// Block until the node has entered the mailbox stall
    WaitStalled {
        node: String,
        #[arg(long, default_value = "20s")]
        timeout: String,
    },
    /// JSON snapshot: trees, idle, conflicts, log tails
    Report,
    /// Internal: run one daemon. Invoked by `up` and `start`.
    #[command(hide = true)]
    Daemon {
        #[arg(long)]
        home: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(err) = dispatch(cli) {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Daemon { home } => daemon::run(home),
        command => {
            let dir = lab::lab_dir(cli.lab);
            match command {
                Command::Up {
                    nodes,
                    space,
                    mount,
                    mailbox,
                } => lab::up(&dir, cli.json, &nodes, &space, &mount, mailbox),
                Command::Down { keep } => lab::down(&dir, keep),
                Command::Write {
                    node,
                    path,
                    content,
                } => lab::write(&dir, &node, &path, &content),
                Command::WaitConverged { timeout } => lab::wait_converged(&dir, &timeout, cli.json),
                Command::Kill { node } => lab::kill(&dir, &node),
                Command::Start { node } => lab::start(&dir, &node),
                Command::Stall { node } => lab::stall(&dir, &node),
                Command::Unstall { node } => lab::unstall(&dir, &node),
                Command::WaitStalled { node, timeout } => lab::wait_stalled(&dir, &node, &timeout),
                Command::Report => lab::report(&dir),
                Command::Daemon { .. } => unreachable!("daemon is dispatched above"),
            }
        }
    }
}
