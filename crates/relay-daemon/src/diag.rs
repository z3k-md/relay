//! Diagnostics bundles (D56): one zip per device holding its logs and a
//! report of what it knows about itself, its network, and its peers.
//!
//! `relay diag` writes this device's bundle and, with `--all`, pulls one from
//! every connected device that lets this one manage it (a remote call, D37).
//! `relay diag merge` interleaves bundles into one timeline, shifting each
//! device's lines by the clock offset measured at connect.
//!
//! A bundle holds paths and file names from the logs, never keys, pairing
//! codes, or file contents.

use std::fs::{self, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use relay_core::diag::NetReport;
use relay_core::remote::{DIAG_REMOTE_MAX, RemoteCall, RemoteError, RemoteErrorCode, RemoteReply};
use relay_engine::Engine;
use relay_ipc::{ActivityItem, DiagParams, DiagSaved, HostKind, LogLevel, RpcErrorBody, Status};
use relay_net::NetCommand;
use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;

use crate::host::Host;
use crate::logging;

pub const REPORT: &str = "report.json";
const LOGS_DIR: &str = "logs/";
const FORMAT: u32 = 1;
const NET_REPORT_WAIT: Duration = Duration::from_secs(3);
const ACTIVITY_ITEMS: usize = 200;
/// Log bytes a remote bundle starts from before shrinking to fit.
const REMOTE_LOG_START: usize = 16 * 1024 * 1024;
const REMOTE_LOG_MIN: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub format: u32,
    pub device_id: String,
    pub device_name: String,
    pub relay_version: String,
    pub os: String,
    pub arch: String,
    /// The host that wrote it; `None` when Relay was not running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostKind>,
    pub collected_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<LogLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<NetReport>,
    /// Peers as the database has them.
    pub peers: Vec<PeerRecord>,
    #[serde(default)]
    pub activity: Vec<ActivityItem>,
    /// Older log lines were left out to fit.
    #[serde(default)]
    pub logs_truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerRecord {
    pub id: String,
    pub name: String,
    pub addresses: Vec<String>,
    pub may_manage: bool,
    pub revoked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<i64>,
}

/// A bundle read back for merging.
pub struct Bundle {
    pub source: PathBuf,
    pub report: Report,
    /// Log files, oldest first: `(name, text)`.
    pub logs: Vec<(String, String)>,
}

/// This device's bundle when Relay is not running: the database and the
/// log files, no live state.
pub fn build_offline(home: &Path) -> Result<Vec<u8>> {
    build(home, None, None)
}

/// The bundle a peer asked for, logs cut until it fits [`DIAG_REMOTE_MAX`].
pub(crate) fn remote_bundle(host: &Host, home: &Path) -> Result<Vec<u8>, RemoteError> {
    let failed = |err: anyhow::Error| RemoteError::new(RemoteErrorCode::Failed, format!("{err:#}"));
    let mut budget = REMOTE_LOG_START;
    loop {
        let bundle = build(home, Some(host), Some(budget)).map_err(failed)?;
        if bundle.len() <= DIAG_REMOTE_MAX {
            return Ok(bundle);
        }
        if budget <= REMOTE_LOG_MIN {
            return Err(RemoteError::new(
                RemoteErrorCode::Failed,
                "the diagnostics bundle does not fit in one reply",
            ));
        }
        budget /= 2;
    }
}

/// `diag` over IPC: write this device's bundle, or a connected peer's, to
/// `dest`.
pub(crate) fn save(host: &Host, params: &DiagParams) -> Result<DiagSaved, RpcErrorBody> {
    let bundle = match &params.peer {
        None => build(&host.home, Some(host), None)
            .map_err(|err| RpcErrorBody::new("failed", format!("{err:#}")))?,
        Some(name) => {
            // A local name or a device id: `relay diag --all` goes by id.
            let peer = match host.peer_named(name) {
                Ok(peer) => peer,
                Err(err) => Engine::open_read_only(&host.home)
                    .and_then(|engine| engine.peers())
                    .ok()
                    .and_then(|peers| peers.into_iter().find(|p| p.id.to_string() == *name))
                    .ok_or(err)?,
            };
            match host.call_peer(peer.id, RemoteCall::Diagnostics) {
                Ok(RemoteReply::Diagnostics { bundle }) => bundle,
                Ok(_) => return Err(RpcErrorBody::new("failed", "unexpected reply")),
                Err(err) => return Err(RpcErrorBody::new(err.code.as_str(), err.message)),
            }
        }
    };
    write_new(&params.dest, &bundle)
        .map_err(|err| RpcErrorBody::new("failed", format!("{err:#}")))?;
    Ok(DiagSaved {
        path: params.dest.clone(),
        bytes: bundle.len() as u64,
    })
}

/// Which bundles [`collect`] gathers besides this device's.
pub enum Pull<'a> {
    None,
    /// Every connected device that lets this one manage it.
    All,
    /// These peers, by local name or device id.
    Peers(&'a [String]),
}

/// What [`collect`] wrote.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Collected {
    pub bundles: Vec<PathBuf>,
    /// The merged timeline, when there is more than one bundle.
    pub timeline: Option<PathBuf>,
    /// `device: why` for each bundle that could not be had.
    pub failures: Vec<String>,
    /// Pulling was asked for, but no connected device lets this one manage it.
    pub nobody_to_pull: bool,
}

