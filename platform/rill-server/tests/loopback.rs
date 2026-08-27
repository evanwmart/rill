//! In-process loopback tests over real TLS: server on 127.0.0.1:0, real
//! client, generated identities. Covers the connection.md §11 matrix and the
//! security.md §8 verification matrix.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rill_auth::{fingerprint_hex, generate_identity, parse_cert_pem};
use rill_client::{Client, ClientConfig, ClientError};
use rill_protocol::Status;
use rill_server::{Server, ServerConfig};

static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rill-loopback-{}-{}",
        std::process::id(),
        DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const POLICY: &str = r#"
default_access = "deny"

[[rule]]
path = "/public/**"
allow = ["anonymous"]

[[rule]]
path = "/private/**"
allow = ["testdev"]
"#;

/// A running server plus everything a test needs to talk (or fail to talk)
/// to it.
struct Env {
    addr: SocketAddr,
    root: PathBuf,
    identity_dir: PathBuf,
    server_fp: String,
    enrolled: rill_auth::PemIdentity,
    stranger: rill_auth::PemIdentity,
    stats: std::sync::Arc<rill_server::WireStats>,
}

async fn start() -> Env {
    let root = temp_dir();
    std::fs::create_dir_all(root.join("public")).unwrap();
    std::fs::create_dir_all(root.join("private")).unwrap();

    let identity_dir = temp_dir();
    let server_id = generate_identity("test-server").unwrap();
    std::fs::write(identity_dir.join("server-key.pem"), &server_id.key_pem).unwrap();
    std::fs::write(identity_dir.join("server-cert.pem"), &server_id.cert_pem).unwrap();
    let server_fp = fingerprint_hex(&parse_cert_pem(&server_id.cert_pem).unwrap());

    let enrolled = generate_identity("testdev").unwrap();
    let enrolled_fp = fingerprint_hex(&parse_cert_pem(&enrolled.cert_pem).unwrap());
    std::fs::write(identity_dir.join("devices.toml"), format!("testdev = \"{enrolled_fp}\"\n"))
        .unwrap();
    std::fs::write(identity_dir.join("policy.toml"), POLICY).unwrap();

    let stranger = generate_identity("stranger").unwrap();

    let server = Server::bind("127.0.0.1", 0, ServerConfig::new(&root, &identity_dir))
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    let stats = server.wire_stats();
    tokio::spawn(server.run());
    Env { addr, root, identity_dir, server_fp, enrolled, stranger, stats }
}

impl Env {
    fn config(&self, device: Option<&rill_auth::PemIdentity>) -> ClientConfig {
        let mut cfg = ClientConfig::new(&self.server_fp);
        cfg.device = device.cloned();
        cfg
    }

    async fn client(&self, device: Option<&rill_auth::PemIdentity>) -> Client {
        Client::connect(&self.addr.ip().to_string(), self.addr.port(), self.config(device))
            .await
            .unwrap()
    }

    fn write(&self, rel: &str, contents: &[u8]) {
        std::fs::write(self.root.join(rel), contents).unwrap();
    }
}

fn expect_not_found(result: Result<rill_client::Fetched, ClientError>) {
    match result {
        Err(ClientError::Server { status: Status::NotFound, .. }) => {}
        Err(other) => panic!("expected NOT_FOUND, got {other:?}"),
        Ok(f) => panic!("expected NOT_FOUND, got {} bytes", f.data.len()),
    }
}

// ------------------------------------------------- transport still works (§11)

/// Wire accounting sees the exchange: rx covers at least the request frame,
/// tx at least the resource body, and the connection was counted. Exact
/// figures are codec-version-dependent, so the assertions are bounds — the
/// point is that the counters are wired to the real stream, not estimated.
#[tokio::test]
async fn wire_stats_count_protocol_bytes() {
    let env = start().await;
    // Incompressible body (xorshift-ish), so compressed-or-not the wire
    // carries roughly the full size. (A constant body compresses to ~34
    // bytes and the counter honestly reports that — wire bytes, not
    // payload bytes — which the first version of this test learned.)
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let body: Vec<u8> = (0..4096)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect();
    env.write("public/counted.bin", &body);

    let (rx0, tx0, conns0) = env.stats.snapshot();
    assert_eq!((rx0, tx0, conns0), (0, 0, 0), "fresh server has zero totals");

    let mut c = env.client(None).await;
    assert_eq!(c.get("/public/counted.bin").await.unwrap().data, body);

    let (rx, tx, conns) = env.stats.snapshot();
    assert_eq!(conns, 1);
    assert!(rx > 0, "request bytes counted (got {rx})");
    // Incompressible ⇒ at least ~the body crossed the wire (zstd may shave
    // or add a little; frame headers add more). Bound loosely from below.
    let floor = (body.len() * 9 / 10) as u64;
    assert!(tx > floor, "response bytes counted (got {tx}, floor {floor})");
}

#[tokio::test]
async fn roundtrip_over_tls() {
    let env = start().await;
    env.write("public/example.txt", b"Hello, Rill!\n");
    // 600 000 bytes at 256 KiB chunks → 3 RESOURCE frames.
    let big: Vec<u8> = (0..600_000u32).map(|i| (i * 31 % 251) as u8).collect();
    env.write("public/big.bin", &big);
    env.write("public/empty", b"");

    let mut c = env.client(None).await;
    assert_eq!(c.get("/public/example.txt").await.unwrap().data, b"Hello, Rill!\n");
    assert_eq!(c.get("/public/big.bin").await.unwrap().data, big);
    assert_eq!(c.get("/public/empty").await.unwrap().data, b"");
    assert_eq!(c.head("/public/example.txt").await.unwrap().size, 13);
    assert!(c.ping().await.unwrap() < std::time::Duration::from_secs(1));
    c.close().await;
}

