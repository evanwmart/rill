//! The system-of-record: the compositor's live feed into `.rhs` history
//! segments (specs/history.md).
//!
//! This runs beside the `.rillrec` recorder, not instead of it — that one is
//! the session-demo replay format, toggled by a person; this one is the
//! always-on substrate the history CLI, retention and the agent surface read
//! (decision 1: always-on, sensitivity is classification, the escape hatch is
//! hard delete).
//!
//! The division of labour is strict, because the render loop must never wait
//! on a disk:
//!
//! * **The compositor's side is a `try_send`.** Frames are cloned at the
//!   latch (they were already in hand), window state goes over as a whole
//!   snapshot at most [`TICK_EVERY`], and if the channel is full the note is
//!   dropped and *counted* — the writer hears about the hole as an honest
//!   [`Event::Gap`], never as silence (shed-and-mark, TODO.md).
//! * **The writer thread owns everything stateful**: diffing snapshots into
//!   Window/Closed/Order events, deduplicating transcripts, the lazy
//!   wall-clock sync, flush deadlines, rotation, and sealing. Every segment
//!   opens with a [`Event::Snapshot`] so it replays self-contained — which is
//!   what lets retention drop whole segments without a replay losing its
//!   footing.
//!
//! Crash honesty comes from the format: a killed compositor leaves an
//! unsealed segment that reads to its last whole chunk, and the *next* start
//! seals it (`seal_path` truncates the torn tail to what was durable). So
//! recovery is not a repair pass, it is the ordinary open path.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use rill_history::crypt::Kek;
use rill_history::event::{Event, GapReason, Stamped, T0_ROUTINE, Tier, WindowState};
use rill_history::segment::{ChunkCodec, Header, SegmentWriter, seal_path_with};

/// Window-state snapshots are sampled at most this often. Geometry at 10 Hz
/// is plenty for history (frames stay exact — they arrive per commit); what
/// this bounds is the cost of an *animated* desktop, where damage ticks run
/// at 60 Hz forever and per-tick snapshots would be 60 allocations a second
/// to say "nothing changed".
const TICK_EVERY: Duration = Duration::from_millis(100);

/// Wall-clock correlation cadence: a `Sync` event rides along once a minute,
/// on the next real event — never on a timer, so an idle desktop appends
/// nothing at all (the segment format's own rule).
const SYNC_EVERY: Duration = Duration::from_secs(60);

/// Bounded queue between the render loop and the disk. Deep enough that a
/// flush stall absorbs a burst of frames; shallow enough that a hung disk
/// costs megabytes, not the machine.
const QUEUE: usize = 256;

enum Note {
    /// The desktop as it stands, bottom → top. The writer diffs it.
    Tick { at: Instant, windows: Vec<WindowState> },
    /// One latched vector frame, verbatim, with the text it painted and the
    /// tier the frame latched at.
    Frame { at: Instant, id: u32, bytes: Vec<u8>, text: Option<String>, tier: Tier },
    /// Notes were dropped on the floor because the queue was full.
    Gap { at: Instant, dropped: u32 },
}

/// The compositor-side handle: cheap sends, a shed counter, a badge.
pub struct History {
    tx: SyncSender<Note>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Notes dropped since the last successful gap report.
    shed: u32,
    last_tick: Option<Instant>,
}

impl History {
    /// Start the writer thread. Any unsealed segment a previous life left in
    /// `dir` is sealed first — recovery is the ordinary open path.
    pub fn start(dir: PathBuf, device: String, kek: Option<Kek>) -> History {
        let (tx, rx) = sync_channel(QUEUE);
        let thread_dir = dir.clone();
        let handle = std::thread::Builder::new()
            .name("rill-history".into())
            .spawn(move || writer_thread(thread_dir, device, kek, rx))
            .expect("spawn history writer");
        History { tx, handle: Some(handle), shed: 0, last_tick: None }
    }