/// Save this device's bundle into `dir`, pull the others `pull` names
/// through the running host, and merge them into a timeline. This device's
/// bundle is named `own_name` when given. With Relay stopped only this
/// device's bundle can be had.
pub fn collect(home: &Path, dir: &Path, own_name: Option<&str>, pull: Pull) -> Result<Collected> {
    // The host writes the bundles, maybe from another working directory.
    let dir = &std::path::absolute(dir)?;
    let now = logging::unix_ms();
    let device_name = Engine::open_read_only(home)?.device().name.clone();
    let own = dir.join(
        own_name
            .map(str::to_owned)
            .unwrap_or_else(|| file_name(&device_name, now)),
    );
    let mut done = Collected::default();
    let Some(mut client) = relay_ipc::Client::connect(home)? else {
        if !matches!(pull, Pull::None) {
            bail!("Relay is not running, so other devices cannot be reached");
        }
        write_new(&own, &build_offline(home)?)?;
        done.bundles.push(own);
        return Ok(done);
    };
    let saved = client
        .diag(&DiagParams {
            dest: own.clone(),
            peer: None,
        })
        .with_context(|| format!("writing {}", own.display()))?;
    done.bundles.push(saved.path);
    let targets: Vec<(String, String)> = match pull {
        Pull::None => Vec::new(),
        Pull::All => client
            .status()?
            .peers
            .into_iter()
            .filter(|p| p.manageable && p.supports_remote)
            .map(|p| (p.id, p.name))
            .collect(),
        Pull::Peers(peers) => peers.iter().map(|p| (p.clone(), p.clone())).collect(),
    };
    done.nobody_to_pull = matches!(pull, Pull::All) && targets.is_empty();
    for (peer, name) in targets {
        let dest = dir.join(file_name(&name, now));
        match client.diag(&DiagParams {
            dest,
            peer: Some(peer),
        }) {
            Ok(saved) => done.bundles.push(saved.path),
            Err(err) => done.failures.push(format!("{name}: {err}")),
        }
    }
    if done.bundles.len() > 1 {
        let path = dir.join(format!("relay-diag-timeline-{}.log", stamp(now)));
        let merged = done
            .bundles
            .iter()
            .map(|path| read(path))
            .collect::<Result<Vec<_>>>()
            .and_then(|bundles| merge(&bundles));
        match merged {
            Ok(merged) => {
                let mut text = merged
                    .notes
                    .iter()
                    .map(|n| format!("# {n}\n"))
                    .collect::<String>();
                text.push_str(&merged.lines.join("\n"));
                text.push('\n');
                write_new(&path, text.as_bytes())?;
                done.timeline = Some(path);
            }
            Err(err) => done.failures.push(format!("merging: {err:#}")),
        }
    }
    Ok(done)
}

