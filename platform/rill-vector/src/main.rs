//! rill-vector: the first vector-native Rill client (W4,
//! specs/wgpu-renderer.md). Its window has **no pixel buffer and no GPU** —
//! every frame is a `rill_ui::stream`-encoded DrawCommand list delivered to
//! the compositor over `rill_stream_v1` (memfd + wl_surface.commit), which
//! renders it via rill-gpu. WM is ordinary xdg_toplevel; input is ordinary
//! wl_seat, hit-tested locally against the client's own command list.
//!
//! Resize is the demo: a configure triggers relayout — the document
//! *reflows* at the new size, kilobytes over the wire, no pixel scaling.
//!
//! ```bash
//! rill-vector                # built-in two-page demo document
//! rill-vector --doc a.rill   # render a compiled .rill document
//! ```

mod dashboard;
mod dock;
mod replay;
mod stream_protocol;

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use std::path::PathBuf;

use rill_doc::{ActionValue, Color, Document};
use rill_gpu::text::{EngineMeasurer, TextEngine};
use rill_ui::{Defaults, DrawCommand, LayoutOptions, NoImages, Rect, layout_document, resolve};
use rill_viewport::theme;
use rill_viewport::{
    AppView, ClickResult, CursorHint, Fetcher, KeyResult, ReadyImages, Source, launch_source,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    Shape, WpCursorShapeDeviceV1,
};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers};
use smithay_client_toolkit::seat::pointer::cursor_shape::CursorShapeManager;
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::{XdgShell, XdgSurface};
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::{
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};
use stream_protocol::rill_stream_manager_v1::RillStreamManagerV1;
use stream_protocol::rill_stream_v1::RillStreamV1;
use wayland_client::backend::WaylandError;
use smithay_client_toolkit::data_device_manager::DataDeviceManagerState;
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::DataSourceHandler;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_data_device, wl_data_source, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface,
};
use wayland_client::{Connection, Dispatch, DispatchError, Proxy, QueueHandle};

/// Client-side chrome: the titlebar is just more DrawCommands in the stream.
/// Its height, the window radius and the frost all come from the theme's
/// `[window]` table (`theme::WindowStyle`), so they can be tuned live; the
/// modes with no theme — the demo, the dashboard, the replay host — run on
/// that struct's defaults.
///
/// Space kept clear at the right end for the close glyph.
const CLOSE_WIDTH: f32 = 40.0;

/// Glass mode: cap the page background's alpha so the frosted desktop shows
/// through it. The page bg is the first near-full-coverage rect in the list.
/// Glass mode: the page background must not paint — the rounded glass fill
/// is chrome-owned (fixed to the window; the page rect scrolls with content,
/// so rounding or tinting it produced moving corner artifacts). Zero the
/// first page-sized rect's alpha and return its color so the chrome fill
/// keeps the page's palette.
fn glass_page_background(commands: &mut [DrawCommand], w: f32, h: f32) -> Option<Color> {
    for cmd in commands.iter_mut().take(8) {
        if let DrawCommand::Rect { rect, color, .. } = cmd
            && rect.x <= 1.0
            && rect.y <= 1.0
            && rect.w >= w * 0.85
            && rect.h >= h * 0.5
        {
            let found = *color;
            color.a = 0;
            return Some(found);
        }
    }
    None
}

/// Edge band (px) that starts a resize drag instead of a move/click.
const EDGE: f32 = 8.0;

/// How far one unit of wheel input moves the page. Applied to the scroll
/// paths only — zoom keeps the raw delta — and *before* the viewport's
/// easing, so a bigger stride arrives smoothly rather than jumping: the
/// ease-toward-target rate is the smoothness, this is the speed.
///
/// A notch is 15 units, so this is 60 px of page per notch — about three
/// lines of body text, which is what everything else on the desktop does.
/// At 2.5 it was 37.5, and a page took half again as many notches to read
/// as the browser beside it.
const SCROLL_SPEED: f32 = 4.0;

// Translucent over a Backdrop command: the bar frosts whatever the desktop
// has behind the window (D6 — frosted glass as a paintable primitive). The
// tint must stay light-handed or it buries the frost — blur only *reads*
// where detail shows through. In hosts without a backdrop the same fill
// just reads darker.
/// Chrome colours for the modes that have no document to ask (the dashboard,
/// the replay host, the built-in demo). A page-backed window overrides these
/// from its own theme — see [`App::chrome_palette`].
const CHROME_BG: Color = Color { r: 0x23, g: 0x2a, b: 0x3d, a: 0x5c };
const CHROME_TEXT: Color = Color { r: 0xDF, g: 0xE4, b: 0xF2, a: 0xFF };
const CHROME_DIM: Color = Color { r: 0x9A, g: 0xA3, b: 0xB5, a: 0xFF };

/// The RILL_TRACE inspector: reverse-maps the unique trace colours back to
/// style names, so hovering any surface names it in the bar. The legend is
/// written by the server on every page it traces; colours are the join key,
/// which is why nothing here needs to know about documents at all.
struct TraceInspector {
    legend_path: std::path::PathBuf,
    legend: std::collections::HashMap<(u8, u8, u8), String>,
    legend_mtime: Option<std::time::SystemTime>,
    frame: Vec<DrawCommand>,
    under_cursor: Option<String>,
}

impl TraceInspector {
    fn reload_if_changed(&mut self) {
        let mtime = std::fs::metadata(&self.legend_path).and_then(|m| m.modified()).ok();
        if mtime == self.legend_mtime {
            return;
        }
        self.legend_mtime = mtime;
        self.legend.clear();
        if let Ok(text) = std::fs::read_to_string(&self.legend_path) {
            for line in text.lines() {
                if let Some((hex, name)) = line.split_once(' ')
                    && let Some(c) = rill_doc::Color::parse_hex(hex)
                {
                    self.legend.insert((c.r, c.g, c.b), name.to_string());
                }
            }
        }
    }

    /// The topmost painted surface under a point, by name (hex if unknown).
    fn lookup(&self, x: f32, y: f32) -> Option<String> {
        for cmd in self.frame.iter().rev() {
            if let DrawCommand::Rect { rect, color, .. } = cmd
                && color.a > 0
                && x >= rect.x
                && x < rect.x + rect.w
                && y >= rect.y
                && y < rect.y + rect.h
            {
                return Some(match self.legend.get(&(color.r, color.g, color.b)) {
                    Some(name) => name.clone(),
                    None => format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b),
                });
            }
        }
        None
    }
}

/// Whether the titlebar carries the live wire cost. It is genuinely useful
/// while building Rill and meaningless to anyone using an app, and it was
/// occupying the most valuable strip in the window — so it is opt-in.
fn wire_cost_visible() -> bool {
    std::env::var_os("RILL_WIRE_COST").is_some()
}

/// Built-in demo: two pages linked to each other, so clicking is visibly
/// interactive through the stream.
const PAGE_ONE: &str = r##"
style "title" size=24 weight="bold" color="#8ab4ff"
style "dim" size=13 color="#9aa3b5"
column gap=14 padding=28 {
    text "Vector-native window" style="title"
    text "This window has no pixel buffer. Every frame you see is a DrawCommand stream the compositor renders itself."
    text "Resize me: the document reflows at the new size — kilobytes over the wire, never a scaled pixel."
    text "A frame of this page is about two kilobytes. A pixel buffer of this window would be a few megabytes." style="dim"
    text "Ctrl+scroll zooms the content — reflowed, re-rasterized, never scaled pixels. Watch the titlebar count the bytes." style="dim"
    link "Go to page two" target="/page/1"
    link "Type specimen" target="/page/2"
}
"##;

const PAGE_TWO: &str = r##"
style "title" size=24 weight="bold" color="#7bd88f"
column gap=14 padding=28 {
    text "Page two" style="title"
    text "That link click travelled: wl_seat input, a local hit-test against the command list, a relayout, and a fresh stream frame."
    link "Back to page one" target="/page/0"
}
"##;

/// The zoom showcase: a type ramp down to fine print. Zoom in — every size
/// re-rasterizes from the same commands; the 6px line reads perfectly at 300%.
const PAGE_THREE: &str = r##"
style "title" size=26 weight="bold" color="#e0a458"
style "t32" size=32 weight="bold"
style "t22" size=22
style "t16" size=16
style "t12" size=12
style "t9" size=9
style "t6" size=6
style "dim" size=12 color="#9aa3b5"
column gap=10 padding=28 {
    text "Type specimen" style="title"
    text "The quick brown fox jumps over the lazy dog" style="t32"
    text "The quick brown fox jumps over the lazy dog" style="t22"
    text "The quick brown fox jumps over the lazy dog" style="t16"
    text "The quick brown fox jumps over the lazy dog" style="t12"
    text "The quick brown fox jumps over the lazy dog" style="t9"
    text "This line is set at six pixels. Ctrl+scroll to 300% and it becomes perfectly legible — the glyphs are re-rasterized from the command stream, not blown up from a bitmap." style="t6"
    text "Every size above travels as the same Text command with a different font_size. Zoom changes one number; the atlas does the rest." style="dim"
    link "Back to page one" target="/page/0"
}
"##;

fn compile_page(kdl: &str) -> Document {
    let compiled = rill_doc::compile(kdl).expect("built-in page compiles");
    rill_doc::decode(&compiled.bytes).expect("built-in page decodes")
}

/// Write a frame into a sealed memfd.
/// Whether one image goes to the compositor this frame.
///
/// Pure, because these four lines are the image transport's entire flow
/// control and every clause has a failure it exists to prevent:
///
/// * Not held at all → **send always**, whatever else is happening. This is
///   the hole-prevention rule: an evicted (`image_released`) or never-sent
///   picture has nothing on the other side to draw.
/// * Held at this exact size → nothing to do.
/// * Held sharper, and ours is a stand-in awaiting a finer copy → skip, or
///   scrolling back to a picture would downgrade it while its refetch lands.
/// * Held at a different size otherwise → **only when the window's shape has
///   settled**. Mid-drag, the compositor scales the copy it holds into the
///   new rect — a stretch nobody can see at drag speed — and the megabytes
///   move once, when the hand stops. Re-sending per halving crossing is what
///   stalled the compositor, backed the socket up, and filled libwayland's
///   outgoing fd ring: the "can't send file descriptor" crash.
fn plan_image_send(
    sent: Option<(u32, u32)>,
    offered: (u32, u32),
    provisional: bool,
    settled: bool,
) -> bool {
    match sent {
        None => true,
        Some(held) if held == offered => false,
        Some((w, h)) if w >= offered.0 && h >= offered.1 && provisional => false,
        Some(_) => settled,
    }
}

