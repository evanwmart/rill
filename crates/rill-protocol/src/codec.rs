use crate::error::{FrameError, Status};
use crate::frame::{ActionValue, Frame, FrameType, Header};
use crate::path::validate_path;
use crate::{
    CRITICAL_FLAGS, FLAG_ACCEPT_ZSTD, FLAG_ACTION_CAS, FLAG_CONTENT_ZSTD, FLAG_MORE,
    HASH_BLAKE3, HASH_LEN,
    KNOWN_CRITICAL_FLAGS, MAGIC, MAX_ACTION_FIELDS, MAX_ERROR_MSG, MAX_FIELD_NAME,
    MAX_FIELD_STRING, MAX_PAYLOAD, MAX_PING_PAYLOAD, VERSION,
};

/// The known flags each frame type may carry (spec §5: a known flag on a
/// frame type it does not apply to is malformed; unknown ignorable bits are
/// ignored regardless).
fn allowed_flags(frame_type: FrameType) -> u16 {
    match frame_type {
        FrameType::Get | FrameType::GetIf => FLAG_ACCEPT_ZSTD,
        FrameType::Resource => FLAG_MORE | FLAG_CONTENT_ZSTD,
        FrameType::Action => FLAG_ACTION_CAS,
        _ => 0,
    }
}

const KNOWN_FLAGS: u16 = FLAG_ACCEPT_ZSTD | FLAG_MORE | FLAG_CONTENT_ZSTD | FLAG_ACTION_CAS;

/// Decode and validate a 16-byte frame header.
///
/// Validation follows the spec §3 order exactly: magic, version, payload
/// length (before any allocation the caller might do), frame type, critical
/// flags. Any error here means the connection should be closed, not just the
/// frame dropped.
pub fn decode_header(bytes: &[u8; 16]) -> Result<Header, FrameError> {
    let magic: [u8; 4] = bytes[0..4].try_into().unwrap();
    if magic != MAGIC {
        return Err(FrameError::BadMagic(magic));
    }

    let version = bytes[4];
    if version != VERSION {
        return Err(FrameError::UnsupportedVersion(version));
    }

    let payload_len = u32::from_be_bytes(bytes[12..16].try_into().unwrap());
    if payload_len > MAX_PAYLOAD {
        return Err(FrameError::PayloadTooLarge(payload_len));
    }

    let frame_type =
        FrameType::from_u8(bytes[5]).ok_or(FrameError::UnknownFrameType(bytes[5]))?;

    let flags = u16::from_be_bytes(bytes[6..8].try_into().unwrap());
    let unknown_critical = flags & CRITICAL_FLAGS & !KNOWN_CRITICAL_FLAGS;
    if unknown_critical != 0 {
        return Err(FrameError::UnknownCriticalFlags(unknown_critical));
    }

    let request_id = u32::from_be_bytes(bytes[8..12].try_into().unwrap());

    Ok(Header { version, frame_type, flags, request_id, payload_len })
}

