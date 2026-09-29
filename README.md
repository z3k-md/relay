# Relay

Local-first file replication across your own machines. Every device works on
ordinary local files; Relay keeps selected directory trees in sync between
them directly over your network, and never needs a cloud account or another
device to be online for you to keep working.

The motivating workflow: edit a game mod in Cursor on a MacBook,
and have the files already be on the Windows desktop where the game runs, with
no commit/push/pull step in between. `.git` is synced along with everything
else, so the repository is the same on both machines.

- Full specification: [`docs/DESIGN.md`](docs/DESIGN.md)
- Decisions that amend the spec: [`docs/DECISIONS.md`](docs/DECISIONS.md)

## Status

**Phase 2: two-device sync.** `relay run` watches your folders and syncs them
with paired devices over QUIC (TLS 1.3, each device pinned by its public key).
Edits, creates and deletes flow both ways within about a second; concurrent
edits keep both versions; per-file history and restore work on every device.

Not yet: a background service (you keep `relay run` open in a terminal),
device discovery (you type the other machine's address), Git-aware conflict
handling, and relaying through a third device. See [Roadmap](#roadmap).

## Install

### macOS (build from source)

You need the Xcode command line tools (`xcode-select --install`; you already
have them if `git` works) and Rust. From a clone of this repository:

```bash
./scripts/install.sh --install-rust   # installs Rust if missing, then relay
relay --version
```

This puts `relay` in `~/.cargo/bin`. The first build takes a few minutes.

### Windows

Use the prebuilt `relay-windows-x86_64.zip` (a single `relay.exe`, no other
files or runtimes needed). To produce it yourself on the Mac:

```bash
brew install mingw-w64
rustup target add x86_64-pc-windows-gnu
./scripts/build-windows.sh            # -> dist/relay-windows-x86_64.zip
```

Copy the zip to the PC, extract it, and in PowerShell inside that folder:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

Open a new terminal afterwards so `relay` is on your PATH. The first time
`relay run` starts, Windows asks whether to allow it through the firewall:
allow **Private networks**.

## Sync your Mac and your Windows PC

Relay uses UDP port 47321. Both machines must be able to reach each other:
same LAN, or both on [Tailscale](https://tailscale.com) (then use the
`100.x.y.z` addresses). Find a machine's LAN address with
`ipconfig getifaddr en0` on macOS or `ipconfig` on Windows.

**1. Create an identity on each machine**

```bash
# Mac
relay init --name mac
```

```powershell
# PC
relay init --name pc
```

Each prints a 64-character device id. `relay id` shows it again.

**2. Pair them.** Each side adds the other. Addresses are optional on one
side, but giving both lets either machine reconnect first.

```bash
# Mac: the PC's id and address
relay peer add pc <PC-DEVICE-ID> --addr 192.168.1.20:47321
```

```powershell
# PC: the Mac's id and address
relay peer add mac <MAC-DEVICE-ID> --addr 192.168.1.10:47321
```

**3. On the Mac, create a space, add the folder and share it**

```bash
relay space create Mods
relay mount add Mods game ~/Code/game-mods
relay share Mods pc
relay run
```

**4. On the PC, join and attach a folder**

Start `relay run` once so the PC receives the Mac's offer, then stop it with
Ctrl-C (the join commands need the database to themselves):

```powershell
relay run          # prints: mac offers: Mods ... then Ctrl-C
relay space join Mods --from mac
relay mount add Mods game "C:\Games\MyGame\Mods"
relay run
```

Point the PC's mount at an **empty folder, or one you don't mind merging**.
If it already holds different versions of the same files, both versions are
kept as conflict copies (see below). For a Git repository, starting from an
empty folder on the second machine is simplest.

Leave `relay run` open on both machines. Edit on the Mac, and the change is on
the PC a moment later; it works the other way too.

**Check on it**

```bash
relay status        # mounts, and per peer: received / acked sequence numbers
relay ls Mods/game
relay conflicts
```

When `received`, `acked` and `local` agree for a peer, the two machines have
exchanged everything.

### Conflicts

If the same file is changed on both machines before they sync (for example
while one was offline), neither edit is lost. Both machines keep the same
winner at the original path and the other version next to it as
`Name.ext.relay-conflict-<device>-<n>`. The extension is deliberately not
last, so games, build tools and test runners ignore conflict copies. Delete the
copy (or merge it by hand) when you're done; that deletion syncs too.

A file deleted on one machine and edited on the other comes back with the
edit.

### Git

Syncing `.git` needs no special setup. Relay excludes Git's transient lock
files and applies ref updates (`HEAD`, `refs/**`, `packed-refs`, `index`)
after the objects they point to. Running Git on one machine at a time works
best; if you commit on both machines while they are offline from each other,
you get conflict copies inside `.git` that you resolve by hand.

## Commands

| Command | What it does |
| --- | --- |
| `relay init [--name NAME]` | Create this device's key, id and local database |
| `relay id` | Print this device's id and name |
| `relay run [--listen ADDR] [--verbose]` | Watch mounts and sync with peers until Ctrl-C. Default listen `0.0.0.0:47321` |
| `relay status` | Device, mounts, entry counts, peers and sync progress |
| `relay peer add NAME ID [--addr HOST:PORT]...` / `peer list` / `peer remove NAME` | Pair with another device |
| `relay share SPACE PEER` / `relay unshare SPACE PEER` | Allow a peer to sync a space |
| `relay space create NAME` / `space list` | Manage Spaces (logical namespaces) |
| `relay space offers` / `space join NAME --from PEER` | See and accept spaces other devices shared with you |
| `relay mount add SPACE MOUNT PATH [--include P]... [--exclude P]... [--dev-excludes]` | Map a directory into a Space, or attach a joined mount to a local folder |
| `relay mount list [SPACE]` | List mounts and their rules |
| `relay conflicts [--space SPACE]` | List conflict copies |
| `relay watch` | Keep the local index live without syncing |
| `relay scan [SPACE[/MOUNT]] [--allow-mass-delete] [--dry-run]` | Index changes once |
| `relay ls SPACE/MOUNT [--deleted] [--prefix PATH]` | Show the logical index |
| `relay history SPACE/MOUNT/PATH` | Every recorded version of one entry |
| `relay restore SPACE/MOUNT/PATH --sequence N` | Write an old version back to disk as a new version (syncs like any edit) |
| `relay verify` | Re-hash every live object in the store |
| `relay gc [--grace-secs N]` | Remove objects no longer referenced by the index or history |

Global flags: `--home DIR` for a different data directory, `--json` for
machine-readable output. Set `RELAY_LOG=info` (or `debug`) for logs on stderr.

Exit codes: `0` ok, `1` error, `2` mass delete refused, `3` `relay verify`
found missing or corrupt objects.

`--dev-excludes` adds `**/node_modules/**`, `**/target/**`, `**/dist/**`,
`**/build/**`, `**/.venv/**` and `**/__pycache__/**`. A `.relayignore` file at
the mount root adds more exclude globs, one per line. Rules are per device.

Only one process can write to a Relay home at a time. Stop `relay run` before
`peer add`, `share`, `space join`, `mount add`, `scan`, `restore` or `gc`.
Read-only commands (`status`, `ls`, `history`, `conflicts`, `verify`) work
while it runs.

## Safety behavior

Relay's rule is to preserve data when unsure.

- **Only paired devices can connect.** Each device's id is its Ed25519 public
  key; TLS handshakes succeed only with keys you added with `relay peer add`.
  A space syncs only with peers it is explicitly shared with, in both
  directions.
- **Nothing is overwritten blind.** Before replacing a local file with a
  remote version, Relay checks that the file is still what it last indexed;
  if you edited it in the meantime, that edit becomes a conflict copy instead
  of being lost.
- **Remote files cannot escape the folder.** Paths from peers are validated;
  `..`, absolute paths, drive letters and writes through symlinks are
  refused.
- **Names a platform cannot hold are skipped, not deleted.** `aux.lua` or
  `a:b.txt` from a Mac are not written on Windows (with a warning) and stay
  intact on the Mac. Same for symlinks on Windows.
- **A scan refuses to run** if the mount root or its `.relay-mount` marker is
  missing, so an unplugged drive or moved folder is not synced as "every file
  deleted".
- **Mass deletes are refused** (exit code 2) when a scan would delete at least
  25 entries and more than half the mount, or everything in it.
- **Unreadable, locked or mid-write files are skipped** and picked up later,
  never recorded as deleted or half-written.
- **Downloads are verified** by BLAKE3 hash and written atomically (temp file,
  fsync, rename). Failed transfers are retried and re-requested; they are
  never silently dropped.
- **History is kept** for every version on every device, and `relay restore`
  brings any of them back.

## Where data lives

| Platform | Default location |
| --- | --- |
| macOS | `~/Library/Application Support/dev.Relay.Relay` |
| Windows | `%APPDATA%\Relay\Relay\data` |
| Linux | `~/.local/share/relay` |

Override with `--home` or `RELAY_HOME`. Inside: `identity/device.key` (this
device's private key; keep it private), `relay.db` (SQLite index),
`store/objects/` (content-addressed file contents) and `store/tmp/`.

Each mount root gets a small `.relay-mount` marker file identifying it.

## Development

Rust 1.98 is pinned in `rust-toolchain.toml` (rustup installs it
automatically). SQLite is bundled; no system libraries needed.

```bash
cargo build --release          # binary at target/release/relay
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

`crates/relay-engine/tests/sync.rs` runs two engines against each other in
process (deterministic, no network); `crates/relay-net/tests/net.rs` tests the
QUIC transport on localhost.

```text
crates/
  relay-core     pure domain types: ids, logical paths, version vectors, entries
  relay-policy   include/exclude glob rules and .relayignore parsing
  relay-fs       scanning, watching, mount markers, safe path mapping, atomic writes
  relay-store    BLAKE3 content-addressed object store with mark-and-sweep GC
  relay-db       SQLite schema, migrations and the local index repository
  relay-crypto   device identity: Ed25519 key, self-signed certificate
  relay-proto    peer wire protocol (protobuf via prost)
  relay-net      QUIC transport: pinned mutual TLS, control and object streams
  relay-engine   scan, watch, sync state machine, remote apply, conflicts, history
apps/
  relay-cli      the `relay` binary
scripts/         install.sh, install.ps1, build-windows.sh
docs/
  DESIGN.md      original specification
  DECISIONS.md   amendments adopted during implementation
```

## Roadmap

The phase plan is in [`docs/DESIGN.md`](docs/DESIGN.md) section 51. Next:

1. **Phase 3**: Git-aware conflict grouping, receive-side mass-delete guard,
   conflict resolution commands.
2. **Phase 4**: `relayd` background service (launchd / Windows service), the
   CLI talking to it over local IPC so commands work while it runs.
3. **Phase 5+**: LAN discovery and pairing codes, more than two devices,
   relaying, encryption at rest.
