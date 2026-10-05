# Design decisions

Each entry records a choice the implementation made and why. Code comments
cite these numbers, so a superseded entry stays as a one-line pointer to what
replaced it. [`DESIGN.md`](DESIGN.md) summarizes the current system; when it
and an entry disagree, the entry wins. What is built and what is next is
[`ROADMAP.md`](ROADMAP.md).

Add an entry with the next free number (check open pull requests), then run
`python3 scripts/check-docs.py --write` to refresh this index.

<!-- index:start -->

| | Decision |
| --- | --- |
| D1 | [`.git` is synchronized like any other directory](#d1-git-is-synchronized-like-any-other-directory) |
| D2 | [Conflict resolution is deterministic and replicated](#d2-conflict-resolution-is-deterministic-and-replicated) |
| D3 | [Version counters survive database restores](#d3-version-counters-survive-database-restores) |
| D4 | [Per-device change sequence instead of per-entry acknowledgements](#d4-per-device-change-sequence-instead-of-per-entry-acknowledgements) |
| D5 | [Merge bases are recorded from the first release](#d5-merge-bases-are-recorded-from-the-first-release) |
| D6 | [Object store GC is mark-and-sweep](#d6-object-store-gc-is-mark-and-sweep) |
| D7 | [Mount markers and mass-delete protection](#d7-mount-markers-and-mass-delete-protection) |
| D8 | [Safe reads and writes of live files](#d8-safe-reads-and-writes-of-live-files) |
| D9 | [Logical path rules](#d9-logical-path-rules) |
| D10 | [Device-local integer references in the database](#d10-device-local-integer-references-in-the-database) |
| D11 | [Superseded by D14](#d11-superseded-by-d14) |
| D12 | [Superseded by D24 and D36](#d12-superseded-by-d24-and-d36) |
| D13 | [Watching](#d13-watching) |
| D14 | [Device identity and transport security](#d14-device-identity-and-transport-security) |
| D15 | [Pairing, addressing and sharing](#d15-pairing-addressing-and-sharing) |
| D16 | [Index exchange](#d16-index-exchange) |
| D17 | [Applying a remote version](#d17-applying-a-remote-version) |
| D18 | [Conflicts](#d18-conflicts) |
| D19 | [Daemon threading](#d19-daemon-threading) |
| D20 | [Desktop app, releases and auto-update](#d20-desktop-app-releases-and-auto-update) |
| D21 | [Git repository conflict groups and conflict resolution](#d21-git-repository-conflict-groups-and-conflict-resolution) |
| D22 | [Receive-side mass-delete guard](#d22-receive-side-mass-delete-guard) |
| D23 | [Batched object durability on macOS](#d23-batched-object-durability-on-macos) |
| D24 | [Local IPC and the host lock](#d24-local-ipc-and-the-host-lock) |
| D25 | [Device pairing and LAN discovery](#d25-device-pairing-and-lan-discovery) |
| D26 | [Space membership](#d26-space-membership) |
| D27 | [Replication policies](#d27-replication-policies) |
| D28 | [Sync progress is a snapshot](#d28-sync-progress-is-a-snapshot) |
| D29 | [Durable mailbox](#d29-durable-mailbox) |
| D30 | [Encryption at rest](#d30-encryption-at-rest) |
| D31 | [Automatic text merge](#d31-automatic-text-merge) |
| D32 | [NAT hole punching](#d32-nat-hole-punching) |
| D33 | [Android is a foreground Tauri shell](#d33-android-is-a-foreground-tauri-shell) |
| D34 | [Generalized networking](#d34-generalized-networking) |
| D35 | [Selective materialization](#d35-selective-materialization) |
| D36 | [Configuration changes on the running host](#d36-configuration-changes-on-the-running-host) |
| D37 | [Remote management](#d37-remote-management) |
| D38 | [Files view and folder choices](#d38-files-view-and-folder-choices) |
| D39 | [Folder pairs set up from either device](#d39-folder-pairs-set-up-from-either-device) |
| D40 | [Opening a file that is not synced here](#d40-opening-a-file-that-is-not-synced-here) |
| D41 | [Read-only copies](#d41-read-only-copies) |
| D42 | [Sizes on disk and a faster Browse view](#d42-sizes-on-disk-and-a-faster-browse-view) |
| D43 | [Online-only files in Explorer](#d43-online-only-files-in-explorer) |
| D44 | [Self-hosted tiers and accounts](#d44-self-hosted-tiers-and-accounts) |
| D45 | [Server role](#d45-server-role) |
| D46 | [What a manage grant reaches](#d46-what-a-manage-grant-reaches) |
| D47 | [Store mode](#d47-store-mode) |

<!-- index:end -->

## D1. `.git` is synchronized like any other directory

The whole working tree, including `.git`, replicates. Relay never runs Git
commands and has no Git-specific logic beyond the following safeguards, which
are generic rules that happen to matter most for Git:

- **Transient files are excluded by default**: `**/.git/**/*.lock`,
  `.git/gc.pid`, `.git/gc.log`, and fsmonitor state. A replicated lock file
  would make Git on the other device refuse to run.
- **Immutable before mutable.** Within a directory containing
  `.git/`, content under `objects/**` is materialized before `HEAD`, `refs/**`,
  `packed-refs` and `index`, so a ref never points at an object that has not
  arrived yet.
- **Repository-level conflict groups (D21).** Concurrent versions of the
  mutable files in one `.git` directory resolve to the same device's version.
  Losing refs become conflict copies such as
  `refs/heads/main.relay-conflict-<device>-<counter>`, which Git shows as
  ordinary branches.
- **Recommended Git settings** (Relay will check and warn, not enforce):
  `core.checkStat=minimal`, `core.trustctime=false`, `core.autocrlf=false`.
  With Relay preserving mtimes on materialization, this keeps a replicated
  `.git/index` valid on both machines, so it is not re-hashed and rewritten on
  every `git status`.

## D2. Conflict resolution is deterministic and replicated

A conflict is resolved identically on every device, from replicated data only:

- The version whose writing device holds the higher counter keeps the original
  path; ties break on device id (`relay_core::conflict::choose_winner`).
- The loser is written to `<name>.relay-conflict-<device>-<counter>`. The
  original extension is intentionally no longer last, so `foo.go` conflict
  copies do not break Go builds and `test_x.py` copies are not collected.
- Concurrent versions with identical content are **not** conflicts
  (`VersionRelation::ConcurrentIdentical`): vectors merge silently. This is the
  common case of running the same `git pull` on two machines.
- Equal vectors with different content (`VersionRelation::Diverged`) indicate a
  reused counter. They are preserved as a conflict and reported.

## D3. Version counters survive database restores

`VersionVector::bump` sets the counter to `max(previous + 1, now_unix_secs)`.
Clocks are never used for ordering; this only keeps a device restored from an
old backup from reissuing counters it already used.

## D4. Per-device change sequence instead of per-entry acknowledgements

Every change a device commits to its index gets the next value of a persistent,
monotonic `Sequence`. Index exchange (D16) is "changes after N", and
acknowledgements are per-peer watermarks. The `replica_acks` table from the
design is replaced by this.

## D5. Merge bases are recorded from the first release

Each version stores `parent_object`, the content it was derived from, and every
version is appended to `history`. Three-way merge (D31) needs bases that
cannot be reconstructed later. GC treats history as a root.

## D6. Object store GC is mark-and-sweep

No reference counts. Live objects are everything referenced by `entries` or
`history`; the sweep skips objects younger than a grace period so a put whose
database commit has not landed yet is never collected.

## D7. Mount markers and mass-delete protection

- Every mount root holds a `.relay-mount` marker with its Space and Mount ids.
  A scan refuses to run if the root or marker is missing or belongs to another
  mount, so an unplugged drive or moved folder never becomes "delete
  everything".
- A scan that would tombstone a large fraction of a mount is refused unless
  explicitly forced.
- A subdirectory containing its own marker is another mount and is skipped.
- Entries under a path the scan could not read (permission denied, non-UTF-8
  or invalid names, nested mounts) are protected: they are not tombstoned by
  that scan.
- Entries that are no longer selected because a rule or `.relayignore`
  changed are reported as deselected and never tombstoned (DESIGN §9.6).
- A file that changes while it is being hashed is reported as unstable and left
  untouched until the next scan.

## D8. Safe reads and writes of live files

- Reading: a file is hashed from an open handle, and its metadata is compared
  before and after. If it changed, the result is discarded and retried later.
- Writing: content is staged in a `.relay-tmp-*` file **in the destination
  directory** (same volume, so rename is atomic), verified, synced, and the
  destination is re-checked immediately before the rename. If the user changed
  the file in the meantime, the write is abandoned and the edit is treated as a
  local change.
- `sync_all` is used for durability (on macOS Rust uses `F_FULLFSYNC`), and the
  parent directory is fsynced on Unix.
- On Windows, change detection relies on size + 100ns mtime (no ctime or file
  id from std), like other sync tools; periodic full scans don't rehash
  either, so an mtime-preserving same-size overwrite on Windows is a
  documented blind spot.

## D9. Logical path rules

- Paths are Unicode NFC, so macOS (NFD-preserving) and Windows spellings of the
  same name are one identity.
- Looking up an existing path walks each component and, if the NFC spelling is
  missing, accepts the unique (or byte-order-first) on-disk name whose NFC form
  matches; `to_os_path` only constructs destinations for new files.
- Include/exclude globs are case-insensitive on macOS and Windows (matching
  those platforms' default filesystems) and case-sensitive elsewhere.
- Names that are legal locally but cannot exist on another platform (Windows
  reserved names, `:` `?` `*` etc., trailing dots or spaces, over-long
  components) are valid identities and flagged as not portable, rather than
  silently skipped.
- Case-insensitive collisions are detected at scan time.

## D10. Device-local integer references in the database

Version vectors are stored normalized with a small integer reference per
device instead of repeating 32-byte ids on every row.

## D11. Superseded by D14

Device identity before peer authentication (random ids).

## D12. Superseded by D24 and D36

The CLI embeds the engine when no host runs and talks to a running host over
local IPC. There is no separate `relayd`.

## D13. Watching

Filesystem events are hints only; they never update the index directly. A
debounce of 200ms, or a 2s max batch delay from the first event in a burst,
coalesces editor save storms into one incremental `scan_paths`. The scope of
a partial scan is the `PartialScan` from `relay_fs`: `Exact` is that path
only; `Subtree` is the path and everything under it. A periodic full scan
(default 10 minutes) heals missed events. `--poll` disables the native
watcher and relies on those periodic scans. Changing the root `.relayignore`
or the mount marker forces a full scan (rules or mount identity changed).
Nested `.relayignore` files are not yet supported.

## D14. Device identity and transport security

Supersedes D11. Each device owns an Ed25519 key at `<home>/identity/device.key`
(PKCS#8 PEM, mode 0600 on Unix). `DeviceId` is the raw 32-byte public key.
A self-signed certificate is regenerated from the key at every start.

Peers talk QUIC (quinn) with TLS 1.3 (rustls, `ring` provider) and ALPN
`relay/1`. Both sides present certificates. Each side accepts a certificate
only if its Ed25519 key equals the id of a peer the user added with
`relay peer add`; anything else fails the handshake. There is no CA and no
TOFU.

## D15. Pairing, addressing and sharing

Pairing is manual and mutual: each device runs `relay peer add <name> <id>
[--addr host:port]` for the other. At least one side needs an address; both
dial every peer with an address and accept from any trusted peer. When two
connections to the same peer exist, the one dialed by the lower device id
survives. The default listen address is `0.0.0.0:47321/udp`.

A Space is synced with a peer only if both devices have it (same `SpaceId`)
and both have shared it with each other (`relay share <space> <peer>`).
On connect each device sends `SpaceOffers` for spaces it shares with that
peer. `relay space join <name> --from <peer>` creates the offered space and
its mounts locally with the offered ids and shares it back with that peer.
`relay mount add` on an existing (offered) mount attaches a local path to it.
Attaching resets the receive watermark of that space for all peers to zero,
so entries skipped while the mount had no local path are re-sent.

## D16. Index exchange

Per (peer, space) each side keeps two watermarks, both in the peer's or our
sequence space as noted:

- `received`: highest *peer* sequence durably applied. Sent in
  `IndexRequest.after_sequence` on every connect.
- `acked`: highest *local* sequence the peer has acknowledged. Used only for
  status ("in sync" when `acked` equals our latest sequence touching the
  space).

The responder streams `changes_since` restricted to the space in batches of at
most 1000 entries in sequence order, then pushes new batches whenever local
sequences are committed. A batch is applied as a unit: every object it needs
is fetched into the store first, then entries are applied, then `received`
advances to `through_sequence` and an `Ack` is sent. A dropped connection
re-requests from `received`; reapplying is idempotent (`Same`).

Entries for mounts with no local path, entries excluded by local mount rules,
and entries that cannot exist on this OS (Windows-invalid names, symlinks on
Windows, case-insensitive collisions with a different live entry) are skipped
with a warning and **not stored**, so a local scan can never turn them into
tombstones.

## D17. Applying a remote version

`compare_versions(local, remote)`:

- `Same`, `LocalNewer`: nothing.
- `ConcurrentIdentical`: store the merged vector (content unchanged).
- `RemoteNewer`: materialize, then store the remote record unchanged (vector,
  `modified_by`, `parent_object`, `modified_at`) with a fresh local sequence
  and the stat observed after writing. Files are written with
  `materialize_file` and `expected_existing` = the local record's stat, so a
  local edit that has not been scanned yet aborts the write
  (`DestinationChanged`); the path is then re-scanned and reconciled again,
  which turns it into a conflict. A missing stat (racy) is verified by
  re-hashing the on-disk file first. Deletions remove a file only if it still
  matches the local record; directories are removed only when empty.
- `Conflict`, `Diverged`: see D18.

File mtimes are set to the sender's observed mtime when known, so Git's stat
checks on the other device match.

Order inside a batch, both when a sender assigns sequences in a scan and when
a receiver applies: tombstones of files, tombstones of directories (deepest
first), directory creations, file and symlink writes outside `.git`, `.git`
content other than refs, and finally `.git` refs (`HEAD`, `ORIG_HEAD`,
`FETCH_HEAD`, `MERGE_HEAD`, `packed-refs`, `refs/**`, `index`). Deleting before creating
makes case-only renames safe on case-insensitive filesystems; refs last keeps
objects-before-refs (D1) even when a large change spans batches.

## D18. Conflicts

For concurrent versions L (local) and R (remote), every device computes the
same result from replicated data only:

1. If exactly one side is a tombstone, the live side wins; no copy.
2. If kinds differ and one is a directory, the directory wins; the other goes
   to a conflict copy.
3. Otherwise the winner is chosen by `choose_group_winner` for Git metadata
   (see D21) or `choose_winner` for everything else.

The path gets the winner's content with vector `merged(L, R)` and no bump
(bumping would make each device's result different and never converge). The
loser is stored at `conflict_path(path, loser.modified_by,
loser.vector[loser.modified_by])` with the loser's own vector, content,
`modified_by` and `parent_object`. Before overwriting a local loser the local
bytes are already in the object store, so the copy is materialized from the
store. `relay conflicts` lists live conflict copies. Git metadata copies are
grouped per repository (D21).

## D19. Daemon threading

`relay run` owns the Engine on one thread. The network runs a tokio runtime on
its own thread (`relay-net`). They talk over channels: the engine sends
`NetCommand`s; the network delivers `NetEvent`s through a callback into the
engine's single input queue, which also carries watcher signals. The network
writes fetched objects into the object store directly (verified by hash
before rename) and serves objects from it; it never touches the database.

## Sync implementation notes

- `to_os_path` returns `Result` and rejects any logical component that is not
  exactly one `Component::Normal` on the current OS (`FsError::Unrepresentable`).
  `materialize_file` takes `MaterializeOptions { mount_root, mtime_ns }` and
  creates parents one real directory at a time (`FsError::UnsafeAncestor`).
- Unknown devices referenced by a version vector are stored with a placeholder
  name equal to the short id (not `"unknown"`).
- On Windows the scanner cannot observe the executable bit. The engine carries
  the existing record's bit forward so a Mac `executable: true` file is not
  rewritten as a new version.
- The engine does not depend on `relay-net` or Tokio. `Engine::run` multiplexes
  filesystem events and `SyncInput`s; the daemon maps `NetEvent`/`NetCommand`
  1:1 onto `SyncInput`/`SyncOutput`. `Engine::watch` calls `run` with no sync.
- Object fetches that fail for a reason other than "not found" are retried up
  to `MAX_FETCH_ATTEMPTS` times. If an entry's object still cannot be fetched,
  the receiver records a hole: `received_seq` (persisted and acked) is held
  below that entry's sender sequence, and after `RESYNC_DELAY` the receiver
  sends `IndexRequest { after_sequence: hole }`. `IndexBatch.after_sequence`
  tells the receiver which request a batch answers; a batch starting at or
  below the hole that applies cleanly clears it. Entries are never dropped
  silently.
- Duplicate connections: normally the one dialed by the lower DeviceId is kept.
  A new connection dialed by the same device as the existing one replaces it,
  because the old one is stale (the peer restarted or its network changed).
  Otherwise a restarted peer waited for the idle timeout before reconnecting.
- Object import calls `sync_all` on a handle opened for writing. On Windows,
  `FlushFileBuffers` on a read-only handle fails with "Access denied".

## D20. Desktop app, releases and auto-update

Desktop users run a Tauri 2 + Vue app (`apps/relay-desktop`). The sync
runner is hosted in-process (the same engine the CLI embeds). The `relay`
CLI is also bundled as a sidecar (`bundle.externalBin`) and placed on PATH
so terminal workflows keep working.

Releases are produced by GitHub Actions only when the release workflow is
dispatched (`.github/workflows/release.yml`). A normal push does not
publish a version. The workflow bumps `[workspace.package] version` (patch
by default), the Tauri and package.json `"version"` fields, commits, and builds.
macOS is a universal (`aarch64` + `x86_64`)
ad-hoc-signed `.dmg` (no notarization yet). Windows is a per-user NSIS
installer (no admin). Linux desktop builds are not shipped; headless
`relay service` remains the path for SSH and servers (see
`scripts/deploy.sh`).

Auto-updates use a Tauri minisign key. The public key lives in
`tauri.conf.json` (`plugins.updater.pubkey`); the private key is the
`TAURI_SIGNING_PRIVATE_KEY` Actions secret. `latest.json` is uploaded to
GitHub Releases and the updater endpoint is
`https://github.com/OWNER/REPO/releases/latest/download/latest.json`.
That URL must be publicly downloadable: if the source repo is private,
set the `RELEASE_REPO` variable and `RELEASE_TOKEN` secret so artifacts
go to a public repo and point the endpoint there. Losing the private key
means existing installs can never auto-update again.

Running apps fetch `latest.json` at startup and every 5 minutes of
wall-clock time. The file is about 1 KB, so a short interval costs nothing,
and measuring wall-clock time rather than sleeping a fixed interval means a
machine that wakes from sleep checks within one 30-second tick.

## D21. Git repository conflict groups and conflict resolution

Concurrent versions of mutable files inside one `.git` directory (anything
under `.git/` except `<gitdir>/objects/**` and the `.git` directory entry
itself) resolve as a group. The winner is the version whose writing
`DeviceId` is greater (`choose_group_winner`). If both sides were written by
the same device, the per-file rule (`choose_winner`) is the fallback.

Device-id rank is used instead of per-file counters because counters are
~unix seconds (D3): two files written in the same second on opposite devices
can otherwise be awarded to different devices (HEAD from one, `index` from
the other) and leave a repository that Git will not open. Object files stay
on the per-file rule; they are content-addressed and grouping them would
only hide a real divergence. The `.git` directory entry itself is a
directory identity, not metadata. A file named `.gitignore` is not Git
metadata.

Every device computes the same result from replicated data only. Copy naming
is unchanged: a losing `refs/heads/main` becomes
`refs/heads/main.relay-conflict-<device>-<n>`, which Git shows as an ordinary
branch.

`relay conflicts` classifies each live copy as `File { original }` or
`Git { git_dir, is_ref }` (`is_ref` means the copy is under `<git_dir>/refs/`).
Git copies are grouped by `(space, mount, git_dir)` so a repository is one
summary, not a pile of metadata files.

Resolution is a filesystem operation inside the mount (`KeepCurrent` deletes
the copy; `UseCopy` atomically replaces the original with the copy's current
on-disk bytes, then deletes the copy). It works whether or not a sync loop
is running; a running watcher records the change. After the edit the engine
tries an exclusive `Engine::open` and a partial scan of the touched paths so
the index updates immediately. If the run lock is held (`EngineError::Running`
or busy), scanning is skipped. `resolve_git_conflicts` deletes metadata
copies under that `.git` directory; ref copies stay unless `--branches` so
the user can merge in Git. Prior versions remain in history.

## D22. Receive-side mass-delete guard

A scan already refuses to tombstone a large fraction of a mount (D7). That
does not stop a peer from sending those tombstones (bad peer state, an
accidental `rm -rf` on the other machine, or a bug). Relay preserves data
when unsure, so the receiver holds a peer mass delete until the user decides.

- **Detection.** In `Syncer::process_head`, before `apply_remote_batch`, count
  tombstones that would delete a live local entry: the local entry is live
  and the tombstone's vector dominates it. Concurrent tombstones lose to the
  live side (D18) and are not counted. A per-connection,
  per-(space, mount) counter tracks deletions already applied in the current
  catch-up and resets when a `caught_up` batch for that space has been
  applied. The same map also records the paths whose local entry actually
  became a tombstone from those applies. Baseline live is the current live
  count (the same `count_live` the scan guard uses) plus deletions applied
  so far. The guard trips when `is_large_fraction_delete(session + batch,
  baseline)` is true — the same function and thresholds as the scan-side
  check (at least 25 entries and more than half the mount).
- **Hold.** If there is no stored decision for (peer, space, mount), persist a
  hold and the held paths (this batch and any already queued for that peer
  and space), plus the already-applied session paths marked as applied.
  Emit `DeletesHeld` once per hold per connection. Do not apply the batch,
  advance `received`, or ack; that (peer, space) queue waits. Other spaces
  and peers keep syncing. A reconnect re-requests from the unchanged
  watermark.
- **Decisions.** Written through `Engine::open_for_config` (`relay deletes
  apply|restore`, or the desktop buttons). A running loop reloads on the
  database change and recreates the Syncer.
  - A decided hold is kept until a `caught_up` batch for the space is applied,
    then deleted so a future mass delete is held again. The decision is read
    from the database on every batch, so it covers the whole catch-up and
    survives reconnects.
  - `apply` lets batches through unguarded.
  - `restore`, on every batch, re-asserts each local file that a tombstone in
    the batch would delete (and, the first time, every held path), provided
    the entry is still live and the file still matches the index: a new
    local version with the same content and a bumped vector (fresh sequence,
    history row, sent to peers). That version is concurrent with the peer's
    tombstone; D18 rule 1 (exactly one side is a tombstone → the live side
    wins, no copy) keeps the file here and brings it back on the peer. Paths
    that changed locally are skipped (the next scan records the change, which
    also beats the tombstone). The first restore batch also resurrects each
    applied path when the local entry is a tombstone last modified by that
    peer, nothing is on disk now, and the most recent earlier history version
    is a regular file whose object is still in the store: the file is written
    back and a new local version is recorded whose vector dominates the
    tombstone, so the peer applies it as a re-create. Symlinks and other
    non-file kinds are skipped; a path that fails a precondition is skipped;
    other per-path failures become `SyncWarning` and do not abort the batch.
    The held path list (including applied paths) is emptied after the first
    re-assert.
- **Limit.** Tombstones arrive in index batches (D16) and the guard only sees
  the batches so far, so a delete spread over several batches trips once the
  running total passes the threshold; the deletes before that are applied.
  Those applied paths are tracked on the connection and persisted with the
  hold, so `restore` brings them back too. Counters and path lists are per
  connection, so deletes applied before a reconnect that preceded the hold
  are not tracked.

## D23. Batched object durability on macOS

A first scan of a large tree was dominated by object-store flushes, not CPU.
On Apple platforms Rust's `File::sync_all` is `fcntl(F_FULLFSYNC)`, which
flushes the whole volume cache. `ObjectStore::put_file` did that twice per
new object (temp file, then parent directory), so N files cost 2N full
drive flushes.

`ObjectStore::batch()` returns a `PutBatch`. Scan uses one batch for the
plan and calls `commit()` after the plan is accepted (after the mass-delete
guard) and **before** the SQLite transaction that records those objects.
Restore, remote fetch/`import_verified`, and `put`/`put_file` from bytes
are unchanged. A dropped batch installs nothing.

**Apple.** Each `PutBatch::put_file` copies and hashes into a tmp file,
`fsync`s it (not `F_FULLFSYNC`), then closes the fd and keeps a `TempPath`.
Duplicates in the same batch (and objects already in the store) report
`already_present` and drop the tmp. Staged names are not visible to
`contains` or other callers. `commit()` then:

1. One `F_FULLFSYNC` on a staged tmp file. This flushes the drive cache,
   so every staged tmp's bytes are durable. It runs on a regular file (as
   `sync_all` did before) and a failure fails the commit.
2. Rename each tmp onto its content-addressed path (same "destination
   already exists → verify → success" handling as a single put).
3. Plain `fsync` of each touched object directory.
4. A final `F_FULLFSYNC` on the store root so those directory entries are
   durable. Best effort, like the directory fsync before: a lost rename
   only means the object is stored again later.

The name must not appear before step 1. `put`/`put_file` treat an existing
destination as already present, so a renamed-but-not-durable file after a
crash would be trusted later. The barrier-before-rename order keeps the
invariant: an object file only appears under its content-addressed name
once its bytes are durable, and the index never commits a row that
references an object that is not durably installed.

A batch auto-commits when it reaches 1024 objects or 256 MiB staged. That
is safe because each auto-commit uses the same barrier-before-rename
order. Auto-commit can leave durable objects without an index row if the
scan later fails; GC already keeps young unreferenced objects for a grace
period (D6).

**Other platforms.** `PutBatch::put_file` is today's `put_file` (fully
durable immediately) and `commit()` is a no-op. Linux and Windows behavior
is unchanged.

## D24. Local IPC and the host lock

A running host (`relay run`, `relay service`, or the desktop app) exposes a
blocking local-socket RPC so the CLI can inspect and steer it without
opening the engine for write. Extra auth tokens are out of scope.

- **Transport.** The `interprocess` crate's local sockets, sync/blocking, one
  thread per client. Unix: a socket file `<home>/relay.sock`. If that path
  is longer than 100 bytes, the socket is
  `$TMPDIR/relay-<16 hex chars of blake3(canonicalized home)>.sock`. After
  bind, the file mode is set to `0600`. A stale socket file is removed only
  by the process that already holds the host lock. Windows: a namespaced
  named pipe `relay-<same 16 hex>` with the default pipe security (see
  Security).
- **Endpoint naming.** The 16-hex token is the first 8 bytes of
  BLAKE3(canonicalized home path). Clients and the server derive the same
  name from `--home`.
- **Protocol.** Newline-delimited JSON.
  Request `{"id":u64,"method":"...","params":{...}}` →
  `{"id":u64,"result":...}` or
  `{"id":u64,"error":{"code":"...","message":"..."}}`.
  `PROTOCOL_VERSION` is 1. `hello` returns `{protocol, relay_version, host,
  pid, started_at_ms}`; the client errors on a protocol mismatch.
  Other methods: `status` (live state, listen address, connected peers,
  per-mount watch/scan info), `pause` / `resume` (persist the flag and
  return the new state), `rescan {space?, mount?}` (queue a full scan;
  error if paused or nothing matches), `activity {limit?}` (last N of a
  500-item in-memory ring), `subscribe` (the connection becomes a stream
  of activity items until the client disconnects).
- **Host lock.** `run()` takes an exclusive `File::try_lock` on
  `<home>/relay.host.lock` for its whole lifetime, including paused mode.
  A second `run()` fails immediately. If the existing host answers `hello`,
  the error names its kind and pid. The engine's `relay.lock` /
  `relay.run.lock` are separate: they are released in paused mode so
  `relay scan` and other exclusive opens work.
- **Pause.** The flag lives in `local_settings` (`key = paused`,
  `value = 1|0`), written through `Engine::set_paused` (including
  `open_for_config`). A running loop sees the `data_version` bump, exits,
  drops the engine, does not start the network, reports `paused`, and waits
  for IPC resume, a flag clear from another process (polled about once a
  second), or `stop`. Desktop Pause/Resume write the same flag; the old
  Tauri `settings.json` `paused` key is migrated once and then abandoned.
- **Security.** Same OS user as the host, no tokens. Unix: socket `0600`.
  Windows: `interprocess` 2.x has no safe DACL builder and the workspace
  forbids `unsafe`, so the pipe keeps the default DACL: full control for
  the creator, SYSTEM and Administrators, read-only for Everyone. A
  read-only handle cannot send a request and every client gets its own
  pipe instance, so other users cannot issue commands or read responses.
  While no host runs, another local user could create the pipe name first
  and answer the CLI; acceptable for a single-user machine.
- **Known limits.** One host per home. The desktop app hosts IPC itself; it
  does not yet attach as a client of a `relay service` runner. Windows pipe
  security is the default (see Security). Subscribe is
  firehose-from-now; a client that needs history calls `activity` first.

## D25. Device pairing and LAN discovery

Pairing replaces copying device ids by hand. A short code plus the existing
QUIC port is enough on the same LAN; over Tailscale the joiner also types an
address. There is no internet rendezvous.

- **Code.** Format `NN-NNNN-NNNN` (ten decimal digits). The first two digits
  are a public *nameplate* used only to pick the right mDNS instance. The
  remaining eight are the secret. Entropy is 10^8 (about 26.6 bits) on
  the secret, plus the nameplate to avoid colliding sessions. Codes are
  generated with `rand`. Input may include dashes or spaces. A session lasts
  10 minutes, allows exactly one authentication attempt (any failed confirm
  aborts it), and is cancelled on success, explicit cancel, or expiry.
- **Protocol.** Same UDP port as sync, second ALPN `relay-pair/1`. The TLS
  verifier accepts any well-formed Relay device certificate because rustls
  cannot see ALPN at verify time. Trust is enforced immediately after the
  handshake, before any stream is accepted: sync ALPN (`relay/1`) from a
  device not in the trusted set is closed; pairing ALPN with no open session
  is closed. `drive_connection` still refuses untrusted peers. The joiner
  dials with pairing ALPN and a verifier that accepts any Relay device cert,
  and reads the initiator `DeviceId` from that cert.
- **SPAKE2 and transcript.** One bidirectional stream, length-prefixed
  protobuf. Joiner sends nameplate + SPAKE2 message B; initiator replies
  with message A. Both derive K with the `spake2` crate (Ed25519 group,
  identities `relay-pair-initiator` / `relay-pair-joiner`, password = the
  full 10-digit code). Confirmation MACs are
  `blake3::keyed_hash(K, label || transcript)` with labels
  `relay-pair/1 confirm B` and `… confirm A`. The transcript is nameplate ||
  initiator DeviceId || joiner DeviceId (both as observed from the TLS
  certificates) || both SPAKE2 messages. Binding the TLS-observed ids
  defeats a MITM that presents its own certificate. Device info
  `{name, addresses}` is sent only after both MACs verify, on the same TLS
  connection.
- **Addresses.** Each side advertises every non-loopback, non-link-local
  interface IP (IPv4 and global IPv6, including Tailscale `100.64.0.0/10`
  and `fd7a:115c:a1e0::/48`) plus the listen port. The receiver also adds
  the observed remote address and prefers it (that path just worked).
  Entries are deduped and capped at 8. The sync dialer gives each stored
  address a short attempt so a dead LAN IP does not hide a working
  loopback or Tailscale one.
- **mDNS.** While the host runs it advertises `_relay._udp.local.` The
  instance name is derived from the device id. TXT: `id`, `name`, `v=1`,
  and `pair=<nameplate>` only while a pairing session is open. Browse is
  continuous. A joiner without `--addr` dials the resolved instance whose
  `pair` TXT matches its nameplate. For already-trusted peers, resolved LAN
  addresses are kept in memory as dial hints (at most two per peer) and
  tried ahead of the stored list. An announcement is unauthenticated, so a
  hint reaches the stored list only after a dial to it completed the pinned
  TLS handshake: it is then merged in front of stored ones (Tailscale
  entries stay), capped at 8, written through `SyncInput::PeerAddresses` on
  the engine loop, then pushed live with `NetCommand::SetPeers`. Multicast
  failures are logged and never stop the host.
- **Runtime writes.** Pairing and address merges must go through the loop's
  engine (`SyncInput::AddPeer` / `PeerAddresses`) and `SetPeers`. A write
  from another connection bumps SQLite `data_version` and the loop reloads,
  dropping QUIC. `open_for_config` is the wrong path here (D19, D24).
- **IPC / CLI / desktop.** `pair_start {share?}` / `pair_status` /
  `pair_join {code, addr?}` (blocks up to 60s) / `pair_cancel`. On
  `NetEvent::Paired` the initiator's host upserts the peer, shares the
  requested spaces, and updates the trusted set live. The joiner learns
  those spaces through the existing offer flow. `relay pair` and the
  desktop Peers view are the primary path; `relay peer add` remains the
  manual/advanced fallback. Over Tailscale, mDNS does not cross the
  overlay: the joiner passes `--addr` (`my-mac:47321` via MagicDNS or
  `100.x.y.z:47321`).
- **Known limits.** No internet rendezvous, no hole punching, no pairing
  through a third device. Two devices that cannot reach each other's UDP
  port cannot pair.

## D26. Space membership

A space shared with several peers is a membership set. Relaying files through
a middle machine that has the folder attached is ordinary index exchange —
not a separate protocol.

- **Members on the offer.** `SpaceOffer` lists the other peers that space is
  also shared with (name and addresses from the local peers table). Old peers
  ignore the field.
- **Adopt only after join.** Receiving an offer still never joins a space.
  Members are trusted only for a space this device has already joined: add
  them as peers if missing, merge addresses (existing first), and share the
  space. `join_space` adopts the offer's members the same way after creating
  the space. An offer for a space you have not joined does not introduce its
  members.
- **Dismissal.** `relay peer remove` records the device id in
  `dismissed_peers`. Later offers skip that id until `peer add` or a pairing
  upsert clears the dismissal.
- **Live refresh.** Sharing a space (or pairing with shares) re-sends offers
  to every connected member of that space so existing members learn the new
  one without reconnecting.
- **Hub forward.** Applied remote entries take a fresh local sequence and are
  offered to other peers of the same space through normal `changes_since`.
  The middle device must have the mount attached (D16): entries for a mount
  with no local path are not stored and cannot be forwarded. Devices that are
  not directly connected still need a path through a member that has the
  folder; there is no introduction of devices for a space you have not joined.

## D27. Replication policies

A shared space can sync different subtrees to different devices. There is
still one logical file; policies only decide who receives which paths.
Share remains required — a policy never grants sync by itself.

- **Default.** With no effective policies, behavior is unchanged: every entry
  in a shared space syncs to every peer it is shared with.
- **Policy.** Belongs to one space, has a name, one or more selectors, and
  targets. A target is a device id, a peer name (or this device's name)
  resolved to an id, or a device group expanded to device ids at decision
  time. Overlapping policies union their targets.
- **Selector.** A glob matched against `mountName/relativePath`
  (`/`-separated), always case-sensitive so both sides agree. Same glob
  semantics as mount rules: `*` does not cross `/`; `**` matches any number
  of components including zero. Invalid globs fail at create time.
- **`wants(space, device, mount, path)`.** If the space has no effective
  policies: true. Else true iff some policy selector matches and `device` is
  in that policy's expanded targets. Effective policies are local policies
  union policy snapshots received from peers. Local targets expand groups
  live; snapshots already carry concrete device ids.
- **Send / receive.** The sender drops entries the peer does not want
  (`through_sequence` still advances; empty filtered batches are still sent).
  The receiver drops entries this device does not want before object fetch;
  those drops are not `skipped` and do not emit `SyncWarning`. Unknown mounts
  are left to the existing unknown-mount path.
- **Epoch and replay.** Each space has `policy_epoch` (default 0), bumped
  when local policies or referenced group membership change. `SpaceOffer`
  carries the epoch and the sender's local policies with groups expanded.
  On a joined space, if the peer's epoch differs from the stored snapshot
  (including the first non-empty/non-zero store), replace the snapshot, reset
  the send cursor to 0, and request the peer's index from 0. First sight of
  epoch 0 with empty policies is stored without replay. Unjoined spaces store
  no policy snapshot.
- **Removal.** Removing a policy bumps the epoch and does not delete files
  already on disk or send tombstones. Shrinking a target set just omits those
  paths on the next replay.
- **Groups.** Named sets of device ids (peers or this device). Deleting a
  group removes the group link from policies but keeps direct peer targets
  and the policy itself.
- **Out of scope.** No durability classes and no policy editor in the app.

## D28. Sync progress is a snapshot

The activity log records that a transfer started and that it finished. Live
progress is a separate snapshot (`WatchEvent::Transfers`, IPC `status.transfers`),
pushed at most a few times a second and omitted when nothing is in flight.

- **Receive denominator.** The sender stamps `plan_files`, `plan_bytes`, and
  `plan_after` on each `IndexBatch` of a catch-up (optional protobuf fields;
  older peers omit them and the receiver shows a rate without a percent).
  `plan_files` counts every entry. `plan_bytes` sums live file sizes. Objects
  already in the local store count toward bytes immediately. In-flight objects
  advance per chunk, and completion replaces that partial count.
- **Send row.** The sender does not know which objects the peer already has,
  so outbound bytes are a running total and a rate, not a percent. The row
  ends when the peer's ack reaches the sequence the plan was built against.
- **Indexing.** A local scan is its own row (files visited, bytes hashed) and
  is not mixed into the transfer byte total. The scan passes each visit and
  hash to that row; it is not a separate estimate.
- **Pause.** Pausing freezes the snapshot and hides the rate. No ETA.

## D29. Durable mailbox

When two devices never overlap online, a shared directory acts as a
non-materializing mailbox so each can catch up later. No QUIC session is
required for that catch-up. The mailbox stores the same content-addressed
bytes as the local object store plus length-prefixed prost entry logs; it is
not a filesystem and does not reconstruct working trees. Objects are sealed
with the space key (D30).

- **Path.** Local setting `replica_path` (string). Empty or absent means
  peer-only sync (today's behavior). `relay replica set PATH` / `clear` /
  `status` / `gc`. One filesystem backend (`relay-replica`).
- **Push.** After a scan commits or remote entries are applied, the watch
  loop pushes this device's new index entries and any referenced objects the
  mailbox does not already have. Watermark `replica_push.pushed_seq` advances
  only after append and object puts succeed; idempotent append makes a crash
  between append and watermark safe to retry.
- **Pull.** On host start (after the initial scan) and about every 5 seconds,
  pull each shared peer's log after `sync_progress.received_seq`. Missing
  objects are fetched from the mailbox into the local store; a missing object
  holds the cursor (same hole rule as a failed QUIC fetch). Contiguous
  prefixes use the same `apply_remote_batch` path and received watermark as
  QUIC. Policy `wants` still filters; filtered sequences advance the cursor
  without apply and without `SyncWarning`. Unchanged logs are probed cheaply
  (stat + cached signature); files are not rewritten on an empty pull.
- **Ack / GC.** Readers record `put_ack` through the applied sequence.
  Mailbox GC deletes an entry (and its objects) only when every other current
  member of that space has acked past it and the entry is older than `grace`.
  A member with no ack blocks GC. No acks means delete nothing. A device that
  joins after GC needs a peer that is online, or mirror mode. Mirror
  (`--mirror`) keeps the object of the latest live entry per path even after
  ack; older versions follow the mailbox rule. A transient apply does not
  advance the received cursor.

## D30. Encryption at rest

Mailbox object payloads are sealed with a random 256-bit space key
(XChaCha20-Poly1305). The object id stays `BLAKE3(plaintext)`. A device
X25519 box key, signed by the Ed25519 identity, wraps the space key for
each member whose box key is in the mailbox. Entry logs stay metadata.
The local object store and working tree stay plaintext. Legacy plaintext
mailbox objects still open.

- **Recovery.** `relay recovery show` prints a secret that wraps every space
  key this device can open. `relay recovery import` installs those keys on
  a new device. The secret is not recoverable if it was never written down.
- **Revoke.** `relay peer revoke` marks the device revoked, unshares every
  space, and drops it from the dial set. That is a soft revoke: data already
  decrypted on that device remains. `relay space rotate` starts a new
  generation for future objects; older generations still open.
- **Known limits.** Logs are not encrypted. A revoked
  device that already held a generation can still decrypt objects sealed
  with that generation until they age out of the mailbox.

## D31. Automatic text merge

Concurrent file edits that share `parent_object` and are UTF-8 without NUL
bytes are three-way merged. A clean merge becomes one object with the merged
version vector and no conflict copy. The merge is symmetric, so both peers
compute the same bytes. Overlapping edits, binary files, git metadata, and
equal vectors with different content (`Diverged`) keep the D18 conflict-copy
path so both versions remain and `relay conflicts` still resolves them.

## D32. NAT hole punching

When a replica path is set, each device writes its listen candidates
(advertised LAN addresses plus a STUN reflexive address, if STUN answered
on the QUIC socket before Quinn takes it) to `nat/<device>` in the mailbox.
Peers merge those addresses into the dial list. Both sides dialing the
reflexive address is the hole punch. No hosted account and no TURN relay.
Peer-only mode (no mailbox) stays LAN, Tailscale, or a manual address.
Symmetric NAT still fails closed to those reachable addresses. A user-run
UDP relay for that case is D34.

## D33. Android is a foreground Tauri shell

A phone is a normal Relay device. The first mobile build is Android only.
It is the existing Tauri and Vue shell with the engine in-process. The
desktop tray, autostart, single-instance plugin, updater, CLI sidecar, and
close-to-tray behavior are not part of the Android build.

- **Home.** The device database lives in the app data directory. Android does
  not use the desktop `directories` path or a bundled `relay` binary.
- **Sync.** The host runs while the app process is alive. Leaving the app can
  stop sync; a foreground service is later work. Connectivity is unchanged:
  LAN, or an explicit address such as Tailscale; D32 and D34 reach other
  networks.
- **Mounts.** No Android-specific folder picker yet. The supported place for
  phone files is the app sandbox; pointing a mount at an arbitrary path is
  not a supported workflow.
- **Out of scope.** iOS, Play Store signing, background execution, and photo
  library access.

## D34. Generalized networking

This removes the requirement that every pair of devices share a LAN,
Tailscale, or hand-written address. Direct
QUIC stays preferred. The relay is a dumb UDP forwarder so the QUIC handshake
remains end to end between the two devices.

- **Ranking.** Dial order is computed at dial time and does not change the
  stored address list. Best first: loopback, RFC1918, Tailscale, link-local,
  DNS names, then other public addresses (including STUN reflexives). Order
  within a class stays as stored. The relay is not an address in that list.
  It is tried only after every direct address has failed.
- **Relay.** Optional. `relay transport set HOST:PORT` stores `transport_relay`.
  `--serve` also sets `transport_serve`, and the host binds `0.0.0.0:<port>`
  and forwards between two peers that have bound the same session. The session
  id is `blake3` of the two device ids, so both sides derive it. A bind frame
  names both devices and is signed by the sender. The relay accepts it only
  when the session id matches that pair. Data frames are accepted only from a
  bound source address. Quinn's max UDP payload is capped at 1400 bytes so a
  21-byte relay header still fits the receive buffer. Virtual dial addresses
  use `198.18.0.0/15` and never go on the wire. The forwarder keeps at most
  4,096 sessions and drops one that carried nothing for 60 seconds; a device
  re-binds on its next dial. Known follow-up: a bind carries no timestamp or
  nonce, so a captured one replayed from another address moves that slot
  until the device binds again. Fixing it means a signed timestamp in the
  bind, which old devices do not send, so it waits for a protocol bump.
- **Discovery.** When a mailbox is configured, the non-empty local relay
  address is written to `transport/relay`. A device with no local address
  adopts that file. Peer-only mode (no mailbox and no local setting) does not
  learn a relay.
- **Multi-source fetch.** An object id is content-addressed, so any peer that
  shares the space, or the mailbox, can supply the bytes. After a fetch
  failure the engine asks each other connected peer once, then opens the
  mailbox object (sealed or legacy plaintext) into the local store, before
  the existing retry limit gives up.
- **Out of scope.** No public TURN account, no global directory beyond the
  mailbox file, no connection migration after a path is up.

## D35. Selective materialization

A device can take part in a space without storing every file's
bytes. Replication policies (D27) still decide who is offered a path.
Materialization is a local decision about what this device does with a path
it wants. Rules are not synced and do not change SpaceOffer.

- **Default.** With no rules, every wanted file is fully materialized, which
  as before.
- **Rules.** Local to one device and one space. A rule has a name, one or
  more selectors, and a mode: `full`, `metadata`, `demand`, or `exclude`.
  Selectors are the same case-sensitive globs as policies, matched against
  `mountName/relativePath`. Rules run in `position` order. Last match wins.
  `relay materialize add` appends, so a later rule overrides an older one
  (`media/**` metadata, then `media/notes/**` full).
- **Modes.** `full` fetches the object and writes the working tree.
  `metadata` stores the index row and does not fetch or write. `demand`
  stays metadata until `relay fetch`, then keeps updating that path until
  `relay evict`. `exclude` drops the entry on receive: no index row, no
  file, no warning, same as a policy `wants` miss.
- **Scanner.** A path this device chose not to write is not a tombstone when
  it is absent, and a leftover file is not hashed into a new version.
  Metadata and exclude are ignored both ways. An unhydrated demand path is
  not tombstoned for absence; a real file that appears is adopted.
  `relay evict` drops bytes without a tombstone or a version bump, and
  refuses when the working-tree bytes do not match the index.
- **Rule changes.** Adding, changing, or removing a rule does not delete
  files, index rows, or send tombstones. When a file's mode becomes `full`
  and the row is still index-only, the syncer's periodic tick materializes
  it from the local store, a connected peer, or the mailbox.
- **Mailbox.** Push skips an object upload when this device does not have
  the bytes and still appends the index entry. Pull does not stop the log
  for a metadata or unhydrated demand entry whose object is missing. Full
  copies, and demand copies that are already hydrated, still stop on a
  missing object.
- **Out of scope.** No rule editor in the app beyond folder choices (D38).
  OS placeholders came later (D43).

## D36. Configuration changes on the running host

Space, mount, share, materialization, peer, group, and policy edits, and
held-delete decisions, are data: a `relay_core::ConfigChange`. Every caller builds one: the CLI, the desktop
app, and a peer that manages this device (D37, D39).
This replaces the per-operation `SyncInput::AddMount` / `Share` inputs and the
`add_mount` / `share` IPC methods.

- **One implementation.** `Engine::apply_config` maps a change onto engine
  calls. With no host running, callers use it through `open_for_config`.
- **One live path.** With a host running, callers send IPC `config` (params
  are the change). The host passes `SyncInput::Config` to the loop on the
  priority channel, so a running scan yields. The loop applies the change,
  replies, and only then runs follow-ups: mount watchers, offers to members,
  `IndexRequest`s, the trusted set, and dropping send cursors on unshare or
  delete. Nothing reloads, so sessions stay up. Transport, mailbox,
  recovery, and key rotation still reload (D24): they rebuild networking or
  keys, which a reload does honestly.
- **Decided holds resume.** A decision re-runs the batches held for that
  space. Before, the held batch waited for a reconnect.
- **Attach requests the index.** A joined mount with no local path skips
  incoming entries (D16), and attaching resets the receive watermark (D15).
  The live attach and join now ask connected members for the index from that
  watermark. Before this, a live attach waited for the next reconnect.
  The other direction needs the same: a peer's first offer of a space this
  device already shares with it means the peer just joined, so this device
  requests that peer's index then. Without it, edits on the joining device
  reached the sharer only after a reconnect.
- **Join can wait for its offer.** `JoinSpace { wait_ms }` is held on the
  loop and retried after peer frames until the offer arrives or the wait
  ends. A third device setting up a pair sends the join and the share on
  different connections. Direct writes do not wait.
- **Remove mount.** Detach only. The folder and its files stay (DESIGN
  §9.6). The local index rows and history for that mount are dropped, because an
  unattached mount stores no entries (D16): kept rows would turn a later
  attach to another folder into "every file was deleted." The `.relay-mount`
  marker is removed. A marker this device wrote for a mount that is no
  longer attached is adoptable, so a failed marker delete cannot wedge the
  folder.
- **Delete space.** Refused while any mount in it is attached. Shares,
  progress, policies, rules, and mounts go. Stored offers stay so the space
  can be joined again, and key wraps stay so a rejoin can still open
  mailbox objects (D30).
- **Errors keep a code.** `EngineError::code` gives a stable class
  (`not_found`, `already_exists`, `invalid`, `precondition`, `busy`, ...).
  The loop replies with `Rejected { code, message }`, and IPC errors
  carry the same code. The message alone is what people see.
- **One decision type.** `DeleteHoldDecision` lives in `relay-core`; the
  database and engine copies are gone.

## D37. Remote management

A paired device can be allowed to manage another: browse its folders, set up
sync on it (D39), and open its files (D40, D41). D46 limits what the grant
reaches.

- **A separate grant.** `peers.may_manage` on the managed device means
  "this peer may manage me." It is set only on purpose: the pairing option
  (`relay pair --allow-manage`, a checkbox in the app that defaults on) or
  `relay peer allow-manage`. Membership adoption (D26), offers, and
  `peer add` never set it. `deny-manage` and `peer revoke` clear it.
  Pairing again with the option off takes it back. One grant covers
  browsing, reading, and setup, because reading any file is as sensitive
  as configuring.
- **Remote calls, not replicated config.** The manager asks; the managed
  device runs the same code a local request would (D36) and stays the only
  authority over its own config.
- **Wire.** `PROTOCOL_VERSION` stays 1. `Hello.features` (tag 5) carries
  `FEATURE_CONTROL`. `PeerGrants` (`Frame` tag 9) tells a peer whether it may
  manage the sender; it is sent after `Hello` and whenever the trusted set
  changes, only to peers with the bit, because an older peer closes the
  connection on an unknown frame. A call rides its own stream: an
  `ObjectRequest` whose `control` field (tag 2) is set, answered by one
  `ControlResponse`. Calls go only to peers with the bit.
- **Where checks happen.** `PeerConfig.may_manage` reaches the network
  layer through `SetPeers`; a call from a peer without it is refused
  `forbidden` before the daemon sees it. The daemon's `ControlHandler`
  checks the database again. Each peer may run four calls at once; a call
  gets ten seconds on a blocking thread (a macOS privacy prompt can stall
  one) and the caller waits fifteen.
- **Paths are opaque.** Paths travel in the managed device's native form.
  The manager shows them and sends them back, never joins them. The
  managed device requires absolute paths, canonicalizes, and refuses its
  Relay home, which holds the device key.
- **Listings.** Folders first, then by name; pages of 2,000 (at most 5,000)
  with a cursor. Entries mark mount roots, ancestors of mounts, hidden
  files, and cloud placeholders (Windows recall and offline attributes;
  `~/Library/CloudStorage` and `~/Library/Mobile Documents` on macOS).
- **Errors** are `relay_core::remote::RemoteErrorCode`: `forbidden`,
  `denied`, `not_found`, `timeout`, `unsupported`, `invalid`, `busy`,
  `offline`, `failed`. An unknown code from a newer peer reads as `failed`.
- **macOS.** `Info.plist` carries folder usage strings. Settings shows
  whether Relay has Full Disk Access and opens the pane.
- **Known limits.** The target must be online. Listings are not logged to
  activity.

## D38. Files view and folder choices

The desktop Files view puts D35 in the app.

- **Listing** is one folder at a time (`Repo::entries_in`, a ranged query
  plus a depth test), so a large mount never goes to the UI whole. Each row
  carries one state: `local`, `online_only` (demand, not downloaded),
  `metadata_only`, or `pending` (full, not written yet).
- **Folder choices** are materialization rules named `folder-…` with the
  selectors `mount/path/**` and `mount/path` (the folder itself, so an
  excluded folder does not arrive empty). Choosing for a folder deletes the `folder-`
  rules at or inside it, then appends its own, so the newest choice for a
  parent covers everything in it ("apply to enclosed items"). A later choice
  for a subfolder sits after it and wins there. Hand-written rules are
  never touched. The live loop re-checks index-only rows on its next tick
  after any rule change instead of waiting out the hydration interval.
- **Evict** is a loop input next to `Fetch`, so freeing space does not
  reload the host. A folder or `""` evicts every downloaded demand-mode file
  under it and keeps any whose bytes changed on disk.
- **Fetch** over IPC returns when the file is written or has failed, with no
  fixed timeout: the loop always replies, or drops the sender when it
  stops. Errors carry a code (`unavailable` when no source has the bytes).
- **Opening** a file resolves its OS path in the engine
  (`local_file_path`, which refuses symlink escapes) and opens it with the
  system handler.

## D39. Folder pairs set up from either device

One device sets up sync between a folder on itself
or a peer and a folder on another device, with nothing done by hand on the
others.

- **Remote writes are `ConfigChange`s.** `RemoteCall::Apply` carries one,
  applied on the managed device through the same live path its own app uses
  (D36). Only setup changes are allowed remotely
  (`ConfigChange::allowed_remotely`): spaces, mounts, shares, and
  materialization or folder choices. Peers, grants, groups, policies, and
  held-delete decisions are refused `forbidden`, so a compromised manager
  cannot bring in another device or widen its own access.
- **One schema.** On the wire the change and its result travel as their
  JSON form inside the protobuf call, the same schema local IPC uses. A
  change a peer cannot decode answers `invalid`.
- **Peers by id.** The engine resolves a peer from its local name or its
  device id (`Engine::find_peer`); remote steps always send ids, because
  names are local labels.
- **Two more calls.** `Preview` reports whether a folder exists, is empty
  (counting at most 100,000 files or 2 s), overlaps a mount, holds cloud
  placeholders, and is writable (a probe file named like Relay's temp
  files). `CreateDir { parent, name }` lets the managed device join the
  path itself.
- **The manager's host runs the steps** (`folder_pair.rs`). Each step is a
  remote call on the device it touches; this device answers its own steps
  through the same handler peers reach, so the code is one path whether the
  manager is the source, the destination, or a third device. Order: source
  creates the space, attaches its folder, excludes, and shares with the
  destination; the destination creates its folder if asked, joins (waiting
  up to 8 s for the offer, under the 10 s per-call limit), sets excludes and
  online-only, then attaches. Changes are recorded, and a failure undoes
  them in reverse.
- **Plan first.** `folder_pair_preview` changes nothing and returns
  blocking problems (missing folder, overlap with a mount, not writable)
  and warnings (files already at the destination become conflict copies;
  cloud placeholders download). `folder_pair` refuses while problems
  remain. One space per pair, named after the source folder, numbered if
  either device already has that name.
- **Known limits.** The source and destination must already be paired with
  each other. Subfolder choices in the app are one level deep. Both ends
  must be online during setup.

## D40. Opening a file that is not synced here

From Browse or `relay open PEER PATH`, a file on a
managed device opens here as if it had been synced all along.

- **`Locate`** asks the other device for the file's folder, name, size, and
  mount (with the path inside it). Paths stay opaque to the asker; the
  owning device does the arithmetic.
- **Three cases.** In a mount this device has attached: download it. In a
  mount this device does not have: the other device shares that space (a
  remote `Share`), and it is joined here online-only. In no mount: the
  file's own folder is paired here online-only (D39) under
  `~/Relay/<device>/<folder>`, numbered if taken.
- **The file first.** A new mount's full scan commits only when it ends.
  `ScanFirst` is a priority loop input that indexes one path right away and
  pushes it, so the opened file's row arrives before the rest of a large
  folder is hashed. The host waits up to 30 s for the row, then fetches
  (D38) and returns the local path.
- **Left out.** Quick-open pairs exclude `~$*`, `.~lock.*#`, `.DS_Store`,
  and `Thumbs.db` on both sides (`FolderPairParams.exclude_patterns`), so
  editor lock files do not travel.
- **Records** live in `<home>/quick-open.json`, written by the host. They
  are host bookkeeping, and a database write from the host would reload
  the engine. A record whose space no longer exists is ignored.
- **Removing** one stops syncing here and deletes the space here; files
  stay on disk. On the other device it removes the pair if quick-open
  created it, or only stops sharing a space that existed before. If that
  device is offline the local part still happens and a note says so.
- **Overlap.** A folder pair whose source contains a quick-open folder is
  refused with a pointer to "Opened from other devices".
- **Known limits.** The quick-open root is `~/Relay` (the CLI takes
  `--into`; the app has no setting yet). Whole-file transfer, so a large
  file opens only after it fully downloads; the app asks first above 1 GB.

## D41. Read-only copies

A quick look at a file on a managed device that
sets nothing up on either device.

- **Same stream as objects.** `ObjectRequest.read_file { path, max_bytes }`
  (tag 3) is answered like an object: an `ObjectHeader` (now also carrying
  `error` and `modified_ms`), then exactly `size` bytes. It goes only to
  peers with `FEATURE_CONTROL`, so older peers never see it.
- **Same gate as calls.** The network layer admits a copy exactly as it
  admits a call: the manage grant, a handler, and one of the peer's four
  call slots, held while the bytes stream. The daemon's
  `ControlHandler::open_file` checks the grant again, requires an absolute
  path to a file outside the Relay home, and refuses files over 256 MB
  (`READ_COPY_MAX`); bigger files are opened by syncing their folder (D40).
  The owner logs "bob copied <path>" to activity.
- **On the asking device** the copy lands in `<home>/read-only/<n>/<name>`,
  written to a `.relay-partial` file and renamed when the byte count
  matches, then marked read-only. A file that changes size mid-copy is
  refused. A copy that stalls for 30 s fails. The folder is emptied when the
  host starts, clearing read-only flags first so Windows can delete them.
- **One open request.** `OpenRemoteParams.read_only` selects a copy instead
  of a quick-open pair; `OpenedRemote.synced` is `None` for a copy. CLI:
  `relay open PEER PATH --read-only`. The app's Browse view offers
  "Read-only" next to each file and then "Sync this folder to edit".
- **Whole file.** No hashing against an index (there is none); the size
  check and QUIC's integrity are what a quick look gets.

## D42. Sizes on disk and a faster Browse view

The Browse view shows how much space every file and folder takes on the
device that owns it, and moves like a file manager.

- **Space on disk, not length.** `DirEntry.disk_size` (wire tag 10) is a
  file's allocated size: `st_blocks × 512` on Unix; on Windows 0 for cloud
  placeholders, `GetCompressedFileSizeW` for compressed or sparse files, and
  otherwise the length rounded up to a 4 KiB cluster. `size` stays the
  length and still drives the large-file and read-only-copy limits.
- **Folder sizes are polled, never awaited.** `RemoteCall::FolderSizes
  { path }` (call and reply tag 10) returns what is counted so far for each
  folder directly inside `path`; the app asks again every 700 ms until
  `done`. Two background threads on the owning device count, newest request
  first. A walk does not follow symlinks or junctions, stays on one
  filesystem, and counts a hard-linked file once. Finished counts are
  cached for ten minutes (4,096 at most); a stale one is reported, not
  done, while it is redone. A count nobody asked about for 15 s is dropped.
  An older device answers `invalid`, and the app stops asking it.
- **Path bar without parsing.** `DirListing.ancestors` (tag 7) lists the
  folder and those above it, outermost first, so the caller still never
  splits a path (D37). The app shows anything above a root (Home, a drive)
  as that root's name, and a typed path goes to the device as is.
- **History.** Back, forward, and up, with mouse buttons 4 and 5, Alt+←/→
  (⌘[ and ⌘] on macOS), Alt+↑, F5 or Ctrl+R to refresh, Ctrl+L to type a
  path. Folders seen before show at once from a cache and refresh behind
  it; a late answer for a folder already left is dropped.
- **IPC accepts block.** The local IPC server polled `accept` every 50 ms,
  and the app connects once per command, so every command waited up to
  that long. Accept now blocks; a watcher wakes it with a throwaway
  connection once the server stops. A remote listing over IPC went from
  ~50 ms to ~4 ms on one machine.

## D43. Online-only files in Explorer

On Windows, online-only files (D35 `demand`) show in Explorer and every Open
dialog as Cloud Files placeholders, and download when an app opens them.
Plan and later stages: [`proposals/os-integration.md`](proposals/os-integration.md).

- **Which folders.** A local mount whose space has a `demand` rule is a sync
  root (provider `Relay`, account = mount id, population always full,
  hydration full). The engine decides which mounts want one; the daemon
  registers and connects them through `PlaceholderHost`. Off where the
  system has no Cloud Files support, where registering fails (FAT, exFAT,
  ReFS, network shares, a folder inside another provider's root), with
  `RELAY_PLACEHOLDERS=0`, and in the sim lab and tests (`placeholders:
  false`). Those mounts behave as before.
- **What is on disk.** A pass from the sync loop (at start before the first
  scans, after config changes, index batches, fetches, evictions and
  committed scans, at most once a second otherwise) makes the disk match
  the index: a placeholder without data for each online-only file, holding
  `RLY1` + the object id; online-only folders as real folders; downloaded
  plain files converted to placeholders in sync; stale placeholders pointed
  at the current version; leftovers of deleted rows removed.
- **A stat marks a placeholder.** An online-only row with a stat has a
  placeholder Relay put on disk, so the scanner reads its absence as a
  delete (the user deleted or moved it). A row without a stat is index
  only, as before. Dropping a root clears those stats before it removes
  the placeholders without data, so turning the feature off never deletes
  files elsewhere.
- **Never read a placeholder without data.** Reading one asks the daemon for
  the bytes, which may wait on the very loop that is reading. The
  connection refuses this process's own reads (the block-self-implicit-
  hydration flag and a process id check), and every engine path that hashed
  the working tree checks first: the scanner never hashes one, writes
  replace one by stat, and evict only marks the row.
- **Scanning a placeholder without data.** Its bytes cannot change without a
  download, so it is never a local edit. At a path with no live row (and
  not what a deleted row left behind) it is a file the user moved there,
  indexed from its stored object id without reading it. On a downloaded
  row it means "Free up space" was used: the row becomes online-only.
- **Opening a file.** The callback asks the loop for the object
  (`SyncInput::FetchObject`: the same peer and mailbox fetch as `Fetch`,
  without writing the working tree), reporting progress every 10 s so the
  system does not cancel, then streams it from the store in 1 MiB writes
  and sends an ordinary `Fetch`, which finds the bytes in place and marks
  the row downloaded. Explorer's dehydrate, delete, and rename are always
  approved; the daemon then queues the paths for a scan
  (`SyncInput::Touched`). Relay's own evict dehydrates a placeholder instead of
  deleting it.
- **Reparse points.** Only symlinks and junctions (name-surrogate reparse
  points, which std reports as symlinks) are links. Other reparse points,
  including a sync root and its placeholders, are ordinary files and
  folders; before this every reparse point was skipped as a symlink.
- **`cloud-filter` 0.0.6** (MIT) wraps the API, so the workspace stays
  `unsafe_code = "forbid"`. It is pinned: its placeholder-info blob starts
  4 bytes late, which `relay_fs::cloud` reads around, and its callback shims
  abort the process if reporting a failure fails, so callbacks report
  success for everything except a download that could not get its bytes.

## D44. Self-hosted tiers and accounts

Relay is self-hostable end to end. There is no third-party backend: this
replaces the Supabase Postgres and Storage backend the original design
planned for a hosted durable replica. The plan is
[`proposals/home-server.md`](proposals/home-server.md).

- **Three tiers.** (1) Sync between your own devices, with no account and no
  server. (2) An optional home server you run, in the sync chain, holding a
  durable copy and history. (3) Later, the same server binary hosted for
  people without hardware. It is blind by default, so it never holds space
  keys. Billing exists only at tier 3.
- **The server is a peer.** It has its own identity and the same protocol,
  and it is not the authoritative filesystem (DESIGN §5). Its extra duties
  (durability target, history owner, preferred source, rendezvous) are
  decided per stage of the proposal.
- **Accounts are a directory, not a trust root.** Sign-in (OIDC, Google
  first) finds your devices. The directory stores account email, device
  names, public keys, and addresses, never file contents or space keys. We
  run a default instance, and the same component runs inside a home server
  for self-hosters. A newly signed-in device gets space keys only when an
  existing device approves it, or from the recovery secret (D30). Approved
  devices on one account manage each other (D37).
- **Accountless stays.** Code pairing (D25) remains, and the sync engine
  never depends on account authentication.

## D45. Server role

Home server Stage 1 ([`proposals/home-server.md`](proposals/home-server.md)).
A device with the server role keeps a copy of every space its peers share
with it, with no one at its keyboard.

- **Setting.** `relay server enable --data DIR` stores `server_data`, the
  canonical folder, which may not overlap the Relay home. `disable` clears
  it; spaces, mounts, and files stay. `status` lists each kept mount.
- **Plan.** `Engine::server_plan` returns `ConfigChange`s: a `JoinSpace` for
  each stored offer from a peer that may manage this device (D37) and is not
  revoked, and an `AddMount` at
  `DIR/<space>/<mount>` for each mount of a joined or planned space with no
  local folder. It creates those folders. A space or mount name that is not
  one safe folder name (`.`, `..`, a `:`) is skipped, as is an offer whose
  name another space here already has.
- **Live.** The loop runs the plan at start and after each `SpaceOffers`
  frame, through the same `ConfigQueue` and follow-ups as any live change
  (D36), so sessions stay up and the new mount's index is requested at
  once. A change that fails warns once and is not retried until the loop
  restarts. A mount removed by hand on a server is attached again on the
  next offer; turn the role off to manage mounts by hand.
- **Only managers.** Auto-join needs the D37 grant, so it adds nothing a
  manager could not already ask for remotely (D39). Members adopted from an
  offer (D26) never hold the grant, so a device that reached the server only
  through a shared space cannot make it join others. The server does not
  share spaces onward by itself. Mounts start in `store` mode (D47).
- **Packaging.** `packaging/server`: a container image (Debian slim, runs as
  a non-root user, UDP 47321, one volume), a compose file that publishes
  host port 47322, and a systemd unit. Linux first; `relay service` stays
  macOS and Windows.
- **Simulator.** `relay-vopr` topology `Server` and scenario `home_server`:
  three devices that pair only with the server, which joins and attaches
  through `server_plan` and makes no edits of its own.

## D46. What a manage grant reaches

A managing device (D37) browses and sets up sync; it does not get every byte
on the managed device. The accounts design hands the grant to every device
on an account, so the grant has to stop short of credentials.

- **Protected folders.** Credential stores, the operating system's keychains
  and keyrings, browser profiles, and the system's own secret files are never
  shown, described, counted, located, or copied for a peer: `~/.ssh`,
  `~/.gnupg`, `~/.aws`, `~/.azure`, `~/.kube`, `~/.docker`,
  `~/.password-store`, `~/.netrc`, `~/.git-credentials`, `~/.npmrc`,
  `~/.pypirc`, Cargo and Terraform credentials, gcloud and 1Password CLI
  state, GNOME Keyring and KWallet, macOS Keychains and Cookies, Windows
  Credentials, Protect, and Crypto, Chrome, Chromium, Edge, Brave, Vivaldi,
  Firefox, and Thunderbird profiles, `/etc/shadow`, `/etc/ssh`,
  `/etc/ssl/private`, `System32\config`, and `ProgramData\ssh`
  (`relay_daemon::protected`). Only what exists on disk counts, and paths
  are compared canonical, so a symlink into one is still inside it. Listings
  leave them out; naming one answers the new `protected` error code, which
  an older manager reads as `failed`.
- **Remote mounts.** A mount a peer adds (`RemoteCall::Apply` with
  `AddMount`) may be neither inside a protected folder or the Relay home nor
  contain one, so a manager cannot sync a whole home folder, `/`, or a drive
  out of a device. The check runs when the mount is added; a folder pair
  that would is undone and reports why. `Preview` still describes a folder
  that contains protected ones, because the same call previews the parent of
  a folder about to be created.
- **Copies stay in synced folders.** A read-only copy (D41) is answered only
  for a file inside one of the managed device's mounts. Anything else is
  reached by syncing its folder (D39, D40), which shows in that device's
  spaces and activity. The app offers Read-only only inside a synced folder.
- **This device's own steps are not restricted.** The folder-pair steps a
  device runs on itself (`remote::answer` as `Asker::ThisDevice`) see
  everything, as a local request would.
- **Known limits.** The list is fixed; there is no per-device setting. A
  credential in an ordinary folder (a `.env`, a token in Downloads) is not
  protected, and a folder the device's own user already syncs is readable
  whatever it holds.

## D47. Store mode

Home server Stage 2 ([`proposals/home-server.md`](proposals/home-server.md)).
A device can keep every file's bytes without a working-tree copy.

- **Mode.** `store` is a fifth materialization mode (D35). It fetches each
  object into the local store, like `full`, and writes nothing to the
  working tree, like `metadata`. Rows stay `materialized = 0`; the bytes are
  recorded in `objects`, so `verify` checks them and `gc` keeps them while
  the index or history names them. A stored file is served to peers like any
  other object. A file already written here (the path was `full` before)
  is kept current like a demand file that was fetched, so the folder never
  holds stale bytes.
- **Scanner.** As for `metadata` for paths not written here: an absent file
  is not a tombstone and a file that appears in the folder is not hashed
  into a version. A file written here is scanned like a full copy.
- **Fetch.** Index batches and mailbox pulls wait for `store` objects as
  they do for `full`. The hydration tick looks for store rows whose object
  is not in the store only at start, after a rule change or a failed fetch,
  and when a peer connects after a give-up, so a complete store costs no
  per-tick walk. It skips mounts whose rules give every path one other
  mode, so `full` hydration on a server reads no rows either.
- **Conflicts on index-only devices.** A device that does not write a path
  resolves a concurrent edit as a writing device would: a clean text merge
  when it holds the base and both sides (usual in `store`, which keeps
  every version it saw), else the winner plus a conflict-copy row. A copy path already holding other content
  skips the entry, as on a writer. Before this, an index-only device never
  merged, so a writer that merged and a hub that copied reached the same
  vector with different content and never reconciled. Any device missing
  the base still copies where others merge; that limit predates D47.
- **Server default.** `server_plan` sets `store` on a mount's root before it
  attaches it, unless a rule already reaches inside that mount (setting the
  root would replace the owner's folder choices), so the data folder stays
  empty. A later rule (`relay materialize add SPACE NAME
  --mode full --selector 'MOUNT/sub/**'`) or a manager's folder choice in
  the app switches a mount or subfolder to `full` to browse it on the server
  (for example over SMB). Mounts attached before Stage 2 keep `full`.
- **Rule changes.** As in D35, nothing is deleted. `store` to `full`
  writes the files on the next tick from the store. `full` to `store` leaves
  the existing files in place and keeps them current; new files go to the
  store only.
- **Schema.** Migration 14 rebuilds the materialization rule tables to allow
  `store`; rules and selectors are kept.
- **Simulator.** In `home_server`, the server's folder must stay empty after
  quiesce, and its tree is read from its index and store, so a file the
  server indexed without its bytes fails the run.