fn memfd_frame(bytes: &[u8]) -> std::io::Result<OwnedFd> {
    let name = std::ffi::CString::new("rill-stream-frame").unwrap();
    let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let file = std::fs::File::from(fd.try_clone()?);
    use std::os::unix::fs::FileExt;
    file.write_all_at(bytes, 0)?;
    // The seals are the compositor's guarantee that the buffer it is about to
    // read cannot be shrunk or rewritten underneath it. If they don't take,
    // the frame is not safe to hand over — fail rather than send an
    // unsealed one and hope.
    // SAFETY: `fd` is a live memfd created above with MFD_ALLOW_SEALING.
    let sealed = unsafe {
        libc::fcntl(
            fd.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE,
        )
    };
    if sealed < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    /// Clipboard. Paste only for now: this client can *read* the selection
    /// but never offers one, because nothing in a vector window is
    /// selectable yet — the terminal draws its scrollback as styled cells
    /// with no selection model over them.
    data_device_manager: Option<DataDeviceManagerState>,
    data_device: Option<DataDevice>,
    output_state: OutputState,
    window: Window,
    stream: RillStreamV1,
    /// Image sources already handed to the compositor, and the size they were
    /// sent at. Pixels ride out of band once per source; a frame that names
    /// one again costs nothing. The size is kept so a source whose content
    /// changed — same path, new picture — is re-sent rather than shown stale.
    sent_images: HashMap<String, (u32, u32)>,
    /// The tier last sent over `set_tier`. Starts at 0 — the compositor's
    /// default — so a window that never shows a sensitive document never
    /// sends the request at all.
    sent_tier: u8,
    engine: TextEngine,
    /// App mode: a full AppView (fetch/state/actions/theme) behind the
    /// stream sink — a *real* Rill app as a vector-native window. When
    /// `None`, the built-in demo pages below drive the window instead.
    view: Option<AppView>,
    /// Dashboard mode (`--dashboard`): a live system monitor drawn
    /// programmatically from /proc, rather than laid out from a document.
    dashboard: Option<dashboard::Dashboard>,
    /// Dock mode (`--dock`): the desktop's launcher strip. The document in
    /// `view` is dock-generated; its `/~…` links are handled here (launch,
    /// theme/desktop toggles) instead of navigating. Chromeless: no titlebar,
    /// no resize edges — the compositor pins and sizes the strip.
    dock: Option<dock::Dock>,
    /// Widget mode (`--widget`): a small always-on window the theme placed.
    /// It keeps its titlebar strip (drag handle, close) but drops the title
    /// *text* — a meter in a corner labelling itself is furniture wearing a
    /// name tag.
    widget: bool,
    /// The dock's material and shape, cached from the theme. Re-read when
    /// the theme file changes, not per frame: it is a file read.
    dock_style: Option<dock::DockStyle>,
    /// Replay mode (`--replay FILE`): a recorded session played back as
    /// vectors in this window.
    replay: Option<replay::Replay>,
    /// Outstanding view work (a fetch or image in flight). Not a repaint
    /// signal — it only shortens how long the loop is willing to sleep.
    view_pending: bool,
    data_dir: Option<PathBuf>,
    /// (theme.toml, runtime sidecar, last runtime mtime) for live theming.
    theme_state: Option<(PathBuf, theme::ThemeStamp)>,
    /// Density fingerprint at last reload; a change means served pages are
    /// stale (metrics bake into layout server-side) — refetch, keep focus.
    metrics_fp: u64,
    /// The minute the dock's clock last showed — it regenerates when this
    /// turns over, and stays quiet in between.
    dock_minute: Option<u32>,
    /// Glass mode (runtime sidecar): frost the desktop behind the whole
    /// window and let the page show it through.
    glass: bool,
    /// How this window is drawn — chrome opacity, radius, bar heights, frost.
    /// Live: the theme watcher replaces it and the next frame uses it.
    look: theme::WindowStyle,
    /// Style inspector (RILL_TRACE): the legend file mapping trace colours
    /// back to style names, the last frame's commands to hit-test against,
    /// and the name currently under the cursor.
    trace: Option<TraceInspector>,
    last_theme_check: std::time::Instant,
    shift_held: bool,
    pages: Vec<Document>,
    page: usize,
    state: Vec<ActionValue>,
    size: (u32, u32),
    cursor: Option<(f32, f32)>,
    pressing: bool,
    /// Last frame's commands — the local hit-test surface.
    commands: Vec<DrawCommand>,
    /// Document scroll offset (logical px; clamped to content in draw).
    scroll: f32,
    /// Browser-style content zoom (Ctrl+scroll): layout at width÷zoom, then
    /// scale the commands by zoom — bigger crisp type, live reflow, same
    /// window. A pure command-space transform. `zoom` eases toward
    /// `target_zoom` a step per frame, anchored at the cursor.
    zoom: f32,
    target_zoom: f32,
    ctrl_held: bool,
    alt_held: bool,
    /// Last frame's wire cost, shown live in the titlebar.
    last_bytes: usize,
    last_cmds: usize,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    seat: Option<wl_seat::WlSeat>,
    cursor_shape: Option<CursorShapeManager>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    enter_serial: u32,
    shape: Shape,
    frame_pending: bool,
    dirty: bool,
    /// When the last frame was actually drawn — the configure handler's guard
    /// against a lost frame callback wedging resize forever.
    last_draw: Option<std::time::Instant>,
    /// The Wayland socket would not take more last flush. Production stops —
    /// no frames, no image attaches — until it drains; input keeps flowing.
    /// This is the backpressure the transport never had: without it, every
    /// queued fd-carrying request held a dup'd fd in libwayland's fixed-size
    /// outgoing ring, and a stalled compositor turned that ring into a crash.
    tx_congested: bool,
    /// A press landed on the close glyph; the close fires on a release that
    /// is still on it, and dies quietly on a release anywhere else.
    close_armed: bool,
    /// The clipboard offer we currently hold, and the text behind it. Kept
    /// until the compositor cancels it (someone else copied) — the source
    /// must outlive the copy, because paste requests arrive whenever the
    /// pasting app pleases.
    copy_source: Option<smithay_client_toolkit::data_device_manager::data_source::CopyPasteSource>,
    copy_text: String,
    /// The most recent input serial — what set_selection must present to
    /// prove the copy came from an interaction, not thin air.
    last_serial: u32,
    exit: bool,
}

impl App {
    fn load_page(&mut self, index: usize) {
        if index < self.pages.len() {
            self.page = index;
            self.state = self.pages[index].states.iter().map(|s| s.initial.clone()).collect();
            self.scroll = 0.0;
        }
    }

    /// How tall this window's titlebar is. A document that supplies its own
    /// chrome is putting controls up there, not just a label. The dock is
    /// chromeless — the strip *is* the content.
    fn bar_h(&self) -> f32 {
        if self.dock.is_some() {
            return 0.0;
        }
        match self.view.as_ref().is_some_and(|v| v.has_chrome()) {
            true => self.look.titlebar_tall,
            false => self.look.titlebar,
        }
    }

    /// Titlebar colours for this window. An app draws its own chrome — a
    /// sidebar, a toolbar — from the theme's `chrome` token; the titlebar is
    /// the same strip of the same window, so it takes its colour from the
    /// same place. That is what makes the bar and the app's chrome read as
    /// one surface instead of two panels that happen to be adjacent — and
    /// `chrome` is translucent, so the whole of it frosts what is behind the
    /// window rather than the bar alone.
    fn chrome_palette(&self) -> (Color, Color, Color) {
        let Some(theme) = self.view.as_ref().map(|v| v.theme()) else {
            return (CHROME_BG, CHROME_TEXT, CHROME_DIM);
        };
        (
            theme.token("chrome").or_else(|| theme.token("surface")).unwrap_or(CHROME_BG),
            theme.token("text").unwrap_or(CHROME_TEXT),
            theme.token("text-muted").unwrap_or(CHROME_DIM),
        )
    }

    /// Layout the current page and ship it as a stream frame: titlebar chrome
    /// plus the document, offset below it — the chrome is just more commands.
    /// Give the compositor the pixels behind any image the page shows and it
    /// has not been sent yet.
    ///
    /// Once per source, not per frame: a command stream is kilobytes and the
    /// point is that it stays kilobytes. Re-sent only when the same source
    /// comes back at a different size, which is the cheap half of "did this
    /// picture change" — the expensive half would be hashing every image
    /// every frame to answer a question that is almost always no.
    ///
    /// A failure here is a placeholder box, not a lost window: the frame
    /// referring to the image is already valid, and the compositor draws the
    /// placeholder for anything it has no pixels for.
    fn send_images(&mut self, images: &ReadyImages) {
        if images.is_empty() {
            return;
        }
        // A v1 compositor has no attach_image, and sending it anyway is a
        // protocol error — which kills the connection, i.e. the window. It
        // draws placeholder boxes instead, which is what it did before images
        // had a transport at all.
        if self.stream.version() < 2 {
            return;
        }
        let settled = self.view.as_ref().is_none_or(|v| v.shape_settled());
        let plan: Vec<&str> = images
            .iter()
            .filter(|(source, image)| {
                plan_image_send(
                    self.sent_images.get(*source).copied(),
                    (image.width, image.height),
                    images.provisional(source),
                    settled,
                )
            })
            .map(|(source, _)| source)
            .collect();
        for source in plan {
            let Some(image) = images.image(source) else { continue };
            let size = (image.width, image.height);
            let pixels: &[u8] = &image.rgba;
            debug_assert_eq!(pixels.len(), (image.width as usize) * (image.height as usize) * 4);
            let fd = match memfd_frame(pixels) {
                Ok(fd) => fd,
                Err(e) => {
                    eprintln!("rill-vector: cannot send image {source:?}: {e}");
                    continue;
                }
            };
            self.stream.attach_image(
                fd.as_fd(),
                pixels.len() as u32,
                image.width,
                image.height,
                source.to_string(),
            );
            self.sent_images.insert(source.to_string(), size);
        }
    }

