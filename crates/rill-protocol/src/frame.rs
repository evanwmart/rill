use crate::error::Status;

/// A typed value carried by ACTION fields and document state
/// (protocol §7.5; the document format reuses this type).
#[derive(Debug, Clone, PartialEq)]
pub enum ActionValue {
    Str(String),
    Num(f64),
    Bool(bool),
}

impl ActionValue {
    pub fn type_name(&self) -> &'static str {
        match self {
            ActionValue::Str(_) => "string",
            ActionValue::Num(_) => "number",
            ActionValue::Bool(_) => "bool",
        }
    }
}

/// Assigned frame type bytes (spec §4). The high bit encodes direction:
/// `0x00–0x7F` client → server, `0x80–0xFF` server → client. CLOSE is the
/// one type legal in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Get = 0x01,
    Head = 0x02,
    Ping = 0x03,
    Close = 0x04,
    GetIf = 0x05,
    Action = 0x07,
    Resource = 0x81,
    Metadata = 0x82,
    Error = 0x83,
    Pong = 0x84,
    NotModified = 0x85,
}

impl FrameType {
    pub fn from_u8(byte: u8) -> Option<FrameType> {
        Some(match byte {
            0x01 => FrameType::Get,
            0x02 => FrameType::Head,
            0x03 => FrameType::Ping,
            0x04 => FrameType::Close,
            0x05 => FrameType::GetIf,
            0x07 => FrameType::Action,
            0x81 => FrameType::Resource,
            0x82 => FrameType::Metadata,
            0x83 => FrameType::Error,
            0x84 => FrameType::Pong,
            0x85 => FrameType::NotModified,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            FrameType::Get => "GET",
            FrameType::Head => "HEAD",
            FrameType::Ping => "PING",
            FrameType::Close => "CLOSE",
            FrameType::GetIf => "GET_IF",
            FrameType::Action => "ACTION",
            FrameType::Resource => "RESOURCE",
            FrameType::Metadata => "METADATA",
            FrameType::Error => "ERROR",
            FrameType::Pong => "PONG",
            FrameType::NotModified => "NOT_MODIFIED",
        }
    }

    /// May a client legally send this frame type?
    pub fn allowed_from_client(self) -> bool {
        matches!(
            self,
            FrameType::Get
                | FrameType::Head
                | FrameType::Ping
                | FrameType::Close
                | FrameType::GetIf
                | FrameType::Action
        )
    }

    /// May a server legally send this frame type?
    pub fn allowed_from_server(self) -> bool {
        matches!(
            self,
            FrameType::Resource
                | FrameType::Metadata
                | FrameType::Error
                | FrameType::Pong
                | FrameType::Close
                | FrameType::NotModified
        )
    }
}

/// Decoded 16-byte frame header. `flags` preserves unknown ignorable bits
/// exactly as received; the critical half is guaranteed known by
/// [`crate::decode_header`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    pub frame_type: FrameType,
    pub flags: u16,
    pub request_id: u32,
    pub payload_len: u32,
}

/// A fully decoded frame. Constructing one of these by hand and calling
/// [`crate::encode`] is how all Rill traffic is produced; `encode` re-checks
/// every invariant, so an invalid `Frame` value cannot reach the wire.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Get { request_id: u32, path: String, accept_zstd: bool },
    Head { request_id: u32, path: String },
    Ping { payload: Vec<u8> },
    Close,
    /// Conditional fetch: RESOURCE unless the server's current bytes hash to
    /// `hash` (BLAKE3-256; resource-format.md).
    GetIf { request_id: u32, path: String, hash: [u8; 32], accept_zstd: bool },
    /// The protocol's write verb: typed fields to an endpoint path; the
    /// server acts, then answers with a document (RESOURCE stream) or ERROR.
    /// Never retried automatically.
    ///
    /// `cas` is the critical CAS flag: the action is conditional on the
    /// revision named by its `_expected` field, and a server that does not
    /// understand the condition must refuse the frame rather than apply it.
    Action { request_id: u32, path: String, fields: Vec<(String, ActionValue)>, cas: bool },
    /// `zstd` MUST be uniform across all chunks of one response
    /// (resource-format.md §8); payload chunks concatenate to one stream.
    Resource { request_id: u32, more: bool, zstd: bool, payload: Vec<u8> },
    /// `hash` is the METADATA v2 field; `None` when decoding the v1 struct.
    Metadata { request_id: u32, size: u64, hash: Option<[u8; 32]> },
    Error { request_id: u32, status: Status, message: String },
    Pong { payload: Vec<u8> },
    NotModified { request_id: u32 },
}

impl Frame {
    pub fn frame_type(&self) -> FrameType {
        match self {
            Frame::Get { .. } => FrameType::Get,
            Frame::Head { .. } => FrameType::Head,
            Frame::Ping { .. } => FrameType::Ping,
            Frame::Close => FrameType::Close,
            Frame::GetIf { .. } => FrameType::GetIf,
            Frame::Action { .. } => FrameType::Action,
            Frame::Resource { .. } => FrameType::Resource,
            Frame::Metadata { .. } => FrameType::Metadata,
            Frame::Error { .. } => FrameType::Error,
            Frame::Pong { .. } => FrameType::Pong,
            Frame::NotModified { .. } => FrameType::NotModified,
        }
    }

    /// The request ID this frame carries on the wire. Connection-level frames
    /// (PING, PONG, CLOSE) always carry the reserved ID 0.
    pub fn request_id(&self) -> u32 {
        match self {
            Frame::Get { request_id, .. }
            | Frame::Head { request_id, .. }
            | Frame::GetIf { request_id, .. }
            | Frame::Action { request_id, .. }
            | Frame::Resource { request_id, .. }
            | Frame::Metadata { request_id, .. }
            | Frame::Error { request_id, .. }
            | Frame::NotModified { request_id } => *request_id,
            Frame::Ping { .. } | Frame::Close | Frame::Pong { .. } => 0,
        }
    }
}
