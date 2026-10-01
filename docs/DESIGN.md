# Relay
## Realtime file sync across your machines

**Status:** Original design specification. Live progress is [`ROADMAP.md`](ROADMAP.md).  
**Platforms:** macOS and Windows, plus a command-line build  
**Scope:** The folders you choose, across your own machines  
**Core principle:** Every participating machine works from ordinary local files. Relay continuously reconciles those local copies and can use an encrypted folder you control to bridge periods when devices are not simultaneously online.

Implementation has passed the original MVP (about Phase 4). Where this file and [`DECISIONS.md`](DECISIONS.md) disagree, DECISIONS.md wins. Do not infer what is built from "eventually", "initial release", or the phase list in §51. Current progress and the next phase are in [`ROADMAP.md`](ROADMAP.md).

---

# 1. Executive Summary

Relay is a local-first file synchronization system designed around a simple user expectation:

> Switch devices and continue working from the same filesystem state without manually committing, pushing, pulling, copying, mounting remote disks, or depending on another machine being online.

The files have to exist on the machine you are using. Copying them by hand, or waiting on a commit and a pull, gets in the way. A remote disk fails when the other machine is off.

Relay keeps a normal local copy of each selected folder on every participating device and continuously reconciles changes between them.

A user can synchronize any collection of filesystem roots, include or exclude subtrees, and define different replication policies for different sets of files and devices.

Examples include:

- `~/Projects/**` on a laptop, a desktop, and a workstation.
- A subset of `Documents/**` only between personal machines.
- Notes or a photo library between a laptop and a desktop.
- Configuration files across all machines.
- A work subtree only between a work computer and one trusted desktop.
- Large datasets on a desktop but metadata-only or excluded on portable devices in later versions.

Relay keeps Git conceptually separate:

- **Relay** manages continuous working-state replication.
- **Git** manages intentional project history, branches, collaboration, releases, and review.

The fundamental system is a distributed set of replicas with explicit replication policy, version tracking, content-addressed storage, peer authentication, and optional encrypted durable storage.

---

# 2. Goals

## 2.1 Primary goals

Relay should:

1. Keep selected files synchronized across multiple user-owned machines.
2. Keep all working files local on devices that materialize them.
3. Continue functioning when other devices are offline.
4. Automatically catch devices up when they reconnect.
5. Support direct peer-to-peer transfer when devices overlap online.
6. Support an optional persistent encrypted backend when they do not.
7. Detect concurrent edits without relying on wall-clock timestamps.
8. Preserve data rather than silently overwrite divergent changes.
9. Support arbitrary filesystem roots rather than one fixed sync folder.
10. Support different file subsets on different device combinations.
11. Be safe across Windows and macOS from the first public build.
12. Keep the synchronization engine independent from the GUI.
13. Keep protocol and data models extensible enough for Linux and generalized use later.
14. Make all destructive behavior explicit and conservative.
15. Remain understandable enough that a user can inspect what is synchronized, where replicas exist, and why an item is out of sync.

## 2.2 Secondary goals

Relay should eventually support:

- Continuous save-level history.
- Automatic text three-way merge.
- Selective materialization.
- Self-hosted durable relay nodes.
- End-to-end encrypted cloud persistence.
- Device groups.
- Advanced replication rules.
- LAN discovery.
- NAT traversal.
- Optional global discovery.
- Selective one-way replication.
- Remote fetch of metadata-only files.
- Editor status, conflict notification, and restore.
- Application-aware workflows such as a script after sync.

---

# 3. Non-Goals for the Initial Release

The first implementation should explicitly avoid solving unnecessary distributed-systems problems.

Initial versions will **not** attempt to provide:

- Git replacement.
- Collaborative multi-user editing.
- POSIX-perfect filesystem semantics across all platforms.
- Full distributed filesystem mounting.
- Remote filesystem access similar to SMB/NFS.
- Arbitrary WAN NAT traversal without supporting infrastructure.
- Mobile clients. An Android foreground shell has since shipped (see [`ROADMAP.md`](ROADMAP.md)); iOS has not.
- Files-on-demand.
- General CRDT editing.
- Very large media-file optimization.
- Cross-user sharing and ACLs.
- High-scale enterprise administration.
- Continuous cloud backup as the authoritative source.
- Automatic destructive removal when a replication policy changes.
- Automatic following of arbitrary filesystem symlinks.

---

# 4. Product Model

Relay should be designed around a logical namespace rather than physical filesystem paths.

The main conceptual entities are:

```text
Ecosystem
├── Devices
├── Spaces
│   ├── Mounts
│   │   └── Entries
│   └── Replication Policies
├── Device Groups
└── Durable Replica(s)
```

## 4.1 Ecosystem

An **Ecosystem** is the trust domain belonging to one user or organization.

It contains:

- Trusted device identities.
- Spaces.
- Device groups.
- Replication policies.
- Cryptographic key relationships.
- Optional durable backend configuration.

A device may belong to the ecosystem without automatically receiving every Space.

---

# 5. Spaces

A **Space** is a logical synchronization namespace.

Examples:

```text
Personal
Work
Research
Games
```

A Space may contain multiple unrelated filesystem roots.

Example logical namespace:

```text
Personal
├── code/
├── documents/
├── configs/
└── games/
```

These are logical paths, not operating-system paths.

---

# 6. Mounts

A **Mount** maps one logical namespace to one physical path on each device.

Example:

### MacBook

```text
code/      -> /Users/alice/Code
documents/ -> /Users/alice/Documents
games/     -> /Users/alice/Games
```

### Windows desktop

```text
code/      -> D:\Code
documents/ -> C:\Users\Alice\Documents
games/     -> D:\Games
```

The synchronization engine should never treat these physical paths as the identity of a file.

The canonical identity is:

```text
(SpaceId, MountId, RelativePath)
```

For example:

```text
Space: Personal
Mount: code
Path: notes/todo.md
```

This survives different operating systems, disk layouts, and usernames.

---

# 7. File Selection

