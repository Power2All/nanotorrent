//! WebSeed (BEP 19) - downloading a torrent's files straight from an HTTP
//! server listed in the metainfo's `url-list`.
//!
//! # Why this looks like a peer
//!
//! A web seed is not a peer. It is an HTTP server that happens to hold the
//! torrent's files, and it has no bitfield, no choking and no messages.
//!
//! But everything the engine does *around* a peer is exactly what a web seed
//! needs: which piece to fetch next, which chunks are outstanding, hashing the
//! result, retrying elsewhere when it does not verify, rate limiting, and the
//! stats the UI reads. Writing a second download path would mean a second
//! implementation of the part most worth having only one of - piece
//! verification.
//!
//! So this speaks the peer protocol over an in-memory duplex and hands the far
//! end to the engine (`Session::add_synthetic_peer`, engine patch 0011). The
//! engine sees a peer that has everything and never chokes; every `Request` it
//! sends is answered from an HTTP range GET. A piece that fails its hash is
//! discarded and re-fetched exactly as it would be from a bad peer - which
//! matters, because a web seed can be stale or plain wrong and there is no
//! other check on what it returns.
//!
//! # Both HTTP seeding standards
//!
//! - **BEP 19** (`url-list`, "GetRight" style) is what the world uses: the
//!   files are laid out on the server as they are in the torrent, and a piece
//!   is assembled from HTTP range requests across them.
//! - **BEP 17** (`httpseeds`, "Hoffman" style) is the older one. It is
//!   piece-oriented rather than file-oriented: one request names the info hash
//!   and a piece index, and the body IS that piece. It is rare, but a torrent
//!   carrying it costs nothing to support once the peer-shaped shell above
//!   exists, and a torrent that carries only `httpseeds` had no web seed at
//!   all before this.
//!
//! The difference between them is entirely inside [`ByteSource`]: everything
//! from the piece cache outwards is shared.
//! # Two things the first live test changed
//!
//! Both were wrong in ways only a real server showed up.
//!
//! - **One HTTP request per piece, not per chunk.** The engine requests in
//!   16 KiB chunks, so the obvious implementation issued 6400 range requests
//!   for a 100 MiB file. A whole piece is fetched once and the chunks are
//!   served from it, which is what other clients do and what any server
//!   expects.
//! - **A failed fetch must not be fatal.** It used to kill the synthetic peer,
//!   and the engine would re-queue the piece for "someone else" - who, with a
//!   single web seed, is nobody. The torrent then sat at 92% forever. Fetches
//!   now retry, and a seed that dies anyway is restarted a few times.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use librqbit_peer_protocol::{Handshake, Message, MessageDeserializeError, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One file as the torrent lays it out, flattened to a byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedFile {
    /// Path components below the torrent root, `/`-joined. Empty for a
    /// single-file torrent, where the torrent name IS the file.
    pub path: String,
    /// Absolute offset of this file within the torrent's byte stream.
    pub offset: u64,
    pub len: u64,
    /// BEP 47 padding. Never fetched - the bytes are zeros by definition, and
    /// no web seed has a file for them.
    pub padding: bool,
}

/// Where a slice of the torrent's byte stream actually lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub file: usize,
    /// Offset within that file.
    pub start: u64,
    pub len: u64,
}

/// Split an absolute byte range into per-file spans.
///
/// A chunk can straddle a file boundary in a v1 torrent (v2 and hybrid align
/// every file to a piece, so they do not), which is why this returns a list.
pub fn spans(files: &[SeedFile], offset: u64, len: u64) -> Vec<Span> {
    let mut out = Vec::new();
    let end = offset + len;
    for (i, f) in files.iter().enumerate() {
        if f.len == 0 {
            continue;
        }
        let f_end = f.offset + f.len;
        if f_end <= offset || f.offset >= end {
            continue;
        }
        let start = offset.max(f.offset);
        let stop = end.min(f_end);
        out.push(Span {
            file: i,
            start: start - f.offset,
            len: stop - start,
        });
    }
    out
}

/// The URL a file lives at, per BEP 19 ("GetRight" style).
///
/// The rule is only implicit in the BEP, so, spelled out: for a **single-file**
/// torrent the URL names the file itself, unless it ends in `/`, in which case
/// the torrent name is appended. For a **multi-file** torrent the URL is a
/// directory and the torrent name plus the file's path are appended.
pub fn file_url(base: &str, torrent_name: &str, file: &SeedFile, multi_file: bool) -> String {
    let mut url = base.to_string();
    if multi_file {
        if !url.ends_with('/') {
            url.push('/');
        }
        url.push_str(&encode_path(torrent_name));
        url.push('/');
        url.push_str(&encode_path(&file.path));
    } else if url.ends_with('/') {
        url.push_str(&encode_path(torrent_name));
    }
    url
}

/// Percent-encode a path for a URL, leaving the separators alone.
///
/// Deliberately minimal: file names in torrents are arbitrary bytes, and a raw
/// space or `#` in a URL is either rejected or silently truncates the path.
fn encode_path(path: &str) -> String {
    // `/` stays: this encodes a path, and escaping its separators would ask
    // the server for one file with slashes in its name.
    crate::core::utils::percent_encode(path.as_bytes(), b"/")
}

