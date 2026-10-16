# Transfer throughput

**Status:** proposal, not scheduled.

Proposal for moving photos and video quickly. Nothing here is decided until a
decision in [`DECISIONS.md`](../DECISIONS.md) adopts it.

The audience is a later performance pass. The order below is the order to
pick the work up in. Measure first, then resume, then the packet-size cap.
Chunking and selective copies are the larger format changes and can wait
until a single large file already survives a dropped link.

## How bytes move today

A live transfer is a QUIC connection between the two devices (Quinn, mutual
TLS, ALPN `relay/1`). File bytes are end to end. Nothing Relay operates sits
on that path.

| Situation | Path | What limits speed |
| --- | --- | --- |
| Same LAN | mDNS, then a private address. Dial order prefers loopback, then RFC1918, then Tailscale (D34). | Disk, packet rate, and the global datagram cap below. |
| Different networks, hole punch works | Each device writes STUN candidates to `nat/<device>` in the mailbox. Both dial the reflexive address (D32). STUN (Google, Cloudflare) only maps the public port. | The slower uplink. |
| Hole punch fails | User-run `relay transport` forwards UDP after every direct address fails (D34). QUIC stays between the two devices. | That machine's uplink, plus the extra delay. |
| Not online together | The sender seals the object into the mailbox directory. The other device reads it later. | The mailbox filesystem, and RAM (the whole file is loaded to seal and to open). |
| Through a third device | Ordinary sync. Any member that has the mount can serve the object (D26). | That machine's disk and links. |

Tailscale is just another direct address. Relay does not see whether
WireGuard punched through or hairpinned.

Pairing still needs a direct UDP path (D25). The forwarder is only for sync
after the devices already trust each other.

One file is one bidirectional stream. The requester sends `ObjectRequest`
(the object id). The responder sends `ObjectHeader` (`found`, `size`) and
then exactly `size` raw bytes. The receiver hashes while it writes, then
hashes again and fsyncs before the object is published. Up to eight objects
are fetched at once (`MAX_CONCURRENT_FETCHES` in
`crates/relay-net/src/session.rs`). A folder of photos already uses that
parallelism. One video is a single stream.

## What will hurt large files

- **No resume.** Any stream error deletes the temp file
  (`receive_object` in `session.rs`). The engine retries the whole object
  up to three times (`MAX_FETCH_ATTEMPTS` in `crates/relay-engine/src/sync.rs`).
- **Every path uses a ~1379 byte packet.** The endpoint receive buffer is
  1400 bytes so a 21-byte relay header still fits
  (`ENDPOINT_MAX_UDP_PAYLOAD`, `RELAY_PATH_MTU` in
  `crates/relay-net/src/relay.rs`). MTU discovery is capped to that on every
  connection (`transport_config` in `crates/relay-net/src/tls.rs`). Direct
  LAN traffic pays the same cap. One endpoint and one `RelaySocket` carry
  both direct and relay datagrams, so the cap is process-wide.
- **Flow control is Quinn's default.** `transport_config` sets the idle
  timeout (30s), keep-alive (10s), and the MTU cap. Congestion control stays
  Cubic and the stream window stays at Quinn's default. On a LAN the round
  trip is short enough that the window can fill the link. On a wide-area or
  relayed path the same window tops out well below a fast fiber link.
- **The mailbox holds the whole object in memory.** `put_space_object` reads
  the file, `seal_object` AEAD-encrypts that buffer, and
  `put_sealed_object` writes the ciphertext (`crates/relay-engine/src/secrets.rs`,
  `crates/relay-crypto/src/secret.rs`, `crates/relay-replica/src/fs.rs`).
  Opening the mailbox does the reverse and then `put_bytes` into the local
  store (`mailbox_object` in `sync.rs`). A long video is a RAM problem
  before it is a network problem.
- **The receiver reads the file twice.** The stream hasher runs in
  `write_and_import`. `import_verified` hashes the temp file again, then
  `sync_all`, then renames (`crates/relay-store/src/store.rs`). The fsync
  stays. The second hash matters on a slow disk and disappears against a
  home uplink.
