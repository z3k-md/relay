# Remote explorer and remote setup

Build plan for browsing another device's folders, setting up sync from
either machine, and opening files that are not on this device yet. This does
not amend [`DESIGN.md`](../DESIGN.md) or [`DECISIONS.md`](../DECISIONS.md).
Stage 0 is recorded as D36, Stage 1 as D38, Stage 2 as D37, Stage 3 as
D39, Stage 4 as D40, and Stage 5 as D41.

The stages are in build order. Each one ships on its own and is useful
without the next. Stages 0 and 1 change no wire format and no trust rules.

## Goal

After installing Relay and pairing once, the user never has to touch the
other machine again:

- Browse the other device's file system from this one.
- Pick a folder there, pick a destination here (or on a third device), and
  sync starts. Uncheck subfolders to leave them out.
- See files in synced folders that are not downloaded yet, and open them on
  demand.
- Open a file on the other device that is in no synced folder. Relay sets up
  sync for its folder in the background, downloads that one file, and opens
  it. Edits sync back like any other synced file.

## What exists and what is missing

| Need | Today | Gap |
| --- | --- | --- |
| Index rows without bytes, fetched on request | D35 `demand` mode. `SyncInput::Fetch` hydrates through the loop. `relay evict` frees the copy | No desktop commands or UI. `evict` and materialize rule edits write through `open_for_config` |
| Config writes while syncing | `AddMount`, `Share`, `Fetch` are loop inputs (`crates/relay-engine/src/sync.rs`, priority-routed in `watch.rs`) | `create_space`, `join_space`, `unshare`, `materialize_add/remove`, and `evict` use `open_for_config`. Any outside write makes the host reload and shut down networking (`RunExit::ExternalChange` in `crates/relay-daemon/src/lib.rs`). A remote request handled that way would close the connection it arrived on |
| Stop syncing a folder | Nothing | No `remove_mount` or `delete_space` in the engine |
| Join a space | `join_space` requires the offer to be stored already (`peers.rs`) | When a third device sets up the pair, the join request and the offer arrive on different connections. It's a race |
| Peer streams | One control stream. Every other bidirectional stream is an object stream (`incoming_objects` → `serve_object` in `crates/relay-net/src/session.rs`) | No way to make a request and get a reply |
| Unknown frames | Prost decodes an unknown `Frame` oneof tag as `body: None`. `read_loop` treats that as malformed and closes the connection | New frame types must only go to peers that advertise them |
| Trust | `peers` has no capability column. D26 adopts space members as peers automatically | No per-peer grant |
| Folder picking | Native dialog on the local machine | No in-app tree, and nothing for a remote machine |
| macOS privacy | `Info.plist` has only the local-network strings | No folder usage strings and no Full Disk Access check |
| Fetch wait | Host `fetch` waits up to 60 s for the reply (`host.rs`) | Too short for a large file over a slow link |
| Mount rules | Include/exclude are set at `add_mount` / attach only | No editing after the fact. Subfolder checkboxes use D35 `exclude` rules instead (below) |

## Decisions to adopt (D37)

### A separate manage grant

Pairing keeps meaning "may sync spaces shared with it." Browsing and remote
setup need a second, explicit grant:

- **Stored by the managed device.** `peers.may_manage` means "this peer may
  manage me." The device being managed is the only one that can set or clear
  it.
- **Set only on purpose:** in the pairing dialog or with `relay peer
  allow-manage NAME`. `adopt_offered_members`, offer-driven upserts, and
  `relay peer add` never set it. `peer remove`, `peer revoke`, and `relay
  peer deny-manage NAME` clear it.
- **One grant, not several.** Reading any file is as sensitive as changing
  config (a manager could mount `~/.ssh`), so browse, read, and setup share
  one bit in v1. D45 later put credential stores out of the grant's reach:
  a manager can neither browse nor mount `~/.ssh` and its kind, and
  read-only copies come only from folders that already sync.
- **Writes are visible.** Every remote write lands in the managed device's
  activity log with the manager's name, and the desktop app shows a tray
  notification ("Mac set up sync for C:\Users\zach\xyz").

### Remote calls, not replicated config

Each device stays the only authority over its own spaces, mounts, and rules
([`DESIGN.md`](../DESIGN.md) §41). A manager does not edit another device's
database. It asks that device's host to make the change, and the host runs
the same code a local request would. There is no config replication and no
config conflict to resolve. The cost is that the target must be online while
you configure it.

### Wire changes

