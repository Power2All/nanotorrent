//! MSE / PE (Message Stream Encryption) for peer connections, injected through
//! the librqbit stream seams (see vendor/librqbit/PATCHES.md, patches 0003 and
//! 0005).
//!
//! - Outgoing: the `StreamTransform` seam runs the *initiator* handshake
//!   (`MseTransform`).
//! - Incoming: the `IncomingStreamTransform` seam (accept path) runs the
//!   *responder* handshake (`IncomingMseTransform`), first peeking whether the
//!   peer is speaking plaintext BitTorrent or MSE.
//!
//! Both wrap the peer socket in RC4 once negotiated.
//!
//! Mode: RC4 is **required** for encrypted connections. Outgoing we advertise
//! only RC4 in `crypto_provide`; incoming we insist the peer offered RC4. A
//! peer that will not do RC4 is dropped - that is the point of a "require
//! encryption" setting.

use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{bail, Context as _, Result};
use librqbit::{BoxAsyncRead, BoxAsyncWrite, Id20, IncomingStreamTransform, StreamTransform};
use num_bigint::BigUint;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

/// The 768-bit MSE Diffie-Hellman prime (`P`), generator `G = 2`. This is the
/// fixed prime from the MSE spec, shared by libtorrent/Vuze/qBittorrent.
const P_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1\
29024E088A67CC74020BBEA63B139B22514A08798E3404DD\
EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245\
E485B576625E7EC6F44C42E9A63A36210000000000090563";

const DH_LEN: usize = 96; // 768 bits
const VC: [u8; 8] = [0u8; 8]; // verification constant
const CRYPTO_RC4: [u8; 4] = [0, 0, 0, 2]; // crypto_provide / crypto_select bit for RC4
const MAX_PAD: usize = 512;

fn hex_to_bytes(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let val = |c: u8| -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => 0,
        }
    };
    let mut i = 0;
    while i + 1 < bytes.len() {
        out.push((val(bytes[i]) << 4) | val(bytes[i + 1]));
        i += 2;
    }
    out
}

fn dh_prime() -> BigUint {
    BigUint::from_bytes_be(&hex_to_bytes(P_HEX))
}

/// Left-pad a big-endian number to exactly `DH_LEN` bytes.
fn to_dh_bytes(n: &BigUint) -> [u8; DH_LEN] {
    let b = n.to_bytes_be();
    let mut out = [0u8; DH_LEN];
    let start = DH_LEN - b.len().min(DH_LEN);
    out[start..].copy_from_slice(&b[b.len().saturating_sub(DH_LEN)..]);
    out
}

/// SHA1 over the concatenation of `parts`.
fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    let mut h = Sha1::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// RC4 stream cipher. Encryption and decryption are the same XOR operation.
struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    fn new(key: &[u8]) -> Self {
        let mut s = [0u8; 256];
        for (k, slot) in s.iter_mut().enumerate() {
            *slot = k as u8;
        }
        let mut j = 0u8;
        for k in 0..256 {
            j = j
                .wrapping_add(s[k])
                .wrapping_add(key[k % key.len()]);
            s.swap(k, j as usize);
        }
        Rc4 { s, i: 0, j: 0 }
    }

    #[inline]
    fn next_byte(&mut self) -> u8 {
        self.i = self.i.wrapping_add(1);
        self.j = self.j.wrapping_add(self.s[self.i as usize]);
        self.s.swap(self.i as usize, self.j as usize);
        let t = self.s[self.i as usize].wrapping_add(self.s[self.j as usize]);
        self.s[t as usize]
    }

    /// XOR `data` in place with the keystream.
    fn apply(&mut self, data: &mut [u8]) {
        for b in data.iter_mut() {
            *b ^= self.next_byte();
        }
    }

    /// Discard `n` keystream bytes (MSE requires discarding the first 1024).
    fn discard(&mut self, n: usize) {
        for _ in 0..n {
            self.next_byte();
        }
    }
}

