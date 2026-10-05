//! The simulator: every node's real engine and sync state machine in one
//! process, on one thread, on a virtual clock, joined by a virtual network.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::prelude::*;
use rand_chacha::ChaCha8Rng;
use relay_core::conflict::is_conflict_copy;
use relay_core::faults::{self, FaultPoint};
use relay_core::{DeleteHoldDecision, DeviceId, LogicalPath, ObjectId};
use relay_crypto::DeviceIdentity;
use relay_engine::{
    Engine, EngineError, EntryContent, ManualClock, ScanOptions, SpaceId, SyncEvent, SyncInput,
    SyncOutput, Syncer,
};
use relay_proto::frame;
use serde::Serialize;
use tempfile::TempDir;

use crate::model::Model;
use crate::scenario::{Scenario, Topology};
use crate::tree::{self, Node as TreeNode, Tree};

pub const SPACE: &str = "sim";
pub const MOUNT: &str = "tree";
const WALL_START_MS: i64 = 1_700_000_000_000;
/// How far virtual time advances between ticks while draining to quiet.
const QUIESCE_TICK_MS: u64 = 5_000;
/// Consecutive quiet ticks (no packets, no outputs, nothing dirty) before
/// the simulator calls the system converged. Covers the engine's 30 s
/// resync delay with room to spare.
const QUIET_TICKS: u32 = 10;
const MAX_QUIESCE_ROUNDS: u32 = 5_000;
const TRACE_TAIL: usize = 80;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    pub steps: u32,
    pub ops: u64,
    pub scans: u64,
    pub frames: u64,
    pub fetches: u64,
    pub fetch_faults: u64,
    /// Frames and transfers that were in flight when their link dropped.
    pub lost_in_flight: u64,
    /// Transfers among those, each reported to its requester as failed.
    pub transfers_cut: u64,
    /// Fetches a node issued over a session it had not yet noticed was dead.
    pub dead_link_fetches: u64,
    pub cuts: u64,
    pub crashes: u64,
    pub io_faults: u64,
    pub write_crashes: u64,
    pub quiesces: u64,
    pub conflict_copies: u64,
    pub merges_checked: u64,
    pub holds_applied: u64,
    pub warnings: u64,
    pub virtual_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunReport {
    pub scenario: String,
    pub seed: u64,
    pub stats: Stats,
    /// Hash of the full event trace. Two runs of one seed must agree.
    pub trace_digest: String,
    pub wall_ms: u128,
}

#[derive(Clone, Debug, Serialize)]
pub struct Failure {
    pub scenario: String,
    pub seed: u64,
    pub step: u32,
    pub message: String,
    pub trace_tail: Vec<String>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "scenario {} seed {} failed at step {}: {}",
            self.scenario, self.seed, self.step, self.message
        )?;
        writeln!(
            f,
            "reproduce: relay-vopr run --scenario {} --seed {} --trace",
            self.scenario, self.seed
        )?;
        writeln!(f, "last events:")?;
        for line in &self.trace_tail {
            writeln!(f, "  {line}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultKind {
    IoError,
    Crash,
}

#[derive(Default)]
struct FaultState {
    armed: Option<(FaultPoint, FaultKind)>,
    fired: Option<FaultKind>,
}

impl FaultState {
    fn hit(&mut self, point: FaultPoint) -> Option<io::Error> {
        let (at, kind) = self.armed?;
        if at != point {
            return None;
        }
        self.armed = None;
        self.fired = Some(kind);
        Some(match kind {
            FaultKind::IoError => io::Error::other("injected I/O error"),
            FaultKind::Crash => io::Error::other("injected crash before rename"),
        })
    }
}

enum FetchResult {
    Bytes(Vec<u8>),
    NotFound,
    Transient(String),
}

enum Payload {
    Frame(frame::Body),
    Fetched {
        object: ObjectId,
        result: FetchResult,
    },
    /// The transport finally noticed the session is gone.
    Disconnect,
}

struct Packet {
    from: usize,
    to: usize,
    payload: Payload,
}

struct Node {
    name: String,
    home: PathBuf,
    mount: PathBuf,
    device: DeviceId,
    clock: Arc<ManualClock>,
    skew_ms: i64,
    engine: Option<Engine>,
    syncer: Syncer,
    /// Working tree changed since the last scan.
    dirty: bool,
}

impl Node {
    fn up(&self) -> bool {
        self.engine.is_some()
    }
}

pub struct Simulator<'a> {
    scenario: &'a Scenario,
    seed: u64,
    rng: ChaCha8Rng,
    base: Instant,
    now_ms: u64,
    nodes: Vec<Node>,
    adjacent: BTreeSet<(usize, usize)>,
    sessions: BTreeSet<(usize, usize)>,
    /// `(node, peer)`: the session is gone but `node` has not been told yet.
    stale: BTreeSet<(usize, usize)>,
    cut: BTreeSet<(usize, usize)>,
    packets: BTreeMap<(u64, u64), Packet>,
    next_seq: u64,
    /// Last scheduled delivery per directed link, so a link stays FIFO.
    link_last: BTreeMap<(usize, usize), u64>,
    model: Model,
    faults: Rc<RefCell<FaultState>>,
    faults_enabled: bool,
    trace: Vec<String>,
    digest: blake3::Hasher,
    pub stats: Stats,
    /// Wall time spent inside engine calls, and in the simulator's own tree
    /// walks, for `--trace` output.
    engine_time: Duration,
    call_time: BTreeMap<String, (u64, Duration)>,
    walk_time: Duration,
    name_counter: u64,
    step: u32,
    verbose: bool,
    // Dropped last: engines hold files under it.
    _root: TempDir,
}

/// Where a run keeps its homes and mounts. `RELAY_VOPR_TMP` wins; otherwise
/// a RAM-backed `/dev/shm` when present, since every engine write is fsynced
/// and that dominates the run time on a real disk; otherwise the OS temp dir.
fn scratch_dir() -> io::Result<TempDir> {
    if let Some(dir) = std::env::var_os("RELAY_VOPR_TMP") {
        return tempfile::Builder::new().prefix("vopr-").tempdir_in(dir);
    }
    let shm = Path::new("/dev/shm");
    if shm.is_dir()
        && let Ok(dir) = tempfile::Builder::new().prefix("vopr-").tempdir_in(shm)
    {
        return Ok(dir);
    }
    tempfile::Builder::new().prefix("vopr-").tempdir()
}

fn pair(a: usize, b: usize) -> (usize, usize) {
    if a < b { (a, b) } else { (b, a) }
}

fn engine_error(err: EngineError) -> String {
    err.to_string()
}