`PROTOCOL_VERSION` stays 1. A version mismatch closes the session, which is
a bigger break than this needs (the transfer-throughput proposal makes the
same choice).

| Change | Compatibility |
| --- | --- |
| `Hello.features` (uint64, tag 5). Bit 0 is `FEATURE_CONTROL` | Missing decodes as 0 |
| `PeerGrants { may_manage_you: bool }` frame (`Frame` body tag 9), sent after `Hello` and whenever the grant changes | Sent only to peers with `FEATURE_CONTROL` |
| `ObjectRequest` gains `control: Option<ControlRequest>` (tag 2) | Wire-identical for old object requests. Control calls go only to peers with the bit |
| `ControlRequest` / `ControlResponse` (one call per stream, length-prefixed like object streams) | Only on control streams |

Control calls use their own streams, not the control stream. A directory
listing can be large, and index sync shouldn't wait behind it.

### Where requests run

- **Net layer.** `PeerConfig` gains `may_manage`, delivered by `SetPeers`.
  Control requests from a peer without the grant get `forbidden` before
  anything else sees them. A separate per-peer semaphore (4 calls) bounds the
  work a manager can cause. `NetCommand::Control { peer, request, reply }` is
  the client side; `NetEvent::ControlRequest { peer, request, reply }` is the
  server side.
- **Daemon.** It checks the grant against the database again. Read calls
  (roots, listing, stat, read) run on a small worker pool with a 10 s timeout,
  not on the engine loop. Write calls become loop inputs (Stage 0) with reply
  channels.
- **Paths are opaque to the manager.** A remote path is a string in the
  managed device's native form. The manager only displays it and sends it
  back. The managed device canonicalizes it (`dunce`) and runs every check
  that `add_mount` already runs.
- **Peers are named by device id on the wire.** Peer names are local labels,
  and the Mac's name for the PC can differ from a laptop's name for it.
- **The Relay home is off limits.** Listing, stat, and read refuse the home
  directory and everything under it (`identity/device.key` lives there).
  `add_mount` already refuses mounts that overlap it.

## Stage 0. Config writes through the loop

**Shipped (D36).** Implemented as one `ConfigChange` type and one
`SyncInput::Config` input rather than one variant per operation. Stage 3's
remote write calls map onto the same type. `Evict` and `ScanPath` move to
the stages that use them (1 and 4).

Prerequisite for everything remote. It also fixes today's behavior: desktop
config edits drop every live session while the host reloads.

Engine:

- `remove_mount(space, mount)`. Detach only. Stop watching, clear the local
  path, remove `.relay-mount`, and keep the files and index rows. The mount
  goes back to the "offered, not attached" state a joined mount starts in.
  Nothing is deleted on disk or tombstoned (§48.6).
- `delete_space(space)`. Allowed only when no mount in the space is attached
  here. Unshares from every peer.
- `join_space` waits for the offer. The loop keeps a pending join and retries
  it when a `SpaceOffers` frame from that peer is applied. It returns
  `UnknownOffer` after `wait_ms`.

Loop inputs (`SyncInput`, priority-routed next to `AddMount` / `Share` /
`Fetch` in `watch.rs`), each with a reply channel: `CreateSpace`,
`JoinSpace { wait_ms }`, `Unshare`, `RemoveMount`, `DeleteSpace`,
`MaterializeAdd`, `MaterializeRemove`, `Evict`, and `ScanPath { space, mount,
path }`. `ScanPath` scans one path ahead of a running full scan; full scans
already yield to priority inputs.

Host and desktop:

- Matching IPC methods in `crates/relay-daemon/src/host.rs`.
- Desktop commands (`create_space`, `join_space`, `unshare`, plus new
  `remove_mount` and `delete_space`) use `host_client` when a host is running,
  as `add_mount` and `share` already do. They fall back to `with_write` only
  when nothing is running.
- A "Stop syncing" action on a mount in `SpacesView.vue`.

Done when: creating, joining, unsharing, or detaching from the app while a
peer is connected does not log "configuration changed; reloading," and the
session stays up. Test in `crates/relay-daemon/tests/daemon.rs`.

## Stage 1. Files view for synced folders

**Shipped (D38).** Fetch keeps answering on completion but without a fixed
timeout, instead of returning on queue. Evict also takes a folder. Folder
choices replace the choices inside them.

The OneDrive-style experience for folders that are already synced. Local
only, no wire change.

- **Index listing.** `relay-db` gets a direct-children query for a mount and
  prefix (the existing prefix query in `repo.rs` returns the whole subtree).
  The webview should never receive a whole large mount at once.