#[tokio::test]
async fn missing_and_directory_hidden_connection_survives() {
    let env = start().await;
    env.write("public/after", b"ok");
    let mut c = env.client(None).await;
    expect_not_found(c.get("/public/nope").await);
    expect_not_found(c.get("/public").await); // directory: hidden in v1
    assert_eq!(c.get("/public/after").await.unwrap().data, b"ok"); // request-fatal only
    c.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_policy_deny_escaping() {
    let env = start().await;
    let outside = temp_dir();
    env.write("public/real.txt", b"inside");
    std::fs::write(outside.join("secret.txt"), b"outside").unwrap();
    std::os::unix::fs::symlink(env.root.join("public/real.txt"), env.root.join("public/link-inside"))
        .unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), env.root.join("public/link-escape"))
        .unwrap();

    let mut c = env.client(None).await;
    assert_eq!(c.get("/public/link-inside").await.unwrap().data, b"inside");
    expect_not_found(c.get("/public/link-escape").await);
    c.close().await;
}

#[tokio::test]
async fn client_size_cap_enforced() {
    let env = start().await;
    env.write("public/big", &vec![1u8; 100_000]);
    let mut cfg = env.config(None);
    cfg.max_resource = 50_000;
    let mut c = Client::connect(&env.addr.ip().to_string(), env.addr.port(), cfg).await.unwrap();
    match c.get("/public/big").await {
        Err(ClientError::ResourceTooLarge) => {}
        other => panic!("expected ResourceTooLarge, got {other:?}"),
    }
    match c.get("/public/big").await {
        Err(ClientError::Dead) => {}
        other => panic!("expected Dead, got {other:?}"),
    }
}

// -------------------------------------------- security matrix (security.md §8)

#[tokio::test]
async fn anonymous_public_allowed_private_hidden() {
    let env = start().await;
    env.write("public/index", b"pub");
    env.write("private/secret.txt", b"secret");

    let mut c = env.client(None).await;
    assert_eq!(c.get("/public/index").await.unwrap().data, b"pub");
    expect_not_found(c.get("/private/secret.txt").await);
    // Unmatched prefix denies too (default deny).
    expect_not_found(c.get("/other").await);
    c.close().await;
}

#[tokio::test]
async fn enrolled_device_reads_private() {
    let env = start().await;
    env.write("private/secret.txt", b"secret");
    env.write("public/index", b"pub");

    let mut c = env.client(Some(&env.enrolled)).await;
    assert_eq!(c.get("/private/secret.txt").await.unwrap().data, b"secret");
    // Enrolling never reduces access: public still works.
    assert_eq!(c.get("/public/index").await.unwrap().data, b"pub");
    c.close().await;
}

#[tokio::test]
async fn unknown_device_private_hidden() {
    let env = start().await;
    env.write("private/secret.txt", b"secret");
    env.write("public/index", b"pub");

    // Stranger presents a valid cert that isn't enrolled: treated as
    // anonymous — public yes, private hidden.
    let mut c = env.client(Some(&env.stranger)).await;
    assert_eq!(c.get("/public/index").await.unwrap().data, b"pub");
    expect_not_found(c.get("/private/secret.txt").await);
    c.close().await;
}

#[tokio::test]
async fn wrong_server_fingerprint_rejected_before_any_request() {
    let env = start().await;
    let mut cfg = env.config(None);
    cfg.server_fingerprint = "0".repeat(64); // pin that can't match
    match Client::connect(&env.addr.ip().to_string(), env.addr.port(), cfg).await {
        Err(ClientError::Wire(_)) => {} // handshake failure
        other => panic!("expected handshake rejection, got {:?}", other.err()),
    }
}

/// security.md §2's fourth identity row ("invalid cert → handshake
/// rejected"), pinned as far as this side of the wire can reach. Both
/// spoofing constructions die *inside rustls before a byte is sent*:
/// unparseable DER refuses to become a config (BadEncoding), and someone
/// else's certificate over our key refuses too (KeyMismatch) — so a
/// conforming client cannot present either. The server half of the
/// property — `verify_tls13_signature` parsing the cert and checking the
/// CertificateVerify — is rustls's own code on both ends; exercising it
/// against a hostile peer would need a hand-rolled TLS stack, and that is
/// worth knowing rather than pretending: this test documents where the
/// enforcement lives, and that neither failure downgrades anyone to an
/// anonymous session.
#[tokio::test]
async fn a_bad_client_certificate_cannot_even_be_presented() {
    let env = start().await;
    env.write("public/ok", b"fine");

    let key = rill_auth::parse_key_pem(&env.enrolled.key_pem).unwrap();
    let garbage = rill_auth::CertificateDer::from(vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]);
    assert!(
        rill_auth::client_tls_config(&env.server_fp, Some((key, garbage))).is_err(),
        "unparseable DER must not produce a sendable config"
    );

    let imposter = generate_identity("imposter").unwrap();
    let stolen_cert = parse_cert_pem(&imposter.cert_pem).unwrap();
    let our_key = rill_auth::parse_key_pem(&env.enrolled.key_pem).unwrap();
    assert!(
        rill_auth::client_tls_config(&env.server_fp, Some((our_key, stolen_cert))).is_err(),
        "a certificate over a key we do not hold must not produce a sendable config"
    );

    // And the honest device is served as ever.
    let mut c = env.client(Some(&env.enrolled)).await;
    assert_eq!(c.get("/public/ok").await.unwrap().data, b"fine");
    c.close().await;
}

