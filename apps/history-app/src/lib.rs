//! The machine's memory as an app (specs/history.md).
//!
//! Three views over one corpus, and the point is that they are the *same*
//! views everything else gets:
//!
//! * `/history` — the standing timeline: what the desktop showed, freshest
//!   first, live-refreshing. This is decision 9's standing agent tail worn
//!   as a page.
//! * `/history/actions/search` — token search over the transcript, served
//!   as a results page.
//! * `/history/context` — the agent's read: the recent transcript as a
//!   diary, the exact shape an LLM wants ("text in time order, not a frame
//!   dump"). An agent doing `rill get rill://host/history/context` today
//!   receives what the future agent surface will consume — this app *is*
//!   the first client of that surface, which is how the spec said it would
//!   go: the history app, the agent, and compliance export are all clients
//!   of one served query surface.
//!
//! Two deliberate postures:
//!
//! * **Every page declares `sensitive tier=1`.** A history viewer's content
//!   is transcripts; recording it at T0 would echo everything it shows back
//!   into the routine index — the mirror in the mirror. Classified T1, the
//!   echo stays out of the index this app itself reads.
//! * **This app shows tier 0 only.** Sensitive and sealed tiers stay out of
//!   a casually-open window; reading them is a deliberate act (`rill
//!   history --tier N`), and the footer says so rather than hiding that
//!   they exist. Brokered-and-logged reads (the spec's full answer) come
//!   later; the server's deny-by-default policy on `/history/**` is the
//!   gate today.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use rill_auth::Identity;
use rill_history::corpus::{Corpus, CorpusHit};
use rill_history::crypt::Kek;
use rill_history::event::T0_ROUTINE;
use rill_protocol::{ActionValue, Status};
use rill_server::AppHandler;

/// How many transcript entries the timeline and the context view carry.
const TAIL: usize = 14;
const CONTEXT_TAIL: usize = 40;
/// Timeline refresh cadence.
const LIVE_MS: u16 = 2000;

/// App styles past the kit's own: the timeline rows.
const EXTRA_STYLES: &str = "\
style \"quiet\" size=12 color=\"text-muted\"\n\
style \"when\" size=12 color=\"text-muted\" width=64\n\
style \"win\" size=12 color=\"accent\" width=120 ellipsis=#true\n\
style \"what\" size=13 ellipsis=#true\n\
style \"entry\" padding-y=4\n";

pub struct History {
    dir: PathBuf,
    /// The device identity dir the KEK derives from. Resolved lazily per
    /// request — enrolment may happen after the server started.
    identity_dir: PathBuf,
    /// Monotonic page revision; bumped whenever the corpus dir looks
    /// different, so live polling costs a stat walk, not a page build.
    rev: AtomicU64,
    /// The three standing pages, built once per corpus change.
    ///
    /// Every fetch used to open the corpus from scratch, and opening the
    /// corpus decodes the *open* segment — the one that grows all day —
    /// twice: once to scan it, once for the tail. Eight concurrent fetches
    /// therefore decompressed the same megabytes sixteen times at once;
    /// measured under a burst of 24 fetches, this one page cost the server
    /// +42 MiB of arena high-water against ~6 MiB for any other page. The
    /// stamp is the same fingerprint `revision` trusts, so serving cached
    /// bytes while it holds is exactly the contract polling already relies
    /// on. Built under the lock, so a burst builds once and the rest wait
    /// the few milliseconds instead of allocating in parallel.
    pages: Mutex<Option<PageCache>>,
}

struct PageCache {
    stamp: u64,
    page: Vec<u8>,
    context: Vec<u8>,
    data: Vec<u8>,
}

impl History {
    pub fn new(dir: PathBuf, identity_dir: PathBuf) -> History {
        History { dir, identity_dir, rev: AtomicU64::new(1), pages: Mutex::new(None) }
    }

    fn corpus(&self) -> Result<Corpus, Status> {
        let kek = Kek::from_identity_dir(&self.identity_dir);
        Corpus::open_with(&self.dir, kek).map_err(|_| Status::NotFound)
    }

