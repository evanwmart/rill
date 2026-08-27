//! Async fetching for [`AppView`](crate::AppView): sources, a persistent
//! reused connection, and the launcher / app-launch / update helpers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::oneshot;
use rill_app::InstallStore;
use rill_client::{Client, ClientConfig, ClientError, util};
use rill_ui::ActionValue;

/// How long to wait for a server connection before giving up. Bounds the
/// "offline" case: an unreachable server fails cleanly in seconds instead of
/// hanging on the OS TCP timeout, so the surface shows an error, not a
/// perpetual "loading…". (Cached/packed resources never reach here.)
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// Connect to a Rill server, failing fast if it is unreachable.
async fn connect(host: &str, port: u16, cfg: ClientConfig) -> Result<Client, String> {
    match tokio::time::timeout(CONNECT_TIMEOUT, Client::connect(host, port, cfg)).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(_) => Err(format!("{host}:{port}: server unreachable (timed out)")),
    }
}

/// Where a document came from — determines how link targets resolve.
#[derive(Clone, Debug)]
pub enum Source {
    Remote { host: String, port: u16, path: String },
    Local { dir: PathBuf, path: String },
    /// The locally generated launcher document.
    Launcher,
    /// An installed application, served pack-first then origin server.
    App { key: String, name: String, path: String },
    /// A document the host generated in memory (e.g. the shell dock).
    Generated { label: String, bytes: Vec<u8> },
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Source::Remote { host, port, path } => format!("rill://{host}:{port}{path}"),
            Source::Local { path, .. } => path.clone(),
            Source::Launcher => "launcher".to_string(),
            Source::App { name, path, .. } => format!("{name} — {path}"),
            Source::Generated { label, .. } => label.clone(),
        }
    }

    pub fn with_path(&self, target: &str) -> Source {
        match self {
            Source::Remote { host, port, .. } => {
                Source::Remote { host: host.clone(), port: *port, path: target.to_string() }
            }
            Source::Local { dir, .. } => {
                Source::Local { dir: dir.clone(), path: target.to_string() }
            }
            Source::Launcher => Source::Launcher,
            Source::App { key, name, .. } => {
                Source::App { key: key.clone(), name: name.clone(), path: target.to_string() }
            }
            Source::Generated { label, bytes } => {
                Source::Generated { label: label.clone(), bytes: bytes.clone() }
            }
        }
    }
}

/// The client half of connection.md §5: ping after this much idle …
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
/// … and an unanswered ping after this long means the peer is gone.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// The persistent connection plus when it last carried traffic.
struct Conn {
    host: String,
    port: u16,
    client: Client,
    last_used: std::time::Instant,
}

/// Async fetcher on a dedicated runtime, with one persistent connection
/// reused across navigations (reconnects transparently).
pub struct Fetcher {
    runtime: tokio::runtime::Runtime,
    pub(crate) identity_dir: PathBuf,
    pub(crate) cache_dir: Option<PathBuf>,
    pub(crate) data_dir: PathBuf,
    connection: tokio::sync::Mutex<Option<Conn>>,
}