Relay must support arbitrary include/exclude behavior.

A user may choose:

```text
~/Code/**
```

but exclude:

```text
~/Code/old-projects/**
~/Code/**/.git/**
~/Code/**/node_modules/**
```

A user may also synchronize only selected subtrees:

```text
~/Documents/Residency/**
~/Documents/Notes/**
```

without synchronizing the entire Documents directory.

## 7.1 Rule model

Each mount should support rules such as:

```toml
include = ["**"]

exclude = [
  "**/.git/**",
  "**/node_modules/**",
  "**/target/**"
]
```

More advanced ordered include/exclude rules can be added later.

## 7.2 UI representation

The UI should present a hierarchical tree:

```text
☑ ~/Code
   ☑ projects
   ☑ notes
   ☐ old-projects
   ☑ evident
      ☐ node_modules

☑ ~/Documents
   ☑ Residency
   ☐ Downloads
```

The UI should generate rules, not store a static enumeration of every selected file.

This ensures new files and folders inherit policy correctly.

## 7.3 `.relayignore`

Relay should support a per-tree ignore file:

```text
.relayignore
```

Initial syntax can resemble glob-based ignore systems:

```text
**/.git/**
**/node_modules/**
**/dist/**
**/*.log
```

Relay may later support optionally importing `.gitignore`, but Git ignore behavior must not be treated as identical to synchronization behavior.

---

# 8. Replication Policies

Relay should not assume that every file in a Space is synchronized to every device.

The core abstraction is:

> **selector -> desired replica set**

A replication policy answers:

1. Which logical entries does this policy match?
2. Which devices should contain those entries?
3. What durability guarantees apply?

Example:

```text
Policy: Work Development

Selector:
  code/work/**

Targets:
  Work PC
  Desktop
```

Another:

```text
Policy: Portable

Selector:
  notes/**
  photos/**

Targets:
  MacBook
  Linux Laptop
```

Policies may overlap.

If one file matches multiple policies, its desired replica set is the union of all applicable target sets.

For example:

```text
Policy 1:
A + B -> Work PC, Desktop

Policy 2:
A + C -> MacBook, Linux

Derived replicas:

A -> Work PC, Desktop, MacBook, Linux
B -> Work PC, Desktop
C -> MacBook, Linux
```

There remains one logical `A`, one version history, and one conflict lineage.

---

# 9. Device Groups

Device groups reduce repetition.

Example:

```text
Personal Computers
├── MacBook
├── Desktop
└── Linux

Work Machines
├── Work Laptop
└── Desktop
```

Policies may target either devices or groups.

Groups should expand into concrete device IDs before reaching the sync engine.

The sync engine should operate only on derived desired replica sets.

---

# 10. Replica Modes

Initial versions need only full materialization and durable storage, but the model should allow future modes.

```rust
enum ReplicaMode {
    Materialized,
    DurableStore,

    // Future:
    MetadataOnly,
}
```

### Materialized

The device has ordinary filesystem files.

### DurableStore

The replica stores metadata and encrypted content objects but does not reconstruct a working filesystem.

### MetadataOnly

Future mode where a device knows an object exists without storing its contents.

---

# 11. Local-First Behavior

A materialized device must always work without network connectivity.

Relay must never require:

- A mounted network share.
- A live server connection.
- Another device to be online.
- A cloud account to open local files.

If the MacBook is offline:

```text
editor -> local filesystem
```

works normally.

When connectivity returns, Relay reconciles.

---

# 12. Synchronization Model

Relay should use **desired-state reconciliation**, not direct event forwarding.

Filesystem events are signals that local state may have changed.

Network messages are signals that remote state has changed.

Periodic scans are signals that Relay should re-evaluate local state.

All feed the reconciler.

```text
filesystem watcher ─┐
periodic scanner ───┼──> local index ──┐
network updates ────┘                  │
                                      v
                                 reconciler
                                      │
                     ┌────────────────┴───────────────┐
                     v                                v
               transfer objects                update filesystem
```

This avoids making unreliable filesystem events authoritative.

---

# 13. File Change Detection

Each materialized mount should have:

1. Native filesystem watcher.
2. Debouncer/coalescer.
3. Dirty-path queue.
4. Filesystem metadata inspection.
5. Content hashing when required.
6. Periodic full/partial scanner.

Initial cross-platform library:

```text
notify
```

Native watcher sources:

- macOS: FSEvents or native backend exposed by the library.
- Windows: ReadDirectoryChangesW or backend exposed by the library.

## 13.1 Watcher semantics

Relay should treat watcher events as:

```text
"path may have changed"
```

not:

```text
"this exact operation definitively occurred"
```

Editors often save via temporary files, rename-overwrite sequences, or multiple partial writes.

Relay should coalesce these into stable final state.

---

# 14. Content Addressing

Initial versions should store each small file as one immutable content-addressed object.

```text
ObjectId = BLAKE3(file bytes)
```

Object store:

```text
~/.relay/
  objects/
    2a/
      9d/
        2a9d...
```

Benefits:

- Deduplication.
- Easy integrity verification.
- Immutable transfer units.
- Natural save history.
- Easy rollback.
- Clean durable storage model.

## 14.1 Whole-file objects first

Do not implement chunking initially.

For source files and typical documents:

```text
10 KB
80 KB
500 KB
2 MB
```

whole-file transfer is simpler and sufficiently efficient.

Later versions may introduce chunking above a threshold such as 8–32 MB.

Possible future approach:

- FastCDC content-defined chunking.
- BLAKE3 chunk hashes.
- Manifest objects.

---

# 15. Version Vectors

Relay should not determine conflict order from timestamps.

Each logical entry maintains a version vector.

Example:

```text
Desktop = D
MacBook = M
```

Initial:

```text
{D: 1}
```

Desktop modifies:

```text
{D: 2}
```

Mac receives and edits:

```text
{D: 2, M: 1}
```

This causally dominates `{D:2}`.

If both devices independently edit after the same base:

