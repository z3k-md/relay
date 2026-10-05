# Development

Rust is pinned in `rust-toolchain.toml` (rustup installs it). SQLite is bundled.

```bash
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

The release binary is `target/release/relay`. Plain `cargo build` skips the desktop app. See [the desktop README](../apps/relay-desktop/README.md) for the Tauri dev loop, and [RELEASING.md](RELEASING.md) for signed desktop builds.

`crates/relay-engine/tests/sync.rs` runs two engines against each other in process. `crates/relay-net/tests/net.rs` tests QUIC on localhost.

## Layout

```text
crates/
  relay-core     ids, logical paths, version vectors, entries
  relay-policy   include/exclude globs and .relayignore
  relay-fs       scanning, watching, mount markers, safe paths, atomic writes
  relay-store    BLAKE3 content-addressed object store
  relay-db       SQLite schema, migrations, local index
  relay-replica  durable mailbox (a filesystem directory)
  relay-crypto  Ed25519 device identity and certificate
  relay-proto    peer wire protocol
  relay-net      QUIC transport, pinned mutual TLS, pairing, LAN discovery
  relay-engine   scan, watch, sync, conflicts, history
  relay-daemon   network plus engine loop, with live config reload
  relay-ipc      local-socket RPC between a running host and the CLI
apps/
  relay-cli      the relay binary
  relay-desktop  menu bar / tray app
  relay-sim      local multi-process lab
  relay-vopr     deterministic single-process sync simulator
scripts/        install, cross-build, deploy, release
docs/
  USAGE.md       commands and safety rules
  ROADMAP.md     what is built and what is next
  DESIGN.md      original specification
  DECISIONS.md   amendments adopted during implementation
  RELEASING.md   desktop release and updater signing
  proposals/     notes that are not decisions yet
```

## Lab

`relay-sim` runs real daemon processes against temporary homes. `wait-converged` returns when every mount tree matches, each index matches its own disk, mailbox pushes are caught up, and nothing is being received or scanned. It holds that for a short settle so a sample during the file-watcher debounce cannot pass early.

```bash
cargo run -p relay-sim -- up mac pc --mailbox shared
cargo run -p relay-sim -- write mac mods/hello.txt v1
cargo run -p relay-sim -- wait-converged --timeout 20s
cargo run -p relay-sim -- report
cargo run -p relay-sim -- down
```

`sim/scripts/pair-and-sync.sh` and `sim/scripts/kill-during-mailbox-push.sh` are the same flow as scripts. The lab defaults to `./.relay-sim`, or `RELAY_SIM_LAB` / `--lab`.

## Simulator

`relay-vopr` runs many engines in one process on one thread, on a virtual clock, over a virtual network, in the style of TigerBeetle's VOPR. It is the fast way to test sync scenarios: a seed fixes the workload, the message schedule and every injected fault, and a failing seed replays exactly. See [`apps/relay-vopr/README.md`](../apps/relay-vopr/README.md).

```bash
cargo run -p relay-vopr -- list
cargo run -p relay-vopr -- run --scenario chaos --seed 7 --trace
cargo run -p relay-vopr -- sweep --seeds 100            # every scenario, all cores
RELAY_VOPR_SEEDS=20 cargo nextest run -p relay-vopr      # the CI test, wider
```

CI runs a one-seed smoke on every pull request, the full suite on Linux when
the engine changes (or on the `ci:full-sim` label) and on pushes to `main`,
and every OS plus a wide seed sweep nightly.

## Install from source

### macOS

You need the Xcode command line tools (`xcode-select --install`) and Rust.

```bash
./scripts/install.sh --install-rust
relay --version
```

This puts `relay` in `~/.cargo/bin`. The first build takes a few minutes.

### Windows

Use a prebuilt `relay-windows-x86_64.zip` (a single `relay.exe`). To produce it on a Mac:

```bash
brew install mingw-w64
rustup target add x86_64-pc-windows-gnu
./scripts/build-windows.sh
```

Copy `dist/relay-windows-x86_64.zip` to the PC, extract it, and in an elevated PowerShell inside that folder:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

That initializes the device if needed and runs `relay service install` (copy to `%LOCALAPPDATA%\Programs\Relay`, user PATH, startup task, firewall rule). Open a new terminal afterwards so `relay` is on your PATH.

## Two-machine dev loop

If you develop on a Mac and test on a Windows PC you can reach over SSH, one command builds this checkout, installs it on both machines, and restarts the background service. This is for working on Relay, not for people installing the app.

**One-time setup.** On the Mac: Rust and the Windows cross compiler.

```bash
./scripts/install.sh --install-rust
brew install mingw-w64
```

On the PC: enable the OpenSSH Server optional feature, then put the Mac’s public key on the PC so `ssh you@pc-host` works without a password.

For a normal Windows user, `ssh-copy-id you@pc-host` is enough. For an administrator account, Windows OpenSSH reads `C:\ProgramData\ssh\administrators_authorized_keys`, not the user’s `authorized_keys`. Append the Mac’s `~/.ssh/id_*.pub` line to that file (as Administrator) and lock the ACL down:

```powershell
icacls C:\ProgramData\ssh\administrators_authorized_keys /inheritance:r /grant "Administrators:F" /grant "SYSTEM:F"
```

Admin SSH sessions are elevated, which `relay service install` needs on Windows.

**First run**, from the repo on the Mac:

```bash
./scripts/deploy.sh --pc you@pc-host --pair
```

That remembers `you@pc-host` in `.relay-deploy` (gitignored). `--pair` reads each device’s id, takes the two addresses from the SSH session, and runs `relay peer add` both ways.

**Everyday:**

```bash
git pull && ./scripts/deploy.sh
```

**Check both sides:**

```bash
relay --version
relay service status
relay service logs -f

ssh you@pc-host relay --version
ssh you@pc-host relay service status
ssh you@pc-host relay service logs -f
```

`RELAY_PC=you@pc-host` also selects the PC. `--mac-only` / `--pc-only` deploy one side. `--mac-name` / `--pc-name` override the `relay init` names (default `mac` and `pc`). See `./scripts/deploy.sh --help`.