    fn draw(&mut self, qh: &QueueHandle<App>) {
        // A socket that would not take the last flush will not take this
        // frame either. Producing anyway is how the outgoing fd ring filled;
        // the frame is not lost, just deferred — `dirty` re-draws the moment
        // the flush succeeds.
        if self.tx_congested {
            self.dirty = true;
            return;
        }
        let bar = self.bar_h();
        let (w, h) = self.size;
        if w == 0 || h == 0 {
            return;
        }
        let (wf, hf) = (w as f32, h as f32);
        let doc_h = hf - bar;
        // Ease toward the target zoom, one step per frame, keeping the
        // document point under the cursor stationary (browser-style anchor).
        if (self.zoom - self.target_zoom).abs() > 0.0005 {
            let old = self.zoom;
            self.zoom += (self.target_zoom - self.zoom) * 0.35;
            if (self.zoom - self.target_zoom).abs() < 0.0005 {
                self.zoom = self.target_zoom;
            }
            if let Some((_, cy)) = self.cursor {
                let doc_y = (cy - bar + self.scroll) / old;
                self.scroll = doc_y * self.zoom - (cy - bar);
            }
        }
        let zoom = self.zoom;
        let mut view_animating = false;
        // Filled by app mode; sent after the borrow of `view` ends.
        let mut page_images: Option<ReadyImages> = None;
        let (doc_commands, doc_offset_y, base_title) = if let Some(rp) = &self.replay {
            // The replay composes at the window's own size; zoom scales the
            // whole stage like any other content.
            let commands = rp.draw(wf / zoom, doc_h / zoom);
            (rill_ui::stream::scale_commands(&commands, zoom), bar, rp.title())
        } else if let Some(dash) = &self.dashboard {
            // Charts are geometry, not document flow: the dashboard draws
            // itself at the content size. Zoom still applies as a pure
            // command-space scale, so the whole thing stays crisp.
            let commands = dash.draw(wf / zoom, doc_h / zoom);
            (
                rill_ui::stream::scale_commands(&commands, zoom),
                bar,
                "Rill — System".to_string(),
            )
        } else if let Some(view) = &mut self.view {
            // App mode: the full AppView engine renders — zoom and scroll are
            // its own; commands come out already zoomed. poll() steps smooth
            // scrolling and applies finished loads; while it reports busy we
            // keep frame callbacks coming, mirroring the gpui host's
            // animation-frame pumping.
            // Only a *changed* picture keeps frames coming. Outstanding work
            // (a fetch in flight) used to land here too, and drove a commit
            // per loop iteration for content that had not arrived yet.
            view_animating = view.poll().changed;
            view.set_zoom(zoom);
            let mut measurer = EngineMeasurer(&self.engine);
            let (cmds, images, _hint) =
                view.layout(Rect { x: 0.0, y: 0.0, w: wf, h: doc_h }, &mut measurer);
            // Sent below, once the borrow of `view` ends.
            page_images = Some(images);
            (cmds, bar - view.scroll_offset(), format!("Rill — {}", view.title()))
        } else {
            let tree = resolve(&self.pages[self.page], Defaults::default());
            let mut measurer = EngineMeasurer(&self.engine);
            // The document sees a cursor in its own (unzoomed, unscrolled)
            // coordinates below the bar.
            let doc_cursor =
                self.cursor.map(|(x, y)| (x / zoom, (y - bar + self.scroll) / zoom));
            // Zoom = layout narrower, paint bigger: the document reflows at
            // the effective width, and the scaled commands re-rasterize
            // crisp. The layout size quantizes to whole logical pixels —
            // during the eased zoom the width moves in integer steps, so
            // borderline line wraps flip once per boundary instead of
            // dithering on float noise.
            let (cmds, content_h) = layout_document(
                &tree,
                LayoutOptions {
                    viewport_width: (wf / zoom).round(),
                    viewport_height: Some((doc_h / zoom).round()),
                },
                &mut measurer,
                &mut NoImages,
                &self.state,
                None,
                0,
                (0, 0),
                doc_cursor,
                self.pressing,
            );
            let cmds = rill_ui::stream::scale_commands(&cmds, zoom);
            // Clamp scroll to the actual (scaled) content overflow.
            self.scroll = self.scroll.clamp(0.0, (content_h * zoom - doc_h).max(0.0));
            (cmds, bar - self.scroll, "Rill — vector".to_string())
        };

        // Hand the compositor any pixels it does not have yet. The frame
        // names images by source and never carries them, so a window stays
        // kilobytes; this is the other door — once per source, not once per
        // frame. See `attach_image` in rill-stream-v1.xml for why the client
        // is the party that resolves them.
        if let Some(images) = page_images {
            self.send_images(&images);
        }

        // Glass mode: one frost pane behind the whole window, a chrome-owned
        // rounded glass fill in the page's own color (the scrolling page bg
        // is zeroed — rounding a scrolling rect smeared corners), and a
        // specular sheen across the top — glossy glass.
        // The dock is chromeless: no glass pane, no bar fill, no title, no
        // close — its document fills the strip and the compositor frosts it.
        // No frame: only the dock draws its whole surface itself, because it
        // *is* the strip. A widget wears a titlebar like any other window —
        // it is a window, one the theme happens to have placed. It also
        // costs nothing to draw: `bar_h` already reserved the strip for
        // every non-dock window, so a chromeless widget was leaving that
        // space blank rather than saving it.
        let chromeless = self.dock.is_some();
        // The dock says what it is made of; every other window is glass if
        // the desktop is. `none` and `solid` both mean no frost and no body
        // tint — `none` paints nothing at all (its document declares a clear
        // page), `solid` leaves the page colour opaque behind the strip.
        let glass = match self.dock_style.map(|s| s.background) {
            Some(dock::DockBackground::Glass) | None => self.glass,
            Some(_) => false,
        };
        // The dock spans an edge of the screen rather than floating in the
        // middle of it, so its corners are its own to choose — square by
        // default, because a strip against an edge with rounded ends reads
        // as a mistake rather than a decision.
        let glass_radius = match self.dock_style {
            Some(style) => style.corner,
            None => self.look.radius,
        };
        let mut doc_commands = doc_commands;
        let mut page_color = None;
        if glass {
            // Chromeless windows need this as much as framed ones: the dock
            // was given a frost and then painted its own opaque page colour
            // straight over it, so the strip was the one solid slab on a
            // glass desktop — exactly the thing the frost was added to fix.
            page_color = glass_page_background(&mut doc_commands, wf, doc_h);
        }
        // Frost is a property of the *window*, not of one strip in it. It
        // used to be the titlebar alone when glass was off, which is what made
        // a bar and a sidebar painted from the same token refuse to look like
        // the same surface: one sat on frosted desktop, the other on an opaque
        // page. A glass window frosts entirely; a plain one does not frost.
        // Frost is a property of the *window*, and chromeless is not
        // glassless: the dock's strip is the same furniture as a window's
        // titlebar, so it gets the same two layers — frost, then the glass
        // body — and therefore reads as the same material.
        let mut commands = Vec::new();
        if glass {
            commands.push(DrawCommand::Backdrop {
                rect: Rect { x: 0.0, y: 0.0, w: wf, h: hf },
                blur: self.look.blur,
                corner_radius: glass_radius,
            });
            // The glass body: the page color, translucent, fixed to the
            // window (content scrolls inside it). Light-handed — chrome is
            // painted *over* this, so whatever opacity is spent here is
            // opacity the titlebar and the sidebar can never get back. A page
            // that wants to be solid says so itself.
            let mut fill = page_color.unwrap_or(Color { r: 0x12, g: 0x14, b: 0x2a, a: 0xFF });
            fill.a = fill.a.min(self.look.glass_body_alpha);
            commands.push(DrawCommand::Rect {
                rect: Rect { x: 0.0, y: 0.0, w: wf, h: hf },
                color: fill,
                corner_radius: glass_radius,
            });
        }
        let (chrome_bg, chrome_text, chrome_dim) = self.chrome_palette();
        // A document that claims the bar wants its toolbar and its sidebar to
        // be one material — which they can only be if the *window body* is
        // that material and neither strip paints its own. So the host adds no
        // bar fill of its own: glass already covers the window, and a plain
        // window gets the page colour behind the strip instead of a chrome
        // panel. A bare bar (no document chrome) keeps the classic fill.
        let has_doc_chrome = self.view.as_ref().is_some_and(|v| v.has_chrome());
        let bar_fill = match (chromeless, has_doc_chrome, glass) {
            (true, ..) => None,
            (false, true, true) => None,
            (false, true, false) => Some(
                self.view
                    .as_ref()
                    .map(|v| v.theme().page_background)
                    .unwrap_or(chrome_bg),
            ),
            (false, false, _) => Some(chrome_bg),
        };
        if let Some(color) = bar_fill {
            commands.push(DrawCommand::Rect {
                // +1: overlap under the document so the shared edge can't
                // show an AA seam (the doc paints over the overlap). In glass
                // mode the bar's top corners follow the pane radius.
                rect: Rect { x: 0.0, y: 0.0, w: wf, h: bar + 1.0 },
                color,
                corner_radius: if glass { self.look.radius } else { 0.0 },
            });
        }
        // The strip is the app's when the app asked for it: the document's own
        // toolbar is laid out here, in window coordinates and unzoomed, so it
        // stays put while the page scales. The close glyph keeps its corner.
        let chrome_doc = match &mut self.view {
            _ if chromeless => None,
            Some(view) if view.has_chrome() => {
                // The whole strip: Close is the document's toolbar member
                // (navigate "/~close"), not a host-reserved corner — that is
                // what lets it share an edge with the app's own controls.
                let rect = Rect { x: 0.0, y: 0.0, w: wf, h: bar };
                let cursor = self.cursor.filter(|(_, y)| *y < bar);
                Some(view.layout_chrome(rect, cursor, &mut EngineMeasurer(&self.engine)))
            }
            _ => None,
        };
        match chrome_doc {
            _ if chromeless => {}
            Some(cmds) => commands.extend(cmds),
            // A widget's bar is a drag handle, not a label: the strip (and
            // its close glyph) stay, the title text goes. What a widget is
            // showing already says what it is.
            None if self.widget => {}
            None => commands.push(DrawCommand::Text {
                rect: Rect { x: 12.0, y: (bar - 18.0) / 2.0, w: wf - 60.0, h: 18.0 },
                text: if (zoom - 1.0).abs() < 0.01 {
                    base_title.clone()
                } else {
                    format!("{base_title}  ·  {:.0}%", zoom * 100.0)
                },
                color: chrome_text,
                font_size: 13.0,
                font_weight: 600,
                font_family: "sans-serif".into(),
            }),
        }
        // The host draws a close control only when no document claimed the
        // bar — a claimed bar carries its own, as a toolbar member.
        if !has_doc_chrome && !chromeless {
            // The glyph alone, on whatever the titlebar already is. A filled
            // swatch behind it read as a button pasted onto the bar rather
            // than a control belonging to it — and the bar is frosted, so a
            // raised rect punched an opaque hole in the one surface that is
            // supposed to show the desktop through. The click target is
            // `CLOSE_WIDTH` either way; it never depended on this rect.
            commands.push(DrawCommand::Text {
                rect: Rect { x: wf - 24.0, y: (bar - 20.0) / 2.0, w: 16.0, h: 18.0 },
                text: "×".into(),
                color: chrome_dim,
                font_size: 15.0,
                font_weight: 400,
                font_family: "sans-serif".into(),
            });
        }
        if wire_cost_visible() {
            // Live wire cost: the whole visible frame, counted in kilobytes.
            commands.push(DrawCommand::Text {
                rect: Rect { x: wf - 190.0, y: 10.0, w: 150.0, h: 14.0 },
                text: format!(
                    "{:.1} KB · {} cmds",
                    self.last_bytes as f32 / 1024.0,
                    self.last_cmds
                ),
                color: chrome_dim,
                font_size: 11.0,
                font_weight: 400,
                font_family: "monospace".into(),
            });
        }
        // Gloss is for a pane that floats: a sheen off the top edge and a
        // shaded bottom read as thickness. The dock is flush against an edge
        // of the screen, where the same two marks read as a seam.
        if glass && !chromeless {
            // Specular sheen: a soft white glow sweeping the top edge —
            // the gloss on the glass.
            commands.push(DrawCommand::Shadow {
                rect: Rect { x: -wf * 0.05, y: -26.0, w: wf * 0.72, h: 30.0 },
                color: Color { r: 255, g: 255, b: 255, a: 26 },
                blur: 34.0,
                spread: 0.0,
                corner_radius: 16.0,
            });
            // A shaded bottom edge grounds the pane (the top edge is lit by
            // the sheen alone — a hard highlight line read as an artifact).
            commands.push(DrawCommand::Rect {
                rect: Rect {
                    x: self.look.radius,
                    y: hf - 1.3,
                    w: wf - 2.0 * self.look.radius,
                    h: 1.3,
                },
                color: Color { r: 0, g: 0, b: 0, a: 90 },
                corner_radius: 0.0,
            });
        }
        // Style inspector: name the surface under the cursor, in the slot
        // the wire cost vacated. The frame is captured *before* this text so
        // the readout never inspects itself.
        if let Some(name) = self.trace.as_ref().and_then(|t| t.under_cursor.clone()) {
            commands.push(DrawCommand::Text {
                rect: Rect { x: wf - 260.0, y: (bar - 14.0) / 2.0, w: 216.0, h: 14.0 },
                text: name,
                color: chrome_text,
                font_size: 11.0,
                font_weight: 600,
                font_family: "monospace".into(),
            });
        }

        // The document scrolls inside a clip below the bar; the offset is a
        // pure command-space translation. The dock is the exception again:
        // it never scrolls, and its menu escapes the strip upward — a clip
        // here would crush client-side what the compositor already allows.
        if chromeless {
            commands.extend(rill_ui::stream::offset_commands(&doc_commands, 0.0, doc_offset_y));
        } else {
            commands.push(DrawCommand::PushClip {
                rect: Rect { x: 0.0, y: bar, w: wf, h: doc_h },
                radius: 0.0,
            });
            commands.extend(rill_ui::stream::offset_commands(&doc_commands, 0.0, doc_offset_y));
            commands.push(DrawCommand::PopClip);
        }
        if let Some(tr) = &mut self.trace {
            tr.frame = commands.clone();
        }
        // Skipping a frame beats taking the window down. `memfd_create` and
        // the write into it can fail under fd or memory pressure, and encode
        // can refuse a frame that overruns the stream cap — none of which is
        // worth losing the session over, least of all in the dock, which is
        // the desktop's launcher. The window redraws on the next change.
        let bytes = match rill_ui::stream::encode(&commands) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("rill-vector: dropping a frame that would not encode: {e}");
                return;
            }
        };
        self.last_bytes = bytes.len();
        self.last_cmds = commands.len();
        let fd = match memfd_frame(&bytes) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("rill-vector: dropping a frame (memfd): {e}");
                return;
            }
        };
        // The tier the shown document declared, sent before the frame it
        // classifies ("latched with the next attach" — specs/history.md
        // decision 4). Fail closed on a compositor that predates set_tier:
        // attaching anyway would record a sensitive page at T0, the exact
        // failure the declaration exists to prevent. The window shows
        // nothing new rather than showing it misclassified.
        let tier = self.view.as_ref().map_or(0, |v| v.tier());
        if tier != self.sent_tier {
            if self.stream.version() < 3 && tier > 0 {
                eprintln!(
                    "rill-vector: this compositor cannot classify recordings                      (needs rill_stream_v1 v3); refusing to show a tier-{tier}                      document at tier 0"
                );
                return;
            }
            if self.stream.version() >= 3 {
                self.stream.set_tier(tier as u32);
            }
            self.sent_tier = tier;
        }
        self.stream.attach(fd.as_fd(), bytes.len() as u32, w, h);

        let surface = self.window.wl_surface();
        self.window.set_window_geometry(0, 0, w, h);
        surface.frame(qh, surface.clone());
        surface.commit();
        self.commands = commands;
        self.frame_pending = true;
        self.last_draw = Some(std::time::Instant::now());
        // Keep animating until zoom and the view's own motion settle (frame
        // callbacks re-enter draw).
        self.dirty = view_animating || (self.zoom - self.target_zoom).abs() > 0.0005;
    }

    fn request_redraw(&mut self, qh: &QueueHandle<App>) {
        if self.frame_pending {
            self.dirty = true;
        } else {
            self.draw(qh);
        }
    }

    /// Advance app-mode background work: async loads, live theme, timers.
    /// Called every loop tick (~100ms) and cheap when idle.
    fn tick(&mut self, qh: &QueueHandle<App>) {
        // The dashboard is its own clock: re-read /proc on its interval and
        // redraw only when a fresh sample actually landed, so an idle window
        // still costs nothing between samples.
        if let Some(rp) = &mut self.replay {
            // Playback is its own clock; a paused or finished replay reports
            // no change and the window goes quiet.
            if rp.advance() {
                self.request_redraw(qh);
            }
            return;
        }
        if let Some(dash) = &mut self.dashboard {
            if dash.sample() {
                self.request_redraw(qh);
            }
            return;
        }
        let Some(view) = &mut self.view else { return };
        // Broker prompts need a trusted host overlay; the vector host doesn't
        // have one yet — decline rather than hang the document.
        if view.has_capability() {
            eprintln!("rill-vector: capability prompts not supported yet — cancelling");
            view.cancel_capability();
        }
        let polled = view.poll();
        if polled.changed {
            self.request_redraw(qh);
        }
        // Nothing to paint for outstanding work — but shorten the loop's wait
        // so its completion is noticed promptly. Painting was how that used to
        // happen, at the cost of a full composite per check.
        self.view_pending = polled.pending;

        // The dock's clock: the strip is a document, so a new minute is a new
        // document — regenerated only when the minute actually turns.
        if let Some(dock) = &mut self.dock {
            let now = dock.clock_minute();
            if now.is_some() && now != self.dock_minute {
                self.dock_minute = now;
                // A minute is also a fine cadence for collecting children that
                // have exited since the last one.
                dock.reap();
                let bytes = dock.document();
                if let Some(view) = &mut self.view {
                    view.reload_keep_focus(Source::Generated {
                        label: "dock".into(),
                        bytes,
                    });
                }
                self.request_redraw(qh);
            }
        }

        // Live theming: poll the theme file's mtime (300ms, like the compositor).
        if self.last_theme_check.elapsed() > std::time::Duration::from_millis(300) {
            self.last_theme_check = std::time::Instant::now();
            if let Some((theme_path, last)) = &mut self.theme_state {
                // One file: theme.toml carries everything — colors, window
                // style, glass, enforce. The sidecar is retired.
                let now = theme::stamp(theme_path);
                if now != *last {
                    *last = now;
                    let desktop = theme::load(theme_path);
                    self.glass = desktop.glass;
                    self.look = desktop.window.clone();
                    self.metrics_fp = desktop.metrics_fingerprint;
                    // Everything derived from the theme is now stale, and
                    // "derived" is broader than colour: a served page may be
                    // *built* from theme.toml (the studio's own pages are),
                    // and the dock's strip certainly is — its layout and
                    // clock live there. Re-resolving tokens only fixes the
                    // half a client can see. So: re-skin, then re-serve.
                    // Widgets are theme too: a studio edit to
                    // [[desktop.widgets]] means processes to spawn or kill,
                    // not just colours to re-resolve.
                    if let Some(dock) = &mut self.dock {
                        dock.sync_widgets();
                    }
                    self.dock_style = self.dock.as_ref().map(|d| d.style());
                    let dock_doc = self.dock.as_ref().map(|d| d.document());
                    if let Some(view) = &mut self.view {
                        view.set_theme(desktop.defaults.clone());
                        match dock_doc {
                            // The dock's document is generated here, so a
                            // reload has to carry the *new* bytes.
                            Some(bytes) => view.reload_keep_focus(Source::Generated {
                                label: "dock".into(),
                                bytes,
                            }),
                            None => {
                                let source = view.current().clone();
                                view.reload_keep_focus(source);
                            }
                        }
                    }
                    self.request_redraw(qh);
                }
            }
        }
    }

    /// Route an activated link (app and dock modes).
    fn follow(&mut self, target: &str) {
        // Dock links are desktop verbs, not navigation: launch spawns a
        // process, the toggles write theme.toml / the runtime sidecar. After
        // a handled verb the dock document is regenerated so labels and
        // colours reflect the new state, keeping focus on the control used.
        if let Some(dock) = &mut self.dock {
            if dock.follow(target) {
                let defaults = dock.themed_defaults();
                let bytes = dock.document();
                if let Some(view) = &mut self.view {
                    view.set_theme(defaults);
                    view.reload_keep_focus(Source::Generated { label: "dock".into(), bytes });
                }
            }
            return;
        }
        if target == "/~close" {
            self.exit = true;
            return;
        }
        let Some(view) = &mut self.view else { return };
        if let Some(key) = target.strip_prefix("/~launch/") {
            if let Some(dir) = &self.data_dir
                && let Ok(source) = launch_source(dir, key)
            {
                view.open(source);
            }
        } else {
            view.navigate(target);
        }
    }

    /// The cursor shape that fits what's under the pointer.
    fn desired_shape(&self, x: f32, y: f32) -> Shape {
        let bar = self.bar_h();
        if let Some(edge) = self.edge_at(x, y) {
            return match edge {
                ResizeEdge::Top => Shape::NResize,
                ResizeEdge::Bottom => Shape::SResize,
                ResizeEdge::Left => Shape::WResize,
                ResizeEdge::Right => Shape::EResize,
                ResizeEdge::TopLeft => Shape::NwResize,
                ResizeEdge::TopRight => Shape::NeResize,
                ResizeEdge::BottomLeft => Shape::SwResize,
                ResizeEdge::BottomRight => Shape::SeResize,
                _ => Shape::Default,
            };
        }
        if y < bar {
            let has_doc_chrome = self.view.as_ref().is_some_and(|v| v.has_chrome());
            if !has_doc_chrome && x > self.size.0 as f32 - CLOSE_WIDTH {
                return Shape::Pointer;
            }
            return match self.view.as_ref().map(|v| v.chrome_hint(x, y)) {
                Some(CursorHint::Pointer) => Shape::Pointer,
                Some(CursorHint::Text) => Shape::Text,
                _ => Shape::Default,
            };
        }
        if let Some(view) = &self.view {
            return match view.hint_at_local(x, y - bar) {
                CursorHint::Pointer => Shape::Pointer,
                CursorHint::Text => Shape::Text,
                CursorHint::Default => Shape::Default,
            };
        }
        if self.link_under(x, y).is_some() {
            return Shape::Pointer;
        }
        Shape::Default
    }

    fn update_cursor(&mut self, x: f32, y: f32) {
        let want = self.desired_shape(x, y);
        if want != self.shape
            && let Some(device) = &self.cursor_device
        {
            device.set_shape(self.enter_serial, want);
            self.shape = want;
        }
    }

    /// Which resize edge (if any) a point in the window falls on. The dock
    /// has none — the compositor pins and sizes the strip.
    fn edge_at(&self, x: f32, y: f32) -> Option<ResizeEdge> {
        if self.dock.is_some() {
            return None;
        }
        let (w, h) = (self.size.0 as f32, self.size.1 as f32);
        let (left, right) = (x < EDGE, x > w - EDGE);
        let (top, bottom) = (y < EDGE, y > h - EDGE);
        Some(match (top, bottom, left, right) {
            (true, _, true, _) => ResizeEdge::TopLeft,
            (true, _, _, true) => ResizeEdge::TopRight,
            (_, true, true, _) => ResizeEdge::BottomLeft,
            (_, true, _, true) => ResizeEdge::BottomRight,
            (true, ..) => ResizeEdge::Top,
            (_, true, ..) => ResizeEdge::Bottom,
            (_, _, true, _) => ResizeEdge::Left,
            (_, _, _, true) => ResizeEdge::Right,
            _ => return None,
        })
    }

    /// Hit-test the last frame for a link under the pointer.
    fn link_under(&self, x: f32, y: f32) -> Option<String> {
        self.commands.iter().rev().find_map(|command| match command {
            DrawCommand::LinkArea { rect, target }
                if x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h =>
            {
                Some(target.clone())
            }
            _ => None,
        })
    }
}

