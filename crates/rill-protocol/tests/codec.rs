//! Conformance tests for the v1 wire format, mirroring `specs/protocol.md`:
//! golden byte vectors from §10, the §3 header validation order, the §7
//! payload rules, and the Phase 2 rejection matrix.

use rill_protocol::{
    decode_header, decode_payload, encode, Frame, FrameError, FrameType, MAX_PAYLOAD,
    Status, validate_path,
};

fn decode_all(bytes: &[u8]) -> Result<Frame, FrameError> {
    let header = decode_header(bytes[..16].try_into().unwrap())?;
    decode_payload(&header, &bytes[16..])
}

fn encode_one(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::new();
    encode(frame, &mut out).expect("encode valid frame");
    out
}

fn roundtrip(frame: &Frame) {
    let bytes = encode_one(frame);
    let decoded = decode_all(&bytes).expect("decode encoded frame");
    assert_eq!(&decoded, frame);
}

// ---------------------------------------------------------------- golden §10

#[test]
fn golden_get() {
    #[rustfmt::skip]
    let bytes: &[u8] = &[
        0x52, 0x49, 0x4C, 0x4C,             // "RILL"
        0x01,                               // version
        0x01,                               // GET
        0x00, 0x00,                         // flags
        0x00, 0x00, 0x00, 0x01,             // request ID 1
        0x00, 0x00, 0x00, 0x0E,             // payload len 14
        0x00, 0x0C,                         // path_len 12
        b'/', b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.', b't', b'x', b't',
    ];
    let frame = Frame::Get { request_id: 1, path: "/example.txt".into(), accept_zstd: false };
    assert_eq!(decode_all(bytes).unwrap(), frame);
    assert_eq!(encode_one(&frame), bytes);
    assert_eq!(bytes.len(), 30); // spec: "30 bytes total on the wire"
}

#[test]
fn golden_resource() {
    #[rustfmt::skip]
    let bytes: &[u8] = &[
        0x52, 0x49, 0x4C, 0x4C,
        0x01,
        0x81,                               // RESOURCE
        0x00, 0x00,                         // flags: MORE clear
        0x00, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x0D,             // payload len 13
        0x48, 0x65, 0x6C, 0x6C, 0x6F, 0x2C, 0x20, 0x52, 0x69, 0x6C, 0x6C, 0x21, 0x0A,
    ];
    let frame = Frame::Resource {
        request_id: 1,
        more: false,
        zstd: false,
        payload: b"Hello, Rill!\n".to_vec(),
    };
    assert_eq!(decode_all(bytes).unwrap(), frame);
    assert_eq!(encode_one(&frame), bytes);
}

#[test]
fn golden_error_not_found() {
    #[rustfmt::skip]
    let bytes: &[u8] = &[
        0x52, 0x49, 0x4C, 0x4C,
        0x01,
        0x83,                               // ERROR
        0x00, 0x00,
        0x00, 0x00, 0x00, 0x02,             // request ID 2
        0x00, 0x00, 0x00, 0x04,             // payload len 4
        0x02, 0x00,                         // NOT_FOUND
        0x00, 0x00,                         // msg_len 0
    ];
    let frame = Frame::Error { request_id: 2, status: Status::NotFound, message: String::new() };
    assert_eq!(decode_all(bytes).unwrap(), frame);
    assert_eq!(encode_one(&frame), bytes);
}

// ------------------------------------------------------------ round trips §11

