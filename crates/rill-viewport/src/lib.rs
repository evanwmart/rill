//! Embeddable Rill application surface (plan § Application Phase 3 / Desktop
//! Phase 1). An [`AppView`] fetches, renders, and drives interaction for one
//! Rill document/app — the whole document pipeline minus any window chrome.
//!
//! Host-agnostic: async work runs on a shared [`Fetcher`] and is advanced by
//! calling [`AppView::poll`] each frame; capability prompts are surfaced to
//! the host via [`AppView::take_capability`] and answered with
//! [`AppView::provide_file`] / [`AppView::cancel_capability`]. One rendering
//! path, no divergence: `rill-vector` hosts an AppView per window (app,
//! dock), and any future host embeds the same engine.

mod fetcher;
pub mod theme;

pub use fetcher::{Fetcher, Source, generate_launcher, launch_source};
pub use rill_ui::Defaults;
pub use theme::DesktopTheme;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use futures::channel::oneshot;
use rill_app::InstallStore;
use rill_protocol_reexport::ActionValue;
use rill_ui::{
    Color, DrawCommand, ImageSizer, LayoutOptions, Rect, ResolvedNode, TextMeasurer, UiAction,
    UiTree, resolve,
};
use rill_ui::MenuItem;

/// A text-input hit region: (rect, state slot, on-Enter action, multiline).
/// A focusable text-input region captured from the last layout, with the font
/// metrics needed to map a click to a caret position.
#[derive(Clone)]
struct InputArea {
    rect: Rect,
    state: u16,
    on_enter: Option<UiAction>,
    multiline: bool,
    tab_inserts: bool,
    font_size: f32,
    font_weight: u16,
    font_family: String,
    pad_x: f32,
    pad_y: f32,
}

/// An interactive element captured from the last layout, in document (tab)
/// order — the unit of both pointer hit-testing and keyboard focus traversal.
#[derive(Clone)]
enum Focusable {
    Input(InputArea),
    Button { rect: Rect, action: UiAction },
    Link { rect: Rect, target: String },
    Slider { rect: Rect, state: u16, min: f32, max: f32, step: f32, on_release: Option<UiAction> },
}

/// Host paths (`/~close`, `/~launch/…`) are the host's to resolve — a
/// window cannot close itself by fetching a page. `/~back` and `/~forward`
/// are the exception: the history stack lives in this view, so it answers
/// those itself.
fn host_path(target: &str) -> bool {
    target.starts_with("/~") && target != "/~back" && target != "/~forward"
}

/// The host path a button's action names, if any: a `navigate` to `/~…`.
/// Buttons perform their actions inside the view, so without this a close
/// button would try to *fetch* `/~close` and answer NOT_FOUND.
fn action_host_link(action: &UiAction) -> Option<String> {
    match action {
        UiAction::Navigate(t) if host_path(t) => Some(t.clone()),
        _ => None,
    }
}

impl Focusable {
    fn rect(&self) -> &Rect {
        match self {
            Focusable::Input(a) => &a.rect,
            Focusable::Button { rect, .. }
            | Focusable::Link { rect, .. }
            | Focusable::Slider { rect, .. } => rect,
        }
    }
}

/// Why a fetch was started. A live tick is the one kind whose failure the
/// view can absorb: the page it would have replaced is still on screen and
/// still true as of its own timestamp, so a blink of the network should not
/// cost you the page *and* the clock that would have brought it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadKind {
    /// Navigation, back/forward, or an explicit reload.
    Nav,
    /// A `live` page re-reading itself on its own clock.
    Live,
    /// The response to an ACTION — a page, re-served.
    Action,
}

/// What one [`AppView::poll`] step did.
///
/// Two separate facts that used to be one bool. `poll` returned
/// `changed || pending`, every caller read it as "repaint", and so a fetch in
/// flight — which changes nothing on screen — drove a redraw on every loop
/// iteration until it landed. Measured on the Pi: 32 client commits per
/// second for 13.5 real updates, each phantom commit costing the compositor a
/// 7.25 ms composite while costing the client almost nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Polled {
    /// The picture is different. Repaint.
    pub changed: bool,
    /// Work is outstanding — a fetch or an image in flight. There is nothing
    /// to repaint *for it*, but the host should come back soon rather than
    /// wait out a full idle timeout, or finishing it would be noticed late.
    pub pending: bool,
}

/// A page/action fetch in flight.
struct PendingFetch {
    generation: u64,
    kind: LoadKind,
    rx: oneshot::Receiver<Result<fetcher::PageResult, String>>,
    /// State slots whose staged value this response is expected to settle —
    /// the slots that were just submitted. Cleared from `dirty` when it
    /// lands, so a delivered form takes the server's fresh value while an
    /// untouched-by-this-action field keeps what the user typed.
    clears: Vec<String>,
}

mod rill_protocol_reexport {
    pub use rill_ui::ActionValue;
}

/// A capability the app requested, awaiting a host-driven trusted prompt.
/// Currently only file picking (application-model.md §10).
pub struct CapabilityRequest {
    pub app_name: String,
    /// State slot the chosen file's content fills.
    pub into: u16,
}

/// A decoded document image: plain RGBA8, backend-neutral. Renderers upload
/// it however they like (the wgpu path's image atlas; a future remote path
/// hashes it for transport).
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: image::RgbaImage,
}

fn decode_image(bytes: &[u8]) -> Result<DecodedImage, String> {
    let decoded = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let rgba = decoded.into_rgba8();
    Ok(DecodedImage { width: rgba.width(), height: rgba.height(), rgba })
}

enum ImageState {
    /// Fetching the bytes.
    Loading(oneshot::Receiver<Result<Vec<u8>, String>>),
    /// Bytes in hand, being decoded and reduced on the scaler thread. There is
    /// nothing to draw yet, exactly as while loading — the difference is only
    /// where the work is happening.
    Preparing(oneshot::Receiver<Prepared>),
    Ready {
        /// The image's own size, which outlives its own pixels: layout asks
        /// for this and nothing else, and it is two numbers.
        natural: (u32, u32),
        /// The power-of-two reduction `pixels` has been taken to (1 = none).
        step: u32,
        /// The picture at roughly the size it is being shown at — the only
        /// copy kept, and the one a host sends onward.
        ///
        /// A photo straight off a phone is 4000x3000, 46 MiB of pixels, and a
        /// thumbnail of it occupies 240x160 on screen. Holding the original
        /// after the reduction exists is that much memory kept to answer a
        /// question (how big is it?) that two integers already answer.
        pixels: Arc<DecodedImage>,
        /// A fetch in flight because the image is now shown larger than
        /// `pixels` can serve — a zoom, a window pulled wider, or a scroll
        /// bringing it back off the floor — and the step it is being fetched
        /// for. The coarse version keeps painting until it lands, so detail
        /// arrives late rather than the picture disappearing.
        refining: Option<Refining>,
        /// Pixels being decoded or coarsened on the scaler thread, and the
        /// step they will arrive at. What is held keeps painting meanwhile.
        scaling: Option<(u32, oneshot::Receiver<Prepared>)>,
    },
    Failed,
}

/// A source fetch in flight for detail the client no longer holds, and the
/// step it is being fetched for.
type Refining = (u32, oneshot::Receiver<Result<Vec<u8>, String>>);

/// What one image made of a poll: bytes to hand on, or a fetch that failed.
enum Landed {
    Bytes { bytes: Vec<u8>, reduce: Reduce, refine_to: Option<u32> },
    /// The first fetch failed — there is no picture and there will not be one.
    Failed,
    /// A refetch failed, which loses detail rather than the picture.
    RefineFailed,
    Nothing,
}

/// A picture decoded and reduced away from the frame path.
struct Prepared {
    natural: (u32, u32),
    step: u32,
    pixels: DecodedImage,
}

/// Where a job should leave a picture.
enum Reduce {
    /// The resident floor for whatever it turns out to be. Used for anything
    /// the window is not showing, where the size is not known until it has
    /// been decoded — so the decision has to travel with the job.
    Floor,
    /// A known power-of-two step (1 = leave it at full size).
    To(u32),
}

enum Job {
    /// Compressed bytes in, reduced pixels out. The queue holds the bytes
    /// rather than the decode, so a page whose pictures all arrive together
    /// queues kilobytes each instead of a full-size picture each.
    Decode { bytes: Vec<u8>, reduce: Reduce, reply: oneshot::Sender<Prepared> },
    /// Pixels already held, taken further down.
    Halve {
        natural: (u32, u32),
        pixels: Arc<DecodedImage>,
        factor: u32,
        to: u32,
        reply: oneshot::Sender<Prepared>,
    },
}

/// Decoding and scaling, off the frame path.
///
/// Both are hundreds of milliseconds for a photograph in a debug build, and
/// both used to run inside `poll` and `layout` — so a window drag that crossed
/// a halving boundary stalled the frame that was meant to show the result.
/// Measured over a 120-step drag of a sixty-photograph roll: 12.2 s of layout
/// before any of this, 1.15 s once culling meant only the visible ones were
/// rescaled, and a 400 ms single frame still inside that.
///
/// Neither job is ever urgent. A coarsening is a memory saving with no visible
/// effect at all, and a decode has nothing to show until it finishes, so both
/// can arrive a frame or two later than they were asked for.
///
/// One worker, deliberately. A second would halve the wait and double the
/// peak, and the peak is the number this whole design is about.
///
/// Which means order is everything, so the queue has two ends. A page of sixty
/// photographs is sixty decodes, and the one the reader is looking at must not
/// wait behind the fifty-nine they are not: measured first-in-first-out, the
/// top photograph of the demo roll stayed coarse for 879 ms in a release build
/// and had not sharpened at all after four seconds in a debug one.
struct Scaler {
    queue: Option<Arc<(Mutex<Queue>, Condvar)>>,
}

#[derive(Default)]
struct Queue {
    /// Detail for something the window is showing. A person is waiting.
    urgent: VecDeque<Job>,
    /// Everything else: first decodes, and coarsenings, which save memory and
    /// change nothing anyone can see.
    background: VecDeque<Job>,
}

impl Scaler {
    fn new() -> Scaler {
        Scaler { queue: None }
    }

    /// Started on first use rather than with the view: most windows never show
    /// a picture, and a thread each for them is a thread each for nothing.
    fn queue(&mut self) -> &Arc<(Mutex<Queue>, Condvar)> {
        self.queue.get_or_insert_with(|| {
            let shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
            let worker = Arc::clone(&shared);
            std::thread::spawn(move || {
                let (lock, wake) = &*worker;
                loop {
                    let job = {
                        let mut q = lock.lock().expect("scaler queue");
                        loop {
                            // Urgent first, always: the queue is drained by
                            // what someone is looking at, not by arrival.
                            if let Some(job) = q.urgent.pop_front().or_else(|| q.background.pop_front()) {
                                break job;
                            }
                            // The view is gone when nothing else holds the
                            // queue, and there is no more work coming.
                            if Arc::strong_count(&worker) == 1 {
                                return;
                            }
                            let (next, timeout) = wake
                                .wait_timeout(q, Duration::from_millis(250))
                                .expect("scaler queue");
                            q = next;
                            let _ = timeout;
                        }
                    };
                    match job {
                        Job::Decode { bytes, reduce, reply } => {
                            // Nobody is waiting: the page moved on, or the
                            // picture left the document. Queued work for an
                            // abandoned image is why a flick used to keep the
                            // machine busy after it stopped.
                            if reply.is_canceled() {
                                continue;
                            }
                            let Ok(decoded) = decode_image(&bytes) else {
                                // Dropping the reply is how a failure travels:
                                // the receiver sees a cancelled channel.
                                continue;
                            };
                            let natural = (decoded.width, decoded.height);
                            let step = match reduce {
                                Reduce::Floor => downscale_step(natural, RESIDENT_FLOOR),
                                Reduce::To(s) => s.max(1),
                            };
                            let pixels =
                                if step == 1 { decoded } else { downscale(&decoded, step) };
                            let _ = reply.send(Prepared { natural, step, pixels });
                        }
                        Job::Halve { natural, pixels, factor, to, reply } => {
                            if reply.is_canceled() {
                                continue;
                            }
                            let pixels = downscale(&pixels, factor);
                            let _ = reply.send(Prepared { natural, step: to, pixels });
                        }
                    }
                }
            });
            shared
        })
    }

    fn submit(&mut self, job: Job, urgent: bool) {
        let (lock, wake) = &**self.queue();
        // A poisoned queue means the worker panicked mid-job. The waiting
        // receivers then read as cancelled, which is the path a failed decode
        // already takes, so nothing here has to be its own kind of error.
        if let Ok(mut q) = lock.lock() {
            if urgent { q.urgent.push_back(job) } else { q.background.push_back(job) }
            wake.notify_one();
        }
    }

    /// Compressed bytes to pixels. Urgent when the window is showing the
    /// picture and holding something coarser than it wants.
    fn decode(
        &mut self,
        bytes: Vec<u8>,
        reduce: Reduce,
        urgent: bool,
    ) -> oneshot::Receiver<Prepared> {
        let (reply, rx) = oneshot::channel();
        self.submit(Job::Decode { bytes, reduce, reply }, urgent);
        rx
    }

    /// Held pixels taken further down. Never urgent: it saves memory and
    /// changes nothing anyone can see.
    fn halve(
        &mut self,
        natural: (u32, u32),
        pixels: Arc<DecodedImage>,
        factor: u32,
        to: u32,
    ) -> oneshot::Receiver<Prepared> {
        let (reply, rx) = oneshot::channel();
        self.submit(Job::Halve { natural, pixels, factor, to, reply }, false);
        rx
    }
}

/// How far beyond the window culling keeps painting, as a multiple of the
/// window's height.
///
/// Above and below, so a scroll in either direction has something to show
/// before the next frame arrives. One screenful each way is generous — the
/// frame that reveals new content is produced by the same scroll that reveals
/// it, so this only has to cover the gap, not the journey.
const CULL_MARGIN_SCREENS: f32 = 1.0;

/// The window plus its margin, in document space — what "on screen" means to
/// everything that has to agree about it.
///
/// The commands and the pixels behind them are culled against the same band by
/// construction: a frame that draws a picture the client has thrown away would
/// be a hole on screen, and a picture kept for a frame that does not draw it is
/// the memory this exists to stop paying.
fn cull_band(scroll: f32, height: f32) -> Option<(f32, f32)> {
    (height > 0.0).then(|| {
        let margin = height * CULL_MARGIN_SCREENS;
        (scroll - margin, scroll + height + margin)
    })
}

/// How small a picture the window is not showing is kept at.
///
/// Not zero, which is the point. Dropping an off-screen image entirely would
/// mean a scroll arrives at an empty box and waits for a fetch; keeping a
/// coarse one means it arrives at a blurry picture that sharpens. The floor is
/// what makes releasing the rest safe.
///
/// Expressed as a box so it goes through the same power-of-two quantisation as
/// everything else: 64 px on the long edge lands a 4:3 photo at 100x75, about
/// 30 KiB. Two hundred of those is 6 MiB, against 1.4 GiB for the same two
/// hundred at the size they would be shown at.
const RESIDENT_FLOOR: (f32, f32) = (64.0, 64.0);

/// How close to its target a smooth scroll counts as stopped, in pixels. Below
/// this the easing snaps, and above it the view is travelling — which is also
/// what decides whether a picture passing through the window is worth
/// sharpening.
const SCROLL_SETTLED: f32 = 0.25;

/// How quickly the view catches up with where it is being scrolled to: the
/// time to close about 63% of the remaining distance, so roughly 100 ms to
/// close 90% of it.
///
/// A time constant rather than a fraction per frame, because the fraction made
/// the speed of a scroll depend on how fast the machine was drawing — the one
/// coupling nobody wants, since a slow machine then lags the hand *more*. This
/// is the smoothness; the stride per wheel notch is the host's (`SCROLL_SPEED`
/// in rill-vector) and is the speed.
const SCROLL_TAU: f32 = 0.045;

/// How long the window's shape must hold still before it counts as settled.
///
/// The scroll rule reads intent from distance-to-target and needs no clock; a
/// resize has no target to read, so a clock is honest: a drag refreshes this
/// continuously and the moment the hand stops it runs out. While it runs,
/// pictures are not refetched for their new size — a drag that wobbles across
/// a halving boundary otherwise re-reads and re-decodes every visible
/// photograph per crossing, and a host defers re-sending pixels the
/// compositor already holds a copy of, which is what fed the fd-exhaustion
/// crash under rapid resize.
const RESHAPE_SETTLE: Duration = Duration::from_millis(150);

/// One painted text run, as selection sees it: where it is and how to
/// measure inside it.
#[derive(Clone)]
struct SelRun {
    rect: Rect,
    text: String,
    font_size: f32,
    font_weight: u16,
    font_family: String,
}