/// Decode a frame payload against its already-validated header.
///
/// `payload` must be exactly `header.payload_len` bytes; anything else is a
/// caller-side framing bug and is rejected, never guessed around.
pub fn decode_payload(header: &Header, payload: &[u8]) -> Result<Frame, FrameError> {
    if payload.len() != header.payload_len as usize {
        return Err(FrameError::LengthMismatch {
            expected: header.payload_len as usize,
            actual: payload.len(),
        });
    }

    // Known flags only apply to specific frame types. (Unknown ignorable
    // bits were already accepted by decode_header and are ignored here.)
    let misplaced = header.flags & KNOWN_FLAGS & !allowed_flags(header.frame_type);
    if misplaced != 0 {
        return Err(FrameError::FlagNotAllowed {
            frame_type: header.frame_type.name(),
            flags: misplaced,
        });
    }

    let ty = header.frame_type;
    let id = header.request_id;
    match ty {
        FrameType::Get | FrameType::Head => {
            require_request_id(ty, id)?;
            let path = decode_path(payload)?;
            let accept_zstd = header.flags & FLAG_ACCEPT_ZSTD != 0;
            Ok(match ty {
                FrameType::Get => Frame::Get { request_id: id, path, accept_zstd },
                _ => Frame::Head { request_id: id, path },
            })
        }
        FrameType::GetIf => {
            require_request_id(ty, id)?;
            if payload.len() < 2 {
                return Err(FrameError::LengthMismatch { expected: 2, actual: payload.len() });
            }
            let path_len = u16::from_be_bytes(payload[0..2].try_into().unwrap()) as usize;
            let expected = 2 + path_len + 1 + HASH_LEN;
            if payload.len() != expected {
                return Err(FrameError::LengthMismatch { expected, actual: payload.len() });
            }
            let path = std::str::from_utf8(&payload[2..2 + path_len])
                .map_err(|_| FrameError::InvalidUtf8)?;
            validate_path(path)?;
            let algo = payload[2 + path_len];
            if algo != HASH_BLAKE3 {
                return Err(FrameError::UnknownHashAlgorithm(algo));
            }
            let hash: [u8; HASH_LEN] = payload[3 + path_len..].try_into().unwrap();
            let accept_zstd = header.flags & FLAG_ACCEPT_ZSTD != 0;
            Ok(Frame::GetIf { request_id: id, path: path.to_owned(), hash, accept_zstd })
        }
        FrameType::Action => {
            require_request_id(ty, id)?;
            let mut r = SliceReader { bytes: payload, pos: 0 };
            let path_len = r.u16()? as usize;
            let path = std::str::from_utf8(r.take(path_len)?)
                .map_err(|_| FrameError::InvalidUtf8)?
                .to_owned();
            validate_path(&path)?;
            let count = r.u16()? as usize;
            if count > MAX_ACTION_FIELDS {
                return Err(FrameError::LengthMismatch { expected: MAX_ACTION_FIELDS, actual: count });
            }
            let mut fields = Vec::with_capacity(count);
            for _ in 0..count {
                let name_len = r.u16()? as usize;
                if name_len == 0 || name_len > MAX_FIELD_NAME {
                    return Err(FrameError::LengthMismatch { expected: MAX_FIELD_NAME, actual: name_len });
                }
                let name = std::str::from_utf8(r.take(name_len)?)
                    .map_err(|_| FrameError::InvalidUtf8)?
                    .to_owned();
                let value = match r.u8()? {
                    1 => {
                        let len = r.u16()? as usize;
                        if len > MAX_FIELD_STRING {
                            return Err(FrameError::LengthMismatch { expected: MAX_FIELD_STRING, actual: len });
                        }
                        ActionValue::Str(
                            std::str::from_utf8(r.take(len)?)
                                .map_err(|_| FrameError::InvalidUtf8)?
                                .to_owned(),
                        )
                    }
                    2 => {
                        let n = f64::from_be_bytes(r.take(8)?.try_into().unwrap());
                        if !n.is_finite() {
                            return Err(FrameError::PathInvalid("non-finite action number"));
                        }
                        ActionValue::Num(n)
                    }
                    3 => match r.u8()? {
                        0 => ActionValue::Bool(false),
                        1 => ActionValue::Bool(true),
                        b => return Err(FrameError::UnknownValueTag(b)),
                    },
                    t => return Err(FrameError::UnknownValueTag(t)),
                };
                fields.push((name, value));
            }
            if r.pos != payload.len() {
                return Err(FrameError::LengthMismatch { expected: r.pos, actual: payload.len() });
            }
            // A conditional action must carry the revision it is
            // conditional on. The flag is critical, so a frame that sets it
            // without the field is asking for an enforcement the payload
            // cannot support — malformed, and refused before any handler
            // sees it.
            let cas = header.flags & FLAG_ACTION_CAS != 0;
            if cas && !has_expected(&fields) {
                return Err(FrameError::CasWithoutExpected);
            }
            Ok(Frame::Action { request_id: id, path, fields, cas })
        }
        FrameType::NotModified => {
            require_request_id(ty, id)?;
            if !payload.is_empty() {
                return Err(FrameError::LengthMismatch { expected: 0, actual: payload.len() });
            }
            Ok(Frame::NotModified { request_id: id })
        }
        FrameType::Ping | FrameType::Pong => {
            require_connection_id(ty, id)?;
            if payload.len() > MAX_PING_PAYLOAD {
                return Err(FrameError::PingPayloadTooLong(payload.len()));
            }
            let payload = payload.to_vec();
            Ok(match ty {
                FrameType::Ping => Frame::Ping { payload },
                _ => Frame::Pong { payload },
            })
        }
        FrameType::Close => {
            require_connection_id(ty, id)?;
            if !payload.is_empty() {
                return Err(FrameError::LengthMismatch { expected: 0, actual: payload.len() });
            }
            Ok(Frame::Close)
        }
        FrameType::Resource => {
            require_request_id(ty, id)?;
            let more = header.flags & FLAG_MORE != 0;
            let zstd = header.flags & FLAG_CONTENT_ZSTD != 0;
            if more && payload.is_empty() {
                return Err(FrameError::EmptyChunkWithMore);
            }
            Ok(Frame::Resource { request_id: id, more, zstd, payload: payload.to_vec() })
        }
        FrameType::Metadata => {
            require_request_id(ty, id)?;
            // Append-only struct (spec §7.3): v1 = size + reserved (10 bytes);
            // v2 appends hash algo + hash (43 bytes). Longer payloads are
            // future fields and are ignored; lengths between known struct
            // sizes are torn structs and malformed.
            const V1: usize = 10;
            const V2: usize = V1 + 1 + HASH_LEN;
            if payload.len() < V1 || (payload.len() > V1 && payload.len() < V2) {
                let expected = if payload.len() < V1 { V1 } else { V2 };
                return Err(FrameError::LengthMismatch { expected, actual: payload.len() });
            }
            let size = u64::from_be_bytes(payload[0..8].try_into().unwrap());
            let reserved = u16::from_be_bytes(payload[8..10].try_into().unwrap());
            if reserved != 0 {
                return Err(FrameError::ReservedNonzero);
            }
            let hash = if payload.len() >= V2 {
                if payload[V1] != HASH_BLAKE3 {
                    return Err(FrameError::UnknownHashAlgorithm(payload[V1]));
                }
                Some(payload[V1 + 1..V2].try_into().unwrap())
            } else {
                None
            };
            Ok(Frame::Metadata { request_id: id, size, hash })
        }
        FrameType::Error => {
            // ERROR may carry any request ID, including the reserved 0.
            if payload.len() < 4 {
                return Err(FrameError::LengthMismatch { expected: 4, actual: payload.len() });
            }
            let code = u16::from_be_bytes(payload[0..2].try_into().unwrap());
            let status = Status::from_u16(code).ok_or(FrameError::UnknownStatus(code))?;
            let msg_len = u16::from_be_bytes(payload[2..4].try_into().unwrap()) as usize;
            if msg_len > MAX_ERROR_MSG {
                return Err(FrameError::MessageTooLong(msg_len));
            }
            if payload.len() != 4 + msg_len {
                return Err(FrameError::LengthMismatch {
                    expected: 4 + msg_len,
                    actual: payload.len(),
                });
            }
            let message = std::str::from_utf8(&payload[4..])
                .map_err(|_| FrameError::InvalidUtf8)?
                .to_owned();
            Ok(Frame::Error { request_id: id, status, message })
        }
    }
}