/// Read `url-list` out of a `.torrent`.
///
/// It sits at the top level, beside `info`, and is either a single string or a
/// list of them. Absent for magnets, which is why a web seed can only come
/// from a real torrent file.
pub fn parse_url_list(torrent_bytes: &[u8]) -> Vec<String> {
    parse_url_key(torrent_bytes, b"url-list")
}

/// The shared parser behind `url-list` (BEP 19) and `httpseeds` (BEP 17).
///
/// Both keys sit at the top level beside `info` and both are "either a string
/// or a list of strings", so one parser does for both.
fn parse_url_key(torrent_bytes: &[u8], key: &[u8]) -> Vec<String> {
    use crate::bittorrent::metainfo::bencode_lookup;
    let Some(raw) = bencode_lookup(torrent_bytes, key) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match raw.first() {
        // A single URL.
        Some(b'0'..=b'9') => {
            if let Some(s) = bencode_str_value(raw) {
                out.push(s);
            }
        }
        // A list of them.
        Some(b'l') => {
            let mut i = 1;
            while raw.get(i).is_some_and(|c| *c != b'e') {
                let Some(end) = str_end(raw, i) else { break };
                if let Some(s) = bencode_str_value(&raw[i..end]) {
                    out.push(s);
                }
                i = end;
            }
        }
        _ => {}
    }
    out.retain(|u| u.starts_with("http://") || u.starts_with("https://"));
    out
}

fn str_end(b: &[u8], i: usize) -> Option<usize> {
    let colon = b[i..].iter().position(|c| *c == b':')? + i;
    let len: usize = std::str::from_utf8(&b[i..colon]).ok()?.parse().ok()?;
    colon.checked_add(1 + len).filter(|e| *e <= b.len())
}

fn bencode_str_value(b: &[u8]) -> Option<String> {
    let colon = b.iter().position(|c| *c == b':')?;
    let len: usize = std::str::from_utf8(&b[..colon]).ok()?.parse().ok()?;
    let s = b.get(colon + 1..colon + 1 + len)?;
    Some(String::from_utf8_lossy(s).into_owned())
}

/// Where the bytes come from. A trait so the protocol loop can be tested
/// without an HTTP server standing behind it.
pub trait ByteSource: Send + Sync + std::fmt::Debug {
    /// Bytes from one file, at a file-relative offset. The BEP 19 shape.
    fn fetch(
        &self,
        file: &SeedFile,
        start: u64,
        len: u64,
    ) -> futures::future::BoxFuture<'_, Result<Vec<u8>>>;

    /// A whole piece in one request, for a source that is piece-oriented.
    ///
    /// `None` - the default - means "assemble it from [`fetch`] calls across
    /// the file spans", which is what BEP 19 does. BEP 17 overrides this
    /// because a piece is the only thing it can ask for: its request names an
    /// info hash and a piece index, and knows nothing about files.
    fn fetch_piece(
        &self,
        _index: u32,
        _len: u64,
    ) -> Option<futures::future::BoxFuture<'_, Result<Vec<u8>>>> {
        None
    }
}

/// How many times one range request is retried before the seed gives up.
const FETCH_ATTEMPTS: u32 = 3;
/// How many times a dead web seed is restarted before it is left alone.
const SEED_RESTARTS: u32 = 5;

/// The real one: an HTTP range request per span.
#[derive(Debug)]
pub struct HttpSource {
    pub client: reqwest::Client,
    pub base: String,
    pub torrent_name: String,
    pub multi_file: bool,
}

impl ByteSource for HttpSource {
    fn fetch(
        &self,
        file: &SeedFile,
        start: u64,
        len: u64,
    ) -> futures::future::BoxFuture<'_, Result<Vec<u8>>> {
        let url = file_url(&self.base, &self.torrent_name, file, self.multi_file);
        Box::pin(async move {
            let end = start + len - 1;
            let resp = self
                .client
                .get(&url)
                .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
                .send()
                .await
                .with_context(|| format!("GET {url}"))?;
            // 206 is the only success we can use. A 200 means the server
            // ignored the Range and is sending the whole file, which for a
            // large file would be a very expensive way to get 16 KiB.
            if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                bail!("{url} answered {}, expected 206", resp.status());
            }
            let body = resp.bytes().await.context("reading body")?;
            if body.len() as u64 != len {
                bail!("{url} returned {} bytes, expected {len}", body.len());
            }
            Ok(body.to_vec())
        })
    }
}

/// BEP 17: one request per piece, against a single URL.
///
/// The query string is the whole protocol - `info_hash` as the raw 20 bytes
/// percent-encoded exactly as a tracker announce encodes them, and `piece` as
/// a decimal index. `ranges` exists in the spec for asking for part of a piece
/// and is deliberately not used: a whole piece is one request either way, and
/// servers that predate it ignore it.
#[derive(Debug)]
pub struct HttpSeedSource {
    pub client: reqwest::Client,
    pub base: String,
    pub info_hash: librqbit::Id20,
}

impl HttpSeedSource {
    /// The URL for one piece. Separated out so the query construction can be
    /// tested without a server.
    pub fn piece_url(&self, index: u32) -> String {
        // A `?` may already be there - BEP 17 URLs are often a CGI endpoint
        // that carries its own parameters.
        let join = if self.base.contains('?') { '&' } else { '?' };
        format!(
            "{}{join}info_hash={}&piece={index}",
            self.base,
            crate::core::utils::percent_encode(&self.info_hash.0, b"")
        )
    }
}