/// Collect the frame's scroll regions, remove the interaction rects their
/// clips hide, and strip the [`DrawCommand::ScrollArea`] markers.
///
/// The trimming is what keeps scrolled-away content honest: a region's child
/// is laid out shifted, so a button scrolled out of view still *has* a rect
/// — above or below the clip, possibly under a toolbar or the rail. Its
/// paint is clipped; its hit rect would not be, and a click on whatever sits
/// there would press a button nobody can see. Anything interactive that no
/// longer intersects its region's visible rect is dropped whole; Tab and
/// clicks then agree with the eye.
fn trim_scroll_regions(commands: &mut Vec<DrawCommand>) -> Vec<(Rect, f32)> {
    let mut regions = Vec::new();
    // Region rect per clip-stack depth: entering the clip right after a
    // marker binds that clip (and everything nested in it) to the region.
    let mut clip_regions: Vec<Option<Rect>> = Vec::new();
    let mut pending: Option<Rect> = None;
    let intersects = |a: &Rect, b: &Rect| {
        a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y
    };
    let mut out = Vec::with_capacity(commands.len());
    for c in commands.drain(..) {
        match &c {
            DrawCommand::ScrollArea { rect, content } => {
                regions.push((*rect, *content));
                pending = Some(*rect);
                continue; // stripped
            }
            DrawCommand::PushClip { .. } => {
                let inherited = clip_regions.last().copied().flatten();
                clip_regions.push(pending.take().or(inherited));
            }
            DrawCommand::PopClip => {
                clip_regions.pop();
            }
            DrawCommand::LinkArea { rect, .. }
            | DrawCommand::ActionArea { rect, .. }
            | DrawCommand::InputArea { rect, .. }
            | DrawCommand::SliderArea { rect, .. }
            | DrawCommand::MenuArea { rect, .. } => {
                if let Some(Some(region)) = clip_regions.last()
                    && !intersects(rect, region)
                {
                    continue; // hidden by the region's clip: not hittable
                }
            }
            _ => {}
        }
        out.push(c);
    }
    *commands = out;
    regions
}

/// Drop paint commands that fall entirely outside the window.
///
/// A frame described the whole document rather than the part on screen, which
/// is free while a page fits and linear in the document once it does not.
/// Measured before this existed: a thousand-row file listing encoded to 660
/// KiB per frame with under 2% of it on screen, and a ten-thousand-row one
/// exceeded the frame's path-point budget outright — the window could not be
/// drawn at all. See `tests/frame_cost.rs`.
///
/// **Paint only.** Clip commands stay, or the push/pop stack stops balancing
/// and everything after a dropped pair is clipped wrongly. Interaction and
/// declaration commands stay too: they are what the host presents menus from
/// and what a page uses to claim the keyboard, they are small, and the
/// focusables this window traverses with Tab were taken from this list before
/// it was cut — but the *host* reads them from the frame, so removing an
/// off-screen field would silently change what Tab reaches.
fn cull_offscreen(commands: &mut Vec<DrawCommand>, scroll: f32, height: f32) {
    let Some((top, bottom)) = cull_band(scroll, height) else {
        return;
    };

    // A command's vertical extent, or `None` for anything that is not paint
    // and must be kept whatever it covers.
    let span = |c: &DrawCommand| -> Option<(f32, f32)> {
        let of = |r: &Rect| Some((r.y, r.y + r.h));
        match c {
            DrawCommand::Rect { rect, .. }
            | DrawCommand::Image { rect, .. }
            | DrawCommand::Text { rect, .. }
            | DrawCommand::Backdrop { rect, .. } => of(rect),
            // Shadows and glows bleed past their rect, so they are measured
            // by what they actually cover.
            DrawCommand::Shadow { rect, blur, spread, .. } => {
                Some((rect.y - blur - spread, rect.y + rect.h + blur + spread))
            }
            DrawCommand::Glow { rect, blur, .. } => {
                Some((rect.y - blur, rect.y + rect.h + blur))
            }
            DrawCommand::Border { rect, width, .. } => {
                Some((rect.y - width, rect.y + rect.h + width))
            }
            DrawCommand::Path { points, width, .. } => {
                let (mut lo, mut hi) = (f32::MAX, f32::MIN);
                for p in points {
                    lo = lo.min(p.y);
                    hi = hi.max(p.y);
                }
                (lo <= hi).then(|| (lo - width, hi + width))
            }
            DrawCommand::FillPath { points, .. } => {
                let (mut lo, mut hi) = (f32::MAX, f32::MIN);
                for p in points {
                    lo = lo.min(p.y);
                    hi = hi.max(p.y);
                }
                (lo <= hi).then_some((lo, hi))
            }
            _ => None,
        }
    };

    commands.retain(|c| match span(c) {
        Some((lo, hi)) => hi >= top && lo <= bottom,
        None => true,
    });
}

/// The power-of-two reduction that still covers `want` for an image of
/// `natural` size — 1 meaning "send it as it is".
///
/// Powers of two rather than an exact fit, for two reasons. Halving is an
/// exact 2x2 average, so the result is properly filtered rather than
/// point-sampled, and it costs about a third more than reading the source
/// once. And it quantises: dragging a window edge changes the wanted size
/// every frame, and a scheme that tracked it exactly would re-scale and
/// re-send a multi-megabyte image continuously. Crossing a halving boundary
/// happens a handful of times instead.
///
/// Never upscales: an image shown larger than it is stays as it is, because
/// inventing pixels here would only move the same blur onto the wire.
fn downscale_step(natural: (u32, u32), want: (f32, f32)) -> u32 {
    let (nw, nh) = (natural.0.max(1) as f32, natural.1.max(1) as f32);
    // The axis that needs the most detail decides.
    let need = (want.0 / nw).max(want.1 / nh);
    if !need.is_finite() || need >= 0.5 {
        return 1;
    }
    let mut step = 1u32;
    // Halve while the half still covers what is being asked for. Bounded so
    // a zero-sized rect cannot iterate away to nothing.
    while step < 64 && need <= 0.5 / step as f32 && nw / (step * 2) as f32 >= 1.0
        && nh / (step * 2) as f32 >= 1.0
    {
        step *= 2;
    }
    step
}

/// Reduce by successive exact halvings.
fn downscale(image: &DecodedImage, step: u32) -> DecodedImage {
    let mut rgba = image.rgba.clone();
    let mut done = 1u32;
    while done < step {
        let (w, h) = (rgba.width().div_ceil(2).max(1), rgba.height().div_ceil(2).max(1));
        // An exact 2:1 triangle resample is a 2x2 box average — the filter
        // this wants, and the reason the steps are powers of two.
        rgba = image::imageops::resize(&rgba, w, h, image::imageops::FilterType::Triangle);
        done *= 2;
    }
    DecodedImage { width: rgba.width(), height: rgba.height(), rgba }
}

/// Loaded images by source path — the paint-time image provider.
pub struct ReadyImages {
    images: HashMap<String, Arc<DecodedImage>>,
    /// Sources whose pixels are a stand-in: coarser than this frame draws them,
    /// with a finer copy either on its way or waiting for the scroll to stop.
    /// Something to draw now rather than a hole, but not the last word.
    provisional: HashSet<String>,
}

impl ReadyImages {
    pub fn empty() -> ReadyImages {
        ReadyImages { images: HashMap::new(), provisional: HashSet::new() }
    }

    pub fn image(&self, source: &str) -> Option<Arc<DecodedImage>> {
        self.images.get(source).cloned()
    }

    /// Whether these pixels are a stand-in for finer ones still coming.
    ///
    /// A painting host can ignore this — coarse pixels scale up to the box
    /// either way. A host that *forwards* them cannot: sending a provisional
    /// copy over a sharper one the far end already holds would visibly
    /// downgrade the picture for as long as the refinement takes.
    pub fn provisional(&self, source: &str) -> bool {
        self.provisional.contains(source)
    }

    /// Every source that has pixels, with them.
    ///
    /// A host that paints for itself calls [`ReadyImages::image`] as it walks
    /// the commands. A host that *hands its window to something else* — the
    /// vector client, whose frame is a command stream a compositor paints —
    /// needs the other direction: which images this page has, so it can send
    /// the ones it has not sent yet.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<DecodedImage>)> {
        self.images.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
}

struct KnownImages<'a>(&'a HashMap<String, ImageState>);

impl ImageSizer for KnownImages<'_> {
    fn natural_size(&mut self, source: &str) -> Option<(f32, f32)> {
        match self.0.get(source) {
            Some(ImageState::Ready { natural, .. }) => {
                Some((natural.0 as f32, natural.1 as f32))
            }
            _ => None,
        }
    }
}

/// One embeddable Rill application surface.
pub struct AppView {
    fetcher: Arc<Fetcher>,
    history: Vec<Source>,
    position: usize,
    tree: Option<UiTree>,
    /// The decoded current document, retained so a theme change can
    /// re-resolve (re-skin) it without re-fetching.
    doc: Option<rill_doc::Document>,
    /// The active theme this surface resolves tokens against.
    theme: Defaults,
    error: Option<String>,
    /// The rendered error page, kept with the text it was built from so it is
    /// rebuilt when the error changes and not once per frame.
    error_tree: Option<(String, UiTree)>,
    generation: u64,
    loading: bool,
    pending_page: Option<PendingFetch>,
    images: HashMap<String, ImageState>,
    /// Image sources the last layout put inside the window's band. What is not
    /// in here is kept at the resident floor, and a picture that lands while
    /// off screen is reduced the moment it arrives rather than at the next
    /// layout — otherwise a page whose images all land together holds every
    /// one of them at full size for a frame, which on a long roll is the whole
    /// problem happening in a single tick.
    visible_images: HashSet<String>,
    /// How many times a picture has had to be fetched again because it is
    /// being shown larger than the pixels kept for it — a zoom, a widened
    /// window, or a scroll bringing it back off the floor. The cost of not
    /// keeping originals, counted rather than assumed: it is also the number
    /// that says whether scrolling thrashes.
    image_refetches: u64,
    /// Decoding and scaling, off the frame path.
    scaler: Scaler,
    state: Vec<ActionValue>,
    /// Every interactive element in document (tab) order — the focus ring
    /// traverses this, and clicks/keys resolve against it.
    focusables: Vec<Focusable>,
    /// Declared context menus, in document order (innermost areas first, so
    /// the first hit under a point is the deepest element's menu).
    menu_areas: Vec<(Rect, Vec<MenuItem>)>,
    /// The host-presented menu currently open, if any.
    open_menu: Option<OpenMenu>,
    /// Menus may escape this surface entirely. For hosts whose window is a
    /// strip smaller than a menu — the dock — where the compositor lets the
    /// shell stream overflow its bounds and routes pointer input by painted
    /// extent. The menu then simply grows from the click, in whichever
    /// direction it was going.
    menu_unbounded: bool,
    /// The page's declared keyboard bindings, in document order (first match
    /// wins). Consulted after focus/input handling, before the viewport's own
    /// built-ins — a page meaning for a key beats the browser meaning.
    key_binds: Vec<(String, Option<String>, Option<UiAction>)>,
    /// The endpoint a capturing page wants every key sent to, if it asked.
    key_capture: Option<String>,
    /// A self-reloading page: where to re-fetch from, how often, and when it
    /// last happened.
    live: Option<(String, Duration)>,
    last_live: Option<Instant>,
    /// Documents actually applied over this view's life. Compared against
    /// the compositor's commit count it says whether a missing frame was a
    /// tick that never fired or a tick whose commit never happened — two
    /// different bugs that look identical from either side alone.
    applied_loads: u64,
    /// Hash of the currently shown page, when a remote fetch computed one.
    /// A live tick sends it as GET_IF so an unchanged page costs a hash
    /// comparison on the wire instead of a transfer (the disk cache stays
    /// out of it — this is the in-memory conditional the cache could never
    /// provide for live pages).
    live_hash: Option<[u8; 32]>,
    /// Consecutive failed live ticks. The page stays on screen and the clock
    /// keeps running, but backs off: a widget pointed at a server that is
    /// down should not open a TLS connection every second forever.
    live_failures: u32,
    /// State slots the user has touched since this page arrived, by name.
    ///
    /// A live refresh rebuilds state from the new document's declared
    /// initials, which is right for everything the server owns and wrong for
    /// the one thing it does not: a half-typed field. That value is not view
    /// state — it is a fact the user is in the middle of proposing — so it
    /// survives an in-place replacement, while every slot nobody has touched
    /// takes the server's word for it.
    dirty: HashSet<String>,
    /// Undo and redo, per state slot: `(value, caret)` snapshots taken
    /// before each edit. Bounded; cleared on navigation with the rest of
    /// the staged world. Redo clears on any fresh edit — the universal
    /// contract: undone futures die when a new one is written.
    undo: HashMap<u16, Vec<(String, usize)>>,
    redo: HashMap<u16, Vec<(String, usize)>>,
    /// Whether a live page should keep showing its end. A page that grows at
    /// the bottom — output arriving, a log being written — has to follow, or
    /// the view drifts backwards through the content as it is read. Scrolling
    /// up stops the follow; scrolling back to the end resumes it.
    stick_to_end: bool,
    /// Interactive regions in the *window's* chrome, in the rect the host
    /// lent it. Kept apart from `focusables` because they live in a different
    /// coordinate space and outside the document's tab order — a titlebar is
    /// not part of the page.
    chrome_focusables: Vec<Focusable>,
    /// Index into `chrome_focusables` of a focused chrome input. Chrome and
    /// document focus are exclusive: a titlebar field and a page input never
    /// both carry the caret.
    chrome_focus: Option<usize>,
    /// Index into `focusables` of the currently focused element.
    focus: Option<usize>,
    /// Whether focus was placed by the keyboard. The ring paints only then —
    /// a click focuses (Enter still works) but doesn't decorate, the
    /// focus-visible convention everywhere else.
    focus_visible: bool,
    /// When set, the next load is an *in-place* replacement of the page you
    /// are already on — an action response or an explicit reload — so scroll
    /// position and focus survive it. Cleared when consumed. Navigation to a
    /// different resource still lands at the top, like every browser.
    in_place_once: bool,
    /// Caret byte-offset within the focused text input's string.
    caret: usize,
    /// Selection anchor byte-offset; equal to `caret` means no selection. The
    /// selected range is `min(anchor, caret)..max(anchor, caret)`.
    anchor: usize,
    scroll: f32,
    scroll_target: f32,
    /// When the last poll ran, so scroll easing can be measured against the
    /// clock instead of against the frame rate.
    last_poll: Option<Instant>,
    /// Independent scroll regions, from the last layout: viewport rect and
    /// content height, both in zoomed document space, document order.
    scroll_regions: Vec<(Rect, f32)>,
    /// Text selection over the document's painted text: anchor and head in
    /// document space, in press order (normalised at use). `None` is no
    /// selection. Born when a press lands on nothing interactive, grown by
    /// the drag, cleared by the next press — the way text selects
    /// everywhere else.
    text_sel: Option<((f32, f32), (f32, f32))>,
    /// The last layout's text runs, kept only while a selection could use
    /// them: what the highlight is computed against and the copy extracted
    /// from.
    sel_runs: Vec<SelRun>,
    /// Their offsets, unzoomed (layout runs unzoomed), same order. The rail
    /// stands still because only the region's child shifts.
    region_offsets: Vec<f32>,
    /// The shape the last layout ran at — width, height, zoom — and when it
    /// last changed. `Some` means the window is being reshaped (a drag, a
    /// zoom) and detail work should wait; cleared by `poll` once the shape
    /// has held still for [`RESHAPE_SETTLE`], with a repaint so the layout
    /// that asks for detail actually runs.
    last_shape: Option<(f32, f32, f32)>,
    reshaping_since: Option<Instant>,
    total_height: f32,
    viewport: Rect,
    zoom: f32,
    cursor: Option<(f32, f32)>,
    pressing: bool,
    /// A press landed on the focused slider and the drag is still live; the
    /// release is what fires its action.
    slider_engaged: bool,
    capability: Option<CapabilityRequest>,
}