#[test]
fn roundtrip_every_frame_type() {
    roundtrip(&Frame::Get { request_id: 1, path: "/".into(), accept_zstd: false });
    roundtrip(&Frame::Head { request_id: 2, path: "/private/notes/2026.txt".into() });
    roundtrip(&Frame::Ping { payload: vec![1, 2, 3] });
    roundtrip(&Frame::Ping { payload: vec![] });
    roundtrip(&Frame::Close);
    roundtrip(&Frame::Resource { request_id: 3, more: true, zstd: false, payload: vec![0xAB; 1024] });
    roundtrip(&Frame::Resource { request_id: 3, more: false, zstd: false, payload: vec![] });
    roundtrip(&Frame::Metadata { request_id: 4, size: u64::MAX, hash: None });
    roundtrip(&Frame::Metadata { request_id: 4, size: 7, hash: Some([0xCD; 32]) });
    roundtrip(&Frame::GetIf { request_id: 5, path: "/private/notes".into(), hash: [0xEF; 32], accept_zstd: false });
    roundtrip(&Frame::NotModified { request_id: 5 });
    roundtrip(&Frame::Error { request_id: 0, status: Status::Internal, message: "oops".into() });
    roundtrip(&Frame::Pong { payload: vec![0; 64] });
}

/// Deterministic pseudo-random round-trip sweep (no RNG deps; xorshift).
#[test]
fn roundtrip_randomized() {
    let mut state: u64 = 0x5EED_CAFE_F00D_0001;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..2000 {
        let r = next();
        let id = (r as u32) | 1; // nonzero
        let frame = match r % 6 {
            0 => Frame::Get {
                request_id: id,
                path: format!("/a/{}", r % 100_000),
                accept_zstd: r % 3 == 0,
            },
            1 => Frame::Head { request_id: id, path: format!("/x{}", r % 10) },
            2 => Frame::Ping { payload: vec![r as u8; (r % 65) as usize] },
            3 => Frame::Resource {
                request_id: id,
                more: r % 2 == 0,
                zstd: r % 3 == 0,
                payload: vec![r as u8; (r % 4096 + 1) as usize],
            },
            4 => Frame::Metadata {
                request_id: id,
                size: r,
                hash: if r % 2 == 0 { Some([(r % 251) as u8; 32]) } else { None },
            },
            _ => Frame::Error {
                request_id: r as u32,
                status: Status::NotFound,
                message: "x".repeat((r % 513) as usize),
            },
        };
        roundtrip(&frame);
    }
}

// ------------------------------------------------- header validation order §3

#[test]
fn header_bad_magic() {
    let mut h = [0u8; 16];
    h[..4].copy_from_slice(b"RULL");
    assert!(matches!(decode_header(&h), Err(FrameError::BadMagic(_))));
}

#[test]
fn header_unsupported_version() {
    let mut h = *b"RILL\x02\x01\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00";
    assert!(matches!(decode_header(&h), Err(FrameError::UnsupportedVersion(2))));
    // Version is checked before frame type: bad version + bad type → version error.
    h[5] = 0xFF;
    assert!(matches!(decode_header(&h), Err(FrameError::UnsupportedVersion(2))));
}

#[test]
fn header_oversized_length_checked_before_type() {
    // payload_len > MAX with an *invalid* type byte: must report the length,
    // proving the pre-allocation check runs first (spec §3 order).
    let mut h = *b"RILL\x01\xFF\x00\x00\x00\x00\x00\x01\xFF\xFF\xFF\xFF";
    assert!(matches!(decode_header(&h), Err(FrameError::PayloadTooLarge(0xFFFF_FFFF))));
    h[12..16].copy_from_slice(&(MAX_PAYLOAD + 1).to_be_bytes());
    assert!(matches!(decode_header(&h), Err(FrameError::PayloadTooLarge(_))));
    // Exactly MAX_PAYLOAD is legal.
    h[5] = 0x81;
    h[12..16].copy_from_slice(&MAX_PAYLOAD.to_be_bytes());
    assert!(decode_header(&h).is_ok());
}

#[test]
fn header_unknown_frame_type() {
    let h = *b"RILL\x01\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00";
    // 0x00 is deliberately unassigned (zero-filled buffer canary).
    assert!(matches!(decode_header(&h), Err(FrameError::UnknownFrameType(0x00))));
    let h = *b"RILL\x01\x7F\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00";
    assert!(matches!(decode_header(&h), Err(FrameError::UnknownFrameType(0x7F))));
}