fn main() {
    // The dev trail sees every panic with its location — the event that
    // used to vanish into whatever terminal the desktop happened to be
    // launched from.
    if rill_log::dev_active() {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            rill_log::dev_emit("rill-vector", "panic", &[("info", &info.to_string())]);
            default_hook(info);
        }));
        rill_log::dev_emit("rill-vector", "start", &[]);
    }
    // Modes: `--app KEY` hosts a full AppView (a real installed Rill app,
    // vector-native); `--doc file.rill` renders a compiled document;
    // `--dashboard` is the live system monitor; none of them show the
    // built-in demo pages.
    let mut pages = Vec::new();
    let mut want_dashboard = false;
    let mut want_dock = false;
    let mut replay_path: Option<PathBuf> = None;
    let mut app_key: Option<String> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut identity_dir: Option<PathBuf> = None;
    let mut cache_dir: Option<PathBuf> = None;
    let mut theme_path: Option<PathBuf> = None;
    let mut pick_root: Option<PathBuf> = None;
    let mut no_cache = false;
    let mut widget_source: Option<String> = None;
    let mut widget_place: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--doc" => {
                if let Some(path) = args.next() {
                    let bytes = std::fs::read(&path).expect("read --doc file");
                    pages.push(rill_doc::decode(&bytes).expect("decode --doc file"));
                }
            }
            "--dashboard" => want_dashboard = true,
            "--dock" => want_dock = true,
            // A widget is a chromeless client the compositor parks on the
            // desktop: same engine, same documents, no titlebar and no
            // place in the window stack.
            "--widget" => widget_source = args.next(),
            // Where it sits, as the compositor will read it back off the
            // app id: `<anchor>:<w>x<h>+<x>+<y>`.
            "--widget-place" => widget_place = args.next(),
            "--replay" => replay_path = args.next().map(PathBuf::from),
            "--app" => app_key = args.next(),
            "--data" => data_dir = args.next().map(PathBuf::from),
            "--identity" => identity_dir = args.next().map(PathBuf::from),
            "--cache" => cache_dir = args.next().map(PathBuf::from),
            "--theme" => theme_path = args.next().map(PathBuf::from),
            // Kept by the dock (the broker's scoped share root, forwarded to
            // launched apps); the plain vector host has no broker overlay
            // yet and ignores it.
            "--pick-root" => pick_root = args.next().map(PathBuf::from),
            "--no-cache" => no_cache = true,
            other => eprintln!("rill-vector: ignoring argument {other}"),
        }
    }
    if no_cache {
        cache_dir = None;
    }
    if pages.is_empty() {
        pages = vec![compile_page(PAGE_ONE), compile_page(PAGE_TWO), compile_page(PAGE_THREE)];
    }

    // App mode: the full engine — fetcher (own tokio runtime), theme, state.
    let mut theme_state = None;
    let mut initial_glass = false;
    let mut initial_metrics_fp = 0u64;
    let mut window_style = theme::WindowStyle::default();
    let mut view = app_key.map(|key| {
        let data = data_dir.clone().expect("--app needs --data");
        let identity = identity_dir.clone().expect("--app needs --identity");
        let fetcher =
            Fetcher::new(identity, cache_dir.clone(), data.clone()).expect("fetcher");
        let source = launch_source(&data, &key).expect("app not installed");
        let desktop = theme_path
            .as_deref()
            .map(theme::load)
            .unwrap_or_else(theme::builtin_dark);
        initial_glass = desktop.glass;
        initial_metrics_fp = desktop.metrics_fingerprint;
        if let Some(path) = &theme_path {
            theme_state = Some((path.clone(), theme::stamp(path)));
        }
        window_style = desktop.window.clone();
        let mut view = AppView::new(fetcher, source);
        view.set_theme(desktop.defaults.clone());
        view
    });

    // Widget mode: an ordinary served document, hosted chromeless. The only
    // difference from an app window is that nothing frames it and the
    // compositor parks it rather than stacking it.
    let widget_view = widget_source.as_deref().map(|src| {
        let data = data_dir.clone().unwrap_or_else(rill_app::default_data_dir);
        let identity = identity_dir
            .clone()
            .unwrap_or_else(rill_client::util::default_identity_dir);
        let cache = if no_cache {
            None
        } else {
            cache_dir.clone().or_else(|| Some(rill_client::util::default_cache_dir()))
        };
        let resolved_theme = theme_path.clone().unwrap_or_else(theme::default_path);
        let desktop = theme::load(&resolved_theme);
        initial_glass = desktop.glass;
        initial_metrics_fp = desktop.metrics_fingerprint;
        window_style = desktop.window.clone();
        theme_state = Some((resolved_theme.clone(), theme::stamp(&resolved_theme)));
        let fetcher = Fetcher::new(identity, cache, data.clone()).expect("fetcher");
        // A widget is addressed the way anything else is: a rill:// URL, or
        // the key of an installed app.
        let source = match src.starts_with("rill://") {
            true => {
                let url = rill_client::RillUrl::parse(src).expect("--widget url");
                Source::Remote { host: url.host, port: url.port, path: url.path }
            }
            false => launch_source(&data, src).expect("widget app not installed"),
        };
        let mut v = AppView::new(fetcher, source);
        v.set_theme(desktop.defaults.clone());
        v
    });

    // Dock mode: the launcher strip as a generated document. Defaults mirror
    // the retired gpui shell so `rill-vector --dock` works bare: identity and
    // cache fall back to the standard directories, the theme to
    // ~/.config/rill/theme.toml.
    let dock_state = want_dock.then(|| {
        let data = data_dir.clone().unwrap_or_else(rill_app::default_data_dir);
        let identity = identity_dir
            .clone()
            .unwrap_or_else(rill_client::util::default_identity_dir);
        let cache = if no_cache {
            None
        } else {
            cache_dir.clone().or_else(|| Some(rill_client::util::default_cache_dir()))
        };
        let resolved_theme = theme_path.clone().unwrap_or_else(theme::default_path);
        let desktop = theme::load(&resolved_theme);
        // The dock frosts with the rest of the desktop, so it needs the glass
        // flag at startup too — not only after the first theme change.
        initial_glass = desktop.glass;
        let dock = dock::Dock::new(
            data.clone(),
            resolved_theme.clone(),
            identity.clone(),
            cache.clone(),
            pick_root.clone(),
        );
        window_style = desktop.window.clone();
        theme_state = Some((resolved_theme.clone(), theme::stamp(&resolved_theme)));
        let fetcher = Fetcher::new(identity, cache, data.clone()).expect("fetcher");
        let mut v = AppView::new(
            fetcher,
            Source::Generated { label: "dock".into(), bytes: dock.document() },
        );
        // The strip is shorter than any menu; the compositor lets the dock's
        // stream escape its bounds and routes input by painted extent — so
        // the app menu simply drops down from the mark.
        v.set_menu_unbounded(true);
        v.set_theme(desktop.defaults.clone());
        (dock, v)
    });
    let dock_obj = match dock_state {
        Some((mut d, v)) => {
            // The desktop's widgets come up with the dock: same identity,
            // same data directory, same theme — which is exactly why the
            // dock is the one that starts them.
            d.spawn_widgets();
            view = Some(v);
            Some(d)
        }
        None => None,
    };

    let replay = replay_path.map(|path| match replay::Replay::open(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rill-vector: {e}");
            std::process::exit(1);
        }
    });

    let conn = Connection::connect_to_env().expect("connect to wayland");
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn).expect("registry");
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg_wm_base");
    // 1..=3: a v1 compositor still works, it just cannot be sent images —
    // the frame names them and the placeholder box is what it draws, which is
    // exactly the behaviour before images had a transport at all. v3 adds
    // set_tier; a document that *declares* a tier refuses to attach frames
    // to a compositor below that (fail closed — see the draw path).
    let stream_manager: RillStreamManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .expect("rill_stream_manager_v1 — is this a Rill compositor?");

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestClient, &qh);
    // The xdg title is what the compositor's WM (and a session recording)
    // sees, so it has to name the mode, not the binary.
    window.set_title(match (&replay, want_dashboard, want_dock) {
        (Some(r), ..) => r.title(),
        (None, true, _) => "Rill — System".to_string(),
        (None, _, true) => "Rill — Shell".to_string(),
        (None, false, false) => "Rill — vector".to_string(),
    });
    // The dock's app id is the compositor's cue to pin the strip to the
    // bottom edge and keep it out of the app-window stack.
    // The app id is the compositor's cue to a surface's role. A widget
    // carries its placement in the tag — `rill-shell-widget#<anchor>:<w>x<h>
    // +<x>+<y>` — because the compositor has to park it before the document
    // it is showing has even arrived, and a role tag is the one thing it can
    // read that early. (Layer-shell is the real answer; this is the same
    // stopgap the wallpaper and the dock already use.)
    // The source URL rides along after the placement, because a widget the
    // user drags has to find its own `[[desktop.widgets]]` entry to write the
    // new position back to, and `app = ` is what identifies that entry. Two
    // widgets can share a size and a corner; they cannot share a URL.
    let app_id = match (want_dock, &widget_place, widget_source.is_some()) {
        (true, ..) => dock::DOCK_APP_ID.to_string(),
        (_, Some(place), _) => match &widget_source {
            Some(src) => format!("{}#{place}#{src}", dock::WIDGET_APP_ID),
            None => format!("{}#{place}", dock::WIDGET_APP_ID),
        },
        (_, None, true) => dock::WIDGET_APP_ID.to_string(),
        _ => "rill-vector".to_string(),
    };
    window.set_app_id(app_id);
    let stream = stream_manager.get_stream(window.wl_surface(), &qh, ());
    window.commit();

    let view = view.or(widget_view);
    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        // A compositor without wl_data_device_manager simply has no
        // clipboard; paste is then a no-op rather than a failure to start.
        data_device_manager: DataDeviceManagerState::bind(&globals, &qh).ok(),
        data_device: None,
        output_state: OutputState::new(&globals, &qh),
        window,
        stream,
        sent_images: HashMap::new(),
        sent_tier: 0,
        engine: TextEngine::new(),
        view,
        dashboard: want_dashboard.then(dashboard::Dashboard::new),
        dock_style: dock_obj.as_ref().map(|d| d.style()),
        dock: dock_obj,
        widget: widget_source.is_some() || widget_place.is_some(),
        replay,
        view_pending: false,
        data_dir,
        theme_state,
        metrics_fp: initial_metrics_fp,
        dock_minute: None,
        glass: initial_glass,
        look: window_style,
        trace: std::env::var_os("RILL_TRACE").map(|p| TraceInspector {
            legend_path: p.into(),
            legend: Default::default(),
            legend_mtime: None,
            frame: Vec::new(),
            under_cursor: None,
        }),
        last_theme_check: std::time::Instant::now(),
        shift_held: false,
        pages,
        page: 0,
        state: Vec::new(),
        size: (0, 0),
        cursor: None,
        pressing: false,
        commands: Vec::new(),
        scroll: 0.0,
        zoom: 1.0,
        target_zoom: 1.0,
        ctrl_held: false,
        alt_held: false,
        last_bytes: 0,
        last_cmds: 0,
        pointer: None,
        keyboard: None,
        seat: None,
        cursor_shape: CursorShapeManager::bind(&globals, &qh).ok(),
        cursor_device: None,
        enter_serial: 0,
        shape: Shape::Default,
        frame_pending: false,
        dirty: false,
        last_draw: None,
        tx_congested: false,
        close_armed: false,
        copy_source: None,
        copy_text: String::new(),
        last_serial: 0,
        exit: false,
    };
    // The dock takes whatever strip the compositor configures; a min size
    // would fight the pin.
    if app.dock.is_none() {
        app.window.set_min_size(Some((240, 160)));
    }
    app.load_page(0);

    // Poll-based loop: wayland events wake us immediately; a ~100ms timeout
    // ticks app-mode background work (async loads, live theme) even when the
    // window is quiet — the vector twin of gpui's animation-frame pumping.
    //
    // Every step here can fail once the compositor goes away, and a dead socket
    // does not throttle: poll() reports POLLHUP immediately, so the 100ms
    // timeout stops applying and the loop free-runs. Discarding these errors
    // therefore turned a compositor exit into a spin that logged a
    // broken-pipe line per iteration — megabytes a second, until the disk
    // filled. A fatal error now ends the loop: the compositor is gone, so is
    // the window.
    let mut lost = None;
    while !app.exit {
        // Flush, with backpressure. A full socket (WouldBlock) is not an
        // error and not permission to keep producing: it means the compositor
        // has fallen behind, every queued fd-carrying request is holding a
        // dup'd fd in libwayland's fixed-size outgoing ring, and the ring
        // running out is a dead window. So congestion stops frame and image
        // production (draw() defers), the poll below starts watching for
        // writability, and the deferred frame goes out when the pipe drains.
        match event_queue.flush() {
            Ok(()) => {
                if app.tx_congested {
                    app.tx_congested = false;
                    if app.dirty {
                        let qh = event_queue.handle();
                        app.request_redraw(&qh);
                    }
                }
            }
            Err(e) => {
                let would_block = matches!(
                    &e,
                    WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock
                );
                if would_block {
                    app.tx_congested = true;
                } else if let Some(e) = fatal(e) {
                    lost = Some(e);
                    break;
                }
            }
        }
        if let Some(guard) = event_queue.prepare_read() {
            let mut pfd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                // Congested: also wake when the socket can take more, since
                // that — not input — is what unblocks the deferred frame.
                events: libc::POLLIN
                    | if app.tx_congested { libc::POLLOUT } else { 0 },
                revents: 0,
            };
            // A self-reloading page sets the pace: waiting the usual 100ms
            // would cap every live document at 10Hz no matter what it asked
            // for. Floored at 10ms so a fast page cannot spin the loop.
            // Sleep until the page's clock is actually due, not on a cadence
            // near it. A fixed cadence cannot hit an arbitrary interval — an
            // 80ms page woken every 40ms fires at 80 or at 120 depending on
            // phase, and the fetch that resets the phase makes it drift; that
            // measured 10.6 ticks/s for a page asking 12.5. Halving the wait
            // did not help, because the problem was alignment rather than
            // granularity.
            let wait = app
                .view
                .as_ref()
                .and_then(|v| v.next_tick_in())
                .map(|d| (d.as_millis() as i32).clamp(1, 100))
                .unwrap_or(100);
            // A fetch in flight wants a prompt look, not a repaint: come back
            // in 8ms to check rather than sleeping out the interval. Without
            // this the frame-callback pumping that used to poll for us is
            // gone and a navigation would appear up to 100ms late.
            let wait = if app.view_pending { wait.min(8) } else { wait };
            let ready = unsafe { libc::poll(&mut pfd, 1, wait) };
            if ready > 0 {
                if let Some(e) = guard.read().err().and_then(fatal) {
                    lost = Some(e);
                    break;
                }
            } else {
                drop(guard);
            }
        }
        if let Err(e) = event_queue.dispatch_pending(&mut app) {
            match e {
                DispatchError::Backend(e) => {
                    if let Some(e) = fatal(e) {
                        lost = Some(e);
                        break;
                    }
                }
                e => {
                    lost = Some(e.to_string());
                    break;
                }
            }
        }
        let qh = event_queue.handle();
        app.tick(&qh);
    }
    if let Some(e) = lost {
        eprintln!("rill-vector: wayland connection lost: {e}");
    }
    // The page's declared goodbye (`closing target=`): fired on the way out,
    // bounded so a gone server cannot hold the window's exit hostage. The
    // app's own idle timeout stays the safety net for the paths that never
    // get here (kill, crash, compositor loss above).
    if let Some(view) = &mut app.view {
        view.say_goodbye(std::time::Duration::from_millis(500));
    }
    // Ticks against the compositor's commit count: if these disagree, frames
    // are being produced and not delivered, which is a different bug from a
    // clock that is not firing.
    if let Some(view) = &app.view {
        eprintln!("rill-vector: applied_loads={}", view.applied_loads());
    }
}

