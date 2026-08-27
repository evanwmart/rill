//! Rill client library (`specs/connection.md`, `specs/security.md`):
//! `rill://` address parsing and a sequential (one request in flight) TLS
//! connection to a Rill server, verified by pinned fingerprint.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use rill_auth::{
    AuthError, ClientTlsStream, PemIdentity, TlsConnector, client_tls_config, parse_cert_pem,
    parse_key_pem, server_name,
};
use rill_protocol::{Frame, Status, validate_path};
use rill_store::{Cache, Hash};
use rill_wire::{FrameDump, Peer, WireError, dump, read_frame, write_frame};
use tokio::net::TcpStream;
use tokio::time::timeout;

type TlsStream = ClientTlsStream<TcpStream>;

/// Client-side conventions shared by the CLI and the viewer: default
/// directories and pinned-fingerprint lookup.
pub mod util {
    use std::path::{Path, PathBuf};

    use rill_auth::{PemIdentity, Pins, load_pem_identity};

    /// `RILL_IDENTITY` env override, else `~/.config/rill`.
    pub fn default_identity_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("RILL_IDENTITY") {
            return dir.into();
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Path::new(&home).join(".config").join("rill")
    }

    /// `RILL_CACHE` env override, else `~/.cache/rill`.
    pub fn default_cache_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("RILL_CACHE") {
            return dir.into();
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Path::new(&home).join(".cache").join("rill")
    }

    /// The pinned server fingerprint (required) and device identity
    /// (optional) for connecting to `host:port`.
    pub fn client_identity_for(
        dir: &Path,
        host: &str,
        port: u16,
    ) -> Result<(String, Option<PemIdentity>), String> {
        let pins = Pins::load(dir).map_err(|e| e.to_string())?;
        let fp = pins.get(host, port).ok_or_else(|| {
            format!(
                "no pinned fingerprint for {host}:{port} — run: rill auth trust rill://{host}:{port}"
            )
        })?;
        let device = load_pem_identity(dir, "device").map_err(|e| e.to_string())?;
        Ok((fp.to_string(), device))
    }
}

/// A completed fetch (resource-format.md §3).
#[derive(Debug)]
pub struct Fetched {
    pub data: Vec<u8>,
    /// BLAKE3 of `data`, computed locally — never trusted from the wire.
    pub hash: Hash,
    /// True when the server answered NOT_MODIFIED and the (re-verified)
    /// cache served the bytes.
    pub from_cache: bool,
}

/// HEAD result: METADATA v2 fields.
#[derive(Debug)]
pub struct Meta {
    pub size: u64,
    pub hash: Option<Hash>,
}

/// A parsed `rill://host[:port]/path` address (connection.md §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RillUrl {
    pub host: String,
    pub port: u16,
    /// Resource path, verbatim, leading `/` included.
    pub path: String,
}

pub const DEFAULT_PORT: u16 = 7331;

