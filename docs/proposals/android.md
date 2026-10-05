# Android app

**Status:** Proposal. Stage 1 (CI APK build) in progress. App stack pending
Zach's call; recommended: keep the Tauri UI, add a Kotlin service.

Turn the D33 foreground shell into a phone that is a full Relay device: it
syncs in the background, backs up the camera folder, and shows its files in
the system Files app and every document picker. This does not amend
[`DESIGN.md`](../DESIGN.md); each stage becomes a decision as it lands.

## Today (D33)

- The Tauri 2 app builds for Android from `apps/relay-desktop/src-tauri/gen/android`.
  Same Vue UI, engine in-process, home in the app data directory.
- Sync runs only while the activity's process is alive.
- Mounts live in the app sandbox. No folder picker, no shared storage.
- No CI build, no signed release, no updater.

## Shape

The hard part is lifecycle and storage, not the UI. Both are Kotlin plus a
thin native entry, whichever UI stack draws the screens.

```
Kotlin RelaySyncService (foreground, dataSync)
  └─ JNI nativeStart(home, opts) ─► thread: relay_daemon::run
                                      ├─ host lock + relay-ipc socket in app data
                                      └─ QUIC on 47321, mDNS
UI (Tauri WebView today) ──relay-ipc──► the same host
WorkManager catch-up job ──► start host, sync to quiescence, stop
DocumentsProvider ──relay-ipc──► list / open / fetch-on-demand
```

- **Engine host.** `relay_daemon::run` already takes a home, options, a stop
  flag, and an event callback, and takes the host lock and IPC socket itself.
  A `#[no_mangle]` JNI entry in the same `.so` starts it on a thread owned by
  the service. The Tauri runner then finds a running host and attaches over
  relay-ipc, the same path desktop uses when `relay service` owns sync.
- **Lifecycle.**
  - The service starts when the app opens, on `BOOT_COMPLETED` if enabled,
    and from the catch-up job. Its notification shows live status, like the
    desktop tray.
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

1. **CI APK build.** A debug `aarch64` APK on every pull request, uploaded
   as a workflow artifact.
2. **Service-hosted engine.** JNI entry, foreground service with status
   notification, start on boot, and the UI attaching over IPC.
3. **Background catch-up.** WorkManager job, the quiescence IPC call, and
   timeout handling.
4. **DocumentsProvider** over existing mounts, with fetch-on-demand.
5. **Shared storage.** "All files access" mounts, a folder picker, and a
   camera-folder preset.
6. **Mobile UI pass.** Layouts, QR pairing, and the share target.
7. **Signed release APKs** in `release.yml`.

If the UI moves to native Compose instead, stages 2–5 are unchanged. Stage 6
becomes a Compose app over UniFFI bindings to `relay_ipc::Client`, and the
Vue UI stays desktop-only.

## Out of scope

iOS, Play Store listing, and photo-library (MediaStore) access beyond the
camera folder.