/// Write `bytes` to `path`, which must not exist.
pub fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// `relay-diag-<device>-<UTC time>.zip`, safe as a file name everywhere.
pub fn file_name(device_name: &str, at_ms: u64) -> String {
    let device: String = device_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    format!("relay-diag-{device}-{}.zip", stamp(at_ms))
}

/// `20261018003056`: a UTC time for file names.
pub fn stamp(at_ms: u64) -> String {
    logging::format_utc(at_ms / 1000)
        .chars()
        .filter(char::is_ascii_digit)
        .collect()
}

fn build(home: &Path, host: Option<&Host>, log_budget: Option<usize>) -> Result<Vec<u8>> {
    let engine = Engine::open_read_only(home).context("opening the Relay database")?;
    let device = engine.device();
    let peers = engine
        .peers()?
        .into_iter()
        .map(|p| PeerRecord {
            id: p.id.to_string(),
            name: p.name,
            addresses: p.addresses,
            may_manage: p.may_manage,
            revoked: p.revoked,
            last_seen_ms: p.last_seen_ms,
        })
        .collect();
    let log_path = logging::log_path(home);
    let files = logging::log_files(&log_path);
    let (logs, logs_truncated) = match log_budget {
        None => (read_logs(&files), false),
        Some(budget) => tail_logs(&files, budget),
    };
    let report = Report {
        format: FORMAT,
        device_id: device.id.to_string(),
        device_name: device.name.clone(),
        relay_version: env!("CARGO_PKG_VERSION").to_owned(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        host: host.map(|h| h.kind),
        collected_at_ms: logging::unix_ms(),
        log_level: logging::level(),
        status: host.map(Host::snapshot),
        net: host.and_then(net_report),
        peers,
        activity: host.map(recent_activity).unwrap_or_default(),
        logs_truncated,
    };
    drop(engine);

    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(REPORT, options)?;
    zip.write_all(&serde_json::to_vec_pretty(&report)?)?;
    for (name, bytes) in logs {
        zip.start_file(format!("{LOGS_DIR}{name}"), options)?;
        zip.write_all(&bytes)?;
    }
    Ok(zip.finish()?.into_inner())
}

fn net_report(host: &Host) -> Option<NetReport> {
    let net = host.net_sender().ok()?;
    let (reply, rx) = mpsc::channel();
    net.send(NetCommand::Diagnostics { reply });
    rx.recv_timeout(NET_REPORT_WAIT).ok()
}

fn recent_activity(host: &Host) -> Vec<ActivityItem> {
    host.activity
        .lock()
        .map(|ring| {
            let skip = ring.len().saturating_sub(ACTIVITY_ITEMS);
            ring.iter().skip(skip).cloned().collect()
        })
        .unwrap_or_default()
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn read_logs(files: &[PathBuf]) -> Vec<(String, Vec<u8>)> {
    files
        .iter()
        .filter_map(|path| Some((file_label(path), fs::read(path).ok()?)))
        .collect()
}

/// The newest `budget` bytes across the log files, starting at a whole line,
/// as one `relay.log`. Returns whether anything was left out.
fn tail_logs(files: &[PathBuf], budget: usize) -> (Vec<(String, Vec<u8>)>, bool) {
    let mut joined = Vec::new();
    for (_, bytes) in read_logs(files) {
        joined.extend_from_slice(&bytes);
    }
    if joined.len() <= budget {
        return (vec![(logging::LOG_FILE.to_owned(), joined)], false);
    }
    let mut tail = joined.split_off(joined.len() - budget);
    if let Some(newline) = tail.iter().position(|&b| b == b'\n') {
        tail.drain(..=newline);
    }
    (vec![(logging::LOG_FILE.to_owned(), tail)], true)
}

/// Read a bundle written by [`build`].
pub fn read(path: &Path) -> Result<Bundle> {
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("{} is not a diagnostics bundle", path.display()))?;
    let report: Report = {
        let mut entry = zip
            .by_name(REPORT)
            .with_context(|| format!("{} has no {REPORT}", path.display()))?;
        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        serde_json::from_str(&text).with_context(|| format!("reading {REPORT}"))?
    };
    let mut logs = Vec::new();
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let Some(name) = entry.name().strip_prefix(LOGS_DIR).map(str::to_owned) else {
            continue;
        };
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        logs.push((name, String::from_utf8_lossy(&bytes).into_owned()));
    }
    // Oldest first: relay.log.2, relay.log.1, relay.log.
    logs.sort_by_key(|(name, _)| std::cmp::Reverse(rotation_index(name)));
    Ok(Bundle {
        source: path.to_path_buf(),
        report,
        logs,
    })
}