#[test]
fn flags_unknown_critical_rejected_ignorable_accepted() {
    // Unknown critical bit (0x0800) → reject. (0x0200 is CONTENT_ZSTD,
    // 0x0400 is ACTION_CAS.)
    let mut h = *b"RILL\x01\x01\x08\x00\x00\x00\x00\x01\x00\x00\x00\x00";
    assert!(matches!(
        decode_header(&h),
        Err(FrameError::UnknownCriticalFlags(0x0800))
    ));
    // Unknown ignorable bit (0x0002) → accepted and preserved in the header.
    h[6..8].copy_from_slice(&0x0002u16.to_be_bytes());
    let header = decode_header(&h).unwrap();
    assert_eq!(header.flags, 0x0002);
    // ...and the payload still decodes (bit is ignored).
    let payload = [0x00, 0x01, b'/'];
    assert!(decode_payload(
        &rill_protocol::Header { payload_len: 3, ..header },
        &payload
    )
    .is_ok());
}

#[test]
fn more_flag_only_on_resource() {
    let mut h = *b"RILL\x01\x01\x01\x00\x00\x00\x00\x01\x00\x00\x00\x03";
    let header = decode_header(&h).unwrap(); // header-level: MORE is a known critical flag
    let err = decode_payload(&header, &[0x00, 0x01, b'/']).unwrap_err();
    assert!(matches!(err, FrameError::FlagNotAllowed { frame_type: "GET", .. }));
    // MORE on RESOURCE is fine.
    h[5] = 0x81;
    h[15] = 0x01;
    let header = decode_header(&h).unwrap();
    assert!(decode_payload(&header, &[0xAA]).is_ok());
}

// ------------------------------------------------------------- payloads §7

#[test]
fn truncated_and_trailing_payloads_rejected() {
    // Slice shorter than payload_len (truncated stream).
    let frame = Frame::Get { request_id: 1, path: "/a".into(), accept_zstd: false };
    let bytes = encode_one(&frame);
    let header = decode_header(bytes[..16].try_into().unwrap()).unwrap();
    assert!(matches!(
        decode_payload(&header, &bytes[16..bytes.len() - 1]),
        Err(FrameError::LengthMismatch { .. })
    ));
    // Trailing byte beyond the declared path (path_len says 2, payload has 3).
    let mut h = *b"RILL\x01\x01\x00\x00\x00\x00\x00\x01\x00\x00\x00\x05";
    let header = decode_header(&h).unwrap();
    assert!(matches!(
        decode_payload(&header, &[0x00, 0x02, b'/', b'a', b'!']),
        Err(FrameError::LengthMismatch { expected: 4, actual: 5 })
    ));
    // CLOSE with a payload is malformed.
    h[5] = 0x04;
    h[8..12].copy_from_slice(&0u32.to_be_bytes());
    h[15] = 0x01;
    let header = decode_header(&h).unwrap();
    assert!(matches!(
        decode_payload(&header, &[0x00]),
        Err(FrameError::LengthMismatch { expected: 0, actual: 1 })
    ));
}

#[test]
fn path_rules() {
    assert!(validate_path("/").is_ok());
    assert!(validate_path("/a").is_ok());
    assert!(validate_path("/private/notes/2026.txt").is_ok());
    assert!(validate_path("/päth/ünïcode").is_ok());

    for bad in ["", "a", "relative/path", "/a//b", "/a/", "/.", "/..", "/a/../b", "/a/./b"] {
        assert!(validate_path(bad).is_err(), "should reject {bad:?}");
    }
    assert!(validate_path("/a\0b").is_err(), "NUL byte");
    assert!(validate_path(&format!("/{}", "x".repeat(1024))).is_err(), "over MAX_PATH");
    assert!(validate_path(&format!("/{}", "x".repeat(1023))).is_ok(), "exactly MAX_PATH");
}

