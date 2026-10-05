# Using Relay

Relay keeps selected folders in sync across your own computers. The desktop app covers everyday use. This page is the command-line reference and the rules the sync follows.

The app and `relay service` share one data directory. Run one of them on a machine, not both. If the service is already running, the app will not start a second sync loop.

Relay listens on UDP port 47321. Machines need a path to each other: the same LAN, or a VPN such as Tailscale.

## Pair

On one machine (a host must be running, or `relay pair` starts one):

```bash
relay pair
```

It prints a code such as `12-3456-7890`. On the other machine, on the same LAN:

```bash
relay pair 12-3456-7890
```

Over Tailscale or another VPN, mDNS does not cross the network, so pass an address (a MagicDNS name or `100.x.y.z`):

```bash
relay pair 12-3456-7890 --addr laptop:47321
```

Pass `--share SPACE` on the machine showing the code if that space already exists and the other device should see it as an offer. The desktop app has the same two actions under Peers: **Pair a device** and **Enter a code**.

`relay peer add NAME ID --addr HOST:PORT` is the manual path when you already have both device ids.

## Manage another device

Add `--allow-manage` on a machine while pairing (the desktop app's pairing dialogs have the same checkbox, on by default) to let the other device browse this one and, in a later release, set up sync on it. The grant is one-way and belongs to the device being managed:

```bash
relay peer allow-manage laptop
relay peer deny-manage laptop
```

From the other device, with both online:

```bash
relay browse desktop
relay browse desktop 'C:\Users\zach'
```

To sync a folder there with one here, all from this device:

```bash
relay pair-folder 'C:\Users\zach\xyz' ~/Code --from desktop --create xyz-foo --exclude cache
```

`--from` and `--to` name the devices (this one when left out), `--create` makes a new folder inside the destination, `--online-only` keeps the destination's files online until opened, and `--check` shows what would happen without changing anything. The app does the same from Browse with "Sync…".

To open one file from there without syncing anything by hand:

```bash
relay open desktop 'C:\Users\zach\Documents\report.docx'
relay opened
relay opened remove Documents
```

`relay open` prints where the file now is. Its folder syncs here online-only under `~/Relay/desktop/` (`--into DIR` to choose), so only the files you open download, and edits sync back. `relay opened` lists those folders; `remove` stops syncing one and undoes the setup on the other device, keeping files here. `relay open --read-only` instead copies just that file (up to 256 MB) into Relay's folder as read-only and sets nothing up; edits to the copy stay here. In the app, click a file in Browse.

The first browse form lists where to start (home, drives, volumes). Paths are in the managed device's own format. Relay's data folder is never listed. On a Mac being managed, grant Relay Full Disk Access (Settings shows the state) so Desktop, Documents, and Downloads do not wait on a prompt nobody is there to answer.

## Share a folder

On the first machine:

```bash
relay space create Projects
relay mount add Projects work ~/Projects
relay share Projects desktop
```

Sharing a space tells the other members about that peer, including addresses. A third computer does not need a hand-copied device id once it has paired with anyone already in the space.

Optional `relay policy` and `relay group` commands limit which subtrees go to which devices. Without them, a shared space syncs in full to every peer it is shared with.

Optional `relay materialize` rules are local to this computer. They decide whether a path is a full copy, an index row with no bytes (`metadata`), fetched only when you ask (`demand`), or ignored (`exclude`). Later rules override earlier ones. `relay fetch SPACE/MOUNT/PATH` writes one demand file; `relay evict` removes that copy without deleting it on other machines. With no rules, every file is a full copy.

The desktop app's Files view does the same per folder: "Always keep on this computer" or "Online only", Download, Open (downloads first if needed), and Free up space. Those choices are `materialize` rules named `folder-…`; a choice for a folder replaces the folder choices inside it, and rules you add by hand are left alone.

On macOS, prefer a folder outside `~/Documents`, `~/Desktop`, `~/Downloads`, and iCloud Drive unless the binary has Full Disk Access. See [Platform notes](#platform-notes).

A running Relay applies `space`, `mount`, `share`, `peer`, `group`, `policy`, `materialize`, and `deletes` changes immediately, without dropping its connections. You do not need to restart it.

To stop syncing a folder on one machine, run `relay mount remove SPACE MOUNT`. The files stay on disk and other machines keep syncing. `relay space delete SPACE` then forgets the space on this machine; you can join it again from a peer's offer.

On the other machine:

```bash
relay space offers
relay space join Projects --from laptop
relay mount add Projects work ~/Projects
```

Point the second mount at an empty folder, or one you are willing to merge. If it already holds different versions of the same files, both versions are kept as conflict copies. For a Git repository, an empty folder on the second machine is the simplest start.

Check progress:

```bash
relay status
relay ls Projects/work
relay conflicts
```

When `received`, `acked`, and `local` agree for a peer, the two machines have exchanged everything.

## Conflicts

If the same file changes on both machines before they sync, neither edit is lost. Both machines keep the same winner at the original path and the other version beside it as `Name.ext.relay-conflict-<device>-<n>`. The extension is not last, so tools that key off the real extension ignore conflict copies.

`relay conflicts` lists ordinary copies and collapses each Git repository into one summary. Resolve a file with:

```bash
relay conflicts resolve SPACE/MOUNT/COPY --keep current|copy
```

`current` deletes the copy. `copy` replaces the original with the copy’s current bytes, including any merge you edited into it. Earlier versions stay in `relay history`. These commands edit files on disk, so they work while the service or the desktop app is running.

A file deleted on one machine and edited on the other comes back with the edit.

## Git

Syncing `.git` needs no extra setup. Relay skips Git’s transient lock files and applies ref updates (`HEAD`, `refs/**`, `packed-refs`, `index`) after the objects they point to. Concurrent commits on two machines while they are offline award every mutable file in that `.git` directory to the same device (the one with the greater device id), so `HEAD`, `index`, and refs stay consistent. The losing ref is kept as `refs/heads/main.relay-conflict-<device>-<n>`, which Git shows as an ordinary branch.

```bash
relay conflicts resolve-git SPACE/MOUNT/path-to-.git
```

That deletes the noisy metadata copies. Add `--branches` to delete the conflicting refs too.

## Background service

The desktop app is the usual way to stay running. The CLI can do the same job:

```bash
relay init --name laptop
relay service install
relay service status
relay service logs -f
```

`relay service install` installs or upgrades a LaunchAgent on macOS, or a scheduled task on Windows, and starts it. Windows must be elevated. `logs -f` tails the log; Ctrl-C stops the tail, not the service.

`uninstall`, `start`, `stop`, and `restart` are the other `relay service` subcommands.

## Commands

| Command | What it does |
| --- | --- |
| `relay init [--name NAME]` | Create this device’s key, id, and local database |
| `relay id` | Print this device’s id and name |
| `relay run [--listen ADDR] [--verbose] [--log-file PATH]` | Watch mounts and sync in the foreground until Ctrl-C. Default listen `0.0.0.0:47321` |
| `relay service install [--listen ADDR]` | Install or upgrade the background service and start it |
| `relay service uninstall` / `start` / `stop` / `restart` / `status` | Remove or control the background service |
| `relay service logs [-n N] [-f]` | Show the service log (`<relay home>/logs/relay.log`) |
| `relay status` | Device, mounts, peers, sync progress, and whether a host is running |
| `relay pair [--share SPACE]... [--allow-manage]` / `relay pair CODE [--addr HOST:PORT] [--allow-manage]` | Pair with another device. `--allow-manage` lets it manage this one |
| `relay peer add NAME ID [--addr HOST:PORT]...` / `peer list` / `peer remove NAME` | Add a peer by device id |
| `relay peer allow-manage NAME` / `peer deny-manage NAME` | Let a peer browse this device and set up sync on it, or stop |
| `relay browse PEER [PATH] [--all]` | List a managed device's roots, or one of its folders |
| `relay open PEER PATH [--into DIR \| --read-only]` | Get a file from a managed device: its folder syncs here online-only and the file downloads, or with `--read-only` a one-off copy. Prints the local path |
| `relay opened` / `opened remove SPACE` | List folders set up by `relay open`, or remove one |
| `relay pair-folder SOURCE DEST [--from DEVICE] [--to DEVICE] [--create NAME] [--exclude SUB]... [--online-only] [--check]` | Sync a folder on one device with a folder on another, set up from here |
| `relay share SPACE PEER` / `relay unshare SPACE PEER` | Allow a peer to sync a space |
| `relay replica set PATH` / `replica clear` / `replica status` | Durable mailbox directory for offline catch-up |
| `relay transport set HOST:PORT [--serve]` / `transport clear` / `transport status` | UDP relay when peers cannot dial each other. `--serve` forwards on this machine |
| `relay replica gc [--mirror] [--grace-secs N]` | Garbage-collect acked mailbox entries and objects |
| `relay group create NAME` / `group add NAME PEER` / `group remove NAME PEER` / `group delete NAME` / `group list` | Device groups for replication policies |
| `relay policy add SPACE NAME --selector GLOB... [--peer NAME]... [--group NAME]...` | Limit which subtrees sync to which devices |
| `relay policy remove SPACE NAME` / `policy list [SPACE]` | Remove a policy or list them |
| `relay materialize add SPACE NAME --mode full\|metadata\|demand\|exclude --selector GLOB...` | Choose how this device stores matching paths. Last rule wins |
| `relay materialize remove SPACE NAME` / `materialize list [SPACE]` | Remove a materialization rule or list them |
| `relay fetch SPACE/MOUNT/PATH` | Write one `demand` file from a peer, the mailbox, or the local store |
| `relay evict SPACE/MOUNT[/PATH]` | Remove a fetched `demand` file here, or every one under a folder or the whole mount. Files edited since are kept. The index rows stay, and other devices are unchanged |
| `relay space create NAME` / `space list` | Manage spaces |
| `relay space delete NAME` | Forget a space on this device. Its mounts must be removed first. Files are not touched |
| `relay space offers` / `space join NAME --from PEER` | See and accept spaces other devices shared with you |
| `relay mount add SPACE MOUNT PATH [--include P]... [--exclude P]... [--dev-excludes]` | Map a directory into a space, or attach a joined mount to a local folder |
| `relay mount remove SPACE MOUNT` | Stop syncing a mount on this device. Files stay on disk; its local index and history are dropped |
| `relay mount list [SPACE]` | List mounts and their rules |
| `relay conflicts [--space SPACE]` | List conflict copies |
| `relay conflicts resolve SPACE/MOUNT/PATH --keep current\|copy` | Keep the current file or replace it with the conflict copy |
| `relay conflicts resolve-git SPACE/MOUNT/PATH [--branches]` | Delete Git metadata conflict copies |
| `relay deletes` / `deletes apply SPACE [--mount NAME] [--peer NAME]` / `deletes restore SPACE [--mount] [--peer]` | List held peer mass-deletes, apply them here, or restore the files on the peer |
| `relay pause` / `relay resume` | Stop watching and networking until resume |
| `relay rescan [SPACE[/MOUNT]] [--no-wait]` | Ask a running host to scan now. Exits 2 when a mass delete was refused, 1 on any other scan failure |
| `relay activity [-n N] [--follow]` | Recent host activity |
| `relay watch` | Keep the local index live without syncing |
| `relay scan [SPACE[/MOUNT]] [--allow-mass-delete] [--dry-run]` | Index changes once |
| `relay ls SPACE/MOUNT [--deleted] [--prefix PATH]` | Show the logical index |
| `relay history SPACE/MOUNT/PATH` | Every recorded version of one entry |
| `relay restore SPACE/MOUNT/PATH --sequence N` | Write an old version back to disk as a new version |
| `relay verify` | Re-hash every live object in the store |
| `relay gc [--grace-secs N]` | Remove objects no longer referenced by the index or history |

Global flags: `--home DIR` for a different data directory, `--json` for machine-readable output. Set `RELAY_LOG=info` (or `debug`) for logs on stderr.

Exit codes: `0` ok, `1` error, `2` mass delete refused, `3` `relay verify` found missing or corrupt objects.

`--dev-excludes` adds `**/node_modules/**`, `**/target/**`, `**/dist/**`, `**/build/**`, `**/.venv/**`, and `**/__pycache__/**`. A `.relayignore` file at the mount root adds more exclude globs, one per line. Rules are per device.

A running Relay applies `space`, `mount`, `share` / `unshare`, `peer add` / `remove` / `revoke`, `group`, `policy`, `materialize`, and `deletes apply` / `restore` on its live loop. `transport`, `replica`, `recovery`, and `space rotate` are picked up within about a second by reloading. Use `relay rescan` to index while a host is running. For `restore` and `gc`, stop the service first (`relay service stop`) so a one-shot write does not interleave with the live loop. Read-only commands (`status`, `ls`, `history`, `conflicts`, `verify`) and `relay pause` / `resume` / `activity` work while it runs.

## Catch-up and hard-to-reach networks

Direct sync is enough when machines can reach each other. Two optional pieces cover the rest:

- **Mailbox.** `relay replica set PATH` points at a durable directory a device can read when the other is offline. It is a folder you control, not a hosted service. Mailbox objects are encrypted with a per-space key.
- **Transport.** `relay transport` is a UDP forwarder you run when two peers cannot dial each other. The QUIC session still ends at the two devices. A mailbox can publish that address. If a mailbox is configured, peers also exchange STUN addresses through it and try to hole-punch.

A missing object is fetched from another connected peer or the mailbox before the transfer is given up. Text files with a shared parent merge automatically when edits do not overlap.

## Safety

Relay keeps data when it is unsure.

- **Only paired devices can connect.** Each device id is its Ed25519 public key. TLS succeeds only with keys established by `relay pair` or `relay peer add`. A space syncs only with peers it is explicitly shared with, in both directions.
- **Nothing is overwritten blind.** Before replacing a local file, Relay checks that it is still what it last indexed. An edit in the meantime becomes a conflict copy.
- **Remote paths cannot escape the folder.** `..`, absolute paths, drive letters, and writes through symlinks are refused.
- **Names a platform cannot hold are skipped, not deleted.** `aux.lua` or `a:b.txt` from a Mac are not written on Windows, and they stay intact on the Mac. The same applies to symlinks on Windows.
- **A scan refuses to run** if the mount root or its `.relay-mount` marker is missing, so an unplugged drive is not synced as “every file deleted.”
- **Mass deletes are refused** (exit code 2) when a scan would delete at least 25 entries and more than half the mount, or everything in it.
- **Mass deletes from a peer are held** until you decide. Relay keeps your files and asks (`relay deletes apply` or `relay deletes restore`).
- **Unreadable, locked, or mid-write files are skipped** and picked up later. They are never recorded as deleted or half-written.
- **Downloads are verified** by BLAKE3 hash and written atomically (temp file, fsync, rename). Failed transfers are retried.
- **History is kept** for every version on every device. `relay restore` brings any of them back.

## Where data lives

| Platform | Default location |
| --- | --- |
| macOS | `~/Library/Application Support/dev.Relay.Relay` |
| Windows | `%APPDATA%\Relay\Relay\data` |
| Linux | `~/.local/share/relay` |

Override with `--home` or `RELAY_HOME`. Inside: `identity/device.key` (keep it private), `relay.db`, `store/objects/`, `store/tmp/`, and `logs/relay.log`.

Each mount root gets a small `.relay-mount` marker file.

## Platform notes

A macOS background agent cannot read `~/Documents`, `~/Desktop`, `~/Downloads`, or iCloud Drive folders unless the binary has Full Disk Access (System Settings, Privacy & Security, Full Disk Access). Prefer folders such as `~/Projects`.

On Windows the task runs as your user without a login session. A Public network profile blocks it:

```powershell
Set-NetConnectionProfile -NetworkCategory Private
```