/// Perform the MSE initiator handshake over the raw stream halves. On success
/// returns `(encrypt, decrypt)` RC4 ciphers positioned at the start of the
/// payload (i.e. ready for the BitTorrent handshake).
async fn client_handshake<R, W>(
    read: &mut R,
    write: &mut W,
    info_hash: [u8; 20],
) -> Result<(Rc4, Rc4)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let prime = dh_prime();
    let g = BigUint::from(2u32);

    // Private key Xa (160 random bits), public Ya = G^Xa mod P.
    let mut xa_bytes = [0u8; 20];
    rand::fill(&mut xa_bytes[..]);
    let xa = BigUint::from_bytes_be(&xa_bytes);
    let ya = to_dh_bytes(&g.modpow(&xa, &prime));

    // Step 1: send Ya (no padding).
    write.write_all(&ya).await.context("sending DH public key")?;
    write.flush().await.ok();

    // Step 2: receive Yb (exactly 96 bytes; PadB, if any, follows and is
    // consumed during VC resynchronisation below).
    let mut yb = [0u8; DH_LEN];
    read.read_exact(&mut yb)
        .await
        .context("reading peer DH public key")?;
    let s = to_dh_bytes(&BigUint::from_bytes_be(&yb).modpow(&xa, &prime));

    // Key derivation.
    let req1 = sha1(&[b"req1", &s]);
    let req2 = sha1(&[b"req2", &info_hash]);
    let req3 = sha1(&[b"req3", &s]);
    let mut req2_3 = [0u8; 20];
    for i in 0..20 {
        req2_3[i] = req2[i] ^ req3[i];
    }
    let mut enc = Rc4::new(&sha1(&[b"keyA", &s, &info_hash]));
    enc.discard(1024);
    let mut dec = Rc4::new(&sha1(&[b"keyB", &s, &info_hash]));
    dec.discard(1024);

    // Step 3: HASH('req1',S) || (HASH('req2',SKEY) xor HASH('req3',S)) ||
    // ENCRYPT(VC || crypto_provide || len(PadC)=0 || len(IA)=0).
    let mut payload = Vec::with_capacity(16);
    payload.extend_from_slice(&VC);
    payload.extend_from_slice(&CRYPTO_RC4);
    payload.extend_from_slice(&[0, 0]); // len(PadC) = 0
    payload.extend_from_slice(&[0, 0]); // len(IA) = 0
    enc.apply(&mut payload);

    let mut out = Vec::with_capacity(40 + payload.len());
    out.extend_from_slice(&req1);
    out.extend_from_slice(&req2_3);
    out.extend_from_slice(&payload);
    write.write_all(&out).await.context("sending MSE request")?;
    write.flush().await.ok();

    // Step 4: resynchronise on the peer's encrypted VC. The peer prefixed an
    // unknown amount of PadB before this; the encrypted VC is a known 8-byte
    // pattern (keystream ^ zeros) we scan for.
    let mut enc_vc = VC;
    dec.apply(&mut enc_vc); // advances `dec` past the VC
    sync_on(read, &enc_vc)
        .await
        .context("MSE VC synchronisation")?;

    // crypto_select, len(PadD), PadD - all RC4-encrypted; decrypt to keep the
    // stream in sync and to check the peer chose RC4.
    let mut sel = [0u8; 4];
    read.read_exact(&mut sel).await.context("reading crypto_select")?;
    dec.apply(&mut sel);
    if sel != CRYPTO_RC4 {
        bail!("peer did not select RC4 encryption (crypto_select={sel:?})");
    }
    let mut lp = [0u8; 2];
    read.read_exact(&mut lp).await.context("reading PadD length")?;
    dec.apply(&mut lp);
    let pad_d = u16::from_be_bytes(lp) as usize;
    if pad_d > MAX_PAD {
        bail!("peer PadD too large ({pad_d})");
    }
    if pad_d > 0 {
        let mut pd = vec![0u8; pad_d];
        read.read_exact(&mut pd).await.context("reading PadD")?;
        dec.apply(&mut pd);
    }

    Ok((enc, dec))
}