#[test]
fn path_non_utf8_rejected() {
    let h = *b"RILL\x01\x01\x00\x00\x00\x00\x00\x01\x00\x00\x00\x04";
    let header = decode_header(&h).unwrap();
    assert!(matches!(
        decode_payload(&header, &[0x00, 0x02, b'/', 0xFF]),
        Err(FrameError::InvalidUtf8)
    ));
}

#[test]
fn empty_chunk_with_more_rejected() {
    let h = *b"RILL\x01\x81\x01\x00\x00\x00\x00\x01\x00\x00\x00\x00";
    let header = decode_header(&h).unwrap();
    assert!(matches!(decode_payload(&header, &[]), Err(FrameError::EmptyChunkWithMore)));
    // And encode refuses to produce it.
    let mut out = Vec::new();
    assert!(matches!(
        encode(&Frame::Resource { request_id: 1, more: true, zstd: false, payload: vec![] }, &mut out),
        Err(FrameError::EmptyChunkWithMore)
    ));
}

#[test]
fn request_id_rules() {
    // Request-scoped frames reject ID 0.
    let h = *b"RILL\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x04";
    let header = decode_header(&h).unwrap();
    assert!(matches!(
        decode_payload(&header, &[0x00, 0x02, b'/', b'a']),
        Err(FrameError::BadRequestId { frame_type: "GET", .. })
    ));
    // Connection-level frames reject nonzero IDs.
    let h = *b"RILL\x01\x03\x00\x00\x00\x00\x00\x07\x00\x00\x00\x00";
    let header = decode_header(&h).unwrap();
    assert!(matches!(
        decode_payload(&header, &[]),
        Err(FrameError::BadRequestId { frame_type: "PING", .. })
    ));
    // ERROR may carry ID 0 (unattributable) or nonzero.
    roundtrip(&Frame::Error { request_id: 0, status: Status::ProtocolMalformed, message: String::new() });
}

#[test]
fn ping_payload_limit() {
    let h = *b"RILL\x01\x03\x00\x00\x00\x00\x00\x00\x00\x00\x00\x41";
    let header = decode_header(&h).unwrap();
    assert!(matches!(
        decode_payload(&header, &[0u8; 65]),
        Err(FrameError::PingPayloadTooLong(65))
    ));
}

#[test]
fn metadata_append_only_and_reserved() {
    // Exactly 10 bytes: v1 struct (no hash).
    let v1 = Frame::Metadata { request_id: 9, size: 8192, hash: None };
    let v1_bytes = encode_one(&v1);
    assert_eq!(v1_bytes.len(), 16 + 10);
    // Exactly 43 bytes: v2 struct (hash present).
    let v2 = Frame::Metadata { request_id: 9, size: 8192, hash: Some([7; 32]) };
    let v2_bytes = encode_one(&v2);
    assert_eq!(v2_bytes.len(), 16 + 43);
    // Trailing future fields beyond v2 are ignored (declared extension point).
    let mut extended = v2_bytes.clone();
    extended.extend_from_slice(&[0xDE, 0xAD]);
    extended[12..16].copy_from_slice(&45u32.to_be_bytes());
    assert_eq!(decode_all(&extended).unwrap(), v2);
    // Torn struct (between v1 and v2 sizes) is malformed.
    let mut torn = v1_bytes.clone();
    torn.extend_from_slice(&[0xDE, 0xAD]);
    torn[12..16].copy_from_slice(&12u32.to_be_bytes());
    assert!(matches!(decode_all(&torn), Err(FrameError::LengthMismatch { expected: 43, .. })));
    // Short payload is malformed.
    let h = *b"RILL\x01\x82\x00\x00\x00\x00\x00\x09\x00\x00\x00\x09";
    let header = decode_header(&h).unwrap();
    assert!(matches!(decode_payload(&header, &[0u8; 9]), Err(FrameError::LengthMismatch { .. })));
    // Nonzero reserved field is rejected.
    let mut bad = v1_bytes;
    bad[16 + 9] = 1;
    assert!(matches!(decode_all(&bad), Err(FrameError::ReservedNonzero)));
    // Unknown hash algorithm byte is rejected.
    let mut bad_algo = v2_bytes;
    bad_algo[16 + 10] = 0x02;
    assert!(matches!(decode_all(&bad_algo), Err(FrameError::UnknownHashAlgorithm(0x02))));
}

