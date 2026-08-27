use std::fmt;

/// Wire status codes carried in ERROR frames (spec §8).
///
/// `0x0000` OK is reserved and never appears in an ERROR frame, so it has no
/// variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Status {
    ProtocolMalformed = 0x0100,
    UnsupportedVersion = 0x0101,
    UnknownFrameType = 0x0102,
    FrameTooLarge = 0x0103,
    UnknownCriticalFlag = 0x0104,
    PathInvalid = 0x0105,
    /// Resource absent **or** access denied — deliberately indistinguishable.
    NotFound = 0x0200,
    /// A conditional ACTION whose `expected` hash is not the resource's
    /// current hash: the caller acted on a state that has since moved.
    ///
    /// Request-scoped (`0x02xx`), so it resolves the one request and leaves
    /// the connection alive — the caller re-reads and decides what to do.
    /// Detecting the conflict is the whole promise; resolving it is the
    /// application's.
    Conflict = 0x0201,
    Internal = 0x0300,
}

impl Status {
    pub fn from_u16(code: u16) -> Option<Status> {
        Some(match code {
            0x0100 => Status::ProtocolMalformed,
            0x0101 => Status::UnsupportedVersion,
            0x0102 => Status::UnknownFrameType,
            0x0103 => Status::FrameTooLarge,
            0x0104 => Status::UnknownCriticalFlag,
            0x0105 => Status::PathInvalid,
            0x0200 => Status::NotFound,
            0x0201 => Status::Conflict,
            0x0300 => Status::Internal,
            _ => return None,
        })
    }

    pub fn code(self) -> u16 {
        self as u16
    }

    pub fn name(self) -> &'static str {
        match self {
            Status::ProtocolMalformed => "PROTOCOL_MALFORMED",
            Status::UnsupportedVersion => "UNSUPPORTED_VERSION",
            Status::UnknownFrameType => "UNKNOWN_FRAME_TYPE",
            Status::FrameTooLarge => "FRAME_TOO_LARGE",
            Status::UnknownCriticalFlag => "UNKNOWN_CRITICAL_FLAG",
            Status::PathInvalid => "PATH_INVALID",
            Status::NotFound => "NOT_FOUND",
            Status::Conflict => "CONFLICT",
            Status::Internal => "INTERNAL",
        }
    }

    /// True if this status accompanies connection closure (spec §8).
    pub fn closes_connection(self) -> bool {
        self.code() & 0xFF00 == 0x0100
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (0x{:04X})", self.name(), self.code())
    }
}

/// Every way a frame can fail to decode or encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// Header does not start with `"RILL"`.
    BadMagic([u8; 4]),
    /// Header version byte is not 1.
    UnsupportedVersion(u8),
    /// `payload_len` exceeds `MAX_PAYLOAD`.
    PayloadTooLarge(u32),
    /// Frame type byte is not assigned.
    UnknownFrameType(u8),
    /// Unknown bit set in the critical half of the flags field.
    UnknownCriticalFlags(u16),
    /// A known flag is set on a frame type it does not apply to.
    FlagNotAllowed { frame_type: &'static str, flags: u16 },
    /// Payload length does not match the structure the frame type requires
    /// (truncated, trailing bytes, or a slice shorter than `payload_len`).
    LengthMismatch { expected: usize, actual: usize },
    /// Path failed the spec §7.1 validity rules.
    PathInvalid(&'static str),
    /// Text field (path or error message) is not valid UTF-8.
    InvalidUtf8,
    /// ERROR message longer than `MAX_ERROR_MSG`.
    MessageTooLong(usize),
    /// PING/PONG payload longer than `MAX_PING_PAYLOAD`.
    PingPayloadTooLong(usize),
    /// ERROR frame carries an unassigned (or reserved-OK) status code.
    UnknownStatus(u16),
    /// Request ID is zero where a request-scoped ID is required, or nonzero
    /// on a connection-level frame (PING, PONG, CLOSE).
    BadRequestId { frame_type: &'static str, request_id: u32 },
    /// RESOURCE frame with an empty payload and MORE set (infinite-stall vector).
    EmptyChunkWithMore,
    /// METADATA reserved field is nonzero.
    ReservedNonzero,
    /// Hash algorithm byte is not an assigned value.
    UnknownHashAlgorithm(u8),
    /// ACTION value type tag (or bool byte) is not an assigned value.
    UnknownValueTag(u8),
    /// ACTION carries the critical CAS flag but no `_expected` field naming
    /// the revision it is conditional on — a condition with nothing to test.
    CasWithoutExpected,
}

impl FrameError {
    /// The status code a server should put on the wire for this error.
    pub fn wire_status(&self) -> Status {
        match self {
            FrameError::UnsupportedVersion(_) => Status::UnsupportedVersion,
            FrameError::PayloadTooLarge(_) => Status::FrameTooLarge,
            FrameError::UnknownFrameType(_) => Status::UnknownFrameType,
            FrameError::UnknownCriticalFlags(_) => Status::UnknownCriticalFlag,
            FrameError::PathInvalid(_) => Status::PathInvalid,
            _ => Status::ProtocolMalformed,
        }
    }
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::BadMagic(m) => write!(f, "bad magic {m:02X?}"),
            FrameError::UnsupportedVersion(v) => write!(f, "unsupported version {v}"),
            FrameError::PayloadTooLarge(n) => write!(f, "payload length {n} exceeds maximum"),
            FrameError::UnknownFrameType(t) => write!(f, "unknown frame type 0x{t:02X}"),
            FrameError::UnknownCriticalFlags(bits) => {
                write!(f, "unknown critical flag bits 0x{bits:04X}")
            }
            FrameError::FlagNotAllowed { frame_type, flags } => {
                write!(f, "flags 0x{flags:04X} not allowed on {frame_type}")
            }
            FrameError::LengthMismatch { expected, actual } => {
                write!(f, "payload length mismatch: expected {expected}, got {actual}")
            }
            FrameError::PathInvalid(reason) => write!(f, "invalid path: {reason}"),
            FrameError::InvalidUtf8 => write!(f, "text field is not valid UTF-8"),
            FrameError::MessageTooLong(n) => write!(f, "error message of {n} bytes too long"),
            FrameError::PingPayloadTooLong(n) => write!(f, "ping payload of {n} bytes too long"),
            FrameError::UnknownStatus(s) => write!(f, "unknown status code 0x{s:04X}"),
            FrameError::BadRequestId { frame_type, request_id } => {
                write!(f, "request ID {request_id} invalid for {frame_type}")
            }
            FrameError::EmptyChunkWithMore => {
                write!(f, "empty RESOURCE chunk with MORE set")
            }
            FrameError::ReservedNonzero => write!(f, "reserved field is nonzero"),
            FrameError::UnknownHashAlgorithm(b) => {
                write!(f, "unknown hash algorithm 0x{b:02X}")
            }
            FrameError::UnknownValueTag(b) => {
                write!(f, "unknown value tag 0x{b:02X}")
            }
            FrameError::CasWithoutExpected => {
                write!(f, "conditional ACTION without an _expected field")
            }
        }
    }
}

impl std::error::Error for FrameError {}
