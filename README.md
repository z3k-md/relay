# Relay

Local-first file replication across your own machines. Every device works on
ordinary local files; Relay keeps selected directory trees in sync between
them, and never needs another device, a network share or a cloud account to be
online for you to keep working.

The motivating workflow: edit a game mod in Cursor on a MacBook,
and have the files already be on the Windows desktop where the game runs, with
no commit/push/pull step in between.

- Full specification: [`docs/DESIGN.md`](docs/DESIGN.md)
- Decisions that amend the spec: [`docs/DECISIONS.md`](docs/DECISIONS.md)

## Status

**Phase 0: single-machine domain model.** Relay can index directories into a
content-addressed object store and a durable SQLite index, detect changes
between scans with version vectors, record deletes as tombstones, keep a
per-file history, and restore old versions. There is no networking, watcher
or background daemon yet; those arrive in Phases 1-4.

## Requirements

- Rust 1.98 (pinned in `rust-toolchain.toml`; `rustup` installs it
  automatically)
- macOS, Windows or Linux. SQLite is bundled; no system libraries needed.

## Build and test

```bash
cargo build --release          # binary at target/release/relay
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

## Quick start

```bash
relay init --name MacBook
relay space create Personal
relay mount add Personal code ~/Code --dev-excludes
relay scan
relay ls Personal/code
```

Edit, add and delete some files, then:

```bash
relay scan                                        # 1 created, 2 modified, 1 deleted, ...
relay history Personal/code/game-mods/Core.lua   # every version seen
relay restore Personal/code/game-mods/Core.lua --sequence 12
relay status
```

### Commands

| Command | What it does |
| --- | --- |
| `relay init [--name NAME]` | Create this device's identity and local database |
| `relay status` | Device, mounts, whether each mount root is present, entry counts |
| `relay space create NAME` / `relay space list` | Manage Spaces (logical namespaces) |
| `relay mount add SPACE MOUNT PATH [--include P]... [--exclude P]... [--dev-excludes]` | Map a directory into a Space |
| `relay mount list [SPACE]` | List mounts and their rules |
| `relay scan [SPACE[/MOUNT]] [--allow-mass-delete]` | Index changes; all mounts if no argument |
| `relay ls SPACE/MOUNT [--deleted] [--prefix PATH]` | Show the logical index |
| `relay history SPACE/MOUNT/PATH` | Every recorded version of one entry |
| `relay restore SPACE/MOUNT/PATH --sequence N` | Write an old version back to disk as a new version |
| `relay verify` | Re-hash every live object in the store |
| `relay gc [--grace-secs N]` | Remove objects no longer referenced by the index or history |

Global flags: `--home DIR` to use a different data directory, `--json` for
machine-readable output. Set `RELAY_LOG=debug` for logs on stderr.

`--dev-excludes` adds `**/node_modules/**`, `**/target/**`, `**/dist/**`,
`**/build/**`, `**/.venv/**` and `**/__pycache__/**`. A `.relayignore` file at
the mount root adds more exclude globs, one per line.

`.git` directories are synced like everything else (see
[D1](docs/DECISIONS.md#d1-git-is-synchronized-like-any-other-directory));
Git's transient lock files are always excluded.

## Where data lives

| Platform | Default location |
| --- | --- |
| macOS | `~/Library/Application Support/dev.Relay.Relay` |
| Windows | `%APPDATA%\Relay\Relay\data` |
| Linux | `~/.local/share/relay` |

Override with `--home` or `RELAY_HOME`. Inside: `relay.db` (SQLite index, safe
to inspect with `sqlite3`), `store/objects/` (content-addressed file
contents) and `store/tmp/`.

Each mount root gets a small `.relay-mount` marker file identifying it.

## Safety behavior

Relay's rule is to preserve data when unsure. In Phase 0 that means:

- **A scan refuses to run** if the mount root or its `.relay-mount` marker is
  missing, so an unplugged drive or moved folder is not recorded as "every file
  deleted".
- **Mass deletes are refused** (exit code 2) when a scan would tombstone at
  least 25 entries and more than half the mount, or everything in it. Re-run
  with `--allow-mass-delete` if it was intentional.
- **Unreadable paths are protected**: files under a directory Relay could not
  read are not marked deleted, and a locked file is skipped with a warning.
- **Rule changes never delete**: entries that stop matching the include and
  exclude rules are reported as deselected, not tombstoned.
- **Files changing mid-read are skipped** and picked up on the next scan, so
  a half-written save is never indexed.
- **Restores are atomic** and abort if the file on disk changed since the last
  scan.
- **Names that cannot exist on Windows** (`aux.txt`, `what?.md`, trailing dots)
  and case-only collisions are reported at scan time.

## Repository layout

```text
crates/
  relay-core     pure domain types: ids, logical paths, version vectors, entries
  relay-policy   include/exclude glob rules and .relayignore parsing
  relay-fs       scanning, mount markers, path mapping, atomic materialization
  relay-store    BLAKE3 content-addressed object store with mark-and-sweep GC
  relay-db       SQLite schema, migrations and the local index repository
  relay-engine   ties the above together: scan, history, restore, verify, gc
apps/
  relay-cli      the `relay` binary
docs/
  DESIGN.md      original specification
  DECISIONS.md   amendments adopted during implementation
```

Crates for networking, the protocol, crypto, reconciliation and merging are
added in the phases that need them.

## Roadmap

The phase plan is in [`docs/DESIGN.md`](docs/DESIGN.md) section 51. Next:

1. **Phase 1**: filesystem watcher with debouncing, periodic rescans, and
   self-write suppression.
2. **Phase 2**: two-node QUIC sync over LAN/Tailscale, with conflict
   preservation required from the start.
3. **Phases 3-4**: deterministic conflict resolution, `relayd` background
   daemon, CLI over local IPC.