impl AppView {
    /// Create a surface for `source` and begin loading it.
    pub fn new(fetcher: Arc<Fetcher>, source: Source) -> AppView {
        let mut view = AppView {
            error_tree: None,
            fetcher,
            history: vec![source],
            position: 0,
            tree: None,
            doc: None,
            theme: Defaults::default(),
            error: None,
            generation: 0,
            loading: false,
            pending_page: None,
            images: HashMap::new(),
            visible_images: HashSet::new(),
            image_refetches: 0,
            scaler: Scaler::new(),
            state: Vec::new(),
            focusables: Vec::new(),
            key_binds: Vec::new(),
            key_capture: None,
            live: None,
            last_live: None,
            live_hash: None,
            live_failures: 0,
            applied_loads: 0,
            dirty: HashSet::new(),
            undo: HashMap::new(),
            redo: HashMap::new(),
            stick_to_end: true,
            menu_areas: Vec::new(),
            open_menu: None,
            menu_unbounded: false,
            chrome_focusables: Vec::new(),
            chrome_focus: None,
            focus: None,
            focus_visible: false,
            in_place_once: false,
            caret: 0,
            anchor: 0,
            scroll: 0.0,
            scroll_target: 0.0,
            last_poll: None,
            scroll_regions: Vec::new(),
            region_offsets: Vec::new(),
            text_sel: None,
            sel_runs: Vec::new(),
            last_shape: None,
            reshaping_since: None,
            total_height: 0.0,
            viewport: Rect::default(),
            zoom: 1.0,
            cursor: None,
            pressing: false,
            slider_engaged: false,
            capability: None,
        };
        view.start_load();
        view
    }

    pub fn current(&self) -> &Source {
        &self.history[self.position]
    }

    /// Swap the active theme and re-skin the current document in place. Token
    /// references re-resolve against the new theme; literal colors follow the
    /// theme only when it enforces an override (see [`Defaults::enforce`]).
    pub fn set_theme(&mut self, theme: Defaults) {
        self.theme = theme;
        // The error page is resolved against the theme too, so it has to be
        // rebuilt rather than re-skinned.
        self.error_tree = None;
        if let Some(doc) = &self.doc {
            self.tree = Some(resolve(doc, self.theme.clone()));
        }
    }

    /// The theme this view resolves documents against. A host that draws its
    /// own window chrome reads `surface` from here, so the chrome it paints
    /// and the chrome the *document* paints are the same colour by
    /// construction rather than by two people picking the same hex.
    pub fn theme(&self) -> &Defaults {
        &self.theme
    }

    pub fn title(&self) -> String {
        format!("{}{}", self.current().describe(), if self.loading { "  (loading…)" } else { "" })
    }

    pub fn is_loading(&self) -> bool {
        self.loading || self.pending_page.is_some()
    }

    fn start_load(&mut self) {
        self.start_load_cached(true, LoadKind::Nav);
    }

    /// `cached = false` for a `live` reload: the page is expected to differ
    /// on every tick, so the disk cache would only turn ticks into orphaned
    /// objects. Live ticks are conditional a different way: against the
    /// in-memory hash of the page on screen ([`AppView::live_hash`]), so the
    /// common tick — nothing changed — costs no transfer at all.
    fn start_load_cached(&mut self, cached: bool, kind: LoadKind) {
        self.generation += 1;
        self.loading = true;
        let held = if kind == LoadKind::Live { self.live_hash } else { None };
        let (tx, rx) = oneshot::channel();
        self.fetcher.spawn_fetch_page(self.current().clone(), cached, held, tx);
        self.pending_page =
            Some(PendingFetch { generation: self.generation, kind, rx, clears: Vec::new() });
    }

    /// How long to wait before the next live tick. Normally the page's own
    /// interval; after a failure, that doubled per consecutive failure and
    /// capped — a widget whose server has gone away keeps trying, but at a
    /// rate that costs a handshake every ten seconds rather than every one.
    fn live_wait(&self, interval: Duration) -> Duration {
        const CEILING: Duration = Duration::from_secs(10);
        if self.live_failures == 0 {
            return interval;
        }
        let factor = 1u32 << self.live_failures.min(6);
        interval.saturating_mul(factor).min(CEILING).max(interval)
    }

    /// True while a live page is showing content older than its clock
    /// intended — the last refresh failed and the page on screen is the last
    /// one that arrived. Hosts may say so; nothing here does.
    pub fn live_stale(&self) -> bool {
        self.live_failures > 0
    }

    /// The action the current page asked to have fired when its window
    /// closes (`closing target=`), if any. Hosts call this on the way out
    /// and fire it best-effort — the app's own timeout stays the safety net
    /// for windows that never get to say goodbye. Read straight off the
    /// document: a declaration, not a laid-out element.
    pub fn close_target(&self) -> Option<String> {
        let doc = self.doc.as_ref()?;
        doc.nodes.iter().find_map(|n| match n {
            rill_doc::Node::Closing { target } => Some(doc.string(*target).to_string()),
            _ => None,
        })
    }

    /// Advance async work. Returns true if anything changed (host should
    /// repaint) and whether work is still outstanding (come back soon, but
    /// do not repaint for it).
    pub fn poll(&mut self) -> Polled {
        let mut changed = false;
        let mut still_pending = false;

        // A self-reloading page. Only ever one fetch in flight: a slow server
        // makes the view lag, never queue — the alternative is a backlog of
        // stale screens arriving after the one that matters.
        if let Some((target, interval)) = self.live.clone()
            && self.pending_page.is_none()
            && self.last_live.is_some_and(|t| t.elapsed() >= self.live_wait(interval))
        {
            self.last_live = Some(Instant::now());
            self.in_place_once = true;
            // `{w}`/`{h}` carry the size of the area the page was laid into.
            // It is the one thing a served document cannot work out for
            // itself, and without it a page can only guess at the window it
            // is living in — a terminal guesses its grid, a chart its
            // buckets. Sent as part of the address, so it arrives on an
            // ordinary fetch rather than needing a channel of its own.
            let target = target
                .replace("{w}", &(self.viewport.w.max(0.0).round() as u32).to_string())
                .replace("{h}", &(self.viewport.h.max(0.0).round() as u32).to_string());
            let next = self.current().with_path(&target);
            self.history[self.position] = next;
            self.start_load_cached(false, LoadKind::Live);
            still_pending = true;
        }

        if let Some(pending) = &mut self.pending_page {
            match pending.rx.try_recv() {
                Ok(Some(result)) => {
                    let pending = self.pending_page.take().expect("just matched");
                    changed |= self.apply_load(pending, result);
                }
                Ok(None) => still_pending = true,
                Err(_) => self.pending_page = None,
            }
        }

        // Advance image loads. Nothing here decodes or scales: bytes that have
        // arrived become a job for the scaler thread, and jobs that have
        // finished become pixels. Both used to happen inline, which put a
        // photograph's decode and a photograph's rescale on the frame path.
        let keys: Vec<String> = self.images.keys().cloned().collect();
        for key in keys {
            // Whether the window is showing it decides what the bytes should
            // be reduced to, and it has to be read before the borrow below.
            let visible = self.visible_images.contains(&key);
            let landed = match self.images.get_mut(&key) {
                Some(ImageState::Loading(rx)) => match rx.try_recv() {
                    // Every first decode goes to the floor, whether or not the
                    // window looks like it is showing the picture, and the
                    // full-size decode never leaves the worker. The peak is
                    // then one decode, whatever the document or the window.
                    //
                    // "Looks like" is the point. Before a picture is decoded
                    // nobody knows how big it is, so layout gives it a
                    // placeholder box — and a screenful of placeholders is a
                    // dozen pictures where the real sizes turn out to be two.
                    // Trusting that guess held eleven photographs at full size
                    // on the way to holding two: 80.9 MiB peak against a 15.3
                    // MiB settled figure. Layout knows the real size a frame
                    // later and asks for detail then, by the same path a
                    // scrolled-to picture takes.
                    Ok(Some(Ok(bytes))) => {
                        Landed::Bytes { bytes, reduce: Reduce::Floor, refine_to: None }
                    }
                    Ok(Some(Err(_))) | Err(_) => Landed::Failed,
                    Ok(None) => {
                        still_pending = true;
                        Landed::Nothing
                    }
                },
                // The source fetched again because the image outgrew the
                // pixels held for it. A failure here is not fatal — the
                // coarse version is still on screen and still correct, just
                // softer than it could be — so the refinement is abandoned
                // rather than the image being lost.
                Some(ImageState::Ready { refining: Some((to, rx)), .. }) => {
                    let to = *to;
                    match rx.try_recv() {
                        Ok(Some(Ok(bytes))) => Landed::Bytes {
                            bytes,
                            reduce: Reduce::To(to),
                            refine_to: Some(to),
                        },
                        Ok(Some(Err(_))) | Err(_) => Landed::RefineFailed,
                        Ok(None) => {
                            still_pending = true;
                            Landed::Nothing
                        }
                    }
                }
                _ => Landed::Nothing,
            };
            match landed {
                Landed::Bytes { bytes, reduce, refine_to } => {
                    // A refetch is detail for a picture on screen, so it goes
                    // to the head of the queue; a first decode is one of however
                    // many the document has, and waits its turn.
                    let rx = self.scaler.decode(bytes, reduce, refine_to.is_some());
                    match refine_to {
                        Some(to) => {
                            if let Some(ImageState::Ready { refining, scaling, .. }) =
                                self.images.get_mut(&key)
                            {
                                *refining = None;
                                *scaling = Some((to, rx));
                            }
                        }
                        None => {
                            self.images.insert(key.clone(), ImageState::Preparing(rx));
                        }
                    }
                    still_pending = true;
                }
                Landed::Failed => {
                    self.images.insert(key.clone(), ImageState::Failed);
                    changed = true;
                }
                Landed::RefineFailed => {
                    if let Some(ImageState::Ready { refining, .. }) = self.images.get_mut(&key) {
                        *refining = None;
                    }
                }
                Landed::Nothing => {}
            }

            // A first decode that has finished: the picture becomes drawable.
            let prepared = match self.images.get_mut(&key) {
                Some(ImageState::Preparing(rx)) => match rx.try_recv() {
                    Ok(Some(p)) => Some(Some(p)),
                    // The worker dropped the reply, which is how it reports a
                    // picture it could not decode.
                    Err(_) => Some(None),
                    Ok(None) => {
                        still_pending = true;
                        None
                    }
                },
                _ => None,
            };
            if let Some(prepared) = prepared {
                self.images.insert(
                    key.clone(),
                    match prepared {
                        Some(p) => ImageState::Ready {
                            natural: p.natural,
                            step: p.step,
                            pixels: Arc::new(p.pixels),
                            refining: None,
                            scaling: None,
                        },
                        None => ImageState::Failed,
                    },
                );
                changed = true;
            }

            // A rescale that has finished. The picture is the same picture, so
            // this only repaints when the window is showing it — where the
            // host has to be told to send the new size onward.
            if let Some(ImageState::Ready { step, pixels, scaling: scaling @ Some(_), .. }) =
                self.images.get_mut(&key)
                && let Some((_, rx)) = scaling
            {
                match rx.try_recv() {
                    Ok(Some(p)) => {
                        *pixels = Arc::new(p.pixels);
                        *step = p.step;
                        *scaling = None;
                        changed |= visible;
                    }
                    Ok(None) => still_pending = true,
                    Err(_) => *scaling = None,
                }
            }
        }

        // The end of a reshape. While the window's shape is moving this
        // reports pending, so the host keeps polling instead of sleeping out
        // its idle timeout; when the shape has held still long enough it
        // reports one *change*, because the layout that runs for that repaint
        // is what asks for the detail the reshape deferred — without it a
        // window resized once and left alone would stay coarse forever.
        match self.reshaping_since {
            Some(t) if t.elapsed() >= RESHAPE_SETTLE => {
                self.reshaping_since = None;
                changed = true;
            }
            Some(_) => still_pending = true,
            None => {}
        }

        // Smooth scrolling: ease toward the target; keep frames coming while
        // unconverged so both hosts get identical motion. This one *is* a
        // change — the content moved — unlike the waiting above.
        //
        // Against the clock rather than against the frame. Moving a fixed
        // fraction of the remaining distance per poll made the *speed* of a
        // scroll a function of the frame rate: the same flick settled in about
        // 170 ms at 60 fps and 400 ms at 25, so the slower the machine, the
        // more the view lagged behind the hand — exactly backwards.
        self.scroll_target = self.scroll_target.clamp(0.0, self.max_scroll());
        let dt = self
            .last_poll
            .replace(Instant::now())
            .map_or(1.0 / 60.0, |t| t.elapsed().as_secs_f32())
            // A view that has been idle, or a host that stopped polling, must
            // not teleport when it comes back.
            .clamp(0.0, 0.1);
        let diff = self.scroll_target - self.scroll;
        if diff.abs() > SCROLL_SETTLED {
            self.scroll += diff * (1.0 - (-dt / SCROLL_TAU).exp());
            changed = true;
        } else {
            self.scroll = self.scroll_target;
        }

        Polled { changed, pending: still_pending }
    }

    /// Returns whether the picture changed (the host should repaint).
    fn apply_load(&mut self, pending: PendingFetch, result: Result<fetcher::PageResult, String>) -> bool {
        if pending.generation != self.generation {
            return false;
        }
        self.loading = false;
        // The page has not changed since the tick that fetched it — the
        // GET_IF against the held hash answered NOT_MODIFIED. A successful
        // tick, showing exactly what is already on screen: reset the backoff
        // and repaint nothing.
        if let Ok(fetcher::PageResult::Unchanged) = &result {
            self.live_failures = 0;
            self.in_place_once = false;
            return false;
        }
        // A live tick that failed changes nothing. The page on screen is the
        // last one that arrived, the clock that fetched it is still declared
        // in it, and the next tick may well succeed — so keep both, count the
        // failure so the retry backs off, and say nothing to the user that
        // the host has not asked to be told.
        //
        // The alternative is what this replaces: an error page, which carries
        // no `live` node, which withdrew the clock (it is re-read from every
        // layout) and left the widget frozen on an error until someone
        // reloaded it by hand. One dropped packet cost the page permanently.
        if result.is_err() && pending.kind == LoadKind::Live && self.tree.is_some() {
            self.live_failures = self.live_failures.saturating_add(1);
            self.last_live = Some(Instant::now());
            self.in_place_once = false;
            return false;
        }
        let (result, page_hash) = match result {
            Ok(fetcher::PageResult::Fresh { bytes, hash }) => (Ok(bytes), hash),
            Ok(fetcher::PageResult::Unchanged) => unreachable!("handled above"),
            Err(e) => (Err(e), None),
        };
        // An in-place replacement keeps you where you were: submitting a
        // control halfway down a long page (a studio stepper, a file row's
        // verb) must not throw you back to the top. Scroll is re-clamped
        // after layout, so a page that got shorter still lands in range.
        let in_place = self.in_place_once;
        if self.in_place_once {
            self.in_place_once = false;
        } else {
            self.scroll = 0.0;
            self.scroll_target = 0.0;
            self.region_offsets.clear();
            self.focus = None;
            // A different resource is a different world: nothing staged
            // against the old one carries into it.
            self.dirty.clear();
            self.undo.clear();
            self.redo.clear();
        }
        // Slots this response settles: the ones just submitted. Their staged
        // value has been delivered, so the server's answer is now the truth
        // about them.
        for name in &pending.clears {
            self.dirty.remove(name);
        }
        // Staged values worth carrying across an in-place replacement, by
        // name — names, not indices, because two documents' state tables need
        // not agree on order.
        let mut staged: HashMap<String, ActionValue> = HashMap::new();
        if in_place
            && !self.dirty.is_empty()
            && let Some(old) = &self.doc
        {
            for (i, var) in old.states.iter().enumerate() {
                let name = old.string(var.name_idx);
                if self.dirty.contains(name)
                    && let Some(value) = self.state.get(i)
                {
                    staged.insert(name.to_string(), value.clone());
                }
            }
        }
        let decoded = result
            .and_then(|bytes| rill_doc::decode(&bytes).map_err(|e| format!("invalid document: {e}")));
        if rill_log::dev_active() {
            match &decoded {
                Ok(_) => rill_log::dev!("viewport", "page", outcome = "ok"),
                Err(e) => rill_log::dev!("viewport", "page", outcome = "error", error = e),
            }
        }
        match decoded
        {
            Ok(doc) => {
                self.live_hash = page_hash;
                // A page written by a newer build still renders, minus the
                // properties this one does not know. Say so once per load:
                // silence here is how a desktop drifts out of sync without
                // anyone noticing which half is stale.
                for warning in &doc.warnings {
                    eprintln!("rill: {}: {warning}", self.current().describe());
                }
                self.state = doc.states.iter().map(|s| s.initial.clone()).collect();
                // Put the staged values back over the fresh initials. Same
                // name and same type only: a slot that changed type between
                // revisions is a different slot wearing an old name, and
                // guessing there would put a string where a `when` expects a
                // bool.
                if !staged.is_empty() {
                    for (i, var) in doc.states.iter().enumerate() {
                        if let Some(value) = staged.get(doc.string(var.name_idx))
                            && value.type_name() == var.initial.type_name()
                        {
                            self.state[i] = value.clone();
                        }
                    }
                }
                self.live_failures = 0;
                self.applied_loads += 1;
                self.tree = Some(resolve(&doc, self.theme.clone()));
                self.doc = Some(doc);
                self.error = None;
                // A new *place* invalidates whatever menu was open over the
                // old one. An in-place refresh does not: a live page that
                // re-serves on a clock (the terminal, every 50ms) would
                // otherwise close every context menu within a tick of it
                // opening — the menu the person is aiming at vanishing
                // under the pointer. The open menu holds its own cloned
                // items, so surviving a refresh is safe even if the page's
                // menus changed underneath; the person completes the
                // gesture they started. (Not in the per-frame collection
                // pass either way — a menu must survive the relayout that
                // paints it.)
                if !in_place {
                    self.open_menu = None;
                }
                self.request_images();
            }
            Err(e) => {
                self.live_hash = None;
                self.tree = None;
                self.doc = None;
                self.error = Some(e);
            }
        }
        true
    }