impl<'a> Simulator<'a> {
    pub fn new(scenario: &'a Scenario, seed: u64, verbose: bool) -> Result<Self, Failure> {
        let root = scratch_dir().map_err(|e| Failure {
            scenario: scenario.name.into(),
            seed,
            step: 0,
            message: format!("temp dir: {e}"),
            trace_tail: Vec::new(),
        })?;
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut nodes = Vec::new();
        for i in 0..scenario.nodes {
            let name = format!("node{i}");
            let dir = root.path().join(&name);
            let home = dir.join("home");
            let mount = dir.join("mount");
            fs::create_dir_all(&mount).expect("create mount dir");
            let mut key_seed = [0u8; 32];
            rng.fill_bytes(&mut key_seed);
            let identity = DeviceIdentity::generate_from_seed(&home.join("identity"), &key_seed)
                .expect("seeded identity");
            let skew_ms = if scenario.clock_skew_ms > 0 {
                rng.gen_range(-scenario.clock_skew_ms..=scenario.clock_skew_ms)
            } else {
                0
            };
            let clock = Arc::new(ManualClock::new(WALL_START_MS + skew_ms));
            nodes.push(Node {
                name,
                home,
                mount,
                device: identity.device_id(),
                clock,
                skew_ms,
                engine: None,
                syncer: Syncer::new(),
                dirty: false,
            });
        }
        let mut adjacent = BTreeSet::new();
        for a in 0..scenario.nodes {
            for b in (a + 1)..scenario.nodes {
                let linked = match scenario.topology {
                    Topology::Mesh => true,
                    Topology::Star => a == 0,
                    Topology::Chain => b == a + 1,
                };
                if linked {
                    adjacent.insert((a, b));
                }
            }
        }
        let faults = Rc::new(RefCell::new(FaultState::default()));
        {
            let hook = Rc::clone(&faults);
            faults::install(Box::new(move |point, _path| hook.borrow_mut().hit(point)));
        }
        let mut sim = Self {
            scenario,
            seed,
            rng,
            base: Instant::now(),
            now_ms: 0,
            nodes,
            adjacent,
            sessions: BTreeSet::new(),
            stale: BTreeSet::new(),
            cut: BTreeSet::new(),
            packets: BTreeMap::new(),
            next_seq: 0,
            link_last: BTreeMap::new(),
            model: Model::new(scenario.nodes),
            faults,
            faults_enabled: false,
            trace: Vec::new(),
            digest: blake3::Hasher::new(),
            stats: Stats::default(),
            engine_time: Duration::ZERO,
            call_time: BTreeMap::new(),
            walk_time: Duration::ZERO,
            name_counter: 0,
            step: 0,
            verbose,
            _root: root,
        };
        sim.setup()?;
        Ok(sim)
    }

    // ----- bookkeeping -----

    fn trace(&mut self, line: impl Into<String>) {
        let mut line = line.into();
        // Engine messages can name files under the run's temp dir; keep the
        // trace, and so its digest, free of that per-run path.
        let root = self._root.path().to_string_lossy().into_owned();
        if line.contains(&root) {
            line = line.replace(&root, "<root>");
        }
        self.digest.update(line.as_bytes());
        self.digest.update(b"\n");
        if self.verbose {
            eprintln!("[t={}ms step={}] {}", self.now_ms, self.step, line);
        }
        self.trace.push(line);
    }

    fn fail(&self, message: String) -> Failure {
        let tail = self.trace.len().saturating_sub(TRACE_TAIL);
        Failure {
            scenario: self.scenario.name.into(),
            seed: self.seed,
            step: self.step,
            message,
            trace_tail: self.trace[tail..].to_vec(),
        }
    }

    fn now_instant(&self) -> Instant {
        self.base + Duration::from_millis(self.now_ms)
    }

    fn index_of(&self, device: DeviceId) -> usize {
        self.nodes
            .iter()
            .position(|n| n.device == device)
            .expect("output names a known device")
    }

    fn fresh_engine(&self, node: usize, init: bool) -> Result<Engine, EngineError> {
        let n = &self.nodes[node];
        let engine = if init {
            Engine::init(&n.home, &n.name)?
        } else {
            Engine::open(&n.home)?
        };
        Ok(engine.with_clock(n.clock.clone()))
    }

    fn set_clocks(&mut self, node: usize) {
        let n = &mut self.nodes[node];
        n.clock.set(WALL_START_MS + self.now_ms as i64 + n.skew_ms);
        n.syncer
            .set_now(self.base + Duration::from_millis(self.now_ms));
    }

    /// Decide whether this engine call hits an injected fault.
    fn arm_fault(&mut self) {
        let mut state = self.faults.borrow_mut();
        state.armed = None;
        state.fired = None;
        if !self.faults_enabled {
            return;
        }
        let profile = &self.scenario.faults;
        let roll: f64 = self.rng.r#gen();
        if roll < profile.io_error_rate {
            let point = match self.rng.gen_range(0..3) {
                0 => FaultPoint::MaterializeWrite,
                1 => FaultPoint::MaterializeRename,
                _ => FaultPoint::StorePut,
            };
            state.armed = Some((point, FaultKind::IoError));
        } else if roll < profile.io_error_rate + profile.crash_on_write_rate {
            state.armed = Some((FaultPoint::MaterializeRename, FaultKind::Crash));
        }
    }

    /// What fired during the last armed call, and disarm.
    fn take_fault(&mut self) -> Option<FaultKind> {
        let mut state = self.faults.borrow_mut();
        state.armed = None;
        state.fired.take()
    }

    // ----- engine calls -----

    /// Run one engine/syncer call on `node`, then route its outputs. Mirrors
    /// the watch loop: after any input the engine offers new local
    /// sequences to every connected peer.
    fn call(
        &mut self,
        node: usize,
        what: &str,
        f: impl FnOnce(
            &mut Engine,
            &mut Syncer,
            &mut dyn FnMut(SyncOutput),
        ) -> Result<Vec<SyncEvent>, EngineError>,
    ) -> Result<(), Failure> {
        if !self.nodes[node].up() {
            return Ok(());
        }
        self.set_clocks(node);
        self.arm_fault();
        let mut outs = Vec::new();
        let started = Instant::now();
        let result = {
            let n = &mut self.nodes[node];
            let engine = n.engine.as_mut().expect("node is up");
            f(engine, &mut n.syncer, &mut |o| outs.push(o)).and_then(|mut events| {
                let more = n.syncer.push_local_changes(engine, &mut |o| outs.push(o))?;
                events.extend(more);
                Ok(events)
            })
        };
        let elapsed = started.elapsed();
        self.engine_time += elapsed;
        if self.verbose {
            let kind = what.split(" from ").next().unwrap_or(what).to_owned();
            let slot = self.call_time.entry(kind).or_default();
            slot.0 += 1;
            slot.1 += elapsed;
        }
        let fired = self.take_fault();
        match fired {
            Some(FaultKind::IoError) => {
                self.stats.io_faults += 1;
                self.trace(format!(
                    "{}: injected I/O error during {what}",
                    self.nodes[node].name
                ));
            }
            Some(FaultKind::Crash) => {
                self.stats.write_crashes += 1;
                self.trace(format!(
                    "{}: crashed between temp write and rename during {what}",
                    self.nodes[node].name
                ));
            }
            None => {}
        }
        let events = match result {
            Ok(events) => events,
            Err(err) => {
                // Engine errors here are either an injected fault surfacing
                // or a bug. Faults are expected to be reported as
                // warnings/retries, not errors; record which it was.
                if fired.is_some() {
                    self.trace(format!(
                        "{}: {what} returned error after fault: {}",
                        self.nodes[node].name,
                        engine_error(err)
                    ));
                    Vec::new()
                } else {
                    return Err(self.fail(format!(
                        "{}: {what} failed: {}",
                        self.nodes[node].name,
                        engine_error(err)
                    )));
                }
            }
        };
        if fired == Some(FaultKind::Crash) {
            self.crash(node)?;
            return Ok(());
        }
        self.on_events(node, events)?;
        self.route(node, outs)
    }