/// The enrollment workflow, without reading a log line. An unknown device
/// connects; the server records its fingerprint in `pending.toml`, where
/// `rill auth pending` finds it — the fact survives the log being off, the
/// terminal being closed, and the server being restarted. Repeat knocks
/// count but do not rewrite the file.
#[tokio::test]
async fn an_unknown_device_records_itself_for_enrollment() {
    let env = start().await;
    env.write("public/ok", b"fine");

    let stranger = env.stranger.clone();
    let expected = fingerprint_hex(&parse_cert_pem(&stranger.cert_pem).unwrap());
    // Nothing pending before anyone knocks.
    assert!(
        rill_auth::PendingDevices::load(&env.identity_dir).unwrap().is_empty(),
        "an untouched server has no pending devices"
    );

    // The stranger is served as anonymous (public only) — recording the
    // fingerprint must not change what it may read.
    let mut c = env.client(Some(&stranger)).await;
    assert_eq!(c.get("/public/ok").await.unwrap().data, b"fine");
    assert!(c.get("/private/secret").await.is_err(), "still anonymous for authorization");
    c.close().await;

    let waiting = rill_auth::PendingDevices::load(&env.identity_dir).unwrap();
    let listed = waiting.list();
    assert_eq!(listed.len(), 1, "exactly the one stranger");
    assert_eq!(listed[0].fingerprint, expected, "and it is the fingerprint to enroll");
    assert_eq!(listed[0].count, 1);

    // Knocking again inside the refresh window counts but does not rewrite:
    // a client retrying in a loop is not a file write per attempt.
    let before = std::fs::metadata(rill_auth::PendingDevices::path(&env.identity_dir))
        .unwrap()
        .modified()
        .unwrap();
    let mut again = env.client(Some(&stranger)).await;
    assert_eq!(again.get("/public/ok").await.unwrap().data, b"fine");
    again.close().await;
    let after = std::fs::metadata(rill_auth::PendingDevices::path(&env.identity_dir))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after, "a repeat sighting did not rewrite the file");
    assert_eq!(
        rill_auth::PendingDevices::load(&env.identity_dir).unwrap().list().len(),
        1,
        "and did not duplicate the entry"
    );
}

#[tokio::test]
async fn hostile_bytes_rejected_server_survives() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let env = start().await;
    env.write("public/ok", b"fine");

    // Raw garbage at the TCP socket: TLS handshake fails, server drops it.
    let mut raw = tokio::net::TcpStream::connect(env.addr).await.unwrap();
    raw.write_all(b"GARBAGE-NOT-TLS-AT-ALL").await.unwrap();
    let mut sink = Vec::new();
    let _ = raw.read_to_end(&mut sink).await; // server closes

    // Well-behaved client still served afterwards.
    let mut c = env.client(None).await;
    assert_eq!(c.get("/public/ok").await.unwrap().data, b"fine");
    c.close().await;
}

#[tokio::test]
async fn wrong_request_id_is_connection_fatal_inside_tls() {
    use rill_protocol::Frame;
    use rill_wire::{Peer, read_frame, write_frame};

    let env = start().await;
    env.write("public/ok", b"fine");

    // Speak correct TLS but violate the session rules: first request ID = 5.
    let tls_config = rill_auth::client_tls_config(&env.server_fp, None).unwrap();
    let name = rill_auth::server_name(&env.addr.ip().to_string()).unwrap();
    let tcp = tokio::net::TcpStream::connect(env.addr).await.unwrap();
    let mut stream =
        rill_auth::TlsConnector::from(tls_config).connect(name, tcp).await.unwrap();
    write_frame(&mut stream, &Frame::Get { request_id: 5, path: "/public/ok".into(), accept_zstd: false })
        .await
        .unwrap();
    match read_frame(&mut stream, Peer::Server).await.unwrap() {
        Frame::Error { request_id: 0, status: Status::ProtocolMalformed, .. } => {}
        other => panic!("expected connection-fatal ERROR, got {other:?}"),
    }
    match read_frame(&mut stream, Peer::Server).await.unwrap() {
        Frame::Close => {}
        other => panic!("expected CLOSE, got {other:?}"),
    }

    // The write verb obeys the same rule, on the same terms. It is checked
    // here because ACTION and the read verbs answer to one gate rather than
    // to two copies of it, and a rule the protocol rests on should be pinned
    // for every verb that carries an id, not just the one that came first.
    let tls_config = rill_auth::client_tls_config(&env.server_fp, None).unwrap();
    let name = rill_auth::server_name(&env.addr.ip().to_string()).unwrap();
    let tcp = tokio::net::TcpStream::connect(env.addr).await.unwrap();
    let mut stream =
        rill_auth::TlsConnector::from(tls_config).connect(name, tcp).await.unwrap();
    write_frame(&mut stream, &Frame::Action {
        request_id: 7,
        path: "/public/ok".into(),
        fields: Vec::new(),
        cas: false,
    })
    .await
    .unwrap();
    match read_frame(&mut stream, Peer::Server).await.unwrap() {
        Frame::Error { request_id: 0, status: Status::ProtocolMalformed, .. } => {}
        other => panic!("ACTION: expected connection-fatal ERROR, got {other:?}"),
    }
    match read_frame(&mut stream, Peer::Server).await.unwrap() {
        Frame::Close => {}
        other => panic!("ACTION: expected CLOSE, got {other:?}"),
    }
}