    fn push(&mut self, note: Note) {
        // A pending gap goes first, so the hole lands in the log *before*
        // the event that follows it. If even the gap won't fit, keep
        // counting — the count is a u32 add, unlosable.
        if self.shed > 0 {
            let gap = Note::Gap { at: Instant::now(), dropped: self.shed };
            match self.tx.try_send(gap) {
                Ok(()) => self.shed = 0,
                Err(TrySendError::Full(_)) => {
                    self.shed = self.shed.saturating_add(1);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        if let Err(TrySendError::Full(_)) = self.tx.try_send(note) {
            self.shed = self.shed.saturating_add(1);
        }
    }

    /// The desktop as it stands. Rate-limited here so callers can hand it
    /// over every damage tick without thinking about it.
    pub fn tick(&mut self, windows: Vec<WindowState>) {
        let now = Instant::now();
        if self.last_tick.is_some_and(|t| now.duration_since(t) < TICK_EVERY) {
            return;
        }
        self.last_tick = Some(now);
        self.push(Note::Tick { at: now, windows });
    }

    /// A latched frame, exactly as the client sent it, plus the text it put
    /// on screen (the writer stores the transcript beside the frame so the
    /// frame can age out from under it — decision 3).
    pub fn frame(&mut self, id: u32, bytes: Vec<u8>, text: Option<String>, tier: Tier) {
        self.push(Note::Frame { at: Instant::now(), id, bytes, text, tier });
    }
}

impl Drop for History {
    /// Closing the channel is the shutdown signal; the writer drains what is
    /// queued, seals, and exits. The join is what makes Ctrl+C safe: the
    /// process outlives the last fsync.
    fn drop(&mut self) {
        let (tx, _rx) = sync_channel(1);
        drop(std::mem::replace(&mut self.tx, tx));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Everything stateful, owned by the one thread that blocks on disks.
struct Writer {
    dir: PathBuf,
    device: String,
    /// The device unlock. `Some` encrypts every new segment (decision 2);
    /// `None` — an unenrolled machine — records plaintext, which the header
    /// reports honestly via its empty keyslot table.
    kek: Option<Kek>,
    seg: Option<SegmentWriter>,
    /// Diff state: the desktop as last written.
    windows: std::collections::HashMap<u32, WindowState>,
    order: Vec<u32>,
    /// Transcript dedup: last text written per window.
    last_text: std::collections::HashMap<u32, String>,
    /// The previous event's instant — deltas are computed here, not at the
    /// send site, so queue latency never distorts the timeline order.
    prev: Option<Instant>,
    last_sync: Option<Instant>,
    /// First write error. One report, then quiet — a full disk must not turn
    /// the log into a log about the log.
    failed: bool,
}

fn writer_thread(dir: PathBuf, device: String, kek: Option<Kek>, rx: Receiver<Note>) {
    // Seal whatever a crashed predecessor left open. Idempotent, and cheap
    // for already-sealed files (two small reads apiece).
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "rhs")
                && let Err(err) = seal_path_with(&p, kek.as_ref())
            {
                eprintln!("rill-compositor: history: could not seal {}: {err}", p.display());
            }
        }
    }
    // Then fidelity decay (specs/history.md decision 3): frames past the
    // window go, transcripts stay, pins hold. At boot rather than on a
    // clock — retention drifting a day because the machine slept is
    // harmless, and boot is when the disk story should be settled anyway.
    // `RILL_HISTORY_FRAME_DAYS` overrides the 90-day default (the
    // appliance profile wants shorter).
    let window = std::env::var("RILL_HISTORY_FRAME_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(rill_history::retention::DEFAULT_FRAME_DAYS);
    for (path, result) in rill_history::retention::age_older_than(&dir, window, kek.as_ref()) {
        match result {
            Ok(r) if r.events_before != r.events_after => println!(
                "rill-compositor: history aged {} ({} -> {} bytes)",
                path.file_name().unwrap_or_default().to_string_lossy(),
                r.bytes_before,
                r.bytes_after
            ),
            Ok(_) => {}
            Err(e) => {
                eprintln!("rill-compositor: history: could not age {}: {e}", path.display())
            }
        }
    }

    let mut w = Writer {
        dir,
        device,
        kek,
        seg: None,
        windows: Default::default(),
        order: Vec::new(),
        last_text: Default::default(),
        prev: None,
        last_sync: None,
        failed: false,
    };
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(note) => w.note(note),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Nothing arrived; honour the flush deadline if one is
                // running. An idle desktop has no pending events and this
                // does nothing at all.
                if let Some(seg) = &mut w.seg
                    && seg.flush_due()
                    && let Err(e) = seg.flush()
                {
                    w.write_failed(&e.to_string());
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    w.close();
}

impl Writer {
    fn write_failed(&mut self, e: &str) {
        if !self.failed {
            self.failed = true;
            eprintln!("rill-compositor: history write failed ({e}); recording degraded");
        }
    }

    /// The open segment, created on first use so a desktop that never emits
    /// an event never creates a file.
    fn seg(&mut self) -> Option<&mut SegmentWriter> {
        if self.seg.is_none() {
            let wall_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let path = self.dir.join(format!("{wall_ms}.rhs"));
            let header = Header {
                version: 1,
                device: self.device.clone(),
                wall_start_ms: wall_ms,
                keyslots: Vec::new(),
            };
            match SegmentWriter::create_with_key(
                &path,
                &header,
                ChunkCodec::Zstd,
                3,
                self.kek.as_ref(),
            ) {
                Ok(seg) => {
                    self.seg = Some(seg);
                    self.prev = None;
                    self.last_sync = None;
                    // Every segment opens self-contained: the desktop as it
                    // stands, so retention can drop older segments without a
                    // reader losing its footing. Routine windows ride one
                    // Snapshot; anything higher gets its own Window event at
                    // its own tier — a T2 title inside a T0 snapshot would
                    // put sealed text in the routine index.
                    if !self.windows.is_empty() {
                        let now = Instant::now();
                        let routine: Vec<WindowState> = self
                            .order
                            .iter()
                            .filter_map(|id| self.windows.get(id).cloned())
                            .filter(|w| w.tier == T0_ROUTINE)
                            .collect();
                        if !routine.is_empty() {
                            self.emit(now, T0_ROUTINE, Event::Snapshot { windows: routine });
                        }
                        let raised: Vec<WindowState> = self
                            .order
                            .iter()
                            .filter_map(|id| self.windows.get(id).cloned())
                            .filter(|w| w.tier != T0_ROUTINE)
                            .collect();
                        for w in raised {
                            let tier = w.tier;
                            self.emit(now, tier, Event::Window(w));
                        }
                        let order = self.order.clone();
                        self.emit(now, T0_ROUTINE, Event::Order { ids: order });
                    }
                }
                Err(e) => self.write_failed(&e.to_string()),
            }
        }
        self.seg.as_mut()
    }

    fn emit(&mut self, at: Instant, tier: Tier, event: Event) {
        // The lazy wall-clock sync: piggybacks on real events, never a timer.
        if self.last_sync.is_none_or(|t| at.duration_since(t) >= SYNC_EVERY)
            && !matches!(event, Event::Sync { .. })
        {
            self.last_sync = Some(at);
            let wall_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            self.emit(at, T0_ROUTINE, Event::Sync { wall_ms });
        }
        let dt_ms = match self.prev {
            Some(p) => at.saturating_duration_since(p).as_millis().min(u32::MAX as u128) as u32,
            None => 0,
        };
        self.prev = Some(at);
        let stamped = Stamped { dt_ms, tier, event };
        let Some(seg) = self.seg() else { return };
        if let Err(e) = seg.append(&stamped) {
            let msg = e.to_string();
            self.write_failed(&msg);
            return;
        }
        if self.seg.as_ref().is_some_and(|s| s.should_rotate()) {
            self.rotate();
        }
    }

    fn rotate(&mut self) {
        if let Some(seg) = self.seg.take() {
            match seg.finish() {
                Ok(path) => println!("rill-compositor: history sealed {}", path.display()),
                Err(e) => self.write_failed(&e.to_string()),
            }
        }
        // The next event opens the next segment (with its snapshot seed).
    }

    fn note(&mut self, note: Note) {
        match note {
            Note::Tick { at, windows } => {
                for w in &windows {
                    if self.windows.get(&w.id) != Some(w) {
                        self.windows.insert(w.id, w.clone());
                        // A window's state rides at the window's own tier:
                        // titles are content, and a sensitive window's title
                        // in the routine index is a leak.
                        let tier = w.tier;
                        self.emit(at, tier, Event::Window(w.clone()));
                    }
                }
                let gone: Vec<(u32, Tier)> = self
                    .windows
                    .iter()
                    .filter(|(id, _)| !windows.iter().any(|w| w.id == **id))
                    .map(|(id, w)| (*id, w.tier))
                    .collect();
                for (id, tier) in gone {
                    self.windows.remove(&id);
                    self.last_text.remove(&id);
                    self.emit(at, tier, Event::Closed { id });
                }
                let order: Vec<u32> = windows.iter().map(|w| w.id).collect();
                if order != self.order {
                    self.order = order.clone();
                    self.emit(at, T0_ROUTINE, Event::Order { ids: order });
                }
            }
            Note::Frame { at, id, bytes, text, tier } => {
                // Transcript first, frame second: the reader prefers stored
                // text, and ordering them this way keeps the pair adjacent
                // in one (non-frame, frame) chunk boundary. Both carry the
                // tier the frame latched with — the entire point of the
                // plumbing: what a sensitive page painted is indexed under
                // its tier, never the routine one.
                if let Some(text) = text
                    && self.last_text.get(&id) != Some(&text)
                {
                    self.last_text.insert(id, text.clone());
                    self.emit(at, tier, Event::Text { id, text });
                }
                self.emit(at, tier, Event::Frame { id, bytes });
            }
            Note::Gap { at, dropped } => {
                self.emit(at, T0_ROUTINE, Event::Gap { reason: GapReason::Backpressure, dropped });
            }
        }
    }

    fn close(&mut self) {
        if let Some(seg) = self.seg.take() {
            match seg.finish() {
                Ok(path) => println!("rill-compositor: history sealed {}", path.display()),
                Err(e) => eprintln!("rill-compositor: history close failed: {e}"),
            }
        }
    }
}

/// The owner's tier policy — the ratchet over what documents claim
/// (specs/history.md decisions 1 and 4): `~/.config/rill/history.toml`.
///
/// ```toml
/// # Minimum tier for everything this machine records.
/// floor = 0
///
/// # Minimum tier per app id — "pin the password manager high" (decision 1:
/// # an app the owner does not want in the searchable corpus is pinned to a
/// # high tier, never excluded).
/// [apps]
/// "vaultapp" = 2
/// ```
///
/// Policy only *raises*: the effective tier is the max of what the document
/// declared, the app's pin, and the floor. Lowering a document's own claim
/// is deliberately not expressible — recording less protected than the app
/// asked for is the one direction the ratchet forbids ("only the owner
/// lowers" is about future org policy, not a knob here). Loaded at boot;
/// an edited policy takes effect next start, the same moment the aging
/// window does.
pub struct TierPolicy {
    floor: Tier,
    apps: std::collections::HashMap<String, Tier>,
}

impl TierPolicy {
    pub fn load(path: &std::path::Path) -> TierPolicy {
        let mut out = TierPolicy { floor: 0, apps: Default::default() };
        let Ok(text) = std::fs::read_to_string(path) else { return out };
        let Ok(root) = text.parse::<toml::Table>() else {
            eprintln!(
                "rill-compositor: {} is not valid TOML; recording at declared tiers",
                path.display()
            );
            return out;
        };
        // Clamped into the closed set the format knows: a floor of 9 is a
        // typo, and silently recording everything at an unknown tier would
        // make the whole corpus unreadable.
        let tier = |v: &toml::Value| -> Option<Tier> {
            v.as_integer().and_then(|n| u8::try_from(n).ok()).filter(|t| *t <= 2)
        };
        if let Some(v) = root.get("floor") {
            match tier(v) {
                Some(t) => out.floor = t,
                None => eprintln!(
                    "rill-compositor: history floor {v} is not a known tier (0..=2); ignored"
                ),
            }
        }
        if let Some(apps) = root.get("apps").and_then(|v| v.as_table()) {
            for (app, v) in apps {
                match tier(v) {
                    Some(t) => {
                        out.apps.insert(app.clone(), t);
                    }
                    None => eprintln!(
                        "rill-compositor: history tier for {app:?} is not 0..=2; ignored"
                    ),
                }
            }
        }
        out
    }

    /// The minimum tier policy imposes on a window of this app.
    pub fn min_for(&self, app: Option<&str>) -> Tier {
        let pinned = app.and_then(|a| self.apps.get(a).copied()).unwrap_or(0);
        self.floor.max(pinned)
    }

    pub fn is_default(&self) -> bool {
        self.floor == 0 && self.apps.is_empty()
    }
}

#[cfg(test)]
mod policy_tests {
    use super::TierPolicy;

    fn from(name: &str, text: &str) -> TierPolicy {
        // Per-test files: tests run in parallel, and a shared path is a race.
        let dir = std::env::temp_dir().join(format!("rill-tierpol-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{name}.toml"));
        std::fs::write(&p, text).unwrap();
        TierPolicy::load(&p)
    }

    /// The ratchet only raises: max of declaration, pin, and floor.
    #[test]
    fn policy_raises_and_never_lowers() {
        let pol = from("raises", "floor = 1\n[apps]\n\"vault\" = 2\n");
        assert_eq!(pol.min_for(Some("vault")), 2, "the pin");
        assert_eq!(pol.min_for(Some("notes")), 1, "the floor");
        assert_eq!(pol.min_for(None), 1, "no app id still gets the floor");
        // The caller composes with the declaration by max(); a document
        // declaring 2 under a floor of 1 stays 2 — nothing here lowers.
        assert_eq!(pol.min_for(Some("notes")).max(2), 2);
    }

    /// A tier outside the closed set is a typo, not a policy.
    #[test]
    fn unknown_tiers_are_ignored_not_obeyed() {
        let pol = from("unknown", "floor = 9\n[apps]\n\"x\" = 3\n");
        assert_eq!(pol.min_for(Some("x")), 0);
        assert!(pol.is_default());
    }

    /// No file, no policy — the common machine.
    #[test]
    fn a_missing_file_is_the_default() {
        assert!(TierPolicy::load(std::path::Path::new("/nonexistent/history.toml")).is_default());
    }
}
