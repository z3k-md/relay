# Online-only files in the OS file manager

Build plan for showing Relay's online-only files (D35 `demand` mode) inside
Explorer, Finder, and Linux file managers, where they open like any other file
and download on first read. This does not amend [`DESIGN.md`](../DESIGN.md) or
[`DECISIONS.md`](../DECISIONS.md). Stage 1 is recorded as D43.

## Goal

A folder synced online-only looks complete in the native file manager and in
every app's Open dialog. Each file shows its real name, size, and date, with a
cloud badge. Opening one downloads it from whichever device has it, then
opens it. "Free up space" and "Always keep on this device" work from the
context menu. Nothing else about syncing changes.

Today an online-only file has an index row and no file on disk: it can be
opened only from Relay's own Files and Browse views.

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

### Stage 1: Windows placeholders (this change)

- **Which folders.** On Windows, every attached mount whose space has a
  `demand` rule becomes a Cloud Files sync root, if the volume is NTFS and the
  platform supports it. `RELAY_PLACEHOLDERS=0` turns it off for the host.
  Registration is per user: provider `Relay`, account `<mount id>`, display
  name `Relay · <space>`.
- **Full population.** Directories are real folders and every file exists on
  disk (`PopulationType::AlwaysFull`), so Explorer never asks us to list a
  folder. A reconcile pass makes the disk match the index for the mount:
  - online-only row, nothing on disk: create a placeholder with the row's
    size and modified time. The placeholder's blob is the object id.
  - online-only row, stale placeholder (size changed remotely): replace it.
  - downloaded row, regular file whose stat matches the index: convert it to
    a placeholder marked in sync.
  - deleted row, placeholder on disk with no data: remove it.
  The pass runs when the host starts, after each received batch for the
  space, and after rule changes.
- **Opening a file.** The driver calls `fetch_data` on a callback thread. The
  daemon asks the sync loop to get the object into the local store
  (`SyncInput::FetchObject`: the same peer and mailbox fetch as `Fetch`, but
  without writing the working tree, because the file being opened is the
  destination). It then streams the stored object into the placeholder in
  4 KiB-aligned chunks with progress, and afterwards sends an ordinary
  `Fetch`, which finds the file already matching and marks the row
  downloaded. Open fails with "network unavailable" if no connected device or
  mailbox has the bytes within the fetch timeout.
- **Free up space.** Explorer's dehydrate is approved, and the row goes back to
  online-only through `Evict`. Relay's own evict dehydrates the placeholder
  instead of deleting the file, so the file stays visible.
- **Always keep on this device.** Pinning hydrates through `fetch_data`. Phase
  2 maps a pin to a `full` folder choice so it survives index rewrites.
- **Deletes and renames** in Explorer are approved immediately. The watcher
  (and the daemon, from the callbacks) queues the paths for a scan, and they
  sync like any other change. An online-only row whose placeholder Relay
  put on disk carries that placeholder's stat, so its absence is a delete;
  a placeholder without data at a new path is indexed from the object id
  it holds, without reading it.
- **The engine never reads a placeholder that holds no data.** Reading one
  would call back into the daemon and wait on the loop that is reading, a
  deadlock. The connection refuses this process's own reads, and the
  scanner never hashes such a file: it is never a local edit. Writing over
  one renames a temp file onto it by stat. `ScannedEntry.dehydrated` and
  `relay_fs::cloud::is_dehydrated` carry the check: Windows recall and
  offline attributes, false elsewhere.
- **Stopping.** Removing the mount or its last demand rule unregisters the
  sync root. The rows stop counting a missing placeholder as a delete, then
  placeholders that hold no data are deleted, because nothing can open them
  afterwards; downloaded files stay.
- **Crate.** `cloud-filter` (MIT, a maintained fork of `wincs`) wraps the API.
  It is a Windows-only dependency of `relay-fs`, so the workspace stays
  `unsafe_code = "forbid"`. If it stops being maintained, the module behind
  `relay_fs::cloud` is about 300 lines to replace with direct `windows`
  calls in a crate that allows `unsafe`.

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

Once online-only files are native everywhere, the Browse view can grow into a
cross-device explorer (tabs, search across devices, moves between devices,
versions) without needing to replace Explorer or Finder. Becoming the default
folder handler stays an option for Windows (`Folder\shell` override) and Linux
(`xdg-mime default … inode/directory`); macOS has no supported way.

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