    /// Fetch what the new page shows, and forget what it doesn't.
    ///
    /// Called once per applied page. The eviction half matters as much as the
    /// fetching half: entries hold decoded RGBA, so without it every image on
    /// every page a window ever visited stays resident for the life of that
    /// window — a browsing session shows up as memory that only goes up. A
    /// live page that re-serves the same sources keeps them, because the walk
    /// finds them again; only what left the document is dropped.
    fn request_images(&mut self) {
        let Some(tree) = &self.tree else { return };
        let mut sources = Vec::new();
        collect_image_sources(&tree.root, &mut sources);

        // Dropping a `Loading` entry drops its receiver, which abandons a
        // fetch nothing is waiting for any more.
        self.images.retain(|source, _| sources.contains(source));

        for source in sources {
            if self.images.contains_key(&source) {
                continue;
            }
            let (tx, rx) = oneshot::channel();
            self.fetcher.spawn_fetch(self.current().with_path(&source), tx);
            self.images.insert(source, ImageState::Loading(rx));
        }
    }

    pub fn navigate(&mut self, target: &str) {
        rill_log::dev!("viewport", "navigate", target = target);
        // Capability launch links (shell) are handled by the host; here only
        // ordinary document navigation. History moves are addressable so a
        // document's own chrome can carry Back and Forward — the stack lives
        // here, and a link is the only verb a document has.
        match target {
            "/~back" => return self.back(),
            "/~forward" => return self.forward(),
            // Never fetch a host path: a server has no answer for /~close,
            // and NOT_FOUND is a lie about what happened.
            t if host_path(t) => return,
            _ => {}
        }
        let next = self.current().with_path(target);
        self.history.truncate(self.position + 1);
        self.history.push(next);
        self.position += 1;
        self.start_load();
    }

    /// Replace the source entirely (e.g. shell launching an app).
    pub fn open(&mut self, source: Source) {
        self.history.truncate(self.position + 1);
        self.history.push(source);
        self.position += 1;
        self.start_load();
    }

    /// Reload with a new source while keeping the current focus position — for
    /// in-place regenerations whose element structure is unchanged (e.g. the
    /// dock re-rendering itself after a palette/override toggle).
    pub fn reload_keep_focus(&mut self, source: Source) {
        self.in_place_once = true;
        self.open(source);
    }

    pub fn back(&mut self) {
        if self.position > 0 {
            self.position -= 1;
            self.start_load();
        }
    }

    pub fn forward(&mut self) {
        if self.position + 1 < self.history.len() {
            self.position += 1;
            self.start_load();
        }
    }

    /// Re-fetch the current page in place — scroll and focus survive, the
    /// way a refresh should when the page is the same page.
    pub fn reload(&mut self) {
        self.in_place_once = true;
        self.start_load();
    }

    /// How often the current page has asked to be reloaded, if it did. The
    /// host uses it to shorten its idle wait: a page with a clock needs the
    /// loop to come back before the next tick, not on the usual cadence.
    pub fn live_interval(&self) -> Option<Duration> {
        self.live.as_ref().map(|(_, d)| *d)
    }

    /// How long until this page's clock is next due, if it has one and is not
    /// already fetching.
    ///
    /// A host that sleeps on a fixed cadence cannot hit an arbitrary interval:
    /// waking every 40ms for an 80ms page fires the tick at 80 or at 120
    /// depending on where the phase happens to sit, and the fetch that resets
    /// the phase makes it drift. An 80ms page measured 10.6 ticks/s that way
    /// instead of 12.5. Sleeping exactly until the deadline removes the
    /// quantisation instead of shrinking it.
    pub fn next_tick_in(&self) -> Option<Duration> {
        let (_, interval) = self.live.as_ref()?;
        if self.pending_page.is_some() {
            return None; // a fetch is already out; the clock waits for it
        }
        let due = self.live_wait(*interval);
        Some(due.saturating_sub(self.last_live?.elapsed()))
    }

    /// Whether the page has taken the keyboard. Hosts show this: a document
    /// that receives every keystroke is a thing the user should be able to
    /// see is happening.
    pub fn captures_keys(&self) -> bool {
        self.key_capture.is_some()
    }

    /// Why the current page could not be shown, if it could not be. A live
    /// page that merely failed its last refresh is not an error — it is
    /// [`AppView::live_stale`].
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// How many documents this view has applied (see `applied_loads`).
    pub fn applied_loads(&self) -> u64 {
        self.applied_loads
    }

    /// Decoded image bytes this view is holding, in total.
    ///
    /// Not the same as what [`AppView::layout`] hands back: that is the visible
    /// set, and this includes the coarse copies kept for everything the window
    /// is not showing. The difference between the two is the whole of the
    /// residency argument, so both have to be observable or only the flattering
    /// one gets measured.
    /// Source fetches issued to recover detail the client no longer holds.
    /// The price of the residency policy, and the thing that would say
    /// scrolling had started thrashing.
    pub fn image_refetches(&self) -> u64 {
        self.image_refetches
    }

    /// Whether the window's shape — size and zoom — has held still long
    /// enough to act on. A host that forwards pixels consults this before
    /// re-sending a picture at a new size: mid-drag, the far end scales the
    /// copy it already holds into the new rect, and the megabytes wait for
    /// the hand to stop.
    pub fn shape_settled(&self) -> bool {
        self.reshaping_since.is_none()
    }

    /// The tier the current document declared for itself (`sensitive
    /// tier=N`), 0 when undeclared or when nothing is loaded. The host sends
    /// this over `set_tier` before the frame that shows the page — the
    /// client→compositor leg of specs/history.md decision 4.
    ///
    /// An error page is deliberately tier 0: it is this client's own text,
    /// not the document that failed to arrive.
    pub fn tier(&self) -> u8 {
        self.tree.as_ref().map_or(0, |t| t.tier)
    }

    /// Whether a text selection currently exists (a click without a drag is
    /// an empty one and reports false).
    pub fn has_selection(&self) -> bool {
        self.text_sel.is_some_and(|(a, h)| a != h)
    }

    pub fn clear_selection(&mut self) {
        self.text_sel = None;
    }

    /// The selection, normalised to reading order: (from, to) by line then
    /// column, plus the band's line extents where known.
    fn sel_bounds(&self) -> Option<((f32, f32), (f32, f32))> {
        let (a, h) = self.text_sel?;
        if a == h {
            return None;
        }
        // Reading order: earlier line first; same line, smaller x first.
        // "Same line" tolerates half a line of wobble so a slightly diagonal
        // drag across one line still reads as one line.
        let line_h = 8.0f32;
        let before = |p: (f32, f32), q: (f32, f32)| {
            if (p.1 - q.1).abs() > line_h { p.1 < q.1 } else { p.0 <= q.0 }
        };
        Some(if before(a, h) { (a, h) } else { (h, a) })
    }

    /// The highlighted spans, one rect per selected slice of a run.
    fn selection_spans(&self, measurer: &mut dyn TextMeasurer) -> Vec<Rect> {
        let Some((from, to)) = self.sel_bounds() else { return Vec::new() };
        let mut out = Vec::new();
        for run in &self.sel_runs {
            let Some((x0, x1)) = span_of_run(run, from, to) else { continue };
            let (c0, c1) = char_bounds(run, x0, x1, measurer);
            if c0 >= c1 {
                continue;
            }
            let w = |m: &mut dyn TextMeasurer, t: &str| {
                if t.is_empty() {
                    0.0
                } else {
                    m.measure(t, run.font_size, run.font_weight, &run.font_family, f32::MAX).width
                }
            };
            let px0 = run.rect.x + w(measurer, &run.text[..c0]);
            let px1 = run.rect.x + w(measurer, &run.text[..c1]);
            out.push(Rect { x: px0, y: run.rect.y, w: (px1 - px0).max(1.0), h: run.rect.h });
        }
        out
    }