/// PING and CLOSE are not requests and consume no request id — so a ping
/// mid-session must not shift the numbering underneath the next real request.
/// Worth pinning now that one gate decides this for every frame: the way to
/// get it wrong is to advance the counter for everything that arrives.
#[tokio::test]
async fn a_ping_does_not_consume_a_request_id() {
    use rill_protocol::Frame;
    use rill_wire::{Peer, read_frame, write_frame};

    let env = start().await;
    env.write("public/ok", b"fine");

    let tls_config = rill_auth::client_tls_config(&env.server_fp, None).unwrap();
    let name = rill_auth::server_name(&env.addr.ip().to_string()).unwrap();
    let tcp = tokio::net::TcpStream::connect(env.addr).await.unwrap();
    let mut stream =
        rill_auth::TlsConnector::from(tls_config).connect(name, tcp).await.unwrap();

    write_frame(&mut stream, &Frame::Ping { payload: b"beat".to_vec() }).await.unwrap();
    match read_frame(&mut stream, Peer::Server).await.unwrap() {
        Frame::Pong { payload } if payload == b"beat" => {}
        other => panic!("expected PONG, got {other:?}"),
    }
    // Still request 1, because the ping was not one.
    write_frame(&mut stream, &Frame::Get {
        request_id: 1,
        path: "/public/ok".into(),
        accept_zstd: false,
    })
    .await
    .unwrap();
    match read_frame(&mut stream, Peer::Server).await.unwrap() {
        Frame::Resource { request_id: 1, .. } => {}
        other => panic!("a ping consumed a request id: {other:?}"),
    }
}

// ------------------------------------ content addressing (resource-format.md)

#[tokio::test]
async fn conditional_fetch_uses_cache() {
    let env = start().await;
    env.write("public/page", b"version one");

    let cache_dir = temp_dir();
    let mut cfg = env.config(None);
    cfg.cache_dir = Some(cache_dir.clone());
    let mut c = Client::connect(&env.addr.ip().to_string(), env.addr.port(), cfg.clone())
        .await
        .unwrap();

    // First fetch: full download, seeds the cache.
    let first = c.get("/public/page").await.unwrap();
    assert!(!first.from_cache);
    assert_eq!(first.data, b"version one");
    assert_eq!(first.hash, rill_store::Hash::of(b"version one"));

    // Second fetch: GET_IF → NOT_MODIFIED → served from verified cache.
    let second = c.get("/public/page").await.unwrap();
    assert!(second.from_cache, "expected NOT_MODIFIED + cache hit");
    assert_eq!(second.data, b"version one");
    assert_eq!(second.hash, first.hash);

    // Change the content (different length → memo rehashes): full download.
    env.write("public/page", b"version two, longer");
    let third = c.get("/public/page").await.unwrap();
    assert!(!third.from_cache);
    assert_eq!(third.data, b"version two, longer");
    // And the ref now tracks the new hash.
    let fourth = c.get("/public/page").await.unwrap();
    assert!(fourth.from_cache);
    c.close().await;
}

#[tokio::test]
async fn corrupt_cache_entry_refetched() {
    let env = start().await;
    env.write("public/thing", b"good bytes");

    let cache_dir = temp_dir();
    let mut cfg = env.config(None);
    cfg.cache_dir = Some(cache_dir.clone());
    let mut c = Client::connect(&env.addr.ip().to_string(), env.addr.port(), cfg).await.unwrap();
    let first = c.get("/public/thing").await.unwrap();
    assert!(!first.from_cache);

    // Corrupt the stored object on disk.
    let hex = first.hash.to_hex();
    let object = cache_dir.join("objects").join(&hex[..2]).join(&hex[2..]);
    std::fs::write(&object, b"tampered!!").unwrap();

    // Next fetch: verification kills the entry, a full GET recovers.
    let second = c.get("/public/thing").await.unwrap();
    assert!(!second.from_cache, "corrupt entry must not serve");
    assert_eq!(second.data, b"good bytes");
    // And the cache is healthy again.
    let third = c.get("/public/thing").await.unwrap();
    assert!(third.from_cache);
    c.close().await;
}

#[tokio::test]
async fn identical_content_two_paths_one_object() {
    let env = start().await;
    env.write("public/a.txt", b"same bytes");
    env.write("public/b.txt", b"same bytes");

    let cache_dir = temp_dir();
    let mut cfg = env.config(None);
    cfg.cache_dir = Some(cache_dir.clone());
    let mut c = Client::connect(&env.addr.ip().to_string(), env.addr.port(), cfg).await.unwrap();
    let a = c.get("/public/a.txt").await.unwrap();
    let b = c.get("/public/b.txt").await.unwrap();
    assert_eq!(a.hash, b.hash);
    c.close().await;

    let cache = rill_store::Cache::open(&cache_dir).unwrap();
    assert_eq!(cache.objects.verify_all().unwrap().len(), 1, "one object");
    assert_eq!(cache.refs.count().unwrap(), 2, "two refs");
}

#[tokio::test]
async fn head_reports_hash() {
    let env = start().await;
    env.write("public/x", b"xyzzy");
    let mut c = env.client(None).await;
    let meta = c.head("/public/x").await.unwrap();
    assert_eq!(meta.size, 5);
    assert_eq!(meta.hash, Some(rill_store::Hash::of(b"xyzzy")));
    c.close().await;
}

