# NanoTorrent patches on librqbit

Currently vendored: **librqbit 9.0.1** and **librqbit-tracker-comms 9.0.1**.

These are unmodified copies of the **published** crates from crates.io (so all
their dependencies still resolve from crates.io) plus the patches in
`../../patches/`, wired up via `[patch.crates-io]` in `../../Cargo.toml`.

Most of them are visibility-only: they expose read access to data the engine
already maintains, or add an option that defaults to `None`/`false` so the
engine behaves exactly as upstream when NanoTorrent doesn't ask for anything.
The three that do change behaviour (0006, 0007, and the seams once a transform
is installed) are called out below.

`build.rs` checks at compile time that every patch is present and fails with
instructions if a re-vendor dropped one.

## Applying

The patches are **ordered**: each is a diff from the state the previous one
left behind, so they must be applied 0001 → 0007. Patches named `*-comms.patch`
apply to `vendor/librqbit-tracker-comms`; everything else applies to
`vendor/librqbit`. `tools/update-librqbit.ps1` handles both.

| # | Patch | Files | Kind |
| --- | --- | --- | --- |
| 0001 | engine visibility | 2 | visibility only |
| 0002 | stream transform seams | 5 | opt-in seam |
| 0003 | per-torrent toggles | 3 | opt-in flags |
| 0004 | proxy scope | 1 | opt-in flags |
| 0005 | tracker stats (+ `-comms`) | 2 + 1 | additive |
| 0006 | session persistence | 1 | **bug fix** |
| 0007 | flush bitfield on pause | 1 | **bug fix** |
| 0008 | BitTorrent v2 (+ `-peerproto`) | 6 + 1 | opt-in seams |
| 0009 | hybrid dual-swarm announce | 2 | **behaviour** |
| 0010 | fast extension (+ `-peerproto`) | 2 + 1 | **behaviour** |
| 0011 | synthetic peer | 1 | opt-in seam |
| 0012 | upload-only (BEP 21) | 1 | **behaviour** |
| 0013 | Windows UDP resets (`-sockets`) | 1 | **bug fix** |
| 0014 | quiet an upstream warning (`-sockets`) | 1 | cosmetic |

## 0001 - engine visibility

Read access to state the engine already maintains. No behaviour change.

- `ManagedTorrent::with_chunk_tracker` from `pub(crate)` to `pub`, for the
  piece progress bar (chunk tracker → `get_have_pieces()`).
- `TorrentStateLive::per_peer_have_pieces() -> Vec<(SocketAddr, u64)>`, an
  additive method reading each live peer's `bitfield.count_ones()`, for real
  seed counts and the availability column.

## 0002 - the stream transform seams

The injection point that lets feature code (protocol encryption) live in the
NanoTorrent crate instead of in patches. Adds, in `stream_connect.rs`:

- `pub trait StreamTransform` + `SessionOptions::stream_transform` - every
  *outgoing* peer stream is passed through the transform (addr + info_hash +
  boxed read/write halves) right after connect, before the BitTorrent
  handshake. Consumed by `bittorrent::mse::MseTransform`.
- `pub trait IncomingStreamTransform` + `SessionOptions::incoming_transform` -
  the same for every *accepted* stream, in `Session::check_incoming_connection`,
  before the handshake is read. Consumed by
  `bittorrent::mse::IncomingMseTransform`. The transform gets *all* active
  info-hashes because the incoming info-hash is not known until the (possibly
  encrypted) handshake has been read - the MSE responder resolves the peer's
  SKEY against them.

Behaviour is unchanged when both options are `None` (the default).

Two mechanical details:

- librqbit 9 reads peer sockets through its own `AsyncReadVectored` trait
  (`BoxAsyncReadVectored`). The transforms deal in a plain
  `BoxAsyncRead`/`BoxAsyncWrite` pair instead, because a cipher layer has to
  see one contiguous byte stream anyway; the connector wraps the result back up
  with the crate's own `into_vectored_compat()`. `type_aliases::BoxAsyncWrite`
  is made `pub` for the same reason.
- `StreamConnector::connect` keeps its upstream body under the name
  `connect_raw`; the public `connect` is a thin wrapper that takes the
  info-hash and applies the transform.

## 0003 - per-torrent toggles (PeX, anonymous mode)

Two flags, one patch: both are a `bool` on `SessionOptions` threaded through to
`ManagedTorrentOptions` and read in `torrent_state/live/mod.rs`, they touch the
same three files at the same anchors, and upstream moving any of those breaks
them together anyway.