    /// The selected text, assembled in reading order — runs on one line
    /// joined as they sit, lines joined with newlines, trailing spaces per
    /// line dropped (a terminal pads its grid with them; nobody wants them
    /// on the clipboard).
    pub fn selection_text(&self, measurer: &mut dyn TextMeasurer) -> Option<String> {
        let (from, to) = self.sel_bounds()?;
        // (line-top quantised, x, slice)
        let mut picked: Vec<(i64, f32, String)> = Vec::new();
        for run in &self.sel_runs {
            let Some((x0, x1)) = span_of_run(run, from, to) else { continue };
            let (c0, c1) = char_bounds(run, x0, x1, measurer);
            if c0 >= c1 {
                continue;
            }
            picked.push((run.rect.y.round() as i64, run.rect.x, run.text[c0..c1].to_string()));
        }
        if picked.is_empty() {
            return None;
        }
        picked.sort_by_key(|(y, x, _)| (*y, *x as i64));
        let mut lines: Vec<String> = Vec::new();
        let mut cur_y: Option<i64> = None;
        for (y, _, slice) in picked {
            if cur_y.is_none_or(|c| (y - c).abs() > 2) {
                lines.push(String::new());
                cur_y = Some(y);
            }
            lines.last_mut().expect("pushed above").push_str(&slice);
        }
        let text =
            lines.iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("
");
        (!text.trim().is_empty()).then_some(text)
    }

    pub fn image_bytes_held(&self) -> usize {
        self.images
            .values()
            .map(|s| match s {
                ImageState::Ready { pixels, .. } => pixels.rgba.len(),
                _ => 0,
            })
            .sum()
    }

    /// The current value of a named state slot. Read-only: state is the
    /// document's, and the only ways to change it are the ones the document
    /// declared.
    pub fn state_value(&self, name: &str) -> Option<ActionValue> {
        let doc = self.doc.as_ref()?;
        let index = doc.states.iter().position(|v| doc.string(v.name_idx) == name)?;
        self.state.get(index).cloned()
    }

    pub fn max_scroll(&self) -> f32 {
        (self.total_height - self.viewport.h).max(0.0)
    }

    /// Wheel input at a point: the innermost scroll region under it takes
    /// the delta; a region already at its limit in that direction — or no
    /// region at all — falls through to the page, which is how every
    /// desktop's nested scrolling behaves. The rail never moves because the
    /// rail was never inside a region.
    pub fn scroll_at(&mut self, local_x: f32, local_y: f32, delta: f32) {
        let (px, py) = (local_x, local_y + self.scroll);
        let z = self.zoom.max(0.01);
        // Innermost = last in document order that contains the point.
        let hit = self
            .scroll_regions
            .iter()
            .enumerate()
            .rev()
            .find(|(_, (r, _))| px >= r.x && px < r.x + r.w && py >= r.y && py < r.y + r.h);
        if let Some((i, (rect, content))) = hit {
            let max_off = ((content - rect.h) / z).max(0.0);
            if self.region_offsets.len() <= i {
                self.region_offsets.resize(i + 1, 0.0);
            }
            let old = self.region_offsets[i];
            // Same sign convention as scroll_by: the page target subtracts.
            let new = (old - delta / z).clamp(0.0, max_off);
            if (new - old).abs() > f32::EPSILON {
                self.region_offsets[i] = new;
                self.open_menu = None;
                return;
            }
        }
        self.scroll_by(delta);
    }

    pub fn scroll_by(&mut self, delta: f32) {
        self.open_menu = None;
        self.scroll_target = (self.scroll_target - delta).clamp(0.0, self.max_scroll());
        // Reading back through the history stops the follow; coming back to
        // the end resumes it.
        self.stick_to_end = self.scroll_target >= self.max_scroll() - 4.0;
    }

    pub fn set_zoom(&mut self, z: f32) {
        self.zoom = z.clamp(0.5, 3.0);
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
        self.scroll_target = self.scroll;
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Lay out for `bounds` (the surface rect in its host's local space) and
    /// return paintable commands + the ready-image provider + whether the
    /// cursor should be a pointer.
    pub fn layout(
        &mut self,
        bounds: Rect,
        measurer: &mut dyn TextMeasurer,
    ) -> (Vec<DrawCommand>, ReadyImages, CursorHint) {
        // Note a change of shape — resize or zoom — before anything below
        // decides what detail to ask for. The first layout is a baseline, not
        // a change: stamping it would hold every window's first sharpen
        // hostage to the settle clock.
        let shape = (bounds.w, bounds.h, self.zoom);
        if self.last_shape.is_some_and(|s| s != shape) {
            self.reshaping_since = Some(Instant::now());
        }
        self.last_shape = Some(shape);
        self.viewport = bounds;
        if self.tree.is_none() {
            // An error page is a fixed two lines, and layout runs every frame
            // — so building it here meant a full KDL parse, encode, decode and
            // resolve per frame for as long as the error was on screen. Build
            // it when the error changes instead; the key is what it displays.
            let message = self.error.clone().unwrap_or_else(|| "nothing loaded".into());
            let title = format!("Could not open {}", self.current().describe());
            let key = format!("{title}\u{0}{message}");
            if self.error_tree.as_ref().is_none_or(|(k, _)| *k != key) {
                // Both the title (a source description) and the message can
                // contain remote-influenced text, so escape both. Fall back to
                // a static doc if compilation somehow still fails — the render
                // path must never panic.
                let source = format!(
                    "column gap=8 padding=24 {{ text {} ; text {} }}",
                    rill_doc::kdl_escape(&title),
                    rill_doc::kdl_escape(&message),
                );
                let bytes = rill_doc::compile(&source).map(|c| c.bytes).unwrap_or_else(|_| {
                    rill_doc::compile("column padding=24 { text \"Could not open page\" }")
                        .expect("static fallback compiles")
                        .bytes
                });
                let doc = rill_doc::decode(&bytes).expect("compiled bytes decode");
                self.error_tree = Some((key, resolve(&doc, self.theme.clone())));
            }
        }
        let tree = match &self.tree {
            Some(tree) => tree,
            // Set immediately above whenever `tree` is None.
            None => &self.error_tree.as_ref().expect("error tree built above").1,
        };

        let z = self.zoom;
        // Region offsets, clamped against the last layout's content heights
        // — content may have shrunk since the offset was set, and an offset
        // past the end is a blank region with the real content clipped away
        // above it.
        for (i, off) in self.region_offsets.iter_mut().enumerate() {
            if let Some((rect, content)) = self.scroll_regions.get(i) {
                *off = off.clamp(0.0, ((content - rect.h) / z).max(0.0));
            }
        }
        let (mut commands, logical_h) = rill_ui::layout_document_with_scroll(
            tree,
            LayoutOptions {
                viewport_width: bounds.w / z,
                viewport_height: Some(bounds.h / z),
            },
            measurer,
            &mut KnownImages(&self.images),
            &self.state,
            self.focused_control_state(),
            self.caret,
            selection_bounds(self.anchor, self.caret, usize::MAX),
            self.cursor.map(|(x, y)| (x / z, y / z)),
            self.pressing,
            &self.region_offsets,
        );
        if z != 1.0 {
            for c in &mut commands {
                // The stream's own transform, not a second copy of it: a
                // window zoomed here and a frame zoomed on the wire must
                // agree, including about which metrics clamp.
                rill_ui::stream::scale_command(c, z);
            }
        }
        self.total_height = logical_h * z;
        // Independent scroll regions: remember them for wheel routing, drop
        // the hit rects their clips hide, and strip the markers — they are
        // for this host, never for a wire or a compositor.
        self.scroll_regions = trim_scroll_regions(&mut commands);
        if self.live.is_some() && self.stick_to_end {
            self.scroll = self.max_scroll();
            self.scroll_target = self.scroll;
        }
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());

        // Extract interaction regions (surface-local doc space, pre-scroll).
        // One ordered pass: interactive commands become focusables in document
        // order, which is the tab order.
        self.focusables = commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::LinkArea { rect, target } => {
                    Some(Focusable::Link { rect: *rect, target: target.clone() })
                }
                DrawCommand::ActionArea { rect, action } => {
                    Some(Focusable::Button { rect: *rect, action: action.clone() })
                }
                DrawCommand::InputArea {
                    rect, state, on_enter, multiline, tab_inserts,
                    font_size, font_weight, font_family, pad_x, pad_y,
                } => Some(Focusable::Input(InputArea {
                    rect: *rect,
                    state: *state,
                    on_enter: on_enter.clone(),
                    multiline: *multiline,
                    tab_inserts: *tab_inserts,
                    font_size: *font_size,
                    font_weight: *font_weight,
                    font_family: font_family.clone(),
                    pad_x: *pad_x,
                    pad_y: *pad_y,
                })),
                DrawCommand::SliderArea { rect, state, min, max, step, on_release } => {
                    Some(Focusable::Slider {
                        rect: *rect,
                        state: *state,
                        min: *min,
                        max: *max,
                        step: *step,
                        on_release: on_release.clone(),
                    })
                }
                _ => None,
            })
            .collect();
        self.key_binds = commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::KeyBind { key, target, action } => {
                    Some((key.clone(), target.clone(), action.clone()))
                }
                _ => None,
            })
            .collect();
        self.menu_areas = commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::MenuArea { rect, items } => Some((*rect, items.clone())),
                _ => None,
            })
            .collect();
        // A page holds the keyboard and its own clock only while it is the
        // page: both are re-read from every layout, so navigating away drops
        // them without anyone having to remember to.
        self.key_capture = commands.iter().find_map(|c| match c {
            DrawCommand::KeyCapture { target } => Some(target.clone()),
            _ => None,
        });
        let live = commands.iter().find_map(|c| match c {
            DrawCommand::LiveRefresh { target, interval } => {
                Some((target.clone(), Duration::from_millis(*interval as u64)))
            }
            _ => None,
        });
        if live != self.live {
            // A changed (or withdrawn) clock starts over rather than firing
            // immediately off the old one's timestamp, and a page that has
            // just started following begins at its end.
            self.last_live = live.is_some().then(Instant::now);
            self.live = live;
            self.stick_to_end = true;
        }
        // Keep the focus index valid across relayouts.
        if let Some(i) = self.focus
            && i >= self.focusables.len()
        {
            self.focus = None;
        }

        // Now that the commands exist, every image's on-screen size is known
        // — so each one can be reduced to about what it is being shown at
        // before anything sends it anywhere. Done here rather than at fetch
        // time because this is the first moment the answer exists: layout
        // needs the image's own size to decide the box, and the box is what
        // decides how much of the image is worth keeping.
        let band = cull_band(self.scroll, bounds.h);
        let mut wanted: HashMap<&str, (f32, f32)> = HashMap::new();
        for c in &commands {
            if let DrawCommand::Image { rect, source } = c {
                // Only a draw inside the band counts. The rest are about to be
                // culled out of the frame, so the size they would have needed
                // is not a size anything is showing.
                if band.is_some_and(|(top, bottom)| rect.y + rect.h < top || rect.y > bottom) {
                    continue;
                }
                // An image can appear more than once at different sizes; the
                // largest is the one that has to look right.
                let e = wanted.entry(source.as_str()).or_insert((0.0, 0.0));
                e.0 = e.0.max(rect.w);
                e.1 = e.1.max(rect.h);
            }
        }
        // Whether the view is still travelling. A picture that passes through
        // the window on the way somewhere else is not a picture anyone is
        // looking at, and sharpening it costs a source read and a decode each
        // — measured at one per picture per pass, so a flick down a long roll
        // and back paid for the whole roll twice while showing a blur.
        // Coarsening still happens while moving; only the fetch waits, and it
        // waits for the scroll to stop rather than for a clock, so it is the
        // same rule at any scroll speed.
        let travelling = (self.scroll_target - self.scroll).abs() > SCROLL_SETTLED
            || self.reshaping_since.is_some();
        // Every held picture against the size the window is showing it at — or
        // against the floor, if the window is showing it nowhere. Layout only
        // *asks*: the scaling itself happens on the scaler thread, because a
        // photograph's rescale is hundreds of milliseconds and this runs on the
        // frame path. Nothing here is urgent — a coarsening has no visible
        // effect at all, and a picture shown larger keeps painting the copy it
        // has until the finer one arrives.
        let adjust: Vec<(String, u32, bool)> = self
            .images
            .iter()
            .filter_map(|(source, state)| match state {
                ImageState::Ready { natural, step, refining, scaling, .. } => {
                    let shown = wanted.get(source.as_str()).copied();
                    let want_step =
                        downscale_step(*natural, shown.unwrap_or(RESIDENT_FLOOR));
                    // Already on its way there. Without this the same job
                    // would be queued again on every frame until it landed,
                    // which on a drag is a job per frame.
                    if scaling.as_ref().is_some_and(|(to, _)| *to == want_step) {
                        return None;
                    }
                    match want_step.cmp(step) {
                        // Held pixels already match how it is shown.
                        std::cmp::Ordering::Equal => None,
                        // Shown smaller than what is held — or not shown at
                        // all, which is the floor asking for the same thing:
                        // reduce further, which is halving what is already
                        // here.
                        std::cmp::Ordering::Greater => {
                            Some((source.clone(), want_step, false))
                        }
                        // Shown larger than the held pixels can serve. The
                        // original is gone, so this needs the source again —
                        // once, not once per frame. This is also the path a
                        // picture scrolled back into view takes out of the
                        // floor.
                        std::cmp::Ordering::Less
                            if shown.is_some() && refining.is_none() && !travelling =>
                        {
                            Some((source.clone(), want_step, true))
                        }
                        // Held finer than the floor asks for while off screen.
                        // Nothing to do: coarsening is free, but fetching a
                        // picture nobody is looking at to make it *smaller*
                        // would be work in the wrong direction.
                        std::cmp::Ordering::Less => None,
                    }
                }
                _ => None,
            })
            .collect();

        for (source, want_step, needs_source) in adjust {
            if needs_source {
                let (tx, rx) = oneshot::channel();
                self.image_refetches += 1;
                self.fetcher.spawn_fetch(self.current().with_path(&source), tx);
                if let Some(ImageState::Ready { refining, scaling, .. }) =
                    self.images.get_mut(&source)
                {
                    *refining = Some((want_step, rx));
                    // A coarsening on its way to a size the window has since
                    // outgrown. Dropping the receiver cancels it, and the
                    // worker skips jobs nobody is waiting for.
                    *scaling = None;
                }
                continue;
            }
            // Halve from where it already is, not from the original — there is
            // no original to halve from, and the arithmetic is the same picture
            // either way.
            let job = match self.images.get(&source) {
                Some(ImageState::Ready { natural, step, pixels, .. }) => {
                    Some((*natural, pixels.clone(), want_step / *step))
                }
                _ => None,
            };
            if let Some((natural, pixels, factor)) = job {
                let rx = self.scaler.halve(natural, pixels, factor, want_step);
                if let Some(ImageState::Ready { refining, scaling, .. }) =
                    self.images.get_mut(&source)
                {
                    *scaling = Some((want_step, rx));
                    // It is being shown smaller than it was, so a fetch for
                    // more detail is answering a question nobody is asking any
                    // more.
                    *refining = None;
                }
            }
        }

        // What the host may paint, and — for a host that forwards its window —
        // what it should send. Both are the visible set: an image nobody draws
        // is an image nobody sends, and the frame this returns draws exactly
        // the ones in the band.
        let mut ready = ReadyImages::empty();
        for (source, want) in &wanted {
            if let Some(ImageState::Ready { natural, step, pixels, .. }) = self.images.get(*source) {
                if downscale_step(*natural, *want) < *step {
                    ready.provisional.insert((*source).to_string());
                }
                ready.images.insert((*source).to_string(), pixels.clone());
            }
        }
        self.visible_images = wanted.keys().map(|s| s.to_string()).collect();
        // Text selection: remember the visible text runs (what a copy will
        // read), and paint the highlight over the selected spans. Culling
        // has already run, so both see exactly what the window shows — a
        // selection can only say what the screen says, which is the honest
        // contract for a copy.
        if self.text_sel.is_some() {
            self.sel_runs = commands
                .iter()
                .filter_map(|c| match c {
                    DrawCommand::Text { rect, text, font_size, font_weight, font_family, .. } => {
                        Some(SelRun {
                            rect: *rect,
                            text: text.clone(),
                            font_size: *font_size,
                            font_weight: *font_weight,
                            font_family: font_family.clone(),
                        })
                    }
                    _ => None,
                })
                .collect();
            let accent = self.theme.link_color;
            for r in self.selection_spans(measurer) {
                commands.push(DrawCommand::Rect {
                    rect: r,
                    color: Color { a: 0x46, ..accent },
                    corner_radius: 0.0,
                });
            }
        } else {
            self.sel_runs.clear();
        }

        // Keyboard focus ring around a focused button/link (inputs draw their
        // own border + caret).
        if self.focus_visible
            && let Some(Focusable::Button { rect, .. } | Focusable::Link { rect, .. }) =
                self.focus.and_then(|i| self.focusables.get(i))
        {
            push_focus_ring(&mut commands, rect, self.theme.link_color);
        }

        // Culling happens *before* the menu paints, deliberately. The cull
        // bounds the document to the window; an open menu is an overlay that
        // may legitimately escape it — the dock's strip is 40px tall and its
        // app menu is not — and culling it cost every item past the band its
        // label while the panel behind them (one tall rect crossing the
        // band) survived. Menus are already bounded by their own size, so
        // they need no culling to stay cheap.
        cull_offscreen(&mut commands, self.scroll, bounds.h);

        // The open context menu paints above everything — the z-axis is
        // command order. Geometry is (re)computed here because this is where
        // the measurer lives; hit-testing reads the stored rects.
        let cursor = self.cursor;
        let theme = self.theme.clone();
        let unbounded = self.menu_unbounded;
        if let Some(menu) = self.open_menu.as_mut() {
            menu.layout(&theme, self.viewport, self.scroll, cursor, unbounded, measurer);
            menu.paint(&theme, &mut commands);
        }

        let hint = self
            .cursor
            .map(|(x, y)| self.cursor_hint(x, y))
            .unwrap_or(CursorHint::Default);
        (commands, ready, hint)
    }

    /// Scroll offset to apply when painting (subtract from y).
    pub fn scroll_offset(&self) -> f32 {
        self.scroll
    }

    /// Update cursor (host coords → surface-local doc space handled by caller
    /// passing already-local coords). Returns true if the hovered region key
    /// changed (host may repaint for hover feedback).
    pub fn set_cursor(&mut self, local_x: f32, local_y: f32) {
        self.cursor = Some((local_x, local_y + self.scroll));
        // Dragging with the button down grows the selection's head.
        if self.pressing
            && let Some((_, head)) = &mut self.text_sel
        {
            *head = (local_x, local_y + self.scroll);
        }
    }

    pub fn clear_cursor(&mut self) {
        self.cursor = None;
    }

    pub fn set_pressing(&mut self, pressing: bool) {
        self.pressing = pressing;
        // Releasing a slider drag is what commits it: the value has been
        // live in its slot all along, the action tells the far end.
        if !pressing && self.slider_engaged {
            self.slider_engaged = false;
            if let Some(Focusable::Slider { on_release: Some(action), .. }) =
                self.focus.and_then(|i| self.focusables.get(i)).cloned()
            {
                self.perform_action(&action);
            }
        }
    }

    fn doc_point(&self, local_x: f32, local_y: f32) -> (f32, f32) {
        (local_x, local_y + self.scroll)
    }

    /// The cursor the host should show for a surface-local point (uses the
    /// most recent layout's hit regions — no relayout).
    pub fn hint_at_local(&self, local_x: f32, local_y: f32) -> CursorHint {
        let (x, y) = self.doc_point(local_x, local_y);
        self.cursor_hint(x, y)
    }

    /// The focused text input's captured metrics, if the focus is an input.
    /// The selected text inside the focused input, if any — the host's
    /// Ctrl+C/X ask this before deciding whether the combo is theirs (an
    /// input's copy) or the page's (a terminal's interrupt).
    pub fn focused_input_selection(&self) -> Option<String> {
        let area = self.focused_input()?;
        let Some(ActionValue::Str(s)) = self.state.get(area.state as usize) else {
            return None;
        };
        let (lo, hi) = selection_bounds(self.anchor, self.caret, s.len());
        (lo < hi).then(|| s[lo..hi].to_string())
    }

    /// Whether an input holds focus — the host's Ctrl+V gate.
    pub fn has_focused_input(&self) -> bool {
        self.focused_input().is_some()
    }

    fn focused_input(&self) -> Option<&InputArea> {
        // Chrome first: focus there is exclusive with document focus, and
        // every editing path — keys, paste, Enter — flows through this one
        // accessor, which is what makes a titlebar field behave like any
        // other input without its own key handling.
        if let Some(Focusable::Input(a)) =
            self.chrome_focus.and_then(|i| self.chrome_focusables.get(i))
        {
            return Some(a);
        }
        match self.focus.and_then(|i| self.focusables.get(i)) {
            Some(Focusable::Input(a)) => Some(a),
            _ => None,
        }
    }

    /// The state slot of the focused text input, if any (drives text editing
    /// and the caret/selection render).
    fn focused_input_state(&self) -> Option<u16> {
        self.focused_input().map(|a| a.state)
    }

    /// The state slot of whatever focused control decorates by slot — a text
    /// input or a slider. Only the layout's focus ring reads this; the text
    /// editing paths stay on [`Self::focused_input_state`].
    fn focused_control_state(&self) -> Option<u16> {
        self.focused_input_state().or_else(|| {
            match self.focus.and_then(|i| self.focusables.get(i)) {
                Some(Focusable::Slider { state, .. }) => Some(*state),
                _ => None,
            }
        })
    }

    /// The value `x` names on a slider track, clamped and quantized.
    fn slider_value_at(rect: &Rect, min: f32, max: f32, step: f32, x: f32) -> f64 {
        let fraction = ((x - rect.x) / rect.w.max(1.0)).clamp(0.0, 1.0);
        let mut value = min + fraction * (max - min);
        if step > 0.0 {
            value = min + ((value - min) / step).round() * step;
        }
        value.clamp(min, max) as f64
    }

    /// Write a dragged slider value into its slot (no action — that fires on
    /// release).
    fn set_slider_value(&mut self, slot: u16, value: f64) {
        if let Some(entry) = self.state.get_mut(slot as usize)
            && *entry != ActionValue::Num(value)
        {
            *entry = ActionValue::Num(value);
            self.mark_dirty(slot);
        }
    }

    /// Put the caret at the end of the focused input, clearing any selection —
    /// used when focus lands on an input via Tab or a fresh click.
    fn caret_to_end(&mut self) {
        self.caret = self
            .focused_input_state()
            .and_then(|s| match self.state.get(s as usize) {
                Some(ActionValue::Str(t)) => Some(t.len()),
                _ => None,
            })
            .unwrap_or(0);
        self.anchor = self.caret;
    }

    /// Move keyboard focus by `delta` (±1) through the focusables, wrapping.
    fn move_focus(&mut self, delta: isize) {
        self.focus_visible = true;
        let n = self.focusables.len() as isize;
        if n == 0 {
            return;
        }
        let cur = self.focus.map(|i| i as isize).unwrap_or(if delta > 0 { -1 } else { 0 });
        self.focus = Some(((cur + delta) % n + n) as usize % n as usize);
        self.caret_to_end();
    }

    fn cursor_hint(&self, x: f32, y: f32) -> CursorHint {
        let hit = |r: &Rect| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
        match self.focusables.iter().find(|f| hit(f.rect())) {
            Some(Focusable::Input(_)) => CursorHint::Text,
            Some(_) => CursorHint::Pointer,
            None => CursorHint::Default,
        }
    }

    /// Whether the document asked the window for chrome of its own.
    pub fn has_chrome(&self) -> bool {
        self.tree.as_ref().is_some_and(|t| t.chrome.is_some())
    }

    /// Lay the document's chrome into a rect the *window* owns — a titlebar
    /// strip — and return what to paint there. Coordinates are the host's, and
    /// zoom does not apply: chrome belongs to the window, so it stays put when
    /// the page is zoomed.
    pub fn layout_chrome(
        &mut self,
        rect: Rect,
        cursor: Option<(f32, f32)>,
        measurer: &mut dyn TextMeasurer,
    ) -> Vec<DrawCommand> {
        let Some(tree) = &self.tree else {
            self.chrome_focusables.clear();
            return Vec::new();
        };
        let chrome_focused = self
            .chrome_focus
            .and_then(|i| self.chrome_focusables.get(i))
            .and_then(|f| match f {
                Focusable::Input(a) => Some(a.state),
                _ => None,
            });
        let commands = rill_ui::layout_chrome(
            tree,
            rect,
            measurer,
            &mut KnownImages(&self.images),
            &self.state,
            chrome_focused,
            self.caret,
            selection_bounds(self.anchor, self.caret, usize::MAX),
            cursor,
            self.pressing,
        );
        self.chrome_focusables = commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::LinkArea { rect, target } => {
                    Some(Focusable::Link { rect: *rect, target: target.clone() })
                }
                DrawCommand::ActionArea { rect, action } => {
                    Some(Focusable::Button { rect: *rect, action: action.clone() })
                }
                DrawCommand::InputArea {
                    rect, state, on_enter, multiline, tab_inserts,
                    font_size, font_weight, font_family, pad_x, pad_y,
                } => Some(Focusable::Input(InputArea {
                    rect: *rect,
                    state: *state,
                    on_enter: on_enter.clone(),
                    multiline: *multiline,
                    tab_inserts: *tab_inserts,
                    font_size: *font_size,
                    font_weight: *font_weight,
                    font_family: font_family.clone(),
                    pad_x: *pad_x,
                    pad_y: *pad_y,
                })),
                _ => None,
            })
            .collect();
        if let Some(i) = self.chrome_focus
            && i >= self.chrome_focusables.len()
        {
            self.chrome_focus = None;
        }
        commands
    }

    /// Handle a click in the chrome rect. Returns `Miss` when the point is
    /// bare chrome, which is the host's cue to drag the window instead.
    pub fn chrome_click(
        &mut self,
        x: f32,
        y: f32,
        measurer: &mut dyn TextMeasurer,
    ) -> ClickResult {
        let hit = |r: &Rect| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
        let Some(idx) = self.chrome_focusables.iter().position(|f| hit(f.rect())) else {
            self.chrome_focus = None;
            return ClickResult::Miss;
        };
        match self.chrome_focusables[idx].clone() {
            Focusable::Button { action, .. } => {
                self.chrome_focus = None;
                if let Some(target) = action_host_link(&action) {
                    return ClickResult::Link(target);
                }
                self.perform_action(&action);
                ClickResult::Consumed
            }
            Focusable::Link { target, .. } => {
                self.chrome_focus = None;
                ClickResult::Link(target)
            }
            Focusable::Input(area) => {
                self.chrome_focus = Some(idx);
                self.focus = None;
                let text = match self.state.get(area.state as usize) {
                    Some(ActionValue::Str(s)) => s.as_str(),
                    _ => "",
                };
                self.caret = caret_at_click(&area, text, x, y, measurer);
                self.anchor = self.caret;
                ClickResult::Consumed
            }
            // A titlebar has no room for a value drag; chrome sliders are
            // not a thing until a document asks for one.
            Focusable::Slider { .. } => ClickResult::Miss,
        }
    }

    /// The cursor shape a point in the chrome rect wants — text over a
    /// field, pointer over anything else clickable.
    pub fn chrome_hint(&self, x: f32, y: f32) -> CursorHint {
        let hit = |r: &Rect| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
        match self.chrome_focusables.iter().find(|f| hit(f.rect())) {
            Some(Focusable::Input(_)) => CursorHint::Text,
            Some(_) => CursorHint::Pointer,
            None => CursorHint::Default,
        }
    }

    /// A context invocation (right-click / long-press) at surface-local
    /// coordinates: opens the innermost declared menu under the point.
    /// Returns true when a menu opened (host should repaint).
    pub fn context_click(&mut self, local_x: f32, local_y: f32) -> bool {
        let (x, y) = self.doc_point(local_x, local_y);
        self.open_menu_at(x, y)
    }

    /// Open the innermost menu containing a document-space point (also the
    /// resolution of a `menu` action from a visible control like the ⋯).
    fn open_menu_at(&mut self, x: f32, y: f32) -> bool {
        let hit = |r: &Rect| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
        match self.menu_areas.iter().find(|(r, _)| hit(r)) {
            Some((_, items)) => {
                self.open_menu = Some(OpenMenu::new(items.clone(), (x, y)));
                true
            }
            None => {
                self.open_menu = None;
                false
            }
        }
    }

    /// True while a context menu is open (the host repaints on pointer moves
    /// so hover tracks).
    pub fn menu_open(&self) -> bool {
        self.open_menu.is_some()
    }

    /// Allow menus to escape this surface (see `menu_unbounded`).
    pub fn set_menu_unbounded(&mut self, on: bool) {
        self.menu_unbounded = on;
    }

    /// Handle a click at surface-local coordinates. Inputs and buttons are
    /// consumed internally; a link click is returned so the host can decide
    /// (navigate in place, or intercept `/~launch/` to open an app).
    pub fn on_click(
        &mut self,
        local_x: f32,
        local_y: f32,
        measurer: &mut dyn TextMeasurer,
    ) -> ClickResult {
        if self.capability.is_some() {
            return ClickResult::Miss;
        }
        let (x, y) = self.doc_point(local_x, local_y);
        // An open menu owns the click: an item activates, anywhere else
        // dismisses — and the dismissing click never falls through.
        if let Some(menu) = self.open_menu.take() {
            if let Some(i) = menu.hit(x, y) {
                let item = menu.items[i].clone();
                if let Some(target) = item.target {
                    return ClickResult::Link(target);
                }
                if let Some(action) = item.action {
                    if let Some(target) = action_host_link(&action) {
                        return ClickResult::Link(target);
                    }
                    self.perform_action(&action);
                }
            } else if menu.inside(x, y) {
                // A separator or padding: keep the menu open.
                self.open_menu = Some(menu);
            }
            return ClickResult::Consumed;
        }
        let hit = |r: &Rect| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
        let Some(idx) = self.focusables.iter().position(|f| hit(f.rect())) else {
            self.focus = None;
            self.chrome_focus = None;
            // Nothing interactive here: the press anchors a text selection,
            // which the drag will grow (see set_cursor) and the next press
            // will replace. A click that never drags selects nothing.
            self.text_sel = Some(((x, y), (x, y)));
            return ClickResult::Miss;
        };
        // A press on a control replaces any selection, like everywhere else.
        self.text_sel = None;
        self.focus = Some(idx);
        self.focus_visible = false;
        self.chrome_focus = None;
        match self.focusables[idx].clone() {
            Focusable::Input(area) => {
                // Caret at the clicked position, fresh (empty) selection there.
                let text = match self.state.get(area.state as usize) {
                    Some(ActionValue::Str(s)) => s.as_str(),
                    _ => "",
                };
                self.caret = caret_at_click(&area, text, x, y, measurer);
                self.anchor = self.caret;
                ClickResult::Consumed
            }
            Focusable::Button { action, .. } => {
                if action == UiAction::OpenMenu {
                    self.open_menu_at(x, y);
                } else if let Some(target) = action_host_link(&action) {
                    return ClickResult::Link(target);
                } else {
                    self.perform_action(&action);
                }
                ClickResult::Consumed
            }
            Focusable::Link { target, .. } => ClickResult::Link(target),
            Focusable::Slider { rect, state, min, max, step, .. } => {
                let value = Self::slider_value_at(&rect, min, max, step, x);
                self.set_slider_value(state, value);
                self.slider_engaged = true;
                ClickResult::Consumed
            }
        }
    }

    /// Extend the selection to a drag position (mouse held) within the focused
    /// input: the caret follows the pointer while the anchor stays put.
    pub fn on_drag(&mut self, local_x: f32, local_y: f32, measurer: &mut dyn TextMeasurer) {
        // A live slider drag follows the pointer's x, however far it strays
        // from the track.
        if self.slider_engaged
            && let Some(Focusable::Slider { rect, state, min, max, step, .. }) =
                self.focus.and_then(|i| self.focusables.get(i)).cloned()
        {
            let (x, _) = self.doc_point(local_x, local_y);
            let value = Self::slider_value_at(&rect, min, max, step, x);
            self.set_slider_value(state, value);
            return;
        }
        let Some(area) = self.focused_input().cloned() else { return };
        let (x, y) = self.doc_point(local_x, local_y);
        let text = match self.state.get(area.state as usize) {
            Some(ActionValue::Str(s)) => s.as_str(),
            _ => "",
        };
        self.caret = caret_at_click(&area, text, x, y, measurer);
    }

    /// The currently selected text, if any (for copy/cut).
    pub fn selected_text(&self) -> Option<String> {
        let slot = self.focused_input_state()?;
        if let Some(ActionValue::Str(s)) = self.state.get(slot as usize) {
            let (lo, hi) = selection_bounds(self.anchor, self.caret, s.len());
            if lo != hi {
                return Some(s[lo..hi].to_string());
            }
        }
        None
    }

    /// Replace the selection (or insert at the caret) with `text` — used for
    /// paste. Returns true if a focused input consumed it.
    pub fn insert_text(&mut self, text: &str) -> bool {
        let Some(slot) = self.focused_input_state() else { return false };
        if let Some(ActionValue::Str(s)) = self.state.get_mut(slot as usize) {
            let c = replace_selection(s, self.anchor, self.caret, text, rill_protocol::MAX_FIELD_STRING);
            self.caret = c;
            self.anchor = c;
            self.mark_dirty(slot);
            return true;
        }
        false
    }

    /// Copy the selection, then delete it (for cut). Returns the cut text.
    pub fn cut_selection(&mut self) -> Option<String> {
        let text = self.selected_text()?;
        self.insert_text("");
        Some(text)
    }

    /// Handle a typed key. `key` is the keystroke's key name; `text` is the
    /// character to insert (if any); `shift` extends the selection during
    /// movement. Returns true if handled.
    pub fn on_key(
        &mut self,
        key: &str,
        text: Option<&str>,
        ctrl: bool,
        shift: bool,
        alt: bool,
    ) -> KeyResult {
        // Key names with modifiers, and only for combos — the trail is a
        // repro script, not a keylogger. Plain typing never leaves the
        // process, not even as names.
        if (ctrl || alt) && rill_log::dev_active() {
            let mods = format!(
                "{}{}{}",
                if ctrl { "ctrl+" } else { "" },
                if shift { "shift+" } else { "" },
                if alt { "alt+" } else { "" }
            );
            rill_log::dev!("viewport", "key", combo = format!("{mods}{key}"));
        }
        // An open menu owns the keyboard: arrows walk it, Enter activates,
        // Escape closes, everything else is swallowed while it's up.
        if let Some(menu) = self.open_menu.as_mut() {
            match key {
                "escape" => {
                    self.open_menu = None;
                }
                "down" => menu.step(1),
                "up" => menu.step(-1),
                "enter" | "space" => {
                    if let Some(i) = menu.hover {
                        let item = menu.items[i].clone();
                        self.open_menu = None;
                        if let Some(target) = item.target {
                            return KeyResult::Link(target);
                        }
                        if let Some(action) = item.action {
                            if let Some(target) = action_host_link(&action) {
                                return KeyResult::Link(target);
                            }
                            self.perform_action(&action);
                        }
                    }
                }
                _ => {}
            }
            return KeyResult::Handled;
        }
        // A capturing page takes everything the host does not reserve. The
        // reserved set is `ctrl+shift+<key>` — already the canonical binding
        // namespace — so a full-screen document can never take the keyboard
        // hostage: the way out is the same combination it always was.
        if let Some(endpoint) = self.key_capture.clone()
            && !(ctrl && shift)
        {
            let mut fields = vec![
                ("key".to_string(), ActionValue::Str(key.to_string())),
                ("ctrl".to_string(), ActionValue::Bool(ctrl)),
                ("shift".to_string(), ActionValue::Bool(shift)),
                ("alt".to_string(), ActionValue::Bool(alt)),
            ];
            if let Some(t) = text {
                fields.push(("text".to_string(), ActionValue::Str(t.to_string())));
            }
            self.submit(endpoint, fields);
            return KeyResult::Handled;
        }
        // Tab moves focus through every interactive element (Shift+Tab
        // back) — except in a code surface, where Tab is a character: four
        // spaces to the next stop, the editor contract. Shift+Tab still
        // walks focus, so the keyboard is never trapped.
        if key == "tab" {
            if !shift
                && let Some(area) = self.focused_input().filter(|a| a.tab_inserts)
            {
                let slot = area.state;
                self.mark_dirty(slot);
                if let Some(ActionValue::Str(before)) = self.state.get(slot as usize) {
                    let stack = self.undo.entry(slot).or_default();
                    stack.push((before.clone(), self.caret));
                    if stack.len() > 200 {
                        stack.remove(0);
                    }
                    self.redo.remove(&slot);
                }
                if let Some(ActionValue::Str(s)) = self.state.get_mut(slot as usize) {
                    let caret = self.caret.min(s.len());
                    let col = s[..caret].rfind('\n').map(|i| caret - i - 1).unwrap_or(caret);
                    let pad = 4 - (col % 4);
                    let spaces = " ".repeat(pad);
                    self.caret =
                        replace_selection(s, self.anchor, caret, &spaces, usize::MAX);
                    self.anchor = self.caret;
                }
                return KeyResult::Handled;
            }
            self.move_focus(if shift { -1 } else { 1 });
            return KeyResult::Handled;
        }
        // A focused button/link activates on Enter or Space. (Inputs handle
        // Enter themselves below.) A link is returned so the host follows it,
        // exactly like a click.
        match self.focus.and_then(|i| self.focusables.get(i)).cloned() {
            Some(Focusable::Button { rect, action }) if key == "enter" || key == "space" => {
                if let Some(target) = action_host_link(&action) {
                    return KeyResult::Link(target);
                }
                if action == UiAction::OpenMenu {
                    // Anchor at the control's bottom-left corner, like a
                    // pointer would.
                    self.open_menu_at(rect.x + 1.0, rect.y + rect.h - 1.0);
                } else {
                    self.perform_action(&action);
                }
                return KeyResult::Handled;
            }
            Some(Focusable::Link { target, .. }) if key == "enter" || key == "space" => {
                return KeyResult::Link(target);
            }
            // Arrows nudge a focused slider by its step (a hundredth of the
            // range when it is continuous) and commit immediately — a key
            // has no release worth waiting for.
            Some(Focusable::Slider { state, min, max, step, on_release, .. })
                if key == "left" || key == "right" =>
            {
                let nudge = if step > 0.0 { step } else { (max - min) / 100.0 };
                let delta = if key == "left" { -nudge } else { nudge };
                let current = match self.state.get(state as usize) {
                    Some(ActionValue::Num(n)) => *n as f32,
                    _ => min,
                };
                self.set_slider_value(state, (current + delta).clamp(min, max) as f64);
                if let Some(action) = on_release {
                    self.perform_action(&action);
                }
                return KeyResult::Handled;
            }
            _ => {}
        }
        if let Some(slot) = self.focused_input_state() {
            const CAP: usize = rill_protocol::MAX_FIELD_STRING;
            // Snapshot the field's traits (owned) so no borrow is held while we
            // mutate state/focus below.
            let (multiline, on_enter) = self
                .focused_input()
                .map(|a| (a.multiline, a.on_enter.clone()))
                .unwrap_or((false, None));
            // Non-editing keys first (they touch focus/actions, not the string).
            match key {
                "escape" => {
                    self.focus = None;
                    return KeyResult::Handled;
                }
                "enter" if !multiline => {
                    self.focus = None;
                    if let Some(a) = on_enter {
                        self.perform_action(&a);
                    }
                    return KeyResult::Handled;
                }
                _ => {}
            }
            // Whether this key edits the string, as opposed to moving the
            // caret or changing the selection. Decided from the key rather
            // than by diffing the value afterwards: the editing block below
            // borrows the slot mutably and returns from several places, and
            // "the user pressed a key that types" is the property being
            // recorded anyway — pressing Backspace in an empty field is still
            // the user editing that field.
            let edits = matches!(key, "backspace" | "delete")
                || (key == "enter" && multiline)
                || text.is_some_and(|t| !t.is_empty() && !ctrl);
            if edits {
                self.mark_dirty(slot);
                // A snapshot before the change is what undo restores.
                // Bounded, and any fresh edit kills the undone futures.
                if let Some(ActionValue::Str(before)) = self.state.get(slot as usize) {
                    let stack = self.undo.entry(slot).or_default();
                    stack.push((before.clone(), self.caret));
                    if stack.len() > 200 {
                        stack.remove(0);
                    }
                    self.redo.remove(&slot);
                }
            }
            // Undo and redo, before the editing block borrows the slot.
            if ctrl && (key == "z" || key == "y") {
                let (from, to) = if key == "z" {
                    (&mut self.undo, &mut self.redo)
                } else {
                    (&mut self.redo, &mut self.undo)
                };
                if let Some((value, caret)) = from.get_mut(&slot).and_then(Vec::pop) {
                    if let Some(ActionValue::Str(now)) = self.state.get(slot as usize) {
                        to.entry(slot).or_default().push((now.clone(), self.caret));
                    }
                    self.state[slot as usize] = ActionValue::Str(value.clone());
                    self.caret = caret.min(value.len());
                    self.anchor = self.caret;
                    self.mark_dirty(slot);
                }
                return KeyResult::Handled;
            }
            // A ctrl-combo the input does not itself consume belongs to the
            // page: Ctrl+S must reach a save binding even while the
            // keyboard lives in an input — swallowing it made every editor
            // shortcut a dead key. Ctrl means the text is not insertable
            // anyway (the insert path filters ctrl), so consulting the
            // page's bindings costs nothing that typing wanted.
            if ctrl && !matches!(key, "a" | "z" | "y") {
                let combo = format!("ctrl+{key}");
                if let Some((_, target, action)) =
                    self.key_binds.iter().find(|(k, ..)| *k == combo).cloned()
                {
                    if let Some(t) = target {
                        return KeyResult::Link(t);
                    }
                    if let Some(a) = action {
                        self.perform_action(&a);
                        return KeyResult::Handled;
                    }
                }
            }
            // Editing / caret movement / selection on the bound string.
            let caret = &mut self.caret;
            let anchor = &mut self.anchor;
            if let Some(ActionValue::Str(s)) = self.state.get_mut(slot as usize) {
                *caret = (*caret).min(s.len());
                *anchor = (*anchor).min(s.len());
                let has_sel = *anchor != *caret;
                let (lo, hi) = selection_bounds(*anchor, *caret, s.len());
                // Movement keys yield a target caret; Left/Right with a
                // selection and no Shift collapse to the selection edge.
                let move_target = match key {
                    "left" if has_sel && !shift => Some(lo),
                    "right" if has_sel && !shift => Some(hi),
                    "left" => Some(move_left(s, *caret)),
                    "right" => Some(move_right(s, *caret)),
                    "home" => Some(line_start(s, *caret)),
                    "end" => Some(line_end(s, *caret)),
                    "up" if multiline => Some(line_up(s, *caret)),
                    "down" if multiline => Some(line_down(s, *caret)),
                    _ => None,
                };
                if let Some(c) = move_target {
                    *caret = c;
                    if !shift {
                        *anchor = c; // collapse the selection
                    }
                    return KeyResult::Handled;
                }
                match key {
                    "a" if ctrl => {
                        *anchor = 0;
                        *caret = s.len();
                    }
                    "backspace" => {
                        if has_sel {
                            *caret = replace_selection(s, *anchor, *caret, "", CAP);
                        } else {
                            backspace_at(s, caret);
                        }
                        *anchor = *caret;
                    }
                    "delete" => {
                        if has_sel {
                            *caret = replace_selection(s, *anchor, *caret, "", CAP);
                        } else {
                            delete_at(s, caret);
                        }
                        *anchor = *caret;
                    }
                    // Insert (a char, or a newline in multiline) replaces any
                    // selection.
                    "enter" => {
                        *caret = replace_selection(s, *anchor, *caret, "\n", CAP);
                        *anchor = *caret;
                    }
                    _ => match text.filter(|c| !c.is_empty() && !ctrl) {
                        Some(ch) => {
                            *caret = replace_selection(s, *anchor, *caret, ch, CAP);
                            *anchor = *caret;
                        }
                        None => return KeyResult::Ignored,
                    },
                }
                return KeyResult::Handled;
            }
            return KeyResult::Ignored;
        }
        // Page-declared bindings, now that no input is focused and no focused
        // element claimed the key. Checked before the viewport's built-ins so
        // a page meaning (↓ moves the selection) beats the browser meaning —
        // and a page that binds nothing keeps history on Backspace/←/→.
        let combo = match (ctrl, shift) {
            (true, true) => format!("ctrl+shift+{key}"),
            (true, false) => format!("ctrl+{key}"),
            (false, true) => format!("shift+{key}"),
            (false, false) => key.to_string(),
        };
        if let Some((_, target, action)) =
            self.key_binds.iter().find(|(k, ..)| *k == combo).cloned()
        {
            if let Some(t) = target {
                return KeyResult::Link(t);
            }
            if let Some(a) = action {
                self.perform_action(&a);
                return KeyResult::Handled;
            }
        }
        match key {
            "backspace" | "left" => self.back(),
            "right" => self.forward(),
            "f5" => self.reload(),
            "=" | "+" if ctrl => self.set_zoom(self.zoom + 0.1),
            "-" if ctrl => self.set_zoom(self.zoom - 0.1),
            "0" if ctrl => self.set_zoom(1.0),
            _ => return KeyResult::Ignored,
        }
        KeyResult::Handled
    }

    /// Record that the user has touched a state slot, so its value survives
    /// the next in-place refresh. Named from the current document — a slot
    /// with no name to record is one no future document could match anyway.
    fn mark_dirty(&mut self, slot: u16) {
        if let Some(doc) = &self.doc
            && let Some(var) = doc.states.get(slot as usize)
        {
            let name = doc.string(var.name_idx).to_string();
            self.dirty.insert(name);
        }
    }

    /// The names of the state slots behind a submit's fields.
    fn slot_names(&self, fields: &[(String, u16)]) -> Vec<String> {
        let Some(doc) = &self.doc else { return Vec::new() };
        fields
            .iter()
            .filter_map(|(_, slot)| {
                doc.states.get(*slot as usize).map(|v| doc.string(v.name_idx).to_string())
            })
            .collect()
    }

    fn perform_action(&mut self, action: &UiAction) {
        match action {
            // Positionless here; the click/key paths that know the anchor
            // handle it before reaching this.
            UiAction::OpenMenu => {}
            UiAction::Navigate(target) => self.navigate(&target.clone()),
            UiAction::Toggle(slot) => {
                if let Some(ActionValue::Bool(b)) = self.state.get_mut(*slot as usize) {
                    *b = !*b;
                    self.mark_dirty(*slot);
                }
            }
            UiAction::Set(slot, value) => {
                if let Some(entry) = self.state.get_mut(*slot as usize) {
                    *entry = value.clone();
                    self.mark_dirty(*slot);
                }
            }
            UiAction::Submit { endpoint, fields } => {
                let payload: Vec<(String, ActionValue)> = fields
                    .iter()
                    .filter_map(|(name, slot)| {
                        self.state.get(*slot as usize).map(|v| (name.clone(), v.clone()))
                    })
                    .collect();
                let clears = self.slot_names(fields);
                self.submit_clearing(endpoint.clone(), payload, clears);
            }
            UiAction::PickFile { into } => self.request_pick_file(*into),
        }
    }

    fn submit(&mut self, endpoint: String, fields: Vec<(String, ActionValue)>) {
        self.submit_clearing(endpoint, fields, Vec::new());
    }

    /// Submit, naming the state slots this action settles (see
    /// [`PendingFetch::clears`]). A keystroke capture settles nothing — its
    /// fields are the keystroke, not slots.
    fn submit_clearing(
        &mut self,
        endpoint: String,
        fields: Vec<(String, ActionValue)>,
        clears: Vec<String>,
    ) {
        // Field *names* only: the trail must never carry typed text.
        if rill_log::dev_active() {
            let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
            rill_log::dev!("viewport", "action", endpoint = endpoint, fields = names.join(","));
        }
        let origin = match self.current() {
            Source::Remote { host, port, .. } => Some((host.clone(), *port)),
            Source::App { key, .. } => InstallStore::open(&self.fetcher.data_dir)
                .ok()
                .and_then(|store| store.get(key).ok().flatten())
                .map(|app| (app.host, app.port)),
            _ => None,
        };
        let Some((host, port)) = origin else {
            self.error = Some("this document has no server to submit actions to".into());
            self.tree = None;
            return;
        };
        self.generation += 1;
        self.loading = true;
        // An action's response *is* this page, re-served: keep scroll/focus.
        self.in_place_once = true;
        let (tx, rx) = oneshot::channel();
        self.fetcher.spawn_action(host, port, endpoint, fields, tx);
        self.pending_page = Some(PendingFetch {
            generation: self.generation,
            kind: LoadKind::Action,
            rx,
            clears,
        });
    }

    /// Fire the page's declared `closing` action, if any, and give it up to
    /// `budget` to land. Called by hosts on the way out — after this the
    /// process exits, so waiting is the only way the goodbye ever reaches
    /// the wire. Best-effort by construction: an error or a blown budget
    /// changes nothing, the app's own timeout is the safety net.
    pub fn say_goodbye(&mut self, budget: Duration) {
        let Some(endpoint) = self.close_target() else { return };
        let origin = match self.current() {
            Source::Remote { host, port, .. } => Some((host.clone(), *port)),
            Source::App { key, .. } => InstallStore::open(&self.fetcher.data_dir)
                .ok()
                .and_then(|store| store.get(key).ok().flatten())
                .map(|app| (app.host, app.port)),
            _ => None,
        };
        let Some((host, port)) = origin else { return };
        let (tx, mut rx) = oneshot::channel();
        self.fetcher.spawn_action(host, port, endpoint, Vec::new(), tx);
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    // ---- capability (host-driven trusted prompt) ----

    fn request_pick_file(&mut self, into: u16) {
        let (allowed, app_name) = match self.current() {
            Source::App { key, name, .. } => InstallStore::open(&self.fetcher.data_dir)
                .ok()
                .and_then(|s| s.manifest(key).ok())
                .map(|m| (m.permissions.get("files").copied().unwrap_or(false), name.clone()))
                .unwrap_or((false, name.clone())),
            _ => (false, String::new()),
        };
        if !allowed {
            return;
        }
        self.capability = Some(CapabilityRequest { app_name, into });
    }

    /// The pending capability request, if the host should raise a prompt.
    pub fn capability(&self) -> Option<&CapabilityRequest> {
        self.capability.as_ref()
    }

    pub fn has_capability(&self) -> bool {
        self.capability.is_some()
    }

    /// Host fulfilled the file pick: text content of the chosen file.
    pub fn provide_file(&mut self, content: String) {
        if let Some(req) = self.capability.take()
            && let Some(slot) = self.state.get_mut(req.into as usize)
        {
            *slot = ActionValue::Str(content);
            self.mark_dirty(req.into);
        }
    }

    pub fn cancel_capability(&mut self) {
        self.capability = None;
    }
}

