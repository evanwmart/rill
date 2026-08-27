//! Rill server library: TLS acceptance and identity (specs/security.md),
//! dispatch pipeline and root jail (specs/connection.md §7–§8). The binary in
//! `main.rs` is a thin argument parser over [`Server`]; tests spawn
//! [`Server`] in-process on port 0.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use rill_auth::{
    Access, DeviceAuth, DeviceRegistry, Identity, Policy, TlsAcceptor, fingerprint_hex,
    load_pem_identity, parse_cert_pem, parse_key_pem, server_tls_config,
};
use rill_protocol::{ActionValue, DEFAULT_CHUNK, Frame, Status};
use rill_store::Hash;
use rill_wire::{FrameDump, Peer, WireError, dump, read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::timeout;

use rill_log::{Level, level as log_level, push_field};

macro_rules! log {
    ($level:ident, $conn:expr, $event:expr $(, $key:ident = $value:expr)* $(,)?) => {
        // The threshold test still guards the *formatting*; the dev trail
        // gets the line regardless of threshold, which is its whole point.
        if Level::$level <= log_level() || rill_log::dev_active() {
            #[allow(unused_mut)]
            let mut fields = String::new();
            $( push_field(&mut fields, stringify!($key), &$value.to_string()); )*
            rill_log::emit("rill-server", Level::$level, $conn, $event, &fields);
        }
    };
}

/// Record an unknown device's fingerprint in `pending.toml`, so
/// `rill auth pending` can show it later.
///
/// Best-effort on purpose. This is a convenience for the operator, not a
/// security control — nothing downstream reads it to make a decision — so a
/// read-only identity directory or a full disk must cost a warning, never
/// the connection. Rate-limiting lives in `record`, which reports whether
/// anything is worth writing; a client retrying in a loop therefore costs a
/// counter bump and no syscall.
fn note_pending(
    pending: &Mutex<rill_auth::PendingDevices>,
    identity_dir: &Path,
    fingerprint: &str,
    conn: u64,
) {
    let mut list = match pending.lock() {
        Ok(list) => list,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !list.record(fingerprint, rill_auth::unix_now()) {
        return;
    }
    if let Err(e) = list.save(identity_dir) {
        log!(Warn, conn, "pending-write-failed", error = e);
    }
}


/// Server-side hash memo (resource-format.md §5): canonical path →
/// (mtime, size, hash). Rehash only when mtime or size changes. Accepted
/// caveat: an edit preserving both serves a stale NOT_MODIFIED until restart.
#[derive(Default)]
struct HashMemo {
    map: Mutex<HashMap<PathBuf, MemoEntry>>,
    clock: std::sync::atomic::AtomicU64,
}

struct MemoEntry {
    mtime: SystemTime,
    size: u64,
    hash: Hash,
    used: u64,
}

impl HashMemo {
    /// One entry is a path plus ~56 bytes, so this bounds the memo at a few
    /// hundred KiB however many distinct files a long-lived server is asked
    /// for. Eviction only costs a re-hash on the next request for that path.
    const MAX_ENTRIES: usize = 4096;

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// A memo hit for this exact (path, mtime, size), freshening its stamp.
    fn cached(&self, canonical: &Path, mtime: SystemTime, size: u64) -> Option<Hash> {
        let now = self.tick();
        let mut map = self.map.lock().unwrap();
        let e = map.get_mut(canonical)?;
        ((e.mtime, e.size) == (mtime, size)).then(|| {
            e.used = now;
            e.hash
        })
    }

    /// Record a hash, evicting the least-recently-used entry at `cap`.
    fn remember(&self, cap: usize, canonical: &Path, mtime: SystemTime, size: u64, hash: Hash) {
        let now = self.tick();
        let mut map = self.map.lock().unwrap();
        if map.len() >= cap && !map.contains_key(canonical) {
            // O(n) scan at eviction time; at this cap that is cheaper than
            // keeping an ordered structure current on every hit.
            if let Some(oldest) = map.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| k.clone()) {
                map.remove(&oldest);
            }
        }
        map.insert(canonical.to_path_buf(), MemoEntry { mtime, size, hash, used: now });
    }

    /// Hash via the already-open handle (rewound afterwards so the caller
    /// can stream it), consulting the memo first.
    async fn current(
        &self,
        canonical: &Path,
        file: &mut tokio::fs::File,
        mtime: SystemTime,
        size: u64,
    ) -> io::Result<Hash> {
        if let Some(hash) = self.cached(canonical, mtime, size) {
            return Ok(hash);
        }
        let mut hasher = rill_store::Hasher::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        file.rewind().await?;
        let hash = hasher.finalize();
        self.remember(HashMemo::MAX_ENTRIES, canonical, mtime, size, hash);
        Ok(hash)
    }
}

/// Dynamic-path revision memo: path → (handler revision, hash of the bytes
/// served for it). Lets a GET_IF answer NOT_MODIFIED without regenerating,
/// for handlers that implement [`AppHandler::revision`].
///
/// Correctness sketch: `fresh` only answers yes when the handler's *current*
/// revision equals the memoized one AND the client's hash equals the
/// memoized hash. The revision recorded is the one read *before* the bytes
/// were generated; with a monotonic revision (the trait contract) the bytes
/// can only be newer than the recorded stamp, so "revision still equal"
/// pins them to exactly that content. A stale memo can only cost a spare
/// regeneration, never serve a stale NOT_MODIFIED.
#[derive(Default)]
struct RevMemo {
    map: Mutex<HashMap<String, (u64, Hash)>>,
}

impl RevMemo {
    /// Entries are a path plus 40 bytes; live pages are few and hot, so a
    /// rare full clear (one regeneration per page) beats bookkeeping.
    const MAX_ENTRIES: usize = 1024;

    fn fresh(&self, path: &str, revision: u64, client_hash: &[u8; 32]) -> bool {
        self.map.lock().unwrap().get(path).is_some_and(|(rev, hash)| {
            *rev == revision && hash.0 == *client_hash
        })
    }

    fn remember(&self, path: &str, revision: u64, hash: Hash) {
        let mut map = self.map.lock().unwrap();
        if map.len() >= RevMemo::MAX_ENTRIES && !map.contains_key(path) {
            map.clear();
        }
        map.insert(path.to_string(), (revision, hash));
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Directory served as the resource tree. Canonicalized at bind time.
    pub root: PathBuf,
    /// Identity directory: server-key.pem, server-cert.pem, devices.toml,
    /// policy.toml (security.md §4). Required — there is no plaintext mode.
    pub identity_dir: PathBuf,
    /// Idle timeout while waiting for the *first byte* of the next request
    /// (connection.md §6).
    pub idle_timeout: Duration,
    /// Once a frame's bytes start arriving, the whole frame must complete
    /// within this budget (connection.md §6). Distinct from `idle_timeout` so a
    /// client cannot trickle one frame across the full idle window (slowloris).
    pub frame_timeout: Duration,
    /// Per-frame write timeout.
    pub write_timeout: Duration,
    /// TLS handshake must complete within this.
    pub handshake_timeout: Duration,
    /// Maximum concurrent connections; excess accepts are dropped immediately.
    pub max_connections: usize,
    /// Maximum concurrent connections from any one **remote** address.
    ///
    /// Without this the global cap is the only cap, so a single peer opening
    /// `max_connections` slow handshakes holds the pool and everyone else is
    /// refused. That is a cheap denial of service against a server anyone can
    /// reach, and the handshake timeout only bounds how long each attempt
    /// lasts, not how many a peer may hold.
    ///
    /// **Loopback is exempt, deliberately.** With the default bind every
    /// connection comes from `127.0.0.1` — the desktop's own windows, the
    /// dock, every app client — so counting them per address would be
    /// counting the whole machine against one number, and a cap that fits a
    /// remote attacker would break a desktop with a dozen windows open. A
    /// local process has the machine already; this limit exists for peers
    /// that don't.
    pub max_connections_per_source: usize,
    /// RESOURCE chunk size (protocol.md §9: sender policy, ≤ MAX_PAYLOAD).
    pub chunk_size: usize,
    /// zstd compression level for eligible responses (resource-format.md §8).
    pub zstd_level: i32,
    /// Never compress resources smaller than this.
    pub zstd_min_size: u64,
    /// Debug tap directory (connection.md §10); one subdirectory per connection.
    pub dump_frames: Option<PathBuf>,
}

use rill_store::encoding::compressible_path as compressible;

/// Wire-byte accounting. Counts **protocol bytes** — plaintext as framed,
/// after TLS decryption / before encryption — so TLS record and handshake
/// overhead are not included (the loopback *interface* totals recorded by
/// bench-device.sh cover those, coarsely). Compressed responses count at
/// their compressed size: this is what actually crossed the protocol layer.
///
/// One instance holds the server's lifetime totals (read it via
/// [`Server::wire_stats`], or set `RILL_STATS=<path>` to have the server
/// write a JSON snapshot every 5 s); a second per-connection instance is
/// tallied on the same reads/writes and logged when the connection closes.
#[derive(Default)]
pub struct WireStats {
    /// Bytes received from clients (request frames).
    pub rx: AtomicU64,
    /// Bytes sent to clients (response frames).
    pub tx: AtomicU64,
    /// Connections that completed the TLS handshake with rill/1 ALPN.
    pub connections: AtomicU64,
}

impl WireStats {
    /// (rx_bytes, tx_bytes, connections) — a consistent-enough snapshot for
    /// reporting; counters are monotonic.
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (self.rx.load(Relaxed), self.tx.load(Relaxed), self.connections.load(Relaxed))
    }
}

/// Counts every byte crossing the protocol layer into two tallies: this
/// connection's and the server's lifetime totals. Transparent otherwise.
struct Counted<S> {
    inner: S,
    conn: Arc<WireStats>,
    total: Arc<WireStats>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            let n = (buf.filled().len() - before) as u64;
            if n > 0 {
                this.conn.rx.fetch_add(n, Relaxed);
                this.total.rx.fetch_add(n, Relaxed);
            }
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &poll {
            this.conn.tx.fetch_add(*n as u64, Relaxed);
            this.total.tx.fetch_add(*n as u64, Relaxed);
        }
        poll
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl ServerConfig {
    pub fn new(root: impl Into<PathBuf>, identity_dir: impl Into<PathBuf>) -> ServerConfig {
        ServerConfig {
            root: root.into(),
            identity_dir: identity_dir.into(),
            idle_timeout: Duration::from_secs(300),
            frame_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            handshake_timeout: Duration::from_secs(10),
            max_connections: 64,
            // A quarter of the pool: generous for one remote device (a phone
            // browsing, a second Pi syncing) and far short of monopolising it.
            max_connections_per_source: 16,
            chunk_size: DEFAULT_CHUNK as usize,
            zstd_level: 3,
            zstd_min_size: 1024,
            dump_frames: None,
        }
    }
}

/// A dynamic endpoint provider (application-model.md; Phase 5): generated
/// documents and action handling under a path prefix. Authorization runs
/// BEFORE any handler method. Keep implementations quick — they run on the
/// connection task.
pub trait AppHandler: Send + Sync {
    /// A generated resource, or None for NOT_FOUND within the prefix.
    fn get(&self, path: &str, identity: &Identity) -> Option<Vec<u8>>;
    /// A cheap monotonic stamp for what [`AppHandler::get`] would currently
    /// return for `path`, or None (the default) when the handler cannot say
    /// without generating.
    ///
    /// This is the live-poll escape hatch: when a GET_IF arrives and the
    /// revision has not moved since the bytes the client holds were served,
    /// the server answers NOT_MODIFIED without calling `get` at all — a
    /// terminal polling 20×/s stops re-rendering its grid for nothing.
    ///
    /// Contract: the value must change whenever *anything* `get` reads for
    /// this path changes (content, theme, whatever), must never repeat an
    /// old value for new content (a counter is the safe construction), and
    /// must cover identity-dependent output or stay None for such paths.
    /// Liveness side effects that `get` performs (session keep-alive
    /// touches) must happen here too — on a quiet page this replaces `get`
    /// as the thing polling calls.
    fn revision(&self, _path: &str, _identity: &Identity) -> Option<u64> {
        None
    }
    /// Perform a typed action; Ok(bytes) is the document to display.
    fn action(
        &self,
        path: &str,
        fields: &[(String, ActionValue)],
        identity: &Identity,
    ) -> Result<Vec<u8>, Status>;
}

/// The revision a conditional action was made against, if it carries one.
///
/// Handlers call [`verify_expected`] rather than reading this directly; it
/// exists because a handler that wants to say "conditional actions are not
/// supported here" needs to be able to notice one.
pub fn expected_hash(fields: &[(String, ActionValue)]) -> Option<Hash> {
    fields.iter().find(|(name, _)| name == rill_protocol::FIELD_EXPECTED).and_then(|(_, v)| {
        match v {
            ActionValue::Str(s) => Hash::from_hex(s),
            _ => None,
        }
    })
}

/// Enforce a conditional action against the bytes it is conditional on.
///
/// The subject is the handler's to name: only it knows which resource an
/// endpoint mutates, and `/notes/actions/save/x` is not the path of the
/// thing it writes. Pass the *current* bytes of that resource — the same
/// bytes a caller would have fetched — and this answers whether the caller's
/// revision is still the current one.
///
/// An action with no `_expected` field is unconditional and always passes:
/// conditionality is the caller's choice, and most actions do not need it.
///
/// ```ignore
/// // in a handler, before writing:
/// let current = self.body(id).unwrap_or_default();
/// verify_expected(fields, current.as_bytes())?;
/// ```
pub fn verify_expected(
    fields: &[(String, ActionValue)],
    current: &[u8],
) -> Result<(), Status> {
    match expected_hash(fields) {
        Some(expected) if expected != Hash::of(current) => Err(Status::Conflict),
        _ => Ok(()),
    }
}

type Handlers = Arc<Vec<(String, Arc<dyn AppHandler>)>>;

fn find_handler<'a>(
    handlers: &'a [(String, Arc<dyn AppHandler>)],
    path: &str,
) -> Option<&'a Arc<dyn AppHandler>> {
    handlers
        .iter()
        .find(|(prefix, _)| {
            // Prefix match on a path *boundary*: "/term" must not claim
            // "/terminal". Comparing the byte after the prefix beats
            // building "{prefix}/" — that allocated a String per handler
            // per request, on the connection task, to throw it away.
            path.len() >= prefix.len()
                && path.is_char_boundary(prefix.len())
                && &path[..prefix.len()] == prefix
                && path[prefix.len()..].chars().next().is_none_or(|c| c == '/')
        })
        .map(|(_, h)| h)
}