/// Encode a frame (header + payload) and append it to `out`.
///
/// Every invariant decoding enforces is re-checked here, so
/// `decode(encode(frame))` always succeeds and yields an equal frame, and an
/// invalid `Frame` value can never reach the wire.
pub fn encode(frame: &Frame, out: &mut Vec<u8>) -> Result<(), FrameError> {
    let mut flags: u16 = 0;
    let mut payload: Vec<u8> = Vec::new();

    match frame {
        Frame::Get { request_id, path, accept_zstd } => {
            require_request_id(FrameType::Get, *request_id)?;
            validate_path(path)?;
            if *accept_zstd {
                flags |= FLAG_ACCEPT_ZSTD;
            }
            payload.reserve(2 + path.len());
            payload.extend_from_slice(&(path.len() as u16).to_be_bytes());
            payload.extend_from_slice(path.as_bytes());
        }
        Frame::Head { request_id, path } => {
            require_request_id(FrameType::Head, *request_id)?;
            validate_path(path)?;
            payload.reserve(2 + path.len());
            payload.extend_from_slice(&(path.len() as u16).to_be_bytes());
            payload.extend_from_slice(path.as_bytes());
        }
        Frame::Ping { payload: p } | Frame::Pong { payload: p } => {
            if p.len() > MAX_PING_PAYLOAD {
                return Err(FrameError::PingPayloadTooLong(p.len()));
            }
            payload = p.clone();
        }
        Frame::Close => {}
        Frame::Resource { request_id, more, zstd, payload: p } => {
            require_request_id(FrameType::Resource, *request_id)?;
            if *more {
                if p.is_empty() {
                    return Err(FrameError::EmptyChunkWithMore);
                }
                flags |= FLAG_MORE;
            }
            if *zstd {
                flags |= FLAG_CONTENT_ZSTD;
            }
            payload = p.clone();
        }
        Frame::GetIf { request_id, path, hash, accept_zstd } => {
            require_request_id(FrameType::GetIf, *request_id)?;
            validate_path(path)?;
            if *accept_zstd {
                flags |= FLAG_ACCEPT_ZSTD;
            }
            payload.reserve(2 + path.len() + 1 + HASH_LEN);
            payload.extend_from_slice(&(path.len() as u16).to_be_bytes());
            payload.extend_from_slice(path.as_bytes());
            payload.push(HASH_BLAKE3);
            payload.extend_from_slice(hash);
        }
        Frame::Action { request_id, path, fields, cas } => {
            require_request_id(FrameType::Action, *request_id)?;
            validate_path(path)?;
            if *cas {
                if !has_expected(fields) {
                    return Err(FrameError::CasWithoutExpected);
                }
                flags |= FLAG_ACTION_CAS;
            }
            if fields.len() > MAX_ACTION_FIELDS {
                return Err(FrameError::LengthMismatch { expected: MAX_ACTION_FIELDS, actual: fields.len() });
            }
            payload.extend_from_slice(&(path.len() as u16).to_be_bytes());
            payload.extend_from_slice(path.as_bytes());
            payload.extend_from_slice(&(fields.len() as u16).to_be_bytes());
            for (name, value) in fields {
                if name.is_empty() || name.len() > MAX_FIELD_NAME {
                    return Err(FrameError::LengthMismatch { expected: MAX_FIELD_NAME, actual: name.len() });
                }
                payload.extend_from_slice(&(name.len() as u16).to_be_bytes());
                payload.extend_from_slice(name.as_bytes());
                match value {
                    ActionValue::Str(s) => {
                        if s.len() > MAX_FIELD_STRING {
                            return Err(FrameError::LengthMismatch { expected: MAX_FIELD_STRING, actual: s.len() });
                        }
                        payload.push(1);
                        payload.extend_from_slice(&(s.len() as u16).to_be_bytes());
                        payload.extend_from_slice(s.as_bytes());
                    }
                    ActionValue::Num(n) => {
                        if !n.is_finite() {
                            return Err(FrameError::PathInvalid("non-finite action number"));
                        }
                        payload.push(2);
                        payload.extend_from_slice(&n.to_be_bytes());
                    }
                    ActionValue::Bool(b) => {
                        payload.push(3);
                        payload.push(*b as u8);
                    }
                }
            }
        }
        Frame::NotModified { request_id } => {
            require_request_id(FrameType::NotModified, *request_id)?;
        }
        Frame::Metadata { request_id, size, hash } => {
            require_request_id(FrameType::Metadata, *request_id)?;
            payload.extend_from_slice(&size.to_be_bytes());
            payload.extend_from_slice(&0u16.to_be_bytes()); // reserved
            if let Some(hash) = hash {
                payload.push(HASH_BLAKE3);
                payload.extend_from_slice(hash);
            }
        }
        Frame::Error { status, message, .. } => {
            if message.len() > MAX_ERROR_MSG {
                return Err(FrameError::MessageTooLong(message.len()));
            }
            payload.reserve(4 + message.len());
            payload.extend_from_slice(&status.code().to_be_bytes());
            payload.extend_from_slice(&(message.len() as u16).to_be_bytes());
            payload.extend_from_slice(message.as_bytes());
        }
    }

    if payload.len() > MAX_PAYLOAD as usize {
        return Err(FrameError::PayloadTooLarge(payload.len() as u32));
    }

    out.reserve(16 + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(frame.frame_type() as u8);
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&frame.request_id().to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(())
}

struct SliceReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> SliceReader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FrameError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.bytes.len())
            .ok_or(FrameError::LengthMismatch { expected: self.pos + n, actual: self.bytes.len() })?;
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, FrameError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, FrameError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
}