impl RillUrl {
    pub fn parse(input: &str) -> Result<RillUrl, ClientError> {
        let bad = |m: &str| ClientError::Url(format!("{m}: {input}"));
        let rest = input
            .strip_prefix("rill://")
            .ok_or_else(|| bad("address must start with rill://"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => return Err(bad("missing resource path")),
        };
        let (host, port_str) = if let Some(v6) = authority.strip_prefix('[') {
            let end = v6.find(']').ok_or_else(|| bad("unterminated IPv6 literal"))?;
            let after = &v6[end + 1..];
            let port = after.strip_prefix(':');
            if !after.is_empty() && port.is_none() {
                return Err(bad("garbage after IPv6 literal"));
            }
            (&v6[..end], port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err(bad("empty host"));
        }
        let port = match port_str {
            Some(p) => p.parse::<u16>().map_err(|_| bad("invalid port"))?,
            None => DEFAULT_PORT,
        };
        validate_path(path).map_err(|e| bad(&format!("invalid path ({e})")))?;
        Ok(RillUrl { host: host.to_string(), port, path: path.to_string() })
    }
}

/// Client-side knobs (connection.md §6 defaults, security.md §3 identity).
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Pinned SHA-256 fingerprint of the server certificate (required — the
    /// client refuses to speak to a server it can't verify).
    pub server_fingerprint: String,
    /// This device's key + certificate; `None` connects anonymously.
    pub device: Option<PemIdentity>,
    /// Content cache root (resource-format.md §4); `None` disables caching
    /// and conditional requests entirely.
    pub cache_dir: Option<PathBuf>,
    /// Ceiling for the on-disk cache. Exceeding it makes the next connect
    /// eligible to collect unreferenced objects — a live page rewrites its
    /// document on a clock, and without this the store grows forever.
    pub cache_budget: u64,
    /// Advertise zstd support on requests (resource-format.md §8).
    pub accept_zstd: bool,
    pub connect_timeout: Duration,
    pub first_byte_timeout: Duration,
    pub inter_chunk_timeout: Duration,
    pub max_resource: u64,
    pub dump_frames: Option<PathBuf>,
}

/// Largest single resource a client will buffer, and the decompression-bomb
/// cap (resource-format.md §8).
///
/// This was 1 GiB, which on the 1 GB target is not a limit: the OOM killer
/// arrives long before the check does, so the guard that exists to turn a
/// hostile response into an error turned it into a dead desktop instead. A
/// cap is only a cap if the machine survives reaching it.
///
/// 32 MiB is far above what the format is for — documents are kilobytes over
/// the wire, and the largest thing anyone legitimately fetches is an image or
/// a font — and small enough that a client can hold one while still running.
/// A caller who genuinely wants a large download (a pack, a backup) raises
/// `max_resource` for that request, which is the deliberate act it should be.
pub const DEFAULT_MAX_RESOURCE: u64 = 32 * 1024 * 1024;

impl ClientConfig {
    pub fn new(server_fingerprint: impl Into<String>) -> ClientConfig {
        ClientConfig {
            server_fingerprint: server_fingerprint.into(),
            device: None,
            cache_dir: None,
            cache_budget: Cache::DEFAULT_BUDGET,
            accept_zstd: true,
            connect_timeout: Duration::from_secs(10),
            first_byte_timeout: Duration::from_secs(30),
            inter_chunk_timeout: Duration::from_secs(30),
            max_resource: DEFAULT_MAX_RESOURCE,
            dump_frames: None,
        }
    }
}

#[derive(Debug)]
pub enum ClientError {
    Url(String),
    /// Identity/TLS configuration problem (bad PEM, bad fingerprint, ALPN).
    Auth(AuthError),
    Wire(WireError),
    /// Server answered with an ERROR frame.
    Server { status: Status, message: String },
    /// Response frame that the request grammar (connection.md §4) forbids.
    UnexpectedFrame(&'static str),
    /// Response carried a request ID other than the outstanding one.
    MismatchedId { expected: u32, got: u32 },
    /// Stream exceeded `max_resource` — server misbehaving; connection dropped.
    ResourceTooLarge,
    Timeout(&'static str),
    /// Connection already failed or was closed; make a new client.
    Dead,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Url(m) => write!(f, "invalid address — {m}"),
            ClientError::Auth(e) => write!(f, "{e}"),
            ClientError::Wire(e) => write!(f, "{e}"),
            ClientError::Server { status, message } if message.is_empty() => {
                write!(f, "server error: {status}")
            }
            ClientError::Server { status, message } => {
                write!(f, "server error: {status} — {message}")
            }
            ClientError::UnexpectedFrame(t) => write!(f, "unexpected {t} frame"),
            ClientError::MismatchedId { expected, got } => {
                write!(f, "response for request {got}, expected {expected}")
            }
            ClientError::ResourceTooLarge => write!(f, "resource exceeds configured size cap"),
            ClientError::Timeout(what) => write!(f, "timed out waiting for {what}"),
            ClientError::Dead => write!(f, "connection no longer usable"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<WireError> for ClientError {
    fn from(e: WireError) -> ClientError {
        ClientError::Wire(e)
    }
}

/// What a response stream resolved to.
enum Outcome {
    Data(Vec<u8>),
    NotModified,
}

/// Run a sweep if one is due, and say so when anything was collected.
///
/// Cheap to call often: [`Cache::sweep_if_due`] gates on one stat of its
/// stamp file and returns immediately unless the interval has elapsed *and*
/// the store is over budget. That is what lets both the connect path and the
/// per-store path call it without either needing to know about the other.
fn sweep_and_report(cache: &Cache, budget: u64) {
    if let Some(swept) = cache.sweep_if_due(budget)
        && swept.removed > 0
    {
        eprintln!(
            "rill-client: cache swept {} unreferenced objects ({:.1} MiB), {} kept",
            swept.removed,
            swept.freed_bytes as f64 / 1048576.0,
            swept.kept
        );
    }
}

/// One TLS connection to a Rill server. Sequential: one request in flight.
pub struct Client {
    stream: TlsStream,
    cfg: ClientConfig,
    /// "host:port" — the cache ref namespace for this connection.
    authority: String,
    cache: Option<Cache>,
    next_id: u32,
    ping_counter: u64,
    dump: Option<FrameDump>,
    dead: bool,
}

impl Client {
    pub async fn connect(host: &str, port: u16, cfg: ClientConfig) -> Result<Client, ClientError> {
        let dump = match &cfg.dump_frames {
            Some(dir) => Some(FrameDump::new(dir).map_err(|e| ClientError::Wire(WireError::Io(e)))?),
            None => None,
        };

        let device = match &cfg.device {
            Some(id) => Some((
                parse_key_pem(&id.key_pem).map_err(ClientError::Auth)?,
                parse_cert_pem(&id.cert_pem).map_err(ClientError::Auth)?,
            )),
            None => None,
        };
        let tls_config = client_tls_config(&cfg.server_fingerprint, device)
            .map_err(ClientError::Auth)?;
        let name = server_name(host).map_err(ClientError::Auth)?;

        let tcp = timeout(cfg.connect_timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| ClientError::Timeout("connect"))?
            .map_err(|e| ClientError::Wire(WireError::Io(e)))?;
        tcp.set_nodelay(true).map_err(|e| ClientError::Wire(WireError::Io(e)))?;

        // Handshake failure here includes "server fingerprint ≠ pinned"
        // (security.md §8): nothing is sent to an unverified server.
        let stream = timeout(cfg.connect_timeout, TlsConnector::from(tls_config).connect(name, tcp))
            .await
            .map_err(|_| ClientError::Timeout("TLS handshake"))?
            .map_err(|e| ClientError::Wire(WireError::Io(e)))?;
        if stream.get_ref().1.alpn_protocol() != Some(rill_auth::ALPN) {
            return Err(ClientError::Auth(AuthError(
                "server did not negotiate ALPN rill/1".into(),
            )));
        }
        let cache = match &cfg.cache_dir {
            Some(dir) => {
                let cache = Cache::open(dir).map_err(|e| ClientError::Wire(WireError::Io(e)))?;
                // Collect on connect: a client that is connecting is a client
                // that is not yet waiting on anything. This is not the only
                // trigger — see `finish_fetch` — because a desktop connects
                // once and then stays connected for days.
                sweep_and_report(&cache, cfg.cache_budget);
                Some(cache)
            }
            None => None,
        };
        Ok(Client {
            stream,
            cfg,
            authority: format!("{host}:{port}"),
            cache,
            next_id: 1,
            ping_counter: 0,
            dump,
            dead: false,
        })
    }

    /// Fetch a resource, using the cache and conditional requests when a
    /// cache is configured (resource-format.md §3). Fails (and poisons the
    /// connection) if the stream exceeds `max_resource`.
    pub async fn get(&mut self, path: &str) -> Result<Fetched, ClientError> {
        self.get_inner(path, true).await
    }

    /// Fetch without reading or writing the cache.
    ///
    /// For a page that is *expected* to differ every time it is asked for —
    /// a `live` document on a clock — the cache is not just useless but
    /// actively harmful: the conditional request can never hit, and every
    /// response writes an object that the ref immediately orphans. An ASCII
    /// widget at 8 Hz is otherwise a disk writer.
    pub async fn get_uncached(&mut self, path: &str) -> Result<Fetched, ClientError> {
        self.get_inner(path, false).await
    }

    /// Conditional fetch against a hash the caller holds in memory, with the
    /// disk cache uninvolved on both sides. `Ok(None)` means NOT_MODIFIED:
    /// the bytes the caller already has are current.
    ///
    /// This is the live-poll verb: a page on a clock usually has NOT changed
    /// since the last tick, and this makes that case cost a hash comparison
    /// instead of a transfer — without the disk-cache write traffic that
    /// [`Client::get`] would add on every tick that did change.
    pub async fn get_if_held(
        &mut self,
        path: &str,
        known: [u8; 32],
    ) -> Result<Option<Fetched>, ClientError> {
        let id = self.start_request()?;
        let accept_zstd = self.cfg.accept_zstd;
        self.send(&Frame::GetIf { request_id: id, path: path.to_string(), hash: known, accept_zstd })
            .await?;
        match self.collect(id, true).await? {
            Outcome::NotModified => Ok(None),
            Outcome::Data(data) => self.finish_fetch(path, data, false).map(Some),
        }
    }

    async fn get_inner(&mut self, path: &str, cached: bool) -> Result<Fetched, ClientError> {
        // Conditional path: we hold a ref and a verifiable object.
        if let Some(known) = cached
            .then(|| self.cache.as_ref().and_then(|c| c.known_hash(&self.authority, path)))
            .flatten()
        {
            let id = self.start_request()?;
            let accept_zstd = self.cfg.accept_zstd;
            self.send(&Frame::GetIf {
                request_id: id,
                path: path.to_string(),
                hash: known.0,
                accept_zstd,
            })
            .await?;
            match self.collect(id, true).await? {
                Outcome::NotModified => {
                    // Cache read re-verifies; a corrupt entry falls back to
                    // a plain GET (the store already deleted it).
                    if let Some((hash, data)) =
                        self.cache.as_ref().and_then(|c| c.lookup(&self.authority, path))
                    {
                        return Ok(Fetched { data, hash, from_cache: true });
                    }
                }
                Outcome::Data(data) => return self.finish_fetch(path, data, cached),
            }
        }
        // Plain path (no cache, no known hash, or corrupt cache entry).
        let id = self.start_request()?;
        let accept_zstd = self.cfg.accept_zstd;
        self.send(&Frame::Get { request_id: id, path: path.to_string(), accept_zstd }).await?;
        match self.collect(id, false).await? {
            Outcome::Data(data) => self.finish_fetch(path, data, cached),
            Outcome::NotModified => unreachable!("collect(false) rejects NOT_MODIFIED"),
        }
    }

    /// Hash received bytes (never trusting the wire) and seed the cache.
    fn finish_fetch(
        &mut self,
        path: &str,
        data: Vec<u8>,
        cached: bool,
    ) -> Result<Fetched, ClientError> {
        let hash = match &self.cache {
            Some(cache) if cached => {
                let hash = cache
                    .store(&self.authority, path, &data)
                    .map_err(|e| ClientError::Wire(WireError::Io(e)))?;
                // Storing is the moment garbage is created: the ref just
                // moved off whatever object it named before, and nothing
                // else will ever remove that object. Sweeping only on
                // connect fires approximately once per process, which is
                // approximately never on the long-lived desktop sessions
                // that are the whole reason the store grows. The gate here
                // is one stat of the stamp file; the walk itself is still
                // held to SWEEP_INTERVAL inside `sweep_if_due`.
                sweep_and_report(cache, self.cfg.cache_budget);
                hash
            }
            _ => Hash::of(&data),
        };
        Ok(Fetched { data, hash, from_cache: false })
    }

    /// Receive one request's response stream, decompressing a zstd-encoded
    /// stream after the final chunk (resource-format.md §8: the compressed
    /// stream AND the decoded output are both capped at `max_resource`).
    async fn collect(&mut self, id: u32, allow_not_modified: bool) -> Result<Outcome, ClientError> {
        let mut data: Vec<u8> = Vec::new();
        let mut first = true;
        let mut stream_zstd = false;
        loop {
            let wait = if first { self.cfg.first_byte_timeout } else { self.cfg.inter_chunk_timeout };
            let frame = self.recv(wait, if first { "first response" } else { "next chunk" }).await?;
            match frame {
                Frame::Resource { request_id, more, zstd, payload } => {
                    self.check_id(id, request_id)?;
                    if first {
                        stream_zstd = zstd;
                        if zstd && !self.cfg.accept_zstd {
                            self.dead = true;
                            return Err(ClientError::UnexpectedFrame("RESOURCE (unrequested zstd)"));
                        }
                    } else if zstd != stream_zstd {
                        // CONTENT_ZSTD must be uniform across the response.
                        self.dead = true;
                        return Err(ClientError::UnexpectedFrame("RESOURCE (mixed encoding)"));
                    }
                    if data.len() as u64 + payload.len() as u64 > self.cfg.max_resource {
                        self.dead = true;
                        return Err(ClientError::ResourceTooLarge);
                    }
                    data.extend_from_slice(&payload);
                    if !more {
                        if stream_zstd {
                            data = rill_store::encoding::decompress(&data, self.cfg.max_resource)
                                .map_err(|e| {
                                    self.dead = true;
                                    if e.kind() == std::io::ErrorKind::FileTooLarge {
                                        ClientError::ResourceTooLarge
                                    } else {
                                        ClientError::Wire(WireError::Io(e))
                                    }
                                })?;
                        }
                        return Ok(Outcome::Data(data));
                    }
                }
                Frame::NotModified { request_id } if allow_not_modified && first => {
                    self.check_id(id, request_id)?;
                    return Ok(Outcome::NotModified);
                }
                other => return self.non_resource(id, other),
            }
            first = false;
        }
    }

    /// Submit a typed request (the protocol's write verb). The response is a
    /// document. Never retried automatically; never cached.
    pub async fn action(
        &mut self,
        path: &str,
        fields: Vec<(String, rill_protocol::ActionValue)>,
    ) -> Result<Vec<u8>, ClientError> {
        self.send_action(path, fields, None).await
    }

    /// Submit conditionally: apply only if `expected` is still the hash of
    /// the resource the action names. Otherwise the server answers
    /// [`Status::Conflict`] and nothing is written.
    ///
    /// This is what makes a mutation safe to send after reading — a minute
    /// ago or three commands ago, since the hash is derived from content
    /// rather than from a session. A stale caller cannot silently overwrite
    /// a world that has moved on.
    pub async fn action_if(
        &mut self,
        path: &str,
        fields: Vec<(String, rill_protocol::ActionValue)>,
        expected: Hash,
    ) -> Result<Vec<u8>, ClientError> {
        self.send_action(path, fields, Some(expected)).await
    }

    async fn send_action(
        &mut self,
        path: &str,
        mut fields: Vec<(String, rill_protocol::ActionValue)>,
        expected: Option<Hash>,
    ) -> Result<Vec<u8>, ClientError> {
        let cas = expected.is_some();
        if let Some(hash) = expected {
            // The reserved field carries the revision; the critical flag is
            // what stops a server that predates conditions from applying one
            // unconditionally.
            fields.retain(|(name, _)| name != rill_protocol::FIELD_EXPECTED);
            fields.push((
                rill_protocol::FIELD_EXPECTED.to_string(),
                rill_protocol::ActionValue::Str(hash.to_string()),
            ));
        }
        let id = self.start_request()?;
        self.send(&Frame::Action { request_id: id, path: path.to_string(), fields, cas }).await?;
        match self.collect(id, false).await? {
            Outcome::Data(data) => Ok(data),
            Outcome::NotModified => unreachable!("collect(false) rejects NOT_MODIFIED"),
        }
    }

    /// Fetch resource metadata (size, and hash when the server sends
    /// METADATA v2).
    pub async fn head(&mut self, path: &str) -> Result<Meta, ClientError> {
        let id = self.start_request()?;
        self.send(&Frame::Head { request_id: id, path: path.to_string() }).await?;
        match self.recv(self.cfg.first_byte_timeout, "metadata").await? {
            Frame::Metadata { request_id, size, hash } => {
                self.check_id(id, request_id)?;
                Ok(Meta { size, hash: hash.map(Hash) })
            }
            other => self.non_resource(id, other),
        }
    }

    /// Liveness check; returns round-trip time.
    pub async fn ping(&mut self) -> Result<Duration, ClientError> {
        if self.dead {
            return Err(ClientError::Dead);
        }
        self.ping_counter += 1;
        let payload = self.ping_counter.to_be_bytes().to_vec();
        let started = tokio::time::Instant::now();
        self.send(&Frame::Ping { payload: payload.clone() }).await?;
        match self.recv(self.cfg.first_byte_timeout, "pong").await? {
            Frame::Pong { payload: echoed } if echoed == payload => Ok(started.elapsed()),
            Frame::Pong { .. } => {
                // Garbled echo: protocol.md §7.3a says close.
                self.dead = true;
                Err(ClientError::UnexpectedFrame("PONG (bad echo)"))
            }
            other => self.non_resource(0, other),
        }
    }

    /// Clean close (connection.md §2): send CLOSE, shut down the write half.
    pub async fn close(mut self) {
        if !self.dead {
            let _ = self.send(&Frame::Close).await;
        }
        use tokio::io::AsyncWriteExt;
        let _ = self.stream.shutdown().await;
    }

    fn start_request(&mut self) -> Result<u32, ClientError> {
        if self.dead {
            return Err(ClientError::Dead);
        }
        let id = self.next_id;
        // Request id 0 is reserved, so wrapping does not merely repeat ids —
        // it starts producing frames the encoder rejects, and the connection
        // is finished either way. Say so, rather than panicking in debug and
        // failing obscurely in release.
        self.next_id = self.next_id.checked_add(1).ok_or_else(|| {
            self.dead = true;
            ClientError::Dead
        })?;
        Ok(id)
    }

    async fn send(&mut self, frame: &Frame) -> Result<(), ClientError> {
        dump(&mut self.dump, true, frame);
        write_frame(&mut self.stream, frame).await.inspect_err(|_| self.dead = true)?;
        Ok(())
    }

    async fn recv(&mut self, wait: Duration, what: &'static str) -> Result<Frame, ClientError> {
        let frame = timeout(wait, read_frame(&mut self.stream, Peer::Server))
            .await
            .map_err(|_| {
                self.dead = true;
                ClientError::Timeout(what)
            })?
            .inspect_err(|_| self.dead = true)?;
        dump(&mut self.dump, false, &frame);
        Ok(frame)
    }

    fn check_id(&mut self, expected: u32, got: u32) -> Result<(), ClientError> {
        if expected != got {
            self.dead = true;
            return Err(ClientError::MismatchedId { expected, got });
        }
        Ok(())
    }

    /// Shared handling for every frame a request may receive besides its
    /// success frames: ERROR resolves the request; anything else is a
    /// grammar violation (connection.md §4).
    fn non_resource<T>(&mut self, id: u32, frame: Frame) -> Result<T, ClientError> {
        match frame {
            Frame::Error { request_id, status, message } => {
                if request_id == 0 || status.closes_connection() {
                    // Connection-level error: nothing further will be usable.
                    self.dead = true;
                } else if request_id != id {
                    return Err(ClientError::MismatchedId { expected: id, got: request_id })
                        .inspect_err(|_| self.dead = true);
                }
                Err(ClientError::Server { status, message })
            }
            other => {
                self.dead = true;
                Err(ClientError::UnexpectedFrame(other.frame_type().name()))
            }
        }
    }
}
