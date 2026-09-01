// BitTorrent peer protocol implementation: parsing, serialization etc.
//
// Can be used outside of librqbit.

mod double_buf;
pub mod extended;

use std::hint::unreachable_unchecked;

use buffers::{ByteBuf, ByteBufOwned};
use byteorder::{BE, ByteOrder};
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use extended::PeerExtendedMessageIds;
use librqbit_core::{constants::CHUNK_SIZE, hash_id::Id20, lengths::ChunkInfo};
use serde_derive::{Deserialize, Serialize};

pub use crate::double_buf::DoubleBufHelper;

use self::extended::ExtendedMessage;

const INTEGER_LEN: usize = 4;
const MSGID_LEN: usize = 1;
const PREAMBLE_LEN: usize = INTEGER_LEN + MSGID_LEN;
const PIECE_MESSAGE_PREAMBLE_LEN: usize = PREAMBLE_LEN + INTEGER_LEN * 2;
pub const PIECE_MESSAGE_DEFAULT_LEN: usize = PIECE_MESSAGE_PREAMBLE_LEN + CHUNK_SIZE as usize;

// extended message ut_metadata request is the largest known message.
const MAX_MSG_LEN_LEN_JUST_IN_CASE_EXTRA: usize = 64;
pub const MAX_MSG_LEN: usize = PREAMBLE_LEN
    + 1
    + b"d8:msg_typei1e5:piecei42e10:total_sizei16384ee".len()
    + CHUNK_SIZE as usize
    + MAX_MSG_LEN_LEN_JUST_IN_CASE_EXTRA;

const PSTR_BT1: &str = "BitTorrent protocol";

type MsgId = u8;

const MSGID_CHOKE: MsgId = 0;
const MSGID_UNCHOKE: MsgId = 1;
const MSGID_INTERESTED: MsgId = 2;
const MSGID_NOT_INTERESTED: MsgId = 3;
const MSGID_HAVE: MsgId = 4;
const MSGID_BITFIELD: MsgId = 5;
const MSGID_REQUEST: MsgId = 6;
const MSGID_PIECE: MsgId = 7;
const MSGID_CANCEL: MsgId = 8;
const MSGID_EXTENDED: MsgId = 20;
// BEP 6 (fast extension). NanoTorrent addition.
const MSGID_SUGGEST_PIECE: MsgId = 0x0D;
const MSGID_HAVE_ALL: MsgId = 0x0E;
const MSGID_HAVE_NONE: MsgId = 0x0F;
const MSGID_REJECT_REQUEST: MsgId = 0x10;
const MSGID_ALLOWED_FAST: MsgId = 0x11;
// BEP 52 (BitTorrent v2) hash exchange. NanoTorrent addition.
const MSGID_HASH_REQUEST: MsgId = 21;
const MSGID_HASHES: MsgId = 22;
const MSGID_HASH_REJECT: MsgId = 23;

pub const EXTENDED_UT_METADATA_KEY: &[u8] = b"ut_metadata";
pub const MY_EXTENDED_UT_METADATA: u8 = 3;

pub const EXTENDED_UT_PEX_KEY: &[u8] = b"ut_pex";
pub const MY_EXTENDED_UT_PEX: u8 = 1;

#[derive(Clone, Copy)]
pub struct MsgIdDebug(MsgId);
impl MsgIdDebug {
    const fn name(&self) -> Option<&'static str> {
        let n = match self.0 {
            MSGID_CHOKE => "choke",
            MSGID_UNCHOKE => "unchoke",
            MSGID_INTERESTED => "interested",
            MSGID_NOT_INTERESTED => "not_interested",
            MSGID_HAVE => "have",
            MSGID_BITFIELD => "bitfield",
            MSGID_REQUEST => "request",
            MSGID_PIECE => "piece",
            MSGID_CANCEL => "cancel",
            MSGID_EXTENDED => "extended",
            MSGID_SUGGEST_PIECE => "suggest_piece",
            MSGID_HAVE_ALL => "have_all",
            MSGID_HAVE_NONE => "have_none",
            MSGID_REJECT_REQUEST => "reject_request",
            MSGID_ALLOWED_FAST => "allowed_fast",
            MSGID_HASH_REQUEST => "hash_request",
            MSGID_HASHES => "hashes",
            MSGID_HASH_REJECT => "hash_reject",
            _ => return None,
        };
        Some(n)
    }
}
impl core::fmt::Debug for MsgIdDebug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "<unknown msg_id {}>", self.0),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum MessageDeserializeError {
    #[error("not enough data (msgid={1:?}): expected at least {0} more bytes")]
    NotEnoughData(usize, Option<MsgIdDebug>),
    #[error("need a contiguous input to deserialize")]
    NeedContiguous,
    #[error("unsupported message id {0}")]
    UnsupportedMessageId(u8),
    #[error(transparent)]
    Bencode(#[from] bencode::DeserializeError),
    #[error("incorrect message length msg_id={msg_id:?}, expected={expected}, received={received}")]
    IncorrectMsgLen {
        received: u32,
        expected: u32,
        msg_id: MsgIdDebug,
    },
    #[error("ut_metadata:data received {received_len} >= total_size is {total_size}")]
    UtMetadataBufLargerThanTotalSize { total_size: u32, received_len: u32 },
    #[error("ut_metadata:data length must be <= {CHUNK_SIZE} but received {0} bytes")]
    UtMetadataTooLarge(u32),
    #[error("ut_metadata: trailing bytes when decoding")]
    UtMetadataTrailingBytes,
    #[error("ut_metadata: missing total_size")]
    UtMetadataMissingTotalSize,
    #[error("ut_metadata: unrecognized message type: {0}")]
    UtMetadataTypeUnknown(u32),
    #[error("ut_metadata: received piece {received_piece} > total pieces {total_pieces}")]
    UtMetadataPieceOutOfBounds {
        total_pieces: u32,
        received_piece: u32,
    },
    #[error("ut_metadata: expected size {expected_size} != received size {received_size}")]
    UtMetadataSizeMismatch {
        expected_size: u32,
        received_size: u32,
    },
    #[error("pstr doesn't match {PSTR_BT1:?}")]
    HandshakePstrWrongContent,
    #[error("pstr should be 19 bytes long but got {0}")]
    HandshakePstrWrongLength(u8),
}

pub fn serialize_piece_preamble(chunk: &ChunkInfo, mut buf: &mut [u8]) -> usize {
    let len_prefix = MSGID_LEN as u32 + INTEGER_LEN as u32 * 2 + chunk.size;
    BE::write_u32(&mut buf[0..4], len_prefix);
    buf[4] = MSGID_PIECE;

    buf = &mut buf[5..];
    BE::write_u32(&mut buf[0..4], chunk.piece_index.get());
    BE::write_u32(&mut buf[4..8], chunk.offset);

    PIECE_MESSAGE_PREAMBLE_LEN
}

pub struct Piece<B> {
    pub index: u32,
    pub begin: u32,
    block_0: B,
    block_1: B,
}

impl<B: AsRef<[u8]>> std::fmt::Debug for Piece<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Piece")
            .field("index", &self.index)
            .field("begin", &self.begin)
            .field("len", &self.len())
            .field("len_0", &self.block_0.as_ref().len())
            .field("len_1", &self.block_1.as_ref().len())
            .finish_non_exhaustive()
    }
}