/// Classify a wayland connection error: `Some(message)` if the connection is
/// finished, `None` if it is ordinary back-pressure worth retrying.
/// `WouldBlock`/`Interrupted` happen routinely on a busy socket; a broken pipe
/// or a protocol error does not heal.
fn fatal(e: WaylandError) -> Option<String> {
    match &e {
        WaylandError::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            None
        }
        _ => Some(e.to_string()),
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let (w, h) = match configure.new_size {
            (Some(w), Some(h)) => (w.get(), h.get()),
            // Our preferred size when the compositor defers. Charts need
            // vertical room to read as curves rather than flat lines, so the
            // dashboard asks for more than a page of text does.
            _ if self.dashboard.is_some() => (760, 600),
            _ => (560, 420),
        };
        self.size = (w, h);
        // Reflow at the new size — at the *frame callback's* pace, not the
        // configure stream's. An interactive resize delivers a configure per
        // pointer motion, and drawing each one unconditionally meant a fast
        // mouse produced hundreds of full layout+encode+memfd frames a
        // second: more fd-bearing requests than the socket could drain, which
        // is the storm that filled libwayland's outgoing fd ring and killed
        // the window ("can't send file descriptor"). Drawing on the callback
        // uses the latest size and skips the sizes in between, which is all a
        // resize ever needed.
        //
        // The callback-lost guard matters here: a first configure (nothing
        // committed yet, no callback in flight) must draw immediately or the
        // window never maps.
        if !self.frame_pending || self.last_draw.is_none_or(|t| t.elapsed().as_millis() > 100) {
            self.frame_pending = false;
            self.draw(qh);
        } else {
            self.dirty = true;
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.frame_pending = false;
        if self.dirty {
            self.draw(qh);
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if self.data_device.is_none()
            && let Some(manager) = &self.data_device_manager
        {
            self.data_device = Some(manager.get_data_device(qh, &seat));
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
            if let (Some(manager), Some(pointer)) = (&self.cursor_shape, &self.pointer) {
                self.cursor_device = Some(manager.get_shape_device(pointer, qh));
            }
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
        self.seat = Some(seat);
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointer = None;
        }
        if capability == Capability::Keyboard {
            self.keyboard = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        let bar = self.bar_h();
        for event in events {
            let (x, y) = (event.position.0 as f32, event.position.1 as f32);
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    self.enter_serial = serial;
                    self.shape = Shape::Default;
                    self.cursor = Some((x, y));
                    if let Some(view) = &mut self.view {
                        view.set_cursor(x, y - bar);
                    }
                    self.update_cursor(x, y);
                    self.request_redraw(qh);
                }
                PointerEventKind::Motion { .. } => {
                    self.cursor = Some((x, y));
                    if let Some(view) = &mut self.view {
                        view.set_cursor(x, y - bar);
                        if self.pressing {
                            view.on_drag(x, y - bar, &mut EngineMeasurer(&self.engine));
                        }
                    }
                    self.update_cursor(x, y);
                    if let Some(tr) = &mut self.trace {
                        tr.reload_if_changed();
                        let hit = tr.lookup(x, y);
                        if hit != tr.under_cursor {
                            tr.under_cursor = hit;
                        }
                    }
                    self.request_redraw(qh);
                }
                PointerEventKind::Leave { .. } => {
                    self.cursor = None;
                    if let Some(view) = &mut self.view {
                        view.clear_cursor();
                    }
                    self.request_redraw(qh);
                }
                PointerEventKind::Press { serial, button, .. } => {
                    self.last_serial = serial;
                    // Right button: the context invocation. The document's
                    // declared menu (innermost under the point) opens,
                    // presented by the viewport — same presenter every app.
                    const BTN_RIGHT: u32 = 0x111;
                    // The dock is chromeless and its menu escapes upward:
                    // every coordinate, negative y included, is document.
                    let chromeless = self.dock.is_some();
                    if button == BTN_RIGHT {
                        if (chromeless || y >= bar) && let Some(view) = &mut self.view {
                            view.context_click(x, y - bar);
                            self.request_redraw(qh);
                        }
                        continue;
                    }
                    // Window chrome first: edges resize, the bar moves, the
                    // corner glyph closes. Everything else is document input.
                    if let (Some(seat), Some(edge)) = (self.seat.clone(), self.edge_at(x, y)) {
                        self.window.resize(&seat, serial, edge);
                        continue;
                    }
                    if y < bar && !chromeless {
                        let has_doc_chrome =
                            self.view.as_ref().is_some_and(|v| v.has_chrome());
                        if !has_doc_chrome && x > self.size.0 as f32 - CLOSE_WIDTH {
                            // Arm, don't fire: close needs the *release* on
                            // the glyph too. The glyph shares its strip with
                            // the drag handle — on a 300px widget it is 13%
                            // of it — and closing on press turned a slightly
                            // misaimed drag into a dead window, repeatedly.
                            // Press-and-release-on-target is what every
                            // button everywhere means; a drag that starts
                            // here now just doesn't move the window, which
                            // is the cheap half of the mistake.
                            self.close_armed = true;
                            continue;
                        }
                        // A control the document put up there wins; bare bar
                        // still drags the window.
                        let hit = match &mut self.view {
                            Some(view) => {
                                view.chrome_click(x, y, &mut EngineMeasurer(&self.engine))
                            }
                            None => ClickResult::Miss,
                        };
                        match hit {
                            ClickResult::Link(target) => self.follow(&target),
                            ClickResult::Consumed => {}
                            ClickResult::Miss => {
                                if let Some(seat) = self.seat.clone() {
                                    self.window.move_(&seat, serial);
                                }
                            }
                        }
                        self.request_redraw(qh);
                        continue;
                    }
                    self.pressing = true;
                    // Click-to-scrub. The bar's rect is in content coords
                    // (below the titlebar) and unaffected by zoom, since the
                    // replay lays out at window size.
                    if let Some(rp) = &mut self.replay {
                        let (cw, ch) = (self.size.0 as f32, self.size.1 as f32 - bar);
                        let scrub = rp.scrub_rect(cw, ch);
                        let cy = y - bar;
                        // Generous vertical target: the track itself is 10px.
                        if cy >= scrub.y - 8.0 && cy <= scrub.y + scrub.h + 8.0 && scrub.w > 0.0 {
                            rp.seek_fraction((x - scrub.x) / scrub.w);
                        } else {
                            rp.toggle();
                        }
                        self.request_redraw(qh);
                        continue;
                    }
                    if let Some(view) = &mut self.view {
                        view.set_pressing(true);
                        let result =
                            view.on_click(x, y - bar, &mut EngineMeasurer(&self.engine));
                        if let ClickResult::Link(target) = result {
                            self.follow(&target);
                        }
                    }
                    self.request_redraw(qh);
                }
                PointerEventKind::Release { .. } => {
                    // The armed close fires only if the release is still on
                    // the glyph; letting go anywhere else cancels it.
                    if std::mem::take(&mut self.close_armed) {
                        let bar = self.bar_h();
                        if y < bar && x > self.size.0 as f32 - CLOSE_WIDTH {
                            self.exit = true;
                        }
                        continue;
                    }
                    self.pressing = false;
                    if let Some(view) = &mut self.view {
                        view.set_pressing(false);
                    } else if let Some(target) = self.link_under(x, y) {
                        // Built-in demo navigation: /page/N switches pages.
                        if let Some(n) = target.strip_prefix("/page/")
                            && let Ok(n) = n.parse::<usize>()
                        {
                            self.load_page(n);
                        } else {
                            println!("rill-vector: link {target}");
                        }
                    }
                    self.request_redraw(qh);
                }
                PointerEventKind::Axis { vertical, .. } => {
                    let delta = if vertical.absolute != 0.0 {
                        vertical.absolute as f32
                    } else {
                        vertical.discrete as f32 * 15.0
                    };
                    if delta == 0.0 {
                        continue;
                    }
                    if self.ctrl_held {
                        // Ctrl+scroll: content zoom (up = in), ~3% per notch,
                        // eased toward in draw() and anchored at the cursor.
                        let factor = 1.03f32.powf(-delta / 15.0);
                        self.target_zoom = (self.target_zoom * factor).clamp(0.5, 3.0);
                    } else if self.view.is_some() {
                        // Routed through the cursor: an independent scroll
                        // region under the pointer takes the wheel; the page
                        // takes what regions leave. (scroll_by's sign: down
                        // = negative.)
                        let bar = self.bar_h();
                        if let Some(view) = &mut self.view {
                            view.scroll_at(x, y - bar, -delta * SCROLL_SPEED);
                        }
                    } else {
                        // Clamped against content in draw.
                        self.scroll += delta * SCROLL_SPEED;
                    }
                    self.request_redraw(qh);
                }
            }
        }
    }
}

