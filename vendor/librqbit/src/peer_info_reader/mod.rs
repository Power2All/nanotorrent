use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use bencode::from_bytes;
use buffers::{ByteBuf, ByteBufOwned};
use bytes::Bytes;
use librqbit_core::{
    constants::CHUNK_SIZE,
    hash_id::Id20,
    lengths::{ChunkInfo, last_element_size},
    torrent_metainfo::TorrentMetaV1Info,
};
use parking_lot::{Mutex, RwLock};
use peer_binary_protocol::{
    Handshake, Message,
    extended::{
        ExtendedMessage,
        handshake::ExtendedHandshake,
        ut_metadata::{UtMetadata, UtMetadataData},
    },
};
use sha1w::{ISha1, Sha1};
use tokio::sync::mpsc::UnboundedSender;
use tracing::trace;

use crate::{
    peer_connection::{
        PeerConnection, PeerConnectionHandler, PeerConnectionOptions, WriterRequest,
    },
    spawn_utils::BlockingSpawner,
    stream_connect::{ConnectionKind, StreamConnector},
};

pub(crate) async fn read_metainfo_from_peer(
    addr: SocketAddr,
    peer_id: Id20,
    info_hash: Id20,
    peer_connection_options: Option<PeerConnectionOptions>,
    spawner: BlockingSpawner,
    connector: Arc<StreamConnector>,
    client_name_and_version: String,
    // NanoTorrent seam, see MetadataInterceptor. None = upstream behaviour.
    interceptor: Option<Arc<dyn crate::piece_verify::MetadataInterceptor>>,
) -> anyhow::Result<TorrentAndInfoBytes> {
    let (result_tx, result_rx) = tokio::sync::oneshot::channel::<
        Result<(TorrentMetaV1Info<ByteBufOwned>, ByteBufOwned), bencode::DeserializeError>,
    >();
    let (writer_tx, writer_rx) = tokio::sync::mpsc::unbounded_channel::<WriterRequest>();
    let handler = Handler {
        addr,
        info_hash,
        writer_tx,
        result_tx: Mutex::new(Some(result_tx)),
        locked: RwLock::new(None),
        client_name_and_version,
        interceptor,
        pending_info: Mutex::new(None),
    };
    let connection = PeerConnection::new(
        addr,
        info_hash,
        peer_id,
        handler,
        peer_connection_options,
        spawner,
        connector,
    );

    let result_reader = result_rx;
    let (_, brx) = tokio::sync::broadcast::channel(1);
    let connection_runner = async move { connection.manage_peer_outgoing(writer_rx, brx).await };

    tokio::select! {
        result = result_reader => Ok(result??),
        whatever = connection_runner => match whatever {
            Ok(_) => anyhow::bail!("connection runner completed first"),
            Err(e) => Err(e.into())
        }
    }
}

#[derive(Default)]
struct HandlerLocked {
    metadata_size: u32,
    total_pieces: usize,
    buffer: Vec<u8>,
    received_pieces: Vec<bool>,
}

impl HandlerLocked {
    fn new(metadata_size: u32) -> anyhow::Result<Self> {
        if metadata_size > 32 * 1024 * 1024 {
            anyhow::bail!("metadata size {} is too big", metadata_size);
        }
        let buffer = vec![0u8; metadata_size as usize];
        let total_pieces: usize = (metadata_size as u64)
            .div_ceil(CHUNK_SIZE as u64)
            .try_into()?;
        let received_pieces = vec![false; total_pieces];
        Ok(Self {
            metadata_size,
            received_pieces,
            buffer,
            total_pieces,
        })
    }
    fn piece_size(&self, index: u32) -> usize {
        if index as usize == self.total_pieces - 1 {
            last_element_size(self.metadata_size as u64, CHUNK_SIZE as u64)
                .try_into()
                .unwrap()
        } else {
            CHUNK_SIZE as usize
        }
    }
    fn record_piece(
        &mut self,
        d: &UtMetadataData<ByteBuf>,
        info_hash: &Id20,
    ) -> anyhow::Result<bool> {
        let piece = d.piece();
        if piece as usize >= self.total_pieces {
            anyhow::bail!("wrong index");
        }
        let offset = (piece * CHUNK_SIZE) as usize;
        let size = self.piece_size(piece);
        if d.len() != size {
            anyhow::bail!(
                "expected length of piece {} to be {}, but got {}",
                piece,
                size,
                d.len()
            );
        }
        if self.received_pieces[piece as usize] {
            anyhow::bail!("already received piece {}", piece);
        }
        d.copy_to_slice(&mut self.buffer[offset..offset + d.len()]);
        self.received_pieces[piece as usize] = true;

        // NanoTorrent: the integrity check moved to the caller, which is the
        // only place that knows whether SHA-1 is even the right function -
        // a v2-only torrent's info hash is a truncated SHA-256.
        let _ = info_hash;
        Ok(self.received_pieces.iter().all(|p| *p))
    }
}

pub type TorrentAndInfoBytes = (TorrentMetaV1Info<ByteBufOwned>, ByteBufOwned);

struct Handler {
    addr: SocketAddr,
    info_hash: Id20,
    writer_tx: UnboundedSender<WriterRequest>,
    result_tx: Mutex<
        Option<
            tokio::sync::oneshot::Sender<Result<TorrentAndInfoBytes, bencode::DeserializeError>>,
        >,
    >,
    locked: RwLock<Option<HandlerLocked>>,
    client_name_and_version: String,
    // NanoTorrent seam.
    interceptor: Option<Arc<dyn crate::piece_verify::MetadataInterceptor>>,
    // The assembled info dict, held while the hash exchange finishes.
    pending_info: Mutex<Option<Bytes>>,
}