    /// A cheap fingerprint of the corpus directory: total size + newest
    /// mtime. Grows monotonically as segments grow, which is the revision
    /// contract's safe construction.
    fn dir_stamp(&self) -> u64 {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return 0 };
        let mut sum = 0u64;
        for e in entries.flatten() {
            if let Ok(m) = e.metadata() {
                sum = sum
                    .wrapping_add(m.len())
                    .wrapping_add(
                        m.modified()
                            .ok()
                            .and_then(|t| {
                                t.duration_since(std::time::UNIX_EPOCH).ok()
                            })
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0),
                    );
            }
        }
        sum
    }

    /// Absolute local clock time, deliberately — the first version said
    /// "55s ago", and a stamp that ages is a page that changes on every
    /// refresh whether or not anything happened. The page must be a pure
    /// function of the corpus: same content, same bytes, so the live tick
    /// answers NOT_MODIFIED, the client never redraws, and the recorder
    /// stays quiet. Relative stamps turned this window into a perpetual
    /// motion machine: an idle desktop with History open recorded 104 KiB
    /// a minute of nothing but its own reflection.
    fn stamp(wall_ms: u64) -> String {
        let secs = (wall_ms / 1000) as i64;
        // Local midnight offset without a tz database: ask libc, the way
        // the dock's clock does... except this crate keeps zero native
        // deps, so read the offset from the difference the C library would
        // apply. Fall back to UTC when TZ is unresolvable.
        let local = secs + Self::local_offset_secs();
        let rem = local.rem_euclid(86_400);
        let (h, m) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32);
        let (h12, half) = match h {
            0 => (12, "AM"),
            1..=11 => (h, "AM"),
            12 => (12, "PM"),
            _ => (h - 12, "PM"),
        };
        format!("{h12}:{m:02} {half}")
    }

    /// The local UTC offset via libc, the way the dock's clock reads it.
    /// Cached: the offset shifting under a running process (a DST edge)
    /// costs at most stale stamps until restart, which beats re-asking on
    /// every entry of every page.
    fn local_offset_secs() -> i64 {
        use std::sync::OnceLock;
        static OFFSET: OnceLock<i64> = OnceLock::new();
        *OFFSET.get_or_init(|| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as libc::time_t)
                .unwrap_or(0);
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            // SAFETY: localtime_r fills the provided tm; a null return
            // leaves it zeroed, which reads as UTC.
            unsafe { libc::localtime_r(&now, &mut tm) };
            tm.tm_gmtoff
        })
    }

    fn snippet(text: &str, max: usize) -> String {
        let cleaned: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let mut out: String = cleaned.chars().take(max).collect();
        if cleaned.chars().count() > max {
            out.push('…');
        }
        out
    }

    fn hit_rows(hits: &[CorpusHit], kdl: &mut String) {
        for h in hits {
            let title = if h.title.is_empty() { "(untitled)".into() } else { h.title.clone() };
            kdl.push_str(&format!(
                "\t\t\trow style=\"entry\" gap=8 {{ text {} style=\"when\"; text {} \
                 style=\"win\"; text {} style=\"what\" }}\n",
                rill_doc::kdl_escape(&History::stamp(h.wall_ms)),
                rill_doc::kdl_escape(&History::snippet(&title, 18)),
                rill_doc::kdl_escape(&History::snippet(&h.text, 96)),
            ));
        }
    }

    /// Kit shell around a body: titlebar claim (the app owns its strip, the
    /// convention every other app follows), the two views as sidebar
    /// places, search in the toolbar — Enter submits.
    fn shell(&self, current: &str, body: &str) -> Result<Vec<u8>, Status> {
        let metrics =
            rill_appkit::Metrics::from_theme_file(&rill_appkit::Metrics::theme_path());
        // A claimed bar carries its own close — a toolbar member like any
        // trailing control (the host draws none once a document owns the
        // strip). Forgetting it shipped a window with no ✕ at all.
        let titlebar = format!(
            "{}{}",
            rill_appkit::sidebar_header(&rill_appkit::location_title("History")),
            rill_appkit::toolbar(&format!(
                "{}{}",
                rill_appkit::search_field(
                    "q",
                    "Search everything you've seen\u{2026}",
                    "submit \"/history/actions/search\" { field \"q\" from=\"q\" }",
                ),
                rill_appkit::close_button(),
            )),
        );
        let places = [
            rill_appkit::Place {
                label: "Timeline".into(),
                target: "/history".into(),
                icon: "clock-fill".into(),
                current: current == "timeline",
            },
            rill_appkit::Place {
                label: "Agent context".into(),
                target: "/history/context".into(),
                icon: "world".into(),
                current: current == "context",
            },
        ];
        let kdl = rill_appkit::shell(&rill_appkit::Shell {
            metrics,
            states: "state \"q\" initial=\"\"\n",
            titlebar: &titlebar,
            places: &places,
            footer: None,
            sidebar_top_gap: metrics.sidebar_align_gap() as u32,
            extra_styles: EXTRA_STYLES,
            content_style: None,
            body,
            rail_body: None,
            scroll_content: true,
        });
        rill_appkit::compile_page("history-app", &kdl)
    }

    /// The timeline: stats, search, the standing tail.
    fn page(&self) -> Result<Vec<u8>, Status> {
        let corpus = self.corpus()?;
        // Stats over SEALED segments only, deliberately. The open segment
        // grows with this very window's own recorded frames, so a live
        // count of it turns the stats line into a self-ticking clock — the
        // page changes because the page was shown. Sealed totals move only
        // at rotation, which is what lets an idle History window serve the
        // same bytes forever and cost nothing.
        let sealed: Vec<_> = corpus
            .segments()
            .iter()
            .filter(|s| {
                matches!(
                    rill_history::segment::read_seal(&s.path),
                    Ok(Some(_)) | Err(rill_history::segment::SegmentError::Locked(_))
                )
            })
            .collect();
        let segments = sealed.len();
        let events: u64 = sealed.iter().map(|s| s.events).sum();
        let bytes: u64 = sealed.iter().map(|s| s.size).sum();
        let tail = corpus.tail(TAIL, T0_ROUTINE);

        let mut body = String::new();
        body.push_str("\t\t\tsensitive tier=1\n");
        body.push_str(&format!(
            "\t\t\ttext {} style=\"quiet\"\n",
            rill_doc::kdl_escape(&format!(
                "{segments} sealed segment(s) · {events} events · {} KiB · what the desktop \
                 showed, as it showed it",
                bytes / 1024
            ))
        ));
        if tail.is_empty() {
            body.push_str("\t\t\ttext \"Nothing recorded yet.\" style=\"quiet\"\n");
        } else {
            History::hit_rows(&tail, &mut body);
        }
        body.push_str(
            "\t\t\trow gap=8 { spacer; text \"T1/T2 stay out of this view — rill history \
             --tier\" style=\"quiet\" }\n",
        );
        body.push_str(&format!("\t\t\tlive target=\"/history\" every={LIVE_MS}\n"));
        self.shell("timeline", &body)
    }

    fn results(&self, query: &str) -> Result<Vec<u8>, Status> {
        let corpus = self.corpus()?;
        let (hits, _opened) = corpus.search(query, T0_ROUTINE, 50);
        let mut body = String::new();
        body.push_str("\t\t\tsensitive tier=1\n");
        body.push_str(&format!(
            "\t\t\trow gap=8 {{ link \"\u{2190} Timeline\" target=\"/history\"; text {} \
             style=\"quiet\" }}\n",
            rill_doc::kdl_escape(&format!("{} hit(s) for \u{201c}{query}\u{201d}", hits.len()))
        ));
        if hits.is_empty() {
            body.push_str(
                "\t\t\ttext \"Nothing matched. Search is whole-token.\" style=\"quiet\"\n",
            );
        } else {
            History::hit_rows(&hits, &mut body);
        }
        self.shell("", &body)
    }

    /// The agent's read: the recent transcript as a diary. Deliberately
    /// sparse — an LLM's context window is the budget this page spends.
    fn context(&self) -> Result<Vec<u8>, Status> {
        let corpus = self.corpus()?;
        let tail = corpus.tail(CONTEXT_TAIL, T0_ROUTINE);
        let mut body = String::new();
        body.push_str("\t\t\tsensitive tier=1\n");
        body.push_str(
            "\t\t\ttext \"The machine-readable recent past: the same query surface an \
             agent reads — no screenshots, no OCR, the text as it was shown.\" \
             style=\"quiet\"\n",
        );
        History::hit_rows(&tail, &mut body);
        self.shell("context", &body)
    }

    /// The same tail with no drawing at all — one entry per line,
    /// `wall_ms\twindow\ttext`. `rill get rill://host/history/data` is an
    /// agent's first tool call.
    fn data(&self) -> Result<Vec<u8>, Status> {
        let corpus = self.corpus()?;
        let tail = corpus.tail(CONTEXT_TAIL, T0_ROUTINE);
        let mut out = String::new();
        for h in &tail {
            out.push_str(&format!(
                "{}\t{}\t{}\n",
                h.wall_ms,
                History::snippet(&h.title, 40),
                History::snippet(&h.text, 400)
            ));
        }
        Ok(out.into_bytes())
    }
}