fn rotation_index(name: &str) -> u32 {
    name.rsplit_once('.')
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(0)
}

/// One merged timeline and notes on how each device's clock was placed.
pub struct Merged {
    pub lines: Vec<String>,
    pub notes: Vec<String>,
}

/// Interleave the bundles' log lines on the first bundle's clock. Each other
/// device's lines move by its clock offset as either side measured it at
/// connect (D56); with neither, they stay as written and a note says so.
pub fn merge(bundles: &[Bundle]) -> Result<Merged> {
    let Some(reference) = bundles.first() else {
        bail!("no bundles to merge");
    };
    let width = bundles
        .iter()
        .map(|b| b.report.device_name.chars().count())
        .max()
        .unwrap_or(0);
    let mut notes = Vec::new();
    let mut timed: Vec<(i64, usize, usize, String)> = Vec::new();
    for (index, bundle) in bundles.iter().enumerate() {
        let offset_ms = if index == 0 {
            0
        } else {
            match offset_to(reference, bundle) {
                Some(ms) => ms,
                None => {
                    notes.push(format!(
                        "{}: no clock offset to {} recorded; times are its own",
                        bundle.report.device_name, reference.report.device_name
                    ));
                    0
                }
            }
        };
        if index > 0 {
            notes.push(format!(
                "{}: clock {offset_ms:+} ms from {}",
                bundle.report.device_name, reference.report.device_name
            ));
        }
        let label = format!("{:width$}", bundle.report.device_name);
        let mut last = i64::MIN;
        let mut seq = 0usize;
        for (_, text) in &bundle.logs {
            for line in text.lines() {
                let (at, rest) = match logging::parse_rfc3339_micros(line) {
                    Some(at) => {
                        let at = at - offset_ms * 1000;
                        last = at;
                        let rest = line.split_once(' ').map_or("", |(_, r)| r);
                        (at, rest.trim_start())
                    }
                    // Headers, panics, and wrapped lines stay with the line before.
                    None if last == i64::MIN => continue,
                    None => (last, line),
                };
                let stamp = logging::format_rfc3339(
                    std::time::UNIX_EPOCH + Duration::from_micros(at.max(0) as u64),
                );
                timed.push((at, index, seq, format!("{stamp} [{label}] {rest}")));
                seq += 1;
            }
        }
    }
    timed.sort_by_key(|a| (a.0, a.1, a.2));
    Ok(Merged {
        lines: timed.into_iter().map(|(_, _, _, line)| line).collect(),
        notes,
    })
}