pub struct Server {
    listener: TcpListener,
    root: PathBuf,
    acceptor: TlsAcceptor,
    devices: Arc<DeviceRegistry>,
    policy: Arc<Policy>,
    memo: Arc<HashMemo>,
    rev_memo: Arc<RevMemo>,
    pending: Arc<Mutex<rill_auth::PendingDevices>>,
    handlers: Vec<(String, Arc<dyn AppHandler>)>,
    stats: Arc<WireStats>,
    cfg: ServerConfig,
}

impl Server {
    /// Load identity material, canonicalize the root (refusing to start on
    /// any failure), lint the policy, and bind.
    pub async fn bind(bind_addr: &str, port: u16, cfg: ServerConfig) -> io::Result<Server> {
        let bad = |m: String| io::Error::new(io::ErrorKind::InvalidInput, m);

        let identity = load_pem_identity(&cfg.identity_dir, "server")
            .map_err(|e| bad(e.to_string()))?
            .ok_or_else(|| {
                bad(format!(
                    "no server identity in {:?} — run: rill auth init-server {:?}",
                    cfg.identity_dir, cfg.identity_dir
                ))
            })?;
        let key = parse_key_pem(&identity.key_pem).map_err(|e| bad(e.to_string()))?;
        let cert = parse_cert_pem(&identity.cert_pem).map_err(|e| bad(e.to_string()))?;
        log!(Info, 0, "identity", fingerprint = fingerprint_hex(&cert));
        let tls = server_tls_config(key, cert).map_err(|e| bad(e.to_string()))?;

        let read = |name: &str| -> io::Result<String> {
            let path = cfg.identity_dir.join(name);
            std::fs::read_to_string(&path)
                .map_err(|e| bad(format!("{}: {e}", path.display())))
        };
        let devices = DeviceRegistry::parse(&read("devices.toml")?)
            .map_err(|e| bad(e.to_string()))?;
        let policy = Policy::parse(&read("policy.toml")?).map_err(|e| bad(e.to_string()))?;
        for warning in policy.lint() {
            log!(Warn, 0, "policy-warning", detail = warning);
        }
        log!(Info, 0, "policy", loaded = true, devices = devices.len());

        let root = tokio::fs::canonicalize(&cfg.root).await.map_err(|e| {
            io::Error::new(e.kind(), format!("content root {:?}: {e}", cfg.root))
        })?;
        let listener = TcpListener::bind((bind_addr, port)).await?;
        Ok(Server {
            listener,
            root,
            acceptor: TlsAcceptor::from(tls),
            devices: Arc::new(devices),
            policy: Arc::new(policy),
            memo: Arc::new(HashMemo::default()),
            rev_memo: Arc::new(RevMemo::default()),
            // Carried across restarts: a device that knocked yesterday is
            // still the one the operator is about to enroll.
            pending: Arc::new(Mutex::new(
                rill_auth::PendingDevices::load(&cfg.identity_dir).unwrap_or_default(),
            )),
            handlers: Vec::new(),
            stats: Arc::new(WireStats::default()),
            cfg,
        })
    }