/// Read one byte at a time until the last 8 bytes seen equal `pat` (the
/// encrypted VC). Bounded by the maximum PadB length so a peer that never
/// sends a matching VC cannot make us read forever.
async fn sync_on<R: AsyncRead + Unpin>(read: &mut R, pat: &[u8; 8]) -> Result<()> {
    let mut window = [0u8; 8];
    let mut filled = 0usize;
    let mut total = 0usize;
    loop {
        let mut b = [0u8; 1];
        read.read_exact(&mut b).await?;
        total += 1;
        if filled < 8 {
            window[filled] = b[0];
            filled += 1;
        } else {
            window.copy_within(1..8, 0);
            window[7] = b[0];
        }
        if filled == 8 && &window == pat {
            return Ok(());
        }
        if total > MAX_PAD + 8 {
            bail!("did not find encrypted VC within PadB bounds");
        }
    }
}

/// Scan byte-by-byte until the last `pat.len()` bytes match `pat`, skipping up
/// to `max` bytes of preceding padding. Used by the responder to skip PadA and
/// land on the `req1` hash.
async fn sync_on_pat<R: AsyncRead + Unpin>(read: &mut R, pat: &[u8], max: usize) -> Result<()> {
    let mut window: Vec<u8> = Vec::with_capacity(pat.len());
    let mut total = 0usize;
    loop {
        let mut b = [0u8; 1];
        read.read_exact(&mut b).await?;
        total += 1;
        if window.len() == pat.len() {
            window.remove(0);
        }
        window.push(b[0]);
        if window == pat {
            return Ok(());
        }
        if total > max + pat.len() {
            bail!("did not find sync pattern within padding bounds");
        }
    }
}

