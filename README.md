# Relay

**Realtime file sync for all your machines.**

Maintain copies of arbitrary folders and file trees on multiple machines in realtime. Clients available for Windows, Linux, MacOS, and Android. Securely connect machines to your mesh network for seemly end-to-end encrypted file transfer and realtime sync.

Install Relay, pair your computers, and choose the folders that should stay the same. Each machine keeps ordinary local files. Changes show up on the others within about a second. There is no cloud account, and you can keep working when another computer is off.

## Install

Download the latest release from [GitHub Releases](https://github.com/z3k-md/relay/releases).

- **macOS:** `Relay_<version>_universal.dmg`
- **Windows:** `Relay_<version>_x64-setup.exe` (per-user, no admin)

The app syncs from the menu bar or system tray, starts at login, updates itself, and installs the `relay` command.

macOS builds are not notarized yet. After moving Relay to Applications, right-click it and choose Open. If macOS still blocks it, run `xattr -dr com.apple.quarantine /Applications/Relay.app` once. Later updates install without this. Windows SmartScreen may warn on first install: More info, then Run anyway.

Build from source with the [development guide](docs/DEVELOPMENT.md). Use either the app or `relay service` on a machine, not both.

## Start

1. **Install Relay** on each computer and name the device.
2. **Pair them.** On one machine, open Peers and choose Pair a device. On the other, enter the code. On the same network that is the whole step. Over a VPN such as [Tailscale](https://tailscale.com), also enter the other machine’s address.
3. **Share a folder.** Create a space (a name for a set of folders, such as Projects), add a folder, and share the space. On the other computer, accept the offer and choose where that folder should live. An empty folder is the simplest start.

A change on one machine is on the others a moment later, in either direction. Add another computer the same way: it only needs to pair with one machine that already has the folder.

The same steps exist in the terminal (`relay pair`, `relay space create`, `relay mount add`, `relay share`). The full reference is [Using Relay](docs/USAGE.md).

## What you get

- **Local files.** You edit normal folders with normal apps. Relay reconciles those copies.
- **Your devices only.** Each computer is pinned by its own key. A space syncs only with the devices you share it with.
- **Nothing discarded.** If the same file changes on two machines before they meet, both versions are kept. Every recorded version can be restored.
- **Catch-up.** A machine that was offline picks up what it missed when it reconnects. A folder you leave on an always-on machine can hold changes in between, if you want that.
- **As many machines as you own.** Files can also pass through a computer that has the folder when two others are not directly connected.

Git repositories sync like any other folder, including `.git`. Relay is continuous working state. Git stays your history, branches, and review.

## More

- [Using Relay](docs/USAGE.md) — commands, conflicts, safety rules, where data lives
- [Desktop app](apps/relay-desktop/README.md)
- [Roadmap](docs/ROADMAP.md) — phases 0–13 shipped; phase 14 is next
- [Design](docs/DESIGN.md)