    /// The server's lifetime wire totals. Grab this before spawning
    /// [`Server::run`]; it stays live for the server's lifetime.
    pub fn wire_stats(&self) -> Arc<WireStats> {
        self.stats.clone()
    }

    /// Register a dynamic handler under a path prefix (e.g. "/notes").
    pub fn dynamic(&mut self, prefix: &str, handler: Arc<dyn AppHandler>) {
        self.handlers.push((prefix.trim_end_matches('/').to_string(), handler));
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept loop; runs until the task is dropped or accept fails fatally.
    pub async fn run(self) -> io::Result<()> {
        let semaphore = Arc::new(Semaphore::new(self.cfg.max_connections));
        let sources: Arc<SourceCounts> = Arc::new(SourceCounts::default());
        let root = Arc::new(self.root);
        let handlers: Handlers = Arc::new(self.handlers);
        let cfg = Arc::new(self.cfg);
        let stats = self.stats;
        let mut conn_no: u64 = 0;

        // Opt-in stats snapshot for harnesses (bench-device.sh): RILL_STATS
        // names a file that gets an atomically-replaced JSON summary every
        // 5 s. Same env-var configuration family as RILL_LOG; no new
        // network surface.
        if let Ok(path) = std::env::var("RILL_STATS") {
            let stats = stats.clone();
            let path = PathBuf::from(path);
            tokio::spawn(async move {
                let tmp = path.with_extension("json.tmp");
                loop {
                    let (rx, tx, conns) = stats.snapshot();
                    let json = format!(
                        "{{\"scope\":\"protocol bytes (post-TLS plaintext)\",\"rx_bytes\":{rx},\"tx_bytes\":{tx},\"connections\":{conns}}}\n"
                    );
                    if std::fs::write(&tmp, &json).is_ok() {
                        let _ = std::fs::rename(&tmp, &path);
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            });
        }

        log!(Info, 0, "listening", addr = self.listener.local_addr()?, tls = "1.3", alpn = "rill/1");
        loop {
            // A failed accept is one peer's problem, not the server's. The
            // errors that land here are per-connection (a peer that reset
            // between SYN and accept) or transient pressure (out of file
            // descriptors) — propagating either one exits the process, which
            // turns a dropped connection into an outage for every other
            // client. Under fd exhaustion, yield first: retrying flat-out
            // spins a core against a condition only other tasks can clear.
            let (tcp, peer_addr) = match self.listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    // A peer that vanished mid-handshake costs nothing to skip
                    // and can arrive in bursts, so it must not be throttled.
                    // Anything else — chiefly running out of descriptors —
                    // will still be true on the next call, so pause rather
                    // than spin a core against a condition only other tasks
                    // can clear.
                    let per_peer = matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::Interrupted
                    );
                    log!(Warn, 0, "accept-failed", error = e, backoff = !per_peer);
                    if !per_peer {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    continue;
                }
            };
            conn_no += 1;
            let Ok(permit) = semaphore.clone().try_acquire_owned() else {
                log!(Warn, conn_no, "refused", peer = peer_addr, reason = "connection cap reached");
                drop(tcp);
                continue;
            };
            // Then the per-source share, so one remote peer cannot hold the
            // pool against everyone else. Loopback is exempt — see the config
            // field: locally, every window on the desktop is a connection from
            // 127.0.0.1, and a cap sized for an attacker would be sized wrong
            // for a desktop.
            let source_slot = if peer_addr.ip().is_loopback() {
                None
            } else {
                match sources.claim(peer_addr.ip(), cfg.max_connections_per_source) {
                    Some(slot) => Some(slot),
                    None => {
                        log!(Warn, conn_no, "refused", peer = peer_addr, reason = "per-source cap reached");
                        drop(tcp);
                        continue;
                    }
                }
            };
            let acceptor = self.acceptor.clone();
            let devices = self.devices.clone();
            let policy = self.policy.clone();
            let memo = self.memo.clone();
            let rev_memo = self.rev_memo.clone();
            let pending = self.pending.clone();
            let root = root.clone();
            let handlers = handlers.clone();
            let cfg = cfg.clone();
            let stats = stats.clone();
            tokio::spawn(async move {
                let _permit = permit;
                // Held for the connection's life; releases the source's slot.
                let _source_slot = source_slot;
                let _ = tcp.set_nodelay(true);

                let tls = match timeout(cfg.handshake_timeout, acceptor.accept(tcp)).await {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(e)) => {
                        log!(Warn, conn_no, "handshake-failed", peer = peer_addr, error = e);
                        return;
                    }
                    Err(_) => {
                        log!(Warn, conn_no, "handshake-timeout", peer = peer_addr);
                        return;
                    }
                };
                let (_, session) = tls.get_ref();
                if session.alpn_protocol() != Some(rill_auth::ALPN) {
                    log!(Warn, conn_no, "rejected", reason = "no rill/1 ALPN agreement");
                    return;
                }
                let identity = match session.peer_certificates().and_then(|c| c.first()) {
                    None => Identity::Anonymous,
                    Some(cert) => {
                        let identity = devices.identify(cert);
                        if identity == Identity::Anonymous {
                            // A stranger knocked. Recording the fingerprint
                            // grants nothing — approval is still a human
                            // running `rill auth enroll` — but it puts the
                            // one fact needed for that into a file instead
                            // of into prose someone has to be watching for.
                            let fp = fingerprint_hex(cert);
                            note_pending(&pending, &cfg.identity_dir, &fp, conn_no);
                            log!(Warn, conn_no, "unknown-device", fingerprint = fp);
                        }
                        identity
                    }
                };
                log!(Info, conn_no, "connected", peer = peer_addr, identity = identity);
                stats.connections.fetch_add(1, Relaxed);
                let conn_stats = Arc::new(WireStats::default());
                let counted =
                    Counted { inner: tls, conn: conn_stats.clone(), total: stats };
                handle_connection(
                    counted, conn_no, identity, &policy, &memo, &rev_memo, &root, &handlers, &cfg,
                )
                .await;
                let (rx, tx, _) = conn_stats.snapshot();
                log!(Info, conn_no, "closed", rx_bytes = rx, tx_bytes = tx);
            });
        }
    }
}