- `disable_pex` gates both PeX directions: stops spawning
  `task_send_pex_to_peer` and ignores incoming `UtPex` messages. Wired to the
  `libtorrent.enable_pex` preference (inverted).
- `anonymize` makes `PeerHandler::update_my_extended_handshake` clear the
  client version (`handshake.v = None`) so peers can't fingerprint the client
  by it. The other half of anonymity - a random peer id with no `-NT-`
  fingerprint - is done app-side in `build_session_options`. Wired to the
  `libtorrent.anonymous_mode` preference. (UDP tracker announces already send
  no client IP, so there is nothing to suppress there.)

Both default to `false` = upstream behaviour.

## 0004 - proxy scope

Adds `SessionOptions::proxy_peers` / `proxy_trackers` / `proxy_hostnames` (all
default `false`) so a configured SOCKS proxy can be applied selectively,
matching PicoTorrent's (and libtorrent's) `proxy_peer_connections` /
`proxy_tracker_connections` / `proxy_hostnames`. Upstream applies a set
`ConnectionOptions::proxy_url` to *both* the peer connector and the reqwest
HTTP-tracker client unconditionally; now the connector proxy is gated on
`proxy_peers`, the reqwest proxy on `proxy_trackers`, and `proxy_hostnames`
upgrades the reqwest proxy to `socks5h` (proxy-side DNS). UDP tracker announces
are never proxied (a librqbit limitation). Wired to the `libtorrent.proxy_*`
preferences.

## 0005 - per-tracker announce stats (Trackers tab)

Surfaces the seeders/leechers/interval each tracker returns (the upstream
crates receive them, then throw them away) so the UI can show a
PicoTorrent-style Trackers tab. This one spans both crates, hence the two patch
files under the same number.

`0005-tracker-stats-comms.patch` (**librqbit-tracker-comms**) adds
`TrackerStat` + `SharedTrackerStats` (a shared `HashMap<url, stat>`), a
`tracker_stats` field on `TrackerComms`, a `tracker_stats` parameter on
`TrackerComms::start`, and records stats in the HTTP and UDP announce paths: on
success `status="Working"`, seeders/leechers and `next_announce = now +
interval`; on error `fails += 1` and the error text. `tracker_one_request_udp`
gains the announce `Url` as a parameter - purely as the stats key, since one URL
can resolve to both a v4 and a v6 address and is then announced to twice, but
the UI shows one row per URL.

> `status` stays **empty** until the first announce settles. Don't be tempted to
> seed it with `"Updating..."`: `tracker_rows` turns a missing/empty status into
> its own `tracker_updating` string, so an English placeholder here would show
> untranslated in all 41 languages. `"Working"` is the one literal the UI
> matches on and translates; anything else passes through as an error message.

`0005-tracker-stats.patch` (**librqbit**) adds a
`tracker_stats: Mutex<HashMap<Id20, SharedTrackerStats>>` field on `Session`,
registered per torrent in `make_peer_rx`, plus the public
`Session::tracker_stats_snapshot(info_hash)` accessor and a
`pub use tracker_comms::TrackerStat` re-export in `lib.rs`. Also a second
registry `tracker_tiers: Mutex<HashMap<Id20, Vec<Vec<Url>>>>`, populated at the
`.torrent` add path from `torrent.meta.announce_list` (librqbit otherwise
flattens tiers into an unordered list), exposed via
`Session::tracker_tiers_snapshot(info_hash)` so the UI can group by tier.

> IMPORTANT: the stats-map registration in `make_peer_rx` must REUSE a stable
> per-info_hash `Arc` (`entry().or_default()`), never re-`insert` a fresh one -
> on startup a torrent is re-announced more than once, and a fresh insert
> orphans the map the live announcer writes to (UI stuck on "Updating").

## 0006 - session persistence (behavioural: two real bug fixes)

**fsync the session index before renaming it.**
`JsonSessionPersistenceStore::flush` writes `session.json.tmp` and renames it
over `session.json` without ever flushing it. The rename is atomic for the
directory entry, but the file's data blocks are not guaranteed to have reached
disk, so a crash or power loss in between leaves a valid name pointing at zero
bytes.

