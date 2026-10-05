use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use relay_engine::{Engine, EntryContent};
use relay_ipc::{Client, HostState, TransferDirection};
use serde::Serialize;

const POLL: Duration = Duration::from_millis(50);
const SETTLE: Duration = Duration::from_millis(400);
const START_TIMEOUT: Duration = Duration::from_secs(15);
const PAIR_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
struct Lab {
    space: String,
    mount: String,
    mailbox: Option<PathBuf>,
    nodes: Vec<Node>,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
struct Node {
    name: String,
    home: PathBuf,
    mount_path: PathBuf,
    log: PathBuf,
    pid: u32,
    /// Start time of `pid` (Linux only), so `down` after a reboot does not
    /// kill whatever reused the number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started: Option<u64>,
}

impl Node {
    fn stall(&self) -> PathBuf {
        self.home
            .parent()
            .expect("node home has a parent")
            .join("stall")
    }

    fn stall_entered(&self) -> PathBuf {
        self.stall().with_extension("entered")
    }
}

pub fn lab_dir(flag: Option<PathBuf>) -> PathBuf {
    let path = flag
        .or_else(|| std::env::var_os("RELAY_SIM_LAB").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(".relay-sim"));
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

pub fn up(
    dir: &Path,
    json: bool,
    names: &[String],
    space: &str,
    mount: &str,
    mailbox: Option<String>,
) -> Result<()> {
    if names.len() < 2 {
        bail!("up needs at least two node names");
    }
    if names.iter().collect::<std::collections::HashSet<_>>().len() != names.len() {
        bail!("node names must be unique");
    }
    let state_path = dir.join("state.json");
    if state_path.exists() {
        bail!(
            "lab already exists at {}; run relay-sim down first",
            dir.display()
        );
    }
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());

    let result = start_lab(&dir, json, names, space, mount, mailbox);
    if result.is_err() {
        // Best effort: the daemons that did come up are detached, so nothing
        // else would stop them. The directory stays for a look at the logs.
        let _ = down(&dir, true);
    }
    result
}

fn start_lab(
    dir: &Path,
    json: bool,
    names: &[String],
    space: &str,
    mount: &str,
    mailbox: Option<String>,
) -> Result<()> {
    let mailbox = mailbox.map(|value| {
        let path = PathBuf::from(&value);
        if path.is_absolute() {
            path
        } else {
            dir.join(value)
        }
    });
    if let Some(path) = &mailbox {
        fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    }

    let mut lab = Lab {
        space: space.to_owned(),
        mount: mount.to_owned(),
        mailbox: mailbox.clone(),
        nodes: Vec::new(),
    };
    for name in names {
        let root = dir.join("nodes").join(name);
        lab.nodes.push(Node {
            name: name.clone(),
            home: root.join("home"),
            mount_path: root.join("mount"),
            log: root.join("daemon.log"),
            pid: 0,
            started: None,
        });
    }
    save(dir, &lab)?;

    for index in 0..lab.nodes.len() {
        let node = lab.nodes[index].clone();
        fs::create_dir_all(&node.home)?;
        fs::create_dir_all(&node.mount_path)?;
        {
            let mut engine = Engine::init(&node.home, &node.name)
                .with_context(|| format!("init {}", node.name))?;
            // Only the first node owns the space. The others join it after
            // pairing and attach their own mount to that same space id.
            if index == 0 {
                engine
                    .create_space(space)
                    .with_context(|| format!("space on {}", node.name))?;
                engine
                    .add_mount(space, mount, &node.mount_path, &[], &[])
                    .with_context(|| format!("mount on {}", node.name))?;
            }
            if let Some(path) = &mailbox {
                engine
                    .set_replica_path(path)
                    .with_context(|| format!("mailbox on {}", node.name))?;
            }
        }
        spawn_daemon(&mut lab.nodes[index])?;
        save(dir, &lab)?;
    }

    for node in &lab.nodes {
        wait_running(node)?;
    }
    pair_all(&lab)?;
    attach_guests(&lab)?;
    for node in &lab.nodes {
        wait_live_mount(node, mount)?;
    }

    if json {
        let nodes = lab
            .nodes
            .iter()
            .map(|node| {
                serde_json::json!({
                    "name": node.name,
                    "pid": node.pid,
                    "listen": listen_of(&node.home).unwrap_or_else(|_| "unknown".to_owned()),
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({"lab": dir, "nodes": nodes}))?
        );
    } else {
        println!("lab {}", dir.display());
        for node in &lab.nodes {
            let listen = listen_of(&node.home).unwrap_or_else(|_| "unknown".to_owned());
            println!("{} pid {} listen {listen}", node.name, node.pid);
        }
    }
    Ok(())
}

pub fn down(dir: &Path, keep: bool) -> Result<()> {
    let mut failures = Vec::new();
    if let Ok(lab) = load(dir) {
        for node in &lab.nodes {
            if let Err(err) = force_kill(node) {
                failures.push(format!("{}: {err:#}", node.name));
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while lab.nodes.iter().any(pid_alive) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }
    if !failures.is_empty() {
        // state.json stays: its pids are what a retry needs.
        bail!("could not stop every daemon: {}", failures.join("; "));
    }
    if !keep && dir.join("state.json").is_file() {
        fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

pub fn write(dir: &Path, name: &str, spec: &str, content: &str) -> Result<()> {
    let lab = load(dir)?;
    let node = node(&lab, name)?;
    let rel = mount_relative(&lab.mount, spec)?;
    let dest = node.mount_path.join(&rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&dest, content.as_bytes()).with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

pub fn kill(dir: &Path, name: &str) -> Result<()> {
    let lab = load(dir)?;
    let node = node(&lab, name)?;
    force_kill(node)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while pid_alive(node) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if pid_alive(node) {
        bail!("pid {} for {} is still running", node.pid, node.name);
    }
    Ok(())
}

pub fn start(dir: &Path, name: &str) -> Result<()> {
    let mut lab = load(dir)?;
    let existing = node(&lab, name)?;
    if pid_alive(existing) {
        bail!(
            "{} is already running as pid {}",
            existing.name,
            existing.pid
        );
    }
    spawn_daemon(node_mut(&mut lab, name)?)?;
    save(dir, &lab)?;
    let started = node(&lab, name)?;
    wait_running(started)?;
    Ok(())
}

pub fn stall(dir: &Path, name: &str) -> Result<()> {
    let lab = load(dir)?;
    let node = node(&lab, name)?;
    let flag = node.stall();
    if let Some(parent) = flag.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&flag, b"")?;
    Ok(())
}

pub fn unstall(dir: &Path, name: &str) -> Result<()> {
    let lab = load(dir)?;
    let node = node(&lab, name)?;
    let _ = fs::remove_file(node.stall());
    let _ = fs::remove_file(node.stall_entered());
    Ok(())
}

pub fn wait_stalled(dir: &Path, name: &str, timeout: &str) -> Result<()> {
    let lab = load(dir)?;
    let node = node(&lab, name)?;
    let timeout = parse_duration(timeout)?;
    let deadline = Instant::now() + timeout;
    let entered = node.stall_entered();
    while Instant::now() < deadline {
        if entered.is_file() {
            return Ok(());
        }
        if !pid_alive(node) {
            bail!(
                "{} exited before the mailbox stall\n{}",
                node.name,
                log_tail(&node.log, 40)
            );
        }
        thread::sleep(POLL);
    }
    bail!(
        "{} did not stall within {}s\n{}",
        node.name,
        timeout.as_secs(),
        log_tail(&node.log, 40)
    )
}

pub fn wait_converged(dir: &Path, timeout: &str, json: bool) -> Result<()> {
    let lab = load(dir)?;
    let timeout = parse_duration(timeout)?;
    let deadline = Instant::now() + timeout;
    let mut since_ok: Option<Instant> = None;
    let mut last = inspect(&lab)?;
    while Instant::now() < deadline {
        last = inspect(&lab)?;
        if last.ok {
            match since_ok {
                None => since_ok = Some(Instant::now()),
                Some(start) if start.elapsed() >= SETTLE => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string(&serde_json::json!({"converged": true}))?
                        );
                    }
                    return Ok(());
                }
                Some(_) => {}
            }
        } else {
            since_ok = None;
        }
        thread::sleep(POLL);
    }
    eprintln!("{}", serde_json::to_string_pretty(&last)?);
    bail!(
        "not converged after {}s: {}",
        timeout.as_secs(),
        last.pending.join("; ")
    )
}

pub fn report(dir: &Path) -> Result<()> {
    let lab = load(dir)?;
    let snap = inspect(&lab)?;
    println!("{}", serde_json::to_string_pretty(&snap)?);
    Ok(())
}

#[derive(Serialize)]
struct Inspection {
    ok: bool,
    pending: Vec<String>,
    nodes: Vec<NodeSnap>,
}

#[derive(Serialize)]
struct NodeSnap {
    name: String,
    pid: u32,
    running: bool,
    listen: Option<String>,
    state: Option<String>,
    idle: Option<IdleSnap>,
    connected: Vec<String>,
    transfers: Vec<String>,
    peers: Vec<String>,
    conflicts: Vec<String>,
    index_matches_disk: Option<bool>,
    tree: BTreeMap<String, String>,
    log_tail: String,
}

#[derive(Serialize)]
struct IdleSnap {
    quiet: bool,
    replica_behind: Option<u64>,
    ready: bool,
}

fn inspect(lab: &Lab) -> Result<Inspection> {
    let mut pending = Vec::new();
    let mut nodes = Vec::new();
    let mut trees = Vec::new();
    for node in &lab.nodes {
        let running = pid_alive(node);
        if !running {
            pending.push(format!("{} is not running", node.name));
        }
        let mut snap = NodeSnap {
            name: node.name.clone(),
            pid: node.pid,
            running,
            listen: None,
            state: None,
            idle: None,
            connected: Vec::new(),
            transfers: Vec::new(),
            peers: Vec::new(),
            conflicts: Vec::new(),
            index_matches_disk: None,
            tree: BTreeMap::new(),
            log_tail: log_tail(&node.log, 20),
        };
        if running
            && let Ok(Some(mut client)) = Client::connect(&node.home)
            && let Ok(status) = client.status()
        {
            snap.listen = status.listen;
            snap.state = Some(status.state.as_str().to_owned());
            snap.connected = status.peers.into_iter().map(|peer| peer.name).collect();
            snap.idle = Some(IdleSnap {
                quiet: status.idle.quiet,
                replica_behind: status.idle.replica_behind,
                ready: status.idle.ready(),
            });
            if status.state != HostState::Running {
                pending.push(format!("{} is {}", node.name, status.state.as_str()));
            }
            snap.transfers = status
                .transfers
                .iter()
                .map(|row| {
                    format!(
                        "{:?} {} files {}/{}",
                        row.direction,
                        row.peer_name,
                        row.files_done,
                        row.files_total
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "?".to_owned())
                    )
                })
                .collect();
            // A send can stay open after the peer already has the bytes (the
            // ack was lost because the process was killed). Receives and
            // index scans still mean the tree may change.
            let blocking: Vec<_> = status
                .transfers
                .iter()
                .filter(|row| row.direction != TransferDirection::Send)
                .map(|row| format!("{:?}", row.direction))
                .collect();
            if !blocking.is_empty() {
                pending.push(format!(
                    "{} has transfer(s): {}",
                    node.name,
                    snap.transfers.join(", ")
                ));
            }
            if status.idle.replica_behind.unwrap_or(0) > 0 {
                pending.push(format!(
                    "{} mailbox is {} behind",
                    node.name,
                    status.idle.replica_behind.unwrap_or(0)
                ));
            }
        } else if running {
            pending.push(format!("{} is not answering ipc", node.name));
        }

        let disk = disk_tree(&node.mount_path)?;
        snap.tree = disk
            .iter()
            .map(|(path, bytes)| (path.clone(), String::from_utf8_lossy(bytes).into_owned()))
            .collect();
        match index_tree(&node.home, &lab.space, &lab.mount) {
            Ok(index) => {
                let matches = index == disk;
                snap.index_matches_disk = Some(matches);
                if !matches {
                    pending.push(format!("{} disk does not match its index", node.name));
                }
            }
            Err(err) => pending.push(format!("{} index: {err}", node.name)),
        }
        if let Ok(engine) = Engine::open_read_only(&node.home) {
            if let Ok(peers) = engine.peers() {
                snap.peers = peers.into_iter().map(|peer| peer.name).collect();
            }
            if let Ok(conflicts) = engine.conflict_infos(Some(&lab.space)) {
                snap.conflicts = conflicts
                    .into_iter()
                    .map(|info| info.record.key.path.to_string())
                    .collect();
            }
        }
        trees.push(disk);
        nodes.push(snap);
    }
    if trees.len() == lab.nodes.len()
        && let Some(first) = trees.first()
        && trees.iter().any(|tree| tree != first)
    {
        pending.push("mount trees differ".to_owned());
    }
    Ok(Inspection {
        ok: pending.is_empty(),
        pending,
        nodes,
    })
}

fn pair_all(lab: &Lab) -> Result<()> {
    let host = &lab.nodes[0];
    for guest in lab.nodes.iter().skip(1) {
        let mut host_client = connect(&host.home, START_TIMEOUT).with_context(|| {
            format!(
                "connecting to {} to pair\n{}",
                host.name,
                log_tail(&host.log, 40)
            )
        })?;
        let started = host_client
            .pair_start(&relay_ipc::PairStartParams {
                share: vec![lab.space.clone()],
                allow_manage: false,
            })
            .with_context(|| format!("pair_start on {}", host.name))?;
        let listen = listen_of(&host.home)?;
        let mut guest_client = connect(&guest.home, START_TIMEOUT)?;
        guest_client
            .pair_join(&relay_ipc::PairJoinParams {
                code: started.code.clone(),
                addr: Some(listen.clone()),
                allow_manage: false,
            })
            .with_context(|| {
                format!(
                    "pair_join {} -> {} ({listen})\n{}\n{}",
                    guest.name,
                    host.name,
                    log_tail(&host.log, 20),
                    log_tail(&guest.log, 20)
                )
            })?;
    }
    let deadline = Instant::now() + PAIR_TIMEOUT;
    loop {
        if peers_linked(lab)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "peers did not show up on every node within {}s",
                PAIR_TIMEOUT.as_secs()
            );
        }
        thread::sleep(POLL);
    }
}

fn attach_guests(lab: &Lab) -> Result<()> {
    let host = &lab.nodes[0];
    for guest in lab.nodes.iter().skip(1) {
        wait_offer(&guest.home, &lab.space).with_context(|| {
            format!(
                "{} did not receive an offer for {}\n{}",
                guest.name,
                lab.space,
                log_tail(&guest.log, 40)
            )
        })?;
        let mut engine = Engine::open_for_config(&guest.home)
            .with_context(|| format!("configuring {}", guest.name))?;
        engine
            .join_space(&lab.space, &host.name)
            .with_context(|| format!("{} joining {}", guest.name, lab.space))?;
        engine
            .add_mount(&lab.space, &lab.mount, &guest.mount_path, &[], &[])
            .with_context(|| format!("mount on {}", guest.name))?;
    }
    Ok(())
}

fn wait_offer(home: &Path, space: &str) -> Result<()> {
    let deadline = Instant::now() + PAIR_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(engine) = Engine::open_read_only(home)
            && engine.offers()?.iter().any(|offer| offer.name == space)
        {
            return Ok(());
        }
        thread::sleep(POLL);
    }
    bail!("offer for {space} did not arrive")
}

