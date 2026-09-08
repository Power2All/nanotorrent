//! NanoTorrent seam: pluggable piece verification.
//!
//! Exists for BitTorrent v2 (BEP 52), where a piece hash is the root of a
//! SHA-256 merkle subtree over the piece's 16 KiB blocks rather than a hash of
//! the piece's bytes. That cannot be expressed by swapping the hash function,
//! because the comparison target is not in the `pieces` blob either - so the
//! engine hands out the accumulator and asks the answer, and the embedder
//! supplies both.
//!
//! When no verifier is installed the engine uses its own SHA-1 path unchanged.

/// Supplies a hasher for each piece of one torrent.
pub trait PieceVerifier: Send + Sync + std::fmt::Debug {
    /// A fresh hasher for `piece_index`, or `None` if this verifier does not
    /// know that piece - the engine then falls back to its own SHA-1 check.
    fn hasher(&self, piece_index: u32) -> Option<Box<dyn PieceHasher>>;
}

/// Takes over metadata handling for a torrent the engine cannot model itself.
///
/// Used for BitTorrent v2 magnets, where the info dict is not a v1 dict, its
/// hash is not a SHA-1, and the piece hashes are not in it at all.
pub trait MetadataInterceptor: Send + Sync + std::fmt::Debug {
    /// Does this info dict match the torrent's info hash? The engine's own
    /// check is SHA-1, which is the wrong function for a v2 torrent.
    fn verify_info(&self, info_bytes: &[u8], info_hash: librqbit_core::hash_id::Id20) -> bool;

    /// What else to ask this peer for before the metadata is usable. Empty
    /// means nothing, and the engine carries on as it always did.
    fn hash_requests(
        &self,
        info_bytes: &[u8],
    ) -> anyhow::Result<Vec<peer_binary_protocol::HashRequest>>;

    /// One `hashes` answer. Returns true once everything asked for has arrived.
    fn on_hashes(
        &self,
        request: &peer_binary_protocol::HashRequest,
        hashes: &[[u8; 32]],
    ) -> anyhow::Result<bool>;

    /// The bytes the engine should parse as the info dict. The ORIGINAL bytes
    /// are still what gets served to peers and persisted - only the model the
    /// engine drives is substituted.
    fn substitute_info(&self, info_bytes: &[u8]) -> anyhow::Result<Vec<u8>>;
}

/// Answers BEP 52 `hash request` messages for one torrent.
///
/// The other half of [`MetadataInterceptor`]: that one ASKS for hashes when
/// resolving a v2 magnet, this one ANSWERS when a peer asks us. Separate
/// traits because they are needed at different times and by different
/// torrents - a torrent added from a `.torrent` never asks, and only a torrent
/// whose layers we hold can answer.
///
/// Installing one is also what makes the engine advertise v2 in its
/// handshake: the bit says "I can serve these", so it must not be set by a
/// torrent that has nothing to serve.
pub trait HashProvider: Send + Sync + std::fmt::Debug {
    /// The hashes satisfying `request`, base layer first and then the proof
    /// layers, or `None` to reject it.
    ///
    /// Returning `None` is normal and not an error: it is the honest answer
    /// for a file we do not have the layers for, and the caller turns it into
    /// a `hash reject`, which is what the BEP asks for.
    fn hashes_for(
        &self,
        request: &peer_binary_protocol::HashRequest,
    ) -> Option<Vec<[u8; 32]>>;
}

/// Accumulates one piece's bytes and answers whether they are correct.
///
/// The engine feeds the piece in order and in arbitrary-sized slices. An
/// implementation that only covers part of the piece (v2's last piece of a
/// file is shorter than the piece length, the rest being alignment padding)
/// is expected to ignore the remainder itself.
pub trait PieceHasher: Send {
    fn update(&mut self, buf: &[u8]);
    /// True when the bytes fed in match this piece's expected hash.
    fn verify(self: Box<Self>) -> bool;
}
