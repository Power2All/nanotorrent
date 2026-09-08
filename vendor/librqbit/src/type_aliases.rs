use std::net::SocketAddr;

use futures::stream::BoxStream;
use tokio::io::AsyncWrite;

use crate::{file_info::FileInfo, storage::TorrentStorage, vectored_traits::AsyncReadVectored};

// NOTE: Msb0 is used because that's what bittorrent protocol uses for bitfield.
// Don't change to Lsb0 even though it might be a bit faster (in theory) on LE architectures.
pub type BS = bitvec::slice::BitSlice<u8, bitvec::order::Msb0>;
pub type BF = bitvec::boxed::BitBox<u8, bitvec::order::Msb0>;

pub type PeerHandle = SocketAddr;

/// NanoTorrent addition: where a peer address came from.
///
/// The discovery streams are merged into one before anybody reads them, so
/// without carrying this the answer to "how many peers did the DHT actually
/// find" is gone by the time a peer is added. Travels with the address rather
/// than being looked up later because after the merge there is nothing left to
/// look it up in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PeerSource {
    Dht,
    Tracker,
    /// Local service discovery.
    Lsd,
    /// Peer exchange (BEP 11).
    Pex,
    /// Named when the torrent was added, or restored with it.
    Initial,
    /// The peer connected to us; we never discovered it.
    Incoming,
    /// Added by hand through the API.
    Manual,
}

impl PeerSource {
    /// Stable identifiers, for a caller that has to name these outside Rust.
    pub fn as_str(&self) -> &'static str {
        match self {
            PeerSource::Dht => "dht",
            PeerSource::Tracker => "tracker",
            PeerSource::Lsd => "lsd",
            PeerSource::Pex => "pex",
            PeerSource::Initial => "initial",
            PeerSource::Incoming => "incoming",
            PeerSource::Manual => "manual",
        }
    }
}

pub type PeerStream = BoxStream<'static, (SocketAddr, PeerSource)>;
pub type FileInfos = Vec<FileInfo>;
pub(crate) type FileStorage = Box<dyn TorrentStorage>;
pub(crate) type FilePriorities = Vec<usize>;

pub(crate) type BoxAsyncReadVectored = Box<dyn AsyncReadVectored + Unpin + Send + 'static>;
// NanoTorrent seam: public because it appears in the StreamTransform traits.
pub type BoxAsyncWrite = Box<dyn AsyncWrite + Unpin + Send + 'static>;