fn wait_live_mount(node: &Node, mount: &str) -> Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if node.pid != 0 && !pid_alive(node) {
            bail!(
                "{} exited while attaching {mount}\n{}",
                node.name,
                log_tail(&node.log, 40)
            );
        }
        if let Ok(Some(mut client)) = Client::connect(&node.home)
            && let Ok(status) = client.status()
            && status.state == HostState::Running
            && status.mounts.iter().any(|live| live.mount == mount)
        {
            return Ok(());
        }
        thread::sleep(POLL);
    }
    bail!(
        "{} did not attach mount {mount}\n{}",
        node.name,
        log_tail(&node.log, 40)
    )
}

fn peers_linked(lab: &Lab) -> Result<bool> {
    for node in &lab.nodes {
        let engine =
            Engine::open_read_only(&node.home).with_context(|| format!("opening {}", node.name))?;
        let peers = engine.peers()?;
        for other in &lab.nodes {
            if other.name != node.name && !peers.iter().any(|peer| peer.name == other.name) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn wait_running(node: &Node) -> Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    let mut last = "starting".to_owned();
    while Instant::now() < deadline {
        if node.pid != 0 && !pid_alive(node) {
            bail!(
                "{} exited during startup\n{}",
                node.name,
                log_tail(&node.log, 40)
            );
        }
        if let Ok(Some(mut client)) = Client::connect(&node.home)
            && let Ok(status) = client.status()
        {
            last = status.state.as_str().to_owned();
            if status.state == HostState::Running
                && status
                    .listen
                    .as_ref()
                    .is_some_and(|listen| !listen.ends_with(":0"))
            {
                return Ok(());
            }
        }
        thread::sleep(POLL);
    }
    bail!(
        "{} did not become ready ({last})\n{}",
        node.name,
        log_tail(&node.log, 40)
    )
}

fn listen_of(home: &Path) -> Result<String> {
    let mut client = connect(home, START_TIMEOUT)?;
    let status = client.status()?;
    status
        .listen
        .filter(|listen| !listen.ends_with(":0"))
        .context("daemon has no listen address")
}

fn connect(home: &Path, timeout: Duration) -> Result<Client> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(mut client) = Client::connect(home)? {
            client.hello()?;
            return Ok(client);
        }
        if Instant::now() >= deadline {
            bail!("no Relay host at {}", home.display());
        }
        thread::sleep(POLL);
    }
}