impl Handler {
    /// NanoTorrent: parse the (possibly substituted) info dict and hand it to
    /// the waiting caller. The bytes RETURNED are the originals - they are
    /// what peers verify against the info hash, and what gets persisted.
    fn finish(&self, buf: Bytes) -> anyhow::Result<()> {
        let to_parse = match self.interceptor.as_ref() {
            Some(i) => Bytes::from(i.substitute_info(&buf)?),
            None => buf.clone(),
        };
        let info = from_bytes::<TorrentMetaV1Info<ByteBuf>>(&to_parse)
            .map(|i| {
                use clone_to_owned::CloneToOwned;
                i.clone_to_owned(Some(&to_parse))
            })
            .map_err(|e| {
                trace!("error deserializing TorrentMetaV1Info: {e:#}");
                e.into_kind()
            })
            .map(|i| (i, ByteBufOwned(buf)));

        self.result_tx
            .lock()
            .take()
            .ok_or_else(|| anyhow::anyhow!("oneshot is consumed"))?
            .send(info)
            .map_err(|_| anyhow::anyhow!("torrent info deserialized, but consumer closed"))
    }
}

impl PeerConnectionHandler for Handler {
    fn should_send_bitfield(&self) -> bool {
        false
    }

    fn serialize_bitfield_message_to_buf(&self, _buf: &mut [u8]) -> anyhow::Result<usize> {
        Ok(0)
    }

    fn on_handshake(&self, handshake: Handshake, _kind: ConnectionKind) -> anyhow::Result<()> {
        if !handshake.supports_extended() {
            anyhow::bail!(
                "this peer does not support extended handshaking, which is a prerequisite to download metadata"
            )
        }
        Ok(())
    }

    async fn on_received_message(&self, msg: Message<'_>) -> anyhow::Result<()> {
        trace!("{}: received message: {:?}", self.addr, msg);

        match msg {
            Message::Extended(ExtendedMessage::UtMetadata(UtMetadata::Data(utdata))) => {
                let piece_ready = self
                    .locked
                    .write()
                    .as_mut()
                    .unwrap()
                    .record_piece(&utdata, &self.info_hash)?;
                if !piece_ready {
                    return Ok(());
                }
                let buf = Bytes::from(self.locked.write().take().unwrap().buffer);

                // Integrity. SHA-1 unless the interceptor knows better.
                let ok = match self.interceptor.as_ref() {
                    Some(i) => i.verify_info(&buf, self.info_hash),
                    None => {
                        let mut hash = Sha1::new();
                        hash.update(&buf);
                        hash.finish() == self.info_hash.0
                    }
                };
                if !ok {
                    anyhow::bail!("info checksum invalid");
                }

                // NanoTorrent: a v2 magnet needs `piece layers`, which are not
                // in the info dict. Ask this peer for them before finishing.
                let requests = match self.interceptor.as_ref() {
                    Some(i) => i.hash_requests(&buf)?,
                    None => Vec::new(),
                };
                if requests.is_empty() {
                    return self.finish(buf);
                }
                trace!(count = requests.len(), "requesting piece layers");
                *self.pending_info.lock() = Some(buf);
                for r in requests {
                    self.writer_tx
                        .send(WriterRequest::Message(Message::HashRequest(r)))?;
                }
                Ok(())
            }
            Message::Hashes(h) => {
                let Some(interceptor) = self.interceptor.as_ref() else {
                    return Ok(());
                };
                let hashes: Vec<[u8; 32]> = h.iter_hashes().collect();
                if !interceptor.on_hashes(&h.request, &hashes)? {
                    return Ok(());
                }
                let buf = self
                    .pending_info
                    .lock()
                    .take()
                    .context("bug: hashes completed with no pending info dict")?;
                self.finish(buf)
            }
            Message::HashReject(_) => {
                anyhow::bail!("peer rejected our hash request")
            }
            _ => Ok(()),
        }
    }

    fn on_uploaded_bytes(&self, _bytes: u32) {}

    fn read_chunk(&self, _chunk: &ChunkInfo, _buf: &mut [u8]) -> anyhow::Result<()> {
        anyhow::bail!("the peer is not supposed to be requesting chunks")
    }

    fn on_extended_handshake(
        &self,
        extended_handshake: &ExtendedHandshake<ByteBuf>,
    ) -> anyhow::Result<()> {
        let metadata_size = match extended_handshake.metadata_size {
            Some(metadata_size) => metadata_size,
            None => anyhow::bail!("peer does not have metadata_size"),
        };

        if extended_handshake.m.ut_metadata.is_none() {
            anyhow::bail!("peer does not support ut_metadata");
        }

        self.writer_tx
            .send(WriterRequest::Message(Message::Unchoke))?;
        self.writer_tx
            .send(WriterRequest::Message(Message::Interested))?;

        let inner = HandlerLocked::new(metadata_size)?;
        let total_pieces = inner.total_pieces;

        self.locked.write().replace(inner);

        for i in 0..total_pieces {
            self.writer_tx
                .send(WriterRequest::Message(Message::Extended(
                    ExtendedMessage::UtMetadata(UtMetadata::Request(i.try_into()?)),
                )))?;
        }
        Ok(())
    }

    fn should_transmit_have(&self, _id: librqbit_core::lengths::ValidPieceIndex) -> bool {
        false
    }

    fn client_name_and_version(&self) -> &str {
        &self.client_name_and_version
    }
}
