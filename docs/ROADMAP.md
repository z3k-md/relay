# Roadmap

What is being built now, what comes next, and what already works. How it
works is [`DESIGN.md`](DESIGN.md); why is [`DECISIONS.md`](DECISIONS.md).
Plans that are not decisions yet live in [`proposals/`](proposals/).

## Now

**Home server** (tier 2, D44; [proposal](proposals/home-server.md)). An
optional, self-hosted, always-on Relay device in the sync chain: a durable
copy, history the other devices cannot erase, and rendezvous. Stage 1, the
server role (D45), shipped: `relay server enable`, auto-join and attach, a
container image, and a systemd unit. Stage 2, store mode (D47), shipped:
the server keeps bytes in its object store with an empty data folder.
Backup semantics (append-only, retention, snapshots) are next.

**Online-only files in the OS file manager**
([proposal](proposals/os-integration.md)). Windows Cloud Files placeholders
shipped (D43). Next: pins and sync status in Explorer, then the macOS File
Provider extension, then a Linux FUSE view.

**Android background sync** ([proposal](proposals/android.md)). Stage 1, a
CI debug APK build, shipped. Stage 2 runs the engine in a foreground sync
service (PR #19); then background catch-up, a DocumentsProvider, and shared
storage.


## Next

Not in a fixed order.

- **Accounts** (D44). A sign-in directory, Google first, that finds your
  devices; an existing device approves a new one. We run a default instance;
  self-hosters run it inside their home server.
- **Relay Explorer.** A Files-class file manager in the desktop app, Windows
  first. The WebView2 performance spike is draft PR #5.
- **Performance insights** ([proposal](proposals/performance-insights.md)).
  A connection test between two devices (D48) is in review; live per-peer
  graphs, `relay bench`, and history follow.
- **Integrations.** Editor sync status, conflict notifications, restore from
  the app, and post-sync automation. These stay above the engine.

## Later

- **Hosted server** (tier 3, D44). The home server binary, run for people
  without hardware. Blind by default; billing only here.
- **iOS.** Out until a decision adds it (D33).

- **Transfer throughput** for photos and video: resume, larger packets on
  direct paths, streamed mailbox objects, then chunks
  ([proposal](proposals/transfer-throughput.md)).

Smaller open items:

- Nested `.relayignore` (only the mount-root file is read).
- Encrypting mailbox entry logs.
- Policy editor and per-file history in the app (CLI only today).
- A timestamp in the `relay transport` bind frame, at the next protocol bump
  (D34).

## Works today

| Area | What ships | Decisions |
| --- | --- | --- |
| Local index | Watcher plus periodic scans, multi-mount, root `.relayignore`, mount markers, mass-delete guard | D7–D9, D13 |
| Direct sync | QUIC with pinned mutual TLS, index deltas, verified atomic writes, forwarding through members | D14–D17, D26 |
| Conflicts | Deterministic conflict copies, Git repository groups, clean three-way text merge, held peer mass deletes | D2, D18, D21, D22, D31 |
| Hosts | `relay run`, `relay service`, the desktop app; local IPC; live config changes | D19, D24, D36 |
| Pairing | Short code with SPAKE2, mDNS on the LAN, `--addr` elsewhere | D25 |
| Selection | Replication policies and device groups; per-device `full` / `metadata` / `demand` / `store` / `exclude` | D27, D35, D47 |
| Catch-up | Mailbox directory with ack-based GC and mirror mode | D29 |
| Encryption | Per-space keys for mailbox objects, recovery secret, revoke, rotate | D30 |
| Networking | Dial ranking, hole punching through the mailbox, user-run UDP relay, multi-source fetch | D32, D34 |
| History | `relay history` and `relay restore` | D5 |
| Desktop app | macOS and Windows, self-updating; Android foreground shell | D20, D33 |
| Remote management | Manage grant, Browse another device, folder pairs from either side, open or copy remote files, folder sizes | D37–D42, D46 |
| Online-only files | Files view choices; Cloud Files placeholders on Windows | D38, D43 |
| Home server | Server role, store mode, container, systemd unit | D45, D47 |
| Testing | `relay-sim` lab, `relay-vopr` deterministic simulator, tiered CI | [`DEVELOPMENT.md`](DEVELOPMENT.md) |

## Accepted limits

Current behavior, not accidental gaps:

- Peer-only mode (no mailbox and no `relay transport` address) is LAN,
  Tailscale, or a manual address. Hole punching needs a mailbox (D32). The
  UDP relay needs one reachable address (D34).
- Two devices that cannot reach each other's UDP port cannot pair (D25).
  The relay is for sync after they have paired.
- A middle machine forwards files only when it has the mount attached (D26).
- Unclean text merges keep both versions as conflict copies (D31).
- The local object store and working tree are plaintext; only mailbox
  objects are sealed (D30). A soft revoke cannot take back data a device
  already decrypted.
