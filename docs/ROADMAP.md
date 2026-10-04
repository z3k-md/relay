# Roadmap

Where implementation stands against the phased plan in
[`DESIGN.md`](DESIGN.md) §51. Amendments live in [`DECISIONS.md`](DECISIONS.md);
when that file and the spec disagree, the decision wins.

`DESIGN.md` is the original specification. Its "eventually", "initial
release", and phase writeups are not a status report. This file is.

## Next

**Remote explorer** ([proposal](proposals/remote-explorer.md)). Stages 0–4
shipped: config changes on the running host (D36), the Files view (D38),
the manage grant and remote browsing (D37), folder pairs set up from either
device (D39), and opening files that are not synced (D40). Stage 5
(read-only copies) is optional; OS placeholder files are a separate later
decision.

**Phase 14 — Integrations.**

Editor status, conflict notification, restore, and post-sync automation.
These stay above the replication engine. Do not start a hosted backend as
part of this phase.

## Done

| Phase | What shipped | Not in this phase |
| --- | --- | --- |
| 0 Repository and domain model | Workspace, core types, SQLite index, BLAKE3 object store | |
| 1 Local filesystem index | Watcher, debounced scans, reconciliation, multi-mount, root `.relayignore` | Nested `.relayignore` |
| 2 Two-node sync | QUIC, pinned mutual TLS, index deltas, object transfer, atomic materialization | |
| 3 Conflict safety | Concurrent versions kept as conflict copies; Git conflict groups (D21); receive-side mass-delete holds (D22) | |
| 4 Daemon and local IPC | `relay service` and the desktop host, `relay-ipc`. No separate `relayd` binary | The desktop app hosts IPC itself. It does not attach as a client of a `relay service` process (D24) |
| 5 Device pairing | `relay pair` and the desktop Peers view: short code, mDNS on the LAN, `--addr` off-LAN (D25) | Internet rendezvous |
| 6 Desktop UI | Tauri 2 + Vue 3 on macOS and Windows: spaces, devices, mounts, status, activity, pairing, conflicts | Policy editing and per-file history are CLI-only |
| 7 Replication policies | `relay policy` and `relay group` (D27). No policies means the whole shared space syncs | Per-policy durability classes. No GUI policy editor. Removing a policy does not delete files already on disk |
| 8 Durable replica | Filesystem mailbox directory (D29): offline catch-up without a QUIC session | No hosted backend |
| 9 Encryption and recovery | Mailbox object payloads sealed with a per-space key (D30). `relay recovery`, `relay peer revoke`, `relay space rotate` | Local object store and working tree stay plaintext. Entry logs are not encrypted. Soft revoke: a device that already held a generation can still decrypt those objects until they leave the mailbox |
| 10 History and restore | `relay history` and `relay restore` | No GUI history browser |
| 11 Text merge | Clean three-way merge when concurrent edits share a parent and do not overlap (D31) | Overlapping edits, binary files, Git metadata, and diverged vectors stay conflict copies |
| 12 Generalized networking | Dial ranking. User-run UDP relay (`relay transport`, D34) tried after direct addresses fail. Mailbox file `transport/relay` publishes that address. Object fetch asks other connected peers, then the mailbox (D34) | No public TURN account. No connection migration once a path is up. Pairing still needs a direct UDP path (D25) |
| 13 Selective materialization | Local `relay materialize` rules (`full`, `metadata`, `demand`, `exclude`); last match wins. `relay fetch` and `relay evict`. The scanner does not tombstone a path this device chose not to write. Mailbox push skips objects this device does not have (D35) | No GUI editor. No placeholder files or OS file-on-demand. Changing a rule does not delete files or send tombstones |

Android (D33) is not a numbered phase. It is the same Vue UI and in-process
engine, foreground only. Sync runs while the process is alive. Device data
lives in the app data directory. There is no tray, autostart, updater, or
bundled `relay` CLI. Mounts are the app sandbox; there is no folder picker.

## Later

In order after Phase 14, unless a decision says otherwise:

1. **Hosted durable replica.** The rest of Phase 8. The client architecture
   stays backend-independent. The filesystem mailbox is the only backend
   today.
2. **Android background sync** and a folder picker. iOS, Play Store signing,
   and photo-library access are out of scope until a decision adds them (D33).

Also still open:

- Nested `.relayignore` (only the mount-root file is read).
- Encrypting mailbox entry logs.
- GUI policy editor and GUI history.
- Transfer throughput for photos and video. Proposal only:
  [`proposals/transfer-throughput.md`](proposals/transfer-throughput.md).

## Accepted limits

These are current behavior, not accidental gaps:

- Peer-only mode (no mailbox and no `relay transport` address) is LAN,
  Tailscale, or a manual address. Hole punching needs a mailbox path (D32).
  The UDP relay needs one reachable address (D34).
- Two devices that cannot reach each other's UDP port cannot pair (D25).
  The relay is for sync after they have paired.
- A middle machine forwards files only when it has the mount attached (D26).
- Unclean text merges keep both versions as conflict copies (D31).
