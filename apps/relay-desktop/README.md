# Relay desktop

The Relay desktop app (Tauri 2 + Vue 3). It talks to the same local engine as the `relay` CLI and shares the same home directory, so pairing a Mac with a Windows PC is one device id + address — not two separate databases.

Closing the window hides Relay in the tray. Quit from the tray menu to stop sync.

## Develop

From this directory:

```bash
bun install          # once
bun run dev          # this checkout, hot reload, same home as the installed app
```

Quit the installed Relay from the tray first. `dev` rebuilds the `relay` CLI sidecar, then starts the app. Cargo skips that rebuild when the CLI has not changed.

## Package

```bash
bun run build
```

macOS produces `.app` / `.dmg` (ad-hoc signed). Windows produces a per-user NSIS installer (no admin). The app binary is `relay-desktop` so it does not collide with the bundled `relay.exe` sidecar on Windows.

## Data and settings

| What | Where |
| --- | --- |
| Device database, object store, logs | `RELAY_HOME` if set, otherwise the platform data dir (`~/Library/Application Support/Relay` on macOS, `%APPDATA%\Relay\Relay` on Windows, `~/.local/share/relay` on Linux). Same as the CLI. |
| App settings (start at login, auto-update, pause) | Tauri store `settings.json` in the app config directory |
| Logs | `<home>/logs` — use **Open logs folder** in Settings |

If `relay service` is already running, the app does not start a second sync loop. Uninstall the service (`relay service uninstall`) to switch to in-app sync.

## Android

The Android app is the same Vue UI and in-process engine. It does not ship the tray, autostart, updater, or the `relay` CLI. Sync runs while the app process is alive. Device data is stored in the app data directory. Background execution, a folder picker, iOS, and Play Store signing are later work (decision D33). See [`../../docs/ROADMAP.md`](../../docs/ROADMAP.md).

One-time setup: Android SDK, NDK 29 (`ndk;29.0.13846066`, the version Tauri CLI 2.12 asks for), and JDK 17+. From this directory, with `ANDROID_HOME` and `NDK_HOME` set:

```bash
bun install
bun run tauri android init
bun run tauri android build -- --apk --target aarch64
```

`android init` writes `src-tauri/gen/android`. The debug APK is under `src-tauri/gen/android/app/build/outputs/apk/`. iOS is not set up.

## First-run defaults

On the first launch Relay enables start-at-login (LaunchAgent on macOS, user autostart elsewhere) and tries to install the `relay` CLI (`~/.local/bin/relay` on macOS/Linux; user PATH on Windows). Autostart launches with `--hidden` so the window stays in the tray.