/// Live connection count per remote address, enforcing
/// [`ServerConfig::max_connections_per_source`].
///
/// The map holds only addresses with a connection open right now: an entry is
/// removed when its last connection closes, so a peer cycling through source
/// addresses cannot grow it. That matters more than it looks — a table keyed
/// by something the network chooses is a memory leak with extra steps.
#[derive(Default)]
struct SourceCounts(Mutex<HashMap<std::net::IpAddr, usize>>);

impl SourceCounts {
    /// Claim a slot for `ip`, or `None` if that address already holds `cap`.
    /// The slot is released when the returned guard drops.
    fn claim(self: &Arc<Self>, ip: std::net::IpAddr, cap: usize) -> Option<SourceSlot> {
        let mut live = self.0.lock().unwrap();
        let n = live.entry(ip).or_insert(0);
        if *n >= cap {
            // Leave a zero entry behind only if we created one; `or_insert`
            // may have. Removing it here keeps the invariant that every entry
            // is a live connection.
            if *n == 0 {
                live.remove(&ip);
            }
            return None;
        }
        *n += 1;
        Some(SourceSlot { counts: Arc::clone(self), ip })
    }

    #[cfg(test)]
    fn tracked_addresses(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

/// Holds one source's slot for the life of a connection task.
struct SourceSlot {
    counts: Arc<SourceCounts>,
    ip: std::net::IpAddr,
}

impl Drop for SourceSlot {
    fn drop(&mut self) {
        let mut live = self.counts.0.lock().unwrap();
        if let Some(n) = live.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                live.remove(&self.ip);
            }
        }
    }
}

/// What the server says when something is not there — and, identically, when
/// it is there and you may not see it.
///
/// Denial is indistinguishable from absence (security.md §6), which means the
/// *message* is part of that guarantee and not merely the status: two spellings
/// of "missing" would tell a caller which kind of missing it met. A constant,
/// so agreement between the paths that send it is structural rather than a
/// matter of everyone remembering to type the same two words.
const NOT_FOUND_MESSAGE: &str = "not found";

/// Clamp what a handler may say on the wire.
///
/// Two reasons, and the second is the one that matters.
///
/// A handler is application code, and the status vocabulary is a protocol
/// surface: `Status::PathInvalid` is a `0x01xx` code, which
/// `closes_connection` reports as connection-fatal, so a handler returning
/// it for a missing field killed the client's connection. (term-app did,
/// four times.)
///
/// And the statuses a handler distinguishes are an oracle. "This parameter
/// was malformed" versus "this resource is not here" tells a caller which of
/// its guesses named something real — exactly the distinction the NOT_FOUND
/// rule exists to erase. Rill core does not expose authorization or
/// existence distinctions through protocol status; application authors
/// remain responsible for their own semantic side channels.
///
/// CONFLICT passes through: it says nothing about existence (the caller
/// already read the resource to have a revision at all) and everything about
/// whether the write applied.
fn handler_status(status: Status) -> Status {
    match status {
        Status::NotFound | Status::Conflict => status,
        _ => Status::Internal,
    }
}

/// What a request frame asks for, after the Get/Head/GetIf destructure.
enum Verb {
    Get { accept_zstd: bool },
    Head,
    GetIf { hash: [u8; 32], accept_zstd: bool },
}

impl Verb {
    fn name(&self) -> &'static str {
        match self {
            Verb::Get { .. } => "GET",
            Verb::Head => "HEAD",
            Verb::GetIf { .. } => "GET_IF",
        }
    }

    fn accepts_zstd(&self) -> bool {
        match self {
            Verb::Get { accept_zstd } | Verb::GetIf { accept_zstd, .. } => *accept_zstd,
            Verb::Head => false,
        }
    }
}

/// Outcome of reading one request under the two-budget policy.
enum ReadOutcome {
    Frame(Frame),
    /// No byte arrived within `idle_timeout` (a quiet, idle connection).
    Idle,
    /// Peer closed cleanly at a frame boundary.
    Closed,
    /// Connection-fatal: I/O error, a slow frame (slowloris), or a bad frame.
    Fatal(WireError),
}

