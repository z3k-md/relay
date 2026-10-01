# Relay desktop

The [Relay](../../README.md) app for macOS and Windows. It runs the same sync engine as the `relay` command and uses the same data directory.

Closing the window leaves Relay in the menu bar or system tray. Quit from the tray menu to stop sync.

## Develop

From this directory:

```bash
bun install
bun run dev
```

Quit an installed Relay from the tray first. `dev` rebuilds the `relay` CLI sidecar, then starts the app with hot reload against the same data directory as the installed app. Cargo skips that rebuild when the CLI has not changed.

## Package

```bash
bun run build
```

macOS produces an ad-hoc signed `.app` and `.dmg`. Windows produces a per-user NSIS installer. The app binary is `relay-desktop` so it does not collide with the bundled `relay.exe` sidecar. Cutting a release is [docs/RELEASING.md](../../docs/RELEASING.md).

## Data and settings

| What | Where |
| --- | --- |
| Device database, object store, logs | `RELAY_HOME` if set, otherwise the platform data directory. Same as the CLI. See [Using Relay](../../docs/USAGE.md). |
| App settings (start at login, auto-update, pause) | Tauri store `settings.json` in the app config directory |
| Logs | `<home>/logs`. Settings has **Open logs folder**. |

If `relay service` is already running, the app does not start a second sync loop. `relay service uninstall` switches the machine to in-app sync.

On first launch, Relay enables start-at-login and tries to install the `relay` command (`~/.local/bin/relay` on macOS and Linux; the user PATH on Windows). Autostart launches with `--hidden`, so the window stays in the tray.

## Android

The Android app is the same interface and in-process engine. It does not ship the tray, autostart, updater, or the `relay` command. Sync runs while the app process is alive. Device data is stored in the app data directory. Background execution, a folder picker, iOS, and Play Store signing are later work. See [the roadmap](../../docs/ROADMAP.md).

One-time setup: Android SDK, NDK 29 (`ndk;29.0.13846066`), and JDK 17+. From this directory, with `ANDROID_HOME` and `NDK_HOME` set:

```bash
bun install
bun run tauri android init
bun run tauri android build -- --apk --target aarch64
```

`android init` writes `src-tauri/gen/android`. The debug APK is under `src-tauri/gen/android/app/build/outputs/apk/`.