/// A host-presented context menu: items from a declared MenuArea, geometry
/// computed against the live theme each layout. Every app gets the same
/// presenter, which is the entire point.
struct OpenMenu {
    items: Vec<MenuItem>,
    /// Where it was invoked, in document coordinates.
    at: (f32, f32),
    rect: Rect,
    item_rects: Vec<Rect>,
    /// Keyboard-hovered item index (skips separators).
    hover: Option<usize>,
}

impl OpenMenu {
    fn new(items: Vec<MenuItem>, at: (f32, f32)) -> OpenMenu {
        OpenMenu { items, at, rect: Rect::default(), item_rects: Vec::new(), hover: None }
    }

    /// Size and place the panel: kit-tier sizing from the theme's base font,
    /// clamped into the viewport, flipping up when there's no room below.
    fn layout(
        &mut self,
        theme: &Defaults,
        viewport: Rect,
        scroll: f32,
        cursor: Option<(f32, f32)>,
        unbounded: bool,
        measurer: &mut dyn TextMeasurer,
    ) {
        let f = theme.font_size;
        let pad = 6.0_f32;
        let item_h = (f * 1.4 + 2.0 * pad).round();
        let sep_h = (pad * 2.0 + 1.0).round();
        let has_icons = self.items.iter().any(|i| i.icon.is_some());
        let icon_col = if has_icons { item_h } else { pad };
        let mut width: f32 = 0.0;
        for item in &self.items {
            if item.separator {
                continue;
            }
            let m = measurer.measure(&item.label, f, 400, &theme.font_family, f32::MAX);
            width = width.max(m.width);
        }
        let width = (width + icon_col + pad * 3.0).clamp(160.0, 340.0);
        let height: f32 = self
            .items
            .iter()
            .map(|i| if i.separator { sep_h } else { item_h })
            .sum();

        // Viewport is host-local; the menu lives in doc space (scrolled).
        let (vx0, vy0) = (viewport.x, viewport.y + scroll);
        let (vx1, vy1) = (viewport.x + viewport.w, viewport.y + viewport.h + scroll);
        // Placement, the native convention: grow right/down from the
        // invocation point; within a menu-width of the right edge (or a
        // menu-height of the bottom), *mirror* around the point instead —
        // the menu hugs the click, never the far wall. Clamping is the last
        // resort, for viewports smaller than the menu itself.
        // A host that lets menus escape (the dock strip) needs no clamping
        // at all: the menu grows from the click and the compositor draws it.
        if unbounded {
            self.rect = Rect { x: self.at.0, y: self.at.1, w: width, h: height };
            self.item_rects.clear();
            let mut cy = self.at.1;
            for item in &self.items {
                let h = if item.separator { sep_h } else { item_h };
                self.item_rects.push(Rect { x: self.at.0, y: cy, w: width, h });
                cy += h;
            }
            if let Some((cx, cyp)) = cursor
                && self.hit(cx, cyp).is_some()
            {
                self.hover = self.hit(cx, cyp);
            }
            return;
        }
        let mut x = self.at.0;
        if x + width > vx1 {
            x = if self.at.0 - width >= vx0 {
                self.at.0 - width
            } else {
                (vx1 - width).max(vx0)
            };
        }
        let mut y = self.at.1;
        if y + height > vy1 {
            y = if self.at.1 - height >= vy0 {
                self.at.1 - height
            } else {
                (vy1 - height).max(vy0)
            };
        }
        self.rect = Rect { x, y, w: width, h: height };
        self.item_rects.clear();
        let mut cy = y;
        for item in &self.items {
            let h = if item.separator { sep_h } else { item_h };
            self.item_rects.push(Rect { x, y: cy, w: width, h });
            cy += h;
        }
        // Pointer hover overrides keyboard hover while the cursor is inside.
        if let Some((cx, cyp)) = cursor
            && let Some(hit) = self.hit(cx, cyp)
        {
            self.hover = Some(hit);
        }
    }