/// Read one request with two budgets (connection.md §6): wait up to
/// `idle_timeout` for the *first byte*, then require the whole frame within
/// `frame_timeout`. Splitting the budgets defeats slowloris — a client cannot
/// dribble a single frame across the entire idle window to pin a slot.
async fn read_budgeted<S>(stream: &mut S, cfg: &ServerConfig) -> ReadOutcome
where
    S: AsyncRead + Unpin,
{
    let mut first = [0u8; 1];
    let n = match timeout(cfg.idle_timeout, stream.read(&mut first)).await {
        Err(_) => return ReadOutcome::Idle,
        Ok(Err(e)) => return ReadOutcome::Fatal(WireError::Io(e)),
        Ok(Ok(n)) => n,
    };
    if n == 0 {
        return ReadOutcome::Closed;
    }
    // A frame is now in progress: bound the remainder by the frame budget,
    // feeding the byte we already read back in front of the stream.
    let mut chained = (&first[..1]).chain(&mut *stream);
    match timeout(cfg.frame_timeout, read_frame(&mut chained, Peer::Client)).await {
        Err(_) => ReadOutcome::Fatal(WireError::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "frame not completed within frame_timeout",
        ))),
        Ok(Ok(frame)) => ReadOutcome::Frame(frame),
        Ok(Err(WireError::Closed)) => ReadOutcome::Closed,
        Ok(Err(e)) => ReadOutcome::Fatal(e),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection<S>(
    mut stream: S,
    conn: u64,
    identity: Identity,
    policy: &Policy,
    memo: &HashMemo,
    rev_memo: &RevMemo,
    root: &Path,
    handlers: &[(String, Arc<dyn AppHandler>)],
    cfg: &ServerConfig,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut dump_tap = cfg.dump_frames.as_ref().and_then(|dir| {
        FrameDump::new(dir.join(format!("conn-{conn}"))).ok()
    });
    let mut expected_id: u32 = 1;

    loop {
        let frame = match read_budgeted(&mut stream, cfg).await {
            ReadOutcome::Idle => {
                log!(Info, conn, "idle-timeout");
                let _ = send(&mut stream, cfg, &mut dump_tap, &Frame::Close).await;
                return;
            }
            ReadOutcome::Closed => {
                log!(Info, conn, "eof", clean = false);
                return;
            }
            ReadOutcome::Fatal(e) => {
                // Connection-fatal (connection.md §2): ERROR with request ID 0,
                // then CLOSE, then drop. Covers a too-slow frame (slowloris).
                log!(Error, conn, "fatal", error = e);
                let status = e.wire_status();
                let _ = send(&mut stream, cfg, &mut dump_tap, &Frame::Error {
                    request_id: 0,
                    status,
                    message: String::new(),
                }).await;
                let _ = send(&mut stream, cfg, &mut dump_tap, &Frame::Close).await;
                return;
            }
            ReadOutcome::Frame(frame) => frame,
        };
        dump(&mut dump_tap, false, &frame);

        // ACTION: the write verb — authorized, then routed to a dynamic
        // handler (static trees have no actions).
        // `cas` needs no handling here: the codec guarantees a CAS-flagged
        // ACTION carries its `_expected` field, so `verify_expected` in the
        // handler is the whole enforcement.
        // Request ids advance one per *request*; PING and CLOSE are neither,
        // and consume none. Checked here, once, for every verb that carries
        // one — this used to be two copies of the same fatal path, one per
        // branch below, which is a poor place for a rule the protocol depends
        // on to live twice.
        let carried_id = match &frame {
            Frame::Action { request_id, .. }
            | Frame::Get { request_id, .. }
            | Frame::Head { request_id, .. }
            | Frame::GetIf { request_id, .. } => Some(*request_id),
            _ => None,
        };
        if let Some(request_id) = carried_id {
            if request_id != expected_id {
                log!(Error, conn, "fatal", reason = "request id", got = request_id, expected = expected_id);
                let _ = send(&mut stream, cfg, &mut dump_tap, &Frame::Error {
                    request_id: 0,
                    status: Status::ProtocolMalformed,
                    message: String::new(),
                }).await;
                let _ = send(&mut stream, cfg, &mut dump_tap, &Frame::Close).await;
                return;
            }
            expected_id += 1;
        }

        if let Frame::Action { request_id, path, fields, .. } = &frame {
            let (request_id, path) = (*request_id, path.clone());
            // ACTION is the write verb, so it asks the write question. A
            // policy that does not distinguish answers both the same way, so
            // this changes nothing for existing files.
            let result = if !policy.authorize_access(&identity, Access::Act, &path) {
                log!(Warn, conn, "ACTION", path = path, result = "denied", identity = identity, hidden = true);
                Err(Status::NotFound)
            } else {
                match find_handler(handlers, &path) {
                    Some(handler) => handler.action(&path, fields, &identity),
                    None => Err(Status::NotFound),
                }
            };
            match result {
                Ok(bytes) => {
                    let n = bytes.len();
                    if let Err(e) =
                        stream_bytes(&mut stream, cfg, &mut dump_tap, request_id, bytes, false)
                            .await
                    {
                        log!(Error, conn, "ACTION", path = path, result = "send-failed", error = e);
                        return;
                    }
                    log!(Debug, conn, "ACTION", path = path, result = "ok", bytes = n);
                }
                Err(status) => {
                    let status = handler_status(status);
                    log!(Warn, conn, "ACTION", path = path, status = status.name());
                    let message = match status {
                        Status::NotFound => NOT_FOUND_MESSAGE.to_string(),
                        Status::Conflict => "resource has changed since you read it".to_string(),
                        _ => "internal error".to_string(),
                    };
                    if send(&mut stream, cfg, &mut dump_tap, &Frame::Error {
                        request_id,
                        status,
                        message,
                    })
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
            }
            continue;
        }

        let (verb, request_id, path) = match frame {
            Frame::Get { request_id, path, accept_zstd } => {
                (Verb::Get { accept_zstd }, request_id, path)
            }
            Frame::Head { request_id, path } => (Verb::Head, request_id, path),
            Frame::GetIf { request_id, path, hash, accept_zstd } => {
                (Verb::GetIf { hash, accept_zstd }, request_id, path)
            }
            Frame::Ping { payload } => {
                if send(&mut stream, cfg, &mut dump_tap, &Frame::Pong { payload }).await.is_err() {
                    return;
                }
                continue;
            }
            Frame::Close => {
                log!(Info, conn, "close", clean = true);
                return;
            }
            // read_frame's direction check makes server-only frames unreachable.
            _ => unreachable!("direction-checked"),
        };

        // Authorization before resource access (security.md §6): a denied
        // request never touches the filesystem, and denial is NOT_FOUND —
        // indistinguishable from absence. An unauthorized GET_IF reveals
        // nothing, including whether its hash matched.
        // Dynamic prefixes are consulted before the static tree.
        //
        // Asked once and reused: the two arms below must agree about what is
        // permitted, and evaluating the policy twice per request left that
        // agreement resting on the calls staying identical.
        let authorized = policy.authorize(&identity, &path);
        if authorized
            && let Some(handler) = find_handler(handlers, &path)
        {
            // Read the revision BEFORE generating: the memo's correctness
            // argument (see RevMemo) needs the stamp to be no newer than the
            // bytes it is stored against.
            let revision = handler.revision(&path, &identity);
            if let (Verb::GetIf { hash: client_hash, .. }, Some(rev)) = (&verb, revision)
                && rev_memo.fresh(&path, rev, client_hash)
            {
                log!(Debug, conn, "GET_IF", path = path, result = "not-modified", by = "revision");
                match send(&mut stream, cfg, &mut dump_tap, &Frame::NotModified { request_id })
                    .await
                {
                    Ok(()) => continue,
                    Err(_) => return,
                }
            }
            let sent = match handler.get(&path, &identity) {
                Some(bytes) => {
                    if let Some(rev) = revision {
                        rev_memo.remember(&path, rev, Hash::of(&bytes));
                    }
                    serve_dynamic(&mut stream, cfg, &mut dump_tap, request_id, &verb, bytes)
                        .await
                }
                None => {
                    log!(Info, conn, verb.name(), path = path, result = "not-found", dynamic = true);
                    match send(&mut stream, cfg, &mut dump_tap, &Frame::Error {
                        request_id,
                        status: Status::NotFound,
                        message: NOT_FOUND_MESSAGE.into(),
                    })
                    .await
                    {
                        Ok(()) => continue,
                        Err(_) => return,
                    }
                }
            };
            match sent {
                Ok(detail) => {
                    log!(Debug, conn, verb.name(), path = path, result = detail);
                    continue;
                }
                Err(e) => {
                    log!(Error, conn, verb.name(), path = path, result = "send-failed", error = e);
                    return;
                }
            }
        }

        let outcome = if !authorized {
            log!(Warn, conn, verb.name(), path = path, result = "denied", identity = identity, hidden = true);
            Err(Status::NotFound)
        } else {
            resolve(root, &path).await
        };

        match outcome {
            Ok(resolved) => {
                let sent = serve_resource(
                    &mut stream, cfg, &mut dump_tap, memo, request_id, &verb, resolved,
                )
                .await;
                match sent {
                    Ok(detail) => log!(Debug, conn, verb.name(), path = path, result = detail),
                    Err(e) => {
                        log!(Error, conn, verb.name(), path = path, result = "send-failed", error = e);
                        return;
                    }
                }
            }
            Err(status) => {
                if status == Status::Internal {
                    log!(Warn, conn, verb.name(), path = path, status = status.name());
                }
                let message = match status {
                    Status::NotFound => NOT_FOUND_MESSAGE.to_string(),
                    _ => "internal error".to_string(),
                };
                if send(&mut stream, cfg, &mut dump_tap, &Frame::Error {
                    request_id,
                    status,
                    message,
                }).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn send<S>(
    stream: &mut S,
    cfg: &ServerConfig,
    tap: &mut Option<FrameDump>,
    frame: &Frame,
) -> Result<(), WireError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    dump(tap, true, frame);
    timeout(cfg.write_timeout, write_frame(stream, frame))
        .await
        .map_err(|_| WireError::Io(io::Error::new(io::ErrorKind::TimedOut, "write timeout")))?
}

/// Serve handler-generated bytes for GET/HEAD/GET_IF.
async fn serve_dynamic<S>(
    stream: &mut S,
    cfg: &ServerConfig,
    tap: &mut Option<FrameDump>,
    request_id: u32,
    verb: &Verb,
    bytes: Vec<u8>,
) -> Result<String, WireError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Only the conditional verbs need the hash, so each computes its own.
    // A plain GET was paying a BLAKE3 pass over the whole generated document
    // to produce a value it then dropped — and a widget on a clock is
    // nothing *but* plain GETs of freshly generated documents.
    //
    // Hashing inside the arms rather than hoisting a placeholder above the
    // match is deliberate: the arm below decides NOT_MODIFIED on a hash
    // equality, and a stand-in value for the verbs that don't need one would
    // be a fabricated hash sitting one arm away from that comparison.
    match verb {
        Verb::Head => {
            let size = bytes.len() as u64;
            let hash = Hash::of(&bytes);
            send(stream, cfg, tap, &Frame::Metadata { request_id, size, hash: Some(hash.0) })
                .await?;
            Ok(format!("OK (dynamic, size {size})"))
        }
        Verb::GetIf { hash: client_hash, .. } if Hash::of(&bytes).0 == *client_hash => {
            send(stream, cfg, tap, &Frame::NotModified { request_id }).await?;
            Ok("NOT_MODIFIED (dynamic)".to_string())
        }
        Verb::Get { accept_zstd } | Verb::GetIf { accept_zstd, .. } => {
            let n = bytes.len();
            let compress = *accept_zstd && n as u64 >= cfg.zstd_min_size;
            stream_bytes(stream, cfg, tap, request_id, bytes, compress).await?;
            Ok(format!("OK (dynamic, {n} bytes)"))
        }
    }
}

/// Stream an in-memory payload as RESOURCE chunks (dynamic responses).
async fn stream_bytes<S>(
    stream: &mut S,
    cfg: &ServerConfig,
    tap: &mut Option<FrameDump>,
    request_id: u32,
    bytes: Vec<u8>,
    compress: bool,
) -> Result<(), WireError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (payload, zstd) = if compress {
        match rill_store::encoding::compress(&bytes, cfg.zstd_level) {
            Ok(packed) if packed.len() < bytes.len() => (packed, true),
            _ => (bytes, false),
        }
    } else {
        (bytes, false)
    };
    // The overwhelmingly common case: a document that fits in one chunk.
    // Send the buffer we already own instead of slicing it and copying every
    // slice back out into a fresh Vec.
    if payload.len() <= cfg.chunk_size.max(1) {
        return send(stream, cfg, tap, &Frame::Resource {
            request_id,
            more: false,
            zstd,
            payload,
        })
        .await;
    }
    // Split off the front of the buffer rather than slicing it and copying
    // every slice into a fresh Vec: `to_vec` on each chunk copied the whole
    // payload a second time on the connection task, for a body already held
    // in full. Draining the tail forward moves the remainder instead, so the
    // bytes are copied once — the frame the chunk becomes.
    let chunk_size = cfg.chunk_size.max(1);
    let mut rest = payload;
    while !rest.is_empty() {
        let tail = rest.split_off(chunk_size.min(rest.len()));
        send(stream, cfg, tap, &Frame::Resource {
            request_id,
            more: !tail.is_empty(),
            zstd,
            payload: rest,
        })
        .await?;
        rest = tail;
    }
    Ok(())
}

/// A successfully resolved resource, ready to serve.
struct Resolved {
    file: tokio::fs::File,
    size: u64,
    canonical: PathBuf,
    mtime: SystemTime,
}

/// Root-jail resolution (connection.md §8). Returns the opened file and its
/// size, or the status to answer with. Never leaks why.
///
/// **Precondition: `root` is already canonical.** [`Server::bind`]
/// canonicalizes it and refuses to start otherwise, so this holds for every
/// production caller. Passing a non-canonical root fails *closed* — the
/// opened handle's real path will not start with it and every request is
/// answered NOT_FOUND — which is the right direction to fail, and is
/// asserted by `a_non_canonical_root_fails_closed`.
///
/// Order matters: we **open first, then verify the opened handle** lies within
/// the root — not canonicalize-then-open. The latter has a TOCTOU: a path
/// component swapped for an outside-pointing symlink between the check and the
/// open would be followed, escaping the jail. Here every check after the open
/// is on the file descriptor itself, so a post-open swap cannot change what we
/// serve.
async fn resolve(root: &Path, path: &str) -> Result<Resolved, Status> {
    let candidate = root.join(&path[1..]);
    let file = tokio::fs::File::open(&candidate).await.map_err(|e| match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => Status::NotFound,
        _ => Status::Internal,
    })?;
    // Directories are hidden in v1; check via the fd (fstat), not the path.
    let meta = file.metadata().await.map_err(|_| Status::Internal)?;
    if !meta.is_file() {
        return Err(Status::NotFound);
    }
    // The real path of the *open descriptor* — symlinks resolved as they were
    // at open time. This, not the pre-open path, is the security boundary.
    let canonical = fd_real_path(&file).await.ok_or(Status::Internal)?;
    // `root` is already canonical: `Server::bind` canonicalizes it and
    // refuses to start if it cannot, so re-resolving it per request was a
    // filesystem round trip that could only ever return its own input.
    if !canonical.starts_with(root) {
        // Resolved outside the root: hidden (deny-escaping policy).
        return Err(Status::NotFound);
    }
    let mtime = meta.modified().map_err(|_| Status::Internal)?;
    Ok(Resolved { file, size: meta.len(), canonical, mtime })
}

/// The real filesystem path an open file descriptor points at, via
/// `/proc/self/fd/<n>` (Linux). Used to verify a handle against the root jail
/// without re-opening (which would reintroduce a TOCTOU).
async fn fd_real_path(file: &tokio::fs::File) -> Option<PathBuf> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    tokio::fs::read_link(format!("/proc/self/fd/{fd}")).await.ok()
}