- **An address change starts the file over.** Connection migration is out of
  scope (D34). Resume covers that failure without migration.

Whole-file objects are the right shape for source and documents
([`DESIGN.md`](../DESIGN.md) §4). Photos at a few megabytes fit that.
Video is the case that needs chunking (§5 below).

## 1. Ranged resume

Keep the temp file and ask for the rest from a byte offset. This is the
change that makes a multi-gigabyte object survivable, and it does not change
what an object id means.

Wire compatibility is the sharp edge. `ObjectHeader.size` is defined as the
number of raw bytes that follow. An old peer that ignores a new `offset`
field would send the file from byte 0 while a new receiver appended or
sought. Prost's unknown-field behavior is safe for the request only if the
response proves what was sent.

- Add a feature bit on `Hello`. Missing fields decode as zero, so an old
  peer advertises nothing and a new peer sets the bit. `PROTOCOL_VERSION`
  stays 1: a version mismatch closes the session, which is a bigger break
  than this needs.
- `ObjectRequest` gains `offset`. `ObjectHeader` echoes `offset`. The bytes
  that follow are `size - offset`. If the echoed offset is not the one we
  asked for, discard the prefix and treat the body as a full copy.
- Send a ranged request only when the peer's Hello has the bit.

Local state:

- Name the partial `tmp/partial-<object hex>` instead of a random temp that
  is deleted on the first error.
- After each durable prefix, record the committed length (a sidecar, or a
  header next to the partial). On restart, truncate to that length and
  rehash the prefix to rebuild the BLAKE3 hasher. Persisting hasher state
  is unnecessary.
- `clean_tmp` deletes aged files under the temp dir. Partials have to live
  outside that sweep, or the sweep has to skip names it does not own.
- Publish only through the existing path: full hash matches, fsync, rename.
  A partial is never materialized.

`spawn_fetch` already no-ops when the object is in the store. A resume
record is only for an object that is still missing.

## 2. Larger packets on direct paths

1379 bytes is a few percent under a normal 1500-byte Ethernet payload, and
it forbids jumbo frames. At a gigabit the cost shows up as packet rate.

Quinn's `max_udp_payload_size` is per endpoint, and this process has one
endpoint. Raising that ceiling is what lets a direct peer send a full-sized
datagram. The relay path still has to send packets small enough for
`MAX_RELAY_PAYLOAD`, because `RelaySocket` wraps those datagrams before they
hit the wire.

Both sides have to clamp, not only the dialer. The acceptor is the same
socket. A direct connection that discovers a 1500-byte path will send that
size; if that connection is actually hairpinning through the forwarder, the
forwarder drops it.

Practical split to implement when this is picked up:

- Raise the endpoint ceiling to a normal UDP payload (the standard Ethernet
  size; jumbo only if both interfaces are jumbo, which discovery can find).
- Keep `RELAY_PATH_MTU` as the send cap for a connection whose packets are
  going through the forwarder.
- Direct connections use MTU discovery up to the endpoint ceiling.

Confirm at that time whether Quinn will take a different `TransportConfig`
for relay dials versus direct dials, and how the accepting side learns the
path before it sends its first full-sized packet. If per-connection send
caps are awkward, a single ceiling at a normal internet MTU (~1450) is still
a small, safe step up from 1379 and does not require jumbo frames.

## 3. Windows and congestion control

Change these only after a baseline. The bench is one large object and a
directory of small ones, on loopback and on a path with artificial delay
(the relay hop, or a local delay). Record bytes per second and time to
first complete object. `crates/relay-engine/tests/scan_bench.rs` is the
existing pattern for a timed bench; a sim scenario is the other place.

`transport_config` is the only knob. Quinn 0.11 can swap Cubic for
`quinn::congestion::BbrConfig` there, and it can raise
`stream_receive_window` and `receive_window`.

Expect the window to matter when delay and bandwidth are both high: a fast
fiber path, or the extra round trip of the UDP forwarder. A typical home
uplink is slower than the default window. A LAN gigabit link is more likely
limited by packet rate (§2) and by the fsync at the end of each object.