impl App {
    /// Offer `text` as the clipboard selection. The source stays alive in
    /// `copy_source` until the compositor cancels it; `send_request` streams
    /// the bytes to whoever pastes, however many times they do.
    fn copy_to_clipboard(&mut self, text: String, qh: &QueueHandle<App>) {
        let (Some(manager), Some(device)) = (&self.data_device_manager, &self.data_device) else {
            // A compositor without a clipboard: the copy quietly has nowhere
            // to go, same as paste already behaves.
            return;
        };
        let source = manager.create_copy_paste_source(
            qh,
            ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"],
        );
        source.set_selection(device, self.last_serial);
        self.copy_text = text;
        self.copy_source = Some(source);
    }

    /// The clipboard's current contents as text, if there are any.
    ///
    /// Read on demand rather than cached on every selection change: the
    /// clipboard is usually someone else's, often large, and a client that
    /// slurps every copy anyone makes is a client that reads a gigabyte
    /// because a file manager put one on the clipboard.
    fn clipboard_text(&self, conn: &Connection) -> Option<String> {
        use std::io::Read;
        // Pasting our own copy: answer from memory. Going over the wire
        // would deadlock — the receive pipe is filled by the source, the
        // source is this same single-threaded process, and it cannot answer
        // a request it is blocked waiting on. Copy-then-paste in one window
        // hung for the full timeout and produced nothing.
        if self.copy_source.is_some() {
            return Some(self.copy_text.clone());
        }
        let offer = self.data_device.as_ref()?.data().selection_offer()?;
        // Ask for UTF-8 first, then the older spelling. A source that offers
        // neither is not text, and paste is simply not for it.
        let mut pipe = ["text/plain;charset=utf-8", "text/plain"]
            .iter()
            .find_map(|mime| offer.receive(mime.to_string()).ok())?;
        // The request has to reach the source before it will write anything,
        // and the source is another client — without this the read blocks on
        // a pipe nobody has been told to fill.
        let _ = conn.flush();

        // Bounded twice over, in size and in time. A paste is a line or two,
        // and the clipboard is arbitrary data from another program: unbounded
        // in size it is a memory bomb someone else can trigger, and unbounded
        // in time it is worse — this runs on the single-threaded event loop,
        // so a source that opens the pipe and never writes to it freezes the
        // window for good. Neither bound is reachable by an honest source.
        const MAX_PASTE: u64 = 1 << 20;
        const PASTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

        let deadline = std::time::Instant::now() + PASTE_TIMEOUT;
        let fd = pipe.as_raw_fd();
        // SAFETY: setting O_NONBLOCK on a pipe fd we own for the duration of
        // this function.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }

        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break, // source closed its end — the whole paste
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() as u64 >= MAX_PASTE {
                        buf.truncate(MAX_PASTE as usize);
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        eprintln!("rill-vector: clipboard source stopped writing; pasting what arrived");
                        break;
                    }
                    let mut pfd =
                        libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                    // SAFETY: one initialized pollfd, described by the count.
                    unsafe { libc::poll(&mut pfd, 1, left.as_millis() as libc::c_int) };
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
        String::from_utf8(buf).ok().filter(|t| !t.is_empty())
    }