impl ByteSource for HttpSeedSource {
    /// Never called: [`fetch_piece`] answers first, and BEP 17 has no way to
    /// ask for a file-relative range - its requests are keyed by piece.
    fn fetch(
        &self,
        _file: &SeedFile,
        _start: u64,
        _len: u64,
    ) -> futures::future::BoxFuture<'_, Result<Vec<u8>>> {
        Box::pin(async move { bail!("BEP 17 seeds are fetched a whole piece at a time") })
    }

    fn fetch_piece(
        &self,
        index: u32,
        len: u64,
    ) -> Option<futures::future::BoxFuture<'_, Result<Vec<u8>>>> {
        let url = self.piece_url(index);
        Some(Box::pin(async move {
            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("GET {url}"))?;
            // 200 here, unlike BEP 19: the request is not a range request, so
            // the whole response IS the piece.
            if !resp.status().is_success() {
                bail!("{url} answered {}", resp.status());
            }
            let body = resp.bytes().await.context("reading body")?;
            if body.len() as u64 != len {
                bail!("{url} returned {} bytes, expected {len}", body.len());
            }
            Ok(body.to_vec())
        }))
    }
}

/// Read `httpseeds` out of a `.torrent` - BEP 17's equivalent of `url-list`.
///
/// Same shape and the same place in the file, so the same parser does both.
pub fn parse_http_seeds(torrent_bytes: &[u8]) -> Vec<String> {
    parse_url_key(torrent_bytes, b"httpseeds")
}

/// What the responder needs to know about the torrent.
#[derive(Debug, Clone)]
pub struct SeedTorrent {
    pub info_hash: librqbit::Id20,
    pub peer_id: librqbit::Id20,
    pub piece_length: u64,
    pub total_pieces: u32,
    pub files: Vec<SeedFile>,
}

/// Speak the peer protocol on `stream`, answering every request from `source`.
///
/// The engine is on the other end and believes this is an incoming peer. The
/// sequence is the one it expects: our handshake, then a full bitfield (we
/// have everything), then unchoke, then answers.
pub async fn run<S>(mut stream: S, torrent: SeedTorrent, source: Arc<dyn ByteSource>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut out = vec![0u8; 1 << 16];

    // Reserved bits all zero: no extended messages, no fast extension. There
    // is nothing a web seed would do with either, and not advertising them is
    // what stops the engine waiting for messages that will never come.
    let mut handshake = Handshake::new(torrent.info_hash, torrent.peer_id);
    handshake.reserved = 0;
    let n = handshake.serialize_unchecked_len(&mut out);
    stream.write_all(&out[..n]).await?;

    // The engine answers with its own handshake. Read and discard it - there
    // is nothing to check that `check_incoming_connection` did not already.
    let mut hs = [0u8; 68];
    stream.read_exact(&mut hs).await.context("reading handshake")?;

    // "I have everything." A bitfield rather than BEP 6 `have all`, because we
    // did not advertise the fast extension.
    let bitfield_bytes = torrent.total_pieces.div_ceil(8) as usize;
    let mut bitfield = vec![0xFFu8; bitfield_bytes];
    // Spare bits in the last byte MUST be zero, or a strict peer drops us.
    let spare = bitfield_bytes * 8 - torrent.total_pieces as usize;
    if spare > 0
        && let Some(last) = bitfield.last_mut()
    {
        *last = 0xFFu8 << spare;
    }
    let n = Message::Bitfield(librqbit::ByteBuf(&bitfield))
        .serialize(&mut out, &Default::default)?;
    stream.write_all(&out[..n]).await?;

    let n = Message::Unchoke.serialize(&mut out, &Default::default)?;
    stream.write_all(&out[..n]).await?;

    // Read messages, answer requests.
    let mut cache = PieceCache::default();
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 16);
    let mut read_chunk = vec![0u8; 1 << 16];
    loop {
        let read = stream.read(&mut read_chunk).await?;
        if read == 0 {
            return Ok(()); // engine hung up: torrent finished, paused or gone
        }
        buf.extend_from_slice(&read_chunk[..read]);

        loop {
            let (msg, consumed) = match Message::deserialize(&buf, &[]) {
                Ok(v) => v,
                Err(MessageDeserializeError::NotEnoughData(..)) => break,
                Err(e) => return Err(e).context("engine sent something unreadable"),
            };
            let request = match msg {
                Message::Request(r) => Some(r),
                // Everything else is either irrelevant to a web seed
                // (interested, have, cancel) or something we never asked for.
                _ => None,
            };
            buf.drain(..consumed);

            if let Some(r) = request {
                serve(&mut stream, &torrent, &source, r, &mut out, &mut cache).await?;
            }
        }
    }
}

/// One whole piece, held so the engine's 16 KiB chunk requests do not each
/// become an HTTP round trip.
#[derive(Default)]
struct PieceCache {
    index: Option<u32>,
    data: Vec<u8>,
}

/// Fetch one span, retrying a few times first.
///
/// A single failed request used to be fatal to the whole web seed, which with
/// one seed configured means the torrent never finishes.
async fn fetch_with_retry(
    source: &Arc<dyn ByteSource>,
    file: &SeedFile,
    start: u64,
    len: u64,
) -> Result<Vec<u8>> {
    let mut last = None;
    for attempt in 0..FETCH_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(250 << attempt)).await;
        }
        match source.fetch(file, start, len).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                tracing::debug!(attempt, "web seed fetch failed: {e:#}");
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no attempts made")))
}