/// Perform the MSE responder handshake. `ya` is the peer's 96-byte DH public
/// key (already read off the wire), `candidates` are the info-hashes of our
/// active torrents - the peer's SKEY picks which one. On success returns
/// `(encrypt, decrypt, ia)` where `ia` is the already-decrypted initial
/// payload that must be replayed to the BitTorrent layer before further reads.
async fn server_handshake<R, W>(
    read: &mut R,
    write: &mut W,
    ya: [u8; DH_LEN],
    candidates: &[[u8; 20]],
) -> Result<(Rc4, Rc4, Vec<u8>)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let prime = dh_prime();
    let g = BigUint::from(2u32);

    // Our private key Xb, public Yb, shared secret S = Ya^Xb mod P.
    let mut xb_bytes = [0u8; 20];
    rand::fill(&mut xb_bytes[..]);
    let xb = BigUint::from_bytes_be(&xb_bytes);
    let yb = to_dh_bytes(&g.modpow(&xb, &prime));
    write.write_all(&yb).await.context("sending DH public key")?;
    write.flush().await.ok();
    let s = to_dh_bytes(&BigUint::from_bytes_be(&ya).modpow(&xb, &prime));

    // Skip PadA and land on HASH('req1', S).
    let req1 = sha1(&[b"req1", &s]);
    sync_on_pat(read, &req1, MAX_PAD)
        .await
        .context("MSE req1 synchronisation")?;

    // HASH('req2',SKEY) xor HASH('req3',S): resolve which torrent this is.
    let mut req2_3 = [0u8; 20];
    read.read_exact(&mut req2_3)
        .await
        .context("reading req2^req3")?;
    let req3 = sha1(&[b"req3", &s]);
    let mut target = [0u8; 20];
    for i in 0..20 {
        target[i] = req2_3[i] ^ req3[i];
    }
    let skey = candidates
        .iter()
        .find(|sk| sha1(&[b"req2", *sk]) == target)
        .copied()
        .context("no active torrent matched the incoming peer's SKEY")?;

    // The initiator encrypts with keyA, so we decrypt its stream with keyA and
    // encrypt ours with keyB.
    let mut dec = Rc4::new(&sha1(&[b"keyA", &s, &skey]));
    dec.discard(1024);
    let mut enc = Rc4::new(&sha1(&[b"keyB", &s, &skey]));
    enc.discard(1024);

    // ENCRYPT(VC || crypto_provide || len(PadC) || PadC || len(IA) || IA).
    let mut vc = [0u8; 8];
    read.read_exact(&mut vc).await.context("reading VC")?;
    dec.apply(&mut vc);
    if vc != VC {
        bail!("MSE VC mismatch - wrong shared secret");
    }
    let mut provide = [0u8; 4];
    read.read_exact(&mut provide)
        .await
        .context("reading crypto_provide")?;
    dec.apply(&mut provide);
    if provide[3] & 0x02 == 0 {
        bail!("incoming peer did not offer RC4 (crypto_provide={provide:?})");
    }
    let mut lp = [0u8; 2];
    read.read_exact(&mut lp).await.context("reading len(PadC)")?;
    dec.apply(&mut lp);
    let pad_c = u16::from_be_bytes(lp) as usize;
    if pad_c > MAX_PAD {
        bail!("PadC too large ({pad_c})");
    }
    if pad_c > 0 {
        let mut pc = vec![0u8; pad_c];
        read.read_exact(&mut pc).await.context("reading PadC")?;
        dec.apply(&mut pc);
    }
    let mut li = [0u8; 2];
    read.read_exact(&mut li).await.context("reading len(IA)")?;
    dec.apply(&mut li);
    let ia_len = u16::from_be_bytes(li) as usize;
    // IA carries the peer's first payload (typically the 68-byte BitTorrent
    // handshake). Cap it so a bogus length can't force a huge allocation.
    if ia_len > 1024 {
        bail!("IA too large ({ia_len})");
    }
    let mut ia = vec![0u8; ia_len];
    if ia_len > 0 {
        read.read_exact(&mut ia).await.context("reading IA")?;
        dec.apply(&mut ia);
    }

    // Respond: ENCRYPT(VC || crypto_select=RC4 || len(PadD)=0).
    let mut resp = Vec::with_capacity(14);
    resp.extend_from_slice(&VC);
    resp.extend_from_slice(&CRYPTO_RC4);
    resp.extend_from_slice(&[0, 0]); // len(PadD) = 0
    enc.apply(&mut resp);
    write
        .write_all(&resp)
        .await
        .context("sending MSE response")?;
    write.flush().await.ok();

    Ok((enc, dec, ia))
}

/// A read half that first replays a buffered prefix, then delegates to the
/// inner reader. Used to hand back bytes we read during detection/handshake
/// (the peeked plaintext header, or the decrypted IA payload).
struct PrefixRead<R> {
    prefix: Vec<u8>,
    pos: usize,
    inner: R,
}