    /// The activatable item at a document-space point.
    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.item_rects.iter().enumerate().find_map(|(i, r)| {
            let inside = x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
            (inside && !self.items[i].separator).then_some(i)
        })
    }

    fn inside(&self, x: f32, y: f32) -> bool {
        let r = &self.rect;
        x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
    }

    /// Move keyboard hover by ±1 over activatable items, wrapping.
    fn step(&mut self, delta: isize) {
        let idx: Vec<usize> =
            (0..self.items.len()).filter(|&i| !self.items[i].separator).collect();
        if idx.is_empty() {
            return;
        }
        let pos = self
            .hover
            .and_then(|h| idx.iter().position(|&i| i == h))
            .map(|p| (p as isize + delta).rem_euclid(idx.len() as isize) as usize)
            .unwrap_or(if delta > 0 { 0 } else { idx.len() - 1 });
        self.hover = Some(idx[pos]);
    }

    fn paint(&self, theme: &Defaults, out: &mut Vec<DrawCommand>) {
        let f = theme.font_size;
        let pad = 6.0_f32;
        let token = |name: &str, fallback: Color| theme.token(name).unwrap_or(fallback);
        let surface = token("surface-raised", Color { r: 0x24, g: 0x24, b: 0x38, a: 0xFF });
        let border = token("border", Color { r: 0x33, g: 0x33, b: 0x4a, a: 0xFF });
        let text = token("text", theme.text_color);
        let muted = token("text-muted", text);
        let lift = token("elevation-lg", border);
        // Destruction never looks like its neighbours (kit rule); the reds
        // are deliberate literals, same as the kit's danger style.
        let danger = Color { r: 0xff, g: 0x9a, b: 0xa6, a: 0xFF };
        out.push(DrawCommand::Shadow {
            rect: self.rect,
            color: Color { r: 0, g: 0, b: 0, a: 120 },
            blur: 18.0,
            spread: 0.0,
            corner_radius: 0.0,
        });
        out.push(DrawCommand::Rect { rect: self.rect, color: surface, corner_radius: 0.0 });
        out.push(DrawCommand::Border {
            rect: self.rect,
            color: border,
            width: 1.0,
            corner_radius: 0.0,
        });
        let has_icons = self.items.iter().any(|i| i.icon.is_some());
        let item_h = (f * 1.4 + 2.0 * pad).round();
        let icon_col = if has_icons { item_h } else { pad };
        for (i, item) in self.items.iter().enumerate() {
            let r = self.item_rects[i];
            if item.separator {
                out.push(DrawCommand::Rect {
                    rect: Rect { x: r.x + pad, y: r.y + r.h / 2.0, w: r.w - 2.0 * pad, h: 1.0 },
                    color: border,
                    corner_radius: 0.0,
                });
                continue;
            }
            if self.hover == Some(i) {
                out.push(DrawCommand::Rect { rect: r, color: lift, corner_radius: 0.0 });
            }
            let color = if item.danger { danger } else { text };
            if let Some(name) = &item.icon
                && let Some(glyph) = rill_ui::icons::icon(name)
            {
                let size = f * 1.4;
                let (points, contours) =
                    glyph.at(r.x + pad, r.y + (r.h - size) / 2.0, size);
                out.push(DrawCommand::FillPath {
                    points,
                    contours,
                    color: if item.danger { danger } else { muted },
                });
            }
            out.push(DrawCommand::Text {
                rect: Rect {
                    x: r.x + icon_col + pad,
                    y: r.y + pad,
                    w: r.w - icon_col - 2.0 * pad,
                    h: r.h - 2.0 * pad,
                },
                text: item.label.clone(),
                color,
                font_size: f,
                font_weight: 400,
                font_family: theme.font_family.clone(),
            });
        }
    }
}