async fn fetch_piece_with_retry(
    source: &Arc<dyn ByteSource>,
    index: u32,
    len: u64,
) -> Result<Vec<u8>> {
    let mut last = None;
    for attempt in 0..FETCH_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(250 << attempt)).await;
        }
        let Some(fut) = source.fetch_piece(index, len) else {
            bail!("source stopped offering whole pieces mid-torrent");
        };
        match fut.await {
            Ok(v) => return Ok(v),
            Err(e) => {
                tracing::debug!(attempt, "web seed piece fetch failed: {e:#}");
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no attempts made")))
}

/// The whole of `index`, from the cache or from the server.
async fn piece_bytes<'a>(
    torrent: &SeedTorrent,
    source: &Arc<dyn ByteSource>,
    index: u32,
    cache: &'a mut PieceCache,
) -> Result<&'a [u8]> {
    if cache.index == Some(index) {
        return Ok(&cache.data);
    }
    let total: u64 = torrent.files.iter().map(|f| f.len).sum();
    let offset = index as u64 * torrent.piece_length;
    if offset >= total {
        bail!("engine asked for piece {index}, past the end of the torrent");
    }
    let len = torrent.piece_length.min(total - offset);

    // A piece-oriented source answers in one request; retried the same number
    // of times as a span fetch, for the same reason.
    if source.fetch_piece(index, len).is_some() {
        let data = fetch_piece_with_retry(source, index, len).await?;
        if data.len() as u64 != len {
            bail!("web seed returned {} bytes for a {len}-byte piece {index}", data.len());
        }
        cache.index = Some(index);
        cache.data = data;
        return Ok(&cache.data);
    }

    let mut data = Vec::with_capacity(len as usize);
    for span in spans(&torrent.files, offset, len) {
        let file = &torrent.files[span.file];
        if file.padding {
            // Padding is zeros by definition and no server has a file for it.
            data.resize(data.len() + span.len as usize, 0);
            continue;
        }
        let bytes = fetch_with_retry(source, file, span.start, span.len).await?;
        data.extend_from_slice(&bytes);
    }
    if data.len() as u64 != len {
        bail!("assembled {} bytes for a {len}-byte piece {index}", data.len());
    }

    cache.index = Some(index);
    cache.data = data;
    Ok(&cache.data)
}

/// Answer one `Request` with a `Piece`, out of the cached piece.
async fn serve<S>(
    stream: &mut S,
    torrent: &SeedTorrent,
    source: &Arc<dyn ByteSource>,
    request: Request,
    out: &mut [u8],
    cache: &mut PieceCache,
) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let piece = piece_bytes(torrent, source, request.index, cache).await?;
    let from = request.begin as usize;
    let to = from + request.length as usize;
    let data = piece
        .get(from..to)
        .with_context(|| {
            format!(
                "engine asked for {}..{to} of piece {}, which is {} bytes",
                from,
                request.index,
                piece.len()
            )
        })?
        .to_vec();

    let msg = Message::Piece(librqbit_peer_protocol::Piece::from_data(
        request.index,
        request.begin,
        &data,
    ));
    let n = msg.serialize(out, &Default::default)?;
    stream.write_all(&out[..n]).await?;
    Ok(())
}