```text
Desktop: {D:3, M:1}
MacBook: {D:2, M:2}
```

neither dominates the other.

That indicates a real concurrent modification.

## 15.1 Comparison states

Version vector comparison yields:

```text
Equal
LocalDominates
RemoteDominates
Concurrent
```

These should be explicit core-domain concepts.

---

# 16. Conflict Handling

Initial behavior should be conservative.

For binary or unmergeable content:

```text
foo.lua
foo.relay-conflict-<device>-<timestamp>.lua
```

No data should be silently discarded.

## 16.1 Future automatic three-way merge

For source code and text files, Relay can improve on traditional sync tools.

If Relay retains:

```text
base = AAA
Desktop = BBB
MacBook = CCC
```

then it can perform:

```text
merge(base, BBB, CCC)
```

If clean, create:

```text
DDD
```

and converge both peers automatically.

If not clean, materialize a conflict with enough ancestry metadata for the user or editor to resolve it.

This feature should be introduced only after base replication is proven reliable.

---

# 17. Deletes and Tombstones

Deletes must be versioned operations.

If a file is simply removed from metadata, an offline machine may later reintroduce it.

Therefore Relay should retain tombstones:

```text
Entry:
  path = old/file.lua
  deleted = true
  vector = {...}
```

A tombstone may be garbage-collected only after all relevant replicas have acknowledged a descendant state that incorporates the deletion.

---

# 18. Durable Offline Relay

Direct peer-to-peer synchronization alone fails if devices never overlap online.

Example:

```text
Desktop edits
Desktop shuts down
MacBook starts later
```

A persistent third replica solves this.

The durable node should be a **non-materializing replica**, not an authoritative filesystem.

It stores:

- Encrypted immutable content objects.
- Encrypted or minimally exposed synchronization metadata.
- Version/index state.
- Acknowledgement state.
- Tombstones needed for convergence.
- Optional short rolling history.

It does **not** need to reconstruct directories.

---

# 19. Durability Based on Acknowledgement, Not Presence

Relay should not use:

```text
if fewer than two devices are online -> upload
```

Presence does not imply replication completion.

Instead, every version should have known acknowledgements.

Example:

```text
Version V102

Desktop    ✓
MacBook    offline
Cloud      ✓
```

This is durable.

A configurable policy can state:

```text
minimum_durable_replicas = 2
allow_durable_store = true
```

Relay should persist to the durable replica whenever required to satisfy the durability target.

---

# 20. Durable Storage Modes

Future user-facing modes:

## 20.1 Peer Only

```text
MacBook <-> Desktop
```

No persistent third replica.

## 20.2 Mailbox

The durable store retains only what is needed to catch registered replicas up.

Objects can be garbage-collected after all relevant devices acknowledge current or descendant state, subject to a grace period.

## 20.3 Mirror

The durable store retains the latest complete logical state of the selected Space.

This provides device-loss recovery.

For source code and documents, this may be a reasonable default because storage requirements are small.

## 20.4 Rolling History

Optional extension:

```text
latest state: indefinite
unacknowledged versions: indefinite
old versions: 7 days
```

This provides low-cost recovery from accidental overwrites.

---

# 21. Device Identity

Every device should generate a long-lived cryptographic identity during first launch.

Recommended:

```text
Ed25519 identity keypair
```

Public-key fingerprint becomes `DeviceId`.

Private keys should be stored using platform-native secure mechanisms where practical.

### Windows

- DPAPI-protected secret storage.
- Windows Credential Manager if appropriate.

### macOS

- Keychain.

The cloud account must not be treated as the device's cryptographic identity.

---

# 22. Device Pairing

Pairing should distinguish:

1. Device registration.
2. Device trust.
3. Space authorization.
4. Replication-policy membership.

A new device can belong to the ecosystem without receiving every Space.

## 22.1 Initial UX

Existing device:

```text
Add Device
-> Generate pairing code
```

New device:

```text
Join Existing Ecosystem
-> Enter pairing code
```

Then existing device:

```text
MacBook wants to pair
Fingerprint: XXXX XXXX ...

[Approve] [Reject]
```

## 22.2 Pairing codes

A short pairing code should reference an ephemeral session.

It should not itself be a long-lived secret.

A pairing session may contain:

- Candidate device public key.
- Inviter public key.
- Ephemeral key material.
- Nonce.
- Expiry.
- One-time session identifier.

## 22.3 Future QR pairing

QR codes may contain:

- Pairing session ID.
- Inviter device ID.
- Public fingerprint.
- Ephemeral public key.
- Rendezvous endpoint.

Long-lived Space keys should be transferred only inside the authenticated encrypted handshake.

---

# 23. Account-Backed vs Accountless Pairing

Relay should allow both architectures eventually.

## 23.1 Account-backed

An account makes these easier:

- Device discovery.
- Remote pairing.
- Durable storage.
- Ecosystem metadata.
- Recovery flow.
- Billing if hosted storage becomes a product.

## 23.2 Accountless

Trusted devices can pair directly without an account.

This supports:

- LAN-only usage.
- Self-hosting.
- Privacy-sensitive users.
- Offline environments.

The synchronization engine should not depend on account authentication.

---

# 24. Key Hierarchy

Do not encrypt all Spaces with one account-wide symmetric key.

Recommended hierarchy:

```text
Ecosystem identity
├── Space A key
├── Space B key
└── Space C key
```

Each Space receives a random 256-bit symmetric key.

For each authorized device, store a wrapped copy of that Space key.

Conceptually:

```text
Space A

Desktop -> encrypted SpaceKeyA
MacBook -> encrypted SpaceKeyA
Linux   -> encrypted SpaceKeyA
```

The backend stores wrapped keys but cannot decrypt the Space key.

---

# 25. Device Revocation

Revocation should:

1. Mark device identity revoked.
2. Reject future peer sessions.
3. Stop serving objects to the device.
4. Remove it from active replication targets.
5. Optionally rotate affected Space keys.

Two conceptual modes:

### Soft revoke

Stops future synchronization.