impl CloneToOwned for Piece<ByteBuf<'_>> {
    type Target = Piece<ByteBufOwned>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        Piece {
            index: self.index,
            begin: self.begin,
            block_0: self.block_0.clone_to_owned(within_buffer),
            block_1: self.block_1.clone_to_owned(within_buffer),
        }
    }
}

impl<B: AsRef<[u8]>> Piece<B> {
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.block_0.as_ref().len() + self.block_1.as_ref().len()
    }

    pub fn serialize_unchecked_len(&self, mut buf: &mut [u8]) -> usize {
        buf[0..4].copy_from_slice(&self.index.to_be_bytes());
        buf[4..8].copy_from_slice(&self.begin.to_be_bytes());
        buf = &mut buf[8..];

        let b0 = self.block_0.as_ref();
        let b1 = self.block_1.as_ref();

        buf[..b0.len()].copy_from_slice(b0);
        buf = &mut buf[b0.len()..];
        buf[..b1.len()].copy_from_slice(b1);
        8 + b0.len() + b1.len()
    }
}

impl Piece<ByteBufOwned> {
    pub fn as_borrowed(&self) -> Piece<ByteBuf<'_>> {
        Piece {
            index: self.index,
            begin: self.begin,
            block_0: self.block_0.as_ref().into(),
            block_1: self.block_1.as_ref().into(),
        }
    }
}

impl<'a> Piece<ByteBuf<'a>> {
    pub fn data(&self) -> (&'a [u8], &'a [u8]) {
        (self.block_0.0, self.block_1.0)
    }

    pub fn from_data(index: u32, begin: u32, block: &'a [u8]) -> Self {
        Piece {
            index,
            begin,
            block_0: ByteBuf(block),
            block_1: ByteBuf(&[]),
        }
    }
}

/// BEP 52 hash request / reject payload - 48 bytes on the wire.
///
/// The BEP describes the fields but not their encoding; this is libtorrent's
/// layout, which is the one in use on the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HashRequest {
    /// The merkle root of the file the hashes belong to.
    pub pieces_root: [u8; 32],
    /// How many layers above the leaves the requested hashes sit. 0 is the
    /// leaf (16 KiB block) layer.
    pub base_layer: u32,
    /// Offset, in hashes, of the first requested hash within `base_layer`.
    /// MUST be a multiple of `length`.
    pub index: u32,
    /// How many hashes to return from `base_layer`. MUST be a power of two and
    /// at least 2; SHOULD NOT exceed 512.
    pub length: u32,
    /// How many ancestor ("uncle") hashes to include, so the receiver can walk
    /// up to `pieces_root`.
    pub proof_layers: u32,
}

impl HashRequest {
    /// pieces_root + four u32s.
    pub const WIRE_LEN: usize = 32 + INTEGER_LEN * 4;

    fn serialize_unchecked_len(&self, out: &mut [u8]) -> usize {
        out[..32].copy_from_slice(&self.pieces_root);
        BE::write_u32(&mut out[32..36], self.base_layer);
        BE::write_u32(&mut out[36..40], self.index);
        BE::write_u32(&mut out[40..44], self.length);
        BE::write_u32(&mut out[44..48], self.proof_layers);
        Self::WIRE_LEN
    }

    fn parse(b: &[u8; HashRequest::WIRE_LEN]) -> Self {
        Self {
            pieces_root: b[..32].try_into().unwrap(),
            base_layer: BE::read_u32(&b[32..36]),
            index: BE::read_u32(&b[36..40]),
            length: BE::read_u32(&b[40..44]),
            proof_layers: BE::read_u32(&b[44..48]),
        }
    }
}

/// A BEP 52 `hashes` message: the request it answers, plus the hashes.
///
/// `hashes` is a flat run of 32-byte SHA-256 values - the base layer first,
/// then the proof layers, ending with the uncle hash closest to the root.
#[derive(Debug)]
pub struct Hashes<'a> {
    pub request: HashRequest,
    pub hashes: ByteBuf<'a>,
}

impl Hashes<'_> {
    /// The hashes as 32-byte chunks. Any trailing partial hash is dropped -
    /// `deserialize` refuses those, so this cannot silently lose data.
    pub fn iter_hashes(&self) -> impl Iterator<Item = [u8; 32]> + '_ {
        self.hashes
            .as_ref()
            .chunks_exact(32)
            .map(|c| <[u8; 32]>::try_from(c).unwrap())
    }
}