/// Start every web seed a torrent lists.
///
/// A no-op when the torrent has no `url-list`, which is most of them and all
/// magnets. Each seed becomes one synthetic peer; if one dies the engine
/// re-queues its pieces for the others exactly as it would for a real peer.
pub fn spawn_all(
    session: Arc<librqbit::Session>,
    runtime: &tokio::runtime::Handle,
    handle: &Arc<librqbit::ManagedTorrent>,
    torrent_bytes: &[u8],
    // Handed in rather than built here. A web seed fetches torrent payload
    // over HTTP, so it is exactly the traffic a proxy is turned on to cover -
    // and a client built locally would have quietly gone direct.
    client: reqwest::Client,
) {
    let urls = parse_url_list(torrent_bytes);
    let http_seeds = parse_http_seeds(torrent_bytes);
    if urls.is_empty() && http_seeds.is_empty() {
        return;
    }

    let Some(metadata) = handle.metadata.load_full() else {
        return; // metadata not resolved yet; a magnet has neither key anyway
    };
    let name = handle.name().unwrap_or_default();
    let files: Vec<SeedFile> = metadata
        .file_infos
        .iter()
        .map(|fi| SeedFile {
            path: fi
                .relative_filename
                .to_string_lossy()
                .replace('\\', "/"),
            offset: fi.offset_in_torrent,
            len: fi.len,
            padding: fi.attrs.padding,
        })
        .collect();
    // "Multi-file" is about the torrent's SHAPE, not how many entries it has:
    // a single-file torrent's one file is the torrent name itself, and its URL
    // must not have the name appended twice.
    let multi_file = files.iter().filter(|f| !f.padding).count() > 1
        || files.first().is_some_and(|f| f.path != name);

    let torrent = SeedTorrent {
        info_hash: handle.info_hash(),
        // A peer id that says what this is, in the Azureus style everyone
        // parses. It only ever appears in our own Peers tab.
        peer_id: librqbit::Id20::new(*b"-NTWS0-webseed000000"),
        piece_length: metadata.lengths().default_piece_length() as u64,
        total_pieces: metadata.lengths().total_pieces(),
        files,
    };

    // BEP 19 first, then BEP 17. A torrent carrying both gets a synthetic peer
    // for each URL of each: they are alternative routes to the same bytes, and
    // every one of them is hash-checked, so there is no harm in trying all.
    let sources: Vec<(String, Arc<dyn ByteSource>)> = urls
        .into_iter()
        .map(|url| {
            let s: Arc<dyn ByteSource> = Arc::new(HttpSource {
                client: client.clone(),
                base: url.clone(),
                torrent_name: name.clone(),
                multi_file,
            });
            (url, s)
        })
        .chain(http_seeds.into_iter().map(|url| {
            let s: Arc<dyn ByteSource> = Arc::new(HttpSeedSource {
                client: client.clone(),
                base: url.clone(),
                info_hash: handle.info_hash(),
            });
            (url, s)
        }))
        .collect();

    for (i, (url, source)) in sources.into_iter().enumerate() {
        let torrent = torrent.clone();
        let session = session.clone();
        let addr = synthetic_addr(i);
        runtime.spawn(async move {
            // Restarted on failure, because with a single web seed there is no
            // "someone else" for the engine to re-queue the pieces to - a seed
            // that dies once would otherwise leave the torrent stuck short of
            // complete, which is exactly what the first live test did.
            for attempt in 0..=SEED_RESTARTS {
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(2 << attempt.min(4))).await;
                    tracing::debug!(attempt, "restarting web seed {url}");
                }

                let (ours, theirs) = tokio::io::duplex(1 << 18);
                let (t, s) = (torrent.clone(), source.clone());
                let responder =
                    tokio::spawn(async move { run(ours, t, s).await });

                let (r, w) = tokio::io::split(theirs);
                if let Err(err) = session
                    .add_synthetic_peer(addr, Box::new(r), Box::new(w))
                    .await
                {
                    // The torrent is gone, or already has this peer. Neither
                    // is worth retrying against.
                    tracing::debug!("web seed {url} could not be attached: {err:#}");
                    responder.abort();
                    return;
                }

                match responder.await {
                    // A clean end means the engine hung up: finished, paused
                    // or removed. Nothing to restart for.
                    Ok(Ok(())) => return,
                    Ok(Err(err)) => tracing::debug!("web seed {url} stopped: {err:#}"),
                    Err(_) => return, // aborted
                }
            }
            tracing::warn!("web seed {url} gave up after {SEED_RESTARTS} restarts");
        });
    }
}