/// Does this field list name the revision a conditional action expects?
fn has_expected(fields: &[(String, ActionValue)]) -> bool {
    fields.iter().any(|(name, value)| {
        name == crate::FIELD_EXPECTED && matches!(value, ActionValue::Str(_))
    })
}

fn decode_path(payload: &[u8]) -> Result<String, FrameError> {
    if payload.len() < 2 {
        return Err(FrameError::LengthMismatch { expected: 2, actual: payload.len() });
    }
    let path_len = u16::from_be_bytes(payload[0..2].try_into().unwrap()) as usize;
    if payload.len() != 2 + path_len {
        return Err(FrameError::LengthMismatch { expected: 2 + path_len, actual: payload.len() });
    }
    let path = std::str::from_utf8(&payload[2..]).map_err(|_| FrameError::InvalidUtf8)?;
    validate_path(path)?;
    Ok(path.to_owned())
}

/// Request-scoped frames carry a nonzero ID (spec §6).
fn require_request_id(ty: FrameType, id: u32) -> Result<(), FrameError> {
    if id == 0 {
        return Err(FrameError::BadRequestId { frame_type: ty.name(), request_id: id });
    }
    Ok(())
}

/// Connection-level frames carry the reserved ID 0 (spec §6).
fn require_connection_id(ty: FrameType, id: u32) -> Result<(), FrameError> {
    if id != 0 {
        return Err(FrameError::BadRequestId { frame_type: ty.name(), request_id: id });
    }
    Ok(())
}