    /// Deliver pasted text to whatever holds the keyboard.
    ///
    /// It rides the ordinary text path — the same one an ordinary keystroke
    /// takes — so the terminal writes it to the pty and a focused input
    /// inserts it, with no paste-specific plumbing on either side. Carriage
    /// returns are normalised to newlines because that is what a shell
    /// expects to read.
    fn paste(&mut self, text: &str, qh: &QueueHandle<Self>) {
        let Some(view) = self.view.as_mut() else { return };
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        // Named "paste", not typed as itself: a capturing page (the
        // terminal) needs to know this is one paste and not keystrokes, so
        // it can frame it under bracketed-paste mode. For an ordinary
        // focused input the text field is what matters and inserts whole,
        // exactly as it did when the key *was* the text.
        view.on_key("paste", Some(&text), false, false, false);
        self.request_redraw(qh);
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        // Ctrl+Shift+V pastes. Terminals have always needed the Shift,
        // because Ctrl+V is a control code a shell is entitled to receive.
        // Taken before the document sees it: a page holding the keyboard
        // would otherwise swallow it as an ordinary key.
        if self.ctrl_held
            && self.shift_held
            && matches!(event.keysym, Keysym::v | Keysym::V)
        {
            if let Some(text) = self.clipboard_text(conn) {
                self.paste(&text, qh);
            }
            return;
        }
        // Plain Ctrl+C/X/V, for the focused input only. The gate is the
        // input itself: a terminal page has no focusable inputs, so its
        // Ctrl+C stays an interrupt and its Ctrl+V stays a byte — the
        // conventions do not collide because they never meet.
        if self.ctrl_held && !self.shift_held {
            match event.keysym {
                Keysym::c | Keysym::C => {
                    if let Some(text) =
                        self.view.as_ref().and_then(|v| v.focused_input_selection())
                    {
                        self.copy_to_clipboard(text, qh);
                        return;
                    }
                }
                Keysym::x | Keysym::X => {
                    if let Some(text) =
                        self.view.as_ref().and_then(|v| v.focused_input_selection())
                    {
                        self.copy_to_clipboard(text, qh);
                        // Cut is copy plus delete: backspace over a
                        // selection deletes exactly the selection.
                        if let Some(view) = self.view.as_mut() {
                            view.on_key("backspace", None, false, false, false);
                        }
                        self.request_redraw(qh);
                        return;
                    }
                }
                Keysym::v | Keysym::V
                    if self.view.as_ref().is_some_and(|v| v.has_focused_input()) =>
                {
                    if let Some(text) = self.clipboard_text(conn) {
                        self.paste(&text, qh);
                    }
                    return;
                }
                _ => {}
            }
        }
        // Ctrl+Shift+C: the selection to the clipboard — the terminal
        // convention, and harmless everywhere else. The viewport owns what
        // is selected; this owns getting it onto the wire.
        if self.ctrl_held
            && self.shift_held
            && matches!(event.keysym, Keysym::c | Keysym::C)
        {
            let text = self
                .view
                .as_ref()
                .and_then(|v| v.selection_text(&mut EngineMeasurer(&self.engine)));
            if let Some(text) = text {
                self.copy_to_clipboard(text, qh);
            }
            return;
        }
        // Replay transport. Handled before anything else so playback keys
        // never reach a document underneath.
        if let Some(rp) = &mut self.replay {
            match event.keysym {
                Keysym::space => rp.toggle(),
                Keysym::r | Keysym::R => rp.restart(),
                Keysym::Left => rp.seek(-5_000),
                Keysym::Right => rp.seek(5_000),
                Keysym::Home => rp.seek(i64::MIN / 2),
                Keysym::End => rp.seek(i64::MAX / 2),
                _ => return,
            }
            self.request_redraw(qh);
            return;
        }
        if self.view.is_some() {
            // App mode: translate to AppView's key vocabulary (the gpui
            // keystroke names) and forward.
            let raw = event.keysym.raw();
            let (key, text): (String, Option<String>) = match event.keysym {
                Keysym::Left => ("left".into(), None),
                Keysym::Right => ("right".into(), None),
                Keysym::Up => ("up".into(), None),
                Keysym::Down => ("down".into(), None),
                Keysym::Home => ("home".into(), None),
                Keysym::End => ("end".into(), None),
                Keysym::BackSpace => ("backspace".into(), None),
                Keysym::Delete => ("delete".into(), None),
                Keysym::Return | Keysym::KP_Enter => ("enter".into(), None),
                Keysym::Tab => ("tab".into(), None),
                Keysym::Escape => ("escape".into(), None),
                // A page that has taken the keyboard needs the keys a form
                // never asked for: paging, function rows, insert. Naming them
                // always is harmless — a document that declared no binding
                // for "f7" simply ignores it.
                Keysym::Page_Up => ("pageup".into(), None),
                Keysym::Page_Down => ("pagedown".into(), None),
                Keysym::Insert => ("insert".into(), None),
                Keysym::F1 => ("f1".into(), None),
                Keysym::F2 => ("f2".into(), None),
                Keysym::F3 => ("f3".into(), None),
                Keysym::F4 => ("f4".into(), None),
                Keysym::F5 => ("f5".into(), None),
                Keysym::F6 => ("f6".into(), None),
                Keysym::F7 => ("f7".into(), None),
                Keysym::F8 => ("f8".into(), None),
                Keysym::F9 => ("f9".into(), None),
                Keysym::F10 => ("f10".into(), None),
                Keysym::F11 => ("f11".into(), None),
                Keysym::F12 => ("f12".into(), None),
                _ => {
                    if self.ctrl_held && (0x61..=0x7a).contains(&raw) {
                        // Ctrl+letter: utf8 would be a control code; name the
                        // letter instead so shortcuts match the gpui host.
                        (char::from(raw as u8).to_string(), None)
                    } else if let Some(t) = event.utf8.as_ref().filter(|t| {
                        !t.is_empty() && t.chars().all(|c| !c.is_control())
                    }) {
                        (t.clone(), Some(t.clone()))
                    } else {
                        return;
                    }
                }
            };
            let result = self.view.as_mut().unwrap().on_key(
                &key,
                text.as_deref(),
                self.ctrl_held,
                self.shift_held,
                self.alt_held,
            );
            if let KeyResult::Link(target) = result {
                self.follow(&target);
            }
            self.request_redraw(qh);
            return;
        }
        if event.keysym == Keysym::Escape {
            self.exit = true;
        }
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _layout: u32,
    ) {
        self.ctrl_held = modifiers.ctrl;
        self.alt_held = modifiers.alt;
        self.shift_held = modifiers.shift;
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// The stream protocol has no events; these dispatches exist so the proxies
// can be created against our state type.
impl Dispatch<RillStreamManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &RillStreamManagerV1,
        event: <RillStreamManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {}
    }
}