/// `other`'s clock minus `reference`'s, in ms, from either report.
fn offset_to(reference: &Bundle, other: &Bundle) -> Option<i64> {
    let measured = |from: &Bundle, to: &Bundle| {
        from.report
            .net
            .as_ref()?
            .peers
            .iter()
            .find(|p| p.id == to.report.device_id)?
            .connection
            .as_ref()?
            .clock_offset_ms
    };
    measured(reference, other).or_else(|| measured(other, reference).map(|ms| -ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_core::diag::{ConnReport, PeerNetReport};

    fn bundle(name: &str, id: &str, offsets: &[(&str, i64)], log: &str) -> Bundle {
        Bundle {
            source: PathBuf::from(name),
            report: Report {
                format: FORMAT,
                device_id: id.into(),
                device_name: name.into(),
                relay_version: "0".into(),
                os: "test".into(),
                arch: "test".into(),
                host: None,
                collected_at_ms: 0,
                log_level: None,
                status: None,
                net: Some(NetReport {
                    peers: offsets
                        .iter()
                        .map(|(peer, ms)| PeerNetReport {
                            id: (*peer).into(),
                            connection: Some(ConnReport {
                                clock_offset_ms: Some(*ms),
                                ..ConnReport::default()
                            }),
                            ..PeerNetReport::default()
                        })
                        .collect(),
                    ..NetReport::default()
                }),
                peers: Vec::new(),
                activity: Vec::new(),
                logs_truncated: false,
            },
            logs: vec![("relay.log".into(), log.into())],
        }
    }

    #[test]
    fn merge_puts_lines_on_the_first_devices_clock() {
        // The phone's clock runs 2 s ahead of the Mac's.
        let mac = bundle(
            "mac",
            "aa",
            &[("bb", 2_000)],
            "==== relay 1 started ====\n\
             2026-10-18T00:00:01.000000Z  INFO dialing\n\
             2026-10-18T00:00:03.000000Z  INFO peer connected\n",
        );
        let phone = bundle(
            "phone",
            "bb",
            &[],
            "2026-10-18T00:00:04.000000Z  INFO incoming\n  wrapped\n",
        );
        let merged = merge(&[mac, phone]).unwrap();
        assert_eq!(
            merged.lines,
            vec![
                "2026-10-18T00:00:01.000000Z [mac  ] INFO dialing",
                "2026-10-18T00:00:02.000000Z [phone] INFO incoming",
                "2026-10-18T00:00:02.000000Z [phone]   wrapped",
                "2026-10-18T00:00:03.000000Z [mac  ] INFO peer connected",
            ]
        );
        assert!(merged.notes[0].contains("+2000 ms"), "{:?}", merged.notes);
    }

    #[test]
    fn merge_uses_the_other_sides_offset_and_notes_a_missing_one() {
        let mac = bundle("mac", "aa", &[], "2026-10-18T00:00:05.000000Z  INFO a\n");
        let phone = bundle(
            "phone",
            "bb",
            &[("aa", -2_000)],
            "2026-10-18T00:00:04.000000Z  INFO b\n",
        );
        let pc = bundle("pc", "cc", &[], "2026-10-18T00:00:06.000000Z  INFO c\n");
        let merged = merge(&[mac, phone, pc]).unwrap();
        assert_eq!(
            merged.lines[0],
            "2026-10-18T00:00:02.000000Z [phone] INFO b"
        );
        assert!(
            merged
                .notes
                .iter()
                .any(|n| n.starts_with("pc: no clock offset"))
        );
    }

    #[test]
    fn tail_starts_at_a_whole_line() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("relay.log.1");
        let new = dir.path().join("relay.log");
        fs::write(&old, "first line\nsecond line\n").unwrap();
        fs::write(&new, "third line\n").unwrap();
        let (logs, cut) = tail_logs(&[old.clone(), new.clone()], 20);
        assert!(cut);
        assert_eq!(String::from_utf8_lossy(&logs[0].1), "third line\n");
        let (logs, cut) = tail_logs(&[old, new], 1_000);
        assert!(!cut);
        assert_eq!(
            String::from_utf8_lossy(&logs[0].1),
            "first line\nsecond line\nthird line\n"
        );
    }

    #[test]
    fn offline_bundle_round_trips() {
        let home = tempfile::tempdir().unwrap();
        Engine::init(home.path(), "orion").unwrap();
        let log = logging::log_path(home.path());
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, "2026-10-18T00:00:01.000000Z  INFO hello\n").unwrap();
        fs::write(log.with_file_name("relay.log.1"), "older\n").unwrap();
        let bytes = build_offline(home.path()).unwrap();
        let path = home.path().join(file_name("orion", 0));
        write_new(&path, &bytes).unwrap();
        assert!(
            write_new(&path, &bytes).is_err(),
            "an existing bundle is kept"
        );
        let read = read(&path).unwrap();
        assert_eq!(read.report.device_name, "orion");
        assert!(read.report.host.is_none());
        let names: Vec<&str> = read.logs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["relay.log.1", "relay.log"]);
        assert_eq!(
            file_name("Zach's Mac", 0),
            "relay-diag-Zach_s_Mac-19700101000000.zip"
        );
    }
}