Eight concurrent object streams stay as they are. They already spread a
folder of photos across the link. Raising that number is a later experiment,
not part of the first pass.

## 4. Stream the mailbox

Required before the offline path is a reasonable place for video. The live
QUIC path already streams in 64 KiB reads (`CHUNK` in `session.rs`); the
mailbox does not.

The sealed layout is `OBJECT_MAGIC || generation || nonce || ciphertext||tag`
over the entire plaintext (`seal_object`). Keep that magic working for
objects already in a mailbox. A new magic identifies a layout that encrypts
in bounded pieces (fixed-size frames, each with its own nonce, or the
crate's streaming AEAD if it matches this header). Readers branch on the
magic.

The IO boundary is `put_sealed_object` / `get_sealed_object`, which take and
return a `Vec<u8>`, and `mailbox_object`, which then `put_bytes` the
plaintext. Those become file-to-file copies into the same temp-and-rename
path the live receiver uses. Peak RAM should stay on the order of one frame,
not the file size.

Resume against the mailbox can reuse the partial from §1: a sealed frame
sequence is just another byte source that honors an offset.

## 5. Chunks above a threshold

The approach: FastCDC, BLAKE3 chunk hashes, manifest objects, threshold
somewhere in 8–32 MB.
Files under the threshold stay one object so a source tree does not turn
into a pile of chunks.

Do this after resume. Resume fixes dropped transfers without a new identity
for the file. Chunks add dedup of identical regions, parallel fetch, and
retry of one piece.

Identity, decided here so it does not get re-litigated casually:

- The entry's object id stays `BLAKE3` of the whole file. History, conflicts,
  and "these two paths are the same bytes" keep their current meaning.
- Above the threshold the store keeps chunks plus a manifest, and can stream
  them back out as the whole file. The manifest records chunk ids, lengths,
  and the FastCDC parameters. Those parameters are fixed in code; a manifest
  with different parameters is rejected, otherwise two devices will not dedup.
- On the wire, a peer without the feature bit still asks for the object id
  and receives the concatenated bytes (the sender streams chunks in order;
  it does not assemble a second full copy). A peer with the bit can ask for
  chunk ids and fetch several at once, under the same eight-stream cap.
- Materialization concatenates chunks into the work tree. The publish check
  is still the whole-file hash.

The manifest can itself be a content-addressed object. It is an
implementation of the file's object id, not a replacement for it.

## 6. Multi-source while a transfer is running

Today another connected peer, then the mailbox, is tried only after a fetch
fails (`on_fetch_failed` in `sync.rs`). That is the right policy for a
single range: one writer owns the partial, and a failure hands the same
offset to the next source.

Once chunks exist, the scheduler can assign different chunk ids to different
connected peers, and to the mailbox, while the transfer is still healthy.
The eight-stream semaphore still bounds it. Striping one byte range across
two peers is not worth a second writer on the same partial.

## 7. Selective copies

Selective materialization (D35) is the product control for a media
library: full, metadata-only, on-demand, and excluded copies. Throughput
work does not replace that. A fast pipe that still copies every video onto
every device is the wrong default for a library.

Keep one object id per file version (§5) so an on-demand fetch is the same
request the sync engine already sends.

## Smaller cuts

- The second full-file hash in `import_verified` can be skipped when the
  caller has already hashed those bytes, as the live receiver does. Keep
  the fsync and the rename. Profile before doing this; it is noise next to
  a slow uplink.
- Read and write sizes above 64 KiB are available if a profile says syscall
  overhead shows up on a LAN. Quinn buffers underneath, so this is unlikely
  to be first.

## Out of scope for this pass

- Compressing JPEG, HEIC, or video. Those bytes are already compressed.
- A second transport next to QUIC.
- A Relay-operated relay or a public TURN account. D34 already leaves that
  out. The user-run forwarder stays a dumb UDP path.
- Connection migration as a prerequisite. A new QUIC session that resumes
  from an offset handles an address change. Migration can be added later
  on the same endpoint if handoff without a reconnect becomes worth it.
