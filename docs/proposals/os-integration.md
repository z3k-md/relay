# Online-only files in the OS file manager

Build plan for showing Relay's online-only files (D35 `demand` mode) inside
Explorer, Finder, and Linux file managers, where they open like any other file
and download on first read. Stage 1 shipped as D43; later stages become
decisions as they land.

## Goal

A folder synced online-only looks complete in the native file manager and in
every app's Open dialog. Each file shows its real name, size, and date, with a
cloud badge. Opening one downloads it from whichever device has it, then
opens it. "Free up space" and "Always keep on this device" work from the
context menu. Nothing else about syncing changes.

On macOS and Linux an online-only file still has an index row and no file on
disk: it can be opened only from Relay's own Files and Browse views.

## Platform APIs

| | Windows | macOS | Linux |
| --- | --- | --- | --- |
| API | Cloud Files (`cfapi`, cldflt.sys), Windows 10 1709+ | File Provider, replicated extension (macOS 11+) | None built in |
| Placeholders | NTFS sparse files with a reparse point, at the real path | Files the system materializes under `~/Library/CloudStorage/Relay-<name>` | FUSE view, or real files only |
| Who calls whom | The filter driver calls the provider process (our daemon) on first read | The system launches our app extension, which must reach the daemon over XPC or IPC | — |
| Folder location | Any NTFS folder we register as a sync root | Fixed by the system, under `~/Library/CloudStorage` | Any |
| Shipping cost | A Rust crate in the daemon. No new process, no signing change | A Swift `.appex` inside the app bundle, an app group, entitlements, Developer ID signing and notarization for the extension | A FUSE dependency, or nothing |
| Used by | OneDrive, Dropbox, Google Drive | iCloud Drive, Dropbox, OneDrive, Google Drive | rclone mount, Nextcloud (partially) |

## Stages

### Stage 1: Windows placeholders (shipped, D43)

Mounts whose space has a `demand` rule become Cloud Files sync roots with full
population: real folders, a placeholder per online-only file, hydrated through
`SyncInput::FetchObject` when an app opens one. The engine never reads a
placeholder that holds no data. Details and limits are in D43.

### Stage 2: Pins, status, and partial reads

- A pin ("Always keep") becomes a `full` folder choice (D38), and an unpin
  becomes `demand`, so the choice syncs with Relay's own UI.
- Sync status in Explorer: uploading and downloading badges through
  `CfReportProviderProgress`, and conflicts as a custom state.
- Ranged hydration (`HydrationType::Progressive`) once objects can be fetched
  by range. Video and large archives would then start without a full
  download.
- Thumbnails for online-only images, served from a small cached preview the
  owning device can generate.

### Stage 3: macOS File Provider

- A replicated File Provider extension (`NSFileProviderReplicatedExtension`)
  in Swift, bundled as `Relay.app/Contents/PlugIns/RelayFileProvider.appex`.
  Its domain is one per online-only mount, shown as `Relay - <space>` under
  Locations.
- The extension is thin. It talks to the daemon over the existing local IPC
  socket placed in the shared app group container, and implements
  `enumerateItems`, `fetchContents`, `createItem`, `modifyItem`, and
  `deleteItem` by calling new IPC methods backed by the same engine calls as
  Windows.
- The system owns where files live (`~/Library/CloudStorage`), so on macOS an
  online-only mount's local path becomes the domain's folder, not a folder
  the user picks. Existing full mounts are unchanged.
- Needs: an Apple Developer team ID, an app group entitlement, signing and
  notarizing the extension, and a CI job on macOS that builds the Swift
  target. Tauri bundles extra `PlugIns` through `bundle.macOS.files`.

### Stage 4: Linux

- No native placeholder API. Two options, in order of value:
  1. A FUSE view (`fuser`) of online-only mounts at the mount path, which
     hydrates on `open`. Works in every file manager and dialog, but adds a
     FUSE dependency and a mount the user can see.
  2. Emblems only: a Nautilus/Dolphin extension that badges files as synced or
     syncing. No placeholders.
- Start with 1 behind a setting once Stages 1 and 3 settle.

### Later: Relay as the file manager

Native placeholders come first because they reach every app's Open dialog,
which a separate file manager never can. A Files-class Relay Explorer, Windows
first, is planned on top of them; its WebView2 performance spike is draft PR
#5. Becoming the default folder handler stays an option on Windows
(`Folder\shell` override) and Linux (`xdg-mime default … inode/directory`);
macOS has no supported way.

## Risks

- **Testing.** Cloud Files needs cldflt.sys. GitHub's Windows runners have it
  on Windows Server 2022, so the integration test registers a temporary sync
  root there; it skips itself where the platform reports no support.
- **Antivirus and indexers** read files. The Windows Search indexer and
  Defender skip recall-on-data-access files by default; other tools may
  hydrate whole folders. Windows lets users block an app from hydrating in
  Settings.
- **A crashed daemon** leaves opens hanging until the driver times out (60 s)
  and then failing. Placeholders stay; nothing is lost.
- **Volume types.** FAT, exFAT, ReFS, and network shares cannot be sync
  roots; those mounts keep today's behavior and log why. CI's Windows temp
  directory is a ReFS dev drive, so the API test uses `%LOCALAPPDATA%`.
- **Pass cost.** Each placeholder pass stats every row of the mount (one
  stat, no handle, for a placeholder already in place). Fine for tens of
  thousands of files; Stage 2 should pass only the paths a batch changed.
- **Moves out of the root.** A placeholder without data moved out of the
  sync root on the same volume is a file nothing can open. Stage 2 should
  hydrate or refuse such moves in the rename callback.