    fn on_events(&mut self, node: usize, events: Vec<SyncEvent>) -> Result<(), Failure> {
        let mut holds = Vec::new();
        for event in events {
            match event {
                SyncEvent::SyncWarning { path, reason, .. } => {
                    self.stats.warnings += 1;
                    self.trace(format!(
                        "{}: warning {path}: {reason}",
                        self.nodes[node].name
                    ));
                }
                SyncEvent::RemoteApplied {
                    peer,
                    written,
                    deleted,
                    conflicts,
                    skipped,
                    ..
                } => {
                    let from = self.index_of(peer);
                    self.trace(format!(
                        "{}: applied batch from {}: written {written} deleted {deleted} conflicts {conflicts} skipped {skipped}",
                        self.nodes[node].name, self.nodes[from].name
                    ));
                    if skipped > 0 {
                        self.stats.warnings += 1;
                    }
                    // The daemon's watcher sees the engine's own writes
                    // (parents created, files replaced) and schedules a scan.
                    if written > 0 || deleted > 0 {
                        self.nodes[node].dirty = true;
                    }
                }
                SyncEvent::DeletesHeld {
                    peer,
                    space,
                    mount,
                    deletions,
                    live,
                } => {
                    let from = self.index_of(peer);
                    self.trace(format!(
                        "{}: held {deletions} deletes ({live} live) from {}; applying",
                        self.nodes[node].name, self.nodes[from].name
                    ));
                    holds.push((space, mount, self.nodes[from].name.clone()));
                }
                SyncEvent::PeerConnected { .. }
                | SyncEvent::PeerDisconnected { .. }
                | SyncEvent::OffersReceived { .. }
                | SyncEvent::SentChanges { .. }
                | SyncEvent::Transfers(_) => {}
            }
        }
        for (space, mount, peer_name) in holds {
            // A real user confirms in the UI; the simulator always lets the
            // deletes through, then resumes the held batch.
            let space_id = self.space_id(node);
            self.stats.holds_applied += 1;
            self.call(node, "decide delete hold", |engine, syncer, out| {
                engine.decide_delete_hold(
                    &space,
                    Some(&mount),
                    Some(&peer_name),
                    DeleteHoldDecision::Apply,
                )?;
                syncer.resume_held(engine, space_id, out)
            })?;
        }
        Ok(())
    }

    fn space_id(&self, node: usize) -> SpaceId {
        self.nodes[node]
            .engine
            .as_ref()
            .expect("node is up")
            .spaces()
            .expect("list spaces")
            .into_iter()
            .find(|s| s.name == SPACE)
            .expect("space exists")
            .id
    }

    /// Hand outputs to the network. Outputs are stably sorted by peer so the
    /// engine's hash-map iteration order cannot leak into the schedule; the
    /// order of frames to one peer is preserved.
    fn route(&mut self, from: usize, mut outs: Vec<SyncOutput>) -> Result<(), Failure> {
        let key = |o: &SyncOutput| match o {
            SyncOutput::Send { peer, .. } => (0u8, *peer, None),
            SyncOutput::FetchObject { peer, object } => (1, *peer, Some(*object)),
            SyncOutput::SetPeers => (2, DeviceId::from_bytes([0; 32]), None),
            SyncOutput::SetRelay(_) => (3, DeviceId::from_bytes([0; 32]), None),
        };
        outs.sort_by_key(key);
        for out in outs {
            match out {
                SyncOutput::Send { peer, body } => {
                    let to = self.index_of(peer);
                    if !self.sessions.contains(&pair(from, to)) {
                        continue;
                    }
                    self.stats.frames += 1;
                    let delay = self.frame_latency();
                    self.send(from, to, delay, Payload::Frame(body));
                }
                SyncOutput::FetchObject { peer, object } => {
                    let to = self.index_of(peer);
                    self.serve_fetch(from, to, object);
                }
                SyncOutput::SetPeers | SyncOutput::SetRelay(_) => {}
            }
        }
        Ok(())
    }

    fn frame_latency(&mut self) -> u64 {
        let (lo, hi) = self.scenario.net.latency_ms;
        self.rng.gen_range(lo..=hi.max(lo))
    }

    fn send(&mut self, from: usize, to: usize, delay_ms: u64, payload: Payload) {
        let earliest = self.link_last.get(&(from, to)).copied().unwrap_or(0);
        let at = (self.now_ms + delay_ms).max(earliest);
        self.link_last.insert((from, to), at);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.packets.insert((at, seq), Packet { from, to, payload });
    }

    /// `requester` asked `server` for `object`: read it from the server's
    /// store now (as the real transport would) and schedule the reply.
    fn serve_fetch(&mut self, requester: usize, server: usize, object: ObjectId) {
        if !self.sessions.contains(&pair(requester, server)) {
            if self.stale.contains(&(requester, server)) {
                // Asked over a connection that is already dead: the
                // transport reports the failure once it gives up.
                self.stats.fetches += 1;
                self.stats.fetch_faults += 1;
                self.stats.dead_link_fetches += 1;
                let delay = self.frame_latency() + self.rng.gen_range(500..=5_000);
                self.send(
                    server,
                    requester,
                    delay,
                    Payload::Fetched {
                        object,
                        result: FetchResult::Transient("connection lost".into()),
                    },
                );
            }
            return;
        }
        self.stats.fetches += 1;
        let net = &self.scenario.net;
        let roll: f64 = self.rng.r#gen();
        let result = if roll < net.fetch_fail_rate {
            self.stats.fetch_faults += 1;
            FetchResult::Transient("injected transfer failure".into())
        } else if roll < net.fetch_fail_rate + net.fetch_not_found_rate {
            self.stats.fetch_faults += 1;
            FetchResult::NotFound
        } else {
            let store = self.nodes[server]
                .engine
                .as_ref()
                .expect("server is up")
                .store();
            if store.contains(&object) {
                match store.read(&object) {
                    Ok(bytes) => FetchResult::Bytes(bytes),
                    Err(err) => FetchResult::Transient(err.to_string()),
                }
            } else {
                FetchResult::NotFound
            }
        };
        let size = match &result {
            FetchResult::Bytes(b) => b.len() as u64,
            _ => 0,
        };
        let delay = self.frame_latency() + size / net.bytes_per_ms.max(1);
        self.send(
            server,
            requester,
            delay,
            Payload::Fetched { object, result },
        );
    }