/// Outcome of an [`AppView::on_click`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickResult {
    /// Nothing interactive was hit.
    Miss,
    /// An input was focused or a button action performed.
    Consumed,
    /// A link was clicked; the host decides how to follow it.
    Link(String),
}

/// The result of a key press (mirrors [`ClickResult`] for links so keyboard
/// activation and clicks route the same way).
#[derive(Debug, PartialEq)]
pub enum KeyResult {
    /// The key wasn't consumed by the surface.
    Ignored,
    /// The key was handled internally (edit, focus move, action).
    Handled,
    /// A focused link was activated; the host decides how to follow it.
    Link(String),
}

/// What cursor the host should show over this surface at the current point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorHint {
    Default,
    Pointer,
    Text,
}


// ---- text-input editing (pure helpers on a string + a byte-offset caret) ----
//
// The caret is a byte offset kept on a UTF-8 char boundary. Movement is by
// char (grapheme clusters are future work); Home/End are line-relative.

/// Delete the char before the caret (Backspace), moving the caret back.
fn backspace_at(s: &mut String, caret: &mut usize) {
    let c = (*caret).min(s.len());
    if c > 0 {
        let prev = s[..c].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
        s.replace_range(prev..c, "");
        *caret = prev;
    } else {
        *caret = c;
    }
}

/// Delete the char after the caret (Delete), caret unchanged.
fn delete_at(s: &mut String, caret: &mut usize) {
    let c = (*caret).min(s.len());
    if let Some(ch) = s[c..].chars().next() {
        s.replace_range(c..c + ch.len_utf8(), "");
    }
    *caret = c;
}

/// Caret one char left (or unchanged at the start).
fn move_left(s: &str, caret: usize) -> usize {
    let c = caret.min(s.len());
    s[..c].char_indices().next_back().map(|(i, _)| i).unwrap_or(0)
}

/// Caret one char right (or unchanged at the end).
fn move_right(s: &str, caret: usize) -> usize {
    let c = caret.min(s.len());
    s[c..].chars().next().map(|ch| c + ch.len_utf8()).unwrap_or(c)
}

/// Caret to the start of its current line.
fn line_start(s: &str, caret: usize) -> usize {
    let c = caret.min(s.len());
    s[..c].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Caret to the end of its current line.
fn line_end(s: &str, caret: usize) -> usize {
    let c = caret.min(s.len());
    s[c..].find('\n').map(|i| c + i).unwrap_or(s.len())
}

/// Caret one line up, holding its column (clamped to the shorter line).
/// On the first line the caret goes to the start — where every editor puts
/// it, and better than doing nothing.
fn line_up(s: &str, caret: usize) -> usize {
    let start = line_start(s, caret);
    if start == 0 {
        return 0;
    }
    let col = s[start..caret.min(s.len())].chars().count();
    let prev_start = line_start(s, start - 1);
    byte_at_col(s, prev_start, start - 1, col)
}

/// Caret one line down, holding its column. On the last line it goes to the
/// end, mirroring [`line_up`].
fn line_down(s: &str, caret: usize) -> usize {
    let end = line_end(s, caret);
    if end == s.len() {
        return s.len();
    }
    let start = line_start(s, caret);
    let col = s[start..caret.min(s.len())].chars().count();
    let next_start = end + 1;
    byte_at_col(s, next_start, line_end(s, next_start), col)
}

/// The byte offset `col` chars into the line `start..end`, clamped to `end`.
fn byte_at_col(s: &str, start: usize, end: usize, col: usize) -> usize {
    s[start..end]
        .char_indices()
        .nth(col)
        .map(|(i, _)| start + i)
        .unwrap_or(end)
}

/// The selected byte range `(lo, hi)`, clamped to the string; `lo == hi` means
/// no selection.
fn selection_bounds(anchor: usize, caret: usize, len: usize) -> (usize, usize) {
    (anchor.min(caret).min(len), anchor.max(caret).min(len))
}

/// Replace the current selection (`anchor..caret`) with `text`, respecting the
/// byte `cap`; returns the new collapsed caret. Deleting the selection always
/// succeeds; the insert is all-or-nothing if it would exceed the cap (so a
/// too-large paste clears the selection but inserts nothing). With no selection
/// (`anchor == caret`) this is a plain insert at the caret.
fn replace_selection(s: &mut String, anchor: usize, caret: usize, text: &str, cap: usize) -> usize {
    let (lo, hi) = selection_bounds(anchor, caret, s.len());
    if s.len() - (hi - lo) + text.len() <= cap {
        s.replace_range(lo..hi, text);
        lo + text.len()
    } else {
        s.replace_range(lo..hi, "");
        lo
    }
}

/// Append a 2px accent outline just outside `r` — a keyboard focus ring. Four
/// thin rects, since `Rect` is a filled primitive.
fn push_focus_ring(commands: &mut Vec<DrawCommand>, r: &Rect, color: Color) {
    let t = 2.0;
    let o = r.inset(-t);
    let bar = |x, y, w, h| DrawCommand::Rect { rect: Rect { x, y, w, h }, color, corner_radius: 0.0 };
    commands.push(bar(o.x, o.y, o.w, t));
    commands.push(bar(o.x, o.y + o.h - t, o.w, t));
    commands.push(bar(o.x, o.y, t, o.h));
    commands.push(bar(o.x + o.w - t, o.y, t, o.h));
}

/// Map a click within a text input to a caret byte-offset, measuring with the
/// shaper so the caret lands where the glyphs actually are. The clicked logical
/// line is chosen by y (multiline splits on `\n`; visual wrapping of one long
/// line isn't accounted for), then the nearest char boundary by x within it.
/// The x-range of `run` the selection covers, or None if it misses the run
/// entirely. Vertical membership is by the run's midline; the from/to lines
/// clip horizontally, every line between is taken whole.
fn span_of_run(run: &SelRun, from: (f32, f32), to: (f32, f32)) -> Option<(f32, f32)> {
    let mid = run.rect.y + run.rect.h / 2.0;
    let on_from_line = from.1 >= run.rect.y && from.1 < run.rect.y + run.rect.h;
    let on_to_line = to.1 >= run.rect.y && to.1 < run.rect.y + run.rect.h;
    let inside_band = mid > from.1 && mid < to.1;
    if !(on_from_line || on_to_line || inside_band) {
        return None;
    }
    let x0 = if on_from_line { from.0.max(run.rect.x) } else { run.rect.x };
    let x1 = if on_to_line {
        to.0.min(run.rect.x + run.rect.w)
    } else {
        run.rect.x + run.rect.w
    };
    (x1 > x0).then_some((x0, x1))
}

/// Char boundaries of a run's slice between two x positions, by the same
/// binary search the input caret uses — prefix width only grows.
fn char_bounds(
    run: &SelRun,
    x0: f32,
    x1: f32,
    m: &mut dyn TextMeasurer,
) -> (usize, usize) {
    let bounds: Vec<usize> =
        run.text.char_indices().map(|(i, _)| i).chain(std::iter::once(run.text.len())).collect();
    let prefix = |m: &mut dyn TextMeasurer, end: usize| -> f32 {
        if end == 0 {
            0.0
        } else {
            m.measure(&run.text[..end], run.font_size, run.font_weight, &run.font_family, f32::MAX)
                .width
        }
    };
    let search = |m: &mut dyn TextMeasurer, target: f32, round_up: bool| -> usize {
        let (mut lo, mut hi) = (0usize, bounds.len() - 1);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if prefix(m, bounds[mid]) < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // `lo` is the first boundary at or past target: taking it as-is
        // rounds the start of a selection in, the end out — a drag that
        // covers half a character keeps it, which is how it feels right.
        if !round_up && lo > 0 && prefix(m, bounds[lo]) > target {
            lo -= 1;
        }
        bounds[lo]
    };
    let c0 = search(m, (x0 - run.rect.x).max(0.0), false);
    let c1 = search(m, (x1 - run.rect.x).max(0.0), true);
    (c0, c1)
}

fn caret_at_click(
    area: &InputArea,
    text: &str,
    x: f32,
    y: f32,
    m: &mut dyn TextMeasurer,
) -> usize {
    let width = |m: &mut dyn TextMeasurer, t: &str| {
        m.measure(t, area.font_size, area.font_weight, &area.font_family, f32::MAX).width
    };
    let line_h = m
        .measure("x", area.font_size, area.font_weight, &area.font_family, f32::MAX)
        .height
        .max(1.0);
    let lines: Vec<&str> = text.split('\n').collect();
    let rel_y = (y - area.rect.y - area.pad_y).max(0.0);
    let line_idx = ((rel_y / line_h) as usize).min(lines.len().saturating_sub(1));
    let line_start: usize = lines[..line_idx].iter().map(|l| l.len() + 1).sum();
    let line = lines[line_idx];

    let target = (x - area.rect.x - area.pad_x).max(0.0);
    let bounds: Vec<usize> =
        line.char_indices().map(|(i, _)| i).chain(std::iter::once(line.len())).collect();
    let prefix_width = |m: &mut dyn TextMeasurer, end: usize| {
        if end == 0 { 0.0 } else { width(m, &line[..end]) }
    };

    // Binary search, because prefix width only grows: measuring every
    // boundary was O(n) measures per click, and each one is a *distinct*
    // shaping-cache key holding its own copy of the prefix — so clicking once
    // in a long line cost quadratic time and inserted a quadratic number of
    // bytes into a cache sized for whole strings, evicting everything real.
    // A search of ~log n prefixes costs neither.
    let mut lo = 0usize;
    let mut hi = bounds.len() - 1;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if prefix_width(&mut *m, bounds[mid]) < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }

    // `lo` is the first boundary at or past the target; the click may still
    // be nearer the one before it — the caret goes wherever it is closest.
    if lo > 0 {
        let after = prefix_width(&mut *m, bounds[lo]);
        let before = prefix_width(&mut *m, bounds[lo - 1]);
        if (target - before).abs() <= (after - target).abs() {
            lo -= 1;
        }
    }
    line_start + bounds[lo]
}


fn collect_image_sources(node: &ResolvedNode, out: &mut Vec<String>) {
    if let ResolvedNode::Image { source, .. } = node {
        out.push(source.clone());
    }
    match node {
        ResolvedNode::Row { children, .. } | ResolvedNode::Column { children, .. } => {
            for c in children {
                collect_image_sources(c, out);
            }
        }
        ResolvedNode::Scroll { child, .. } | ResolvedNode::When { child, .. } => {
            collect_image_sources(child, out)
        }
        _ => {}
    }
}

/// List enrolled files under a pick root (flat), for the file-picker prompt.
pub fn pick_root_files(root: &PathBuf) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false)
                && let Some(name) = e.file_name().to_str()
                // Never expose dotfiles — they are configuration/secrets
                // (.ssh, .bash_history, .gitconfig, …), not documents to share.
                && !name.starts_with('.')
            {
                files.push((name.to_string(), e.path()));
            }
        }
    }
    files.sort();
    files
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    /// A fixed-advance measurer, so a caret position is arithmetic and the
    /// test can say exactly where a click should land. It also counts calls,
    /// which is the other half of the point.
    struct Monospace {
        advance: f32,
        calls: std::cell::Cell<usize>,
    }

    impl TextMeasurer for Monospace {
        fn measure(
            &mut self,
            text: &str,
            _size: f32,
            _weight: u16,
            _family: &str,
            _max_width: f32,
        ) -> rill_ui::LineMetrics {
            self.calls.set(self.calls.get() + 1);
            rill_ui::LineMetrics {
                width: text.chars().count() as f32 * self.advance,
                height: 10.0,
            }
        }
    }

    /// Clicking into a field must land on the nearest character boundary, and
    /// must not cost a measurement per character to work it out.
    ///
    /// The count matters as much as the answer. Every prefix measured is a
    /// separate shaping-cache key that owns a copy of that prefix, so the
    /// exhaustive scan this replaced spent quadratic time *and* pushed
    /// quadratic bytes through a cache sized for whole strings — one click in
    /// a long line could evict everything else in it.
    #[test]
    fn a_click_finds_its_caret_without_measuring_every_prefix() {
        let area = InputArea {
            rect: Rect { x: 0.0, y: 0.0, w: 1000.0, h: 20.0 },
            state: 0,
            on_enter: None,
            multiline: false,
            tab_inserts: false,
            font_size: 10.0,
            font_weight: 400,
            font_family: "mono".into(),
            pad_x: 0.0,
            pad_y: 0.0,
        };
        let text: String = std::iter::repeat_n('x', 512).collect();
        let mut m = Monospace { advance: 8.0, calls: std::cell::Cell::new(0) };

        // Exhaustively: what the linear scan would have answered, everywhere.
        for chars in [0usize, 1, 7, 100, 255, 511, 512] {
            m.calls.set(0);
            let click_x = chars as f32 * 8.0;
            let caret = caret_at_click(&area, &text, click_x, 0.0, &mut m);
            assert_eq!(caret, chars, "click at x={click_x} should sit before char {chars}");
            assert!(
                m.calls.get() < 32,
                "took {} measurements for a 512-char line — the search is linear again",
                m.calls.get()
            );
        }

        // Between two characters, the nearer one wins; exactly halfway rounds
        // to the earlier boundary.
        m.calls.set(0);
        assert_eq!(caret_at_click(&area, &text, 8.0 * 10.0 + 3.0, 0.0, &mut m), 10);
        assert_eq!(caret_at_click(&area, &text, 8.0 * 10.0 + 5.0, 0.0, &mut m), 11);
        assert_eq!(caret_at_click(&area, &text, 8.0 * 10.0 + 4.0, 0.0, &mut m), 10);
        // Past the end clamps to the end rather than running off it.
        assert_eq!(caret_at_click(&area, &text, 99_999.0, 0.0, &mut m), 512);
    }

    #[test]
    fn backspace_and_delete_at_caret() {
        let mut s = "abc".to_string();
        let mut caret = 2;
        backspace_at(&mut s, &mut caret); // removes 'b'
        assert_eq!((s.as_str(), caret), ("ac", 1));
        delete_at(&mut s, &mut caret); // removes 'c'
        assert_eq!((s.as_str(), caret), ("a", 1));
    }

    #[test]
    fn movement_is_char_aware_and_line_relative() {
        let s = "aé\nxy"; // 'é' is 2 bytes: indices a=0, é=1..3, \n=3, x=4, y=5
        assert_eq!(move_right(s, 1), 3, "skips the whole 2-byte char");
        assert_eq!(move_left(s, 3), 1);
        assert_eq!(line_start(s, 5), 4, "start of the second line");
        assert_eq!(line_end(s, 0), 3, "end of the first line (before \\n)");
    }

    #[test]
    fn replace_selection_edits_and_collapses() {
        // Replace "bc" in "abcd" with "X".
        let mut s = "abcd".to_string();
        let caret = replace_selection(&mut s, 1, 3, "X", 100);
        assert_eq!((s.as_str(), caret), ("aXd", 2));
        // Empty selection → plain insert at the caret.
        let mut t = "ad".to_string();
        let c = replace_selection(&mut t, 1, 1, "bc", 100);
        assert_eq!((t.as_str(), c), ("abcd", 3));
        // Delete-then-insert respects the cap: a paste too big to fit still
        // clears the selection but inserts nothing.
        let mut u = "abcd".to_string();
        let c = replace_selection(&mut u, 1, 2, "xxxxxx", 5); // len 3 + 6 > 5
        assert_eq!((u.as_str(), c), ("acd", 1), "selection cleared, oversized insert dropped");
    }

    #[test]
    fn selection_bounds_orders_and_clamps() {
        assert_eq!(selection_bounds(4, 1, 10), (1, 4));
        assert_eq!(selection_bounds(2, 2, 10), (2, 2)); // empty
        assert_eq!(selection_bounds(0, 99, 5), (0, 5)); // clamped to len
    }
}