/// Answer an authorized, resolved request per its verb. Returns the log
/// detail line.
async fn serve_resource<S>(
    stream: &mut S,
    cfg: &ServerConfig,
    tap: &mut Option<FrameDump>,
    memo: &HashMemo,
    request_id: u32,
    verb: &Verb,
    mut resolved: Resolved,
) -> Result<String, WireError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let hash_needed = !matches!(verb, Verb::Get { .. });
    let current = if hash_needed {
        Some(
            memo.current(&resolved.canonical, &mut resolved.file, resolved.mtime, resolved.size)
                .await
                .map_err(WireError::Io)?,
        )
    } else {
        None
    };

    // Compression policy (resource-format.md §8): client accepts ∧ big
    // enough ∧ not a known-compressed format.
    let compress = verb.accepts_zstd()
        && resolved.size >= cfg.zstd_min_size
        && compressible(&resolved.canonical);

    match verb {
        Verb::Head => {
            let hash = current.expect("hash computed for HEAD");
            send(stream, cfg, tap, &Frame::Metadata {
                request_id,
                size: resolved.size,
                hash: Some(hash.0),
            })
            .await?;
            Ok(format!("OK (size {}, {hash})", resolved.size))
        }
        Verb::GetIf { hash: client_hash, .. } => {
            let hash = current.expect("hash computed for GET_IF");
            if hash.0 == *client_hash {
                send(stream, cfg, tap, &Frame::NotModified { request_id }).await?;
                return Ok("NOT_MODIFIED (0 payload bytes)".to_string());
            }
            let sent = stream_file(stream, cfg, tap, request_id, resolved.file, compress).await?;
            Ok(format!("changed → {}", sent.describe()))
        }
        Verb::Get { .. } => {
            let sent = stream_file(stream, cfg, tap, request_id, resolved.file, compress).await?;
            Ok(sent.describe())
        }
    }
}