The revoked device may retain already-decrypted data.

### Hard revoke

Rotates relevant Space keys for future data.

Historic data may remain readable to the revoked device unless old objects are re-encrypted.

---

# 26. Recovery

If all trusted materialized devices are lost but encrypted durable storage remains, Relay needs a recovery path.

Recommended future mechanism:

```text
Recovery key / recovery phrase
```

It should wrap a root recovery secret capable of recovering Space keys.

Users should be explicitly told:

- Relay cannot recover E2E-encrypted data without a trusted device or recovery key.
- The recovery secret should be stored separately.

This feature is not required for the first peer-only prototype.

---

# 27. Network Transport

Recommended initial stack:

```text
QUIC via quinn
```

Benefits:

- TLS encryption.
- Multiple independent streams.
- Better fit than building custom multiplexing over TCP.
- Cross-platform support.
- Peer can act as both initiator and listener.

Conceptual streams:

```text
QUIC connection
├── control/index stream
├── object transfer stream A
├── object transfer stream B
└── heartbeat / session control
```

Initial networking can assume reachable device addresses, ideally over Tailscale or LAN.

NAT traversal should not be part of the earliest implementation.

---

# 28. Wire Protocol

Recommended:

```text
Protocol Buffers via prost
```

Primary goals:

- Stable schema evolution.
- Compact binary messages.
- Language independence if future clients use other languages.

Initial message concepts:

```protobuf
DeviceHello
SpaceSummary
IndexSnapshot
IndexDelta
EntryUpdate
ObjectRequest
ObjectChunk
ObjectComplete
Ack
Error
Heartbeat
```

Object transfers may initially send the entire file payload.

Later versions can use chunk manifests.

---

# 29. Persistent Metadata Database

Recommended:

```text
SQLite + rusqlite
```

Reasons:

- Embedded.
- Transactional.
- Inspectable.
- Easy debugging.
- Mature.
- Appropriate for a single local daemon.

Enable:

```text
WAL
foreign_keys = ON
```

Prefer a dedicated database worker/task instead of scattering asynchronous DB access across the application.

---

# 30. Core Metadata Schema

Illustrative schema:

```text
ecosystem
---------
id

devices
-------
id
name
public_key
status
last_seen

spaces
------
id
name

mounts
------
id
space_id
namespace

device_mounts
-------------
device_id
mount_id
local_path
materialization_mode

entries
-------
space_id
mount_id
relative_path
entry_type
object_hash
size
mtime_hint
deleted

entry_versions
--------------
space_id
mount_id
relative_path
device_id
counter

objects
-------
hash
size
local_path
ref_count
created_at

history
-------
id
space_id
mount_id
relative_path
object_hash
device_id
created_at

device_groups
-------------
id
name

device_group_members
--------------------
group_id
device_id

replication_policies
--------------------
id
space_id
name
sync_mode
minimum_durable_replicas
allow_durable_store

replication_rules
-----------------
policy_id
mount_id
pattern
rule_type
priority

replication_targets
-------------------
policy_id
target_type
target_id

replica_acks
------------
space_id
mount_id
relative_path
device_id
version_digest
acknowledged_at
```

Schema should evolve through explicit migrations.

---

# 31. Path Normalization

Cross-platform path handling must be designed early.

Internal logical paths should:

- Always be relative.
- Use `/` as separator.
- Never contain drive letters.
- Never contain platform-specific root prefixes.
- Be represented in normalized UTF-8 form where practical.
- Preserve display casing.
- Detect case collisions before materialization.

Example:

```text
notes/todo.md
```

not:

```text
C:\Users\Alice\Documents\notes\todo.md
```

## 31.1 Case collisions

Linux can distinguish:

```text
Foo.lua
foo.lua
```

while common Windows/macOS configurations may not.

Relay should detect unsafe collisions and refuse materialization on incompatible targets rather than guess.

---

# 32. Symlink Policy

Default behavior:

```text
preserve symlink where supported
do not recursively follow external directory symlinks
```

Following arbitrary symlinks can escape the selected mount and create loops or unintended synchronization.

Initial configuration:

```text
follow_external_symlinks = false
```

Windows junctions and symlinks require deliberate handling.

Windows junctions may still be useful when an application expects files in a fixed directory and the synced folder lives somewhere else.

---

# 33. Overlapping Mounts

Relay should reject ambiguous physical mount overlap by default.

Example:

```text
~/Code
~/Code/foo
```

If both are independent mounts, the same physical file can acquire two logical identities.

Initial behavior:

```text
Reject overlapping mounts with a clear error.
```

A future advanced mode may allow shadowing.

---

# 34. Local Object Store

Recommended layout:

```text
~/.relay/
├── relay.db
├── objects/
│   ├── 00/
│   ├── 01/
│   └── ...
├── tmp/
├── logs/
└── config/
```

On Windows, use an appropriate app-data location rather than literal `~/.relay`.

On macOS, use Application Support.

The logical organization remains equivalent.

---

# 35. Atomic Materialization

Relay must never stream a partial network transfer directly into the live destination file.

Process:

```text
request object
-> receive into temporary file
-> verify BLAKE3
-> fsync/close where appropriate
-> atomically replace destination
```

This protects against:

- Interrupted transfers.
- Process crashes.
- Corrupt objects.
- Connection loss.
- Partial writes.

Platform-specific atomic replacement behavior must be tested carefully.

---

# 36. Save-Level History

Because all file contents are immutable objects, Relay can cheaply track local history.

Example:

```text
10:31:01 notes.md -> AAA
10:31:14 notes.md -> BBB
10:31:37 notes.md -> CCC
```

Future CLI:

```bash
relay history notes/notes.md
relay restore notes/notes.md --at "10:31"
```

History should remain separate from Git history.

Git remains intentional project history.

Relay history is continuous working-state recovery.

---

# 37. Cloud Encryption

When durable hosted storage is introduced, all file content should be encrypted before upload.

Recommended direction:

```text
XChaCha20-Poly1305 or another well-reviewed AEAD construction
```