#[test]
fn zstd_flag_placement() {
    // ACCEPT_ZSTD roundtrips on GET and GET_IF.
    roundtrip(&Frame::Get { request_id: 1, path: "/a".into(), accept_zstd: true });
    roundtrip(&Frame::GetIf {
        request_id: 2,
        path: "/a".into(),
        hash: [1; 32],
        accept_zstd: true,
    });
    // CONTENT_ZSTD roundtrips on RESOURCE (with and without MORE).
    roundtrip(&Frame::Resource { request_id: 3, more: true, zstd: true, payload: vec![9; 8] });
    roundtrip(&Frame::Resource { request_id: 3, more: false, zstd: true, payload: vec![] });

    // CONTENT_ZSTD (0x0200) on GET → known flag, wrong frame type.
    let h = *b"RILL\x01\x01\x02\x00\x00\x00\x00\x01\x00\x00\x00\x03";
    let header = decode_header(&h).unwrap();
    assert!(matches!(
        decode_payload(&header, &[0x00, 0x01, b'/']),
        Err(FrameError::FlagNotAllowed { frame_type: "GET", .. })
    ));
    // ACCEPT_ZSTD (0x0001) on PING → known flag, wrong frame type.
    let h = *b"RILL\x01\x03\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00";
    let header = decode_header(&h).unwrap();
    assert!(matches!(
        decode_payload(&header, &[]),
        Err(FrameError::FlagNotAllowed { frame_type: "PING", .. })
    ));
}

#[test]
fn get_if_layout_and_rejections() {
    let frame = Frame::GetIf { request_id: 3, path: "/a".into(), hash: [9; 32], accept_zstd: false };
    let bytes = encode_one(&frame);
    assert_eq!(bytes.len(), 16 + 2 + 2 + 1 + 32);
    assert_eq!(bytes[16 + 4], 0x01, "hash algo byte");

    // Unknown algo byte rejected.
    let mut bad_algo = bytes.clone();
    bad_algo[16 + 4] = 0x03;
    assert!(matches!(decode_all(&bad_algo), Err(FrameError::UnknownHashAlgorithm(0x03))));

    // Truncated hash rejected (payload_len shortened by one).
    let mut short = bytes.clone();
    short.truncate(bytes.len() - 1);
    short[12..16].copy_from_slice(&((2 + 2 + 1 + 32 - 1) as u32).to_be_bytes());
    assert!(matches!(decode_all(&short), Err(FrameError::LengthMismatch { .. })));

    // NOT_MODIFIED must be empty.
    let h = *b"RILL\x01\x85\x00\x00\x00\x00\x00\x03\x00\x00\x00\x01";
    let header = decode_header(&h).unwrap();
    assert!(matches!(
        decode_payload(&header, &[0]),
        Err(FrameError::LengthMismatch { expected: 0, actual: 1 })
    ));
}

#[test]
fn error_payload_rules() {
    // Unknown status code rejected.
    let h = *b"RILL\x01\x83\x00\x00\x00\x00\x00\x01\x00\x00\x00\x04";
    let header = decode_header(&h).unwrap();
    assert!(matches!(
        decode_payload(&header, &[0x99, 0x99, 0x00, 0x00]),
        Err(FrameError::UnknownStatus(0x9999))
    ));
    // Status 0x0000 (OK) is reserved: never valid in an ERROR frame.
    assert!(matches!(
        decode_payload(&header, &[0x00, 0x00, 0x00, 0x00]),
        Err(FrameError::UnknownStatus(0x0000))
    ));
    // msg_len over MAX_ERROR_MSG rejected; encode refuses too.
    assert!(matches!(
        decode_payload(&header, &[0x02, 0x00, 0x02, 0x01]),
        Err(FrameError::MessageTooLong(513))
    ));
    let mut out = Vec::new();
    assert!(matches!(
        encode(
            &Frame::Error { request_id: 1, status: Status::NotFound, message: "x".repeat(513) },
            &mut out
        ),
        Err(FrameError::MessageTooLong(513))
    ));
}

