# NanoTorrent visibility patches on librqbit

This is an unmodified copy of the **published** `librqbit` crate from
crates.io (so all its dependencies still resolve from crates.io) plus the
visibility-only patches in `../../patches/`. **No engine behavior is
changed** - the patches only expose read access to data the engine already
maintains. Wired up via `[patch.crates-io]` in `../../Cargo.toml`.

## Patches (see ../../patches/*.patch)

1. `0001-expose-chunk-tracker.patch` -
   `ManagedTorrent::with_chunk_tracker` changed from `pub(crate)` to `pub`.
   Used for: the piece progress bar (chunk tracker -> `get_have_pieces()`).

2. `0002-per-peer-have-pieces.patch` - adds
   `TorrentStateLive::per_peer_have_pieces() -> Vec<(SocketAddr, u64)>`
   (additive method; reads each live peer's `bitfield.count_ones()`).
   Used for: real seed counts and the availability column.

3. `0003-stream-transform-seam.patch` - the injection seam. Adds
   `pub trait StreamTransform` + `SessionOptions::stream_transform`; every
   outgoing peer stream is passed through the transform (addr + info_hash +
   boxed read/write halves) right after connect, before the BitTorrent
   handshake. The engine's behavior is unchanged when the option is `None`
   (the default). This is the hook that lets feature code (e.g. outgoing
   protocol encryption) live in the NanoTorrent crate instead of in patches.
   Consumed by `bittorrent::mse::MseTransform` (outgoing MSE/PE RC4).

4. `0004-pex-toggle.patch` - adds `SessionOptions::disable_pex` (threaded to
   `ManagedTorrentOptions::disable_pex`) and gates both PeX directions in
   `torrent_state/live/mod.rs`: stops spawning `task_send_pex_to_peer` and
   ignores incoming `UtPex` messages when set. Default `false` = upstream
   behavior. Wired to the `libtorrent.enable_pex` preference (inverted).

5. `0005-incoming-stream-transform-seam.patch` - the *incoming* counterpart of
   the 0003 seam. Adds `pub trait IncomingStreamTransform` +
   `SessionOptions::incoming_transform`; every accepted peer stream is passed
   through the transform (addr + the info-hashes of all active torrents +
   boxed read/write halves) in `Session::check_incoming_connection`, *before*
   the BitTorrent handshake is read. To carry the (possibly cipher-wrapped)
   halves through, `CheckedIncomingConnection.stream: TcpStream` becomes
   `read: BoxAsyncRead` + `write: BoxAsyncWrite`, and
   `PeerConnection::manage_peer_incoming` takes those boxed halves instead of a
   `TcpStream` (the downstream `manage_peer` is already generic). Behavior is
   unchanged when the option is `None`. Consumed by
   `bittorrent::mse::IncomingMseTransform` (inbound MSE/PE RC4 responder, with
   plaintext-vs-MSE detection). The transform gets *all* active info-hashes
   because the incoming info-hash is not known until the (possibly encrypted)
   handshake is read - the MSE responder resolves the peer's SKEY against them.

6. `0006-proxy-scope.patch` - adds `SessionOptions::proxy_peers` /
   `proxy_trackers` / `proxy_hostnames` (all default `false`) so a configured
   SOCKS proxy can be applied selectively, matching PicoTorrent's (and
   libtorrent's) `proxy_peer_connections` / `proxy_tracker_connections` /
   `proxy_hostnames`. Previously a set `socks_proxy_url` was applied to *both*
   the peer connector and the reqwest HTTP-tracker client unconditionally; now
   the connector proxy is gated on `proxy_peers`, the reqwest proxy on
   `proxy_trackers`, and `proxy_hostnames` upgrades the reqwest proxy to
   `socks5h` (proxy-side DNS). UDP tracker announces are never proxied (a
   librqbit limitation). Wired to the `libtorrent.proxy_*` preferences.

7. `0007-anonymous-mode.patch` - adds `SessionOptions::anonymize` (threaded to
   `ManagedTorrentOptions::anonymize`, mirroring 0004's `disable_pex`). When
   set, `PeerHandler::update_my_extended_handshake` clears the client version
   (`handshake.v = None`) so peers can't fingerprint the client by it. The
   other half of anonymity - a random peer id with no `-NT-` fingerprint - is
   done app-side in `build_session_options`. Wired to the
   `libtorrent.anonymous_mode` preference. (UDP tracker announces already send
   no client IP, so there is nothing to suppress there.)

## 0008 - per-tracker announce stats (Trackers tab)

Surfaces the seeders/leechers/interval each tracker returns (the upstream
crate receives them, then throws them away) so the UI can show a
PicoTorrent-style Trackers tab.

This one spans TWO crates:

- **`librqbit-tracker-comms` (now also vendored, `vendor/librqbit-tracker-comms`)**:
  adds `TrackerStat` + `SharedTrackerStats` (a shared `HashMap<url, stat>`),
  a `tracker_stats` field on `TrackerComms`, a `tracker_stats` param on
  `TrackerComms::start`, and records stats in the HTTP/UDP announce loops
  (`task_single_tracker_monitor_*`): on success `status="Working"`,
  seeders/leechers, `next_announce = now + interval`; on error `fails += 1`
  and the error text. `tracker_one_request_http` now returns
  `(interval, complete, incomplete)`.
- **`librqbit`**: a `tracker_stats: Mutex<HashMap<Id20, SharedTrackerStats>>`
  field on `Session` (init `Default::default()`), registered per torrent in
  `make_peer_rx` (passed into `TrackerComms::start`), plus the public
  `Session::tracker_stats_snapshot(info_hash)` accessor and a
  `pub use tracker_comms::TrackerStat` re-export in `lib.rs`. Also a second
  registry `tracker_tiers: Mutex<HashMap<Id20, Vec<Vec<Url>>>>` populated at the
  `AddTorrent::TorrentFileBytes` add path from `torrent.info.announce_list`
  (librqbit otherwise flattens tiers into the `trackers` HashSet), exposed via
  `Session::tracker_tiers_snapshot(info_hash)` so the UI can group by tier.
  IMPORTANT: the stats map registration in `make_peer_rx` must REUSE a stable
  per-info_hash `Arc` (`entry().or_default()`), never re-`insert` a fresh one -
  on startup a torrent is re-announced more than once and a fresh insert orphans
  the map the live announcer writes to (UI stuck on "Updating").

## 0009 - quiet upstream lints

`0009-quiet-upstream-lints.patch` - adds `#![allow(mismatched_lifetime_syntaxes)]`
to `src/lib.rs`. Not a visibility patch and not behavioral: crates.io
dependencies are compiled with `--cap-lints allow`, but a `[patch.crates-io]`
*path* dependency counts as local, so upstream's style-lint warnings appear in
every NanoTorrent build. This restores the normal quiet-dependency behavior.
Delete the patch once upstream builds clean on current rustc.

NOTE: `tools/update-librqbit.ps1` only re-vendors `librqbit`. The
`librqbit-tracker-comms` crate is vendored separately (copied from the cargo
registry, wired via a second `[patch.crates-io]` entry) and must be
re-vendored + re-patched by hand until this is submitted upstream.

## Updating to a new librqbit release

One command - it downloads the published crate, replaces this folder,
re-applies the patches and bumps the version in Cargo.toml:

    powershell -File tools\update-librqbit.ps1              # latest
    powershell -File tools\update-librqbit.ps1 -Version x.y.z

If a patch no longer applies cleanly (upstream moved the code), the script
says which one; re-apply it by hand (each is a few lines, described above)
and regenerate the .patch file with `git diff --no-index pristine patched`.

`build.rs` verifies at compile time that all patches are present and
fails with instructions if not - so a forgotten re-patch can't produce
confusing compile errors.

Long term these should be submitted upstream as PRs so this folder can be
deleted again.
