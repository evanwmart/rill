//! Rill protocol version 1: frame types and byte encoding.
//!
//! This crate is sans-I/O — it converts between Rust values and protocol
//! bytes and nothing else. No sockets, no TLS, no filesystem, no async.
//! The wire format is specified in `specs/protocol.md`; that document is the
//! source of truth and this crate is its witness.
//!
//! Entry points:
//!
//! * [`decode_header`] — validate and decode a 16-byte frame header;
//! * [`decode_payload`] — decode a frame's payload against its header;
//! * [`encode`] — encode a [`Frame`] to bytes (header + payload).
//!
//! Decoding is strict: every byte sequence is either valid with exactly one
//! meaning or rejected with exactly one [`FrameError`]. There is no lenient
//! mode.

mod codec;
mod error;
mod frame;
mod path;

pub use codec::{decode_header, decode_payload, encode};
pub use error::{FrameError, Status};
pub use frame::{ActionValue, Frame, FrameType, Header};
pub use path::validate_path;

/// Frame magic: every frame starts with these four bytes.
pub const MAGIC: [u8; 4] = *b"RILL";

/// The only protocol version this crate speaks.
pub const VERSION: u8 = 1;

/// Fixed frame header size in bytes.
pub const HEADER_LEN: usize = 16;

/// Hard upper bound on `payload_len`. Checked before any allocation.
pub const MAX_PAYLOAD: u32 = 0x0010_0000; // 1 MiB

/// Recommended sender chunk size for RESOURCE frames (policy, not wire law).
pub const DEFAULT_CHUNK: u32 = 256 * 1024;

/// Maximum request path length in bytes.
pub const MAX_PATH: usize = 1024;

/// Maximum ERROR message length in bytes.
pub const MAX_ERROR_MSG: usize = 512;

/// Maximum PING/PONG payload length in bytes.
pub const MAX_PING_PAYLOAD: usize = 64;

/// Maximum fields in one ACTION request.
pub const MAX_ACTION_FIELDS: usize = 32;

/// Maximum ACTION field-name length in bytes.
pub const MAX_FIELD_NAME: usize = 64;

/// Maximum ACTION string-value length in bytes.
///
/// The wire has always carried these with a `u16` length, so 65535 is the
/// format's own ceiling rather than a new allowance — 1024 was a policy
/// choice from when the only fields were search boxes and note bodies. An
/// editor's buffer is a field value, and a 1 KiB limit made "open a file"
/// mean "open a very short file". A field is still bounded far below
/// MAX_PAYLOAD, so a single ACTION cannot be a memory bomb.
pub const MAX_FIELD_STRING: usize = u16::MAX as usize;

/// GET/GET_IF flag (ignorable): sender can decode zstd-encoded responses.
pub const FLAG_ACCEPT_ZSTD: u16 = 0x0001;

/// RESOURCE flag: another chunk with this request ID follows.
pub const FLAG_MORE: u16 = 0x0100;

/// RESOURCE flag (critical): the chunk stream is one zstd-compressed stream.
pub const FLAG_CONTENT_ZSTD: u16 = 0x0200;

/// ACTION flag (critical): this action is **conditional**. It carries a
/// [`FIELD_EXPECTED`] field naming the resource revision the caller acted on,
/// and the server must refuse it with [`Status::Conflict`] if the resource
/// has moved since.
///
/// Critical on purpose. The flag is the part a server cannot afford to
/// ignore: a receiver that did not understand the condition and applied the
/// mutation anyway would be doing the exact thing the caller asked it not to
/// do. An older build rejects the frame outright instead — loudly, and
/// before anything is written.
pub const FLAG_ACTION_CAS: u16 = 0x0400;

/// The only assigned hash algorithm byte: BLAKE3-256 (resource-format.md §1).
pub const HASH_BLAKE3: u8 = 0x01;

/// Hash length in bytes (BLAKE3-256).
pub const HASH_LEN: usize = 32;

/// Mask of the critical (reject-if-unknown) half of the flags field.
pub const CRITICAL_FLAGS: u16 = 0xFF00;

/// Critical flags this version understands.
pub const KNOWN_CRITICAL_FLAGS: u16 = FLAG_MORE | FLAG_CONTENT_ZSTD | FLAG_ACTION_CAS;

/// The reserved ACTION field naming the revision a conditional action was
/// made against: the `blake3:<64 hex>` form of the resource's content hash,
/// as [`crate::HASH_BLAKE3`] addresses it elsewhere.
///
/// A field name rather than a new payload struct, because ACTION field names
/// are already an open vocabulary and the underscore prefix is not a legal
/// start for the names applications choose. Reserved: a handler must not
/// treat it as one of its own fields.
pub const FIELD_EXPECTED: &str = "_expected";