That is fatal on the next start: `new` treats a *missing* index as an empty
session but an *unreadable* one as an error, so the whole app fails with
`error deserializing session database: EOF while parsing a value at line 1
column 0` and cannot be started again. Adds `tmp.sync_all()` between the write
and the rename. `src/bittorrent/session.rs::quarantine_unreadable_session_index`
is the other half - it moves an already-corrupt index aside so existing broken
profiles can start. Both are needed: this patch prevents it, that function
recovers from it.

**`next_id` must reserve an id, not report one.** Upstream's `next_id` takes a
*read* lock and returns `max(existing ids) + 1`. That reserves nothing: N
concurrent `add_torrent` calls all read the same state and all receive the same
id. `Session::add_torrent` then treats a colliding id as an existing torrent:

```rust
if t.info_hash() == info_hash || *eid == id {
    return Ok(AddTorrentResponse::AlreadyManaged(id, handle));
}
```

So selecting five different torrents in the Add dialog added the first and
reported the other four as `AlreadyManaged` - naming the first one, because that
is the handle the id matched. One torrent appeared and four vanished without an
error. The non-persistent path never had this: it uses `next_id.fetch_add`. The
store now keeps its own `AtomicUsize`, seeded from the loaded database, so the
persistent path behaves the same. Ids are monotonic and never reused, which also
stops a deleted id from colliding with a later add.

Covered by `session::tests::concurrent_adds_all_land`, which fails with "only 1
of 5 concurrent adds landed" when this is reverted. Note that the test must
enable persistence - with `persistence: None` it passes either way, which is
exactly why this went unnoticed.

## 0007 - flush the have-bitfield when pausing (behavioural)

The per-torrent `.bitv` file - the record of which pieces have been downloaded
AND hash-checked - is an mmap. It is written back in exactly two places: every
`FLUSH_BITV_EVERY_BYTES` (16 MB) of completed pieces, and once when the torrent
finishes.

`TorrentStateLive::pause` moves the piece tracker into `TorrentStatePaused`
without flushing, and pause is also the shutdown path. So up to 16 MB of
verified pieces can exist only as dirty pages. A clean exit survives on the
kernel writing the mapping back; a crash, a power cut or a force-kill does not.

The piece DATA is already on disk - only the record of having verified it is
lost. The torrent therefore comes back a few pieces short of complete, with a
file that is fully allocated and plays almost to the end. If the swarm still has
those pieces it silently re-downloads them; if it does not, the torrent never
finishes and a Force recheck is the only way out.

`pause()` now calls `try_flush_bitv()` while the tracker is still owned by the
live state. Not in `Drop`: a failed msync should be logged, not swallowed.

## 0008 - BitTorrent v2 (BEP 52)

Four parts that are useless apart, so they share a number. NanoTorrent's half
of all of it lives in `src/bittorrent/v2.rs`.

### The piece verification seam

The seam BitTorrent v2 hangs off. Adds `src/piece_verify.rs` with:

```rust
pub trait PieceVerifier { fn hasher(&self, piece_index: u32) -> Option<Box<dyn PieceHasher>>; }
pub trait PieceHasher   { fn update(&mut self, buf: &[u8]); fn verify(self: Box<Self>) -> bool; }
```

plus `AddTorrentOptions::piece_verifier` (threaded to `ManagedTorrentOptions`)
and the wiring in `file_ops.rs`.

Why the whole accumulate-then-compare step is the seam, rather than just the
hash function: a v1 piece hash is SHA-1 of the piece's bytes, looked up by index
in the `pieces` blob. A v2 piece hash is the root of a SHA-256 merkle subtree
over the piece's 16 KiB blocks - not a hash of the piece, and not in `pieces`
either. Parameterising the digest would not have been enough.

`update_hash_from_file` now takes a `&mut dyn FnMut(&[u8])` sink instead of a
concrete hasher, so one read loop feeds either path, and an internal `PieceHash`
enum picks between the engine's `Sha1` + `compare_hash` and a plugged-in
verifier. With no verifier installed the behaviour is byte-for-byte what it was.

### Identity overrides

Two `AddTorrentOptions` fields, both `None` by default:
`override_info_hash` and `override_info_bytes`.

A v2-only torrent is handed to the engine in a **v1-shaped** form it can
actually drive (`bittorrent::v2::synthetic_v1`: the same files, BEP 47 padding
files to keep every file starting on a piece boundary, and a filler `pieces`
blob that only has to be the right length because 0008 does the real checking).
That synthetic dict must never become the torrent's identity:

- BEP 52 says a v2-only torrent is known on the wire, to trackers and in the
  DHT by its **SHA-256 info hash truncated to 20 bytes**. `override_info_hash`
  restores that, so the swarm we join is the right one.
- BEP 9 metadata exchange must serve the **genuine** info dict, since peers
  verify what they receive against that hash. `override_info_bytes` restores
  that.

Together they are what keeps the synthetic model strictly local. Neither
changes anything for a v1 or hybrid torrent.

> This is the pragmatic half of v2 support. The thorough version is v2 metainfo
> parsing inside `librqbit-core` (`meta version`, `file tree`, `piece layers`,
> optional `pieces`, a v2-aware `Lengths`), which means vendoring a third crate
> and touching the types every other crate is built on. These two fields buy
> the same result for v2 **.torrent files** at a fraction of the maintenance
> cost, which is the right trade while this is a patch rather than a fork. What
> it does not buy is v2 **magnets** - see the README.

### The hash messages (`0008-bittorrent-v2-peerproto.patch`)

Applies to **`vendor/librqbit-peer-protocol`**, the third vendored crate. Adds
peer messages 21 (`hash request`), 22 (`hashes`) and 23 (`hash reject`), and
the v2 handshake bit.

BEP 52 names the fields of these messages but never states their widths or
byte order. The layout implemented here is libtorrent's, from
`src/bt_peer_connection.cpp` - which is what is actually spoken on the network:

```
hash request (21) / hash reject (23)     53 bytes
  <u32 len=49><u8 id><pieces_root: 32><base: u32><index: u32>
  <length: u32><proof_layers: u32>
hashes (22)
  the same 48-byte header, then N hashes of 32 bytes
```

All integers big-endian. The handshake bit is `reserved[7] & 0x10`, exposed as
`Handshake::supports_v2()` / `set_supports_v2()`.

The patch carries its own tests (`mod bep52_tests`), which assert the byte
offsets literally rather than only round-tripping - a round-trip test agrees
with a wrong format quite happily. Run them with:

    cargo test --manifest-path vendor/librqbit-peer-protocol/Cargo.toml bep52

One free side effect: before this, a peer sending message 21/22/23 produced
`UnsupportedMessageId` and the connection was dropped. Now the message parses
and librqbit's catch-all ignores it, so v2-capable peers stay connected.

> **This patch is the wire format only.** It is not wired into a conversation
> yet - see below.

### Magnet metadata

Three things stopped a v2-only magnet dead in `peer_info_reader`:

1. The assembled info dict is checked with **SHA-1** against the torrent's info
   hash. A v2-only torrent's hash is a truncated SHA-256, so a perfectly good
   info dict was thrown away as corrupt.
2. The dict is then parsed as `TorrentMetaV1Info`, which a v2 dict is not.
3. Even with both fixed there would be no piece hashes: `piece layers` is NOT
   part of the info dict, and has to be fetched from the peer with the BEP 52
   hash messages (the `-peerproto` half of this patch).

All three are decided by knowledge the engine does not have, so this adds one
seam - `MetadataInterceptor` - that owns all of it while the engine keeps
driving the conversation. It is asked, in order: does this info dict match the
hash; what else should I request from this peer; here is an answer, are we
done; and finally, what should I parse. `None` (the default) is exactly
upstream behaviour.

The bytes RETURNED are always the originals - they are what peers verify
against the info hash over BEP 9, and what gets persisted. Only the model the
engine drives is substituted.

NanoTorrent's implementation is `bittorrent::v2::V2Magnet`, which is installed
as the interceptor AND as the torrent's `PieceVerifier`, so the piece layers it
collected are exactly what pieces are later checked against. One object plays
both parts precisely so the two cannot disagree.

## 0009 - hybrid dual-swarm announce

A hybrid torrent has two identities - the v1 SHA-1 info hash and the v2
SHA-256 one truncated to 20 bytes - and they are two separate swarms, on the
same trackers and the same DHT. librqbit models a torrent as having exactly one
`info_hash`, used for the DHT lookup, the tracker announce AND for matching
incoming connections, so a hybrid was only ever present in its v1 swarm: half
the peers holding the identical data were invisible, and the ones we did find
were only those still on v1.

Adds `ManagedTorrentShared::secondary_info_hash` (from
`AddTorrentOptions::secondary_info_hash`, `None` for v1 and v2-only torrents),
makes the peer stream the merge of a lookup under each hash, and lets an
incoming handshake match either one.