/// A distinct loopback address per web seed, so the engine can key peers by it.
///
/// Loopback on purpose: PeX already refuses to pass private addresses on to
/// remote peers, so a synthetic peer cannot leak into the swarm.
pub fn synthetic_addr(index: usize) -> SocketAddr {
    use std::net::{IpAddr, Ipv4Addr};
    // 127.0.0.0/8 is all loopback; walking the third and fourth octets gives
    // plenty of room and never collides with a real local peer on 127.0.0.1.
    let a = ((index >> 8) & 0xFF) as u8;
    let b = (index & 0xFF) as u8;
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 88, a, b)), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<SeedFile> {
        vec![
            SeedFile { path: "a.bin".into(), offset: 0, len: 100, padding: false },
            SeedFile { path: ".pad/28".into(), offset: 100, len: 28, padding: true },
            SeedFile { path: "sub/b.bin".into(), offset: 128, len: 200, padding: false },
        ]
    }

    #[test]
    fn spans_split_on_file_boundaries() {
        let f = files();
        // Wholly inside the first file.
        assert_eq!(
            spans(&f, 10, 20),
            vec![Span { file: 0, start: 10, len: 20 }]
        );
        // Straddling all three.
        assert_eq!(
            spans(&f, 90, 50),
            vec![
                Span { file: 0, start: 90, len: 10 },
                Span { file: 1, start: 0, len: 28 },
                Span { file: 2, start: 0, len: 12 },
            ]
        );
        // Exactly one whole file.
        assert_eq!(
            spans(&f, 128, 200),
            vec![Span { file: 2, start: 0, len: 200 }]
        );
        // Zero-length files never appear.
        let with_empty = vec![
            SeedFile { path: "e".into(), offset: 0, len: 0, padding: false },
            SeedFile { path: "a".into(), offset: 0, len: 10, padding: false },
        ];
        assert_eq!(
            spans(&with_empty, 0, 10),
            vec![Span { file: 1, start: 0, len: 10 }]
        );
    }

    #[test]
    fn urls_follow_the_getright_rules() {
        let f = SeedFile { path: "sub/b.bin".into(), offset: 0, len: 1, padding: false };

        // Multi-file: base is a directory, name and path appended.
        assert_eq!(
            file_url("http://h/d", "My Torrent", &f, true),
            "http://h/d/My%20Torrent/sub/b.bin"
        );
        // A trailing slash on the base is not doubled.
        assert_eq!(
            file_url("http://h/d/", "T", &f, true),
            "http://h/d/T/sub/b.bin"
        );

        // Single-file: the URL names the file, and is used verbatim.
        let single = SeedFile { path: String::new(), offset: 0, len: 1, padding: false };
        assert_eq!(
            file_url("http://h/movie.mkv", "movie.mkv", &single, false),
            "http://h/movie.mkv"
        );
        // ...unless it ends in a slash, when the name is appended.
        assert_eq!(
            file_url("http://h/files/", "movie.mkv", &single, false),
            "http://h/files/movie.mkv"
        );
    }

    #[test]
    fn url_list_reads_both_shapes() {
        // A single string.
        let one = b"d8:url-list19:http://example.com/4:infod0:0:ee";
        assert_eq!(parse_url_list(one), vec!["http://example.com/"]);

        // A list of them.
        let many = b"d8:url-listl19:http://example.com/20:https://example.com/e4:infod0:0:ee";
        assert_eq!(
            parse_url_list(many),
            vec!["http://example.com/", "https://example.com/"]
        );

        // Absent, and non-HTTP schemes (BEP 19 allows ftp; we do not speak it).
        assert!(parse_url_list(b"d4:infod0:0:eee").is_empty());
        let ftp = b"d8:url-list18:ftp://example.com/4:infod0:0:ee";
        assert!(parse_url_list(ftp).is_empty());
        // Malformed must not panic.
        assert!(parse_url_list(b"d8:url-list").is_empty());
        assert!(parse_url_list(b"junk").is_empty());
    }

    #[test]
    fn httpseeds_reads_the_same_shapes_as_url_list() {
        // BEP 17 keys the list `httpseeds`, otherwise identical.
        let one = b"d9:httpseeds23:http://example.com/seed4:infod0:0:ee";
        assert_eq!(parse_http_seeds(one), vec!["http://example.com/seed"]);

        let many = b"d9:httpseedsl18:http://a.example/s19:https://b.example/se4:infod0:0:ee";
        assert_eq!(
            parse_http_seeds(many),
            vec!["http://a.example/s", "https://b.example/s"]
        );

        // A torrent with only `url-list` has no BEP 17 seeds and vice versa.
        assert!(parse_http_seeds(b"d8:url-list19:http://example.com/4:infod0:0:ee").is_empty());
        assert!(parse_url_list(one).is_empty());

        // Malformed must not panic, same as the other key.
        assert!(parse_http_seeds(b"d9:httpseeds").is_empty());
    }

    #[test]
    fn bep17_urls_carry_the_info_hash_and_piece() {
        // The hash bytes are chosen to cover both halves of the rule: `A` and
        // `7` are unreserved and stay, 0x00 and 0xFF must be escaped.
        let mut raw = [0u8; 20];
        raw[0] = b'A';
        raw[1] = 0x00;
        raw[2] = 0xFF;
        raw[3] = b'7';
        let source = HttpSeedSource {
            client: reqwest::Client::new(),
            base: "http://h/seed.cgi".into(),
            info_hash: librqbit::Id20::new(raw),
        };
        let url = source.piece_url(3);
        assert!(
            url.starts_with("http://h/seed.cgi?info_hash=A%00%FF7%00%00%00"),
            "unexpected encoding: {url}"
        );
        assert!(url.ends_with("&piece=3"), "unexpected tail: {url}");

        // A base that already has a query string gets `&`, not a second `?`.
        let with_query = HttpSeedSource {
            client: reqwest::Client::new(),
            base: "http://h/seed.cgi?x=1".into(),
            info_hash: librqbit::Id20::new(raw),
        };
        let url = with_query.piece_url(0);
        assert_eq!(url.matches('?').count(), 1, "two query separators: {url}");
        assert!(url.contains("?x=1&info_hash="), "unexpected join: {url}");
    }

    /// The BEP 17 path through the same protocol loop the GetRight one uses.
    ///
    /// The source answers whole pieces and refuses file-relative fetches - so
    /// if `piece_bytes` ever stopped preferring `fetch_piece`, this fails
    /// rather than silently falling back.
    #[tokio::test]
    async fn the_responder_serves_a_piece_oriented_source() {
        #[derive(Debug)]
        struct Pieces {
            content: Vec<u8>,
            piece_length: u64,
        }
        impl ByteSource for Pieces {
            fn fetch(
                &self,
                _file: &SeedFile,
                _start: u64,
                _len: u64,
            ) -> futures::future::BoxFuture<'_, Result<Vec<u8>>> {
                Box::pin(async move { bail!("a BEP 17 source must never be asked for a file span") })
            }

            fn fetch_piece(
                &self,
                index: u32,
                len: u64,
            ) -> Option<futures::future::BoxFuture<'_, Result<Vec<u8>>>> {
                let from = index as u64 * self.piece_length;
                let slice = self.content[from as usize..(from + len) as usize].to_vec();
                Some(Box::pin(async move { Ok(slice) }))
            }
        }

        let content: Vec<u8> = (0..328u32).map(|i| (i * 7) as u8).collect();
        let torrent = SeedTorrent {
            info_hash: librqbit::Id20::new([9u8; 20]),
            peer_id: librqbit::Id20::new([8u8; 20]),
            piece_length: 256,
            total_pieces: 2,
            files: files(),
        };

        let (ours, mut theirs) = tokio::io::duplex(1 << 16);
        let source: Arc<dyn ByteSource> = Arc::new(Pieces {
            content: content.clone(),
            piece_length: 256,
        });
        let t = torrent.clone();
        let task = tokio::spawn(async move { run(ours, t, source).await });

        let mut buf = vec![0u8; 1024];
        let hs = Handshake::new(torrent.info_hash, librqbit::Id20::new([1u8; 20]));
        let n = hs.serialize_unchecked_len(&mut buf);
        theirs.write_all(&buf[..n]).await.unwrap();
        let mut their_hs = [0u8; 68];
        theirs.read_exact(&mut their_hs).await.unwrap();

        // Ask for a chunk that sits in the middle of piece 0, so a wrong piece
        // index or a wrong offset inside it both show up as wrong bytes.
        let req = Request { index: 0, begin: 64, length: 32 };
        let n = Message::Request(req).serialize(&mut buf, &Default::default).unwrap();
        theirs.write_all(&buf[..n]).await.unwrap();

        let mut acc: Vec<u8> = Vec::new();
        loop {
            let n = theirs.read(&mut buf).await.unwrap();
            assert_ne!(n, 0, "the responder closed before answering");
            acc.extend_from_slice(&buf[..n]);
            let mut consumed = 0;
            let mut answered = false;
            while let Ok((m, used)) = Message::deserialize(&acc[consumed..], &[]) {
                consumed += used;
                if let Message::Piece(p) = m {
                    assert_eq!(p.index, 0);
                    assert_eq!(p.begin, 64);
                    // No public accessor for the block, so read it back out of
                    // the wire form: index, begin, then the data.
                    let mut tmp = vec![0u8; p.len() + 8];
                    let written = p.serialize_unchecked_len(&mut tmp);
                    assert_eq!(&tmp[8..written], &content[64..96]);
                    answered = true;
                }
            }
            if answered {
                break;
            }
            acc.drain(..consumed);
        }
        drop(theirs);
        let _ = task.await;
    }

    /// End to end against a REAL web seed, through the real engine.
    ///
    /// Builds a torrent for a file the server actually hosts, hands it to a
    /// librqbit session with no DHT, no trackers and no listener - so the
    /// **only** possible source of data is the web seed - and waits for it to
    /// complete. Every piece is hash-checked by the engine on the way in, so
    /// finishing at all means the HTTP path produced correct bytes.
    ///
    /// Ignored by default: it needs the network. Run it with
    ///
    ///     cargo test --bin nanotorrent-gui webseed_downloads_from_a_real_server -- --ignored --nocapture
    ///
    /// A short final piece is deliberate (8 MiB pieces over 100 MiB leaves a
    /// 4 MiB tail): if the range arithmetic were wrong at the end, the server
    /// would return fewer bytes than asked for and the fetch would fail. That
    /// matters here because the test file is all zeros, so a wrong offset in
    /// the MIDDLE cannot be caught by content - the in-memory test above uses
    /// varying bytes for exactly that reason.
    #[tokio::test]
    #[ignore = "downloads 100 MiB from www.nanotorrent.org"]
    async fn webseed_downloads_from_a_real_server() {
        use crate::bittorrent::torrent_create::{CreateInput, TorrentVersion, build};

        const URL: &str = "https://www.nanotorrent.org/testfile.bin";
        const PIECE_LEN: u32 = 8 * 1024 * 1024;

        let dir = std::env::temp_dir().join(format!("nt-ws-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        let out = dir.join("out");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&out).unwrap();

        // Fetch it once so the torrent describes what the server really has.
        // Hashing what we downloaded (rather than assuming the contents) keeps
        // this honest if the file is ever regenerated.
        let body = reqwest::get(URL).await.unwrap().bytes().await.unwrap();
        assert!(!body.is_empty(), "the server returned nothing");
        let payload = src.join("testfile.bin");
        std::fs::write(&payload, &body).unwrap();
        eprintln!("web seed test: {} bytes", body.len());

        let mut torrent = build(&CreateInput {
            source: &payload,
            trackers: &[],
            comment: "",
            created_by: "nanotorrent webseed test".into(),
            private: false,
            piece_length: Some(PIECE_LEN),
            version: TorrentVersion::V1,
        })
        .unwrap()
        .bytes;

        // Append `url-list` to the outer dict. "url-list" sorts after every
        // other key we emit, so the end is where bencode wants it.
        let entry = format!("8:url-list{}:{URL}", URL.len());
        torrent.truncate(torrent.len() - 1); // drop the closing 'e'
        torrent.extend_from_slice(entry.as_bytes());
        torrent.push(b'e');
        assert_eq!(parse_url_list(&torrent), vec![URL], "url-list did not stick");

        // A session that CANNOT get data any other way.
        let session = librqbit::Session::new_with_opts(
            out.clone(),
            librqbit::SessionOptions {
                dht: None,
                listen: None,
                disable_trackers: true,
                persistence: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let handle = session
            .add_torrent(
                librqbit::AddTorrent::from_bytes(torrent.clone()),
                Some(librqbit::AddTorrentOptions {
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();

        spawn_all(
            session.clone(),
            &tokio::runtime::Handle::current(),
            &handle,
            &torrent,
            reqwest::Client::new(),
        );

        let finished = tokio::time::timeout(
            std::time::Duration::from_secs(240),
            handle.wait_until_completed(),
        )
        .await;

        let stats = handle.stats();
        assert!(
            finished.is_ok(),
            "timed out with {} of {} bytes - progress {:?}",
            stats.progress_bytes,
            stats.total_bytes,
            stats.state
        );
        finished.unwrap().unwrap();

        // The engine hash-checked every piece to get here; this confirms what
        // landed on disk is byte-identical to what the server served.
        let got = std::fs::read(out.join("testfile.bin")).unwrap();
        assert_eq!(got.len(), body.len(), "wrong size on disk");
        assert!(got == body.as_ref(), "downloaded bytes differ from the source");

        session.stop().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn synthetic_addresses_are_loopback_and_distinct() {
        let a = synthetic_addr(0);
        let b = synthetic_addr(1);
        let c = synthetic_addr(300);
        assert!(a.ip().is_loopback() && b.ip().is_loopback() && c.ip().is_loopback());
        assert_ne!(a, b);
        assert_ne!(b, c);
    }

    /// The protocol loop, end to end, with the bytes coming from memory
    /// instead of a server. This is the part that would otherwise only ever be
    /// exercised against a live web seed.
    #[tokio::test]
    async fn the_responder_answers_requests_from_the_source() {
        #[derive(Debug)]
        struct Mem(Vec<u8>);
        impl ByteSource for Mem {
            fn fetch(
                &self,
                file: &SeedFile,
                start: u64,
                len: u64,
            ) -> futures::future::BoxFuture<'_, Result<Vec<u8>>> {
                let from = (file.offset + start) as usize;
                let slice = self.0[from..from + len as usize].to_vec();
                Box::pin(async move { Ok(slice) })
            }
        }

        let content: Vec<u8> = (0..328u32).map(|i| (i * 7) as u8).collect();
        let torrent = SeedTorrent {
            info_hash: librqbit::Id20::new([9u8; 20]),
            peer_id: librqbit::Id20::new([8u8; 20]),
            // 256 so that ONE piece spans all three files: a chunk never
            // crosses a piece boundary, and piece_bytes now enforces that.
            piece_length: 256,
            total_pieces: 2, // 328 bytes over 256 => 2 pieces
            files: files(),
        };

        let (ours, mut theirs) = tokio::io::duplex(1 << 16);
        let source: Arc<dyn ByteSource> = Arc::new(Mem(content.clone()));
        let t = torrent.clone();
        let task = tokio::spawn(async move { run(ours, t, source).await });

        // Play the engine's part: handshake, then read ours.
        let mut buf = vec![0u8; 1024];
        let hs = Handshake::new(torrent.info_hash, librqbit::Id20::new([1u8; 20]));
        let n = hs.serialize_unchecked_len(&mut buf);
        theirs.write_all(&buf[..n]).await.unwrap();

        let mut their_hs = [0u8; 68];
        theirs.read_exact(&mut their_hs).await.unwrap();
        let (parsed, _) = Handshake::deserialize(&their_hs).unwrap();
        assert_eq!(parsed.info_hash, torrent.info_hash);
        assert!(
            !parsed.supports_extended(),
            "a web seed must not advertise extended messages"
        );

        // Collect messages until we have the bitfield and the unchoke.
        let mut acc: Vec<u8> = Vec::new();
        let mut saw_bitfield = false;
        let mut saw_unchoke = false;
        while !(saw_bitfield && saw_unchoke) {
            let n = theirs.read(&mut buf).await.unwrap();
            assert_ne!(n, 0, "the responder closed before sending bitfield/unchoke");
            acc.extend_from_slice(&buf[..n]);
            while let Ok((m, used)) = Message::deserialize(&acc, &[]) {
                match m {
                    Message::Bitfield(b) => {
                        // 2 pieces => one byte, top two bits set.
                        assert_eq!(b.as_ref(), &[0b1100_0000]);
                        saw_bitfield = true;
                    }
                    Message::Unchoke => saw_unchoke = true,
                    _ => {}
                }
                acc.drain(..used);
            }
        }

        // Ask for a chunk that straddles all three files, padding included.
        let req = Request { index: 0, begin: 90, length: 50 };
        let n = Message::Request(req).serialize(&mut buf, &Default::default).unwrap();
        theirs.write_all(&buf[..n]).await.unwrap();

        let piece = loop {
            let n = theirs.read(&mut buf).await.unwrap();
            assert_ne!(n, 0, "the responder closed the connection instead of answering");
            acc.extend_from_slice(&buf[..n]);
            if let Ok((Message::Piece(p), used)) = Message::deserialize(&acc, &[]) {
                // No public accessor for the block, so read it back out of the
                // wire form: index, begin, then the data.
                let mut tmp = vec![0u8; p.len() + 8];
                let written = p.serialize_unchecked_len(&mut tmp);
                let data = tmp[8..written].to_vec();
                let (index, begin) = (p.index, p.begin);
                acc.drain(..used);
                break (index, begin, data);
            }
        };

        assert_eq!(piece.0, 0);
        assert_eq!(piece.1, 90);
        // Bytes 90..100 from the file, then 28 zeros of padding (which no
        // server is asked for), then bytes 128..140.
        let mut want = content[90..100].to_vec();
        want.extend(std::iter::repeat_n(0u8, 28));
        want.extend_from_slice(&content[128..140]);
        assert_eq!(piece.2, want, "the assembled chunk is wrong");

        drop(theirs);
        let _ = task.await;
    }
}