fn spawn_daemon(node: &mut Node) -> Result<()> {
    if let Some(parent) = node.log.parent() {
        fs::create_dir_all(parent)?;
    }
    let log = File::options()
        .create(true)
        .append(true)
        .open(&node.log)
        .with_context(|| format!("opening {}", node.log.display()))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("daemon")
        .arg("--home")
        .arg(&node.home)
        .env("RELAY_SIM_STALL", node.stall())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    detach(&mut cmd);
    let child = cmd
        .spawn()
        .with_context(|| format!("starting daemon {}", node.name))?;
    node.pid = child.id();
    node.started = process_start_time(node.pid);
    Ok(())
}

fn detach(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
}

fn force_kill(node: &Node) -> Result<()> {
    if !pid_alive(node) {
        return Ok(());
    }
    let status = kill_command(node.pid)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() || !pid_alive(node) {
        return Ok(());
    }
    bail!("could not kill pid {}", node.pid)
}

fn kill_command(pid: u32) -> Command {
    #[cfg(unix)]
    {
        let mut cmd = Command::new("kill");
        cmd.args(["-9", &pid.to_string()]);
        cmd
    }
    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/F", "/PID", &pid.to_string()]);
        cmd
    }
}

/// Whether `node.pid` is still the daemon this lab started. On Linux the
/// start time recorded at spawn tells a reused pid apart; elsewhere a live
/// pid is taken to be ours.
fn pid_alive(node: &Node) -> bool {
    if node.pid == 0 || !os_pid_alive(node.pid) {
        return false;
    }
    match (node.started, process_start_time(node.pid)) {
        (Some(recorded), Some(current)) => recorded == current,
        _ => true,
    }
}