Use a per-Space key.

Potential object flow:

```text
plaintext
  -> BLAKE3
  -> object identity
  -> encrypt with Space key
  -> durable store
```

To reduce known-plaintext hash correlation, public cloud object identifiers can be derived using keyed hashing or opaque random IDs rather than directly exposing raw BLAKE3 object hashes.

Metadata should also be minimized or encrypted where practical.

---

# 38. Backend Architecture

Initial durable service can be built using:

```text
Supabase Postgres
Supabase Storage
```

because it is convenient for:

- Accounts.
- Device metadata.
- Pairing sessions.
- Replication metadata.
- Object storage.
- Presence/notifications.

However, the daemon should depend only on a backend abstraction.

Example:

```rust
trait DurableReplica {
    async fn put_object(...);
    async fn get_object(...);
    async fn publish_index(...);
    async fn fetch_index(...);
    async fn acknowledge(...);
}
```

Possible future implementations:

```text
Relay Cloud
Supabase
S3
Cloudflare R2
self-hosted relay server
NAS
home Raspberry Pi
```

The persistent service must not be the authoritative filesystem.

---

# 39. Daemon Architecture

The synchronization engine should run as a standalone process:

```text
relayd
```

The GUI and CLI are clients of the daemon.

```text
               relay CLI
                   |
                   v
filesystem <-> relayd <-> peers
                   ^
                   |
               Relay GUI
```

Closing the GUI should not stop synchronization.

---

# 40. Platform Service Behavior

Initial platforms:

## macOS

Use:

```text
LaunchAgent
```

running in the logged-in user's session.

## Windows

Prefer a per-user background process/startup mechanism initially rather than a privileged Windows Service.

Rationale:

- Files belong to the user.
- User profile paths matter.
- Credential access is simpler.
- Administrator privileges should not be required.

A service model can be reconsidered later.

---

# 41. Local IPC

CLI and GUI should communicate with the daemon through local IPC.

Potential implementation:

```text
interprocess
```

Mapping to:

- Unix-domain sockets on macOS.
- Named pipes on Windows.

Potential local API methods:

```text
GetStatus
ListSpaces
ListDevices
ListMounts
ListConflicts
PairDevice
ApprovePairing
Pause
Resume
Rescan
GetHistory
RestoreVersion
SetPolicy
```

The daemon is the sole authority.

The CLI and GUI should contain no independent synchronization logic.

---

# 42. GUI

Recommended:

```text
Tauri 2
Vue 3
TypeScript
Vite
```

The GUI should initially expose only the common workflow.

Example:

```text
Relay

SPACES
Personal                      Synced
  MacBook                     Online
  Desktop                     Online
  Durable Store              Current

ACTIVITY
10:31:22 Core.lua             Mac -> Desktop
10:31:18 Config.lua           Mac -> Desktop

DEVICES
MacBook                       This device
Desktop                       Online
```

Advanced replication rules should be added only after the base system is usable.

---

# 43. CLI

Recommended:

```text
clap
```

Initial commands:

```bash
relay status
relay spaces
relay devices
relay pair
relay approve
relay rescan
relay conflicts
relay pause
relay resume
relay logs
```

Later:

```bash
relay history
relay restore
relay policy
relay fetch
relay gc
```

CLI must operate through `relayd`, not bypass it.

---

# 44. Rust Codebase Organization

Use a Cargo workspace.

```text
relay/
├── Cargo.toml
├── README.md
├── docs/
│   └── DESIGN.md
│
├── crates/
│   ├── relay-core/
│   ├── relay-fs/
│   ├── relay-db/
│   ├── relay-proto/
│   ├── relay-net/
│   ├── relay-crypto/
│   ├── relay-policy/
│   ├── relay-reconcile/
│   ├── relay-store/
│   └── relay-merge/
│
├── apps/
│   ├── relayd/
│   ├── relay-cli/
│   └── relay-desktop/
│
├── proto/
│   └── relay.proto
│
├── migrations/
│   └── ...
│
└── tests/
    ├── integration/
    ├── fixtures/
    └── simulation/
```

Avoid making every crate tiny. Crate boundaries should represent meaningful architectural isolation.

---

# 45. Crate Responsibilities

## `relay-core`

Pure domain types.

Examples:

```text
SpaceId
MountId
DeviceId
EntryKey
ObjectId
VersionVector
EntryVersion
ReplicaState
ConflictState
```

No filesystem or network side effects.

This crate should be heavily unit-tested.

---

## `relay-fs`

Responsible for:

- Filesystem scanning.
- Watcher abstraction.
- Path normalization.
- Atomic writes.
- Symlink handling.
- Filesystem metadata.
- Temporary-file behavior.
- Windows/macOS platform differences.

---

## `relay-db`

Responsible for:

- SQLite schema.
- Migrations.
- Queries.
- Transactions.
- Durable local index.
- History.
- Acknowledgements.

---

## `relay-proto`

Generated protobuf types and conversion helpers.

Protocol structs should not leak throughout the domain layer.

---

## `relay-net`

Responsible for:

- QUIC connections.
- Peer session lifecycle.
- Handshake.
- Stream routing.
- Retry/reconnect.
- Object transfer.
- Index exchange.

---

## `relay-crypto`

Responsible for:

- Device identities.
- Key generation.
- Signatures.
- Space keys.
- Key wrapping.
- Cloud object encryption.
- Secure serialization.

Keep cryptographic behavior isolated and reviewable.

---

## `relay-policy`

Responsible for:

- Include/exclude evaluation.
- Device-group expansion.
- Matching replication policies.
- Deriving desired replica sets.

Input:

```text
EntryKey
PolicySet
DeviceGroupSet
```

Output:

```text
DesiredReplicaSet
```

---

## `relay-reconcile`

Core synchronization state machine.

Inputs:

- Local index state.
- Remote index state.
- Desired replica set.
- Object availability.
- Acknowledgements.

Outputs:

