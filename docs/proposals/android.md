# Android app

**Status:** Stages 1 (CI APK) and 2 (shared in-process runner, #19)
shipped; #30 then dropped the always-on notification, so the service runs
only during a send. Stage 3 (quiet background catch-up) in review. Stack:
Tauri UI plus Kotlin (chosen 2026-10-05).

Turn the D33 foreground shell into a phone that is a full Relay device: it
syncs in the background, backs up the camera folder, and shows its files in
the system Files app and every document picker. This does not amend
[`DESIGN.md`](../DESIGN.md); each stage becomes a decision as it lands.

## Today (D33)

- The Tauri 2 app builds for Android from `apps/relay-desktop/src-tauri/gen/android`.
  Same Vue UI, engine in-process, home in the app data directory.
- Sync runs only while the activity's process is alive.
- Mounts live in the app sandbox. No folder picker, no shared storage.
- No signed release, no updater.

## Shape

The hard part is lifecycle and storage, not the UI. Both are Kotlin plus a
thin native entry, whichever UI stack draws the screens.

```
Kotlin RelaySyncService (foreground, dataSync)
  └─ JNI RelayNative.start(home) ─► Runner thread: relay_daemon::run
                                      ├─ host lock + relay-ipc socket in app data
                                      └─ QUIC on 47321, mDNS
UI (Tauri WebView) ──in-process──► the same runner
SyncCatchUpJob (JobScheduler) ──► start runner, sync until quiet, stop
DocumentsProvider ──relay-ipc──► list / open / fetch-on-demand
```

- **Engine host.** The Tauri `Runner` no longer needs an `AppHandle` to run;
  the UI attaches to it for events. On Android one runner per process lives
  in a static (`src-tauri/src/mobile.rs`). `RelayNative.start/stop/status`
  (JNI, same `.so`) drive it from the service; the Tauri setup attaches to
  the same runner, so the activity and the service never run two engines.
  The service and UI share a process, so no IPC hop is needed yet.
- **Lifecycle.**
  - Sync runs in-process while the app is open. The foreground service
    runs only while a drop is being sent (#30), so nothing sits in the
    notification shade.
  - With the app closed, `SyncCatchUpJob` (a periodic JobScheduler job,
    every 15 min at best, network required, not on low battery, persisted
    across reboots) wakes the process, starts the runner, and stops it once
    `RelayNative.busy()` has been false for 20 s after a 45 s reconnect
    window, or after 8 min. It leaves the engine running if the app is on
    screen, a send is in flight, or the engine was already running. No
    notification, no boot receiver starting a service (Android 15 forbids
    that for `dataSync`).
  - A `WifiManager.MulticastLock` is held while the job or the service
    runs, so mDNS discovery works. Doze defers jobs to maintenance windows.
- **Storage**, in order:
  1. **DocumentsProvider.** Each mount is a root in the system Files app and
     the SAF picker. Reads and writes go through the provider. `demand`
     files (D35) are fetched on `openDocument`. This is the Android
     counterpart of Cloud Files and File Provider (D43) and needs no
     special permission.
  2. **Shared folders** (DCIM/Camera, Documents, Download) through "All
     files access" (`MANAGE_EXTERNAL_STORAGE`). The engine keeps real paths
     under `/storage/emulated/0`, so relay-fs needs no new backend. On
     Android 11+ inotify sees writes made through the FUSE layer, which is
     every app write. The periodic full scan is the backstop. Play
     restricts this permission. Sideloaded, F-Droid, and GitHub-release
     builds are unaffected, and Play stays out of scope (D33).
  3. **SAF tree URIs as a mount backend: not planned.** The engine assumes
     `std::fs` paths, so this would need a VFS seam through relay-fs. Only
     revisit if Play distribution requires it.
- **UI.** Single-column mobile layouts, a QR code for pairing (the existing
  code plus addresses), a "Send to Relay" share target, and approving new
  devices from the phone once accounts (D44) land.
- **Distribution.** A signed release APK on GitHub releases, installable
  directly or through Obtainium or F-Droid. The Tauri updater has no
  Android support. Signing needs a keystore in Actions secrets.

## Stages

1. **CI APK build.** A debug `aarch64` APK on path-filtered pull requests and main pushes, uploaded
   as a workflow artifact.
2. **Shared runner.** JNI entries and one in-process runner shared by the
   UI, the send service and background jobs.
3. **Background catch-up.** Periodic JobScheduler job with no
   notification, `RelayNative.running/busy`, persisted across reboots.
4. **DocumentsProvider** over existing mounts, with fetch-on-demand.
5. **Shared storage.** "All files access" mounts, a folder picker, and a
   camera-folder preset.
6. **Mobile UI pass.** Layouts, QR pairing, and the share target.
7. **Signed release APKs** in `release.yml`.

Native Compose over UniFFI was considered and not chosen: the lifecycle and
storage work is the same either way, and keeping Tauri reuses the Vue UI.

## Out of scope

iOS, Play Store listing, and photo-library (MediaStore) access beyond the
camera folder. [Sending a photo to a computer](photo-drop.md) reads new
camera and screenshot rows only.