impl Dispatch<RillStreamV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &RillStreamV1,
        event: <RillStreamV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // The compositor needed the memory and dropped a picture this
            // window is not currently showing. Forgetting that we sent it is
            // the whole of the response: the next frame that names it will
            // send it again, because `send_images` only skips what it
            // believes the compositor still has.
            stream_protocol::rill_stream_v1::Event::ImageReleased { source } => {
                app.sent_images.remove(&source);
            }
        }
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_seat!(App);
delegate_pointer!(App);
delegate_keyboard!(App);
// (cursor-shape dispatches ride along with delegate_pointer!)
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
// Clipboard. Every callback here is deliberately empty: paste reads the
// current selection at the moment the key is pressed (see `paste`), so
// there is no state to keep as offers come and go, and nothing is ever
// offered *by* this client because nothing in a vector window is selectable
// yet. Drag-and-drop is likewise unimplemented rather than half-implemented.
impl DataDeviceHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_device::WlDataDevice,
        _: f64,
        _: f64,
        _: &wl_surface::WlSurface,
    ) {
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice) {}
    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_device::WlDataDevice,
        _: f64,
        _: f64,
    ) {
    }
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_device::WlDataDevice,
    ) {
    }
    fn drop_performed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_device::WlDataDevice,
    ) {
    }
}

impl DataOfferHandler for App {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }
    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }
}

impl DataSourceHandler for App {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_source::WlDataSource,
        _: Option<String>,
    ) {
    }
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &wl_data_source::WlDataSource,
        _mime: String,
        mut pipe: smithay_client_toolkit::data_device_manager::WritePipe,
    ) {
        // Our copy offer being pasted somewhere. Every advertised mime is
        // plain text, so the payload is the same whichever was picked. A
        // broken pipe means the paster went away mid-read — their loss,
        // not an error of ours.
        if self.copy_source.as_ref().is_some_and(|s| s.inner() == source) {
            use std::io::Write as _;
            let _ = pipe.write_all(self.copy_text.as_bytes());
        }
    }
    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &wl_data_source::WlDataSource,
    ) {
        // Someone else owns the clipboard now; release ours.
        if self.copy_source.as_ref().is_some_and(|s| s.inner() == source) {
            self.copy_source = None;
            self.copy_text.clear();
        }
    }
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource) {}
    fn action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_source::WlDataSource,
        _: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }
}

smithay_client_toolkit::delegate_data_device!(App);
delegate_registry!(App);

#[cfg(test)]
mod image_send_tests {
    use super::plan_image_send;

    /// A picture the compositor lacks goes immediately, storm or no storm —
    /// deferring it is a placeholder box on screen, which is the one outcome
    /// worse than traffic.
    #[test]
    fn a_missing_picture_always_goes() {
        assert!(plan_image_send(None, (800, 600), false, true));
        assert!(plan_image_send(None, (800, 600), false, false));
        assert!(plan_image_send(None, (100, 75), true, false));
    }

    #[test]
    fn a_picture_already_there_at_this_size_does_not_go_again() {
        assert!(!plan_image_send(Some((800, 600)), (800, 600), false, true));
    }

    /// The floor copy of a picture scrolled back into view must not overwrite
    /// the sharp copy the compositor still holds.
    #[test]
    fn a_stand_in_never_downgrades_a_sharper_copy() {
        assert!(!plan_image_send(Some((1600, 1200)), (100, 75), true, true));
        assert!(!plan_image_send(Some((1600, 1200)), (100, 75), true, false));
        // But a stand-in *better* than what is held is still an improvement.
        assert!(plan_image_send(Some((100, 75)), (400, 300), true, true));
    }

    /// The rule that keeps a resize from being a megabyte storm: size changes
    /// wait for the shape to settle, in both directions.
    #[test]
    fn a_resize_waits_for_the_shape_to_settle() {
        // Finer after a widening: waits.
        assert!(!plan_image_send(Some((800, 600)), (1600, 1200), false, false));
        assert!(plan_image_send(Some((800, 600)), (1600, 1200), false, true));
        // Coarser after a narrowing (a deliberate reduction): also waits, then
        // goes — the compositor does eventually stop holding pixels nobody
        // needs.
        assert!(!plan_image_send(Some((1600, 1200)), (800, 600), false, false));
        assert!(plan_image_send(Some((1600, 1200)), (800, 600), false, true));
    }
}
