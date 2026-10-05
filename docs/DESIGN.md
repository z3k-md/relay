# Relay design

How Relay works today and the rules it keeps. Details and the reasons behind
them are in [`DECISIONS.md`](DECISIONS.md), cited as D-numbers. What is
shipping next is [`ROADMAP.md`](ROADMAP.md). Where this file and a decision
disagree, the decision wins; fix this file.

---

# 1. What Relay is

Every participating device works from ordinary local files. Relay keeps
selected folders the same across a user's own devices by reconciling those
local copies, peer to peer, with an optional always-on copy for devices that
are never online together.

- **Relay** is continuous working state. **Git** stays intentional history,
  branches, and review. `.git` syncs like any other folder (D1, D21).
- **Self-hosted end to end** (D44). No third-party backend. Three tiers:
  1. Sync between your own devices, with no account and no server.
  2. An optional home server you run, a peer in the sync chain that keeps a
     durable copy and history ([`proposals/home-server.md`](proposals/home-server.md)).
  3. Later, the same server hosted as a paid service, blind by default.
- **Platforms.** Desktop app on macOS and Windows (D20), headless `relay` on
  macOS, Windows, and Linux, Android as a foreground shell (D33).

# 2. Model

```text
Device ── trusts ── Device
Space
├── Mounts ── Entries (one logical path each)
├── Shares (which peers sync it)
├── Replication policies and device groups
└── Materialization rules (per device)
```

- A **device** is an Ed25519 key; its `DeviceId` is the public key (D14).
- A **space** is a sync namespace: a set of mounts shared with chosen peers.
- A **mount** maps one logical folder to one physical path on each device that
  attaches it. `D:\Code` on Windows and `~/Code` on a Mac are the same mount.
- An **entry** is identified by `(SpaceId, MountId, RelativePath)`, never by
  an OS path. Logical paths are relative, `/`-separated, Unicode NFC (D9).
  Names a platform cannot hold are flagged not portable and skipped there,
  never deleted (D9, D16).
- Each device keeps its index in SQLite (`relay.db`, WAL) and file contents
  in a BLAKE3 content-addressed object store.

# 3. Which files go where

Three layers, each narrower than the last:

1. **Mount rules.** Include/exclude globs and the mount-root `.relayignore`
   decide what is part of the mount at all (D13). Case-insensitive on macOS
   and Windows. Nested `.relayignore` is not read.
2. **Replication policies** (D27). `selector -> devices or groups`. With no
   policies, a shared space syncs everything to every peer it is shared with.
   Overlapping policies union. A policy never grants sync without a share.
3. **Materialization** (D35, D38). Each device decides what it does with a
   path it receives: `full`, `metadata`, `demand` (online-only until opened),
   or `exclude`. Last matching rule wins. On Windows, online-only files are
   Cloud Files placeholders in Explorer (D43;
   [`proposals/os-integration.md`](proposals/os-integration.md) for macOS and
   Linux).

Changing any rule stops future sync. It never deletes files on disk or sends
tombstones (§9.6).

# 4. Sync

Relay reconciles state; it does not forward events.

- **Change detection.** Watcher events are hints that a path may have
  changed. They are debounced (200 ms, 2 s max) into partial scans, and a
  periodic full scan (10 min) heals missed events (D13). A file is hashed
  from an open handle and discarded if it changed mid-read (D8).
- **Content.** One object per file version, `ObjectId = BLAKE3(bytes)`. No
  chunking yet ([`proposals/transfer-throughput.md`](proposals/transfer-throughput.md)).
- **Versions.** Each entry has a version vector. Comparison is `Same`,
  `LocalNewer`, `RemoteNewer`, `ConcurrentIdentical` (merge silently), or a
  conflict. Clocks never order versions (D3). Every version records its
  parent object and is kept in history (D5).
- **Index exchange.** Each device numbers its own changes with a monotonic
  sequence. Peers ask for "changes after N" per space and ack with
  watermarks (D4, D16). A batch is applied only after all its objects are in
  the store. A missing object holds the watermark; entries are never dropped
  silently.
- **Applying.** Downloads are verified, staged in the destination folder,
  synced, re-checked against the index, and renamed into place (D8, D17). A
  local edit in the meantime aborts the write and becomes a conflict.
  Within a batch, deletes go first and Git refs go last (D1, D17).
