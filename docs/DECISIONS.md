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

## D9. Logical path rules

- Paths are Unicode NFC, so macOS (NFD-preserving) and Windows spellings of the
  same name are one identity.
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
