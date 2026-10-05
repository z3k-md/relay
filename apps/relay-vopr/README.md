# relay-vopr

Deterministic simulation testing for Relay sync, after TigerBeetle's VOPR.

Every node is the real `relay-engine` (SQLite index, object store, scans,
apply, conflicts) and its sans-I/O `Syncer`, all in one process on one
thread. What is simulated is everything around them:

- **Time.** A virtual clock drives both the wall clock (`ManualClock`, per
  node, with optional skew) and the monotonic clock the `Syncer` uses for
  retry, resync and presence timers (`Syncer::set_now`). A 30 s resync delay
  costs no real time.
- **Network.** Frames and object transfers are packets in a priority queue
  with per-packet latency, FIFO per link (QUIC streams are ordered), link cuts
  that tear sessions down and heal later, and object fetches that fail or
  come back "not found".
- **Faults.** A node can crash (its process dies; its disk stays) and restart.
  Working-tree writes, renames and object installs can fail with an I/O
  error, or the node can die between a finished temp file and its rename
  (`relay_core::faults`, a thread-local hook that production never installs).
- **Randomness.** One `ChaCha8` generator seeded from the command line decides
  the workload, the schedule and every fault. Device identities are derived
  from the seed too (`DeviceIdentity::generate_from_seed`), so conflict
  winners are the same on every replay.

## Invariants

After every step:

- **No partial writes.** Every file in every mount holds either something the
  workload wrote or an object the engine materialized from its store.

Periodically (`quiesce_every` steps) and at the end, the simulator heals
every link, restarts every node, drains the network and the timers, then
checks:

- **Convergence.** Every replica has the same working tree and the same
  index rows (path, content, version vector).
- **Index matches disk.** Each live index row describes the bytes on disk
  and nothing on disk is unindexed.
- **Store integrity.** `verify_objects` finds nothing missing or corrupt.
- **No data loss.** For every path, the last thing each node wrote since
  the previous converged state is still present: as the file, as a conflict
  copy (nested ones included), or, for text, as its changed lines inside a
  merged result. A record is dropped only once another node is seen building
  on it. A path deleted with nothing written since must stay deleted.

## Scenarios

```
cargo run -p relay-vopr -- list
```

| name | what it exercises |
|---|---|
| `two_node_lan` | two paired devices, reliable link |
| `three_node_mesh` | full mesh, clocks skewed by up to 90 s |
| `hub_and_spokes` | spokes reach each other only through a hub |
| `chain` | four devices in a line |
| `partitions` | links cut and heal mid-edit; merges and conflict copies |
| `flaky_fetches` | transient and not-found object fetches; re-requests |
| `crashes` | crash and restart, including mid-materialize |
| `disk_errors` | I/O errors on writes, renames and object installs |
| `many_small_batches` | index batches of three entries |
| `delete_heavy` | deletes, folder deletes, renames, mass-delete holds |
| `chaos` | all of the above on four devices |

## Running

```bash
cargo run -p relay-vopr -- run --scenario partitions --seed 42
cargo run -p relay-vopr -- run --scenario partitions --seed 42 --trace   # every event
cargo run -p relay-vopr -- sweep --seeds 100 --jobs 8                   # all scenarios
cargo run -p relay-vopr -- sweep --scenario chaos --start 1000 --seeds 500
```

A failure prints the scenario, the seed, the violated invariant, the last
events, and the command that replays it.

`cargo nextest run -p relay-vopr` runs every scenario for three seeds and
checks that one seed replays to one trace. `RELAY_VOPR_SEEDS` and
`RELAY_VOPR_STEPS` widen and lengthen it.

Homes and mounts live under `/dev/shm` when it exists (every engine write is
fsynced, which otherwise dominates the run time), else the OS temp dir;
`RELAY_VOPR_TMP` overrides.

## What is not simulated

The QUIC transport, TLS, pairing, discovery and the relay server
(`relay-net`), the file watcher and the watch loop's debouncing
(`Engine::run`), the IPC server and the CLI. The simulator feeds the
`Syncer` the same `SyncInput`s the daemon would, and after each one offers
local changes to every peer the way the watch loop does. Scans are explicit
steps; the watcher's effect is modelled by marking a mount dirty after the
workload or the engine itself writes to it.