- **IPC.**
  - `list_dir { space, mount, prefix }` returns each child's name, kind, size,
    modified time, and state: `local`, `online_only` (unhydrated `demand`),
    `metadata`, or `conflict_copy`.
  - Add `evict` and materialize rule methods. `fetch` exists.
- **Fetch wait.** `fetch` returns once the fetch is queued. The UI waits on
  the existing `Transfers` events and the entry's `materialized` flag. This
  removes the 60 s ceiling for large files.
- **`FilesView.vue`.** Breadcrumbs, a folder list, and state icons. Actions:
  - **Open:** fetch if needed, then open with `tauri-plugin-opener` (already a
    dependency).
  - **Download** and **Free up space** (`evict`).
  - **Folder-level "Always keep on this device" / "Online only":** writes
    `full` / `demand` materialize rules. This is the GUI materialize editor
    from the roadmap's open list.
- **Unavailable.** When no connected peer or mailbox has the object, show
  "Available when Desktop is online."

Done when: on a mount with a `demand` rule, a file can be found in the Files
view, opened, and evicted without the CLI.

## Stage 2. Manage grant and remote browsing

**Shipped (D37).** Calls and results are one set of `relay_core::remote`
types; `relay-proto::control` maps them to the wire. The network layer owns
the grant check and `PeerGrants`; the daemon's `ControlHandler` does the
filesystem work. Desktop: a Browse view and the pairing checkbox. CLI:
`relay browse`. The per-entry `denied` flag was dropped: probing each child
folder would trigger the very prompts it reports, so a refused listing
returns `denied` instead.

Read-only. No remote writes yet.

- **Migration `0013_peer_manage.sql`:** `ALTER TABLE peers ADD COLUMN
  may_manage INTEGER NOT NULL DEFAULT 0 CHECK (may_manage IN (0, 1))`.
  `PeerInfo` gains the field. `set_peer_manage` is a loop input that is
  followed by `SetPeers`.