// ------------------------------------------------------------- misc surface

#[test]
fn direction_helpers() {
    assert!(FrameType::Get.allowed_from_client());
    assert!(!FrameType::Get.allowed_from_server());
    assert!(FrameType::Resource.allowed_from_server());
    assert!(!FrameType::Resource.allowed_from_client());
    assert!(FrameType::Close.allowed_from_client());
    assert!(FrameType::Close.allowed_from_server());
}

#[test]
fn wire_status_mapping() {
    assert_eq!(FrameError::UnsupportedVersion(9).wire_status(), Status::UnsupportedVersion);
    assert_eq!(FrameError::PayloadTooLarge(0).wire_status(), Status::FrameTooLarge);
    assert_eq!(FrameError::UnknownFrameType(9).wire_status(), Status::UnknownFrameType);
    assert_eq!(FrameError::UnknownCriticalFlags(2).wire_status(), Status::UnknownCriticalFlag);
    assert_eq!(FrameError::PathInvalid("x").wire_status(), Status::PathInvalid);
    assert_eq!(FrameError::EmptyChunkWithMore.wire_status(), Status::ProtocolMalformed);
    assert!(Status::ProtocolMalformed.closes_connection());
    assert!(!Status::NotFound.closes_connection());
}

#[test]
fn action_roundtrip_and_rejections() {
    use rill_protocol::ActionValue;
    let frame = Frame::Action {
        request_id: 4,
        path: "/actions/create-note".into(),
        fields: vec![
            ("title".into(), ActionValue::Str("hello".into())),
            ("count".into(), ActionValue::Num(3.5)),
            ("done".into(), ActionValue::Bool(true)),
        ],
        cas: false,
    };
    roundtrip(&frame);
    roundtrip(&Frame::Action { request_id: 1, path: "/a".into(), fields: vec![], cas: false });

    let bytes = encode_one(&frame);
    // Bad value tag rejected.
    let mut bad = bytes.clone();
    // Locate the type tag for "title" (after path block + count + name).
    let tag_off = 16 + 2 + 20 + 2 + 2 + 5;
    assert_eq!(bad[tag_off], 1, "expected string tag at computed offset");
    bad[tag_off] = 9;
    assert!(matches!(decode_all(&bad), Err(FrameError::UnknownValueTag(9))));

    // Trailing garbage rejected.
    let mut trailing = bytes.clone();
    trailing.push(0);
    let new_len = (trailing.len() - 16) as u32;
    trailing[12..16].copy_from_slice(&new_len.to_be_bytes());
    assert!(decode_all(&trailing).is_err());

    // Too many fields rejected at encode.
    let mut out = Vec::new();
    let many = Frame::Action {
        request_id: 1,
        path: "/a".into(),
        fields: (0..33).map(|i| (format!("f{i}"), ActionValue::Bool(false))).collect(),
        cas: false,
    };
    assert!(encode(&many, &mut out).is_err());

    // Non-finite number rejected at encode.
    let mut out = Vec::new();
    let nan = Frame::Action {
        request_id: 1,
        path: "/a".into(),
        fields: vec![("n".into(), ActionValue::Num(f64::NAN))],
        cas: false,
    };
    assert!(encode(&nan, &mut out).is_err());
}