Outgoing connections keep using the primary (v1) hash. Anyone in the hybrid's
v2 swarm necessarily holds the hybrid info dict - a v2-only torrent of the same
files has a different info dict and therefore a different hash - so they know
the v1 hash too and accept a v1 handshake. That avoids having to tag every
discovered peer with the swarm it came from, which is where this would
otherwise become a far larger change.

> One wrinkle worth knowing: the Trackers tab reads announce stats keyed by the
> primary hash (patch 0005), so the seeder/leecher numbers it shows are the v1
> swarm's. The second announce happens and finds peers; it just is not counted
> in that column.

## 0010 - BEP 6 fast extension

`0010-fast-extension-peerproto.patch` adds the five messages and the
handshake bit; `0010-fast-extension.patch` makes them mean something.

```
suggest piece  (0x0D)  <len=5><id><piece: u32>
have all       (0x0E)  <len=1><id>
have none      (0x0F)  <len=1><id>
reject request (0x10)  <len=13><id><index: u32><begin: u32><length: u32>
allowed fast   (0x11)  <len=5><id><piece: u32>
```

Handshake bit `reserved[7] & 0x04`. This is deliberately all-or-nothing:
fast messages are only ever exchanged when BOTH peers set the bit, so parsing
them without advertising would be dead code, and advertising without the
semantics would leave peers waiting.

Advertising obliges three things, all met:

- the first message must be bitfield / have all / have none. We always send a
  bitfield, which is compliant;
- a request we will not serve must be answered with `reject request` rather
  than ignored. Those paths used to drop the connection, which told the peer
  even less;
- outstanding requests must be rejected when we choke. **librqbit never chokes
  anyone** - `Message::Choke` appears only as an incoming handler - so there is
  nothing to do.

Incoming, `have all` / `have none` set the peer's bitfield in one go, and
`reject request` releases that one piece back to the queue instead of waiting
out a timeout (`PieceTracker::release_piece_owned_by`, the single-piece
counterpart of the existing `release_pieces_owned_by`). `suggest piece` and
`allowed fast` are hints; the BEP permits ignoring them, and librqbit picks
pieces by its own strategy.

## 0011 - synthetic peer

One method, `Session::add_synthetic_peer(addr, reader, writer)`: a peer whose
transport is not a socket. It goes through exactly the same
`check_incoming_connection` path a real peer does - the handshake is read and
matched to a torrent as usual - so a synthetic peer cannot skip a check a real
one has to pass.

This is what BEP 19 (WebSeed) hangs off. A web seed is an HTTP server, not a
peer, but everything the engine does *around* a peer is what a web seed needs:
piece picking, chunk tracking, hash verification, rate limiting, stats.
Building a parallel download path would have meant a second implementation of
piece verification, which is the last thing worth having two of. So
`bittorrent::webseed` speaks the peer protocol over a `tokio::io::duplex` and
answers each `Request` from an HTTP range GET; a piece that fails its hash is
discarded and re-fetched exactly as it would be from a bad peer.

The synthetic peer sits on a **loopback** address, which is not an aesthetic
choice: PeX already refuses to pass private addresses on to remote peers
(`torrent_state/live/mod.rs`), so it cannot leak into the swarm.

## 0012 - upload-only (BEP 21)

`upload_only` already existed on the BEP 10 extended handshake struct; it was
simply never set and never read. Setting it tells a peer not to expect us to
download, and reading it lets two seeds stop wasting a connection on each
other.

librqbit already disconnects peers that turn out to hold the whole torrent, but
only once their bitfield has arrived and only when WE finish. This reaches the
same conclusion a round trip earlier, without needing a bitfield at all.

> The extended handshake is sent once, at connect time, so this says "I was
> already complete when we met". BEP 21 permits re-sending it on completion;
> that needs a trigger the engine does not have, and the existing
> bitfield-based disconnect already covers finishing mid-connection.

## 0013 - Windows UDP resets (`0013-windows-udp-connreset-sockets.patch`)

Applies to **`vendor/librqbit-dualstack-sockets`**, a fourth vendored crate -
and note it is versioned separately from the librqbit family (0.7.0, not
9.0.1), so `update-librqbit.ps1 -Version` does not cover it.

On Windows a UDP socket that provokes an ICMP error fails the **next**
`recv_from` with a connection error, on a connectionless socket. Unix does not
do this, so code written on Unix treats the error as fatal and stops. The DHT
died 24 ms after startup:

```
INFO  librqbit_dht::dht: starting up DHT with peer id ...
ERROR librqbit_core::spawn_utils: dht finished with error:
      framer failed: Recv(Os { code: 10054, kind: ConnectionReset })
```

One dead bootstrap node is enough, and there is always a dead bootstrap node.
With no DHT, a magnet carrying no trackers can never find a peer - which is how
this was found: a real BitTorrent v2 magnet resolved nothing for four minutes
and never saw a single peer.

Two ioctls are needed, not one. Disabling `SIO_UDP_CONNRESET` alone just moved
the error to `WSAENETRESET` (10052) from a different ICMP message, so
`SIO_UDP_NETRESET` is disabled too. Both are set in `bind_udp`, so this fixes
the DHT, uTP and LSD together.

Measured afterwards: the routing table reaches ~1200 nodes and drives requests
to completion, where before it never grew past what was loaded from cache.

## 0014 - quiet an upstream warning (`0014-quiet-upstream-warning-sockets.patch`)

Applies to **`vendor/librqbit-dualstack-sockets`**. One character.

`BindDevice::new_from_name` has a `#[cfg(windows)]` arm that returns
`BindDeviceNotSupported` without looking at its argument, so every Windows
build of the crate emits `unused variable: name`. Renamed to `_name`.

Purely cosmetic, and upstream's rather than ours - but a warning nobody can act
on is a warning everybody learns to scroll past, and the build is otherwise
clean. Carried as a patch rather than an edit so `tools/update-librqbit.ps1`
does not drop it on the next version bump.


## What is still missing for v2

Downloading is complete, for `.torrent` files and magnets alike. What is left
is all on the **seeding** side:

- Nothing answers an incoming `hash request`. We parse and ignore it, where a
  `hash reject` would at least be polite - and serving real hashes would let us
  bootstrap someone else's v2 magnet instead of only consuming other people's.
  This is the natural next step: the layers are already in hand for any torrent
  added from a `.torrent`, so it is mostly a matter of answering.
- The v2 handshake bit is never set on outgoing connections, because
  advertising support we cannot honour would be worse than staying quiet. It
  should go on at the same time as the above, not before.

Also worth knowing: none of the v2 work has been tested against a real v2
swarm, only against torrents NanoTorrent builds itself. The wire format matches
libtorrent's byte for byte and the merkle arithmetic is checked both
directions, but no byte has crossed a network.

## Retired patches

Kept here so nobody re-derives them. These numbers do not appear in
`../../patches/` any more; the surviving patches were renumbered 0001-0007 at
the librqbit 8 → 9 bump.

| Was | What it did | Why it's gone |
| --- | --- | --- |
| 0002 | per-peer have-pieces | folded into 0001 - one hunk, same purpose |
| 0005 | incoming stream transform seam | folded into 0002 - most of what it did by hand (carrying boxed read/write halves through `CheckedIncomingConnection` and `manage_peer_incoming` instead of a raw `TcpStream`) is how librqbit 9 already works, since uTP streams are not `TcpStream` either |
| 0007 | anonymous mode | folded into 0003 - identical shape to the PeX toggle, same three files, same anchors |
| 0009 | `#![allow(mismatched_lifetime_syntaxes)]` in `lib.rs` | librqbit 9.0.1 builds warning-free on current rustc, which is exactly the condition the patch named for its own deletion |
| 0010 | Windows short-read/short-write fix in `FilesystemStorage` | **fixed upstream.** `pread_exact` now loops and turns `Ok(0)` into `UnexpectedEof` instead of discarding the byte count, and `pwrite_all` advances buffer and offset instead of rewriting `buf` at the same offset every pass. The code moved to `storage/filesystem/opened_file.rs` (`OurFileExt`) |
| 0011 | our own name in the BEP 10 extended handshake | **available upstream.** librqbit 9 has `SessionOptions::client_name_and_version`, so the `CLIENT_NAME` static is gone and `build_session_options` sets the option instead |

A second consolidation followed, once the v2 and BEP 6 work was in: what had
grown to fifteen numbers became eleven, on the rule **one number = one
feature**. The v2 seams (piece verification, identity overrides, hash messages,
magnet metadata) were four patches that are useless apart, and the fast
extension was a wire format and its semantics that must ship together or not at
all. A feature spanning crates now writes a second file under the SAME number
with a `-comms` or `-peerproto` suffix, the way 0005 always did.