#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Option<u64> {
    // Field 22 of /proc/<pid>/stat, counted from the end of the
    // parenthesised command name because that name may hold spaces.
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after_comm) = stat.rsplit_once(')')?;
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn process_start_time(_pid: u32) -> Option<u64> {
    None
}

fn os_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .ok()
            .is_some_and(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
    }
}

fn disk_tree(root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    if root.is_dir() {
        walk_files(root, root, &mut out)?;
    }
    Ok(out)
}

fn walk_files(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".relay-") {
            continue;
        }
        if path.is_dir() {
            walk_files(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            out.insert(rel, bytes);
        }
    }
    Ok(())
}

fn index_tree(home: &Path, space: &str, mount: &str) -> Result<BTreeMap<String, Vec<u8>>> {
    let engine = Engine::open_read_only(home)?;
    let mut out = BTreeMap::new();
    for entry in engine.entries(space, mount, false)? {
        let EntryContent::File { object, .. } = &entry.content else {
            continue;
        };
        let bytes = engine
            .store()
            .read(object)
            .with_context(|| format!("object for {}", entry.key.path))?;
        out.insert(entry.key.path.to_string(), bytes);
    }
    Ok(out)
}

fn mount_relative(mount: &str, spec: &str) -> Result<PathBuf> {
    let (head, rel) = spec
        .split_once(['/', '\\'])
        .with_context(|| format!("path {spec:?} must look like {mount}/file"))?;
    if head != mount {
        bail!("this lab's mount is {mount}, not {head}");
    }
    let rel = PathBuf::from(rel);
    if rel.as_os_str().is_empty()
        || rel.is_absolute()
        || rel
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("path must stay inside the mount");
    }
    Ok(rel)
}

