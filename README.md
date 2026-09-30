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

**Phase 2: two-device sync.** Relay watches your folders and syncs them with
paired devices over QUIC (TLS 1.3, each device pinned by its public key).
Edits, creates and deletes flow both ways within about a second; concurrent
edits keep both versions; per-file history and restore work on every device.
`relay service install` runs it in the background so you do not have to keep
a terminal open. The [desktop app](apps/relay-desktop/README.md) does the same
from the tray, starts at login and updates itself from GitHub Releases.

Not yet: device discovery (you type the other machine's address) and relaying
through a third device. See [Roadmap](#roadmap).

## Install

### Desktop app (recommended)

Download the latest release from this repository's GitHub Releases page:
`Relay_<version>_universal.dmg` for macOS, `Relay_<version>_x64-setup.exe` for
Windows (per-user install, no admin). The app syncs in the background from
the tray, starts at login, installs updates by itself, and puts the `relay`
command on your PATH.

- macOS: the app is not notarized yet. After dragging it to Applications, run
  `xattr -dr com.apple.quarantine /Applications/Relay.app` once (or right-click
  > Open). Later updates install without this.
- Windows: SmartScreen may warn on first install: More info > Run anyway.

Cutting releases and the one-time signing setup are in
[docs/RELEASING.md](docs/RELEASING.md). Use either the desktop app or
`relay service` on a machine, not both; the app stands aside when the
service is running.

### Dev loop: upgrade both machines with one command

If you develop on a Mac and test on a Windows PC you can reach over SSH, one
command builds this checkout, installs it on both machines, and restarts the
background service.

**One-time setup**

On the Mac: Rust (skip if `cargo` already works) and the Windows cross
compiler.

```bash
./scripts/install.sh --install-rust
brew install mingw-w64
```

On the PC: enable the OpenSSH Server optional feature, then put the Mac's
public key on the PC so `ssh you@pc-host` works without a password.

For a normal Windows user, `ssh-copy-id you@pc-host` is enough. For an
**administrator** account, Windows OpenSSH reads

`C:\ProgramData\ssh\administrators_authorized_keys`

not the user's `authorized_keys`. Append the Mac's `~/.ssh/id_*.pub` line to
that file (as Administrator) and lock the ACL down:

```powershell
icacls C:\ProgramData\ssh\administrators_authorized_keys /inheritance:r /grant "Administrators:F" /grant "SYSTEM:F"
```

Admin SSH sessions are elevated, which `relay service install` needs on
Windows (copy, PATH, scheduled task, firewall rule).

**First run** (from the repo on the Mac):

```bash
./scripts/deploy.sh --pc you@pc-host --pair
```

That remembers `you@pc-host` in `.relay-deploy` (gitignored). `--pair` reads
each device's id, takes the two addresses from the SSH session, and runs
`relay peer add` both ways.

**Everyday**

```bash
git pull && ./scripts/deploy.sh
```

**Check both sides**

```bash
relay --version              # e.g. relay 0.1.0 (abc1234)
relay service status
relay service logs -f        # Ctrl-C only stops the tail, not the service

ssh you@pc-host relay --version
ssh you@pc-host relay service status
ssh you@pc-host relay service logs -f
```

`RELAY_PC=you@pc-host` also selects the PC. `--mac-only` / `--pc-only` deploy
just one side; `--mac-name` / `--pc-name` override the `relay init` names
(default `mac` and `pc`). See `./scripts/deploy.sh --help`.

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

Copy the zip to the PC, extract it, and in an **elevated** PowerShell inside
that folder:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

That initializes the device if needed and runs `relay service install` (copy
to `%LOCALAPPDATA%\Programs\Relay`, user PATH, startup task, firewall rule).
Open a new terminal afterwards so `relay` is on your PATH.

## Sync your Mac and your Windows PC

Relay uses UDP port 47321. Both machines must be able to reach each other:
same LAN, or both on [Tailscale](https://tailscale.com) (then use the
`100.x.y.z` addresses). Find a machine's LAN address with
`ipconfig getifaddr en0` on macOS or `ipconfig` on Windows.

If you used `./scripts/deploy.sh --pc you@pc-host --pair`, both devices
already have an identity, the background service is running, and each side
has the other as a peer. Skip to creating a space.

**1. On the Mac, create a space, add the folder and share it**

```bash
relay space create Mods
relay mount add Mods game ~/Code/game-mods
relay share Mods pc
```

Prefer a folder such as `~/Code` (see [Notes](#notes) if you need Documents,
Desktop or Downloads). A running Relay picks up configuration changes
(`peer`, `share`, `space`, `mount`) within about a second; you never need to
Ctrl-C or restart the service first.

**2. On the PC, join and attach a folder**

Over SSH or locally:

```powershell
relay space offers
relay space join Mods --from mac
relay mount add Mods game "C:\Games\MyGame\Mods"
```

Point the PC's mount at an **empty folder, or one you don't mind merging**.
If it already holds different versions of the same files, both versions are
kept as conflict copies (see below). For a Git repository, starting from an
empty folder on the second machine is simplest.

Edit on the Mac, and the change is on the PC a moment later; it works the
other way too.

**Without SSH** (zip installer, pair by hand)

Build `dist/relay-windows-x86_64.zip` with `./scripts/build-windows.sh`, copy
it to the PC, extract it, and run `install.ps1` from an elevated PowerShell.
Then on each machine, `relay init` if `relay id` fails, and:

```bash
# Mac
relay peer add pc <PC-DEVICE-ID> --addr 192.168.1.20:47321
```

```powershell
# PC
relay peer add mac <MAC-DEVICE-ID> --addr 192.168.1.10:47321
```

`relay id` prints `<64-hex-device-id> <name>`. Addresses are optional on one
side, but giving both lets either machine reconnect first.

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
last, so games, build tools and test runners ignore conflict copies.

`relay conflicts` lists ordinary copies and collapses each Git repository into
one summary. Resolve a file with
`relay conflicts resolve SPACE/MOUNT/COPY --keep current|copy` (`current`
deletes the copy; `copy` replaces the original with the copy's current bytes,
including any merge you edited into it). Prior versions stay in
`relay history`. Those commands edit the files on disk, so they work even
while `relay service` or the desktop app is running.

A file deleted on one machine and edited on the other comes back with the
edit.

### Git

Syncing `.git` needs no special setup. Relay excludes Git's transient lock
files and applies ref updates (`HEAD`, `refs/**`, `packed-refs`, `index`)
after the objects they point to. Concurrent commits on two machines while they
are offline award every mutable file in that `.git` directory to the same
device (the one with the greater device id), so `HEAD`, `index` and refs stay
consistent. The losing ref is kept as
`refs/heads/main.relay-conflict-<device>-<n>`, which Git shows as an ordinary
branch. `relay conflicts resolve-git SPACE/MOUNT/path-to-.git` deletes the
noisy metadata copies; add `--branches` to delete the conflicting refs too.

## Commands

| Command | What it does |
| --- | --- |
| `relay init [--name NAME]` | Create this device's key, id and local database |
| `relay id` | Print this device's id and name |
| `relay run [--listen ADDR] [--verbose] [--log-file PATH]` | Watch mounts and sync with peers in the foreground until Ctrl-C. Default listen `0.0.0.0:47321` |
| `relay service install [--listen ADDR]` | Install or upgrade the background service and start it (macOS LaunchAgent; Windows scheduled task). Requires `relay init` first. Windows must be elevated. |
| `relay service uninstall` / `start` / `stop` / `restart` / `status` | Remove or control the background service |
| `relay service logs [-n N] [-f]` | Show the service log (`<relay home>/logs/relay.log`) |
| `relay status` | Device, mounts, entry counts, peers and sync progress |
| `relay peer add NAME ID [--addr HOST:PORT]...` / `peer list` / `peer remove NAME` | Pair with another device |
| `relay share SPACE PEER` / `relay unshare SPACE PEER` | Allow a peer to sync a space |
| `relay space create NAME` / `space list` | Manage Spaces (logical namespaces) |
| `relay space offers` / `space join NAME --from PEER` | See and accept spaces other devices shared with you |
| `relay mount add SPACE MOUNT PATH [--include P]... [--exclude P]... [--dev-excludes]` | Map a directory into a Space, or attach a joined mount to a local folder |
| `relay mount list [SPACE]` | List mounts and their rules |
| `relay conflicts [--space SPACE]` | List conflict copies (Git repositories are one line each) |
| `relay conflicts resolve SPACE/MOUNT/PATH --keep current\|copy` | Keep the current file or replace it with the conflict copy |
| `relay conflicts resolve-git SPACE/MOUNT/PATH [--branches]` | Delete Git metadata conflict copies; `--branches` also deletes conflicting refs |
| `relay deletes` / `deletes apply SPACE [--mount NAME] [--peer NAME]` / `deletes restore SPACE [--mount] [--peer]` | List held peer mass-deletes, or apply them here / restore the files on the peer |
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

A running Relay (`relay run` or the background service) picks up
`peer add` / `peer remove`, `share` / `unshare`, `space create` / `space join`,
`mount add` and `deletes apply` / `deletes restore` within about a second. For `scan`, `restore` and `gc`, prefer
stopping the service first (`relay service stop`) so a one-shot write does not
interleave with the live loop. Read-only commands (`status`, `ls`, `history`,
`conflicts`, `verify`) work while it runs. `relay run` is still available if
you want a foreground process instead of the service.

## Notes

A macOS background agent cannot read `~/Documents`, `~/Desktop`,
`~/Downloads` or iCloud Drive folders unless the binary has Full Disk Access
(System Settings > Privacy & Security > Full Disk Access; add
`~/.cargo/bin/relay`). Prefer folders such as `~/Code`.

On Windows the task runs as your user without a login session. The Public
network profile blocks it:

```powershell
Set-NetConnectionProfile -NetworkCategory Private
```

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
- **Mass deletes from a peer are held** until you decide. If another device
  tries to delete at least 25 entries and more than half a mount, Relay keeps
  your files and asks (`relay deletes apply` or `relay deletes restore`).
  Nothing is deleted until you choose.
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
`store/objects/` (content-addressed file contents), `store/tmp/` and
`logs/relay.log`.

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
  relay-daemon   reusable sync runner: network + engine loop with live config reload
apps/
  relay-cli      the `relay` binary
scripts/         install.sh, install.ps1, build-windows.sh, deploy.sh
docs/
  DESIGN.md      original specification
  DECISIONS.md   amendments adopted during implementation
```

## Roadmap

The phase plan is in [`docs/DESIGN.md`](docs/DESIGN.md) section 51. Next:

1. **Phase 3**: Git-aware conflict grouping and resolve commands are in;
   receive-side mass-delete guard is next.
2. **Phase 4**: local IPC so the CLI talks to a running daemon (config
   changes are already picked up by a live reload within about a second).
3. **Phase 5+**: LAN discovery and pairing codes, more than two devices,
   relaying, encryption at rest.