impl Fetcher {
    pub fn new(
        identity_dir: PathBuf,
        cache_dir: Option<PathBuf>,
        data_dir: PathBuf,
    ) -> Result<Arc<Fetcher>, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        let fetcher = Arc::new(Fetcher {
            runtime,
            identity_dir,
            cache_dir,
            data_dir,
            connection: tokio::sync::Mutex::new(None),
        });
        // Keepalive driver (connection.md §5): the spec assigns pinging to
        // the client library, and until now nothing drove Client::ping.
        // Weak, so the task never keeps a dropped Fetcher's runtime alive.
        let weak = Arc::downgrade(&fetcher);
        fetcher.runtime.spawn(async move {
            let mut tick = tokio::time::interval(KEEPALIVE_IDLE);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(fetcher) = weak.upgrade() else { break };
                fetcher.keepalive().await;
            }
        });
        Ok(fetcher)
    }

    /// Ping the held connection if it has sat idle past the threshold; a
    /// failed or timed-out ping drops it so the next fetch reconnects
    /// instead of inheriting a dead socket.
    async fn keepalive(&self) {
        let mut slot = self.connection.lock().await;
        let Some(conn) = slot.as_mut() else { return };
        if conn.last_used.elapsed() < KEEPALIVE_IDLE {
            return;
        }
        match tokio::time::timeout(KEEPALIVE_TIMEOUT, conn.client.ping()).await {
            Ok(Ok(_)) => conn.last_used = std::time::Instant::now(),
            _ => *slot = None,
        }
    }

    pub fn data_dir(&self) -> &PathBuf {
        &self.data_dir
    }

    pub(crate) fn spawn_fetch(
        self: &Arc<Self>,
        source: Source,
        tx: oneshot::Sender<Result<Vec<u8>, String>>,
    ) {
        let fetcher = self.clone();
        self.runtime.spawn(async move {
            let _ = tx.send(fetcher.fetch(&source, true, None).await.map(|page| match page {
                PageResult::Fresh { bytes, .. } => bytes,
                PageResult::Unchanged => unreachable!("no held hash was sent"),
            }));
        });
    }

    /// Fetch a page. `cached = false` bypasses the disk content cache: a
    /// `live` page reloads itself on a clock and is expected to be different
    /// each time, and storing those responses fills the cache with objects
    /// that are orphaned by the very next tick — the measured cause of a
    /// 225 MiB cache holding 9.9 MiB of reachable content.
    ///
    /// `held` is the in-memory hash of the page the caller already shows:
    /// when set, a remote fetch goes out as GET_IF and an unchanged page
    /// comes back as [`PageResult::Unchanged`] — a hash comparison on the
    /// wire instead of a transfer (connection.md §4; the disk cache stays
    /// uninvolved either way).
    pub(crate) fn spawn_fetch_page(
        self: &Arc<Self>,
        source: Source,
        cached: bool,
        held: Option<[u8; 32]>,
        tx: oneshot::Sender<Result<PageResult, String>>,
    ) {
        let fetcher = self.clone();
        self.runtime.spawn(async move {
            let _ = tx.send(fetcher.fetch(&source, cached, held).await);
        });
    }

    pub(crate) fn spawn_action(
        self: &Arc<Self>,
        host: String,
        port: u16,
        endpoint: String,
        fields: Vec<(String, ActionValue)>,
        tx: oneshot::Sender<Result<PageResult, String>>,
    ) {
        let fetcher = self.clone();
        self.runtime.spawn(async move {
            // Rides the shared per-origin connection like every GET — a
            // fresh TLS dial per click was measurable overhead. No
            // stale-socket retry, though: see `with_client`.
            let result = fetcher
                .with_client(&host, port, false, |client| {
                    let endpoint = endpoint.clone();
                    let fields = fields.clone();
                    Box::pin(async move { client.action(&endpoint, fields).await })
                })
                .await;
            let _ = tx.send(result.map(|bytes| PageResult::Fresh { bytes, hash: None }));
        });
    }

    async fn fetch(
        &self,
        source: &Source,
        cached: bool,
        held: Option<[u8; 32]>,
    ) -> Result<PageResult, String> {
        // Only a remote round trip can profit from the held hash; every
        // local source just serves fresh bytes.
        let fresh = |bytes: Vec<u8>| Ok(PageResult::Fresh { bytes, hash: None });
        match source {
            Source::Generated { bytes, .. } => fresh(bytes.clone()),
            Source::Launcher => {
                fresh(generate_launcher(
                    &InstallStore::open(&self.data_dir).map_err(|e| e.to_string())?,
                )?)
            }
            Source::App { key, path, .. } => {
                let store = InstallStore::open(&self.data_dir).map_err(|e| e.to_string())?;
                if let Some(bytes) = store.read_resource(key, path).map_err(|e| e.to_string())? {
                    return fresh(bytes);
                }
                let app = store
                    .get(key)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("{key}: not installed"))?;
                self.fetch_remote(&app.host, app.port, path, cached, held).await
            }
            Source::Local { dir, path } => {
                let file = dir.join(path.trim_start_matches('/'));
                fresh(tokio::fs::read(&file).await.map_err(|e| format!("{}: {e}", file.display()))?)
            }
            Source::Remote { host, port, path } => {
                self.fetch_remote(host, *port, path, cached, held).await
            }
        }
    }

    async fn fetch_remote(
        &self,
        host: &str,
        port: u16,
        path: &str,
        cached: bool,
        held: Option<[u8; 32]>,
    ) -> Result<PageResult, String> {
        self.with_client(host, port, true, |client| {
            let path = path.to_string();
            Box::pin(async move { get_with(client, &path, cached, held).await })
        })
        .await
    }

    /// Run one protocol operation on the shared per-origin connection,
    /// dialling (and remembering the connection) when there is none.
    ///
    /// `retry_on_stale` is the read/write distinction: a GET that dies on a
    /// stale socket is safely re-run on a fresh connection, but an ACTION is
    /// not — a transport error mid-write is ambiguous (the server may have
    /// applied it before the socket died), so the error surfaces and any
    /// retry is the caller's deliberate decision. The keepalive ping keeps
    /// the shared connection from going stale in the first place.
    async fn with_client<T>(
        &self,
        host: &str,
        port: u16,
        retry_on_stale: bool,
        mut op: impl for<'a> FnMut(
            &'a mut Client,
        ) -> futures::future::BoxFuture<'a, Result<T, ClientError>>,
    ) -> Result<T, String> {
        let mut slot = self.connection.lock().await;
        if let Some(conn) = slot.as_mut()
            && conn.host == host
            && conn.port == port
        {
            match op(&mut conn.client).await {
                Ok(v) => {
                    conn.last_used = std::time::Instant::now();
                    return Ok(v);
                }
                Err(e @ ClientError::Server { status, .. }) if !status.closes_connection() => {
                    conn.last_used = std::time::Instant::now();
                    return Err(e.to_string());
                }
                Err(e) => {
                    *slot = None;
                    if !retry_on_stale {
                        return Err(e.to_string());
                    }
                }
            }
        } else {
            *slot = None;
        }
        let (fingerprint, device) = util::client_identity_for(&self.identity_dir, host, port)?;
        let mut cfg = ClientConfig::new(fingerprint);
        cfg.device = device;
        cfg.cache_dir = self.cache_dir.clone();
        let mut client = connect(host, port, cfg).await?;
        let keep = |client: Client| Conn {
            host: host.to_string(),
            port,
            client,
            last_used: std::time::Instant::now(),
        };
        match op(&mut client).await {
            Ok(v) => {
                *slot = Some(keep(client));
                Ok(v)
            }
            Err(e @ ClientError::Server { status, .. }) if !status.closes_connection() => {
                *slot = Some(keep(client));
                Err(e.to_string())
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

/// A page fetch's outcome, hash included where the wire computed one.
#[derive(Debug)]
pub(crate) enum PageResult {
    Fresh { bytes: Vec<u8>, hash: Option<[u8; 32]> },
    /// GET_IF against the held hash answered NOT_MODIFIED: what the caller
    /// already shows is current.
    Unchanged,
}

/// One fetch: conditional against a held hash when there is one, else plain,
/// with or without the content cache behind it.
async fn get_with(
    client: &mut Client,
    path: &str,
    cached: bool,
    held: Option<[u8; 32]>,
) -> Result<PageResult, ClientError> {
    if let Some(known) = held {
        return Ok(match client.get_if_held(path, known).await? {
            None => PageResult::Unchanged,
            Some(f) => PageResult::Fresh { hash: Some(f.hash.0), bytes: f.data },
        });
    }
    let f = if cached { client.get(path).await? } else { client.get_uncached(path).await? };
    Ok(PageResult::Fresh { hash: Some(f.hash.0), bytes: f.data })
}

/// Build the launcher document from the install index — a plain Rill page.
pub fn generate_launcher(store: &InstallStore) -> Result<Vec<u8>, String> {
    let apps = store.list().map_err(|e| e.to_string())?;
    let mut kdl = String::from(
        "style \"title\" size=26 weight=\"bold\" color=\"#1a1a2e\"\n\
         style \"muted\" color=\"#70708a\"\n\
         style \"small\" size=12 color=\"#9a9ab0\"\n\
         style \"card\" background=\"#e9e9f4\" corner=10\n\n\
         column gap=12 padding=24 {\n\
         \trow gap=12 { text \"Rill\" style=\"title\"; spacer; text \"launcher\" style=\"small\" }\n",
    );
    if apps.is_empty() {
        kdl.push_str(
            "\tcolumn gap=6 padding=14 style=\"card\" {\n\
             \t\ttext \"No applications installed.\"\n\
             \t\ttext \"rill app install rill://host:port/path-to-manifest\" style=\"muted\"\n\
             \t}\n",
        );
    } else {
        kdl.push_str("\ttext \"Installed applications\" style=\"muted\"\n");
        for app in &apps {
            kdl.push_str(&format!(
                "\trow gap=10 padding=12 style=\"card\" {{ link {} target=\"/~launch/{}\"; spacer; \
                 text \"rill://{}:{}\" style=\"small\"; text \"v {}\" style=\"small\" }}\n",
                rill_doc::kdl_escape(&app.name),
                app.key,
                app.host,
                app.port,
                &app.current.to_hex()[..8],
            ));
        }
    }
    kdl.push('}');
    rill_doc::compile(&kdl).map(|c| c.bytes).map_err(|e| format!("launcher generation: {e}"))
}

/// Resolve an installed app into a launchable source (promoting any staged
/// update first).
pub fn launch_source(data_dir: &PathBuf, key: &str) -> Result<Source, String> {
    let store = InstallStore::open(data_dir).map_err(|e| e.to_string())?;
    // A launch names an install key — or an app id, which is what a
    // *served* launcher can speak: the server proposes an id, the device
    // resolves it against its own installs. Two installs of the same id
    // resolve to whichever lists first; ids are unique per server, and a
    // device holding both has made its own bed.
    let key = match store.get(key).map_err(|e| e.to_string())? {
        Some(_) => key.to_string(),
        None => store
            .list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|i| i.app_id == key)
            .map(|i| i.key)
            .ok_or_else(|| format!("{key}: not installed"))?,
    };
    let key = key.as_str();
    let _ = store.promote_staged(key);
    let installed = store
        .get(key)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{key}: not installed"))?;
    let manifest = store.manifest(key).map_err(|e| e.to_string())?;
    Ok(Source::App { key: key.to_string(), name: installed.name, path: manifest.entry })
}