fn node<'a>(lab: &'a Lab, name: &str) -> Result<&'a Node> {
    lab.nodes
        .iter()
        .find(|node| node.name == name)
        .with_context(|| format!("no node {name:?} in this lab"))
}

fn node_mut<'a>(lab: &'a mut Lab, name: &str) -> Result<&'a mut Node> {
    lab.nodes
        .iter_mut()
        .find(|node| node.name == name)
        .with_context(|| format!("no node {name:?} in this lab"))
}

fn load(dir: &Path) -> Result<Lab> {
    let path = dir.join("state.json");
    let text = fs::read_to_string(&path)
        .with_context(|| format!("no lab at {} (relay-sim up)", dir.display()))?;
    serde_json::from_str(&text).context("reading lab state")
}

fn save(dir: &Path, lab: &Lab) -> Result<()> {
    let path = dir.join("state.json");
    let text = serde_json::to_string_pretty(lab)?;
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn log_tail(path: &Path, lines: usize) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return String::new();
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

fn parse_duration(text: &str) -> Result<Duration> {
    if let Some(ms) = text.strip_suffix("ms") {
        let n: u64 = ms
            .parse()
            .with_context(|| format!("timeout {text:?} is not a duration"))?;
        return Ok(Duration::from_millis(n));
    }
    let seconds = text.strip_suffix('s').unwrap_or(text);
    let n: u64 = seconds
        .parse()
        .with_context(|| format!("timeout {text:?} is not a duration (use 20s or 500ms)"))?;
    Ok(Duration::from_secs(n))
}