```text
RequestObject
AdvertiseEntry
MaterializeObject
CreateConflict
PublishAck
PersistToDurableStore
GarbageCollect
NoOp
```

This should be deterministic and testable independently of real networking.

---

## `relay-store`

Content-addressed object storage.

Responsible for:

- Put/get object.
- Hash verification.
- Temporary receive objects.
- Object reference tracking.
- Garbage collection.

---

## `relay-merge`

Initially minimal.

Later responsible for:

- Text detection.
- Common-base selection.
- Three-way merge.
- Conflict output.
- Merge ancestry metadata.

---

# 46. Recommended Initial Rust Dependencies

Likely baseline:

```text
tokio
notify
rusqlite
blake3
quinn
rustls
prost
prost-types
serde
serde_json
toml
uuid
clap
tracing
tracing-subscriber
thiserror
anyhow
interprocess
tempfile
```

Potential later dependencies:

```text
xchacha20poly1305 / appropriate AEAD crate
keyring
globset
similar / diff libraries
proptest
```

Dependency count should stay deliberately low.

Avoid early introduction of:

```text
libp2p
RocksDB
Redis
Kafka
Docker
Kubernetes
general CRDT libraries
Electron
```

until a concrete requirement justifies them.

---

# 47. Logging and Diagnostics

Use:

```text
tracing
```

Every synchronization action should have structured context:

```text
space_id
mount_id
entry_path
peer_device_id
object_id
version
action
duration
```

Logs should make it possible to answer:

```text
Why did Relay overwrite/materialize/request this file?
```

The GUI can later expose a human-readable activity log derived from structured events.

---

# 48. Safety Invariants

Several invariants should be treated as architectural requirements.

## 48.1 Never silently discard divergent content

Concurrent versions must either:

- Merge cleanly.
- Be retained as explicit conflicts.

## 48.2 Never write unverified network content into live files

All received objects must hash correctly before materialization.

## 48.3 Never trust filesystem events as authoritative

Periodic reconciliation must heal missed events.

## 48.4 Never identify entries by absolute OS path

Use:

```text
SpaceId + MountId + RelativePath
```

## 48.5 Never treat online presence as proof of durable replication

Require acknowledgements.

## 48.6 Never automatically delete local user data solely because a policy changed

Policy removal should stop future synchronization first.

Explicit cleanup may be offered separately.

---

# 49. Testing Strategy

Distributed file synchronization is dangerous without extensive automated testing.

## 49.1 Unit tests

Focus on deterministic logic:

- Version-vector comparison.
- Rule evaluation.
- Desired replica derivation.
- Tombstone rules.
- Conflict detection.
- Path normalization.
- Device-group expansion.
- Object reference counting.

## 49.2 Filesystem integration tests

Use temporary directories.

Test:

- Create.
- Modify.
- Rename.
- Delete.
- Rapid save sequences.
- Temporary-file editor behavior.
- Nested directory creation.
- Symlinks.
- Permission failures.
- Case collisions.

## 49.3 Two-node simulation tests

Run two Relay instances against temporary trees.

Test:

```text
A edit -> B receives
B edit -> A receives
A/B simultaneous different files
A/B same-file conflict
delete propagation
offline/reconnect
restart during transfer
```

## 49.4 Property testing

Useful for:

- Version-vector ordering.
- Reconciliation convergence.
- Rule semantics.

Example invariant:

> If no new user writes occur and all peers can eventually communicate, repeated reconciliation eventually reaches a stable state.

## 49.5 Crash testing

Kill processes during:

- Object receive.
- DB transaction.
- Atomic replacement.
- Initial scan.
- Conflict creation.

Verify restart correctness.

---

# 50. Initial Windows/macOS Compatibility Requirements

Before calling the first build usable, test:

### macOS

- APFS case-insensitive default.
- APFS case-sensitive where available.
- FSEvents behavior.
- Atomic rename.
- Keychain.
- LaunchAgent.
- Long paths.
- Unicode names.

### Windows

- NTFS.
- ReadDirectoryChangesW behavior through library.
- Junctions.
- Symlinks.
- Locked files.
- Rename-overwrite semantics.
- Windows Defender interaction.
- Long path handling.
- DPAPI/credential storage.
- Per-user background startup.

Cross-platform tests should use the same logical synchronization scenarios.

---

# 51. Phased Implementation Plan

The implementation should be deliberately staged so each phase proves one fundamental property.

This section is the original plan, kept as the definition of each phase.
Phases 0–13 are implemented. Phase 14 is next. What shipped inside each
phase, and what was deferred, is [`ROADMAP.md`](ROADMAP.md).

---

# Phase 0 — Repository and Domain Model

## Objective

Establish architecture without networking or background services.

## Deliverables

- Cargo workspace.
- `relay-core`.
- `relay-db`.
- `relay-store`.
- Core IDs and entry model.
- Version vectors.
- SQLite schema/migrations.
- BLAKE3 content-addressed object store.
- Basic CLI test harness.

## Implement

```text
Space
Mount
Device
EntryKey
ObjectId
VersionVector
EntryState
```

Implement:

- Object put/get.
- Index entry insertion.
- Version increment.
- Tombstones.
- Version comparison.
- History records.

## Success criteria

A test program can:

1. Create a Space.
2. Add a Mount.
3. Index a directory.
4. Hash files into the object store.
5. Detect changes between scans.
6. Record deletes.
7. Print current logical state.

No networking.

---

# Phase 1 — Reliable Local Filesystem Index

## Objective

Make Relay correctly model one machine.

## Deliverables

- `relay-fs`.
- Native watcher.
- Dirty-path queue.
- Debouncing.
- Periodic reconciliation scan.
- Atomic local materialization.
- `.relayignore`.
- Multi-mount support.
- Include/exclude rules.

## Required behavior

User can configure:

```text
~/Code
~/Documents/Notes
```

with exclusions.

Relay maintains an accurate logical index.

## Success criteria

Repeated editor saves do not produce corrupt state.

Restarting Relay reconstructs the same logical index.

Missed watcher events are eventually healed by scanning.