- **Conflicts.** Resolved identically on every device from replicated data:
  live beats tombstone, directory beats file, otherwise a deterministic
  winner keeps the path and the loser becomes
  `name.relay-conflict-<device>-<counter>.ext` (D2, D18). One `.git` directory
  resolves as a group (D21). Non-overlapping concurrent text edits merge
  three-way instead (D31).
- **Deletes** are versioned tombstones. A scan that would delete a large
  share of a mount is refused (D7), and a peer's mass delete is held until
  the user applies or restores it (D22).
- **Forwarding.** A device that has a mount attached forwards entries to the
  other members of that space (D26). Any member or the mailbox can supply an
  object (D34).

# 5. Durability

Online presence is not durability; acknowledgements are.

- **History.** Every version on every device stays in history until GC.
  `relay history` / `relay restore` (D5, D6).
- **Mailbox** (D29). A directory you control (a NAS share, a synced folder)
  that devices push entry logs and objects to and pull from when they are
  never online together. Acked entries are collected after a grace period;
  `--mirror` keeps the latest copy of every path. Objects are sealed with the
  space key (D30).
- **Home server** (D44, D45). A Relay device with the server role joins every
  space its peers share with it and keeps a full copy. Store mode, durability
  targets, server-owned history, and rendezvous are the next stages
  ([`proposals/home-server.md`](proposals/home-server.md)). The server is a
  peer, never the authoritative filesystem.

# 6. Trust and keys

- **Pairing** (D25). A 10-digit code drives SPAKE2 over the sync port, bound
  to both TLS certificates. mDNS finds the other device on a LAN; elsewhere
  the joiner gives an address. `relay peer add` is the manual fallback.
- **Sessions.** QUIC with mutual TLS 1.3, each side pinned to the other's
  key. No CA, no TOFU (D14).
- **Spaces.** A space syncs with a peer only when both have shared it.
  Joining adopts the space's other members as peers (D26).
- **Encryption at rest.** Each space has a random 256-bit key. Mailbox
  objects are sealed with it (XChaCha20-Poly1305); each member's copy is
  wrapped with its X25519 box key. The local store, the working tree, and
  entry logs are plaintext. `relay recovery` exports a secret that unwraps
  every space key; `relay peer revoke` and `relay space rotate` handle a lost
  device (D30).
- **Manage grant** (D37, D46). A peer granted `may_manage` can browse this
  device, set up folder pairs on it (D39), open files from it (D40, D41), and
  nothing more: no peers, grants, or policies, and never credential stores
  or the Relay home.
- **Accounts** (planned, D44). An OIDC sign-in directory that finds your
  devices. It holds emails, device names, public keys, and addresses, never
  keys or contents. A new device gets space keys only when an existing
  device approves it. Code pairing stays, and the engine never depends on an
  account.

# 7. Network

- One UDP port (47321) per device: QUIC for sync (`relay/1`) and pairing
  (`relay-pair/1`).
- Dial order ranks loopback, LAN, Tailscale, link-local, DNS names, then
  public addresses (D34).
- With a mailbox, devices exchange STUN candidates through it and hole-punch
  (D32).
- When nothing direct works, a user-run UDP forwarder (`relay transport`)
  carries the QUIC packets. The session stays end to end (D34).
- Pairing still needs a direct UDP path (D25).

# 8. Processes

- **One host per Relay home.** `relay run`, `relay service` (a LaunchAgent on
  macOS, a scheduled task on Windows), the desktop app, or the server
  container and systemd unit on Linux (D45). A host lock enforces one (D24).
- **Threads.** The engine owns the database on one loop thread. Networking is
  a Tokio runtime on another, connected by channels. The network writes
  verified objects into the store and never touches the database (D19).
- **Local IPC.** A Unix socket or named pipe, newline-delimited JSON. The CLI
  and the app steer a running host through it (D24).
- **Configuration is data.** Every edit is a `ConfigChange` applied on the
  live loop, whether it comes from the CLI, the app, or a managing peer, so
  sessions stay up (D36). Transport, mailbox, recovery, and key changes
  reload the host.
- **Desktop app.** Tauri 2 and Vue 3, engine in-process, CLI bundled,
  self-updating from GitHub Releases (D20, [`RELEASING.md`](RELEASING.md)).

# 9. Safety invariants

These hold everywhere. A change that breaks one needs a decision first.

1. **Never silently discard divergent content.** Concurrent versions merge
   cleanly or are kept as conflict copies.
