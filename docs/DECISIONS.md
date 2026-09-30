# Design decisions and amendments

`DESIGN.md` is the original specification. This file records where the
implementation deliberately departs from or tightens it, and why. When the two
disagree, this file wins.

## D1. `.git` is synchronized like any other directory

The whole working tree, including `.git`, replicates. Relay never runs Git
commands and has no Git-specific logic beyond the following safeguards, which
are generic rules that happen to matter most for Git:

- **Transient files are excluded by default**: `**/.git/**/*.lock`,
  `.git/gc.pid`, `.git/gc.log`, and fsmonitor state. A replicated lock file
  would make Git on the other device refuse to run.
- **Immutable before mutable (Phase 2).** Within a directory containing
  `.git/`, content under `objects/**` is materialized before `HEAD`, `refs/**`,
  `packed-refs` and `index`, so a ref never points at an object that has not
  arrived yet.
- **Repository-level conflict groups (Phase 3).** Concurrent versions of the
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
monotonic `Sequence`. Index exchange (Phase 2) is "changes after N", and
acknowledgements are per-peer watermarks. The `replica_acks` table from the
design is replaced by this.

## D5. Merge bases are recorded from the first release

Each version stores `parent_object`, the content it was derived from, and every
version is appended to `history`. Three-way merge (Phase 11) needs bases that
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
  changed are reported as deselected and never tombstoned (design §48.6).
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

## D11. Device identity before Phase 2

Until peer authentication exists, `DeviceId` is 32 random bytes generated at
`relay init`. Phase 2 derives it from an Ed25519 public key. No state leaves the
machine before then, so nothing shared needs migrating. Key wrapping for Space
keys (Phase 9) will use a separate X25519 key signed by the identity key.

## D12. Phase 0 CLI talks to the engine directly

The `relay` binary embeds `relay-engine` in Phase 0. From Phase 4 it becomes a
thin IPC client of `relayd`, and commands keep their names.

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

## D14. Device identity and transport security (Phase 2)

Supersedes D11. Each device owns an Ed25519 key at `<home>/identity/device.key`
(PKCS#8 PEM, mode 0600 on Unix). `DeviceId` is the raw 32-byte public key.
A self-signed certificate is regenerated from the key at every start.

Peers talk QUIC (quinn) with TLS 1.3 (rustls, `ring` provider) and ALPN
`relay/1`. Both sides present certificates. Each side accepts a certificate
only if its Ed25519 key equals the id of a peer the user added with
`relay peer add`; anything else fails the handshake. There is no CA, no TOFU
and no discovery in Phase 2. A home created before this change has a random
id and no key; it must be re-initialized.

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

## Phase 2 implementation notes

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

Releases are produced by GitHub Actions when a `v*` tag is pushed
(`.github/workflows/release.yml`). `scripts/release.sh` bumps
`[workspace.package] version`, the Tauri and npm `"version"` fields,
commits, tags, and pushes. macOS is a universal (`aarch64` + `x86_64`)
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

  History and objects are kept (no GC yet), so a later change can restore
  those too.

## D24. Local IPC and the host lock

A running host (`relay run`, `relay service`, or the desktop app) exposes a
blocking local-socket RPC so the CLI can inspect and steer it without
opening the engine for write. Pairing, discovery, and extra auth tokens are
out of scope.

- **Transport.** The `interprocess` crate's local sockets, sync/blocking, one
  thread per client. Unix: a socket file `<home>/relay.sock`. If that path
  is longer than 100 bytes, the socket is
  `$TMPDIR/relay-<16 hex chars of blake3(canonicalized home)>.sock`. After
  bind, the file mode is set to `0600`. A stale socket file is removed only
  by the process that already holds the host lock. Windows: a namespaced
  named pipe `relay-<same 16 hex>`. `interprocess` 2.x does not expose a
  safe per-user DACL builder under this workspace's `unsafe_code = forbid`,
  so the pipe inherits the default local-user ACL; that is the documented
  limit, not a second trust domain.
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
- **Security.** Same OS user as the host. Unix socket `0600`; Windows named
  pipe uses the default current-user ACL. No tokens, no cross-user access.
- **Known limits.** One host per home. The desktop app hosts IPC itself; it
  does not yet attach as a client of a `relay service` runner. Windows pipe
  ACLs cannot be tightened further without `unsafe`. Subscribe is
  firehose-from-now; a client that needs history calls `activity` first.
