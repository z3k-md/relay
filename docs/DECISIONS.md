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
3. Otherwise `choose_winner` picks the version that keeps the path.

The path gets the winner's content with vector `merged(L, R)` and no bump
(bumping would make each device's result different and never converge). The
loser is stored at `conflict_path(path, loser.modified_by,
loser.vector[loser.modified_by])` with the loser's own vector, content,
`modified_by` and `parent_object`. Before overwriting a local loser the local
bytes are already in the object store, so the copy is materialized from the
store. `relay conflicts` lists live conflict copies. Git-aware grouping of
`.git` conflicts is deferred to Phase 3.

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