2. **Never write unverified network content into live files.** Hash, stage,
   then rename.
3. **Never trust filesystem events as authoritative.** Scans heal them.
4. **Never identify entries by absolute OS path.** Use space, mount, and
   relative path.
5. **Never treat online presence as proof of durable replication.**
6. **Never delete local user data because a rule, policy, share, or mount
   changed.** Stop syncing first; cleanup is a separate, explicit action.
7. **Mounts never overlap** each other or the Relay home. One physical file
   has one logical identity.
8. **When unsure, keep the data and ask** (mount markers, mass-delete guards,
   held deletes).

# 10. Code layout

```text
crates/
  relay-core     pure domain types: ids, paths, vectors, conflicts, merge, config changes
  relay-policy   include/exclude globs and .relayignore
  relay-fs       scan, watch, mount markers, safe paths, atomic writes, Cloud Files
  relay-store    BLAKE3 object store
  relay-db       SQLite schema, migrations, index, history
  relay-replica  mailbox backend (a directory)
  relay-crypto   device identity, certificates, space keys, sealing
  relay-proto    peer wire protocol (protobuf)
  relay-net      QUIC, pinned TLS, pairing, mDNS, hole punching, UDP relay
  relay-engine   scan, sync, apply, policies, materialization, placeholders
  relay-daemon   the host loop: network + engine, remote management
  relay-ipc      local RPC between a host and its clients
apps/
  relay-cli      the relay binary
  relay-desktop  Tauri app (macOS, Windows, Android)
  relay-sim      multi-process lab
  relay-vopr     deterministic single-process simulator
```

Where things happen:

| Step | Code |
| --- | --- |
| Host startup, the loop that joins engine and network | `relay-daemon/src/lib.rs`; desktop: `relay-desktop/src-tauri/src/runner.rs` |
| Local IPC server and methods | `relay-daemon/src/host.rs`, `relay-ipc/src/protocol.rs` |
| Watcher events become partial scans | `relay-engine/src/watch/`, `relay-fs/src/watch.rs` |
| Scan a mount into the index | `relay-engine/src/scan.rs`, `relay-fs/src/scan.rs` |
| Index tables and queries | `relay-db/src/repo.rs`, `relay-db/src/migrate.rs` |
| Peer frames in and out (sans-I/O `Syncer`) | `relay-engine/src/sync/mod.rs` |
| Index exchange, watermarks, acks | `relay-engine/src/sync/index.rs` |
| Object fetch, retry across peers and mailbox | `relay-engine/src/sync/fetch.rs` |
| Apply a remote batch, conflicts, merge | `relay-engine/src/apply.rs`, `relay-core/src/conflict.rs`, `relay-core/src/merge.rs` |
| Atomic writes into the working tree | `relay-fs/src/materialize.rs` |
| Offers, membership, policy snapshots | `relay-engine/src/sync/offers.rs`, `relay-engine/src/policies.rs` |
| Held mass deletes | `relay-engine/src/sync/deletes.rs` |
| Live config changes | `relay-engine/src/live_config.rs`, `relay-engine/src/config.rs` |
| Mailbox push and pull | `relay-engine/src/replica.rs`, `relay-replica/` |
| QUIC sessions, object streams | `relay-net/src/session.rs`, `relay-net/src/tls.rs` |
| Pairing, mDNS, STUN, UDP relay | `relay-net/src/{pairing,discovery,stun,relay}.rs` |
| Remote management calls | `relay-net/src/control.rs`, `relay-daemon/src/remote.rs`, `relay-daemon/src/folder_pair.rs` |
| Online-only files and placeholders | `relay-engine/src/materialize.rs`, `relay-engine/src/placeholders.rs`, `relay-fs/src/cloud/` |
| CLI commands | `relay-cli/src/main.rs` |

The workspace forbids `unsafe`. The engine does not depend on networking or
Tokio, so it runs the same under `relay-vopr`'s virtual clock.

# 11. Testing

- Unit tests for deterministic logic (vectors, rules, conflicts, paths).
- Two engines in process (`crates/relay-engine/tests/sync.rs`) and QUIC on
  localhost (`crates/relay-net/tests/net.rs`).
- `relay-sim`: real daemon processes against temporary homes.
- `relay-vopr`: many engines on a virtual clock and network with injected
  faults. A seed replays exactly. CI tiers are in
  [`DEVELOPMENT.md`](DEVELOPMENT.md).
