//! Live transfer snapshot. The activity log only records start and finish.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use relay_core::{DeviceId, ObjectId, SpaceId};
use serde::{Deserialize, Serialize};

const RATE_WINDOW: Duration = Duration::from_secs(2);
const EMIT_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Receive,
    Send,
    Index,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferLive {
    pub peer_id: String,
    pub peer_name: String,
    pub space: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount: Option<String>,
    pub direction: TransferDirection,
    pub files_done: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_total: Option<u64>,
    pub bytes_done: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
    pub bytes_per_sec: u64,
    pub started_at_ms: u64,
    pub retries: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookend {
    pub kind: String,
    pub summary: String,
}

#[derive(Clone, Debug)]
pub struct IncomingFile {
    pub sequence: u64,
    pub path: String,
    pub object: ObjectId,
    pub size: u64,
    pub local: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    peer: DeviceId,
    space: SpaceId,
    send: bool,
}

struct WaitingFile {
    sequence: u64,
    path: String,
    size: u64,
}

struct RateWindow {
    samples: VecDeque<(Instant, u64)>,
}

impl RateWindow {
    fn new() -> Self {
        Self {
            samples: VecDeque::new(),
        }
    }

    fn observe(&mut self, now: Instant, cumulative: u64) -> u64 {
        self.samples.push_back((now, cumulative));
        let cutoff = now.checked_sub(RATE_WINDOW).unwrap_or(now);
        while self.samples.len() > 1 && self.samples.front().is_some_and(|(t, _)| *t < cutoff) {
            self.samples.pop_front();
        }
        let Some(&(t0, b0)) = self.samples.front() else {
            return 0;
        };
        let dt = now.saturating_duration_since(t0).as_secs_f64();
        if dt < 0.2 {
            return 0;
        }
        let db = cumulative.saturating_sub(b0) as f64;
        (db / dt).round() as u64
    }
}

struct RecvRow {
    peer_name: String,
    space_name: String,
    plan_after: Option<u64>,
    files_total: Option<u64>,
    bytes_total: Option<u64>,
    files_done: u64,
    counted: HashSet<u64>,
    completed_bytes: u64,
    byte_done: HashSet<u64>,
    waiting: HashMap<ObjectId, Vec<WaitingFile>>,
    partial: HashMap<ObjectId, u64>,
    retries: u64,
    caught_up: bool,
    started_at_ms: u64,
    rate: RateWindow,
    bytes_per_sec: u64,
}

struct SendRow {
    peer_name: String,
    space_name: String,
    plan_after: u64,
    plan_through: u64,
    files_total: u64,
    bytes_total: u64,
    files_sent: u64,
    files_done: u64,
    marks: Vec<(u64, u64)>,
    objects: HashMap<ObjectId, u64>,
    have: HashMap<ObjectId, u64>,
    bytes_done: u64,
    started_at_ms: u64,
    rate: RateWindow,
    bytes_per_sec: u64,
}

#[derive(Default)]
pub(crate) struct ProgressBook {
    recv: HashMap<Key, RecvRow>,
    send: HashMap<Key, SendRow>,
    dirty: bool,
    last_emit: Option<Instant>,
    last: Vec<TransferLive>,
}

impl ProgressBook {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_send(
        &mut self,
        peer: DeviceId,
        peer_name: &str,
        space: SpaceId,
        space_name: &str,
        files: u64,
        bytes: u64,
        plan_after: u64,
        plan_through: u64,
        now_ms: u64,
    ) {
        if files == 0 {
            return;
        }
        let key = key(peer, space, true);
        if let Some(existing) = self.send.get(&key)
            && existing.plan_after == plan_after
            && existing.plan_through == plan_through
        {
            return;
        }
        self.send.insert(
            key,
            SendRow {
                peer_name: peer_name.to_owned(),
                space_name: space_name.to_owned(),
                plan_after,
                plan_through,
                files_total: files,
                bytes_total: bytes,
                files_sent: 0,
                files_done: 0,
                marks: Vec::new(),
                objects: HashMap::new(),
                have: HashMap::new(),
                bytes_done: 0,
                started_at_ms: now_ms,
                rate: RateWindow::new(),
                bytes_per_sec: 0,
            },
        );
        self.dirty = true;
    }

    pub(crate) fn note_sent(
        &mut self,
        peer: DeviceId,
        space: SpaceId,
        through: u64,
        entries: u64,
        objects: &[(ObjectId, u64)],
    ) {
        let Some(row) = self.send.get_mut(&key(peer, space, true)) else {
            return;
        };
        row.files_sent = row.files_sent.saturating_add(entries);
        row.marks.push((through, row.files_sent));
        for (object, size) in objects {
            row.objects.entry(*object).or_insert(*size);
        }
        self.dirty = true;
    }

    pub(crate) fn note_ack(&mut self, peer: DeviceId, space: SpaceId, through: u64) {
        let key = key(peer, space, true);
        let Some(row) = self.send.get_mut(&key) else {
            return;
        };
        if let Some((_, files)) = row.marks.iter().rev().find(|(seq, _)| *seq <= through) {
            row.files_done = *files;
        }
        if through >= row.plan_through {
            self.send.remove(&key);
        }
        self.dirty = true;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn observe_incoming(
        &mut self,
        peer: DeviceId,
        peer_name: &str,
        space: SpaceId,
        space_name: &str,
        plan_files: Option<u64>,
        plan_bytes: Option<u64>,
        plan_after: Option<u64>,
        files: &[IncomingFile],
        has_entries: bool,
        now_ms: u64,
    ) {
        if plan_files == Some(0) && !has_entries {
            return;
        }
        if !has_entries && files.is_empty() && plan_files.is_none() {
            return;
        }
        let key = key(peer, space, false);
        let reset = match self.recv.get(&key) {
            Some(row) => plan_after.is_some() && row.plan_after != plan_after,
            None => true,
        };
        if reset {
            self.recv.insert(
                key,
                RecvRow {
                    peer_name: peer_name.to_owned(),
                    space_name: space_name.to_owned(),
                    plan_after,
                    files_total: plan_files.filter(|n| *n > 0),
                    bytes_total: plan_bytes.filter(|_| plan_files.is_some_and(|n| n > 0)),
                    files_done: 0,
                    counted: HashSet::new(),
                    completed_bytes: 0,
                    byte_done: HashSet::new(),
                    waiting: HashMap::new(),
                    partial: HashMap::new(),
                    retries: 0,
                    caught_up: false,
                    started_at_ms: now_ms,
                    rate: RateWindow::new(),
                    bytes_per_sec: 0,
                },
            );
        }
        let Some(row) = self.recv.get_mut(&key) else {
            return;
        };
        for file in files {
            if row.byte_done.contains(&file.sequence) {
                continue;
            }
            if row
                .waiting
                .values()
                .any(|queued| queued.iter().any(|w| w.sequence == file.sequence))
            {
                continue;
            }
            if file.local {
                row.byte_done.insert(file.sequence);
                row.completed_bytes = row.completed_bytes.saturating_add(file.size);
            } else {
                row.waiting
                    .entry(file.object)
                    .or_default()
                    .push(WaitingFile {
                        sequence: file.sequence,
                        path: file.path.clone(),
                        size: file.size,
                    });
            }
        }
        self.dirty = true;
    }

    pub(crate) fn note_applied(
        &mut self,
        peer: DeviceId,
        space: SpaceId,
        sequences: &[u64],
        caught_up: bool,
        queue_empty: bool,
    ) {
        let key = key(peer, space, false);
        let Some(row) = self.recv.get_mut(&key) else {
            return;
        };
        for seq in sequences {
            if row.counted.insert(*seq) {
                row.files_done = row.files_done.saturating_add(1);
            }
        }
        if caught_up {
            row.caught_up = true;
        }
        let done = row.caught_up && queue_empty && row.waiting.is_empty();
        if done {
            self.recv.remove(&key);
        }
        self.dirty = true;
    }

    pub(crate) fn note_download(
        &mut self,
        peer: DeviceId,
        object: ObjectId,
        have: u64,
        now: Instant,
    ) {
        let mut changed = false;
        for (key, row) in self.recv.iter_mut() {
            if key.peer != peer || !row.waiting.contains_key(&object) {
                continue;
            }
            let prev = row.partial.get(&object).copied().unwrap_or(0);
            if have <= prev {
                continue;
            }
            row.partial.insert(object, have);
            let bytes = recv_bytes(row);
            row.bytes_per_sec = row.rate.observe(now, bytes);
            changed = true;
        }
        if changed {
            self.dirty = true;
        }
    }

    pub(crate) fn note_fetched(&mut self, peer: DeviceId, object: ObjectId, now: Instant) {
        let mut changed = false;
        for (key, row) in self.recv.iter_mut() {
            if key.peer != peer {
                continue;
            }
            let Some(files) = row.waiting.remove(&object) else {
                continue;
            };
            row.partial.remove(&object);
            for file in files {
                if row.byte_done.insert(file.sequence) {
                    row.completed_bytes = row.completed_bytes.saturating_add(file.size);
                }
            }
            let bytes = recv_bytes(row);
            row.bytes_per_sec = row.rate.observe(now, bytes);
            changed = true;
        }
        if changed {
            self.dirty = true;
        }
    }

    pub(crate) fn note_upload(
        &mut self,
        peer: DeviceId,
        object: ObjectId,
        have: u64,
        now: Instant,
    ) {
        let mut changed = false;
        for (key, row) in self.send.iter_mut() {
            if key.peer != peer {
                continue;
            }
            let Some(size) = row.objects.get(&object).copied() else {
                continue;
            };
            let capped = have.min(size);
            let prev = row.have.get(&object).copied().unwrap_or(0);
            if capped <= prev {
                continue;
            }
            row.have.insert(object, capped);
            row.bytes_done = row.have.values().copied().sum();
            row.bytes_per_sec = row.rate.observe(now, row.bytes_done);
            changed = true;
        }
        if changed {
            self.dirty = true;
        }
    }

    pub(crate) fn set_retries(&mut self, peer: DeviceId, space: SpaceId, retries: u64) {
        let Some(row) = self.recv.get_mut(&key(peer, space, false)) else {
            return;
        };
        if row.retries != retries {
            row.retries = retries;
            self.dirty = true;
        }
    }

    pub(crate) fn drop_peer(&mut self, peer: DeviceId) {
        let before = self.recv.len() + self.send.len();
        self.recv.retain(|key, _| key.peer != peer);
        self.send.retain(|key, _| key.peer != peer);
        if self.recv.len() + self.send.len() != before {
            self.dirty = true;
        }
    }

    pub(crate) fn emit_if_changed(
        &mut self,
        now: Instant,
        force: bool,
    ) -> Option<Vec<TransferLive>> {
        if !self.dirty {
            return None;
        }
        if !force
            && self
                .last_emit
                .is_some_and(|t| now.saturating_duration_since(t) < EMIT_INTERVAL)
        {
            return None;
        }
        let rows = self.snapshot();
        self.dirty = false;
        self.last_emit = Some(now);
        if rows == self.last {
            return None;
        }
        self.last = rows.clone();
        Some(rows)
    }

    pub(crate) fn snapshot(&self) -> Vec<TransferLive> {
        let mut rows = Vec::new();
        for (key, row) in &self.recv {
            rows.push(TransferLive {
                peer_id: key.peer.to_string(),
                peer_name: row.peer_name.clone(),
                space: row.space_name.clone(),
                mount: None,
                direction: TransferDirection::Receive,
                files_done: row.files_done,
                files_total: row.files_total,
                bytes_done: recv_bytes(row),
                bytes_total: row.bytes_total,
                bytes_per_sec: row.bytes_per_sec,
                started_at_ms: row.started_at_ms,
                retries: row.retries,
                current_path: dominant_path(row),
            });
        }
        for (key, row) in &self.send {
            rows.push(TransferLive {
                peer_id: key.peer.to_string(),
                peer_name: row.peer_name.clone(),
                space: row.space_name.clone(),
                mount: None,
                direction: TransferDirection::Send,
                files_done: row.files_done,
                files_total: Some(row.files_total),
                bytes_done: row.bytes_done,
                bytes_total: None,
                bytes_per_sec: row.bytes_per_sec,
                started_at_ms: row.started_at_ms,
                retries: 0,
                current_path: None,
            });
        }
        rows.sort_by(|a, b| {
            direction_rank(a.direction)
                .cmp(&direction_rank(b.direction))
                .then(a.peer_name.cmp(&b.peer_name))
                .then(a.space.cmp(&b.space))
        });
        rows
    }
}

fn key(peer: DeviceId, space: SpaceId, send: bool) -> Key {
    Key { peer, space, send }
}

fn recv_bytes(row: &RecvRow) -> u64 {
    let partial = row.waiting.iter().fold(0u64, |sum, (object, files)| {
        let have = row.partial.get(object).copied().unwrap_or(0);
        sum + files.iter().map(|file| have.min(file.size)).sum::<u64>()
    });
    row.completed_bytes.saturating_add(partial)
}

fn dominant_path(row: &RecvRow) -> Option<String> {
    let mut best: Option<(u64, &str)> = None;
    let mut rest = 0u64;
    for (object, files) in &row.waiting {
        let have = row.partial.get(object).copied().unwrap_or(0);
        for file in files {
            let left = file.size.saturating_sub(have.min(file.size));
            rest = rest.saturating_add(left);
            if best.as_ref().is_none_or(|(n, _)| left > *n) {
                best = Some((left, file.path.as_str()));
            }
        }
    }
    let (left, path) = best?;
    if rest > 0 && left.saturating_mul(2) > rest {
        Some(path.to_owned())
    } else {
        None
    }
}

fn direction_rank(direction: TransferDirection) -> u8 {
    match direction {
        TransferDirection::Receive => 0,
        TransferDirection::Send => 1,
        TransferDirection::Index => 2,
    }
}

/// One line for the tray: the receive with the most bytes left, else a send, else an index.
pub fn summary_line(rows: &[TransferLive]) -> Option<String> {
    let receive = rows
        .iter()
        .filter(|row| row.direction == TransferDirection::Receive)
        .max_by_key(|row| bytes_left(row));
    let row = receive
        .or_else(|| {
            rows.iter()
                .find(|row| row.direction == TransferDirection::Send)
        })
        .or_else(|| {
            rows.iter()
                .find(|row| row.direction == TransferDirection::Index)
        })?;
    Some(match row.direction {
        TransferDirection::Receive => receive_line(row),
        TransferDirection::Send => send_line(row),
        TransferDirection::Index => index_line(row),
    })
}

fn bytes_left(row: &TransferLive) -> u64 {
    row.bytes_total
        .unwrap_or(row.bytes_done)
        .saturating_sub(row.bytes_done)
}

fn receive_line(row: &TransferLive) -> String {
    let who = format!("Receiving from {} · {}", row.peer_name, row.space);
    match (row.bytes_total, row.bytes_per_sec) {
        (Some(total), rate) if rate > 0 => format!(
            "{who} · {} of {} · {}",
            format_bytes(row.bytes_done),
            format_bytes(total),
            format_rate(rate)
        ),
        (Some(total), _) => format!(
            "{who} · {} of {}",
            format_bytes(row.bytes_done),
            format_bytes(total)
        ),
        (None, rate) if rate > 0 => format!("{who} · {} · {}", file_phrase(row), format_rate(rate)),
        (None, _) => format!("{who} · {}", file_phrase(row)),
    }
}

fn send_line(row: &TransferLive) -> String {
    let who = format!("Sending to {} · {}", row.peer_name, row.space);
    if row.bytes_per_sec > 0 {
        format!(
            "{who} · {} · {}",
            format_bytes(row.bytes_done),
            format_rate(row.bytes_per_sec)
        )
    } else {
        format!("{who} · {}", format_bytes(row.bytes_done))
    }
}

fn index_line(row: &TransferLive) -> String {
    let mount = row.mount.as_deref().unwrap_or(&row.space);
    format!("Indexing {mount} · {} files", row.files_done)
}

fn file_phrase(row: &TransferLive) -> String {
    match row.files_total {
        Some(total) => format!("{} of {total} files", row.files_done),
        None => format!("{} files", row.files_done),
    }
}

pub fn format_bytes(n: u64) -> String {
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
    let gb = mb / 1024.0;
    format!("{gb:.1} GB")
}

pub fn format_rate(n: u64) -> String {
    format!("{}/s", format_bytes(n))
}

pub fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    format!("{mins}m {}s", secs % 60)
}

/// Start and finish lines for rows that appeared or disappeared.
pub fn bookends(prev: &[TransferLive], next: &[TransferLive], now_ms: u64) -> Vec<Bookend> {
    let mut out = Vec::new();
    for row in next {
        if !prev.iter().any(|old| same_row(old, row)) {
            out.push(Bookend {
                kind: "sync".into(),
                summary: start_line(row),
            });
        }
    }
    for old in prev {
        if !next.iter().any(|row| same_row(row, old)) {
            out.push(Bookend {
                kind: "sync".into(),
                summary: finish_line(old, now_ms),
            });
        }
    }
    out
}

fn same_row(a: &TransferLive, b: &TransferLive) -> bool {
    a.peer_id == b.peer_id && a.space == b.space && a.direction == b.direction && a.mount == b.mount
}

fn start_line(row: &TransferLive) -> String {
    match row.direction {
        TransferDirection::Receive => match (row.files_total, row.bytes_total) {
            (Some(files), Some(bytes)) => format!(
                "started receiving {files} files ({}) of {} from {}",
                format_bytes(bytes),
                row.space,
                row.peer_name
            ),
            _ => format!("started receiving {} from {}", row.space, row.peer_name),
        },
        TransferDirection::Send => match row.files_total {
            Some(files) => format!(
                "started sending {files} files of {} to {}",
                row.space, row.peer_name
            ),
            None => format!("started sending {} to {}", row.space, row.peer_name),
        },
        TransferDirection::Index => {
            let mount = row.mount.as_deref().unwrap_or(&row.space);
            format!("started indexing {}/{mount}", row.space)
        }
    }
}

fn finish_line(row: &TransferLive, now_ms: u64) -> String {
    let elapsed = now_ms.saturating_sub(row.started_at_ms);
    let dur = format_duration(elapsed);
    let avg = average_rate(row.bytes_done, elapsed);
    match row.direction {
        TransferDirection::Receive => match avg {
            Some(rate) => format!(
                "finished receiving {} from {} · {dur} · {rate}",
                row.space, row.peer_name
            ),
            None => format!(
                "finished receiving {} from {} · {dur}",
                row.space, row.peer_name
            ),
        },
        TransferDirection::Send => match avg {
            Some(rate) => format!(
                "finished sending {} to {} · {dur} · {rate}",
                row.space, row.peer_name
            ),
            None => format!(
                "finished sending {} to {} · {dur}",
                row.space, row.peer_name
            ),
        },
        TransferDirection::Index => {
            let mount = row.mount.as_deref().unwrap_or(&row.space);
            format!("finished indexing {}/{mount}", row.space)
        }
    }
}

fn average_rate(bytes: u64, elapsed_ms: u64) -> Option<String> {
    if bytes == 0 || elapsed_ms == 0 {
        return None;
    }
    let secs = (elapsed_ms as f64 / 1000.0).max(1.0);
    Some(format_rate((bytes as f64 / secs).round() as u64))
}

pub fn index_row(space: &str, mount: &str, files_seen: u64, bytes_hashed: u64) -> TransferLive {
    TransferLive {
        peer_id: String::new(),
        peer_name: String::new(),
        space: space.to_owned(),
        mount: Some(mount.to_owned()),
        direction: TransferDirection::Index,
        files_done: files_seen,
        files_total: None,
        bytes_done: bytes_hashed,
        bytes_total: None,
        bytes_per_sec: 0,
        started_at_ms: 0,
        retries: 0,
        current_path: None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use relay_core::{DeviceId, ObjectId, SpaceId};

    use super::*;

    fn peer() -> DeviceId {
        DeviceId::from_bytes([9; 32])
    }

    fn object() -> ObjectId {
        ObjectId::of(b"object")
    }

    fn incoming(local: bool) -> IncomingFile {
        IncomingFile {
            sequence: 4,
            path: "photos/big.bin".into(),
            object: object(),
            size: 100,
            local,
        }
    }

    fn open_recv(book: &mut ProgressBook, space: SpaceId, local: bool) {
        book.observe_incoming(
            peer(),
            "macbook",
            space,
            "Photos",
            Some(1),
            Some(100),
            Some(0),
            &[incoming(local)],
            true,
            1_000,
        );
    }

    #[test]
    fn local_object_counts_before_apply() {
        let mut book = ProgressBook::default();
        open_recv(&mut book, SpaceId::new(), true);
        let rows = book.emit_if_changed(Instant::now(), true).unwrap();
        assert_eq!(rows[0].bytes_done, 100);
        assert_eq!(rows[0].bytes_total, Some(100));
        assert_eq!(rows[0].files_done, 0);
    }

    #[test]
    fn chunk_progress_is_replaced_when_the_object_finishes() {
        let mut book = ProgressBook::default();
        let space = SpaceId::new();
        let start = Instant::now();
        open_recv(&mut book, space, false);
        book.note_download(peer(), object(), 40, start);
        let mid = book.emit_if_changed(start, true).unwrap();
        assert_eq!(mid[0].bytes_done, 40);
        assert_eq!(mid[0].current_path.as_deref(), Some("photos/big.bin"));

        book.note_fetched(peer(), object(), start + Duration::from_millis(400));
        let done = book
            .emit_if_changed(start + Duration::from_millis(400), true)
            .unwrap();
        assert_eq!(done[0].bytes_done, 100);

        book.note_applied(peer(), space, &[4], true, true);
        let cleared = book
            .emit_if_changed(start + Duration::from_millis(500), true)
            .unwrap();
        assert!(cleared.is_empty());
    }

    #[test]
    fn rate_window_uses_about_two_seconds() {
        let mut window = RateWindow::new();
        let start = Instant::now();
        assert_eq!(window.observe(start, 0), 0);
        let rate = window.observe(start + Duration::from_secs(1), 2_000);
        assert!((1_500..2_500).contains(&rate), "{rate}");
    }

    #[test]
    fn send_row_ends_on_ack_with_fewer_bytes_than_the_plan() {
        let mut book = ProgressBook::default();
        let space = SpaceId::new();
        let now = Instant::now();
        book.begin_send(peer(), "macbook", space, "Photos", 2, 500, 0, 10, 5_000);
        book.note_sent(peer(), space, 10, 2, &[(object(), 100)]);
        book.note_upload(peer(), object(), 80, now);
        let live = book.emit_if_changed(now, true).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].direction, TransferDirection::Send);
        assert_eq!(live[0].bytes_done, 80);
        assert_eq!(live[0].bytes_total, None);
        assert_eq!(live[0].files_total, Some(2));

        book.note_ack(peer(), space, 10);
        let cleared = book.emit_if_changed(now, true).unwrap();
        assert!(cleared.is_empty());
        let lines = bookends(&live, &cleared, 9_000);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].summary.contains("finished sending"));
        assert!(lines[0].summary.contains("4s"));
    }

    #[test]
    fn ticks_do_not_open_new_bookends() {
        let row = TransferLive {
            peer_id: "p".into(),
            peer_name: "macbook".into(),
            space: "Photos".into(),
            mount: None,
            direction: TransferDirection::Receive,
            files_done: 1,
            files_total: Some(2),
            bytes_done: 10,
            bytes_total: Some(20),
            bytes_per_sec: 5,
            started_at_ms: 0,
            retries: 0,
            current_path: None,
        };
        let mut later = row.clone();
        later.bytes_done = 15;
        assert!(bookends(std::slice::from_ref(&row), &[later], 1_000).is_empty());
        assert_eq!(bookends(&[], std::slice::from_ref(&row), 1_000).len(), 1);
        assert_eq!(bookends(&[row], &[], 2_000).len(), 1);
    }
}