---

# Phase 2 — Two-Node Direct Synchronization

## Objective

Prove real local-first replication.

## Deliverables

- `relay-proto`.
- `relay-net`.
- QUIC peer connection.
- Static/manual peer addressing.
- Device identity.
- Basic mutual peer authentication.
- Index snapshot.
- Index deltas.
- Object requests.
- Object transfer.
- Atomic materialization.
- ACK tracking.

Use LAN addresses or Tailscale.

No cloud.

## Primary test scenario

```text
MacBook:
~/Projects

Windows:
D:\Projects
```

Edit on Mac:

```text
notes.txt
```

File appears on Windows within seconds.

Edit on Windows:

```text
todo.txt
```

File appears on Mac.

Either device can be offline temporarily and catches up when both are online again.

## Success criteria

This phase should already be useful for everyday work across two machines.

---

# Phase 3 — Conflict Safety

## Objective

Guarantee no silent data loss under concurrent editing.

## Deliverables

- Concurrent version detection.
- Conflict materialization.
- Conflict metadata.
- Conflict listing in CLI.
- Tombstone conflict behavior.
- Extensive partition/reconnect tests.

## Scenario

Both machines start from:

```text
Core.lua = V5
```

Then offline:

```text
Mac -> V6M
Windows -> V6W
```

Reconnect.

Relay detects concurrency and preserves both.

## Success criteria

No version is silently overwritten.

---

# Phase 4 — Background Daemon and Local IPC

## Objective

Make Relay operate continuously independent of terminal sessions.

## Deliverables

- `relayd`.
- Local IPC.
- `relay-cli`.
- macOS LaunchAgent setup.
- Windows per-user startup/background process.
- Structured status API.
- Pause/resume.
- Rescan.
- Activity stream.

## Success criteria

Relay starts automatically after login and syncs without an open CLI or GUI.

---

# Phase 5 — Device Pairing

## Objective

Replace manual endpoint configuration with a secure trust workflow.

## Deliverables

- Pairing session.
- Short-lived pairing code.
- Approve/reject flow.
- Trusted-device registry.
- Device naming.
- Secure local identity storage.
- Space authorization.

Initially pairing may use a lightweight rendezvous service or LAN/Tailscale addressing.

## Success criteria

Installing Relay on a second machine should require no manual public-key copying or config editing.

---

# Phase 6 — Tauri Desktop UI

## Objective

Make Relay approachable without reducing architectural rigor.

## Deliverables

- Tauri 2 desktop shell.
- Vue 3 frontend.
- Space list.
- Device list.
- Mount configuration.
- Folder tree selection.
- Sync status.
- Recent activity.
- Pair-device flow.
- Conflict visibility.
- Basic policy editing.

The UI communicates only through daemon IPC.

## Success criteria

A user can install Relay on macOS and Windows, pair devices, select folders, and synchronize without touching config files.

---

# Phase 7 — Replication Policies and Device Groups

## Objective

Expose flexible synchronization topology.

## Deliverables

- Device groups.
- Multiple replication policies.
- Selector -> replica-set policy engine.
- Overlapping policy union.
- Per-policy durability configuration.
- Safe policy-removal behavior.

## Example supported configuration

```text
notes/**      -> Laptop, Desktop
work/**       -> Work PC, Desktop
photos/**     -> Laptop, Desktop
documents/**  -> Laptop, Desktop
```

## Success criteria

Different subsets can converge across different device combinations without creating duplicate logical files.

---

# Phase 8 — Persistent Durable Replica

## Objective

Support offline handoff when peers never overlap.

## Deliverables

- Durable replica abstraction.
- Hosted implementation.
- Encrypted object upload.
- Metadata/index persistence.
- ACK-aware durability.
- Offline catch-up.
- Garbage collection.
- Mailbox mode.
- Mirror mode.

Likely initial backend:

```text
Supabase Postgres + Storage
```

but client architecture must remain backend-independent.

## Core scenario

```text
Desktop edits
Desktop uploads durable state
Desktop turns off
MacBook turns on later
MacBook catches up from durable replica
```

## Success criteria

No direct device overlap is required.

---

# Phase 9 — End-to-End Space Encryption and Recovery

## Objective

Make hosted persistence cryptographically private.

## Deliverables

- Per-Space symmetric keys.
- Device-specific wrapped Space keys.
- Encrypted cloud objects.
- Encrypted metadata where practical.
- Device revocation.
- Key rotation.
- Recovery-key workflow.

## Success criteria

The hosted backend cannot decrypt user file contents.

---

# Phase 10 — Save History and Restore

## Objective

Exploit immutable objects for useful local recovery.

## Deliverables

- History retention policies.
- Per-file timeline.
- CLI restore.
- GUI history.
- Durable rolling-history option.
- Garbage collection awareness.

## Success criteria

User can restore a previous save without Git.

---

# Phase 11 — Automatic Text Merge

## Objective

Turn many sync conflicts into transparent convergence.

## Deliverables

- Common ancestor tracking.
- Text-file classification.
- Three-way merge.
- Clean-merge auto-accept.
- Conflict-marker fallback.
- Merge provenance.

## Success criteria

Independent non-overlapping edits to the same text file reconcile automatically.

---

# Phase 12 — Generalized Networking

## Objective

Remove dependence on LAN/Tailscale/manual reachability.

Possible components:

- LAN discovery.
- Global discovery.
- NAT traversal.
- Relay transport.
- Connection ranking.
- Multi-source object fetching.

This phase should be pursued only after synchronization correctness is mature.

---

# Phase 13 — Selective Materialization

## Objective

Support large Spaces without local copies everywhere.

Potential features:

```text
Full
Metadata-only
On-demand
Excluded
```

Use cases:

- Large research datasets.
- Media.
- Archives.
- Desktop-only assets.

Not required to sync whole folders between machines.

---

# Phase 14 — Integrations

Possible later integrations:

## Editors and other apps

