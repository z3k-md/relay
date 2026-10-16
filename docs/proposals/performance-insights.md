# Performance insights

**Status:** Stage 1 (connection test, D48) in review. Stages 2 to 4 not
scheduled.

What the app should show about speed, and where each number comes from.
The aim is to answer "why is this slow?" without a terminal: the link, the
disk, or Relay.

## 1. Connection test (D48)

A button on each connected peer in Peers, and `relay peer test NAME`. Five
seconds down, five up, over the live QUIC session; no disk on either side.
Shows Mbit/s each way, a 250 ms throughput graph, RTT, loss, packet size,
and the path (LAN, Tailscale, internet, relayed).

First reading, two daemons on one Linux container over loopback: about
930 Mbit/s each way with 1379-byte packets and 1.8% loss. That ceiling is
the packet rate the cap in
[`transfer-throughput.md`](transfer-throughput.md) §2 predicts, so the
test is also the measuring stick for that work.

## 2. Live graphs

Throughput and RTT per peer over the last ten minutes, on Peers and
Overview.

- **Source.** The net thread samples `Connection::stats()` and `rtt()` for
  each session once a second (bytes sent and received, lost packets,
  congestion window) and keeps a ring buffer per peer in memory. The engine
  already computes `TransferLive.bytes_per_sec`; the graph adds what moves
  between transfers (index batches, pings) and the link's own health.
- **Read path.** A new IPC method returns the buffers. Cost is O(peers ×
  600 samples), never a store walk ([#20](https://github.com/z3k-md/relay/pull/20)).
- **Nothing persisted.** A restart starts the graph over.

## 3. `relay bench`

Local numbers to put beside the connection test:

- BLAKE3 hash rate over a temp file (the scan and receive cost).
- Store import: hash, fsync, rename, per file size bucket (the receive
  path in `relay-store`).
- Scan rate on a chosen mount (files per second, warm and cold).

Shown in Settings as "This computer". When download in the connection
test is well above store import, the disk is the bottleneck.

## 4. History

- Daily bytes in and out per peer, and p50 / p95 sync latency: from a local
  change being indexed here to the peer's `Ack` for that sequence (D4
  already records both ends).
- Kept in SQLite, one row per peer per day, 90 days. A new migration.
- 7 and 30 day charts on Overview.

## Order

1, then 2 (cheap, mostly in `relay-net`), then 3, then 4 (needs a schema
change and a decision on what latency means for a peer that is offline).