> The suffix is `-peerproto`, not `-peer`, because `-peer` collided with a
> librqbit feature legitimately called `synthetic-peer` and routed it to the
> wrong crate. It failed loudly rather than silently, but only by luck.

## New in librqbit 9 and deliberately not turned on

librqbit 9 gained uTP (`librqbit-utp`) and local service discovery
(`librqbit-lsd`). `build_session_options` pins `ListenerMode::TcpOnly` and
`disable_local_service_discovery: true`, so NanoTorrent puts exactly what it
put on the network before. Both would need a Preferences switch, a README/BEP
table update and a privacy-policy line before being enabled - turning them on
silently would change what the app broadcasts. LSD in particular defaults to
`true` in the settings database, so wiring its (currently disabled) checkbox up
would switch it on for every existing profile at once.

## BitTorrent v2 (BEP 52) in librqbit 9.0.1

Measured against the 9.0.1 sources, not assumed. **Upstream has nothing usable
yet** - it added the types it will need and stopped. Patches 0008 and 0009 are
NanoTorrent's own v2 support, built on top of that gap; this section is what is
NOT there, so nobody goes looking for it.

Present:

- `librqbit_core::hash_id::Id32`, a 32-byte hash with `truncate_for_dht()`.
  Used by exactly one thing: `Magnet` parsing of `urn:btmh:`, exposed as
  `Magnet::as_id32()`.
- `librqbit_sha1_wrapper::ISha256`, with both backends (crypto-hash and
  aws-lc-rs) implemented. **Nothing in the stack calls it.**
- 17 BEP 52 error variants in `librqbit_core::Error` (`V2MissingFileTree`,
  `V2PieceLayersRootMismatch`, `V2InvalidPieceLength`, …). **Every one is
  declared and never constructed.**

Absent:

- `file_tree`, `meta_version`, `piece_layers` and `pieces_root` do not appear in
  `torrent_metainfo.rs` at all - the metainfo parser has no v2 fields.
- No merkle tree code anywhere in any of the crates.
- No BEP 52 wire messages. `librqbit-peer-protocol` implements message ids 0-8
  and 20; BEP 52 needs 21 (hash request), 22 (hashes) and 23 (hash reject), plus
  its own handshake reserved bit - the only reserved bit checked today is
  BEP 10's.
- No v2 identity plumbing: `Session`, `ManagedTorrent`, `TorrentMetadata`, the
  persistence store, the DHT and the trackers are all `Id20`.

What NanoTorrent does about it, all of it in `src/bittorrent/v2.rs`:

- reads the v2 metainfo (`file tree`, `piece layers`, per-file `pieces root`),
- checks every `piece layers` entry against its file's `pieces root` - the
  layer travels OUTSIDE the info dict, so the info hash does not cover it and
  this is the only thing stopping a peer supplying hashes of its own choosing,
- verifies pieces by re-deriving their merkle subtree root, through 0008,
- and hands the engine a v1-shaped model with the real identity restored
  through 0009.

Hybrid torrents are untouched by all of this: their v1 half is what every
client uses, and it is what we have always used.

The one thing still out of reach is **v2 magnets**, and that is a protocol
limit rather than a plumbing one: a magnet gets you the info dict over BEP 9,
but `piece layers` is not in the info dict. Fetching it needs the BEP 52 hash
messages (peer protocol ids 21/22/23), which `librqbit-peer-protocol` does not
implement - a fourth crate to vendor. Without them there are no piece hashes
for any file bigger than one piece, so nothing can be verified.

## Updating to a new librqbit release

One command - it downloads both published crates, replaces these folders,
re-applies the patches in order and bumps the version in Cargo.toml:

    powershell -File tools\update-librqbit.ps1              # latest
    powershell -File tools\update-librqbit.ps1 -Version x.y.z

If a patch no longer applies cleanly (upstream moved the code), the script says
which one. Before re-applying it by hand, check whether upstream has since done
the job itself - that is how 0010 and 0011 were retired. Then regenerate the
`.patch` file by diffing the pristine crate against the patched one and
rewriting the paths to `a/` and `b/`. Remember that the patches are sequential:
regenerating one means diffing against the state after its predecessor, not
against pristine.

Long term these should be submitted upstream as PRs so this folder can be
deleted again. 0006 and 0007 are ordinary bug fixes and should go first.