/// A conditional ACTION round-trips with its flag, and the flag and the
/// field travel together in both directions: encoding one without the other
/// is refused, and so is decoding it.
#[test]
fn conditional_action_carries_its_revision_or_is_refused() {
    let expected = rill_protocol::ActionValue::Str(format!("blake3:{}", "ab".repeat(32)));
    let conditional = Frame::Action {
        request_id: 1,
        path: "/notes/actions/save/x".into(),
        fields: vec![
            ("body".into(), rill_protocol::ActionValue::Str("hello".into())),
            (rill_protocol::FIELD_EXPECTED.into(), expected.clone()),
        ],
        cas: true,
    };
    roundtrip(&conditional);

    // The flag reaches the wire in the critical half.
    let mut bytes = Vec::new();
    rill_protocol::encode(&conditional, &mut bytes).unwrap();
    let flags = u16::from_be_bytes(bytes[6..8].try_into().unwrap());
    assert_eq!(flags & rill_protocol::FLAG_ACTION_CAS, rill_protocol::FLAG_ACTION_CAS);
    assert_eq!(
        flags & rill_protocol::CRITICAL_FLAGS,
        rill_protocol::FLAG_ACTION_CAS,
        "the condition is critical: a receiver that ignores it must not proceed"
    );

    // A condition with nothing to test is malformed, at both ends.
    let empty_promise = Frame::Action {
        request_id: 1,
        path: "/a".into(),
        fields: vec![("body".into(), rill_protocol::ActionValue::Str("x".into()))],
        cas: true,
    };
    let mut out = Vec::new();
    assert!(matches!(
        rill_protocol::encode(&empty_promise, &mut out),
        Err(FrameError::CasWithoutExpected)
    ));

    // Same, arriving from the wire: take the valid frame's bytes and drop
    // the `_expected` field, keeping the flag.
    let unconditional = Frame::Action {
        request_id: 1,
        path: "/notes/actions/save/x".into(),
        fields: vec![("body".into(), rill_protocol::ActionValue::Str("hello".into()))],
        cas: false,
    };
    let mut raw = Vec::new();
    rill_protocol::encode(&unconditional, &mut raw).unwrap();
    raw[6..8].copy_from_slice(&rill_protocol::FLAG_ACTION_CAS.to_be_bytes());
    let header = decode_header(raw[..16].try_into().unwrap()).unwrap();
    assert!(matches!(
        decode_payload(&header, &raw[16..]),
        Err(FrameError::CasWithoutExpected)
    ));
}

/// The compatibility property, stated as a test: a build that does not know
/// about conditional actions rejects one rather than applying it
/// unconditionally. That is what makes the flag critical — the failure mode
/// it prevents is a caller's "only if unchanged" being silently dropped.
#[test]
fn a_build_without_cas_refuses_a_conditional_action() {
    let conditional = Frame::Action {
        request_id: 1,
        path: "/a".into(),
        fields: vec![(
            rill_protocol::FIELD_EXPECTED.into(),
            rill_protocol::ActionValue::Str(format!("blake3:{}", "cd".repeat(32))),
        )],
        cas: true,
    };
    let mut bytes = Vec::new();
    rill_protocol::encode(&conditional, &mut bytes).unwrap();

    // What an older decoder does: the CAS bit is not in its known-critical
    // set, so header validation refuses the frame outright.
    let flags = u16::from_be_bytes(bytes[6..8].try_into().unwrap());
    let known_before_cas =
        rill_protocol::KNOWN_CRITICAL_FLAGS & !rill_protocol::FLAG_ACTION_CAS;
    let unknown = flags & rill_protocol::CRITICAL_FLAGS & !known_before_cas;
    assert_eq!(
        unknown,
        rill_protocol::FLAG_ACTION_CAS,
        "an older build sees an unknown critical bit and rejects the frame"
    );
}

/// CONFLICT is request-scoped: it answers one request and leaves the
/// connection alive, unlike the `0x01xx` protocol failures.
#[test]
fn conflict_is_a_request_answer_not_a_connection_failure() {
    assert_eq!(Status::from_u16(0x0201), Some(Status::Conflict));
    assert!(!Status::Conflict.closes_connection());
    assert!(Status::ProtocolMalformed.closes_connection());
    roundtrip(&Frame::Error {
        request_id: 7,
        status: Status::Conflict,
        message: "resource has changed since you read it".into(),
    });
}
