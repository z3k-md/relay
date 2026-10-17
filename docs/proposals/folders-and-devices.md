# Folders and devices

**Status:** Agreed in outline 2026-10-17. Zach chose the one-screen app
(direction B in the [UI proposal](https://claude.ai/artifact/FP4hjaXZBdwmrKE4z4TdE6))
and this model under it. Relay is still in test use, so breaking changes to
config and the CLI are fine; the wire stays additive (D37). Each step lands
with its D-entry.

Relay grew one concept per feature. A user who wants one folder on two
computers meets device ids, peers, spaces, mounts, shares, offers, joins, and
attaching, and the CLI adds policies, groups, five materialization modes,
mount globs, mailboxes, and transport relays. The app is complicated because
it shows all of them.

The proposal: people see **devices** and **folders**, plus a **home server**
when they have one. Everything else is either derived from those or moved
behind "Advanced". The sync core (entries, version vectors, index exchange,
conflicts, the wire protocol, and the DESIGN §9 invariants) does not change.

## The model

```text
Device ── trusted ── Device          (pairing today, account approval later)
Folder
├── Devices it syncs with
├── On each device: Keep · Online only · Not here   (server: Backup)
└── Ignored files (.relayignore)
```

| Today | Proposed | Under the hood |
| --- | --- | --- |
| Peer, device id, address | **Device** | Unchanged. Ids and addresses live under Details. |
| Space + mount | **Folder** | A space with exactly one mount. Keys stay per space (D30), so per folder. |
| Share, offer, join, attach | **Sync with a device** | One step from either side through folder pairs (D39). Offers remain for devices that may not manage this one. |
| Manage grant | **Your devices** | Implied for devices you pair or approve; "Limited" under Details turns it off. |
| Replication policies, device groups | Retired | Covered by "Not here" on a device. |
| Modes `full` / `demand` / `metadata` / `store` / `exclude` | **Keep / Online only / Not here / Backup** | `metadata` folds into `demand`. `store` is set by the server role. |
| Mount include/exclude globs, `.relayignore` | **Ignored files** | The app edits `.relayignore`, which syncs with the folder. Per-device globs stay CLI-only. |
| Mailbox, transport relay | **Home server** | The server becomes the answer for "never online together" and hard networks. Mailbox stays as a CLI option. |
| Held deletes, conflicts, offers | **Needs you** | One list in the app. |

## Changes

### 1. A folder is a space with one mount

Every space the app and folder pairs create already has one mount (D39
names it after the source folder). Multi-mount spaces only buy a shared
member list and one key, and neither is something people ask for.

- New spaces get exactly one mount. `relay folder add PATH [--name N]`
  creates both. `relay space` and `relay mount` stay as hidden aliases for one
  release.
- Entry identity stays `(SpaceId, MountId, path)`, so nothing re-syncs and
  old peers see no difference.
- An existing multi-mount space keeps working. The app lists each mount as
  its own folder and says that its device list is shared with the others.
  Splitting one would change every entry's id, so it is not offered.

### 2. Syncing with a device is one step

"Sync Projects with MacBook" runs the folder-pair steps (D39) when MacBook
lets this device manage it, which is the default for your own devices.

Each device picks its own location, and the locations don't have to match:
`~/Desktop` on ORION can be `D:\Archive\Desktop` on one machine and
`~/Documents/ORION Desktop` on another. Mounts already work this way. The
app suggests the same place under the home folder when it exists there
(`~/Documents` for `C:\Users\zach\Documents`), else `~/Relay/<Folder>`,
and the sheet lets you browse that device to choose any folder (D37) before
it starts. A device's row in the folder sheet shows its path and has
"Move…" to change it later. On Android the default is the app's
synced-folders root.

When the other device does not allow management, it gets an offer, which
shows in its Needs you list as "ORION wants to sync Projects with this
device". Share flags, offers, and joins stay as they are on the wire.

### 3. Trust has two levels

Pairing (and, later, account approval, D44) adds a device that may manage
this one, as the pairing checkbox already defaults. The app stops showing
the grant except as "Limited device" under Details, for a machine you do not
fully control. D46's protected folders still apply to every manager.

### 4. Delete replication policies and groups

Policies (D27) are CLI-only, have no app editor, and duplicate what a
per-device "Not here" folder choice (`exclude`, D35/D38) does now that a
managing device can set it remotely (D39). No device uses them, so they go
outright: the `relay policy` and `relay group` commands, the
`ConfigChange` variants, the policy epoch, snapshots, and replay logic in
`relay-engine/src/policies.rs`, and their tables (migration 0016).

- `SpaceOffer.policy_epoch` and `policies` (tags 5 and 6) are no longer
  sent and are ignored on receive. The tags stay reserved.
- Removing a policy can only widen what a device wants, so nothing is
  deleted (DESIGN §9.6).

### 5. Three choices per folder per device

`Keep` (full), `Online only` (demand), `Not here` (exclude), and `Backup`
(store) on a server. `metadata` becomes `demand`: both keep only the index
row until asked, and nothing in the app sets `metadata`. Migration 0016
rewrites `metadata` rules to `demand`. Files then show as placeholders where
the OS supports them; no bytes are fetched until opened.

### 6. Ignored files belong to the folder

The folder sheet edits the mount-root `.relayignore`, which syncs like any
file, so every device agrees. "Skip build folders" writes the dev excludes
(`--dev-excludes`) into it. Per-device include/exclude globs keep working
(the receiver already drops entries its own rules exclude) and stay a CLI
option.

### 7. The home server absorbs the network extras

The app's answer to "my devices are never on at the same time" and "they
can't reach each other" is the home server (D45): it is always on, keeps
every file, and can carry the rendezvous and UDP forwarding that the
mailbox (D29, D32) and `relay transport` (D34) provide today. Both stay in
the CLI for people without a server.

## What does not change

- Entry ids, version vectors, conflicts, merges, held deletes, history.
- The wire protocol. Every change above is local or rides existing frames.
- The DESIGN §9 invariants. No step deletes user files; a migration only
  rewrites rules.
- The engine stays sans-I/O, and UI reads stay O(folders + devices).

## Order

1. **App only.** The one-screen app, with folder and device vocabulary. "Add
   folder" creates a one-mount space; "Sync with" runs a folder pair. No
   engine change.
2. **CLI and docs.** `relay folder …` commands; USAGE and DESIGN use folder
   and device.
3. **Engine cleanup.** Delete policies and groups; `metadata` to `demand`;
   both in migration 0016.
4. **Ignored files editor** and the home server taking over rendezvous and
   forwarding, with the server stages.

## Open questions

- Should a server show as a device with a Backup badge (proposed), or in its
  own section?