- **Pairing.** `pair_start` and `pair_join` take `allow_manage`. The host
  stores it on `NetEvent::Paired`. Nothing about the grant crosses the
  pairing protocol, because each side decides for itself. Desktop: a checkbox
  on both sides of the pairing dialog, "Let Mac browse and set up sync on
  this computer." CLI: `relay pair --allow-manage`, plus `relay peer
  allow-manage` / `deny-manage`.
- **Wire.** `Hello.features`, `PeerGrants`, and the control stream as above.
  - First calls: `Roots`, `ListDir { path, cursor, limit }`, `Stat { path }`,
    and `ListSpaces`.
  - Errors: `forbidden`, `denied` (OS permission), `not_found`, `timeout`,
    `unsupported`, `busy`.
  - The manager keeps the peer's `PeerGrants` in memory and shows "Manage"
    only when it is set.
- **Roots.** The home directory. Windows: each drive letter that exists
  (probe `C:\` through `Z:\`). macOS: `/Volumes/*`.
- **Listing entries:** name, kind, size, modified time, and flags:
  - `mount`: is a mount root, with its space and mount names.
  - `inside_mount` and `contains_mount`.
  - `cloud_only`. On Windows, the `RECALL_ON_DATA_ACCESS`, `RECALL_ON_OPEN`,
    and `OFFLINE` attributes from
    `std::os::windows::fs::MetadataExt::file_attributes` (no `unsafe`). On
    macOS, paths under `~/Library/CloudStorage` and `~/Library/Mobile
    Documents`.
  - `denied` and `hidden`.
  - Large folders page with a cursor (2,000 entries per page).
- **macOS permissions.** The first time the app touches Desktop, Documents,
  or Downloads, macOS shows a consent dialog on the Mac's own screen. The call
  can wait on that dialog or fail with `EPERM`.
  - Map both outcomes to "needs permission on Mac."
  - Add `NSDesktopFolderUsageDescription`,
    `NSDocumentsFolderUsageDescription`,
    `NSDownloadsFolderUsageDescription`,
    `NSRemovableVolumesUsageDescription`, and
    `NSNetworkVolumesUsageDescription` to `Info.plist`.
  - Setup and Settings get an "Allow access to all folders" step. It checks
    whether Full Disk Access is granted and opens that settings pane if not.
    `relay service` (LaunchAgent) only ever sees `EPERM`; it needs Full Disk
    Access.
- **Local IPC.** `remote_roots { peer }`, `remote_list { peer, path, cursor }`,
  `remote_stat`, and `remote_spaces`.
- **Desktop.** `FilesView.vue` gets a device switcher. A remote device shows
  its live listing with the flags above. A peer whose `Hello` has no
  `FEATURE_CONTROL` shows "Update Relay on Desktop to manage it."

Done when: from the Mac, the PC's `C:\Users\zach` can be browsed without
touching the PC. A peer without the grant gets `forbidden`. An old peer keeps
syncing normally.

## Stage 3. Folder pairs set up from either device

**Shipped (D39).** Remote writes are one `Apply` call carrying a
`ConfigChange` (allowlisted) rather than a call per operation, and peers are
addressed by device id. Subfolder choices are one level deep in the app.

- **Write calls** in `ControlRequest`: `Preview { path }`, `CreateDir`,
  `CreateSpace`, `AddMount`, `Share { space, peer_id }`, `JoinSpace {
  space_id, from_peer_id, wait_ms }`, `RemoveMount`, `DeleteSpace`, and
  `MaterializeSet { space, rules }`. Each maps to a Stage 0 loop input.
- **`Preview`** reports whether the folder exists and is a directory, whether
  it's empty, file count and bytes (bounded walk: 100k entries or 2 s, then
  "at least"), any overlapping mount (§33), `cloud_only`, and whether it's
  writable.
- **The orchestrator lives in the manager's host, not the UI**, so closing
  the window doesn't leave a half-built pair. It has two IPC methods.
  `pair_folder_preview` takes source `{ device, path }`, destination `{ device,
  path, create }`, a name, excludes, and the destination mode (`full` or
  `demand`). `pair_folder` takes the same inputs and commits:
  1. Preview both sides. Stop on overlap or a destination that isn't
     writable. Non-empty destinations go back to the UI for confirmation:
     "This folder has 1,204 files. Any that differ become conflict copies."
  2. Source: `CreateSpace`, `AddMount`, `MaterializeSet` (excludes),
     `Share(dest)`.
  3. Destination: `JoinSpace` (waits for the offer), `AddMount` (creating the
     folder if asked), `MaterializeSet` (excludes and mode).
  4. On failure, undo in reverse (`RemoveMount`, `Unshare`, `DeleteSpace`) and
     report what was undone.
- **The manager can be the source, the destination, or a third device.** It
  must hold the grant for every remote device involved. Because `JoinSpace`
  waits for the offer, the third-device case needs no extra ordering.
- **Naming.** One space per pair. The space and mount are named after the
  folder, with a suffix when the name is taken on either device (checked with
  `ListSpaces`). Pairs still appear in the Spaces view. The simple flow just
  never asks for a space name.
- **Subfolder checkboxes** become materialize `exclude` rules on both devices.
  Selectors are `mountName/sub/folder/**`. D35 already guarantees that a rule
  change does not delete files or send tombstones, so unchecking later keeps
  what has already synced. Changing checkboxes later is another
  `MaterializeSet` on each device.
- **UI.**
  - "Sync to…" on a folder in the Files view opens the pair dialog, with the
    destination device defaulting to this one. A local destination uses the
    native folder dialog; a remote one uses the in-app tree.
  - The dialog shows the non-empty warning and a lazily loaded subfolder
    checkbox tree.
  - Synced folders show a badge. The action is disabled inside or above a
    synced folder, with the reason from §33.

Done when: with only install and pairing done, a pair between
`C:\Users\zach\xyz` and `~/xyz-foo` can be created from the PC, from the Mac,
and from a third paired device. A change on either side syncs, and an
injected failure at step 3 leaves nothing behind on the source.

## Stage 4. Open a file that is not synced

**Shipped (D40).** A file inside a mount the other device has but this one
does not joins that space online-only instead of being refused. Records are
a host JSON file rather than a migration. The quick-open root is not yet a
setting in the app.

`open_remote { peer, path }` on the manager's host:

1. If a pair already covers the file, map it to a space, mount, and path,
   then fetch and open.
2. Otherwise, run `pair_folder`:
   - Source: the file's own folder (the deepest one, to limit overlap later).
   - Destination: `<quick-open root>/<device name>/<folder name>` on this
     device.
   - Destination mode: `demand` (`**`), so nothing else downloads.
3. Ask the source to `ScanPath` the file first. Its index row then exists
   before the rest of the folder is hashed.
4. Wait for the row, then fetch and open as in Stage 1.

Details:

- **Quick-open root.** A setting. The default is `~/Relay` on macOS and
  `%USERPROFILE%\Relay` on Windows, outside the macOS-protected folders.
- **Record quick-open pairs** in a small table (`0014_quick_open.sql`:
  `space_id`, `peer`, `created_at_ms`, `last_opened_ms`). The Files view
  lists them as "Opened from other devices," with a Remove action that
  detaches both sides, unshares, and deletes the space.
- **Default excludes for quick-open pairs:** Office lock files (`~$*`),
  `.DS_Store`, `Thumbs.db`. Nothing in the scanner skips these today.
- **Overlap.** A later explicit pair on an ancestor folder overlaps a
  quick-open pair on the source. `pair_folder_preview` lists the quick-open
  pairs inside the new source and offers to remove them first. Their copies
  under the quick-open root stay on disk (§48.6), and the dialog says so.
- **Large files.** Objects are whole files ([`DESIGN.md`](../DESIGN.md)
  §14.1), so a file opens only after it fully downloads. Show the size before
  starting, show progress, and confirm above 1 GB.

Done when: a `.docx` on the PC that is in no pair opens on the Mac from the
Files view. Saving it on the Mac updates the PC's file. A concurrent edit on
the PC produces a conflict copy, not a lost edit.

## Stage 5. Read-only copy (optional)

**Shipped (D41).** The request rides the object stream (`ObjectRequest.
read_file`) rather than a `ControlResponse` header. The copy lives under the
Relay home and is cleared when the host starts, since the host, not the app,
fetches it.

A quick look without creating a pair. Stage 4 covers the main case, so this
can wait.

- `ReadFile { path, max_bytes }`: a `ControlResponse` header (size, modified)
  followed by raw bytes, the same shape as `ObjectHeader`. Default cap
  256 MB. Refuses the Relay home.
- The manager writes the copy to the app's cache directory, marks it
  read-only, and opens it. The cache is cleared on quit.
- The source logs "Mac read C:\...\report.docx" to activity.
- UI: a secondary "Open read-only copy" action. A banner says edits stay on
  this Mac and offers "Sync this folder to edit."

## Later: placeholder files in Finder and File Explorer

This is what makes OneDrive files look local before they are. D35 leaves it
out of scope, and it needs its own decision:

- **Windows Cloud Files API.** Requires FFI. The workspace sets `unsafe_code =
  "forbid"` (root `Cargo.toml`), so this means a separate crate with a narrow
  exception or a maintained safe wrapper.
- **macOS File Provider.** A Swift app extension. The synced folder lives
  under `~/Library/CloudStorage`, not at an arbitrary path.

The Files view covers most of the same use for a fraction of the work.

## Testing

- **`crates/relay-net/tests/net.rs`**
  - A control call round trip.
  - A peer with `features = 0` never receives `PeerGrants` or a control
    stream.
  - A call from a peer without `may_manage` gets `forbidden`.
  - An old `ObjectRequest` still decodes and serves.
- **`crates/relay-engine/tests`**
  - `remove_mount` then re-attach.
  - The `delete_space` guard.
  - `JoinSpace` waits for a late offer and times out without one.
  - `ScanPath` runs ahead of a full scan.
- **`crates/relay-daemon/tests/daemon.rs`:** each Stage 0 loop input changes
  config without a reload or a dropped session.
- **`relay-sim`:** a three-node `sim/scripts/remote-pair.sh`. Device C pairs
  S's folder to D. A write on S lands on D. An injected failure undoes
  cleanly.
- **Manual**
  - On a Mac being managed, the Desktop folder prompt appears on that Mac,
    and the manager shows "needs permission on Mac."
  - A OneDrive folder on Windows shows `cloud_only`.
  - A Word open, edit, and save round trip.

## Settled choices

Agreed when this plan was adopted:

1. **Pairing checkbox.** On by default in the desktop app, because the
   product promise is "pair once, then do everything from either machine."
   Off in the CLI unless `--allow-manage` is passed.
2. **Browse scope.** Anything the OS lets the Relay process read, minus the
   Relay home. Restricting to home plus drives adds little, because a manager
   can mount any folder anyway.
3. **No first-write confirmation.** The grant is explicit, and activity plus
   a notification is enough.
4. **No offline queue in v1.** The target must be online while it is
   configured.
5. **Android as a managed device.** Only the app sandbox is reachable (D33).
   It shows up as a single root. Not in v1.

## Out of scope

- Replicated configuration.
- Remote conflict resolution, delete holds, pause, and policy editing. They
  can use the same channel later.
- Streaming or ranged reads of large files. See
  [`transfer-throughput.md`](transfer-throughput.md).
- OS placeholder files (above).
