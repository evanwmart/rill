//! Framed I/O for the Rill protocol (`specs/connection.md` §9).
//!
//! Pairs the sans-I/O codec with tokio streams: read exactly one frame,
//! write exactly one frame, enforce direction, and distinguish a clean close
//! from a truncated stream. Timeouts belong to the callers (wrap calls in
//! `tokio::time::timeout` per the §6 matrix) — this layer stays policy-free.

use std::fmt;
use std::io;
use std::path::PathBuf;

use rill_protocol::{
    Frame, FrameError, HEADER_LEN, Status, decode_header, decode_payload, encode,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Which peer the bytes are coming from, for direction validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    Client,
    Server,
}

#[derive(Debug)]
pub enum WireError {
    /// Clean EOF at a frame boundary (peer went away without CLOSE; abnormal
    /// but quiet if no request was in flight — see connection.md §2).
    Closed,
    /// EOF in the middle of a frame. Always an error.
    Truncated,
    /// Frame type not legal from this peer.
    WrongDirection(&'static str),
    /// Codec rejection — connection-fatal per connection.md §2.
    Protocol(FrameError),
    Io(io::Error),
}

impl WireError {
    /// Status a server should put in its final ERROR frame for this error.
    pub fn wire_status(&self) -> Status {
        match self {
            WireError::Protocol(e) => e.wire_status(),
            _ => Status::ProtocolMalformed,
        }
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Closed => write!(f, "connection closed by peer"),
            WireError::Truncated => write!(f, "connection closed mid-frame"),
            WireError::WrongDirection(t) => write!(f, "{t} frame from wrong direction"),
            WireError::Protocol(e) => write!(f, "protocol error: {e}"),
            WireError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for WireError {}

/// Read exactly one frame from `reader`, validating that its type is legal
/// coming from `from`.
pub async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
    from: Peer,
) -> Result<Frame, WireError> {
    let mut header = [0u8; HEADER_LEN];
    let mut filled = 0;
    while filled < HEADER_LEN {
        let n = reader.read(&mut header[filled..]).await.map_err(WireError::Io)?;
        if n == 0 {
            return Err(if filled == 0 { WireError::Closed } else { WireError::Truncated });
        }
        filled += n;
    }
    let h = decode_header(&header).map_err(WireError::Protocol)?;

    let legal = match from {
        Peer::Client => h.frame_type.allowed_from_client(),
        Peer::Server => h.frame_type.allowed_from_server(),
    };
    if !legal {
        return Err(WireError::WrongDirection(h.frame_type.name()));
    }

    // Header is validated, so payload_len <= MAX_PAYLOAD: safe to allocate.
    let mut payload = vec![0u8; h.payload_len as usize];
    reader.read_exact(&mut payload).await.map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            WireError::Truncated
        } else {
            WireError::Io(e)
        }
    })?;

    decode_payload(&h, &payload).map_err(WireError::Protocol)
}

/// Encode and write one frame as a single buffer (header + payload together,
/// one write — connection.md §9 / protocol.md §10a).
pub async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &Frame,
) -> Result<(), WireError> {
    let mut buf = Vec::new();
    encode(frame, &mut buf).map_err(WireError::Protocol)?;
    writer.write_all(&buf).await.map_err(WireError::Io)?;
    writer.flush().await.map_err(WireError::Io)?;
    Ok(())
}

/// Debug frame tap (connection.md §10): records each sent/received frame as
/// `NNNN-{tx,rx}-<TYPE>.bin` for later `rill inspect`. Best-effort — dump
/// failures never affect the connection.
pub struct FrameDump {
    dir: PathBuf,
    counter: u32,
}

impl FrameDump {
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<FrameDump> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(FrameDump { dir, counter: 0 })
    }

    pub fn record(&mut self, sent: bool, frame: &Frame) {
        self.counter += 1;
        let mut bytes = Vec::new();
        if encode(frame, &mut bytes).is_ok() {
            let name = format!(
                "{:04}-{}-{}.bin",
                self.counter,
                if sent { "tx" } else { "rx" },
                frame.frame_type().name()
            );
            let _ = std::fs::write(self.dir.join(name), bytes);
        }
    }
}

/// Record into an optional dump without cluttering call sites.
pub fn dump(dump: &mut Option<FrameDump>, sent: bool, frame: &Frame) {
    if let Some(d) = dump.as_mut() {
        d.record(sent, frame);
    }
}