    fn deliver(&mut self, packet: Packet) -> Result<(), Failure> {
        let Packet { from, to, payload } = packet;
        if !self.nodes[to].up() {
            return Ok(());
        }
        let peer = self.nodes[from].device;
        if let Payload::Disconnect = payload {
            if self.stale.remove(&(to, from)) {
                self.trace(format!(
                    "{} noticed {} is gone",
                    self.nodes[to].name, self.nodes[from].name
                ));
                self.call(to, "peer disconnected", |engine, syncer, out| {
                    syncer.handle(engine, SyncInput::PeerDisconnected { peer }, out)
                })?;
            }
            return Ok(());
        }
        let live = self.sessions.contains(&pair(from, to));
        let failed_fetch = matches!(
            &payload,
            Payload::Fetched {
                result: FetchResult::Transient(_),
                ..
            }
        );
        // Only a failure report crosses a dead connection the receiver still
        // believes in.
        if !live && !(failed_fetch && self.stale.contains(&(to, from))) {
            return Ok(());
        }
        match payload {
            Payload::Disconnect => Ok(()),
            Payload::Frame(body) => {
                let what = format!("frame {} from {}", frame_name(&body), self.nodes[from].name);
                self.call(to, &what, |engine, syncer, out| {
                    syncer.handle(engine, SyncInput::Frame { peer, body }, out)
                })
            }
            Payload::Fetched { object, result } => {
                let input = match result {
                    FetchResult::Bytes(bytes) => {
                        // The transport verifies and installs the object
                        // before telling the engine.
                        self.set_clocks(to);
                        self.arm_fault();
                        let put = self.nodes[to]
                            .engine
                            .as_ref()
                            .expect("node is up")
                            .store()
                            .put_bytes(&bytes);
                        let fired = self.take_fault();
                        if fired.is_some() {
                            self.stats.io_faults += 1;
                            self.trace(format!(
                                "{}: injected I/O error installing object",
                                self.nodes[to].name
                            ));
                        }
                        match put {
                            Ok(_) => SyncInput::ObjectFetched { peer, object },
                            Err(err) => SyncInput::ObjectFetchFailed {
                                peer,
                                object,
                                not_found: false,
                                reason: err.to_string(),
                            },
                        }
                    }
                    FetchResult::NotFound => SyncInput::ObjectFetchFailed {
                        peer,
                        object,
                        not_found: true,
                        reason: "not found".into(),
                    },
                    FetchResult::Transient(reason) => SyncInput::ObjectFetchFailed {
                        peer,
                        object,
                        not_found: false,
                        reason,
                    },
                };
                let what = format!("object result from {}", self.nodes[from].name);
                self.call(to, &what, |engine, syncer, out| {
                    syncer.handle(engine, input, out)
                })
            }
        }
    }

    // ----- sessions and links -----

    fn try_connect(&mut self, a: usize, b: usize) -> Result<(), Failure> {
        let key = pair(a, b);
        if !self.adjacent.contains(&key)
            || self.cut.contains(&key)
            || self.sessions.contains(&key)
            || !self.nodes[key.0].up()
            || !self.nodes[key.1].up()
        {
            return Ok(());
        }
        // A new connection supersedes a stale one: the transport reports the
        // old session gone before it reports the new one up.
        for (me, other) in [(key.0, key.1), (key.1, key.0)] {
            if self.stale.remove(&(me, other)) {
                self.packets.retain(|_, p| {
                    !(matches!(p.payload, Payload::Disconnect) && p.to == me && p.from == other)
                });
                let peer = self.nodes[other].device;
                self.call(me, "peer disconnected", |engine, syncer, out| {
                    syncer.handle(engine, SyncInput::PeerDisconnected { peer }, out)
                })?;
            }
        }
        self.sessions.insert(key);
        self.trace(format!(
            "session {} <-> {} up",
            self.nodes[key.0].name, self.nodes[key.1].name
        ));
        for (me, other) in [(key.0, key.1), (key.1, key.0)] {
            let peer = self.nodes[other].device;
            let name = self.nodes[other].name.clone();
            self.call(me, "peer connected", |engine, syncer, out| {
                syncer.handle(engine, SyncInput::PeerConnected { peer, name }, out)
            })?;
        }
        Ok(())
    }

    fn disconnect(&mut self, a: usize, b: usize, why: &str) -> Result<(), Failure> {
        let key = pair(a, b);
        if !self.sessions.remove(&key) {
            return Ok(());
        }
        self.trace(format!(
            "session {} <-> {} down ({why})",
            self.nodes[key.0].name, self.nodes[key.1].name
        ));
        // Everything in flight on the link is lost. A transfer that was
        // under way fails instead: the transport reports it, before or
        // after it reports the session gone.
        let mut in_flight = Vec::new();
        let before = self.packets.len();
        self.packets.retain(|_, p| {
            if pair(p.from, p.to) != key {
                return true;
            }
            if let Payload::Fetched { object, .. } = &p.payload {
                in_flight.push((p.to, *object));
            }
            false
        });
        self.stats.lost_in_flight += (before - self.packets.len()) as u64;
        self.stats.transfers_cut += in_flight.len() as u64;
        self.link_last.remove(&(key.0, key.1));
        self.link_last.remove(&(key.1, key.0));
        let (lo, hi) = self.scenario.net.notice_delay_ms;
        for (me, other) in [(key.0, key.1), (key.1, key.0)] {
            if !self.nodes[me].up() {
                continue;
            }
            let delay = if hi > 0 {
                self.rng.gen_range(lo..=hi.max(lo))
            } else {
                0
            };
            let mine: Vec<ObjectId> = in_flight
                .iter()
                .filter(|(to, _)| *to == me)
                .map(|(_, object)| *object)
                .collect();
            if delay > 0 {
                // `me` keeps believing in the session until the transport
                // gives up on it; what it sends meanwhile is lost.
                self.stale.insert((me, other));
                for object in mine {
                    let at = self.rng.gen_range(0..=delay);
                    self.send(
                        other,
                        me,
                        at,
                        Payload::Fetched {
                            object,
                            result: FetchResult::Transient("connection lost".into()),
                        },
                    );
                }
                self.send(other, me, delay, Payload::Disconnect);
                continue;
            }
            let peer = self.nodes[other].device;
            let failures_first = self.rng.r#gen::<bool>();
            if failures_first {
                self.fail_in_flight(me, peer, &mine)?;
            }
            self.call(me, "peer disconnected", |engine, syncer, out| {
                syncer.handle(engine, SyncInput::PeerDisconnected { peer }, out)
            })?;
            if !failures_first {
                self.fail_in_flight(me, peer, &mine)?;
            }
        }
        Ok(())
    }

    fn fail_in_flight(
        &mut self,
        me: usize,
        peer: DeviceId,
        objects: &[ObjectId],
    ) -> Result<(), Failure> {
        for &object in objects {
            self.call(
                me,
                "transfer failed: connection lost",
                |engine, syncer, out| {
                    syncer.handle(
                        engine,
                        SyncInput::ObjectFetchFailed {
                            peer,
                            object,
                            not_found: false,
                            reason: "connection lost".into(),
                        },
                        out,
                    )
                },
            )?;
        }
        Ok(())
    }

    fn connect_all(&mut self) -> Result<(), Failure> {
        let edges: Vec<_> = self.adjacent.iter().copied().collect();
        for (a, b) in edges {
            self.try_connect(a, b)?;
        }
        Ok(())
    }

    fn crash(&mut self, node: usize) -> Result<(), Failure> {
        if !self.nodes[node].up() {
            return Ok(());
        }
        self.stats.crashes += 1;
        self.trace(format!("{} crashed", self.nodes[node].name));
        self.nodes[node].engine = None;
        self.nodes[node].syncer = Syncer::new();
        self.stale.retain(|(me, _)| *me != node);
        let sessions: Vec<_> = self
            .sessions
            .iter()
            .copied()
            .filter(|k| k.0 == node || k.1 == node)
            .collect();
        for (a, b) in sessions {
            self.disconnect(a, b, "crash")?;
        }
        // Peers' delayed disconnect notices about this node stay queued.
        self.packets.retain(|_, p| {
            p.to != node && (p.from != node || matches!(p.payload, Payload::Disconnect))
        });
        Ok(())
    }