#[tokio::test]
async fn unauthorized_get_if_is_not_a_hash_oracle() {
    use rill_protocol::Frame;
    use rill_wire::{Peer, read_frame, write_frame};

    let env = start().await;
    env.write("private/secret.txt", b"secret");
    let correct_hash = rill_store::Hash::of(b"secret");

    // Anonymous GET_IF with the CORRECT hash of a private file must answer
    // NOT_FOUND — never NOT_MODIFIED, which would confirm the content.
    let tls_config = rill_auth::client_tls_config(&env.server_fp, None).unwrap();
    let name = rill_auth::server_name(&env.addr.ip().to_string()).unwrap();
    let tcp = tokio::net::TcpStream::connect(env.addr).await.unwrap();
    let mut stream =
        rill_auth::TlsConnector::from(tls_config).connect(name, tcp).await.unwrap();
    write_frame(&mut stream, &Frame::GetIf {
        request_id: 1,
        path: "/private/secret.txt".into(),
        hash: correct_hash.0,
        accept_zstd: false,
    })
    .await
    .unwrap();
    match read_frame(&mut stream, Peer::Server).await.unwrap() {
        Frame::Error { status: Status::NotFound, .. } => {}
        other => panic!("expected NOT_FOUND, got {other:?}"),
    }
}

// --------------------------------------- compression (resource-format.md §8)

