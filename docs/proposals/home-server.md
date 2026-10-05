# Home server

**Status:** Stage 1 (server role, D45) and Stage 2 (store mode, D47) shipped. Stage 3 is next.

Build plan for an always-on Relay server: a NAS, mini PC, or VPS that joins
the sync chain, holds a durable copy of every space, keeps history the other
devices cannot erase, and gives roaming devices one place to meet. This does
not amend [`DESIGN.md`](../DESIGN.md). The product direction and the account
model are D44. Stage 1, the server role, is D45; Stage 2, store mode, is
D47. The rest becomes decisions as each stage lands.

## Product tiers (D44)

1. **Sync.** Multi-device sync between your own machines. Shipped through
   Phase 13. Works with no account and no server.
2. **Home server.** An optional Relay server you run yourself, in the sync
   chain. This document.
3. **Hosted server, later.** The same server binary, run for people who do
   not want their own hardware. Blind by default (below): it never holds
   space keys. Billing exists only at this tier.

Everything is self-hostable. There is no third-party backend (D44 replaced the Supabase backend of the original design).

## What exists today

| Need | Today | Gap |
| --- | --- | --- |
| Always-on peer | Any device running `relay run` is a peer. A middle device forwards when it has the mount attached (D26) | `relay service` is macOS and Windows only. No container image or systemd unit. A headless device must accept each offer and attach each mount by hand |
| Offline catch-up | Filesystem mailbox directory (D29), sealed objects (D30) | Needs a shared directory both devices can reach. Entry logs are plaintext |
| Durable copy without a working tree | `metadata` mode stores rows only (D35) | No mode that keeps bytes in the object store without writing files |
| History | `relay history` / `relay restore` per entry. `relay gc` removes unreferenced objects | No retention policy, no whole-space restore. Any device's tombstone is history on every device |
| Rendezvous | `nat/<device>` and `transport/relay` files in the mailbox (D32, D34) | Needs the mailbox directory. Pairing needs a direct UDP path (D25) |
| Remote setup | Manage grant and remote calls (D37, D39) | Setting a headless server up from the desktop app works only after a grant is given at pairing |
| Online-only files | Windows Cloud Files placeholders (D43) | macOS File Provider and Android are later stages of `os-integration.md` |

## Decisions to adopt

### A peer with a role, never the authority

The server is an ordinary device: its own Ed25519 identity, paired like any
other, version vectors, the same QUIC protocol. It does not order writes, take
locks, or resolve conflicts differently (D18, D31). Devices on the same LAN
keep syncing directly when it is down. It is not the authoritative filesystem (DESIGN §5).

It differs in four narrow ways:

1. **Durability target.** It counts toward `minimum_durable_replicas` (DESIGN §5).
   Clients show "backed up" from its acks, never from its presence.
2. **History owner.** Its retention policy decides what it deletes. Clients
   cannot shorten it.
3. **Preferred source.** First stop for multi-source fetch (D34) and for
   `demand` hydration (D35, D43).
4. **Rendezvous.** One stable address. It runs `relay transport --serve` and
   publishes candidates, replacing the mailbox directory as the discovery
   channel.

### Server role

A local setting, `role = server`, set with `relay server enable --data DIR`.

- **Auto-join.** A server joins every space offered by a peer allowed to
  manage it (D37) and attaches each mount under `DIR/<space>/<mount>`.
  Offers from other peers, including members adopted from offers (D26), wait
  for a manual join as today.
- **No UI, no tray.** Linux first: a container image (amd64, arm64) and a
  systemd unit. Windows under WSL2 or Docker is the stand-in until real
  hardware exists.
- **Managed from the desktop.** Pair the server with `--allow-manage` on the
  server side so the desktop app can browse and set it up (D37, D39).

### Store mode

Shipped (D47). A fifth materialization mode, `store`: fetch the object into
the local store and do not write the working tree. A server attaches every
mount in `store` and can use `full` for a subtree that should also be
browsable over SMB on the NAS. Like `metadata`, an absent file in `store`
mode is not a delete. Index-only devices now resolve conflicts exactly as
writers do, including clean text merges.

### Backup is not sync

Sync faithfully copies deletes, overwrites, and ransomware. A server that only
mirrors is not a backup.

- **Append-only from clients.** A client adds versions and tombstones. It
  cannot purge objects or history on the server. Only the server's retention
  policy, or a command run on the server, deletes.
- **Retention per space**, applied on the server and independent of client
  tombstones: for example every version for 30 days, daily for a year, the
  latest forever. Generalizes the D29 mirror.
- **Snapshots.** A snapshot is an index manifest over the object store.
  `relay restore SPACE --at TIME` restores a whole tree.
- **Mass-change hold.** The D22 receive-side hold, extended to many files
  rewritten with high-entropy content: freeze retention GC and alert.
- **Offsite copy** is a second server (a friend's machine, a VPS, or tier 3)
  in blind mode, replicating from the first like any peer.

### Two trust levels

- **Trusted** (your own hardware, default): holds space keys, seals objects
  at rest (D30), can serve file contents, and later thumbnails, search, or a
  web view. Disk encryption is the OS's job.
- **Blind** (VPS, a friend's house, tier 3): no space keys. Stores sealed
  objects and logs, relays, acks. Needs encrypted entry logs and keyed object ids first, or it learns paths, sizes, and content hashes.

`peer revoke` must take effect on the server at once. `space rotate` does not
re-encrypt history; say so.

### Accounts (D44)

The account is a directory and sign-in layer, never the trust root (DESIGN §6).
OIDC sign-in, Google first. The directory stores account email, device
names, device public keys, and addresses. It never stores file contents or
space keys. We run a free default; the same component ships inside the
server for self-hosters with their own OIDC provider.

A new device that signs in is pending until an existing device approves it
and wraps the space keys for it. With no device online, the recovery secret
(D30) works instead. A stolen Google login gets device names, not data.
Approved devices on one account hold the D37 manage grant on each other.
Code pairing stays, and the sync engine never depends on accounts (DESIGN §6).

The account-wide grant waits on manage-grant scoping (N-H4), so that a
signed-in device cannot read outside the folders the grant should cover.

## Stages

Each stage ships on its own.

1. **Server role.** `relay server enable`, auto-join and auto-attach,
   container image and systemd unit, a server peer in `relay-vopr`.
2. **Store mode.** `store` materialization; the server default. Shipped (D47).
3. **Backup semantics.** Append-only enforcement, retention, snapshots,
   whole-space restore, mass-change hold.
4. **Backed-up state in the app**, from server acks.
5. **Accounts and directory.** Sign-in, enrollment with approval,
   account-wide manage grant, directory rendezvous (removes D25's direct
   path requirement for pairing). Can run alongside 2–4.
6. **Blind mode.** Encrypted logs, keyed object ids, server-to-server
   offsite.
7. **Hosted server (tier 3).** Multi-tenancy, quotas, billing around the
   same binary.

A web UI and share links come after all of this, if at all.

## Not doing

- Making the server authoritative or adding locks.
- Replicating device config through the server (D37: remote calls).
- Calling a mirror with tombstones a backup.
- A third-party backend, or accounts inside the sync engine.
- A public HTTP surface in the first versions. QUIC with pinned mutual TLS
  is the only listener.