impl AppHandler for History {
    fn get(&self, path: &str, _identity: &Identity) -> Option<Vec<u8>> {
        let which = match path {
            "/history" | "/history/" => 0,
            "/history/context" => 1,
            "/history/data" => 2,
            _ => return None,
        };
        let stamp = self.dir_stamp();
        let mut cache = self.pages.lock().unwrap_or_else(|p| p.into_inner());
        if cache.as_ref().is_none_or(|c| c.stamp != stamp) {
            *cache = Some(PageCache {
                stamp,
                page: self.page().ok()?,
                context: self.context().ok()?,
                data: self.data().ok()?,
            });
        }
        let c = cache.as_ref()?;
        Some(match which {
            0 => c.page.clone(),
            1 => c.context.clone(),
            _ => c.data.clone(),
        })
    }

    fn revision(&self, path: &str, _identity: &Identity) -> Option<u64> {
        if path != "/history" && path != "/history/" {
            return None;
        }
        // Fold the dir fingerprint into a monotonic counter: the counter
        // only moves when the fingerprint does, and never backwards.
        let stamp = self.dir_stamp();
        let prev = self.rev.load(Ordering::Relaxed);
        // Low bits carry the counter, high bits the last stamp hash.
        let stamp_hash = stamp.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
        if prev >> 32 != stamp_hash {
            let next = ((stamp_hash) << 32) | ((prev & 0xFFFF_FFFF) + 1);
            self.rev.store(next, Ordering::Relaxed);
            return Some(next);
        }
        Some(prev)
    }

    fn action(
        &self,
        path: &str,
        fields: &[(String, ActionValue)],
        _identity: &Identity,
    ) -> Result<Vec<u8>, Status> {
        match path {
            "/history/actions/search" => {
                let q = fields
                    .iter()
                    .find(|(name, _)| name == "q")
                    .and_then(|(_, v)| match v {
                        ActionValue::Str(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                if q.trim().is_empty() {
                    return self.page();
                }
                self.results(q.trim())
            }
            _ => Err(Status::NotFound),
        }
    }
}