#[derive(Debug)]
pub enum Message<'a> {
    Request(Request),
    Cancel(Request),
    Bitfield(ByteBuf<'a>),
    KeepAlive,
    Have(u32),
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Piece(Piece<ByteBuf<'a>>),
    Extended(ExtendedMessage<ByteBuf<'a>>),
    /// BEP 6 (fast extension). NanoTorrent addition.
    SuggestPiece(u32),
    HaveAll,
    HaveNone,
    RejectRequest(Request),
    AllowedFast(u32),
    /// BEP 52. NanoTorrent addition.
    HashRequest(HashRequest),
    Hashes(Hashes<'a>),
    HashReject(HashRequest),
}

#[derive(thiserror::Error, Debug)]
pub enum SerializeError {
    #[error("not enough space in buffer")]
    NoSpaceInBuffer,
    #[error(transparent)]
    Bencode(#[from] bencode::SerializeError),
    #[error("need peer's handshake to serialize ut_metadata, or peer does't support ut_metadata")]
    NeedUtMetadata,
    #[error("need peer's handshake to serialize ut_pex, or peer does't support ut_pex")]
    NeedPex,
}

impl From<std::io::Error> for SerializeError {
    fn from(_: std::io::Error) -> Self {
        Self::NoSpaceInBuffer
    }
}

impl Message<'_> {
    pub fn serialize(
        &self,
        out: &mut [u8],
        peer_extended_messages: &dyn Fn() -> PeerExtendedMessageIds,
    ) -> Result<usize, SerializeError> {
        macro_rules! check_len {
            ($l:expr) => {
                if out.len() < $l {
                    return Err(SerializeError::NoSpaceInBuffer);
                }
            };
        }

        macro_rules! write_preamble {
            ($msg_len:expr, $msg_id:expr) => {
                out[0..4].copy_from_slice(&(($msg_len + 1u32).to_be_bytes()));
                out[4] = $msg_id;
            };
        }

        match self {
            Message::Request(request) | Message::Cancel(request) => {
                const TOTAL_LEN: usize = PREAMBLE_LEN + INTEGER_LEN * 3;
                check_len!(TOTAL_LEN);
                let msg_id = match self {
                    Message::Request(..) => MSGID_REQUEST,
                    Message::Cancel(..) => MSGID_CANCEL,
                    _ => unsafe { unreachable_unchecked() },
                };
                write_preamble!((INTEGER_LEN * 3) as u32, msg_id);
                request.serialize_unchecked_len(&mut out[PREAMBLE_LEN..]);
                Ok(TOTAL_LEN)
            }
            Message::Bitfield(b) => {
                let block_len = b.as_ref().len();
                let total_len: usize = PREAMBLE_LEN + block_len;
                check_len!(total_len);
                write_preamble!(block_len as u32, MSGID_BITFIELD);
                out[PREAMBLE_LEN..PREAMBLE_LEN + block_len].copy_from_slice(b.as_ref());
                Ok(total_len)
            }
            Message::Choke | Message::Unchoke | Message::Interested | Message::NotInterested => {
                check_len!(PREAMBLE_LEN);
                let msg_id = match self {
                    Message::Choke => MSGID_CHOKE,
                    Message::Unchoke => MSGID_UNCHOKE,
                    Message::Interested => MSGID_INTERESTED,
                    Message::NotInterested => MSGID_NOT_INTERESTED,
                    _ => unsafe { unreachable_unchecked() },
                };
                write_preamble!(0, msg_id);
                Ok(PREAMBLE_LEN)
            }
            Message::Piece(p) => {
                let block_len = p.len();
                let payload_len = INTEGER_LEN * 2 + block_len;
                let total_len = PREAMBLE_LEN + payload_len;
                check_len!(total_len);
                write_preamble!(payload_len as u32, MSGID_PIECE);
                p.serialize_unchecked_len(&mut out[PREAMBLE_LEN..]);
                Ok(total_len)
            }
            Message::KeepAlive => {
                check_len!(4);
                out[0..4].copy_from_slice(&0u32.to_be_bytes());
                Ok(4)
            }
            Message::Have(v) => {
                check_len!(PREAMBLE_LEN + INTEGER_LEN);
                write_preamble!(INTEGER_LEN as u32, MSGID_HAVE);
                out[5..9].copy_from_slice(&v.to_be_bytes());
                Ok(9)
            }
            Message::Extended(e) => {
                check_len!(PREAMBLE_LEN + 2);
                let msg_len = e.serialize(&mut out[PREAMBLE_LEN..], peer_extended_messages)?;
                write_preamble!(msg_len as u32, MSGID_EXTENDED);
                Ok(PREAMBLE_LEN + msg_len)
            }
            Message::SuggestPiece(p) | Message::AllowedFast(p) => {
                check_len!(PREAMBLE_LEN + INTEGER_LEN);
                let msg_id = match self {
                    Message::SuggestPiece(..) => MSGID_SUGGEST_PIECE,
                    _ => MSGID_ALLOWED_FAST,
                };
                write_preamble!(INTEGER_LEN as u32, msg_id);
                out[5..9].copy_from_slice(&p.to_be_bytes());
                Ok(PREAMBLE_LEN + INTEGER_LEN)
            }
            Message::HaveAll | Message::HaveNone => {
                check_len!(PREAMBLE_LEN);
                let msg_id = match self {
                    Message::HaveAll => MSGID_HAVE_ALL,
                    _ => MSGID_HAVE_NONE,
                };
                write_preamble!(0, msg_id);
                Ok(PREAMBLE_LEN)
            }
            Message::RejectRequest(request) => {
                const TOTAL_LEN: usize = PREAMBLE_LEN + INTEGER_LEN * 3;
                check_len!(TOTAL_LEN);
                write_preamble!((INTEGER_LEN * 3) as u32, MSGID_REJECT_REQUEST);
                request.serialize_unchecked_len(&mut out[PREAMBLE_LEN..]);
                Ok(TOTAL_LEN)
            }
            Message::HashRequest(r) | Message::HashReject(r) => {
                const TOTAL_LEN: usize = PREAMBLE_LEN + HashRequest::WIRE_LEN;
                check_len!(TOTAL_LEN);
                let msg_id = match self {
                    Message::HashRequest(..) => MSGID_HASH_REQUEST,
                    _ => MSGID_HASH_REJECT,
                };
                write_preamble!(HashRequest::WIRE_LEN as u32, msg_id);
                r.serialize_unchecked_len(&mut out[PREAMBLE_LEN..]);
                Ok(TOTAL_LEN)
            }
            Message::Hashes(h) => {
                let hashes = h.hashes.as_ref();
                let payload_len = HashRequest::WIRE_LEN + hashes.len();
                let total_len = PREAMBLE_LEN + payload_len;
                check_len!(total_len);
                write_preamble!(payload_len as u32, MSGID_HASHES);
                h.request
                    .serialize_unchecked_len(&mut out[PREAMBLE_LEN..]);
                out[PREAMBLE_LEN + HashRequest::WIRE_LEN..total_len].copy_from_slice(hashes);
                Ok(total_len)
            }
        }
    }
}

impl Message<'_> {
    pub fn deserialize<'a>(
        buf: &'a [u8],
        buf2: &'a [u8],
    ) -> Result<(Message<'a>, usize), MessageDeserializeError> {
        let mut buf = DoubleBufHelper::new(buf, buf2);
        let len_prefix = buf
            .read_u32_be()
            .map_err(|rem| MessageDeserializeError::NotEnoughData(rem, None))?;
        let total_len = len_prefix as usize + 4;
        if len_prefix == 0 {
            return Ok((Message::KeepAlive, total_len));
        }

        let msg_id = buf.read_u8().ok_or(MessageDeserializeError::NotEnoughData(
            len_prefix as usize,
            None,
        ))?;

        let msg_len = len_prefix as usize - 1;
        if buf.len() < msg_len {
            return Err(MessageDeserializeError::NotEnoughData(
                msg_len - buf.len(),
                Some(MsgIdDebug(msg_id)),
            ));
        }

        macro_rules! check_msg_len {
            ($expected:expr) => {{
                if msg_len != $expected {
                    return Err(MessageDeserializeError::IncorrectMsgLen {
                        received: len_prefix - 1,
                        expected: $expected,
                        msg_id: MsgIdDebug(msg_id),
                    });
                }
            }};
            (min $expected:expr) => {{
                if msg_len < $expected {
                    return Err(MessageDeserializeError::IncorrectMsgLen {
                        received: len_prefix - 1,
                        expected: $expected,
                        msg_id: MsgIdDebug(msg_id),
                    });
                }
            }};
        }

        match msg_id {
            MSGID_CHOKE => {
                check_msg_len!(0);
                Ok((Message::Choke, total_len))
            }
            MSGID_UNCHOKE => {
                check_msg_len!(0);
                Ok((Message::Unchoke, total_len))
            }
            MSGID_INTERESTED => {
                check_msg_len!(0);
                Ok((Message::Interested, total_len))
            }
            MSGID_NOT_INTERESTED => {
                check_msg_len!(0);
                Ok((Message::NotInterested, total_len))
            }
            MSGID_HAVE => {
                check_msg_len!(4);
                let have = buf.read_u32_be().unwrap();
                Ok((Message::Have(have), total_len))
            }
            MSGID_SUGGEST_PIECE | MSGID_ALLOWED_FAST => {
                check_msg_len!(4);
                let piece = buf.read_u32_be().unwrap();
                let msg = if msg_id == MSGID_SUGGEST_PIECE {
                    Message::SuggestPiece(piece)
                } else {
                    Message::AllowedFast(piece)
                };
                Ok((msg, total_len))
            }
            MSGID_HAVE_ALL => {
                check_msg_len!(0);
                Ok((Message::HaveAll, total_len))
            }
            MSGID_HAVE_NONE => {
                check_msg_len!(0);
                Ok((Message::HaveNone, total_len))
            }
            MSGID_REJECT_REQUEST => {
                check_msg_len!(12);
                const I32: usize = 4;
                let req = buf.consume::<{ I32 * 3 }>().unwrap();
                Ok((
                    Message::RejectRequest(Request {
                        index: BE::read_u32(&req[0..I32]),
                        begin: BE::read_u32(&req[I32..I32 * 2]),
                        length: BE::read_u32(&req[I32 * 2..I32 * 3]),
                    }),
                    total_len,
                ))
            }
            MSGID_HASH_REQUEST | MSGID_HASH_REJECT => {
                // Not check_msg_len!: it needs one token usable as both usize
                // and u32, which an unsuffixed literal is and a const is not.
                if msg_len != HashRequest::WIRE_LEN {
                    return Err(MessageDeserializeError::IncorrectMsgLen {
                        received: msg_len as u32,
                        expected: HashRequest::WIRE_LEN as u32,
                        msg_id: MsgIdDebug(msg_id),
                    });
                }
                let b = buf.consume::<{ HashRequest::WIRE_LEN }>().unwrap();
                let req = HashRequest::parse(&b);
                let msg = if msg_id == MSGID_HASH_REQUEST {
                    Message::HashRequest(req)
                } else {
                    Message::HashReject(req)
                };
                Ok((msg, total_len))
            }
            MSGID_HASHES => {
                if msg_len < HashRequest::WIRE_LEN {
                    return Err(MessageDeserializeError::IncorrectMsgLen {
                        received: msg_len as u32,
                        expected: HashRequest::WIRE_LEN as u32,
                        msg_id: MsgIdDebug(msg_id),
                    });
                }
                let hashes_len = msg_len - HashRequest::WIRE_LEN;
                // A partial hash means the sender is confused or hostile;
                // refuse rather than round down and verify against garbage.
                if !hashes_len.is_multiple_of(32) {
                    return Err(MessageDeserializeError::IncorrectMsgLen {
                        received: msg_len as u32,
                        expected: (HashRequest::WIRE_LEN + hashes_len.next_multiple_of(32)) as u32,
                        msg_id: MsgIdDebug(msg_id),
                    });
                }
                let b = buf.consume::<{ HashRequest::WIRE_LEN }>().unwrap();
                let request = HashRequest::parse(&b);
                let hashes = buf
                    .get_contiguous(hashes_len)
                    .ok_or(MessageDeserializeError::NeedContiguous)?;
                Ok((
                    Message::Hashes(Hashes {
                        request,
                        hashes: ByteBuf::from(hashes),
                    }),
                    total_len,
                ))
            }
            MSGID_BITFIELD => {
                check_msg_len!(min 1);
                // In practice, as bitfield is always (almost) the first message, it should be contiguous.
                let data = buf
                    .get_contiguous(msg_len)
                    .ok_or(MessageDeserializeError::NeedContiguous)?;
                Ok((Message::Bitfield(ByteBuf::from(data)), total_len))
            }
            MSGID_REQUEST | MSGID_CANCEL => {
                check_msg_len!(12);
                const I32: usize = 4;
                const I32_3: usize = I32 * 3;
                let req = buf.consume::<I32_3>().unwrap();
                let request = Request {
                    index: BE::read_u32(&req[0..I32]),
                    begin: BE::read_u32(&req[I32..I32 * 2]),
                    length: BE::read_u32(&req[I32 * 2..I32 * 3]),
                };
                let req = if msg_id == MSGID_REQUEST {
                    Message::Request(request)
                } else {
                    Message::Cancel(request)
                };
                Ok((req, total_len))
            }
            MSGID_PIECE => {
                const MIN_PAYLOAD: usize = 1;
                const MIN_LENGTH: usize = INTEGER_LEN * 2 + MIN_PAYLOAD;
                if msg_len < MIN_LENGTH {
                    return Err(MessageDeserializeError::IncorrectMsgLen {
                        expected: MIN_LENGTH as u32,
                        received: msg_len as u32,
                        msg_id: MsgIdDebug(msg_id),
                    });
                }

                let index = buf.read_u32_be().unwrap();
                let begin = buf.read_u32_be().unwrap();

                let block_len = msg_len - INTEGER_LEN * 2;
                let (block_0, block_1) = buf.consume_variable(block_len).unwrap();

                Ok((
                    Message::Piece(Piece {
                        index,
                        begin,
                        block_0: block_0.into(),
                        block_1: block_1.into(),
                    }),
                    total_len,
                ))
            }
            MSGID_EXTENDED => Ok((
                Message::Extended(ExtendedMessage::deserialize(buf.with_max_len(msg_len))?),
                PREAMBLE_LEN + msg_len,
            )),
            msg_id => Err(MessageDeserializeError::UnsupportedMessageId(msg_id)),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Handshake {
    pub reserved: u64,
    pub info_hash: Id20,
    pub peer_id: Id20,
}

impl Handshake {
    pub fn new(info_hash: Id20, peer_id: Id20) -> Handshake {
        debug_assert_eq!(PSTR_BT1.len(), 19);

        let mut reserved: u64 = 0;
        // supports extended messaging
        reserved |= 1 << 20;
        // BEP 6 fast extension. NanoTorrent: advertised because the semantics
        // are implemented - see librqbit patch 0010.
        reserved |= 1 << 2;

        Handshake {
            reserved,
            info_hash,
            peer_id,
        }
    }

    pub fn deserialize(b: &[u8]) -> Result<(Handshake, usize), MessageDeserializeError> {
        const LEN: usize = 1 + PSTR_BT1.len() + 8 + 20 + 20;
        if b.len() < LEN {
            return Err(MessageDeserializeError::NotEnoughData(LEN - b.len(), None));
        }
        if b[0] as usize != PSTR_BT1.len() {
            return Err(MessageDeserializeError::HandshakePstrWrongLength(b[0]));
        }
        if &b[1..20] != PSTR_BT1.as_bytes() {
            return Err(MessageDeserializeError::HandshakePstrWrongContent);
        }

        let h = Handshake {
            reserved: BE::read_u64(&b[20..28]),
            info_hash: Id20::new(b[28..48].try_into().unwrap()),
            peer_id: Id20::new(b[48..68].try_into().unwrap()),
        };
        Ok((h, LEN))
    }

    pub fn supports_extended(&self) -> bool {
        self.reserved.to_be_bytes()[5] & 0x10 > 0
    }

    /// BEP 6: the peer understands the fast extension.
    ///
    /// Byte 7, mask 0x04. Fast messages are only exchanged when both sides set
    /// it. NanoTorrent addition.
    pub fn supports_fast(&self) -> bool {
        self.reserved.to_be_bytes()[7] & 0x04 > 0
    }

    /// Advertise the fast extension on an outgoing handshake.
    pub fn set_supports_fast(&mut self) {
        self.reserved |= 1 << 2;
    }

    /// BEP 52: the peer understands BitTorrent v2 and the hash messages.
    ///
    /// Byte 7, mask 0x10 - the same bit libtorrent sets. NanoTorrent addition.
    pub fn supports_v2(&self) -> bool {
        self.reserved.to_be_bytes()[7] & 0x10 > 0
    }

    /// Advertise v2 support on an outgoing handshake. NanoTorrent addition.
    pub fn set_supports_v2(&mut self) {
        self.reserved |= 1 << 4;
    }

    #[must_use]
    pub fn serialize_unchecked_len(&self, buf: &mut [u8]) -> usize {
        debug_assert_eq!(PSTR_BT1.len(), 19);
        buf[0] = 19;
        buf[1..20].copy_from_slice(PSTR_BT1.as_bytes());
        buf[20..28].copy_from_slice(&self.reserved.to_be_bytes());
        buf[28..48].copy_from_slice(&self.info_hash.0);
        buf[48..68].copy_from_slice(&self.peer_id.0);
        68
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct Request {
    pub index: u32,
    pub begin: u32,
    pub length: u32,
}

impl Request {
    pub fn new(index: u32, begin: u32, length: u32) -> Self {
        Self {
            index,
            begin,
            length,
        }
    }

    pub fn serialize_unchecked_len(&self, buf: &mut [u8]) -> usize {
        buf[0..4].copy_from_slice(&self.index.to_be_bytes());
        buf[4..8].copy_from_slice(&self.begin.to_be_bytes());
        buf[8..12].copy_from_slice(&self.length.to_be_bytes());
        12
    }
}

#[cfg(test)]
mod bep52_tests {
    use super::*;

    fn req() -> HashRequest {
        HashRequest {
            pieces_root: [7u8; 32],
            base_layer: 1,
            index: 4,
            length: 2,
            proof_layers: 3,
        }
    }

    /// The exact byte layout, spelled out rather than derived from the code -
    /// otherwise a round-trip test would happily agree with a wrong format.
    /// These numbers come from libtorrent's bt_peer_connection.cpp.
    #[test]
    fn hash_request_is_53_bytes_on_the_wire() {
        let mut out = [0u8; 128];
        let n = Message::HashRequest(req())
            .serialize(&mut out, &Default::default)
            .unwrap();
        assert_eq!(n, 4 + 1 + 48, "hash request is not 53 bytes");
        assert_eq!(&out[0..4], &49u32.to_be_bytes(), "wrong length prefix");
        assert_eq!(out[4], 21, "wrong message id");
        assert_eq!(&out[5..37], &[7u8; 32], "pieces root misplaced");
        assert_eq!(&out[37..41], &1u32.to_be_bytes(), "base layer misplaced");
        assert_eq!(&out[41..45], &4u32.to_be_bytes(), "index misplaced");
        assert_eq!(&out[45..49], &2u32.to_be_bytes(), "length misplaced");
        assert_eq!(&out[49..53], &3u32.to_be_bytes(), "proof layers misplaced");
    }

    #[test]
    fn hash_messages_round_trip() {
        let mut out = [0u8; 512];

        for (msg, id) in [
            (Message::HashRequest(req()), 21u8),
            (Message::HashReject(req()), 23u8),
        ] {
            let n = msg.serialize(&mut out, &Default::default).unwrap();
            assert_eq!(out[4], id);
            let (back, consumed) = Message::deserialize(&out[..n], &[]).unwrap();
            assert_eq!(consumed, n);
            match back {
                Message::HashRequest(r) | Message::HashReject(r) => assert_eq!(r, req()),
                other => panic!("wrong variant: {other:?}"),
            }
        }

        // hashes: header plus three 32-byte values.
        let hashes: Vec<u8> = (0..96u8).collect();
        let n = Message::Hashes(Hashes {
            request: req(),
            hashes: ByteBuf::from(&hashes[..]),
        })
        .serialize(&mut out, &Default::default)
        .unwrap();
        assert_eq!(n, 4 + 1 + 48 + 96);
        assert_eq!(out[4], 22);
        let (back, consumed) = Message::deserialize(&out[..n], &[]).unwrap();
        assert_eq!(consumed, n);
        match back {
            Message::Hashes(h) => {
                assert_eq!(h.request, req());
                let got: Vec<[u8; 32]> = h.iter_hashes().collect();
                assert_eq!(got.len(), 3);
                assert_eq!(got[0][0], 0);
                assert_eq!(got[1][0], 32);
                assert_eq!(got[2][31], 95);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// A `hashes` payload that is not a whole number of hashes must be
    /// refused, not truncated - the leftover would otherwise be verified
    /// against a hash nobody sent.
    #[test]
    fn a_ragged_hashes_payload_is_refused() {
        let mut out = [0u8; 512];
        let hashes = vec![0u8; 40]; // one hash and a bit
        let n = Message::Hashes(Hashes {
            request: req(),
            hashes: ByteBuf::from(&hashes[..]),
        })
        .serialize(&mut out, &Default::default)
        .unwrap();
        assert!(Message::deserialize(&out[..n], &[]).is_err());
    }

    /// Byte offsets spelled out, not derived from the code - a round-trip
    /// test agrees with a wrong format perfectly happily.
    #[test]
    fn fast_extension_messages_have_the_right_bytes() {
        let mut out = [0u8; 64];

        let n = Message::HaveAll.serialize(&mut out, &Default::default).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&out[0..4], &1u32.to_be_bytes());
        assert_eq!(out[4], 0x0E);

        let n = Message::HaveNone.serialize(&mut out, &Default::default).unwrap();
        assert_eq!(n, 5);
        assert_eq!(out[4], 0x0F);

        let n = Message::SuggestPiece(7).serialize(&mut out, &Default::default).unwrap();
        assert_eq!(n, 9);
        assert_eq!(&out[0..4], &5u32.to_be_bytes());
        assert_eq!(out[4], 0x0D);
        assert_eq!(&out[5..9], &7u32.to_be_bytes());

        let n = Message::AllowedFast(9).serialize(&mut out, &Default::default).unwrap();
        assert_eq!(out[4], 0x11);
        assert_eq!(&out[5..9], &9u32.to_be_bytes());
        assert_eq!(n, 9);

        let req = Request { index: 1, begin: 2, length: 3 };
        let n = Message::RejectRequest(req).serialize(&mut out, &Default::default).unwrap();
        assert_eq!(n, 17);
        assert_eq!(&out[0..4], &13u32.to_be_bytes());
        assert_eq!(out[4], 0x10);
        assert_eq!(&out[5..9], &1u32.to_be_bytes());
        assert_eq!(&out[9..13], &2u32.to_be_bytes());
        assert_eq!(&out[13..17], &3u32.to_be_bytes());
    }

    #[test]
    fn fast_extension_messages_round_trip() {
        let mut out = [0u8; 64];
        let req = Request { index: 4, begin: 5, length: 6 };
        for msg in [
            Message::HaveAll,
            Message::HaveNone,
            Message::SuggestPiece(11),
            Message::AllowedFast(12),
            Message::RejectRequest(req),
        ] {
            let expect = format!("{msg:?}");
            let n = msg.serialize(&mut out, &Default::default).unwrap();
            let (back, consumed) = Message::deserialize(&out[..n], &[]).unwrap();
            assert_eq!(consumed, n);
            assert_eq!(format!("{back:?}"), expect);
        }
    }

    #[test]
    fn the_fast_handshake_bit_is_byte_7_mask_0x04() {
        // Advertised by default now that the semantics are implemented, so a
        // fresh handshake already has it - that IS the behaviour under test.
        let mut h = Handshake::new(Id20::new([1u8; 20]), Id20::new([2u8; 20]));
        assert!(h.supports_fast(), "fast is not advertised by default");
        assert_eq!(h.reserved.to_be_bytes()[7], 0x04);
        assert!(h.supports_extended(), "the fast bit clobbered the extended bit");

        // Idempotent, and coexists with the v2 bit, which shares the byte.
        h.set_supports_fast();
        assert_eq!(h.reserved.to_be_bytes()[7], 0x04);
        h.set_supports_v2();
        assert!(h.supports_fast() && h.supports_v2());
        assert_eq!(h.reserved.to_be_bytes()[7], 0x14);
    }

    #[test]
    fn the_v2_handshake_bit_is_byte_7_mask_0x10() {
        let mut h = Handshake::new(Id20::new([1u8; 20]), Id20::new([2u8; 20]));
        assert!(!h.supports_v2(), "v2 advertised without being asked");
        assert!(h.supports_extended(), "the extended bit was disturbed");
        h.set_supports_v2();
        assert!(h.supports_v2());
        assert_eq!(
            h.reserved.to_be_bytes()[7] & 0x10,
            0x10,
            "wrong byte or mask"
        );
        assert!(h.supports_extended(), "setting v2 clobbered the extended bit");
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use crate::extended::handshake::ExtendedHandshake;

    const EXTENDED: &[u8] = include_bytes!("../../librqbit/resources/test/extended-handshake.bin");

    use super::*;
    #[test]
    fn test_handshake_serialize() {
        let info_hash = Id20::new([
            1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ]);
        let peer_id = Id20::new([
            1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ]);
        let mut buf = [0u8; 100];
        let se = Handshake::new(info_hash, peer_id);
        let len = se.serialize_unchecked_len(&mut buf);
        assert_eq!(len, 20 + 20 + 8 + 19 + 1);
        assert_eq!(buf[0], 19);
        assert_eq!(&buf[1..20], PSTR_BT1.as_bytes());
        assert_eq!(&buf[28..48], &info_hash.0);
        assert_eq!(&buf[48..68], &peer_id.0);

        let (de, dlen) = Handshake::deserialize(&buf).unwrap();
        assert_eq!(dlen, len);
        assert_eq!(se, de);
    }

    #[test]
    fn test_extended_serialize() {
        let msg = Message::Extended(ExtendedMessage::Handshake(ExtendedHandshake::new()));
        let mut out = [0u8; 100];
        msg.serialize(&mut out, &Default::default).unwrap();
        dbg!(out);
    }

    #[test]
    fn test_deserialize_serialize_extended_non_contiguous() {
        for split_point in 0..EXTENDED.len() {
            let (first, second) = EXTENDED.split_at(split_point);
            let res = Message::deserialize(first, second);
            if split_point > PREAMBLE_LEN + 1 && split_point < EXTENDED.len() {
                assert!(
                    matches!(res, Err(MessageDeserializeError::NeedContiguous)),
                    "expected NeedContiguous: {split_point}"
                )
            } else {
                let (msg, len) = res
                    .inspect_err(|e| panic!("split_point={split_point:?}; error: {e:#}"))
                    .unwrap();
                assert!(matches!(msg, Message::Extended(..)));
                assert_eq!(len, EXTENDED.len());
            }
        }
    }

    #[test]
    fn test_deserialize_piece() {
        const LEN: usize = 100;
        const EXTRA: usize = 100;
        let mut buf = [0u8; LEN + EXTRA];

        #[allow(clippy::needless_range_loop)]
        for id in 0..buf.len() {
            buf[id] = id as u8;
        }

        let block_len = LEN - PREAMBLE_LEN - INTEGER_LEN * 2;
        let len_prefix: u32 = (block_len + INTEGER_LEN * 2 + MSGID_LEN) as u32;
        let index: u32 = 42;
        let begin: u32 = 43;

        buf[0..4].copy_from_slice(&len_prefix.to_be_bytes());
        buf[4] = MSGID_PIECE;
        buf[5..9].copy_from_slice(&index.to_be_bytes());
        buf[9..13].copy_from_slice(&begin.to_be_bytes());

        for split_point in 0..buf.len() {
            dbg!(split_point);
            let (first, second) = buf.split_at(split_point);
            let (msg, len) = Message::deserialize(first, second).unwrap();

            let piece = match &msg {
                Message::Piece(piece) => piece,
                other => panic!("expected piece got {other:?}"),
            };

            assert_eq!(piece.len(), block_len);
            assert_eq!(piece.index, index);
            assert_eq!(piece.begin, begin);
            assert_eq!(len, LEN);

            let mut tmp = [0u8; 100];
            let slen = msg.serialize(&mut tmp, &|| Default::default()).unwrap();
            assert_eq!(slen, len);
            assert_eq!(buf[..len], tmp[..len]);

            let (first, second) = piece.data();

            assert_eq!(first.len() + second.len(), block_len);
            assert_eq!(first, &buf[13..13 + first.len()]);
            assert_eq!(
                second,
                &buf[13 + first.len()..13 + first.len() + second.len()]
            );
        }
    }

    #[test]
    fn test_deserialize_request() {
        let mut buf = [0u8; 100];

        let len_prefix: u32 = (MSGID_LEN + INTEGER_LEN * 3) as u32;
        let index: u32 = 42;
        let begin: u32 = 43;
        let length: u32 = 44;

        buf[0..4].copy_from_slice(&len_prefix.to_be_bytes());
        buf[4] = MSGID_REQUEST;
        buf[5..9].copy_from_slice(&index.to_be_bytes());
        buf[9..13].copy_from_slice(&begin.to_be_bytes());
        buf[13..17].copy_from_slice(&length.to_be_bytes());

        for split_point in 0..buf.len() {
            dbg!(split_point);
            let (first, second) = buf.split_at(split_point);
            let (msg, len) = Message::deserialize(first, second).unwrap();

            let request = match msg {
                Message::Request(req) => req,
                other => panic!("expected request got {other:?}"),
            };

            assert_eq!(request.index, index);
            assert_eq!(request.begin, begin);
            assert_eq!(request.length, length);
            assert_eq!(len, 17);

            let mut tmp = [0u8; 100];
            let slen = msg.serialize(&mut tmp, &|| Default::default()).unwrap();
            assert_eq!(slen, len);
            assert_eq!(buf[..len], tmp[..len]);
        }
    }

    #[test]
    fn test_keepalive() {
        let buf = [0u8; 100];

        for split_point in 0..buf.len() {
            let (first, second) = buf.split_at(split_point);
            let (msg, len) = Message::deserialize(first, second).unwrap();
            assert!(matches!(msg, Message::KeepAlive));
            assert_eq!(len, 4);
            let mut tmp = [0u8; 100];
            let slen = msg.serialize(&mut tmp, &|| Default::default()).unwrap();
            assert_eq!(slen, len);
            assert_eq!(buf[..len], tmp[..len]);
        }
    }

    #[test]
    fn test_have() {
        let mut buf = [0u8; 100];
        buf[0..4].copy_from_slice(&5u32.to_be_bytes());
        buf[4] = MSGID_HAVE;
        buf[5..9].copy_from_slice(&42u32.to_be_bytes());

        for split_point in 0..buf.len() {
            let (first, second) = buf.split_at(split_point);
            let (msg, len) = Message::deserialize(first, second).unwrap();
            assert!(matches!(msg, Message::Have(42)));
            assert_eq!(len, 9);
            let mut tmp = [0u8; 100];
            let slen = msg.serialize(&mut tmp, &|| Default::default()).unwrap();
            assert_eq!(slen, len);
            assert_eq!(buf[..len], tmp[..len]);
        }
    }

    #[test]
    fn test_bitfield() {
        let mut buf = [0u8; 100];
        buf[0..4].copy_from_slice(&43u32.to_be_bytes());
        buf[4] = MSGID_BITFIELD;
        for byte in buf[5..47].iter_mut() {
            *byte = 0b10101010;
        }

        for split_point in 0..buf.len() {
            let (first, second) = buf.split_at(split_point);
            let res = Message::deserialize(first, second);
            if (6..47).contains(&split_point) {
                assert!(
                    matches!(res, Err(MessageDeserializeError::NeedContiguous)),
                    "expected NeedContiguous: split_point={split_point}"
                );
                continue;
            }
            let (msg, len) = res.context(split_point).unwrap();
            let bf = match &msg {
                Message::Bitfield(bf) => bf,
                other => panic!("expected bitfield, got {other:?}"),
            };
            assert_eq!(len, 47);
            assert_eq!(bf.as_ref().len(), 42);
            for byte in bf.as_ref() {
                assert_eq!(*byte, 0b10101010);
            }
            let mut tmp = [0u8; 100];
            let slen = msg.serialize(&mut tmp, &|| Default::default()).unwrap();
            assert_eq!(slen, len);
            assert_eq!(buf[..len], tmp[..len]);
        }
    }

    #[test]
    fn test_no_data_messages() {
        let mut buf = [0u8; 100];

        for msgid in [
            MSGID_CHOKE,
            MSGID_UNCHOKE,
            MSGID_INTERESTED,
            MSGID_NOT_INTERESTED,
        ] {
            buf[0..4].copy_from_slice(&1u32.to_be_bytes());
            buf[4] = msgid;
            for split_point in 0..buf.len() {
                let (first, second) = buf.split_at(split_point);
                let (msg, len) = Message::deserialize(first, second).unwrap();
                match (msgid, &msg) {
                    (MSGID_CHOKE, Message::Choke)
                    | (MSGID_UNCHOKE, Message::Unchoke)
                    | (MSGID_INTERESTED, Message::Interested)
                    | (MSGID_NOT_INTERESTED, Message::NotInterested) => {}
                    (msgid, msg) => panic!("msgid={msgid}, msg={msg:?}"),
                }
                assert_eq!(len, 5);
                let mut tmp = [0u8; 100];
                let slen = msg.serialize(&mut tmp, &|| Default::default()).unwrap();
                assert_eq!(slen, len);
                assert_eq!(buf[..len], tmp[..len]);
            }
        }
    }
}
