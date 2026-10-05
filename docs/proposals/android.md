# Android app

**Status:** Stage 1 (CI APK build) shipped. Stack chosen 2026-10-05: keep
the Tauri UI and add a Kotlin service. Stage 2 (service-hosted engine) in
review.

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
WorkManager catch-up job ──► start host, sync to quiescence, stop
DocumentsProvider ──relay-ipc──► list / open / fetch-on-demand
```

- **Engine host.** The Tauri `Runner` no longer needs an `AppHandle` to run;
  the UI attaches to it for events. On Android one runner per process lives
  in a static (`src-tauri/src/mobile.rs`). `RelayNative.start/stop/status`
  (JNI, same `.so`) drive it from the service; the Tauri setup attaches to
  the same runner, so the activity and the service never run two engines.
  The service and UI share a process, so no IPC hop is needed yet.
- **Lifecycle.**
  - The service starts when the app opens and from the catch-up job. Its
    notification shows live status, like the desktop tray, with a "Stop
    sync" action. Android 15 forbids starting a `dataSync` foreground
    service from `BOOT_COMPLETED`, so start-on-boot goes through the
    WorkManager job (stage 3).
  - Android 15 caps `dataSync` foreground services at 6 h per 24 h.
    `onTimeout` stops the host cleanly.
  - A WorkManager periodic job (15 min minimum, optional unmetered and
    charging constraints) runs a bounded catch-up: start the host, sync
    until it reports quiescent or a time budget runs out, then stop. This
    needs a quiescence signal from the host (a new IPC call: no pending
    scan, fetch, or push).
  - A `WifiManager.MulticastLock` is held while the host runs, so mDNS
    discovery works. Doze cuts the network outside maintenance windows, and
    the foreground service is exempt. The app offers the battery
    optimization exemption; it does not require it.
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
2. **Service-hosted engine.** JNI entries, a foreground service with a
   status notification and multicast lock, and the UI attaching to the
   service's runner.
3. **Background catch-up.** WorkManager job (also start-on-boot), the
   quiescence signal, and restart after the 6 h timeout.
4. **DocumentsProvider** over existing mounts, with fetch-on-demand.
5. **Shared storage.** "All files access" mounts, a folder picker, and a
   camera-folder preset.
6. **Mobile UI pass.** Layouts, QR pairing, and the share target.
7. **Signed release APKs** in `release.yml`.

Native Compose over UniFFI was considered and not chosen: the lifecycle and
storage work is the same either way, and keeping Tauri reuses the Vue UI.

## Out of scope

iOS, Play Store listing, and photo-library (MediaStore) access beyond the
camera folder.
