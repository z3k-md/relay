# Relay desktop

The Relay desktop app (Tauri 2 + Vue 3). It talks to the same local engine as the `relay` CLI and shares the same home directory, so pairing a Mac with a Windows PC is one device id + address — not two separate databases.

Closing the window hides Relay in the tray. Quit from the tray menu to stop sync.

## Develop

From this directory:

```bash
# once per machine / after changing the CLI
bash scripts/prepare-sidecar.sh

npm install
npm run tauri dev
```

`prepare-sidecar.sh` builds `relay-cli` in release mode and copies it to `src-tauri/binaries/relay-<target-triple>` so Tauri can bundle it as `binaries/relay`. Pass a target triple to cross-build; `universal-apple-darwin` builds both Apple architectures and `lipo`s them.

The frontend type-check + Vite build (no Rust) is:

```bash
npm run build
```

## Package

```bash
bash scripts/prepare-sidecar.sh
npm run tauri build
```

macOS produces `.app` / `.dmg` (ad-hoc signed). Windows produces a per-user NSIS installer (no admin). The app binary is `relay-desktop` so it does not collide with the bundled `relay.exe` sidecar on Windows.

## Data and settings

| What | Where |
| --- | --- |
| Device database, object store, logs | `RELAY_HOME` if set, otherwise the platform data dir (`~/Library/Application Support/Relay` on macOS, `%APPDATA%\Relay\Relay` on Windows, `~/.local/share/relay` on Linux). Same as the CLI. |
| App settings (start at login, auto-update, pause) | Tauri store `settings.json` in the app config directory |
| Logs | `<home>/logs` — use **Open logs folder** in Settings |

If `relay service` is already running, the app does not start a second sync loop. Uninstall the service (`relay service uninstall`) to switch to in-app sync.

## First-run defaults

On the first launch Relay enables start-at-login (LaunchAgent on macOS, user autostart elsewhere) and tries to install the `relay` CLI (`~/.local/bin/relay` on macOS/Linux; user PATH on Windows). Autostart launches with `--hidden` so the window stays in the tray.