/// Raw frame-level client: fetch `path` and report (zstd flag, wire bytes,
/// decoded data). Panics on mixed encoding flags.
async fn raw_fetch(
    env: &Env,
    request_id: u32,
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    path: &str,
    accept_zstd: bool,
) -> (bool, usize, Vec<u8>) {
    use rill_protocol::Frame;
    use rill_wire::{Peer, read_frame, write_frame};
    let _ = env;
    write_frame(stream, &Frame::Get {
        request_id,
        path: path.into(),
        accept_zstd,
    })
    .await
    .unwrap();
    let mut wire = Vec::new();
    let mut zstd_flag = None;
    loop {
        match read_frame(stream, Peer::Server).await.unwrap() {
            Frame::Resource { more, zstd, payload, .. } => {
                match zstd_flag {
                    None => zstd_flag = Some(zstd),
                    Some(z) => assert_eq!(z, zstd, "CONTENT_ZSTD must be uniform"),
                }
                wire.extend_from_slice(&payload);
                if !more {
                    break;
                }
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    let zstd = zstd_flag.unwrap();
    let data = if zstd {
        rill_store::encoding::decompress(&wire, u64::MAX).unwrap()
    } else {
        wire.clone()
    };
    (zstd, wire.len(), data)
}

#[tokio::test]
async fn zstd_negotiated_and_policied_on_wire() {
    let env = start().await;
    let text: Vec<u8> = b"compressible text. ".repeat(2000).to_vec(); // 38 KB
    env.write("public/text.txt", &text);
    env.write("public/photo.png", &vec![0xAB; 5000]); // skip-listed extension
    env.write("public/tiny.txt", b"hi"); // below min size

    let tls_config = rill_auth::client_tls_config(&env.server_fp, None).unwrap();
    let name = rill_auth::server_name(&env.addr.ip().to_string()).unwrap();
    let tcp = tokio::net::TcpStream::connect(env.addr).await.unwrap();
    let mut s = rill_auth::TlsConnector::from(tls_config).connect(name, tcp).await.unwrap();

    // Accepting client + compressible file → zstd on the wire, smaller.
    let (zstd, wire_len, data) = raw_fetch(&env, 1, &mut s, "/public/text.txt", true).await;
    assert!(zstd);
    assert!(wire_len < text.len() / 5, "expected real compression, wire={wire_len}");
    assert_eq!(data, text);

    // Known-compressed extension → raw despite ACCEPT_ZSTD.
    let (zstd, _, data) = raw_fetch(&env, 2, &mut s, "/public/photo.png", true).await;
    assert!(!zstd);
    assert_eq!(data.len(), 5000);

    // Below the size threshold → raw.
    let (zstd, _, data) = raw_fetch(&env, 3, &mut s, "/public/tiny.txt", true).await;
    assert!(!zstd);
    assert_eq!(data, b"hi");

    // Client that doesn't accept → raw.
    let (zstd, wire_len, data) = raw_fetch(&env, 4, &mut s, "/public/text.txt", false).await;
    assert!(!zstd);
    assert_eq!(wire_len, text.len());
    assert_eq!(data, text);
}

#[tokio::test]
async fn zstd_transparent_to_cache_and_hashes() {
    let env = start().await;
    let text: Vec<u8> = b"cache me compressed. ".repeat(1000).to_vec();
    env.write("public/doc", &text);

    let mut cfg = env.config(None);
    cfg.cache_dir = Some(temp_dir());
    let mut c = Client::connect(&env.addr.ip().to_string(), env.addr.port(), cfg).await.unwrap();

    // Compressed transfer, but hash and cache see decoded bytes.
    let first = c.get("/public/doc").await.unwrap();
    assert_eq!(first.data, text);
    assert_eq!(first.hash, rill_store::Hash::of(&text));
    // Conditional flow still works on top.
    let second = c.get("/public/doc").await.unwrap();
    assert!(second.from_cache);
    assert_eq!(second.data, text);
    c.close().await;
}

#[tokio::test]
async fn decompression_bomb_guarded() {
    let env = start().await;
    // 400 KB of zeros compresses to well under the 50 KB cap: the compressed
    // stream passes the receive cap, so only the decode cap can stop it.
    env.write("public/zeros", &vec![0u8; 400_000]);
    let mut cfg = env.config(None);
    cfg.max_resource = 50_000;
    let mut c = Client::connect(&env.addr.ip().to_string(), env.addr.port(), cfg).await.unwrap();
    match c.get("/public/zeros").await {
        Err(ClientError::ResourceTooLarge) => {}
        other => panic!("expected ResourceTooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn dump_frames_tap_records_exchange() {
    let env = start().await;
    env.write("public/x", b"x");

    let dump_dir = temp_dir();
    let mut cfg = env.config(None);
    cfg.dump_frames = Some(dump_dir.clone());
    let mut c = Client::connect(&env.addr.ip().to_string(), env.addr.port(), cfg).await.unwrap();
    c.get("/public/x").await.unwrap();
    c.close().await;

    let mut names: Vec<String> = std::fs::read_dir(&dump_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["0001-tx-GET.bin", "0002-rx-RESOURCE.bin", "0003-tx-CLOSE.bin"]);
}

// --------------------------------------- declarative actions (Phase 5)

struct EchoApp;

impl rill_server::AppHandler for EchoApp {
    fn get(&self, path: &str, _id: &rill_auth::Identity) -> Option<Vec<u8>> {
        (path == "/app/index" || path == "/app").then(|| b"generated".to_vec())
    }
    fn action(
        &self,
        path: &str,
        fields: &[(String, rill_protocol::ActionValue)],
        _id: &rill_auth::Identity,
    ) -> Result<Vec<u8>, Status> {
        if path == "/app/actions/echo" {
            let title = fields.iter().find(|(n, _)| n == "title").and_then(|(_, v)| match v {
                rill_protocol::ActionValue::Str(s) => Some(s.clone()),
                _ => None,
            });
            Ok(format!("echoed: {}", title.unwrap_or_default()).into_bytes())
        } else {
            Err(Status::NotFound)
        }
    }
}

#[tokio::test]
async fn dynamic_get_and_action() {
    use rill_protocol::ActionValue;
    let env = start().await;
    // Rebuild a server with the app handler registered under /app.
    let identity_dir = temp_dir();
    let server_id = generate_identity("app-server").unwrap();
    std::fs::write(identity_dir.join("server-key.pem"), &server_id.key_pem).unwrap();
    std::fs::write(identity_dir.join("server-cert.pem"), &server_id.cert_pem).unwrap();
    let fp = fingerprint_hex(&parse_cert_pem(&server_id.cert_pem).unwrap());
    std::fs::write(
        identity_dir.join("devices.toml"),
        format!("testdev = \"{}\"\n", fingerprint_hex(&parse_cert_pem(&env.enrolled.cert_pem).unwrap())),
    )
    .unwrap();
    std::fs::write(
        identity_dir.join("policy.toml"),
        "default_access = \"deny\"\n[[rule]]\npath = \"/app/**\"\nallow = [\"testdev\"]\n",
    )
    .unwrap();
    let root = temp_dir();
    let mut server = Server::bind("127.0.0.1", 0, ServerConfig::new(&root, &identity_dir))
        .await
        .unwrap();
    server.dynamic("/app", std::sync::Arc::new(EchoApp));
    let addr = server.local_addr().unwrap();
    tokio::spawn(server.run());

    let mut cfg = ClientConfig::new(&fp);
    cfg.device = Some(env.enrolled.clone());
    let mut c = Client::connect(&addr.ip().to_string(), addr.port(), cfg).await.unwrap();

    // Dynamic GET.
    assert_eq!(c.get("/app/index").await.unwrap().data, b"generated");
    // ACTION returns a document.
    let echoed = c
        .action("/app/actions/echo", vec![("title".into(), ActionValue::Str("hi".into()))])
        .await
        .unwrap();
    assert_eq!(echoed, b"echoed: hi");
    // Unknown action path → NOT_FOUND, connection survives.
    match c.action("/app/actions/nope", vec![]).await {
        Err(ClientError::Server { status: Status::NotFound, .. }) => {}
        other => panic!("expected NOT_FOUND, got {other:?}"),
    }
    assert_eq!(c.get("/app/index").await.unwrap().data, b"generated");
    c.close().await;
}

#[tokio::test]
async fn action_authorized_before_handler() {
    use rill_protocol::ActionValue;
    // Anonymous client cannot reach a /app action (policy denies) → NOT_FOUND.
    let env = start().await;
    let mut c = env.client(None).await;
    match c
        .action("/private/actions/x", vec![("a".into(), ActionValue::Bool(true))])
        .await
    {
        Err(ClientError::Server { status: Status::NotFound, .. }) => {}
        other => panic!("expected NOT_FOUND, got {other:?}"),
    }
    c.close().await;
}

// ------------------------------ conditional actions, and the write verb ---

/// A handler with state worth racing over: one string, readable at
/// `/app/doc/data`, written by `/app/actions/write`, conditionally when the
/// caller says which revision it read.
struct DocApp {
    body: std::sync::Mutex<String>,
}

impl DocApp {
    fn new() -> std::sync::Arc<DocApp> {
        std::sync::Arc::new(DocApp { body: std::sync::Mutex::new("first".into()) })
    }
}

impl rill_server::AppHandler for DocApp {
    fn get(&self, path: &str, _id: &rill_auth::Identity) -> Option<Vec<u8>> {
        match path {
            "/app/doc/data" => Some(self.body.lock().unwrap().clone().into_bytes()),
            _ => None,
        }
    }

    fn action(
        &self,
        path: &str,
        fields: &[(String, rill_protocol::ActionValue)],
        _id: &rill_auth::Identity,
    ) -> Result<Vec<u8>, Status> {
        match path {
            "/app/actions/write" => {
                let text = fields
                    .iter()
                    .find(|(n, _)| n == "text")
                    .and_then(|(_, v)| match v {
                        rill_protocol::ActionValue::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .ok_or(Status::Internal)?;
                // Read, compare and write under one lock: two writers that
                // both passed the comparison separately would still lose one
                // of the two writes.
                let mut body = self.body.lock().unwrap();
                rill_server::verify_expected(fields, body.as_bytes())?;
                *body = text.clone();
                Ok(format!("wrote: {text}").into_bytes())
            }
            // A handler being sloppy on purpose: PATH_INVALID is a `0x01xx`
            // status, which `closes_connection()` reports as fatal.
            "/app/actions/sloppy" => Err(Status::PathInvalid),
            _ => Err(Status::NotFound),
        }
    }
}

/// Spin up a server with `handler` under `/app` and the given policy.
async fn app_server(
    policy: &str,
    handler: std::sync::Arc<dyn rill_server::AppHandler>,
    device: &rill_auth::PemIdentity,
) -> (SocketAddr, String) {
    let identity_dir = temp_dir();
    let server_id = generate_identity("app-server").unwrap();
    std::fs::write(identity_dir.join("server-key.pem"), &server_id.key_pem).unwrap();
    std::fs::write(identity_dir.join("server-cert.pem"), &server_id.cert_pem).unwrap();
    let fp = fingerprint_hex(&parse_cert_pem(&server_id.cert_pem).unwrap());
    std::fs::write(
        identity_dir.join("devices.toml"),
        format!("testdev = \"{}\"\n", fingerprint_hex(&parse_cert_pem(&device.cert_pem).unwrap())),
    )
    .unwrap();
    std::fs::write(identity_dir.join("policy.toml"), policy).unwrap();
    let root = temp_dir();
    let mut server = Server::bind("127.0.0.1", 0, ServerConfig::new(&root, &identity_dir))
        .await
        .unwrap();
    server.dynamic("/app", handler);
    let addr = server.local_addr().unwrap();
    tokio::spawn(server.run());
    (addr, fp)
}

const APP_POLICY: &str = "default_access = \"deny\"\n\
                          [[rule]]\npath = \"/app/**\"\nallow = [\"testdev\"]\n";

// ------------------------------------------- live polling, made cheap ---

/// A live page with a cheap revision, counting its own regenerations so the
/// test can see whether the server called `get` at all.
struct LivePage {
    revision: std::sync::atomic::AtomicU64,
    gets: std::sync::atomic::AtomicU64,
}

impl rill_server::AppHandler for LivePage {
    fn get(&self, path: &str, _id: &rill_auth::Identity) -> Option<Vec<u8>> {
        use std::sync::atomic::Ordering;
        matches!(path, "/app/live" | "/app/norev").then(|| {
            self.gets.fetch_add(1, Ordering::Relaxed);
            format!("content at rev {}", self.revision.load(Ordering::Relaxed)).into_bytes()
        })
    }
    fn revision(&self, path: &str, _id: &rill_auth::Identity) -> Option<u64> {
        (path == "/app/live").then(|| self.revision.load(std::sync::atomic::Ordering::Relaxed))
    }
    fn action(
        &self,
        _path: &str,
        _fields: &[(String, rill_protocol::ActionValue)],
        _id: &rill_auth::Identity,
    ) -> Result<Vec<u8>, Status> {
        Err(Status::NotFound)
    }
}

/// The live-poll promise, over the wire: an unchanged page polled with
/// GET_IF against a held hash is answered NOT_MODIFIED *without the handler
/// regenerating it*, a moved revision serves fresh bytes, and a handler
/// without a revision still gets the hash-compare fallback (regenerates,
/// but does not transfer).
#[tokio::test]
async fn an_unchanged_live_poll_costs_no_regeneration() {
    use std::sync::atomic::Ordering;
    let env = start().await;
    let page = std::sync::Arc::new(LivePage {
        revision: std::sync::atomic::AtomicU64::new(0),
        gets: std::sync::atomic::AtomicU64::new(0),
    });
    let (addr, fp) = app_server(APP_POLICY, page.clone(), &env.enrolled).await;
    let mut cfg = ClientConfig::new(&fp);
    cfg.device = Some(env.enrolled.clone());
    let mut c = Client::connect(&addr.ip().to_string(), addr.port(), cfg).await.unwrap();

    // First fetch generates, and primes the server's revision memo.
    let first = c.get_uncached("/app/live").await.unwrap();
    assert_eq!(first.data, b"content at rev 0");
    assert_eq!(page.gets.load(Ordering::Relaxed), 1);

    // The idle poll: NOT_MODIFIED without get() being called.
    let held = first.hash.0;
    assert!(c.get_if_held("/app/live", held).await.unwrap().is_none(), "unchanged");
    assert!(c.get_if_held("/app/live", held).await.unwrap().is_none(), "still unchanged");
    assert_eq!(page.gets.load(Ordering::Relaxed), 1, "no regeneration for quiet polls");

    // Content moves; the same poll now serves fresh bytes.
    page.revision.fetch_add(1, Ordering::Relaxed);
    let fresh = c.get_if_held("/app/live", held).await.unwrap().expect("changed");
    assert_eq!(fresh.data, b"content at rev 1");
    assert_eq!(page.gets.load(Ordering::Relaxed), 2);
    assert!(c.get_if_held("/app/live", fresh.hash.0).await.unwrap().is_none());
    assert_eq!(page.gets.load(Ordering::Relaxed), 2);

    // No revision → the old contract: regenerate, hash-compare, still no
    // transfer for an unchanged page.
    let norev = c.get_uncached("/app/norev").await.unwrap();
    let before = page.gets.load(Ordering::Relaxed);
    assert!(c.get_if_held("/app/norev", norev.hash.0).await.unwrap().is_none());
    assert_eq!(page.gets.load(Ordering::Relaxed), before + 1, "fallback regenerates");
    c.close().await;
}

/// The whole promise, over the wire: read a resource, hold its hash, and a
/// write conditional on that hash applies. Someone else writes; the same
/// conditional write is now refused, nothing is changed, and the connection
/// is still usable — so the caller can re-read and decide.
#[tokio::test]
async fn a_conditional_action_applies_then_conflicts_once_the_world_moves() {
    use rill_protocol::ActionValue;
    let env = start().await;
    let (addr, fp) = app_server(APP_POLICY, DocApp::new(), &env.enrolled).await;
    let mut cfg = ClientConfig::new(&fp);
    cfg.device = Some(env.enrolled.clone());
    let mut c = Client::connect(&addr.ip().to_string(), addr.port(), cfg).await.unwrap();

    // Read the resource: this is the revision the caller observed.
    let seen = c.get("/app/doc/data").await.unwrap();
    assert_eq!(seen.data, b"first");

    // Conditional on it, so it applies.
    let ok = c
        .action_if(
            "/app/actions/write",
            vec![("text".into(), ActionValue::Str("second".into()))],
            seen.hash,
        )
        .await
        .unwrap();
    assert_eq!(ok, b"wrote: second");

    // The same (now stale) revision must not apply again.
    match c
        .action_if(
            "/app/actions/write",
            vec![("text".into(), ActionValue::Str("third".into()))],
            seen.hash,
        )
        .await
    {
        Err(ClientError::Server { status: Status::Conflict, .. }) => {}
        other => panic!("expected CONFLICT, got {other:?}"),
    }

    // Nothing was written, and the connection still works — a conflict is a
    // request-scoped answer, not a fatal one.
    let now = c.get("/app/doc/data").await.unwrap();
    assert_eq!(now.data, b"second", "the refused write changed nothing");

    // Re-read, re-try, and it applies: that is the whole loop.
    let after = c
        .action_if(
            "/app/actions/write",
            vec![("text".into(), ActionValue::Str("third".into()))],
            now.hash,
        )
        .await
        .unwrap();
    assert_eq!(after, b"wrote: third");

    // And an unconditional write still works, because conditionality is the
    // caller's choice and most actions do not want it.
    c.action("/app/actions/write", vec![("text".into(), ActionValue::Str("fourth".into()))])
        .await
        .unwrap();
    assert_eq!(c.get("/app/doc/data").await.unwrap().data, b"fourth");
    c.close().await;
}

/// An identity may be allowed to read a resource and not to act on it. The
/// refusal is NOT_FOUND, exactly like absence — a reader must not be able to
/// discover which actions exist by being denied them.
#[tokio::test]
async fn a_reader_may_be_denied_actions_and_cannot_tell_them_from_absence() {
    use rill_protocol::ActionValue;
    let env = start().await;
    let policy = "default_access = \"deny\"\n\
                  [[rule]]\npath = \"/app/**\"\nallow = [\"testdev\"]\nallow_actions = []\n";
    let (addr, fp) = app_server(policy, DocApp::new(), &env.enrolled).await;
    let mut cfg = ClientConfig::new(&fp);
    cfg.device = Some(env.enrolled.clone());
    let mut c = Client::connect(&addr.ip().to_string(), addr.port(), cfg).await.unwrap();

    // Reading is allowed.
    assert_eq!(c.get("/app/doc/data").await.unwrap().data, b"first");

    // Acting is not — and the action that exists is indistinguishable from
    // the one that does not.
    let real = c
        .action("/app/actions/write", vec![("text".into(), ActionValue::Str("x".into()))])
        .await;
    let imaginary = c.action("/app/actions/nonexistent", vec![]).await;
    for (what, result) in [("existing", real), ("imaginary", imaginary)] {
        match result {
            Err(ClientError::Server { status: Status::NotFound, message }) => {
                assert_eq!(message, "not found", "{what}: the message must not vary either");
            }
            other => panic!("{what}: expected NOT_FOUND, got {other:?}"),
        }
    }

    // Nothing was written, and reads still work.
    assert_eq!(c.get("/app/doc/data").await.unwrap().data, b"first");
    c.close().await;
}

/// A handler's status vocabulary is a protocol surface. PATH_INVALID is
/// connection-fatal by class, so a handler returning it for a bad field used
/// to kill the client's connection; and a status that distinguishes "your
/// parameter was wrong" from "no such thing" is an oracle. Both are clamped.
#[tokio::test]
async fn a_handlers_status_cannot_kill_the_connection_or_leak_a_distinction() {
    let env = start().await;
    let (addr, fp) = app_server(APP_POLICY, DocApp::new(), &env.enrolled).await;
    let mut cfg = ClientConfig::new(&fp);
    cfg.device = Some(env.enrolled.clone());
    let mut c = Client::connect(&addr.ip().to_string(), addr.port(), cfg).await.unwrap();

    match c.action("/app/actions/sloppy", vec![]).await {
        Err(ClientError::Server { status: Status::Internal, .. }) => {}
        other => panic!("expected INTERNAL, got {other:?}"),
    }
    // The connection survived, which is the part that used to be false.
    assert_eq!(c.get("/app/doc/data").await.unwrap().data, b"first");
    c.close().await;
}