    fn restart(&mut self, node: usize) -> Result<(), Failure> {
        if self.nodes[node].up() {
            return Ok(());
        }
        let engine = self.fresh_engine(node, false).map_err(|e| {
            self.fail(format!(
                "{}: reopen after crash: {e}",
                self.nodes[node].name
            ))
        })?;
        self.nodes[node].engine = Some(engine);
        self.nodes[node].syncer = self.new_syncer();
        // The daemon scans every mount on start.
        self.nodes[node].dirty = true;
        self.trace(format!("{} restarted", self.nodes[node].name));
        let edges: Vec<_> = self
            .adjacent
            .iter()
            .copied()
            .filter(|k| k.0 == node || k.1 == node)
            .collect();
        for (a, b) in edges {
            self.try_connect(a, b)?;
        }
        Ok(())
    }

    fn new_syncer(&self) -> Syncer {
        match self.scenario.index_batch_entries {
            Some(n) => Syncer::with_index_batch_entries(n),
            None => Syncer::new(),
        }
    }

    fn cut_link(&mut self, a: usize, b: usize) -> Result<(), Failure> {
        let key = pair(a, b);
        if !self.cut.insert(key) {
            return Ok(());
        }
        self.stats.cuts += 1;
        self.trace(format!(
            "link {} <-> {} cut",
            self.nodes[key.0].name, self.nodes[key.1].name
        ));
        self.disconnect(key.0, key.1, "link cut")
    }

    fn heal_link(&mut self, a: usize, b: usize) -> Result<(), Failure> {
        let key = pair(a, b);
        if !self.cut.remove(&key) {
            return Ok(());
        }
        self.trace(format!(
            "link {} <-> {} healed",
            self.nodes[key.0].name, self.nodes[key.1].name
        ));
        self.try_connect(key.0, key.1)
    }

    // ----- setup -----

    fn setup(&mut self) -> Result<(), Failure> {
        for i in 0..self.nodes.len() {
            let engine = self
                .fresh_engine(i, true)
                .map_err(|e| self.fail(format!("init node{i}: {e}")))?;
            self.nodes[i].engine = Some(engine);
            self.nodes[i].syncer = self.new_syncer();
        }
        let edges: Vec<_> = self.adjacent.iter().copied().collect();
        for (a, b) in &edges {
            for (me, other) in [(*a, *b), (*b, *a)] {
                let name = self.nodes[other].name.clone();
                let id = self.nodes[other].device;
                self.nodes[me]
                    .engine
                    .as_mut()
                    .expect("up")
                    .add_peer(&name, id, &["127.0.0.1:1".into()])
                    .map_err(|e| self.fail(format!("add_peer: {e}")))?;
            }
        }
        // Node 0 owns the space; every other node joins from its BFS parent.
        {
            let mount = self.nodes[0].mount.clone();
            let engine = self.nodes[0].engine.as_mut().expect("up");
            let result = engine
                .create_space(SPACE)
                .map_err(|e| format!("create_space: {e}"))
                .and_then(|_| {
                    engine
                        .add_mount(SPACE, MOUNT, &mount, &[], &[])
                        .map_err(|e| format!("add_mount: {e}"))
                });
            result.map_err(|m| self.fail(m))?;
        }
        self.share_with_neighbours(0)?;
        let mut joined = BTreeSet::from([0usize]);
        let mut queue = VecDeque::from([0usize]);
        while let Some(parent) = queue.pop_front() {
            let children: Vec<usize> = edges
                .iter()
                .filter_map(|(a, b)| {
                    if *a == parent {
                        Some(*b)
                    } else if *b == parent {
                        Some(*a)
                    } else {
                        None
                    }
                })
                .filter(|c| !joined.contains(c))
                .collect();
            for child in children {
                joined.insert(child);
                self.try_connect(parent, child)?;
                self.settle()?;
                let parent_name = self.nodes[parent].name.clone();
                let mount = self.nodes[child].mount.clone();
                {
                    let engine = self.nodes[child].engine.as_mut().expect("up");
                    let result = engine
                        .join_space(SPACE, &parent_name)
                        .map_err(|e| format!("node{child} join from {parent_name}: {e}"))
                        .and_then(|_| {
                            engine
                                .add_mount(SPACE, MOUNT, &mount, &[], &[])
                                .map_err(|e| format!("node{child} add_mount: {e}"))
                        });
                    result.map_err(|m| self.fail(m))?;
                }
                self.share_with_neighbours(child)?;
                // Reconnect so the new mount and shares are in the offers.
                self.disconnect(parent, child, "setup")?;
                self.nodes[child].syncer = self.new_syncer();
                self.try_connect(parent, child)?;
                self.settle()?;
                queue.push_back(child);
            }
        }
        // Fresh sessions everywhere now that every node has joined.
        let sessions: Vec<_> = self.sessions.iter().copied().collect();
        for (a, b) in sessions {
            self.disconnect(a, b, "setup")?;
        }
        for i in 0..self.nodes.len() {
            self.nodes[i].syncer = self.new_syncer();
        }
        self.connect_all()?;
        self.settle()?;
        let ids: Vec<String> = self
            .nodes
            .iter()
            .map(|n| format!("{}={}", n.name, n.device.short()))
            .collect();
        self.trace(format!("setup complete: {}", ids.join(" ")));
        self.faults_enabled = true;
        Ok(())
    }

    fn share_with_neighbours(&mut self, node: usize) -> Result<(), Failure> {
        let others: Vec<usize> = self
            .adjacent
            .iter()
            .filter_map(|(a, b)| {
                if *a == node {
                    Some(*b)
                } else if *b == node {
                    Some(*a)
                } else {
                    None
                }
            })
            .collect();
        for other in others {
            let name = self.nodes[other].name.clone();
            self.nodes[node]
                .engine
                .as_mut()
                .expect("up")
                .share(SPACE, &name)
                .map_err(|e| {
                    self.fail(format!("{} share with {name}: {e}", self.nodes[node].name))
                })?;
        }
        Ok(())
    }

    /// Deliver every packet in order, advancing the clock as needed.
    fn settle(&mut self) -> Result<(), Failure> {
        let mut guard = 0u32;
        while let Some((&(at, seq), _)) = self.packets.iter().next() {
            guard += 1;
            if guard > 200_000 {
                return Err(self.fail("network never went quiet".into()));
            }
            let packet = self.packets.remove(&(at, seq)).expect("present");
            self.now_ms = self.now_ms.max(at);
            self.deliver(packet)?;
        }
        Ok(())
    }

    // ----- the step loop -----

    pub fn run(&mut self) -> Result<(), Failure> {
        let steps = self.scenario.steps;
        for _ in 0..steps {
            self.step()?;
        }
        self.quiesce()?;
        self.stats.steps = steps;
        self.stats.virtual_ms = self.now_ms;
        if self.verbose {
            eprintln!(
                "time: engine {:?}, simulator tree walks {:?}",
                self.engine_time, self.walk_time
            );
            for (kind, (count, total)) in &self.call_time {
                eprintln!("  {kind}: {count} calls, {total:?}");
            }
        }
        Ok(())
    }

    pub fn report(&self, wall: Duration) -> RunReport {
        RunReport {
            scenario: self.scenario.name.into(),
            seed: self.seed,
            stats: self.stats.clone(),
            trace_digest: self.digest.finalize().to_hex().to_string(),
            wall_ms: wall.as_millis(),
        }
    }