- Current sync state.
- Conflict notification.
- Restore previous save.
- Device targeting.
- “Open on device.”
- Post-sync tasks.

## Automation

Example:

```text
When files under projects/** change on the desktop:
-> run a check
-> write a ready marker
```

Keep these above the core replication engine.

---

# 52. MVP Scope Recommendation

This was the original stop line for a first useful version. That bar has been
met; do not treat Phase 4 as the current roadmap. See [`ROADMAP.md`](ROADMAP.md).

The first genuinely useful version should stop at roughly Phase 4.

It should provide:

```text
Windows + macOS
multiple local roots
include/exclude filters
two trusted devices
direct QUIC sync
offline local edits
reconnect catch-up
version-vector conflict detection
safe conflict copies
background daemon
CLI
```

That is enough to replace Git-as-transport for a real workflow.

The GUI, policy editor, and cloud persistence should come after the engine proves reliable.

---

# 53. First Real-World Dogfood Scenario

Use a shared project folder between a laptop and a desktop as the primary real-world test.

### Windows

```text
D:\Projects
```

### macOS

```text
~/Projects
```

Relay:

```text
Space: Personal
Mount: work
Selector: **
Targets: Laptop, Desktop
```

Expected loop:

```text
Edit on the laptop
-> save
-> Relay indexes new object
-> direct QUIC transfer
-> the desktop verifies the object
-> atomic materialization
-> the file is there
```

This scenario should be used continuously during development because it is concrete, latency-sensitive, and immediately exposes poor behavior.

---

# 54. Architectural Principles to Preserve

As Relay evolves, the following principles should remain stable.

## 54.1 Local files are primary for interactive work

Users should edit ordinary files using ordinary applications.

## 54.2 The backend is not the filesystem

Hosted infrastructure helps peers discover, persist, and recover state.

It is not the authoritative working directory.

## 54.3 Git remains separate

Do not conflate synchronization with source-control semantics.

## 54.4 Synchronization is state reconciliation

Do not model the system as merely forwarding filesystem events.

## 54.5 Logical identity is independent of physical path

Always use:

```text
Space + Mount + RelativePath
```

## 54.6 Replication policy is independent of file identity

One logical file may be replicated to any derived set of devices.

## 54.7 Durability is acknowledgement-based

Online status is not proof that a version is safe.

## 54.8 Preserve before resolving

When unsure, keep both versions.

## 54.9 Avoid destructive defaults

Removing a policy or target should not silently erase local data.

## 54.10 Make advanced features incremental

The architecture should support sophisticated future behavior without making the MVP sophisticated.

---

# 55. Recommended Initial Milestone Order

A practical implementation sequence:

```text
1. Cargo workspace
2. IDs + logical path types
3. SQLite schema
4. BLAKE3 object store
5. local directory scanner
6. version vectors
7. filesystem watcher
8. multi-mount + ignore rules
9. deterministic reconciler
10. two-process local simulation
11. QUIC protocol
12. object transfer
13. two-machine Mac/Windows sync
14. conflict detection
15. tombstones
16. daemon
17. CLI IPC
18. login startup
19. pairing
20. GUI
21. replication-policy UI
22. durable backend
23. encryption/recovery
24. history
25. automatic merges
```

The critical rule is to prove correctness before convenience.

---

# 56. Initial Repository Issues / Epics

A useful initial backlog could be organized into these epics.

## Epic A — Core Domain

- Define IDs.
- Define normalized logical paths.
- Implement version vectors.
- Implement entry model.
- Implement error taxonomy.

## Epic B — Local Storage

- SQLite migrations.
- Object store.
- Atomic writes.
- Reference tracking.
- Tombstones.

## Epic C — Filesystem

- Recursive scanner.
- Watcher.
- Debounce.
- Ignore rules.
- Multi-mount mapping.
- macOS integration tests.
- Windows integration tests.

## Epic D — Reconciler

- Local changes.
- Remote dominance.
- Equality.
- Concurrent versions.
- Delete propagation.
- Desired replica set.

## Epic E — Networking

- QUIC session.
- Peer hello.
- Authentication.
- Index snapshot.
- Delta update.
- Object transfer.
- ACK.

## Epic F — Daemon

- Process lifecycle.
- IPC.
- Startup.
- status/events.
- pause/resume.
- diagnostics.

## Epic G — Pairing

- Device identity.
- pairing token.
- approval.
- trust persistence.
- revoke.

## Epic H — Desktop UI

- setup.
- folder picker.
- device list.
- space editor.
- activity.
- conflicts.

## Epic I — Durable Relay

- backend abstraction.
- upload.
- download.
- ACK tracking.
- GC.
- offline handoff.

---

# 57. Definition of Success for Version 0.1

Relay 0.1 is successful when this scenario is boring:

1. Install on a MacBook.
2. Install on a Windows desktop.
3. Pair the devices.
4. Select a development directory.
5. Exclude `.git` and build artifacts.
6. Edit a source file on the Mac.
7. See it appear automatically on Windows.
8. Edit a different file on Windows.
9. See it appear automatically on the Mac.
10. Disconnect either machine.
11. Continue editing locally.
12. Reconnect.
13. Converge automatically.
14. Make a conflicting edit while disconnected.
15. Preserve both versions instead of losing data.
16. Restart both machines and retain correct state.

Version 0.1 does not need to look impressive.

It needs to be trustworthy.

---

# 58. Long-Term Product Direction

Relay is a local-first replication layer for the files you choose:

```text
Documents
Projects
Notes
Photos
Configs
Research data
Machine-specific working sets
```

The distinctive product characteristics should remain:

- Local-first.
- Multi-device.
- Selective.
- Explicitly replicated.
- Peer-to-peer when possible.
- Durable when necessary.
- End-to-end encrypted.
- Conflict-aware.
- Cross-platform.
- Understandable.

The long-term conceptual model is:

> Relay continuously maintains the user-defined relationship between logical data and the devices that should possess it.

The filesystem is only the first materialization layer.

That gives Relay room to grow without requiring the initial implementation to solve every future use case.