/// Transfer accounting for the log line.
struct Sent {
    raw: u64,
    wire: u64,
    chunks: u32,
    zstd: bool,
}

impl Sent {
    fn describe(&self) -> String {
        if self.zstd {
            format!(
                "OK ({} bytes → {} on wire, zstd, {} chunks)",
                self.raw, self.wire, self.chunks
            )
        } else {
            format!("OK ({} bytes, {} chunks)", self.raw, self.chunks)
        }
    }
}

/// Stream a file as RESOURCE chunks, optionally zstd-compressed
/// (compress-then-chunk with bounded memory). A pending-chunk buffer keeps
/// the MORE flag correct without trusting the stat size.
async fn stream_file<S>(
    stream: &mut S,
    cfg: &ServerConfig,
    tap: &mut Option<FrameDump>,
    request_id: u32,
    mut file: tokio::fs::File,
    compress: bool,
) -> Result<Sent, WireError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut compressor = if compress {
        Some(rill_store::encoding::Compressor::new(cfg.zstd_level).map_err(WireError::Io)?)
    } else {
        None
    };
    let mut pending: Option<Vec<u8>> = None;
    let mut sent = Sent { raw: 0, wire: 0, chunks: 0, zstd: compress };

    // Emit `piece` after first flushing whatever was pending (with MORE set).
    #[allow(clippy::too_many_arguments)] // local helper; a param struct would just rename these
    async fn emit<S: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut S,
        cfg: &ServerConfig,
        tap: &mut Option<FrameDump>,
        request_id: u32,
        zstd: bool,
        pending: &mut Option<Vec<u8>>,
        sent: &mut Sent,
        piece: Vec<u8>,
    ) -> Result<(), WireError> {
        if let Some(prev) = pending.take() {
            sent.wire += prev.len() as u64;
            sent.chunks += 1;
            send(stream, cfg, tap, &Frame::Resource {
                request_id,
                more: true,
                zstd,
                payload: prev,
            })
            .await?;
        }
        *pending = Some(piece);
        Ok(())
    }

    loop {
        let mut buf = vec![0u8; cfg.chunk_size];
        let mut filled = 0;
        while filled < buf.len() {
            let n = file.read(&mut buf[filled..]).await.map_err(WireError::Io)?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        sent.raw += buf.len() as u64;

        let eof = buf.is_empty();
        match &mut compressor {
            Some(c) => {
                if !eof {
                    c.write(&buf).map_err(WireError::Io)?;
                    let out = c.drain(cfg.chunk_size);
                    if !out.is_empty() {
                        emit(stream, cfg, tap, request_id, true, &mut pending, &mut sent, out)
                            .await?;
                    }
                }
            }
            None => {
                if !eof {
                    emit(stream, cfg, tap, request_id, false, &mut pending, &mut sent, buf)
                        .await?;
                }
            }
        }

        if eof {
            if let Some(c) = compressor.take() {
                // Stream end marker + remaining output, in chunk-size pieces.
                let tail = c.finish().map_err(WireError::Io)?;
                for piece in tail.chunks(cfg.chunk_size.max(1)) {
                    emit(stream, cfg, tap, request_id, true, &mut pending, &mut sent,
                        piece.to_vec())
                        .await?;
                }
            }
            let last = pending.take().unwrap_or_default();
            sent.wire += last.len() as u64;
            sent.chunks += 1;
            send(stream, cfg, tap, &Frame::Resource {
                request_id,
                more: false,
                zstd: sent.zstd,
                payload: last,
            })
            .await?;
            return Ok(sent);
        }
    }
}

#[cfg(test)]
mod log_tests {
    use super::{Level, push_field};

    /// A line stays parseable whatever the application puts in a value:
    /// device names and status summaries routinely contain spaces, and a
    /// bare one would silently split one field into several.
    #[test]
    fn field_values_that_would_break_the_split_are_quoted() {
        let mut line = String::new();
        push_field(&mut line, "path", "/public/ok");
        push_field(&mut line, "identity", "device laptop");
        push_field(&mut line, "result", "OK (3 bytes, 1 chunks)");
        push_field(&mut line, "empty", "");
        push_field(&mut line, "quoted", "say \"hi\"");
        push_field(&mut line, "lines", "a\nb");
        assert_eq!(
            line,
            r#" path=/public/ok identity="device laptop" result="OK (3 bytes, 1 chunks)" empty="" quoted="say \"hi\"" lines="a\nb""#
        );
    }

    /// The gate is an ordering, and the request level is the talkative one:
    /// a threshold that admits Info must still exclude Debug, which is the
    /// property keeping a chatty desktop's logs empty by default.
    #[test]
    fn levels_order_from_quiet_to_talkative() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
        assert!(Level::Debug > Level::Info, "requests are off at the Info threshold");
    }
}

#[cfg(test)]
mod memo_tests {
    use super::HashMemo;
    use rill_store::Hash;
    use std::path::PathBuf;
    use std::time::SystemTime;

    /// The memo is bounded, and the entry that leaves is the one least
    /// recently *used*, not least recently inserted — a hot path survives
    /// churn in cold ones.
    #[test]
    fn memo_evicts_least_recently_used_at_cap() {
        let memo = HashMemo::default();
        let t = SystemTime::UNIX_EPOCH;
        let h = Hash::of(b"x");
        let p = |name: &str| PathBuf::from(format!("/srv/{name}"));

        memo.remember(2, &p("a"), t, 1, h);
        memo.remember(2, &p("b"), t, 2, h);
        // Touch `a`, then insert over the cap: `b` is now the oldest.
        assert!(memo.cached(&p("a"), t, 1).is_some());
        memo.remember(2, &p("c"), t, 3, h);

        assert_eq!(memo.map.lock().unwrap().len(), 2, "cap held");
        assert!(memo.cached(&p("a"), t, 1).is_some(), "hot entry survived");
        assert!(memo.cached(&p("b"), t, 2).is_none(), "cold entry evicted");
        assert!(memo.cached(&p("c"), t, 3).is_some());
    }