    fn step(&mut self) -> Result<(), Failure> {
        self.step += 1;
        self.fault_events()?;
        let w = &self.scenario.workload;
        let total = w.op_weight + w.scan_weight + w.deliver_weight + w.tick_weight;
        let roll = self.rng.gen_range(0..total);
        if roll < w.op_weight {
            self.workload_op()?;
        } else if roll < w.op_weight + w.scan_weight {
            self.scan_one()?;
        } else if roll < w.op_weight + w.scan_weight + w.deliver_weight {
            self.deliver_next()?;
        } else {
            self.advance()?;
        }
        self.check_partial_writes()?;
        if self
            .step
            .is_multiple_of(self.scenario.workload.quiesce_every)
        {
            self.quiesce()?;
        }
        Ok(())
    }

    fn fault_events(&mut self) -> Result<(), Failure> {
        let net = &self.scenario.net;
        let faults = &self.scenario.faults;
        let (cut_rate, heal_rate, crash_rate, restart_rate) = (
            net.cut_rate,
            net.heal_rate,
            faults.crash_rate,
            faults.restart_rate,
        );
        if self.rng.r#gen::<f64>() < cut_rate {
            let live: Vec<_> = self
                .adjacent
                .iter()
                .copied()
                .filter(|k| !self.cut.contains(k))
                .collect();
            if let Some(&(a, b)) = live.choose(&mut self.rng) {
                self.cut_link(a, b)?;
            }
        }
        if self.rng.r#gen::<f64>() < heal_rate {
            let cut: Vec<_> = self.cut.iter().copied().collect();
            if let Some(&(a, b)) = cut.choose(&mut self.rng) {
                self.heal_link(a, b)?;
            }
        }
        if self.rng.r#gen::<f64>() < crash_rate {
            let up: Vec<usize> = (0..self.nodes.len())
                .filter(|i| self.nodes[*i].up())
                .collect();
            if let Some(&n) = up.choose(&mut self.rng) {
                self.crash(n)?;
            }
        }
        if self.rng.r#gen::<f64>() < restart_rate {
            let down: Vec<usize> = (0..self.nodes.len())
                .filter(|i| !self.nodes[*i].up())
                .collect();
            if let Some(&n) = down.choose(&mut self.rng) {
                self.restart(n)?;
            }
        }
        Ok(())
    }

    fn deliver_next(&mut self) -> Result<(), Failure> {
        let Some((&(at, seq), _)) = self.packets.iter().next() else {
            return self.advance();
        };
        let packet = self.packets.remove(&(at, seq)).expect("present");
        self.now_ms = self.now_ms.max(at);
        self.deliver(packet)
    }

    /// Move the clock forward, delivering what comes due, then tick every
    /// running node so retry and resync timers can fire.
    fn advance(&mut self) -> Result<(), Failure> {
        let dt = self.rng.gen_range(50..=1_500);
        self.advance_by(dt)
    }

    fn advance_by(&mut self, dt: u64) -> Result<(), Failure> {
        let target = self.now_ms + dt;
        while let Some((&(at, seq), _)) = self.packets.iter().next() {
            if at > target {
                break;
            }
            let packet = self.packets.remove(&(at, seq)).expect("present");
            self.now_ms = self.now_ms.max(at);
            self.deliver(packet)?;
        }
        self.now_ms = target;
        for node in 0..self.nodes.len() {
            let now = self.now_instant();
            self.call(node, "tick", |engine, syncer, out| {
                syncer.tick(engine, now, out)
            })?;
        }
        Ok(())
    }

    fn scan_one(&mut self) -> Result<(), Failure> {
        let dirty: Vec<usize> = (0..self.nodes.len())
            .filter(|i| self.nodes[*i].up() && self.nodes[*i].dirty)
            .collect();
        let Some(&node) = dirty.choose(&mut self.rng) else {
            return self.deliver_next();
        };
        self.scan(node)
    }

    fn scan(&mut self, node: usize) -> Result<(), Failure> {
        if !self.nodes[node].up() {
            return Ok(());
        }
        self.stats.scans += 1;
        self.nodes[node].dirty = false;
        let name = self.nodes[node].name.clone();
        let mut unstable = false;
        let mut report_lines = Vec::new();
        self.call(node, "scan", |engine, _syncer, _out| {
            let report = engine.scan(
                SPACE,
                MOUNT,
                ScanOptions {
                    allow_mass_delete: true,
                    dry_run: false,
                },
            )?;
            unstable = !report.unstable.is_empty();
            report_lines.push(format!(
                "{name}: scanned: created {} modified {} deleted {} unchanged {}{}",
                report.created,
                report.modified,
                report.deleted,
                report.unchanged,
                if unstable {
                    " (unstable paths remain)"
                } else {
                    ""
                }
            ));
            for w in &report.warnings {
                report_lines.push(format!("{name}: scan warning: {w}"));
            }
            Ok(Vec::new())
        })?;
        for line in report_lines {
            self.trace(line);
        }
        if unstable || self.last_call_errored() {
            self.nodes[node].dirty = true;
        }
        Ok(())
    }

    /// `call` traces an error line when a faulted call returns `Err`; a scan
    /// that did so must run again.
    fn last_call_errored(&self) -> bool {
        self.trace
            .last()
            .is_some_and(|l| l.contains("returned error after fault"))
    }

    // ----- workload -----

    fn fresh_name(&mut self, prefix: &str, ext: &str) -> String {
        self.name_counter += 1;
        format!("{prefix}{}{ext}", self.name_counter)
    }

    fn text_content(&mut self, node: usize) -> Vec<u8> {
        let lines = self.rng.gen_range(3..=8);
        let mut out = String::new();
        for _ in 0..lines {
            out.push_str(&self.token(node));
            out.push('\n');
        }
        out.into_bytes()
    }

    fn token(&mut self, node: usize) -> String {
        format!("node{node}-s{}-{:08x}", self.step, self.rng.r#gen::<u32>())
    }

    fn binary_content(&mut self) -> Vec<u8> {
        let max = self.scenario.workload.max_bytes.max(2);
        let len = self.rng.gen_range(1..=max);
        let mut bytes = vec![0u8; len];
        self.rng.fill_bytes(&mut bytes);
        // Guarantee it is not mergeable text.
        bytes[0] = 0;
        bytes
    }

    fn edit_text(&mut self, node: usize, current: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(current).into_owned();
        let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
        let token = format!("{}\n", self.token(node));
        if lines.is_empty() || self.rng.gen_bool(0.3) {
            let at = self.rng.gen_range(0..=lines.len());
            lines.insert(at, token);
        } else {
            let at = self.rng.gen_range(0..lines.len());
            lines[at] = token;
        }
        lines.concat().into_bytes()
    }

    fn workload_op(&mut self) -> Result<(), Failure> {
        let up: Vec<usize> = (0..self.nodes.len())
            .filter(|i| self.nodes[*i].up())
            .collect();
        let Some(&node) = up.choose(&mut self.rng) else {
            return Ok(());
        };
        let w = self.scenario.workload.clone();
        let tree = tree::snapshot(&self.nodes[node].mount);
        let files: Vec<String> = tree::files(&tree).map(|(p, _)| p.clone()).collect();
        let dirs: Vec<String> = tree::dirs(&tree).cloned().collect();
        let can_create = files.len() < w.max_files;
        let weights = [
            if can_create { w.create } else { 0 },
            if files.is_empty() { 0 } else { w.modify },
            if files.is_empty() { 0 } else { w.delete },
            if can_create { w.mkdir } else { 0 },
            if files.is_empty() { 0 } else { w.rename },
            if dirs.is_empty() { 0 } else { w.delete_dir },
        ];
        let total: u32 = weights.iter().sum();
        if total == 0 {
            return Ok(());
        }
        let mut roll = self.rng.gen_range(0..total);
        let mut op = 0;
        for (i, weight) in weights.iter().enumerate() {
            if roll < *weight {
                op = i;
                break;
            }
            roll -= weight;
        }
        self.stats.ops += 1;
        let mount = self.nodes[node].mount.clone();
        match op {
            0 => {
                let text = self.rng.gen_bool(w.text_fraction);
                let name = self.fresh_name("f", if text { ".txt" } else { ".bin" });
                let dir = self.pick_dir(&dirs);
                let path = join(&dir, &name);
                let content = if text {
                    self.text_content(node)
                } else {
                    self.binary_content()
                };
                self.write_file(node, &mount, &path, &content)?;
                self.trace(format!(
                    "{}: create {path} ({} bytes, {})",
                    self.nodes[node].name,
                    content.len(),
                    &tree::hash(&content).to_string()[..8]
                ));
            }
            1 => {
                let path = files.choose(&mut self.rng).expect("non-empty").clone();
                let current = match tree.get(&path) {
                    Some(TreeNode::File(bytes)) => bytes.clone(),
                    _ => Vec::new(),
                };
                let content = if path.ends_with(".txt") {
                    self.edit_text(node, &current)
                } else {
                    self.binary_content()
                };
                let before = tree::hash(&current).to_string();
                self.write_file(node, &mount, &path, &content)?;
                self.trace(format!(
                    "{}: modify {path} ({} bytes, {} <- {})",
                    self.nodes[node].name,
                    content.len(),
                    &tree::hash(&content).to_string()[..8],
                    &before[..8]
                ));
            }
            2 => {
                let path = files.choose(&mut self.rng).expect("non-empty").clone();
                let was = match tree.get(&path) {
                    Some(TreeNode::File(bytes)) => tree::hash(bytes).to_string(),
                    _ => "?".repeat(8),
                };
                self.delete_file(node, &mount, &path, &tree)?;
                self.trace(format!(
                    "{}: delete {path} ({})",
                    self.nodes[node].name,
                    &was[..8]
                ));
            }
            3 => {
                let name = self.fresh_name("d", "");
                let dir = self.pick_dir(&dirs);
                let path = join(&dir, &name);
                fs::create_dir_all(mount.join(&path))
                    .map_err(|e| self.fail(format!("mkdir {path}: {e}")))?;
                self.trace(format!("{}: mkdir {path}", self.nodes[node].name));
            }
            4 => {
                let from = files.choose(&mut self.rng).expect("non-empty").clone();
                let ext = if from.ends_with(".txt") {
                    ".txt"
                } else {
                    ".bin"
                };
                let name = self.fresh_name("r", ext);
                let dir = self.pick_dir(&dirs);
                let to = join(&dir, &name);
                let content = match tree.get(&from) {
                    Some(TreeNode::File(bytes)) => bytes.clone(),
                    _ => Vec::new(),
                };
                fs::rename(mount.join(&from), mount.join(&to))
                    .map_err(|e| self.fail(format!("rename {from} -> {to}: {e}")))?;
                self.model.write(&from, node, Some(&content), None);
                self.model.write(&to, node, None, Some(&content));
                self.trace(format!(
                    "{}: rename {from} -> {to} ({})",
                    self.nodes[node].name,
                    &tree::hash(&content).to_string()[..8]
                ));
            }
            _ => {
                let dir = dirs.choose(&mut self.rng).expect("non-empty").clone();
                let prefix = format!("{dir}/");
                let under: Vec<String> = files
                    .iter()
                    .filter(|f| f.starts_with(&prefix))
                    .cloned()
                    .collect();
                for path in &under {
                    let current = match tree.get(path) {
                        Some(TreeNode::File(bytes)) => Some(bytes.as_slice()),
                        _ => None,
                    };
                    self.model.write(path, node, current, None);
                }
                fs::remove_dir_all(mount.join(&dir))
                    .map_err(|e| self.fail(format!("rmdir {dir}: {e}")))?;
                self.trace(format!(
                    "{}: delete dir {dir} ({} files)",
                    self.nodes[node].name,
                    under.len()
                ));
            }
        }
        self.nodes[node].dirty = true;
        Ok(())
    }

    fn pick_dir(&mut self, dirs: &[String]) -> String {
        if dirs.is_empty() || self.rng.gen_bool(0.4) {
            String::new()
        } else {
            dirs.choose(&mut self.rng).cloned().unwrap_or_default()
        }
    }

    fn write_file(
        &mut self,
        node: usize,
        mount: &Path,
        path: &str,
        content: &[u8],
    ) -> Result<(), Failure> {
        let dest = mount.join(path);
        let current = fs::read(&dest).ok();
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| self.fail(format!("mkdir for {path}: {e}")))?;
        }
        fs::write(&dest, content).map_err(|e| self.fail(format!("write {path}: {e}")))?;
        self.model
            .write(path, node, current.as_deref(), Some(content));
        Ok(())
    }

    fn delete_file(
        &mut self,
        node: usize,
        mount: &Path,
        path: &str,
        tree: &Tree,
    ) -> Result<(), Failure> {
        let current = match tree.get(path) {
            Some(TreeNode::File(bytes)) => Some(bytes.as_slice()),
            _ => None,
        };
        fs::remove_file(mount.join(path)).map_err(|e| self.fail(format!("delete {path}: {e}")))?;
        self.model.write(path, node, current, None);
        Ok(())
    }

    // ----- invariants -----

    /// Every file a user could open holds complete content: something the
    /// workload wrote or an object the engine materialized from its store.
    fn check_partial_writes(&mut self) -> Result<(), Failure> {
        let started = Instant::now();
        let result = self.check_partial_writes_inner();
        self.walk_time += started.elapsed();
        result
    }

    fn check_partial_writes_inner(&mut self) -> Result<(), Failure> {
        for node in 0..self.nodes.len() {
            if !self.nodes[node].up() {
                continue;
            }
            let tree = tree::snapshot(&self.nodes[node].mount);
            let store = self.nodes[node].engine.as_ref().expect("up").store();
            for (path, bytes) in tree::files(&tree) {
                let id = tree::hash(bytes);
                if !self.model.known.contains(&id) && !store.contains(&id) {
                    return Err(self.fail(format!(
                        "partial or unknown content visible at {}:{path} ({} bytes, {})",
                        self.nodes[node].name,
                        bytes.len(),
                        &id.to_string()[..8]
                    )));
                }
            }
        }
        Ok(())
    }

    /// Heal every link, restart every node, drain the network and the
    /// timers, then check that every replica agrees and nothing was lost.
    pub fn quiesce(&mut self) -> Result<(), Failure> {
        self.faults_enabled = false;
        self.faults.borrow_mut().armed = None;
        self.trace("quiesce: healing links and restarting nodes");
        let cut: Vec<_> = self.cut.iter().copied().collect();
        for (a, b) in cut {
            self.heal_link(a, b)?;
        }
        for node in 0..self.nodes.len() {
            self.restart(node)?;
        }
        self.connect_all()?;
        let mut quiet = 0u32;
        let mut rounds = 0u32;
        while quiet < QUIET_TICKS {
            rounds += 1;
            if rounds > MAX_QUIESCE_ROUNDS {
                return Err(self.fail(format!(
                    "did not reach a quiet state after {MAX_QUIESCE_ROUNDS} rounds; {} packets in flight",
                    self.packets.len()
                )));
            }
            let before = (self.stats.frames, self.stats.fetches, self.stats.scans);
            self.settle()?;
            let dirty: Vec<usize> = (0..self.nodes.len())
                .filter(|i| self.nodes[*i].dirty)
                .collect();
            for node in dirty {
                self.scan(node)?;
            }
            self.advance_by(QUIESCE_TICK_MS)?;
            let after = (self.stats.frames, self.stats.fetches, self.stats.scans);
            let dirty = self.nodes.iter().any(|n| n.dirty);
            if before == after && self.packets.is_empty() && !dirty {
                quiet += 1;
            } else {
                quiet = 0;
            }
        }
        self.stats.quiesces += 1;
        self.check_converged()?;
        self.faults_enabled = true;
        Ok(())
    }

    fn check_converged(&mut self) -> Result<(), Failure> {
        let trees: Vec<Tree> = self
            .nodes
            .iter()
            .map(|n| tree::snapshot(&n.mount))
            .collect();
        for (i, tree) in trees.iter().enumerate().skip(1) {
            if *tree != trees[0] {
                let a = tree::describe(&trees[0]);
                let b = tree::describe(tree);
                return Err(self.fail(format!(
                    "trees differ after quiesce\n--- {}:\n{a}\n--- {}:\n{b}",
                    self.nodes[0].name, self.nodes[i].name
                )));
            }
        }
        let indexes: Vec<BTreeSet<(String, String, String)>> = self
            .nodes
            .iter()
            .map(|n| {
                n.engine
                    .as_ref()
                    .expect("up")
                    .entries(SPACE, MOUNT, true)
                    .expect("entries")
                    .into_iter()
                    .map(|e| {
                        (
                            e.key.path.to_string(),
                            format!("{:?}", e.content),
                            format!("{:?}", e.vector),
                        )
                    })
                    .collect()
            })
            .collect();
        for (i, index) in indexes.iter().enumerate().skip(1) {
            if *index != indexes[0] {
                let only_a: Vec<_> = indexes[0].difference(index).collect();
                let only_b: Vec<_> = index.difference(&indexes[0]).collect();
                return Err(self.fail(format!(
                    "indexes differ after quiesce ({} vs {} rows)\nonly on {}: {only_a:#?}\nonly on {}: {only_b:#?}\nall on {}: {:#?}",
                    indexes[0].len(), index.len(),
                    self.nodes[0].name, self.nodes[i].name, self.nodes[0].name, indexes[0]
                )));
            }
        }
        for (node, tree) in trees.iter().enumerate() {
            self.check_index_matches_disk(node, tree)?;
            let report = self.nodes[node]
                .engine
                .as_ref()
                .expect("up")
                .verify_objects()
                .map_err(|e| self.fail(format!("verify objects: {e}")))?;
            if !report.missing.is_empty() || !report.corrupt.is_empty() {
                return Err(self.fail(format!(
                    "{}: object store has {} missing and {} corrupt objects",
                    self.nodes[node].name,
                    report.missing.len(),
                    report.corrupt.len()
                )));
            }
        }
        let tree = &trees[0];
        self.stats.conflict_copies += tree
            .keys()
            .filter(|p| LogicalPath::new(p).is_ok_and(|l| is_conflict_copy(&l)))
            .count() as u64;
        if let Err(message) = self.model.check(tree) {
            // Add each node's index row and history for the path named.
            let path = message
                .split(" of ")
                .nth(1)
                .or_else(|| message.strip_prefix("resurrection: "))
                .and_then(|rest| rest.split(' ').next())
                .unwrap_or("")
                .to_owned();
            let detail = self.describe_path(&path);
            return Err(self.fail(format!("{message}\n{detail}")));
        }
        self.model.reset();
        let n = tree::files(tree).count();
        self.trace(format!(
            "quiesce: converged on {n} files, {} entries",
            tree.len()
        ));
        Ok(())
    }

    /// Every node's index row and version history for one path.
    fn describe_path(&self, path: &str) -> String {
        let Ok(logical) = LogicalPath::new(path) else {
            return String::new();
        };
        let mut out = format!("history of {path}:\n");
        for n in &self.nodes {
            let Some(engine) = n.engine.as_ref() else {
                continue;
            };
            let row = engine
                .entries(SPACE, MOUNT, true)
                .ok()
                .and_then(|rows| rows.into_iter().find(|e| e.key.path == logical))
                .map(|e| {
                    format!(
                        "{:?} {:?} by {:?} seq {}",
                        e.content, e.vector, e.modified_by, e.sequence.0
                    )
                })
                .unwrap_or_else(|| "no row".into());
            out.push_str(&format!("  {} ({}): {row}\n", n.name, n.device.short()));
            if let Ok(history) = engine.history(SPACE, MOUNT, &logical) {
                for h in history {
                    out.push_str(&format!(
                        "      seq {} {:?} {:?} by {:?} parent {:?}\n",
                        h.sequence.0,
                        h.content,
                        h.vector,
                        h.modified_by,
                        h.parent_object.map(|o| o.to_string()[..8].to_owned())
                    ));
                }
            }
        }
        out
    }

    fn check_index_matches_disk(&self, node: usize, tree: &Tree) -> Result<(), Failure> {
        let engine = self.nodes[node].engine.as_ref().expect("up");
        let entries = engine
            .entries(SPACE, MOUNT, false)
            .map_err(|e| self.fail(format!("entries: {e}")))?;
        let mut indexed = BTreeSet::new();
        for entry in entries {
            let path = entry.key.path.to_string();
            indexed.insert(path.clone());
            match (&entry.content, tree.get(&path)) {
                (EntryContent::File { object, .. }, Some(TreeNode::File(bytes))) => {
                    if tree::hash(bytes) != *object {
                        return Err(self.fail(format!(
                            "{}: index says {path} is {} but disk hashes to {}",
                            self.nodes[node].name,
                            &object.to_string()[..8],
                            &tree::hash(bytes).to_string()[..8]
                        )));
                    }
                }
                (EntryContent::Directory, Some(TreeNode::Dir)) => {}
                (EntryContent::Symlink { .. }, _) => {}
                (EntryContent::Deleted, None) => {}
                (content, on_disk) => {
                    return Err(self.fail(format!(
                        "{}: index has {path} as {content:?} but disk has {on_disk:?}",
                        self.nodes[node].name
                    )));
                }
            }
        }
        for path in tree.keys() {
            if !indexed.contains(path) {
                return Err(self.fail(format!(
                    "{}: {path} is on disk but not in the index after quiesce",
                    self.nodes[node].name
                )));
            }
        }
        Ok(())
    }
}

impl Drop for Simulator<'_> {
    fn drop(&mut self) {
        faults::clear();
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn frame_name(body: &frame::Body) -> &'static str {
    match body {
        frame::Body::Hello(_) => "hello",
        frame::Body::SpaceOffers(_) => "offers",
        frame::Body::IndexRequest(_) => "index-request",
        frame::Body::IndexBatch(_) => "index-batch",
        frame::Body::Ack(_) => "ack",
        frame::Body::Ping(_) => "ping",
        frame::Body::Pong(_) => "pong",
        frame::Body::Error(_) => "error",
        frame::Body::PeerGrants(_) => "grants",
    }
}