impl<R: AsyncRead + Unpin> AsyncRead for PrefixRead<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let remaining = &this.prefix[this.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

/// A read half that RC4-decrypts everything it reads.
struct EncryptedRead<R> {
    inner: R,
    rc4: Rc4,
}

impl<R: AsyncRead + Unpin> AsyncRead for EncryptedRead<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let start = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let filled = buf.filled_mut();
                this.rc4.apply(&mut filled[start..]);
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

/// A write half that RC4-encrypts everything written. Any encrypted bytes the
/// downstream socket could not accept immediately are stashed in `pending`
/// and drained on the next write/flush/shutdown, so a caller that never calls
/// `flush` (e.g. `write_all` on a raw socket) still makes progress.
struct EncryptedWrite<W> {
    inner: W,
    rc4: Rc4,
    pending: Vec<u8>,
    pstart: usize,
}

impl<W: AsyncWrite + Unpin> EncryptedWrite<W> {
    fn drain(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        while self.pstart < self.pending.len() {
            match Pin::new(&mut self.inner).poll_write(cx, &self.pending[self.pstart..]) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()));
                }
                Poll::Ready(Ok(n)) => self.pstart += n,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        self.pending.clear();
        self.pstart = 0;
        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for EncryptedWrite<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        // Flush any previously stashed ciphertext before encrypting more, so
        // the RC4 stream stays in wire order.
        match this.drain(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }
        let mut tmp = data.to_vec();
        this.rc4.apply(&mut tmp);
        match Pin::new(&mut this.inner).poll_write(cx, &tmp) {
            Poll::Ready(Ok(n)) => {
                if n < tmp.len() {
                    this.pending.extend_from_slice(&tmp[n..]);
                    this.pstart = 0;
                }
                Poll::Ready(Ok(data.len()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            // Socket not ready: the data is encrypted and committed to the
            // RC4 stream, so we must accept it fully and queue it.
            Poll::Pending => {
                this.pending.extend_from_slice(&tmp);
                this.pstart = 0;
                Poll::Ready(Ok(data.len()))
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.drain(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_flush(cx),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.drain(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_shutdown(cx),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Requires RC4 (MSE/PE) encryption on every outgoing peer connection.
#[derive(Debug)]
pub struct MseTransform;

impl StreamTransform for MseTransform {
    fn transform(
        &self,
        _addr: SocketAddr,
        info_hash: Id20,
        mut read: BoxAsyncRead,
        mut write: BoxAsyncWrite,
    ) -> futures::future::BoxFuture<'_, Result<(BoxAsyncRead, BoxAsyncWrite)>> {
        Box::pin(async move {
            let (enc, dec) = client_handshake(&mut read, &mut write, info_hash.0).await?;
            let r: BoxAsyncRead = Box::new(EncryptedRead { inner: read, rc4: dec });
            let w: BoxAsyncWrite = Box::new(EncryptedWrite {
                inner: write,
                rc4: enc,
                pending: Vec::new(),
                pstart: 0,
            });
            Ok((r, w))
        })
    }
}

/// Accepts incoming peers that speak MSE/PE, transparently passing plaintext
/// peers through unless `require` is set (in which case plaintext is dropped).
#[derive(Debug)]
pub struct IncomingMseTransform {
    pub require: bool,
}

/// A plaintext BitTorrent handshake begins with this 20-byte prefix
/// (pstrlen=19 followed by "BitTorrent protocol"). Anything else on an
/// incoming connection is treated as an MSE DH public key.
const BT_PSTR: &[u8; 20] = b"\x13BitTorrent protocol";

impl IncomingStreamTransform for IncomingMseTransform {
    fn transform(
        &self,
        _addr: SocketAddr,
        info_hashes: Vec<Id20>,
        mut read: BoxAsyncRead,
        mut write: BoxAsyncWrite,
    ) -> futures::future::BoxFuture<'_, Result<(BoxAsyncRead, BoxAsyncWrite)>> {
        let require = self.require;
        Box::pin(async move {
            // Peek the first 20 bytes to tell plaintext from encrypted.
            let mut head = [0u8; 20];
            read.read_exact(&mut head)
                .await
                .context("reading incoming header")?;

            if &head == BT_PSTR {
                if require {
                    bail!("plaintext incoming peer rejected (encryption required)");
                }
                // Plaintext: replay the peeked header, otherwise untouched.
                let r: BoxAsyncRead = Box::new(PrefixRead {
                    prefix: head.to_vec(),
                    pos: 0,
                    inner: read,
                });
                return Ok((r, write));
            }

            // Encrypted: `head` is the first 20 bytes of the peer's Ya.
            let mut ya = [0u8; DH_LEN];
            ya[..20].copy_from_slice(&head);
            read.read_exact(&mut ya[20..])
                .await
                .context("reading peer DH public key")?;

            let candidates: Vec<[u8; 20]> = info_hashes.iter().map(|h| h.0).collect();
            let (enc, dec, ia) =
                server_handshake(&mut read, &mut write, ya, &candidates).await?;

            let r: BoxAsyncRead = Box::new(PrefixRead {
                prefix: ia,
                pos: 0,
                inner: EncryptedRead { inner: read, rc4: dec },
            });
            let w: BoxAsyncWrite = Box::new(EncryptedWrite {
                inner: write,
                rc4: enc,
                pending: Vec::new(),
                pstart: 0,
            });
            Ok((r, w))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RC4 test vector from the spec: key "Key" -> keystream applied to
    // "Plaintext" yields BBF316E8D940AF0AD3.
    #[test]
    fn rc4_known_answer() {
        let mut rc4 = Rc4::new(b"Key");
        let mut data = b"Plaintext".to_vec();
        rc4.apply(&mut data);
        assert_eq!(
            data,
            [0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3]
        );
    }

    #[test]
    fn sha1_known_answer() {
        // SHA1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
        assert_eq!(
            sha1(&[b"abc"]),
            [
                0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78,
                0x50, 0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
            ]
        );
    }

    // Two independent DH parties must agree on the shared secret.
    #[test]
    fn dh_shared_secret_agrees() {
        let prime = dh_prime();
        let g = BigUint::from(2u32);
        let xa = BigUint::from_bytes_be(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let xb = BigUint::from_bytes_be(&[10, 9, 8, 7, 6, 5, 4, 3, 2, 1]);
        let ya = g.modpow(&xa, &prime);
        let yb = g.modpow(&xb, &prime);
        let sa = to_dh_bytes(&yb.modpow(&xa, &prime));
        let sb = to_dh_bytes(&ya.modpow(&xb, &prime));
        assert_eq!(sa, sb);
    }

    #[test]
    fn dh_prime_is_768_bits() {
        assert_eq!(hex_to_bytes(P_HEX).len(), DH_LEN);
        assert_eq!(dh_prime().bits(), 768);
    }

    // Full handshake between our initiator and a minimal MSE responder over an
    // in-memory duplex, then a payload byte each way through the RC4 wrappers.
    #[test]
    fn mse_handshake_roundtrip() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let info_hash = [0x42u8; 20];
            let (a, b) = tokio::io::duplex(4096);
            let (mut ar, mut aw) = tokio::io::split(a);

            // Responder side (B) running the peer half of the handshake.
            let responder = tokio::spawn(async move {
                let (mut br, mut bw) = tokio::io::split(b);
                let prime = dh_prime();
                let g = BigUint::from(2u32);

                // Read A's Ya.
                let mut ya = [0u8; DH_LEN];
                br.read_exact(&mut ya).await.unwrap();

                // Send our Yb.
                let mut xb = [0u8; 20];
                rand::fill(&mut xb[..]);
                let xb = BigUint::from_bytes_be(&xb);
                let yb = to_dh_bytes(&g.modpow(&xb, &prime));
                bw.write_all(&yb).await.unwrap();

                let s = to_dh_bytes(&BigUint::from_bytes_be(&ya).modpow(&xb, &prime));
                // A encrypts with keyA, so B decrypts A with keyA and encrypts
                // its own side with keyB.
                let mut dec_a = Rc4::new(&sha1(&[b"keyA", &s, &info_hash]));
                dec_a.discard(1024);
                let mut enc_b = Rc4::new(&sha1(&[b"keyB", &s, &info_hash]));
                enc_b.discard(1024);

                // Read req1 || req2^req3 || ENCRYPT(VC||provide||0||0).
                let mut hashes = [0u8; 40];
                br.read_exact(&mut hashes).await.unwrap();
                let mut block = [0u8; 16];
                br.read_exact(&mut block).await.unwrap();
                dec_a.apply(&mut block);
                assert_eq!(&block[0..8], &VC);
                assert_eq!(&block[8..12], &CRYPTO_RC4);

                // Send ENCRYPT(VC || crypto_select || len(PadD)=0).
                let mut resp = Vec::new();
                resp.extend_from_slice(&VC);
                resp.extend_from_slice(&CRYPTO_RC4);
                resp.extend_from_slice(&[0, 0]);
                enc_b.apply(&mut resp);
                bw.write_all(&resp).await.unwrap();

                // Echo one encrypted payload byte back to A.
                let mut byte = [0u8; 1];
                br.read_exact(&mut byte).await.unwrap();
                dec_a.apply(&mut byte);
                assert_eq!(byte[0], 0x99);
                let mut reply = [0xABu8];
                enc_b.apply(&mut reply);
                bw.write_all(&reply).await.unwrap();
            });

            let (enc, dec) = client_handshake(&mut ar, &mut aw, info_hash).await.unwrap();
            let mut w = EncryptedWrite {
                inner: aw,
                rc4: enc,
                pending: Vec::new(),
                pstart: 0,
            };
            let mut r = EncryptedRead { inner: ar, rc4: dec };

            w.write_all(&[0x99]).await.unwrap();
            w.flush().await.unwrap();
            let mut got = [0u8; 1];
            r.read_exact(&mut got).await.unwrap();
            assert_eq!(got[0], 0xAB);

            responder.await.unwrap();
        });
    }

    // Our production initiator against our production responder: full MSE
    // negotiation (incl. req1 sync + SKEY resolution) then an encrypted
    // payload byte each way through the RC4 wrappers.
    #[test]
    fn initiator_responder_interop() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let info_hash = [0x42u8; 20];
            let (a, b) = tokio::io::duplex(4096);
            let (mut ar, mut aw) = tokio::io::split(a);

            let responder = tokio::spawn(async move {
                let (mut br, mut bw) = tokio::io::split(b);
                // Read the peer's Ya (initiator sends no PadA).
                let mut ya = [0u8; DH_LEN];
                br.read_exact(&mut ya).await.unwrap();
                // A decoy hash plus the real one - SKEY resolution must pick
                // the matching torrent.
                let candidates = [[0x11u8; 20], info_hash, [0x33u8; 20]];
                let (enc, dec, ia) =
                    server_handshake(&mut br, &mut bw, ya, &candidates).await.unwrap();
                assert!(ia.is_empty()); // our initiator sends len(IA)=0
                let mut r = PrefixRead { prefix: ia, pos: 0, inner: EncryptedRead { inner: br, rc4: dec } };
                let mut w = EncryptedWrite { inner: bw, rc4: enc, pending: Vec::new(), pstart: 0 };
                let mut byte = [0u8; 1];
                r.read_exact(&mut byte).await.unwrap();
                assert_eq!(byte[0], 0x99);
                w.write_all(&[0xAB]).await.unwrap();
                w.flush().await.unwrap();
            });

            let (enc, dec) = client_handshake(&mut ar, &mut aw, info_hash).await.unwrap();
            let mut w = EncryptedWrite { inner: aw, rc4: enc, pending: Vec::new(), pstart: 0 };
            let mut r = EncryptedRead { inner: ar, rc4: dec };
            w.write_all(&[0x99]).await.unwrap();
            w.flush().await.unwrap();
            let mut got = [0u8; 1];
            r.read_exact(&mut got).await.unwrap();
            assert_eq!(got[0], 0xAB);

            responder.await.unwrap();
        });
    }

    #[test]
    fn sync_on_pat_skips_padding() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let pat = [0xDE, 0xAD, 0xBE, 0xEF];
            let mut buf = vec![0x00u8; 37]; // 37 bytes of padding
            buf.extend_from_slice(&pat);
            buf.extend_from_slice(&[0x99, 0x98]); // trailing payload
            let mut cursor: &[u8] = &buf;
            sync_on_pat(&mut cursor, &pat, MAX_PAD).await.unwrap();
            // The reader must now be positioned right after the pattern.
            let mut rest = [0u8; 2];
            cursor.read_exact(&mut rest).await.unwrap();
            assert_eq!(rest, [0x99, 0x98]);
        });
    }

    #[test]
    fn plaintext_prefix_is_the_bt_pstr() {
        // The detection constant must equal a real BitTorrent handshake prefix.
        assert_eq!(BT_PSTR.len(), 20);
        assert_eq!(BT_PSTR[0], 19);
        assert_eq!(&BT_PSTR[1..], b"BitTorrent protocol");
    }
}