    /// A changed (mtime, size) is a miss, never a stale hash.
    #[test]
    fn memo_misses_on_changed_stat() {
        let memo = HashMemo::default();
        let t = SystemTime::UNIX_EPOCH;
        let p = PathBuf::from("/srv/f");
        memo.remember(8, &p, t, 10, Hash::of(b"old"));
        assert!(memo.cached(&p, t, 11).is_none(), "size changed");
        assert!(memo.cached(&p, t + std::time::Duration::from_secs(1), 10).is_none(), "mtime changed");
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::{Status, resolve};
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    /// `resolve` requires a canonical root (see its docs); production gets
    /// that from `Server::bind`, and these tests get it from here.
    fn canonical(dir: &tempfile::TempDir) -> PathBuf {
        std::fs::canonicalize(dir.path()).unwrap()
    }

    /// A file inside the root resolves and opens.
    #[tokio::test]
    async fn serves_in_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("ok.txt"), b"hi").unwrap();
        let r = resolve(&canonical(&root), "/ok.txt").await;
        assert!(r.is_ok(), "in-root file should serve");
        assert_eq!(r.unwrap().size, 2);
    }

    /// The precondition, stated as a test: a root that is not canonical
    /// denies everything rather than serving anything it shouldn't. Failing
    /// closed is what makes the removed per-request `canonicalize` safe.
    #[tokio::test]
    async fn a_non_canonical_root_fails_closed() {
        let real = tempfile::tempdir().unwrap();
        std::fs::write(real.path().join("ok.txt"), b"hi").unwrap();
        let link_parent = tempfile::tempdir().unwrap();
        let link = link_parent.path().join("via-symlink");
        symlink(canonical(&real), &link).unwrap();

        // Through the symlinked (non-canonical) root: denied, not served.
        assert!(
            matches!(resolve(&link, "/ok.txt").await, Err(Status::NotFound)),
            "a non-canonical root must deny, never serve"
        );
        // And the canonical form of that same root does serve it.
        assert!(resolve(&canonical(&real), "/ok.txt").await.is_ok());
    }

    /// A symlink to a file *outside* the root is refused (hidden as NotFound).
    #[tokio::test]
    async fn rejects_symlink_escape_file() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"top secret").unwrap();
        let root = tempfile::tempdir().unwrap();
        symlink(outside.path().join("secret"), root.path().join("leak")).unwrap();
        // Opening follows the symlink; verifying the *handle* catches the escape.
        assert!(matches!(resolve(&canonical(&root), "/leak").await, Err(Status::NotFound)));
    }

    /// A symlinked *directory* component pointing outside the root is refused —
    /// this is the path the TOCTOU order specifically protects.
    #[tokio::test]
    async fn rejects_symlink_escape_dir() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("passwd"), b"root:x:0:0").unwrap();
        let root = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("sub")).unwrap();
        assert!(matches!(resolve(&canonical(&root), "/sub/passwd").await, Err(Status::NotFound)));
    }

    /// A missing file is NotFound, not an error that leaks the tree shape.
    #[tokio::test]
    async fn missing_is_not_found() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(resolve(&canonical(&root), "/nope").await, Err(Status::NotFound)));
    }

    /// Directories themselves are hidden in v1.
    #[tokio::test]
    async fn directories_hidden() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        assert!(matches!(resolve(&canonical(&root), "/dir").await, Err(Status::NotFound)));
    }
}

#[cfg(test)]
mod source_cap_tests {
    use super::SourceCounts;
    use std::sync::Arc;

    fn ip(s: &str) -> std::net::IpAddr {
        s.parse().unwrap()
    }

    /// One address gets its share and no more, and the shares are independent
    /// — the point of the cap is that a peer filling its own quota does not
    /// take anyone else's.
    #[test]
    fn a_source_is_capped_without_capping_its_neighbours() {
        let counts: Arc<SourceCounts> = Arc::new(SourceCounts::default());
        let noisy = ip("203.0.113.7");
        let quiet = ip("203.0.113.8");

        let held: Vec<_> = (0..3).map(|_| counts.claim(noisy, 3).expect("within cap")).collect();
        assert!(counts.claim(noisy, 3).is_none(), "the cap must actually stop the fourth");
        assert!(counts.claim(quiet, 3).is_some(), "a different peer is unaffected");

        // Releasing one frees exactly one.
        drop(held.into_iter().next_back());
        assert!(counts.claim(noisy, 3).is_some(), "a closed connection returns its slot");
    }

    /// The table is keyed by an address the network chooses, so it has to
    /// empty itself — otherwise refusing connections is a slow memory leak,
    /// and the refusals are exactly what an attacker can generate cheaply.
    #[test]
    fn the_table_holds_only_live_connections() {
        let counts: Arc<SourceCounts> = Arc::new(SourceCounts::default());
        for octet in 0..200u8 {
            let addr = ip(&format!("198.51.100.{octet}"));
            let slot = counts.claim(addr, 4).expect("first from this address");
            drop(slot);
        }
        assert_eq!(counts.tracked_addresses(), 0, "closed addresses left entries behind");

        // A refused claim must not leave one either.
        let addr = ip("198.51.100.1");
        let held = counts.claim(addr, 1).unwrap();
        assert!(counts.claim(addr, 1).is_none());
        drop(held);
        assert_eq!(counts.tracked_addresses(), 0, "a refusal left an entry behind");

        // And a claim refused at cap 0 — the degenerate config — leaves none.
        assert!(counts.claim(ip("198.51.100.2"), 0).is_none());
        assert_eq!(counts.tracked_addresses(), 0, "a zero cap left an entry behind");
    }
}

#[cfg(test)]
mod budget_tests {
    use super::{ReadOutcome, ServerConfig, read_budgeted};
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    fn cfg(idle_ms: u64, frame_ms: u64) -> ServerConfig {
        let mut c = ServerConfig::new(".", ".");
        c.idle_timeout = Duration::from_millis(idle_ms);
        c.frame_timeout = Duration::from_millis(frame_ms);
        c
    }

    /// No bytes within the idle window → Idle (not fatal).
    #[tokio::test]
    async fn idle_with_no_bytes() {
        let (mut a, _b) = tokio::io::duplex(64); // keep _b open (no EOF)
        assert!(matches!(read_budgeted(&mut a, &cfg(30, 10_000)).await, ReadOutcome::Idle));
    }

    /// Clean EOF before any byte → Closed.
    #[tokio::test]
    async fn clean_close_on_eof() {
        let (mut a, b) = tokio::io::duplex(64);
        drop(b);
        assert!(matches!(read_budgeted(&mut a, &cfg(10_000, 10_000)).await, ReadOutcome::Closed));
    }

    /// The slowloris case: a frame's first byte arrives (idle satisfied) but the
    /// rest never comes. The frame budget — not the long idle window — fires,
    /// so the connection is dropped fast instead of pinning a slot.
    #[tokio::test]
    async fn partial_frame_hits_frame_budget() {
        let (mut a, mut b) = tokio::io::duplex(64);
        b.write_all(&[0x01]).await.unwrap(); // one byte, then stall
        let out = read_budgeted(&mut a, &cfg(10_000, 30)).await; // idle huge, frame tiny
        assert!(matches!(out, ReadOutcome::Fatal(_)), "slow frame must be fatal, not hang");
        drop(b);
    }
}
