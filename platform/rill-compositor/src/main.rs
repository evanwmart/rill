//! rill-compositor: the Rill Wayland compositor (Desktop Phase 2, milestone
//! 14). A **nested** compositor — it runs as a window inside the current
//! desktop and hosts ordinary Wayland clients *and* Rill apps (which are
//! themselves Wayland clients: `rill-view` processes). See
//! `specs/compositor.md`.
//!
//! Slices so far: 14a composites `wl_shm` clients; 14b adds `wl_output` +
//! dmabuf so gpui/Vulkan clients (rill-view) render; 14c adds a real seat —
//! pointer motion/button/axis with focus-under-cursor and keyboard with
//! proper serials/timestamps — plus a `Space` for window tracking and
//! interactive move/resize grabs.
//!
//! **W3 (specs/wgpu-renderer.md D2): the render path is wgpu.** Smithay
//! provides the Wayland *frontend* (protocols, surfaces, seat, xdg-shell);
//! the window is raw winit (smithay's own reexport) with input translated by
//! hand, because an EGL context on the window would claim the host's one
//! DRM-syncobj slot and lock Vulkan's WSI out. Client buffers import through
//! `rill_gpu::dmabuf` (dmabufs, cached per wl_buffer) or `write_texture`
//! (shm, per frame); `rill_gpu::Renderer::composite` stacks wallpaper-clear,
//! window textures bottom→top, and the focus-border overlay into the
//! swapchain, vsync-paced.
//!
//! ```bash
//! rill-compositor                 # spawns a vector-native dashboard window
//! rill-compositor foot            # spawn a different client
//! rill-compositor /path/rill-view --app KEY --data DIR
//! ```

use std::os::unix::io::OwnedFd;
mod audio;
mod history_writer;
mod recorder;
mod stream_protocol;

use std::collections::HashMap;
use std::sync::Arc;

use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::{DataInit, Dispatch, GlobalDispatch, New};
use stream_protocol::rill_stream_manager_v1::{self, RillStreamManagerV1};
use stream_protocol::rill_stream_v1::{self, RillStreamV1};

use rill_gpu::dmabuf::{DmabufDevice, DmabufPlan};
use rill_gpu::{Renderer as GpuRenderer, SceneLayer};
use rill_ui::{Color as UiColor, DrawCommand, Point as UiPoint, Rect as UiRect};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer as _, Format as DrmFormat, Fourcc, Modifier};
use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::backend::renderer::utils::{RendererSurfaceStateUserData, on_commit_buffer_handler};
use smithay::desktop::space::RenderZindex;
use smithay::desktop::{
    PopupKind, PopupManager, Space, Window, WindowSurfaceType, find_popup_root_surface,
};
use smithay::input::keyboard::{FilterResult, Keycode, Keysym};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorImageStatus, Focus, GestureHoldBeginEvent, GestureHoldEndEvent,
    GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
    GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData, MotionEvent, PointerGrab,
    PointerHandle, PointerInnerHandle, RelativeMotionEvent,
};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{
    wl_buffer, wl_seat, wl_shm, wl_surface::WlSurface,
};
use smithay::reexports::wayland_server::{
    Client, Display, DisplayHandle, ListeningSocket, Resource,
};
use smithay::reexports::winit::application::ApplicationHandler;
use smithay::reexports::winit::dpi::LogicalSize;
use smithay::reexports::winit::event::{
    ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent,
};
use smithay::reexports::winit::event_loop::{ActiveEventLoop, EventLoop};
use smithay::reexports::winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
use smithay::reexports::winit::platform::scancode::PhysicalKeyExtScancode;
use smithay::reexports::winit::window::{CursorIcon, Fullscreen, Window as WinitWindow, WindowId};
use smithay::utils::{Logical, Physical, Point, Rectangle, SERIAL_COUNTER, Serial, Size, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, SubsurfaceCachedState,
    SurfaceAttributes, TraversalAction, with_states, with_surface_tree_downward,
    with_surface_tree_upward,
};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{
    DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
    set_data_device_focus,
};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::SurfaceCachedState;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgPopupSurfaceData, XdgShellHandler,
    XdgShellState, XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState, with_buffer_contents};
use smithay::{
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_dmabuf,
    delegate_output, delegate_seat, delegate_shm, delegate_xdg_decoration, delegate_xdg_shell,
};

/// Shell surfaces are ordinary toplevels tagged by app_id, special-cased into
/// desktop roles (a stopgap for real layer-shell background/panel semantics).
const SHELL_BACKGROUND_APP_ID: &str = "rill-shell-background";
const SHELL_DOCK_APP_ID: &str = "rill-shell-dock";
/// Desktop widgets: parked below every window, never focused, placed by the
/// anchor in their own app id (`rill-shell-widget#<anchor>:<w>x<h>+<x>+<y>`).
const SHELL_WIDGET_APP_ID: &str = "rill-shell-widget";
/// How far a per-window effect may reach beyond its own window, in logical
/// pixels. The fx layer for a window is scissored to its rect grown by this,
/// so the cost stays proportional to the windows on screen rather than to the
/// output. Generous on purpose: too small silently clips a flame, and the
/// renderer cannot know how far a given shader spills.
const WINDOW_FX_REACH: f32 = 256.0;

/// Height reserved at the *top* for the dock when the theme says nothing.
/// This is the appkit *region* tier at default metrics — line(F14 × 1.4) +
/// 2P control + 2P region = 43.6, ceiled — duplicated as data because the
/// compositor sizes the strip before the dock's document arrives.
///
/// `[desktop.dock] height` overrides it. The compositor has to own this
/// number even though the dock draws the strip: it is the compositor that
/// reserves the space and keeps every other window out of it.
const DOCK_HEIGHT: i32 = 44;

/// Where a widget sits: a corner (or the middle) of the usable desktop, a
/// size, and a margin from that corner.
#[derive(Clone, Copy, PartialEq, Debug)]
struct WidgetPlace {
    anchor: Anchor,
    w: i32,
    h: i32,
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Anchor {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Center,
}

impl WidgetPlace {
    /// `<anchor>:<w>x<h>+<x>+<y>`, e.g. `top-right:320x140+16+16`. Read off
    /// the app id, so it has to survive being written by hand.
    fn parse(spec: &str) -> Option<WidgetPlace> {
        let (anchor, rest) = spec.split_once(':')?;
        let anchor = match anchor {
            "top-left" => Anchor::TopLeft,
            "top-right" => Anchor::TopRight,
            "bottom-left" => Anchor::BottomLeft,
            "bottom-right" => Anchor::BottomRight,
            "center" | "centre" => Anchor::Center,
            _ => return None,
        };
        let (size, offset) = rest.split_once('+')?;
        let (w, h) = size.split_once('x')?;
        let (x, y) = offset.split_once('+')?;
        Some(WidgetPlace {
            anchor,
            w: w.parse::<i32>().ok()?.clamp(16, 4000),
            h: h.parse::<i32>().ok()?.clamp(16, 4000),
            x: x.parse::<i32>().ok()?.clamp(0, 4000),
            y: y.parse::<i32>().ok()?.clamp(0, 4000),
        })
    }

    /// Top-left corner within an output, given the space the dock has
    /// already claimed. Anchored rather than absolute so a widget stays in
    /// its corner when the resolution changes.
    fn origin(&self, output: Size<i32, Logical>, top: i32) -> Point<i32, Logical> {
        let right = (output.w - self.w - self.x).max(0);
        let bottom = (output.h - self.h - self.y).max(top);
        let (x, y) = match self.anchor {
            Anchor::TopLeft => (self.x, top + self.y),
            Anchor::TopRight => (right, top + self.y),
            Anchor::BottomLeft => (self.x, bottom),
            Anchor::BottomRight => (right, bottom),
            Anchor::Center => (
                ((output.w - self.w) / 2).max(0),
                (top + (output.h - top - self.h) / 2).max(top),
            ),
        };
        Point::from((x, y))
    }

    /// The inverse of [`WidgetPlace::origin`]: the placement that would put
    /// this widget's top-left at `at`.
    ///
    /// The anchor is re-chosen from which quadrant the widget's centre lands
    /// in, rather than kept. That is the point of anchoring: drop a widget
    /// near the bottom-right and it should still hug the bottom-right when
    /// the resolution changes, not sit at a stale absolute offset. It also
    /// gives `center` somewhere to go — a centred widget that has been
    /// dragged is, by definition, no longer centred.
    fn placed_at(
        &self,
        at: Point<i32, Logical>,
        output: Size<i32, Logical>,
        top: i32,
    ) -> WidgetPlace {
        let centre_x = at.x + self.w / 2;
        let centre_y = at.y + self.h / 2;
        let right = centre_x * 2 >= output.w;
        let bottom = (centre_y - top) * 2 >= (output.h - top).max(1);
        let anchor = match (right, bottom) {
            (false, false) => Anchor::TopLeft,
            (true, false) => Anchor::TopRight,
            (false, true) => Anchor::BottomLeft,
            (true, true) => Anchor::BottomRight,
        };
        WidgetPlace {
            anchor,
            w: self.w,
            h: self.h,
            // Margins are measured from the anchored edge, so they stay
            // non-negative and mean the same thing `origin` reads them as.
            x: if right { (output.w - self.w - at.x).max(0) } else { at.x.max(0) },
            y: if bottom { (output.h - self.h - at.y).max(0) } else { (at.y - top).max(0) },
        }
    }
}

impl Anchor {
    /// The spelling `WidgetPlace::parse` and `theme.toml` both use.
    fn name(self) -> &'static str {
        match self {
            Anchor::TopLeft => "top-left",
            Anchor::TopRight => "top-right",
            Anchor::BottomLeft => "bottom-left",
            Anchor::BottomRight => "bottom-right",
            Anchor::Center => "center",
        }
    }
}

/// A desktop widget: a window the theme placed.
///
/// Widgets are ordinary windows in every respect that matters to a person —
/// they focus, raise, drag and close like anything else. What makes one a
/// widget is only that the theme said where it starts and how big it is,
/// and that dragging it writes that decision back.
struct DesktopWidget {
    window: Window,
    place: WidgetPlace,
    /// The `app = ` URL from its `[[desktop.widgets]]` entry. This is the
    /// identity that survives a restart, so it is what a moved widget writes
    /// its new position against.
    app: String,
}

struct Rill {
    /// Desktop widgets and where each one sits.
    widgets: Vec<DesktopWidget>,
    /// The strip's reserved height — `[desktop.dock] height`, live.
    dock_height: i32,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    dmabuf_state: DmabufState,
    #[allow(dead_code)] // holds the xdg-output global alive
    output_manager_state: OutputManagerState,
    #[allow(dead_code)] // holds the cursor-shape global alive
    cursor_shape_state: CursorShapeManagerState,
    #[allow(dead_code)] // holds the xdg-decoration global alive
    xdg_decoration_state: XdgDecorationState,
    /// The cursor a client most recently requested (applied to the host
    /// window each frame — a nested compositor draws no cursor of its own).
    cursor_status: CursorImageStatus,
    #[allow(dead_code)] // keeps the wl_seat global alive
    seat: Seat<Self>,
    /// Mapped windows and their positions — the source of truth for both
    /// rendering and pointer hit-testing.
    space: Space<Window>,
    /// The window currently being resized (if any) and its fixed-edge anchors,
    /// so the opposite edge stays put as the client commits new sizes.
    resize_state: Option<ResizeState>,
    /// The foreign window whose resize band the pointer is in, and which
    /// edge. Native rill windows drive their own resize through the
    /// protocol; a foreign client never gets pointer events outside its
    /// rect, so the compositor owns its edges — hit-tested every motion,
    /// it turns the cursor and arms the press-to-grab.
    edge_hover: Option<(Window, xdg_toplevel::ResizeEdge)>,
    /// xdg popups (menus, dropdowns, the browser's autocomplete panel),
    /// tracked so they configure, map, render, and dismiss. A popup left
    /// unconfigured is worse than invisible: the client believes a menu is
    /// open and grabbing, and every click and key disappears into it.
    popups: PopupManager,
    /// An explicit popup grab: (the grabbing popup's surface, the toplevel
    /// to hand keyboard focus back to). Rill's focus type is a bare
    /// WlSurface, so the grab is bespoke rather than smithay's PopupGrab:
    /// keyboard follows the popup, and a press outside every popup of the
    /// chain dismisses it — same first-click-away the dock menu has.
    popup_grab: Option<(WlSurface, WlSurface)>,
    /// Logical output size (for placing the shell surfaces / usable area).
    output_size: Size<i32, Logical>,
    /// The shell's background (wallpaper) and dock surfaces, special-cased so
    /// the wallpaper stays below apps and the dock stays above them.
    background: Option<Window>,
    dock: Option<Window>,
    /// Kept so the focused client can be given data-device (clipboard) focus.
    display_handle: DisplayHandle,
    /// Vector-native window content, keyed by wl_surface id: decoded
    /// DrawCommand frames delivered via rill_stream_v1 (specs/wgpu-renderer.md
    /// W4). `pending` is staged by attach, latched into `current` on commit.
    streams: HashMap<ObjectId, StreamWindow>,
    /// Last-seen window origins + timestamp, for the fx speed channel.
    prev_window_pos: HashMap<ObjectId, (f32, f32, std::time::Instant)>,
    /// Smoothed window velocity in px/sec, for the fx direction channel. Raw
    /// frame-to-frame deltas jitter and vanish the instant a drag stops; a
    /// shader wants the push to read as a gesture, so this trails it with a
    /// short time constant and decays back to rest on its own.
    window_velocity: HashMap<ObjectId, (f32, f32)>,
    /// Whether the compositor draws the pointer itself. When it does, moving
    /// the mouse is a reason to recomposite — the host is no longer drawing
    /// a cursor over our frame.
    draw_cursor: bool,
    /// Damage flag: set whenever anything visible changed (client commit,
    /// window move/resize/raise/map, focus, reflow). The render loop only
    /// composites when set — an idle desktop renders nothing (P3).
    needs_redraw: bool,
    /// Barrel factor of the installed effect shader's distortion, if it
    /// declares one (`[desktop] warp_barrel`): pointer coordinates run
    /// through the same forward map so clicks land on what's on screen.
    pointer_warp: Option<f64>,
    /// Live stats overlay (`[desktop] hud = true`), drawn by the compositor
    /// as DrawCommands on a frosted panel.
    show_hud: bool,
    /// When each toplevel surface mapped, for the spawn scale/fade
    /// animation (`[desktop] animations`, default on).
    spawn_times: HashMap<ObjectId, std::time::Instant>,
    /// Commits carrying content since the last HUD sample.
    commit_count: u32,
    /// Lifetime count of commits that carried content, printed on the way
    /// out beside the frame count. `commit_count` above is the HUD's and is
    /// zeroed every sample, so it cannot answer the question that matters for
    /// a whole run: how many frames did we draw per change the clients made?
    total_commits: u64,
    /// The in-progress session recording, if any (Ctrl+Alt+R). The compositor
    /// is the only party that sees every window and every frame, so it is the
    /// only place a faithful recording can be made.
    recorder: Option<recorder::Recorder>,
    /// The always-on system-of-record (specs/history.md decision 1). `None`
    /// only when disabled at boot (`RILL_HISTORY=0`) — configuration, not a
    /// pause; there is no runtime off switch by design.
    history: Option<history_writer::History>,
    /// The owner's tier ratchet (`~/.config/rill/history.toml`): per-app
    /// pins and a floor, composed with document declarations by max().
    tier_policy: history_writer::TierPolicy,
    /// Surface → the id a recording knows it by. **Not** `protocol_id()`:
    /// that number is only unique within one client's connection, so two
    /// clients each numbering a surface 7 collided — the recorder's per-id
    /// map thrashed between them, re-emitting both windows on every sync
    /// (22,784 events in 150s where ~20 were warranted), and frames were
    /// attributed to whichever window last claimed the number. These ids
    /// are compositor-wide and monotonic.
    record_ids: HashMap<ObjectId, u32>,
    next_record_id: u32,
}

#[derive(Default)]
struct StreamWindow {
    pending: Option<StreamFrame>,
    current: Option<StreamFrame>,
    /// The tier the client declared for what this surface shows
    /// (`set_tier`, specs/history.md decision 4). Latched with the next
    /// attach: `declared_tier` is what the client said, `tier` is what the
    /// latched frames record at.
    declared_tier: u8,
    tier: u8,
    /// The app id behind this surface, cached at first latch — what the
    /// owner's tier policy pins by.
    app: Option<String>,
    /// Pixels for this surface's image sources, keyed by the string its
    /// frames name them with (`attach_image`).
    ///
    /// Per surface, not global: two clients naming "/logo.png" mean their
    /// own, and they may be talking to different servers. Dropped with the
    /// stream, so a closed window's textures go with it.
    images: HashMap<String, HeldImage>,
    /// Bytes those textures occupy, against [`MAX_SURFACE_IMAGE_BYTES`].
    image_bytes: usize,
    /// The sources the frame on screen names.
    ///
    /// The working set, and the thing eviction is keyed to: what is *shown*
    /// rather than what has been *seen*. A window that scrolls through a
    /// thousand pictures holds the handful it is displaying, not the
    /// thousand it has displayed.
    live_images: std::collections::HashSet<String>,
    /// Bumped per latched frame; stamps [`HeldImage::used`] so eviction can
    /// order what is not on screen by how recently it was.
    image_clock: u64,
    /// Validated pixels waiting for the render loop to upload them.
    ///
    /// Dispatch does not own the GPU device — buffer imports happen in the
    /// loop too — so `attach_image` checks the pixels and parks them here,
    /// and they become textures (and are dropped) on the next pass.
    pending_images: Vec<PendingImage>,
}

/// One attached image: its texture, what it costs, and when it was last on
/// screen.
struct HeldImage {
    bundle: TexBundle,
    bytes: usize,
    used: u64,
}

/// One surface's attached images, as the renderer's image source.
///
/// Answers with a texture rather than pixels, so a frame that names an image
/// costs a bind group — the pixels were uploaded once, when the client
/// attached them.
struct StreamImages<'a>(&'a HashMap<String, HeldImage>);

impl rill_gpu::ImageSource for StreamImages<'_> {
    fn texture(&self, source: &str) -> Option<&wgpu::TextureView> {
        self.0.get(source).map(|h| &h.bundle.view)
    }
}

/// What to release so `size` more bytes will fit, or `None` if nothing would
/// be enough.
///
/// Two rules, and the first is the one that matters. **Never release what the
/// current frame names**, so eviction cannot touch a picture on screen and
/// cannot become a release-then-re-attach loop. Beyond that, least recently
/// shown goes first, which makes the budget bound the *working set* rather
/// than the number of images a window has ever displayed.
///
/// `None` means the frame on screen alone wants more than a surface may hold
/// — a genuine refusal, where releasing more would not help.
fn plan_release(
    held_images: &[(&str, usize, u64)],
    live: &std::collections::HashSet<String>,
    held: usize,
    incoming: &str,
    size: usize,
    budget: usize,
) -> Option<Vec<String>> {
    if held + size <= budget {
        return Some(Vec::new());
    }
    let mut candidates: Vec<&(&str, usize, u64)> = held_images
        .iter()
        .filter(|(s, _, _)| *s != incoming && !live.contains(*s))
        .collect();
    candidates.sort_unstable_by_key(|(_, _, used)| *used);

    let mut freed = 0usize;
    let mut release = Vec::new();
    for (source, bytes, _) in candidates {
        if held - freed + size <= budget {
            break;
        }
        freed += bytes;
        release.push(source.to_string());
    }
    (held - freed + size <= budget).then_some(release)
}

/// One `attach_image` payload, validated and awaiting upload.
struct PendingImage {
    source: String,
    pixels: Vec<u8>,
    w: u32,
    h: u32,
}

/// How much image memory one surface may hold.
///
/// A 1080p RGBA image is 8.3 MB, and the whole idle desktop measures 28-34
/// MiB PSS on the 1 GB target — so this is a real limit, not a formality.
/// Generous enough for a page of photographs, small enough that a client
/// cannot quietly take the machine.
///
/// Reaching it releases images the current frame does not name, least
/// recently shown first, and tells the client so it can send them again
/// (`image_released`). The client is told because eviction without recall is
/// a picture that silently never returns — and the alternative, refusing at
/// the cap, is worse still: a window that browsed enough pictures would stop
/// showing new ones for the rest of its life. The budget bounds the *working
/// set*, which is what is on screen, not the number of images a window has
/// ever displayed.
const MAX_SURFACE_IMAGE_BYTES: usize = 64 * 1024 * 1024;

struct StreamFrame {
    commands: Vec<DrawCommand>,
    /// The tier this frame's content records at — the surface's declared
    /// tier as it stood when the frame attached ("latched with the next
    /// attach", specs/history.md decision 4).
    tier: u8,
    /// The logical size the frame was laid out for — also the window's
    /// effective geometry (a bufferless surface has no bbox to derive one).
    width: u32,
    height: u32,
    /// The client's encoded size, for the HUD's wire readout. A number, not
    /// the bytes: the bytes below are gone after the latch, and this is all
    /// anything wanted from them afterwards.
    wire_len: usize,
    /// The frame exactly as the client encoded it, so a recording stores the
    /// client's own bytes rather than a re-encoding.
    ///
    /// **Moved out at latch and empty from then on.** Held for the life of the
    /// frame it cost every window a second copy of its content — decoded
    /// commands and the encoding they came from — recording or not, and a
    /// client may attach up to `MAX_STREAM_SIZE` (4 MiB) per surface.
    raw: Vec<u8>,
}

/// One shader slot's parameter upload: which slot, the packed values, and
/// the setter that carries them to the GPU. Named because the tuple is
/// three unrelated things and reads as noise inline.
type ParamUpload<'a> = (usize, [[f32; 4]; 8], &'a dyn Fn([[f32; 4]; 8]));

/// User data on a rill_stream_v1 object: which surface it feeds.
struct StreamUserData {
    surface_id: ObjectId,
}

/// Anchors for an in-progress resize: the fixed (opposite) edge positions and
/// which edges are moving. Repositioning happens on commit, using the size the
/// client actually applied — so the window doesn't drift ahead of its content.
struct ResizeState {
    window: Window,
    top: bool,
    left: bool,
    anchor_right: i32,
    anchor_bottom: i32,
    initial_loc: Point<i32, Logical>,
}

impl Rill {
    /// The topmost window whose *visible geometry* contains a compositor-space
    /// point. Hit-testing by geometry (not the client's input region) keeps
    /// overlapping windows unambiguous — a click on the visible top window
    /// goes to it, even when a client-side-decoration shadow would otherwise
    /// let it fall through to the window below.
    /// A window's on-screen rect. Buffer windows use xdg geometry; vector-
    /// native windows use their stream frame's declared size — their surface
    /// has no buffer, so smithay clamps xdg geometry to an empty bbox.
    fn window_rect(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let loc = self.space.element_location(window)?;
        if let Some(frame) = window
            .toplevel()
            .and_then(|t| self.streams.get(&t.wl_surface().id()))
            .and_then(|s| s.current.as_ref())
        {
            return Some(Rectangle::new(
                loc,
                (frame.width as i32, frame.height as i32).into(),
            ));
        }
        let geo = window.geometry();
        Some(Rectangle::new(loc + geo.loc, geo.size))
    }

    /// The live windows as a recording wants them: bottom → top, app windows
    /// only. The wallpaper and dock are shell chrome rather than windows — a
    /// replay draws its own — so they stay out of the event stream.
    fn record_snapshots(&mut self) -> Vec<recorder::Snapshot> {
        // Gathered first, ids assigned second: `record_id` needs `&mut self`
        // and the walk borrows `self.space`.
        type Gathered = (ObjectId, i32, i32, u32, u32, String, bool, u8, String);
        let gathered: Vec<Gathered> = self
            .space
            .elements()
            .filter(|w| Some(*w) != self.background.as_ref() && Some(*w) != self.dock.as_ref())
            .filter_map(|window| {
                let toplevel = window.toplevel()?;
                let surface = toplevel.wl_surface();
                let rect = self.window_rect(window)?;
                // A zero-area window has nothing to replay. Skipping them
                // keeps out throwaway surfaces that appear, never get a size,
                // and close again inside half a second, which replayed as
                // grey boxes flickering at the start of every recording. It
                // also means a real window is first recorded at its first
                // real geometry rather than at 0x0.
                if rect.size.w <= 0 || rect.size.h <= 0 {
                    return None;
                }
                let app = window
                    .toplevel()
                    .and_then(toplevel_app_id)
                    .unwrap_or_default();
                let title = with_states(surface, |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .and_then(|d| d.lock().unwrap().title.clone())
                })
                .unwrap_or_default();
                Some((
                    surface.id(),
                    rect.loc.x,
                    rect.loc.y,
                    rect.size.w.max(0) as u32,
                    rect.size.h.max(0) as u32,
                    // A title longer than the codec's short-string cap is
                    // truncated rather than failing the whole recording.
                    truncate_title(&title),
                    self.streams.contains_key(&surface.id()),
                    // The latched tier rides with the snapshot so history
                    // stamps this window's state at its own classification.
                    self.streams.get(&surface.id()).map_or(0, |st| st.tier),
                    app,
                ))
            })
            .collect();
        gathered
            .into_iter()
            .map(|(oid, x, y, w, h, title, vector, tier, app)| recorder::Snapshot {
                id: self.record_id(&oid),
                x,
                y,
                w,
                h,
                title,
                vector,
                tier,
                app,
            })
            .collect()
    }

    /// The id a recording knows a surface by — compositor-wide and stable
    /// for the surface's lifetime. Assigned on first sight.
    fn record_id(&mut self, surface: &ObjectId) -> u32 {
        if let Some(n) = self.record_ids.get(surface) {
            return *n;
        }
        let n = self.next_record_id;
        self.next_record_id += 1;
        self.record_ids.insert(surface.clone(), n);
        n
    }

    /// Start or stop recording. Returns a line to print — the only feedback
    /// there is until the dock grows a Rec button.
    fn toggle_recording(&mut self) -> String {
        if let Some(rec) = self.recorder.take() {
            let (path, failed) = rec.finish();
            return match failed {
                Some(e) => format!("recording stopped, incomplete ({e}): {}", path.display()),
                None => format!("recording stopped: {}", path.display()),
            };
        }
        let path = recording_path();
        let (w, h) = (self.output_size.w.max(1) as u32, self.output_size.h.max(1) as u32);
        match recorder::Recorder::start(&path, w, h) {
            Ok(mut rec) => {
                // Seed the recording with the desktop as it stands, so a
                // replay opens on the real screen rather than an empty one.
                rec.sync(&self.record_snapshots());
                self.recorder = Some(rec);
                format!("recording to {}", path.display())
            }
            Err(e) => format!("recording failed to start: {e}"),
        }
    }

    fn window_under(&self, point: Point<f64, Logical>) -> Option<Window> {
        // The dock's stream may paint beyond its strip (its menu opens
        // upward), and input follows paint: wherever the dock actually drew
        // is the dock's, before any window-rect test.
        if let Some(dock) = &self.dock
            && let Some(rect) = self.window_rect(dock)
            && let Some(extent) = self.dock_paint_extent()
        {
            let local = point - rect.loc.to_f64();
            if extent.contains(local) {
                return Some(dock.clone());
            }
            // An overlay past the strip means a menu is open, and an open
            // menu is a pointer grab — the way it is in every toolkit. All
            // pointer input goes to the dock, so a click anywhere else
            // lands on its outside-the-menu path and dismisses (the
            // viewport already does that half); the click is swallowed,
            // which is the standard first-click-away behaviour. Without
            // this the dock never *hears* the click-away — it lands on
            // whatever window was under it — and the menu simply stays.
            // "Past the strip" with a generous slack: shadows and glows
            // bleed a few px past the surface on an ordinary frame, and a
            // grab that triggered on a shadow would swallow the desktop
            // for good. A real menu overshoots by at least one item row.
            const SLACK: f64 = 28.0;
            let overlay_open = extent.loc.y < -SLACK
                || extent.loc.x < -SLACK
                || extent.loc.y + extent.size.h > rect.size.h as f64 + SLACK
                || extent.loc.x + extent.size.w > rect.size.w as f64 + SLACK;
            if overlay_open {
                return Some(dock.clone());
            }
        }
        self.space
            .elements()
            .rev()
            .find(|w| {
                self.window_rect(w).map(|r| r.to_f64().contains(point)).unwrap_or(false)
            })
            .cloned()
    }

    /// The resize band of a *foreign* window under the pointer, and which
    /// edge. Native windows (they have a command stream) run their own
    /// edges through the protocol and are never matched here. A foreign
    /// client is different: with client-side decoration refused any real
    /// margin, its border is the compositor's to own — a band reaching
    /// `EDGE_OUT` past the rect and `EDGE_IN` into it, corners widened to
    /// `EDGE_CORNER` so a diagonal is actually hittable. Content wins over
    /// a band: a window stacked above another's edge takes the point.
    fn foreign_edge_at(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(Window, xdg_toplevel::ResizeEdge)> {
        const EDGE_OUT: f64 = 6.0;
        const EDGE_IN: f64 = 4.0;
        const EDGE_CORNER: f64 = 16.0;
        let over = self.window_under(point);
        for w in self.space.elements().rev() {
            if Some(w) == self.background.as_ref() || Some(w) == self.dock.as_ref() {
                continue;
            }
            let Some(surface) = w.toplevel().map(|t| t.wl_surface().clone()) else { continue };
            if self.streams.contains_key(&surface.id()) {
                continue;
            }
            let Some(r) = self.window_rect(w) else { continue };
            let r = r.to_f64();
            // Above this window in the stack, the point belongs to whoever
            // holds it: only the topmost containing window may offer its
            // inner band, and any containing window occludes bands below.
            if let Some(over) = &over {
                if over != w {
                    if r.contains(point) {
                        continue;
                    }
                } else if !r.contains(point) {
                    continue;
                }
            }
            let outer = Rectangle::<f64, Logical>::new(
                (r.loc.x - EDGE_OUT, r.loc.y - EDGE_OUT).into(),
                (r.size.w + EDGE_OUT * 2.0, r.size.h + EDGE_OUT * 2.0).into(),
            );
            if !outer.contains(point) {
                continue;
            }
            let l = point.x <= r.loc.x + EDGE_IN;
            let rt = point.x >= r.loc.x + r.size.w - EDGE_IN;
            let t = point.y <= r.loc.y + EDGE_IN;
            let b = point.y >= r.loc.y + r.size.h - EDGE_IN;
            if !(l || rt || t || b) {
                continue;
            }
            let (tc, bc) = (
                t || ((l || rt) && point.y <= r.loc.y + EDGE_CORNER),
                b || ((l || rt) && point.y >= r.loc.y + r.size.h - EDGE_CORNER),
            );
            let (lc, rc) = (
                l || ((t || b) && point.x <= r.loc.x + EDGE_CORNER),
                rt || ((t || b) && point.x >= r.loc.x + r.size.w - EDGE_CORNER),
            );
            use xdg_toplevel::ResizeEdge::*;
            let edge = match (tc, bc, lc, rc) {
                (true, _, true, _) => TopLeft,
                (true, _, _, true) => TopRight,
                (_, true, true, _) => BottomLeft,
                (_, true, _, true) => BottomRight,
                (true, ..) => Top,
                (_, true, ..) => Bottom,
                (_, _, true, _) => Left,
                (_, _, _, true) => Right,
                // Unreachable: a band test passed to get here.
                _ => continue,
            };
            return Some((w.clone(), edge));
        }
        None
    }

    /// The cursor the desktop imposes over whatever a client set: the
    /// matching resize arrow while the pointer is in a foreign window's
    /// band (and, since the hover is pinned, for the whole drag).
    fn edge_cursor(&self) -> Option<CursorIcon> {
        use xdg_toplevel::ResizeEdge::*;
        self.edge_hover.as_ref().map(|(_, e)| match e {
            Top => CursorIcon::NResize,
            Bottom => CursorIcon::SResize,
            Left => CursorIcon::WResize,
            Right => CursorIcon::EResize,
            TopLeft => CursorIcon::NwResize,
            TopRight => CursorIcon::NeResize,
            BottomLeft => CursorIcon::SwResize,
            _ => CursorIcon::SeResize,
        })
    }

    /// Bounding box of everything the dock's current frame paints, in
    /// dock-local logical coordinates. None while the dock has no frame.
    fn dock_paint_extent(&self) -> Option<Rectangle<f64, Logical>> {
        let dock = self.dock.as_ref()?;
        let surface = dock.toplevel()?.wl_surface().clone();
        let frame = self.streams.get(&surface.id())?.current.as_ref()?;
        let (mut x0, mut y0) = (f64::MAX, f64::MAX);
        let (mut x1, mut y1) = (f64::MIN, f64::MIN);
        let mut grow = |x: f32, y: f32, w: f32, h: f32| {
            x0 = x0.min(x as f64);
            y0 = y0.min(y as f64);
            x1 = x1.max((x + w) as f64);
            y1 = y1.max((y + h) as f64);
        };
        for c in &frame.commands {
            match c {
                DrawCommand::Rect { rect, .. }
                | DrawCommand::Shadow { rect, .. }
                | DrawCommand::Glow { rect, .. }
                | DrawCommand::Border { rect, .. }
                | DrawCommand::Text { rect, .. }
                | DrawCommand::Image { rect, .. }
                | DrawCommand::Backdrop { rect, .. } => grow(rect.x, rect.y, rect.w, rect.h),
                DrawCommand::Path { points, .. } | DrawCommand::FillPath { points, .. } => {
                    for p in points {
                        grow(p.x, p.y, 0.0, 0.0);
                    }
                }
                _ => {}
            }
        }
        (x0 < x1 && y0 < y1).then(|| {
            Rectangle::new((x0, y0).into(), ((x1 - x0), (y1 - y0)).into())
        })
    }

    /// The compositor window resized: track the new output size and re-fit the
    /// shell surfaces — the wallpaper fills the output, the dock stays pinned
    /// to the bottom edge and full width.
    fn reflow_shell(&mut self, size: Size<i32, Logical>) {
        if size.w <= 0 || size.h <= 0 || size == self.output_size {
            return;
        }
        self.output_size = size;
        self.needs_redraw = true;
        if let Some(bg) = self.background.clone()
            && let Some(t) = bg.toplevel().cloned()
        {
            t.with_pending_state(|s| s.size = Some(size));
            t.send_configure();
            self.space.map_element(bg, (0, 0), false);
        }
        self.replace_widgets();
        if let Some(dock) = self.dock.clone()
            && let Some(t) = dock.toplevel().cloned()
        {
            let h = self.dock_height;
            t.with_pending_state(|s| s.size = Some((size.w, h).into()));
            t.send_configure();
            self.space.map_element(dock, (0, 0), false);
        }
    }

    /// Push every window back inside the usable area. Called when the dock's
    /// height changes: the strip is reserved space, so growing it has to
    /// move whatever was sitting where it now is.
    fn reclamp_windows(&mut self) {
        self.replace_widgets();
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for window in windows {
            if Some(&window) == self.background.as_ref()
                || Some(&window) == self.dock.as_ref()
                || self.is_widget(&window)
            {
                continue;
            }
            let Some(rect) = self.window_rect(&window) else { continue };
            let next = self.clamp_to_usable(rect.loc, rect.size);
            if next != rect.loc {
                self.space.map_element(window, next, false);
            }
        }
    }

    /// The usable desktop area for app windows: the output minus the dock
    /// strip along the top. Apps are confined to this so they can't be
    /// dragged off-screen or under the dock (a stand-in for real panel
    /// usable-area).
    fn usable_area(&self) -> (i32, i32) {
        (self.output_size.w, (self.output_size.h - self.dock_height).max(0))
    }

    /// Clamp a window's top-left so the window stays inside the usable area —
    /// which now starts *below* the dock, so a window can never sit under it.
    fn clamp_to_usable(
        &self,
        loc: Point<i32, Logical>,
        size: Size<i32, Logical>,
    ) -> Point<i32, Logical> {
        let (uw, uh) = self.usable_area();
        let max_x = (uw - size.w).max(0);
        let max_y = (uh - size.h).max(0) + self.dock_height;
        Point::from((loc.x.clamp(0, max_x), loc.y.clamp(self.dock_height, max_y)))
    }

    fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == surface).unwrap_or(false))
            .cloned()
    }

    /// The topmost *app* window — excludes the shell background/dock — for the
    /// focus indicator and the new-window cascade.
    fn top_app_window(&self) -> Option<Window> {
        self.space
            .elements()
            .rev()
            // Widgets are included: they are windows, and one you have
            // clicked is the window you meant. The concern this used to
            // guard against — a widget taking the keyboard from whatever you
            // were typing in — is handled where it belongs instead: a widget
            // never takes focus on *map*, so one appearing at login or
            // reloading on its clock cannot steal anything. Focus follows
            // the click, as it does for every other window.
            .find(|w| {
                Some(*w) != self.background.as_ref() && Some(*w) != self.dock.as_ref()
            })
            .cloned()
    }

    fn is_widget(&self, window: &Window) -> bool {
        self.widgets.iter().any(|w| &w.window == window)
    }

    /// A widget was dropped: remember where, and write it back to the theme.
    ///
    /// Both halves matter. Updating the in-memory placement is what stops
    /// the next `replace_widgets` — a resolution change, the dock resizing —
    /// from yanking it back to where the theme last said. Writing the file
    /// is what makes it survive a restart. A drag that only did the first
    /// would be the "appeared to work but reverted" failure this used to
    /// refuse to risk.
    fn widget_dropped(&mut self, window: &Window) {
        let Some(origin) = self.space.element_location(window) else { return };
        let (output, top) = (self.output_size, self.dock_height);
        let Some(widget) = self.widgets.iter_mut().find(|w| &w.window == window) else {
            return;
        };
        let next = widget.place.placed_at(origin, output, top);
        if next == widget.place {
            return;
        }
        widget.place = next;
        let app = widget.app.clone();
        // A widget launched by hand rather than from the theme has no entry
        // to write to. It still moves; it just does not persist.
        if app.is_empty() {
            return;
        }
        if let Err(e) = save_widget_place(&app, next) {
            eprintln!("rill-compositor: could not save widget position: {e}");
        }
    }

    /// Apply placements read back from `[[desktop.widgets]]` to the live
    /// widget windows — the other half of the studio's anchor chips, which
    /// edit the file and rely on someone moving the actual window.
    ///
    /// Entries are matched to windows by `app =` URL, consuming each file
    /// entry at most once so two widgets sharing a URL each get their own
    /// row. A placement identical to what the window already has is skipped,
    /// which is what makes the compositor's own drag-write round-trip
    /// through here as a no-op instead of a fight.
    fn apply_widget_places(&mut self, places: &[(String, WidgetPlace)]) {
        let mut pool: Vec<Option<&(String, WidgetPlace)>> = places.iter().map(Some).collect();
        let (output, top) = (self.output_size, self.dock_height);
        let mut moved = false;
        for w in &mut self.widgets {
            if w.app.is_empty() {
                continue;
            }
            let Some(slot) = pool
                .iter_mut()
                .find(|s| s.is_some_and(|(app, _)| *app == w.app))
            else {
                continue;
            };
            let &(_, place) = slot.take().expect("checked by find");
            if place == w.place {
                continue;
            }
            let resized = (place.w, place.h) != (w.place.w, w.place.h);
            w.place = place;
            if resized && let Some(t) = w.window.toplevel() {
                t.with_pending_state(|s| s.size = Some((place.w, place.h).into()));
                t.send_configure();
            }
            self.space
                .map_element(w.window.clone(), place.origin(output, top), false);
            moved = true;
        }
        if moved {
            self.needs_redraw = true;
        }
    }

    /// Put every widget back in its corner — after a resolution change, or
    /// after the dock's height moved the top edge.
    fn replace_widgets(&mut self) {
        let placed: Vec<(Window, Point<i32, Logical>)> = self
            .widgets
            .iter()
            .map(|w| (w.window.clone(), w.place.origin(self.output_size, self.dock_height)))
            .collect();
        for (window, origin) in placed {
            self.space.map_element(window, origin, false);
        }
    }

    /// The (surface, surface-origin) under a point, for pointer focus. Resolves
    /// the precise sub-surface within the top window, or the toplevel itself.
    /// The popup under the pointer, if any: its surface and the surface's
    /// global origin. Popups float above all of their window's content and
    /// may escape its rect entirely, so they are hit-tested before any
    /// window — topmost window first, deepest popup first.
    fn popup_under(&self, point: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        for w in self.space.elements().rev() {
            let Some(t) = w.toplevel() else { continue };
            let Some(loc) = self.space.element_location(w) else { continue };
            let popups: Vec<_> = PopupManager::popups_for_surface(t.wl_surface()).collect();
            for (popup, off) in popups.into_iter().rev() {
                let geo = popup.geometry();
                let rect = Rectangle::new(loc + off, geo.size);
                if rect.to_f64().contains(point) {
                    // The returned origin is the *surface's*: input
                    // coordinates are surface-local, and the geometry may
                    // start inside it.
                    return Some((popup.wl_surface().clone(), (loc + off - geo.loc).to_f64()));
                }
            }
        }
        None
    }

    fn surface_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if let Some(hit) = self.popup_under(point) {
            return Some(hit);
        }
        let window = self.window_under(point)?;
        let loc = self.space.element_location(&window)?;
        if let Some((surface, offset)) =
            window.surface_under(point - loc.to_f64(), WindowSurfaceType::ALL)
        {
            return Some((surface, (loc + offset).to_f64()));
        }
        let surface = window.toplevel()?.wl_surface().clone();
        Some((surface, loc.to_f64()))
    }
}

/// Set by SIGINT/SIGTERM; the render loop notices and exits through the
/// normal shutdown path.
static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    // Async-signal-safe: a relaxed atomic store and nothing else. The loop
    // does the actual work.
    SHUTDOWN.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Ask for a graceful stop on Ctrl+C and `kill`.
///
/// Without this, a recording started with RILL_RECORD dies with the process
/// and loses whatever is still in the writer's buffer — the file decodes only
/// up to its last flushed event, so a 20-second session came back as two
/// seconds. The recording is append-only, so that is survivable rather than
/// fatal, but it is not what anyone wants from Ctrl+C.
fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

fn shutting_down() -> bool {
    SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed)
}

/// Surface present mode: `RILL_PRESENT_MODE=auto|immediate|mailbox|fifo`.
///
/// The diagnostic for a frame time that will not move. Under load the span is
/// 29.00 ms at p50 whether the output is 1280x800 or 640x400 and whether the
/// scene is one window or three — flat against pixels *and* against content,
/// which is not what work looks like. FIFO paces presentation to the display,
/// so if the wait is there, `immediate` removes it and the frame collapses to
/// its real cost. If the frame stays at 29 ms, the wait is ours.
///
/// Which is also why Mailbox is the default wherever a surface offers it.
/// Nested inside another compositor, the host already paces us with frame
/// callbacks, and asking the swapchain to pace us as well is two clocks for
/// one rhythm. Measured over 45 s on this NVIDIA/Wayland surface, same scene
/// and the same 60 fps either way:
///
/// ```text
///              frame p50   p99      max      acquire mean
///   AutoVsync   14.75 ms   17.25    71.44    13.09
///   Mailbox      0.75 ms    1.25    25.33     0.06
/// ```
///
/// The max is the part that shows: FIFO stalled 60-70 ms every ten seconds,
/// like clockwork, four dropped frames at a time, and all of it inside
/// `acquire`. The compositor's own drawing was 0.67 ms in both — the
/// difference is entirely waiting.
///
/// Not Immediate, which would tear. Mailbox always presents whole frames; it
/// just does not make us sit in the queue behind them.
fn present_mode(supported: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    let set = std::env::var("RILL_PRESENT_MODE");
    let (asked, explicit) = match set.as_deref().map(str::trim) {
        Ok("immediate") => (wgpu::PresentMode::Immediate, true),
        Ok("mailbox") => (wgpu::PresentMode::Mailbox, true),
        Ok("fifo") => (wgpu::PresentMode::Fifo, true),
        Ok("auto") => return wgpu::PresentMode::AutoVsync,
        // The default. Falls through to the support check below, so a surface
        // without Mailbox — the Pi's V3D may be one, and is unmeasured — lands
        // on AutoVsync exactly as before, and quietly, since nobody asked for
        // anything.
        _ => (wgpu::PresentMode::Mailbox, false),
    };
    // Ask the surface before asking wgpu: configuring an unsupported mode is
    // a validation *panic*, not an error, so a diagnostic knob that a given
    // surface happens not to offer would take the whole compositor down at
    // startup. This NVIDIA/Wayland surface offers [Mailbox, Fifo] and no
    // Immediate; the Pi's V3D surface does offer it.
    if supported.contains(&asked) {
        return asked;
    }
    if explicit {
        eprintln!(
            "rill-compositor: present mode {asked:?} unsupported here (have {supported:?}) \
             — falling back to AutoVsync"
        );
    }
    wgpu::PresentMode::AutoVsync
}

/// The nested output's size: `RILL_BENCH_RESOLUTION=WxH`, else 1280x800.
///
/// It was hardcoded in three places while `bench-device.sh --resolution`
/// exported this variable and its help text claimed the size was "requested
/// of the compositor". It was recorded and ignored. Beyond making the flag
/// honest, varying the output size is how you tell drawing cost that scales
/// with pixels (fill rate, the fx chain) from cost that scales with objects.
fn nested_output_size() -> (i32, i32) {
    const DEFAULT: (i32, i32) = (1280, 800);
    let Ok(spec) = std::env::var("RILL_BENCH_RESOLUTION") else { return DEFAULT };
    let Some((w, h)) = spec.split_once(['x', 'X']) else { return DEFAULT };
    match (w.trim().parse::<i32>(), h.trim().parse::<i32>()) {
        (Ok(w), Ok(h)) if (16..=7680).contains(&w) && (16..=4320).contains(&h) => (w, h),
        _ => {
            eprintln!("rill-compositor: ignoring RILL_BENCH_RESOLUTION={spec:?} (want WxH)");
            DEFAULT
        }
    }
}

/// How long frames took to draw, as a fixed-size histogram.
///
/// A mean frame rate hides the thing that makes a desktop feel bad: 30 fps
/// with an occasional 120 ms stall reads as broken while 30 fps flat reads as
/// fine, and the two report the same number. The Pi made this urgent — its
/// compositor averaged 27% of a core with individual seconds peaking at 54%,
/// which is the shape of work arriving in lumps.
///
/// Buckets rather than a `Vec<Duration>` because this runs for the lifetime
/// of a session that may last days: 0.25 ms resolution to 64 ms, one overflow
/// bucket, about 1 KiB total and O(1) forever. The exact maximum is kept
/// separately, since it is the number an outlier hunt actually wants and it is
/// the one thing bucketing would round away.
struct FrameTimes {
    /// 0.25 ms per bucket; the last bucket is everything ≥ 64 ms.
    buckets: [u32; Self::BUCKETS],
    count: u64,
    total: std::time::Duration,
    max: std::time::Duration,
}

impl FrameTimes {
    const BUCKETS: usize = 257;
    const RESOLUTION_US: u64 = 250;

    fn new() -> FrameTimes {
        FrameTimes {
            buckets: [0; Self::BUCKETS],
            count: 0,
            total: std::time::Duration::ZERO,
            max: std::time::Duration::ZERO,
        }
    }

    fn record(&mut self, d: std::time::Duration) {
        let i = (d.as_micros() as u64 / Self::RESOLUTION_US) as usize;
        self.buckets[i.min(Self::BUCKETS - 1)] += 1;
        self.count += 1;
        self.total += d;
        if d > self.max {
            self.max = d;
        }
    }

    /// The `p`th percentile in milliseconds (0.0–1.0), as the upper edge of
    /// the bucket the sample falls in. The overflow bucket reports the real
    /// maximum rather than "≥64", so a percentile can never understate.
    fn percentile_ms(&self, p: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let target = (self.count as f64 * p).ceil() as u64;
        let mut seen = 0u64;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += *n as u64;
            if seen >= target {
                if i == Self::BUCKETS - 1 {
                    return self.max.as_secs_f64() * 1000.0;
                }
                return ((i + 1) as f64 * Self::RESOLUTION_US as f64) / 1000.0;
            }
        }
        self.max.as_secs_f64() * 1000.0
    }

    fn mean_ms(&self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        self.total.as_secs_f64() * 1000.0 / self.count as f64
    }
}

/// What the damage gate actually did, printed once on the way out.
///
/// The claim "an idle desktop renders nothing" is only worth making if it is
/// checked, and it cannot be checked from inside a run: the HUD's counter
/// resets every sample and the HUD itself marks damage at 2 Hz. A lifetime
/// count against wall-clock is the honest form — a desktop that sat idle for
/// a minute should report a mean near the 1 Hz heartbeat, not near 60.
fn frame_report(
    frames: u64,
    heartbeat: u64,
    uptime: std::time::Duration,
    times: &FrameTimes,
    acquire: &FrameTimes,
    commits: u64,
) {
    let secs = uptime.as_secs_f64().max(0.001);
    println!(
        "rill-compositor: frames={frames} heartbeat={heartbeat} damage={} \
         uptime={secs:.1}s mean_fps={:.2}",
        frames.saturating_sub(heartbeat),
        frames as f64 / secs
    );
    // A separate line on purpose: bench-stack.sh and bench-device.sh both
    // parse the one above by pattern, and widening it to carry percentiles
    // would break every bundle already collected.
    // Frames drawn per content-carrying client commit. One is the ideal; the
    // Pi measured ~2.1, and halving that halves the compositor's cost without
    // making any single frame faster.
    if commits > 0 {
        println!(
            "rill-compositor: commits={commits} frames_per_commit={:.2}",
            frames.saturating_sub(heartbeat) as f64 / commits as f64
        );
    }
    if times.count > 0 {
        println!(
            "rill-compositor: frame_ms mean={:.2} p50={:.2} p95={:.2} p99={:.2} \
             max={:.2} n={}",
            times.mean_ms(),
            times.percentile_ms(0.50),
            times.percentile_ms(0.95),
            times.percentile_ms(0.99),
            times.max.as_secs_f64() * 1000.0,
            times.count
        );
        // The split that says whether a slow frame is our work or the
        // display's pacing. `work` is the span minus the acquire wait; when
        // vsync is the limiter, acquire carries almost all of it.
        println!(
            "rill-compositor: acquire_ms mean={:.2} p50={:.2} p95={:.2} max={:.2} \
             work_ms_mean={:.2}",
            acquire.mean_ms(),
            acquire.percentile_ms(0.50),
            acquire.percentile_ms(0.95),
            acquire.max.as_secs_f64() * 1000.0,
            (times.mean_ms() - acquire.mean_ms()).max(0.0)
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if rill_log::dev_active() {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            rill_log::dev_emit("rill-compositor", "panic", &[("info", &info.to_string())]);
            default_hook(info);
        }));
        rill_log::dev_emit("rill-compositor", "start", &[]);
    }
    install_signal_handlers();
    // Clients to launch inside the compositor, `+`-separated so several can
    // run together (milestone-14 exit condition: a Rill app *and* an ordinary
    // Wayland app). Each group is a command with its own arguments.
    //   rill-compositor alacritty + /path/rill-view --app KEY --data DIR
    // Foreign clients still work and still exercise the shm path — they are
    // just no longer what you get by default.
    let mut clients: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for arg in std::env::args().skip(1) {
        if arg == "+" {
            if !current.is_empty() {
                clients.push(std::mem::take(&mut current));
            }
        } else {
            current.push(arg);
        }
    }
    if !current.is_empty() {
        clients.push(current);
    }
    if clients.is_empty() {
        // Default to our own window, not a terminal: a vector-native client
        // is what this desktop is *for*, and a pixel client records as an
        // empty placeholder in a session recording. Resolved next to this
        // binary (same rule the dock uses) so a dev build launches its own
        // rill-vector rather than whatever is on PATH.
        let vector = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("rill-vector")))
            .filter(|path| path.exists())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "rill-vector".to_string());
        clients.push(vec![vector, "--dashboard".to_string()]);
    }

    let mut display: Display<Rill> = Display::new()?;
    let dh = display.handle();

    let compositor_state = CompositorState::new::<Rill>(&dh);
    let shm_state = ShmState::new::<Rill>(&dh, vec![]);
    let xdg_shell_state = XdgShellState::new::<Rill>(&dh);
    let data_device_state = DataDeviceState::new::<Rill>(&dh);
    let output_manager_state = OutputManagerState::new_with_xdg_output::<Rill>(&dh);
    let cursor_shape_state = CursorShapeManagerState::new::<Rill>(&dh);
    let xdg_decoration_state = XdgDecorationState::new::<Rill>(&dh);
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "rill");

    let listener = ListeningSocket::bind_auto("wayland", 2..32)?;
    let socket_name = listener
        .socket_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or("no socket name")?;
    println!("rill-compositor: listening on WAYLAND_DISPLAY={socket_name}");

    // The host window is raw winit (smithay's reexport, so versions align):
    // smithay's own winit backend would create an EGL surface eagerly, and the
    // host's DRM-syncobj protocol allows only one sync object per wl_surface —
    // EGL's would lock Vulkan's WSI out. Raw winit means wgpu owns every pixel
    // AND the only GPU context (W3, specs/wgpu-renderer.md D2); input is
    // translated by hand below with smithay's own mappings.
    let mut event_loop = EventLoop::new()?;
    let mut host = Host { window: None, events: Vec::new() };
    for _ in 0..50 {
        let _ = event_loop.pump_app_events(Some(std::time::Duration::ZERO), &mut host);
        if host.window.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let window = host.window.clone().ok_or("winit window did not initialize")?;

    // wgpu: a surface on the winit window, and a dmabuf-capable device on an
    // adapter that can present to it (same GPU imports and composites).
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
    });
    let wgpu_surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::from_window(&*window)?)?
    };
    let gpu = DmabufDevice::new_on(&instance, Some(&wgpu_surface))
        .ok_or("no dmabuf-capable Vulkan device")?;
    // Name alone does not identify a renderer: the same card through a
    // different ICD or driver version is a different set of numbers, and a
    // measurement that cannot name its renderer cannot be reproduced.
    // scripts/bench-stack.sh records this line with the rest of the specs.
    {
        let info = gpu.adapter().get_info();
        println!(
            "rill-compositor: wgpu on {} ({:?}, {:?}, driver {} {})",
            info.name, info.backend, info.device_type, info.driver, info.driver_info
        );
    }

    // Prefer a non-sRGB surface format: colors were authored against a linear
    // 8-bit pipeline (the GLES path), and client buffers are raw bytes.
    let caps = wgpu_surface.get_capabilities(gpu.adapter());
    let surface_format =
        caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
    let renderer = GpuRenderer::with_device(
        gpu.device.clone(),
        gpu.queue.clone(),
        surface_format,
        gpu.adapter_name(),
    );

    // Advertise exactly what the importer can bind.
    let mut dmabuf_formats: Vec<DrmFormat> = Vec::new();
    for fourcc in [Fourcc::Argb8888, Fourcc::Xrgb8888] {
        for modifier in gpu.supported_modifiers(fourcc as u32) {
            dmabuf_formats.push(DrmFormat { code: fourcc, modifier: Modifier::from(modifier) });
        }
    }
    println!("rill-compositor: {} importable dmabuf formats", dmabuf_formats.len());
    let mut dmabuf_state = DmabufState::new();
    let _dmabuf_global = dmabuf_state.create_global::<Rill>(&dh, dmabuf_formats);

    // Vector-native window content (rill_stream_v1, W4).
    // Version 2: streams inherit it, and that is what makes attach_image
    // reachable (see the manager's description in rill-stream-v1.xml).
    let _stream_global = dh.create_global::<Rill, RillStreamManagerV1, ()>(3, ());

    let output = Output::new(
        "rill-0".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Rill".into(),
            model: "Nested".into(),
        },
    );
    let (out_w, out_h) = nested_output_size();
    println!("rill-compositor: nested output {out_w}x{out_h}");
    let mode = Mode { size: (out_w, out_h).into(), refresh: 60_000 };
    output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((0, 0).into()));
    output.set_preferred(mode);
    let _output_global = output.create_global::<Rill>(&dh);

    // A seat with keyboard + pointer.
    let keyboard = seat.add_keyboard(Default::default(), 200, 25).unwrap();
    let pointer = seat.add_pointer();

    let mut state = Rill {
        widgets: Vec::new(),
        dock_height: theme_desktop_fx().dock_height,
        compositor_state,
        xdg_shell_state,
        shm_state,
        seat_state,
        data_device_state,
        dmabuf_state,
        output_manager_state,
        cursor_shape_state,
        xdg_decoration_state,
        cursor_status: CursorImageStatus::Named(CursorIcon::Default),
        seat,
        space: Space::default(),
        resize_state: None,
        edge_hover: None,
        popups: PopupManager::default(),
        popup_grab: None,
        output_size: nested_output_size().into(),
        background: None,
        dock: None,
        display_handle: dh.clone(),
        streams: HashMap::new(),
        prev_window_pos: HashMap::new(),
        window_velocity: HashMap::new(),
        draw_cursor: true,
        record_ids: HashMap::new(),
        next_record_id: 1,
        needs_redraw: true,
        pointer_warp: None,
        show_hud: false,
        spawn_times: HashMap::new(),
        commit_count: 0,
        total_commits: 0,
        recorder: None,
        history: None,
        tier_policy: history_writer::TierPolicy::load(&history_policy_path()),
    };
    // Map the output into the space so element geometry is output-relative.
    state.space.map_output(&output, (0, 0));

    // Spawn every client pointed at *our* socket.
    for client_cmd in &clients {
        let mut cmd = std::process::Command::new(&client_cmd[0]);
        cmd.args(&client_cmd[1..]).env("WAYLAND_DISPLAY", &socket_name).env_remove("DISPLAY");
        match cmd.spawn() {
            Ok(_) => println!("rill-compositor: spawned client {:?}", client_cmd.join(" ")),
            Err(e) => eprintln!("rill-compositor: could not spawn {:?}: {e}", client_cmd[0]),
        }
    }

    // Frame budget, from the display rather than from a guess.
    //
    // This was a flat 15ms — 66.7fps — presenting into a surface paced by
    // the monitor's vsync. On a 60Hz screen that asks for a frame every
    // 15ms and gets one every 16.7ms, so the two run at a beat against each
    // other and the phase slips continuously; the visible result is an
    // occasional hitch on an otherwise smooth desktop, and it only shows
    // when something animates continuously, because that is the only time
    // the compositor renders every frame.
    //
    // Pacing to the refresh interval removes the beat. A monitor that does
    // not report its rate falls back to 60Hz, which is the number the old
    // constant was reaching for anyway.
    let frame_budget = host
        .window
        .as_ref()
        .and_then(|w| w.current_monitor())
        .and_then(|m| m.refresh_rate_millihertz())
        .filter(|mhz| *mhz >= 20_000)
        .map(|mhz| std::time::Duration::from_nanos(1_000_000_000_000u64 / mhz as u64))
        .unwrap_or(std::time::Duration::from_nanos(16_666_667));
    println!(
        "rill-compositor: frame budget {:.2}ms ({:.1}fps)",
        frame_budget.as_secs_f64() * 1000.0,
        1.0 / frame_budget.as_secs_f64()
    );

    // `RILL_FRAME_LOG=<ms>` prints every frame that took longer than that,
    // with when it happened. A stutter you can see but cannot time is a
    // guessing game; a list of outliers with timestamps has a *period* in
    // it, and a period names the cause.
    let frame_log_threshold = std::env::var("RILL_FRAME_LOG")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|ms| std::time::Duration::from_secs_f64(ms / 1000.0));
    let mut last_frame_end: Option<std::time::Instant> = None;

    let start_time = std::time::Instant::now();
    // Lifetime frame count, printed on the way out. The HUD's `render_count`
    // resets every sample, so it cannot answer "did the damage gate actually
    // hold for that run" — which is the only question a before/after
    // measurement asks. scripts/bench-stack.sh parses the shutdown line.
    let mut total_frames: u64 = 0;
    // Of those, how many were the 1 Hz self-heal rather than real damage.
    // The difference is the whole claim: an idle desktop should be almost
    // all heartbeat, and if it is not, something is marking damage that
    // nothing asked to change.
    let mut heartbeat_frames: u64 = 0;
    // How long each of those frames took, start of composite to post-present.
    let mut frame_times = FrameTimes::new();
    // ...and how much of that was spent *waiting* for a swapchain image
    // rather than doing anything. Under vsync the acquire blocks until the
    // presentation queue has a slot, so without this split the frame span
    // reads as our cost when it is mostly the display's cadence — which is
    // exactly how the first Pi measurement was nearly misread.
    let mut acquire_times = FrameTimes::new();
    // `RILL_RECORD=1` records the whole session from launch — the same path
    // Ctrl+Alt+R drives, for when there is nobody at the keyboard.
    // The system-of-record is on by default (specs/history.md decision 1:
    // always-on; sensitivity is classification, the escape hatch is hard
    // delete). `RILL_HISTORY=0` disables it at boot — configuration, not a
    // pause — and the badge in the corner never lies about which it is.
    if std::env::var("RILL_HISTORY").as_deref() != Ok("0") {
        let dir = history_dir();
        println!("rill-compositor: history recording to {} (rill history list)", dir.display());
        if !state.tier_policy.is_default() {
            // The owner should see that their ratchet loaded — a policy that
            // silently failed to parse would record everything at declared
            // tiers, which is exactly what the policy exists to raise.
            println!(
                "rill-compositor: history tier policy active ({})",
                history_policy_path().display()
            );
        }
        // The device unlock (specs/history.md decision 2): derived from the
        // identity key the machine already has, same default and
        // RILL_IDENTITY override as rill-client::util::default_identity_dir
        // — mirrored here rather than depended on, since this is one join.
        let identity = std::env::var("RILL_IDENTITY").map(std::path::PathBuf::from).unwrap_or_else(
            |_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                std::path::Path::new(&home).join(".config").join("rill")
            },
        );
        let kek = rill_history::crypt::Kek::from_identity_dir(&identity);
        if kek.is_none() {
            // Stated at boot, once: an unenrolled machine records plaintext,
            // and the honest report beats a silent downgrade.
            println!(
                "rill-compositor: history is UNENCRYPTED (no device identity at {})",
                identity.display()
            );
        }
        state.history = Some(history_writer::History::start(dir, String::new(), kek));
    }
    if std::env::var_os("RILL_RECORD").is_some() {
        println!("rill-compositor: {}", state.toggle_recording());
    }
    let mut pointer_loc: Point<f64, Logical> = (0.0, 0.0).into();
    let mut applied_cursor: (bool, CursorIcon) = (true, CursorIcon::Default);
    let mut configured_size: Option<Size<i32, Physical>> = None;
    let mut last_render = std::time::Instant::now();
    let mut last_window_count = 0usize;
    // Imported client textures, keyed by wl_buffer id. dmabufs import once
    // per buffer (clients double-buffer, so entries cycle); shm re-uploads
    // per frame (cheap at this scale — damage tracking is P3).
    let mut dmabuf_cache: HashMap<ObjectId, Arc<TexBundle>> = HashMap::new();
    // How many surfaces each pixel window's tree contributed last frame,
    // keyed by root surface id — the dev trail gets one event per *change*,
    // not per frame, so "root only, forever" is visible without spam.
    let mut tree_counts: HashMap<ObjectId, usize> = HashMap::new();
    // Whole-output shader (D5): `[desktop] shader` in theme.toml, hot-reloaded
    // by mtime. `installed_shader` records what was last acted on (even a
    // rejected source — retry only when the file changes again).
    let mut installed_shader: Option<(std::path::PathBuf, std::time::SystemTime)> = None;
    let mut installed_window_shader: Option<(std::path::PathBuf, std::time::SystemTime)> = None;
    type ShaderStamp = Option<(std::path::PathBuf, std::time::SystemTime)>;
    let mut installed_particles: (ShaderStamp, ShaderStamp, ShaderStamp) = (None, None, None);
    let mut installed_bg: Option<(std::path::PathBuf, std::time::SystemTime)> = None;
    let mut installed_model: (Stamped, Stamped) = (None, None);
    // The showroom knobs, refreshed by the theme poll — live-editable like
    // every other piece of the desktop's look.
    let mut scene_params = theme_desktop_fx().scene;
    // (focus glow, shadow colour, glow blur, shadow blur) — refreshed by the
    // theme poll like everything else the desktop's look is made of.
    let mut window_dress = {
        let fx = theme_desktop_fx();
        (fx.focus_glow, fx.shadow, fx.focus_glow_blur, fx.shadow_blur)
    };
    let mut cursor_style = theme_desktop_fx().cursor;
    // Pixel wallpaper: decoded once per (path, mtime) change, painted as the
    // scene's bottom layer. `None` = clear color shows.
    let mut installed_wall: Option<(std::path::PathBuf, std::time::SystemTime)> = None;
    let mut wall_tex: Option<TexBundle> = None;
    let mut fx_animated = false;
    // Whether the per-window effect moves. Folded into the animation check
    // with the grader's: either one moving means the desktop cannot idle.
    let mut window_fx_animated = false;
    let mut bg_animated = false;
    let mut effect_label = String::from("off");
    let mut bg_label = String::from("off");
    // Each effect slot's declared `// @param` list, captured at install from
    // the same source the pipeline compiled — the declarations are part of
    // the shader, so they live and die with it. Values come from the theme
    // each poll; rows are re-uploaded only when they actually change.
    let mut bg_param_decls: Vec<rill_appkit::params::ShaderParam> = Vec::new();
    let mut fx_param_decls: Vec<rill_appkit::params::ShaderParam> = Vec::new();
    let mut window_param_decls: Vec<rill_appkit::params::ShaderParam> = Vec::new();
    let mut sent_param_rows: [Option<[[f32; 4]; 8]>; 3] = [None, None, None];
    let mut last_shader_check = std::time::Instant::now() - std::time::Duration::from_secs(1);
    // theme.toml's mtime when widget placements were last applied. Seeded
    // with the current stamp: at startup there are no widget windows yet, so
    // the first apply would be a no-op anyway.
    let mut widget_places_stamp =
        std::fs::metadata(theme_path()).and_then(|m| m.modified()).ok();
    // Stats HUD sampler state: previous CPU ticks per pid, render counter,
    // and the pre-formatted lines the overlay paints.
    let mut hud_prev: HashMap<i32, u64> = HashMap::new();
    let mut hud_last_sample = std::time::Instant::now();
    let mut hud_lines: Vec<String> = Vec::new();
    let mut render_count: u32 = 0;
    let own_pid = std::process::id() as i32;
    let mut installed_boids: u32 = 0;
    let mut last_boid_step = std::time::Instant::now();
    let mut glass_on = false;
    let mut anims_enabled = true;
    // The desktop's floor: `[desktop] background_color`, refreshed by the
    // theme poll; the built-in near-black when the theme says nothing.
    const CLEAR_DEFAULT: UiColor = UiColor { r: 14, g: 16, b: 32, a: 255 };
    let mut clear_color = CLEAR_DEFAULT;
    // The desktop's ears: a parec tap on the output monitor, reduced to the
    // AudioFx rows the shaders read. Silent (all zeros) if parec is absent.
    let mut audio_tap = audio::AudioTap::start();

    loop {
        host.events.clear();
        if last_shader_check.elapsed() > std::time::Duration::from_millis(300) {
            last_shader_check = std::time::Instant::now();
            let fx_conf = theme_desktop_fx_cached();
            scene_params = fx_conf.scene;
            window_dress =
                (fx_conf.focus_glow, fx_conf.shadow, fx_conf.focus_glow_blur, fx_conf.shadow_blur);
            if fx_conf.cursor.draw != cursor_style.draw {
                state.needs_redraw = true;
            }
            cursor_style = fx_conf.cursor;
            state.draw_cursor = cursor_style.draw;
            if fx_conf.hud != state.show_hud {
                state.show_hud = fx_conf.hud;
                state.needs_redraw = true;
            }
            if fx_conf.boids != installed_boids {
                installed_boids = fx_conf.boids;
                // Scattered across the output as it is now, not across a
                // guess — see `set_boids`.
                renderer.set_boids(
                    installed_boids,
                    [state.output_size.w as f32, state.output_size.h as f32],
                );
                println!("rill-compositor: boids {}", installed_boids);
                state.needs_redraw = true;
            }
            if fx_conf.dock_height != state.dock_height {
                // Re-reserve the strip and push every window back inside the
                // usable area — a dock that grew must not leave a window
                // sitting under it.
                state.dock_height = fx_conf.dock_height;
                let size = state.output_size;
                state.output_size = (0, 0).into();
                state.reflow_shell(size);
                state.reclamp_windows();
                state.needs_redraw = true;
            }
            if fx_conf.glass != glass_on || fx_conf.animations != anims_enabled {
                glass_on = fx_conf.glass;
                anims_enabled = fx_conf.animations;
                state.needs_redraw = true;
            }
            let next_clear = fx_conf.background_color.unwrap_or(CLEAR_DEFAULT);
            if next_clear != clear_color {
                clear_color = next_clear;
                state.needs_redraw = true;
            }
            // Widget placement is theme state too: when theme.toml moves,
            // re-read each widget's anchor/offset/size and move the live
            // window — this is what makes the studio's anchor chips act on
            // the desktop rather than only on the file. The compositor's own
            // drag-write comes back through here as a no-op.
            let widget_stamp =
                std::fs::metadata(theme_path()).and_then(|m| m.modified()).ok();
            if widget_stamp != widget_places_stamp {
                widget_places_stamp = widget_stamp;
                state.apply_widget_places(&theme_widget_places());
            }
            let stamp = |p: std::path::PathBuf| {
                let mtime = std::fs::metadata(&p).ok()?.modified().ok()?;
                Some((p, mtime))
            };
            // Pixel wallpaper: decode + upload only when (path, mtime)
            // changes; a bad file logs and clears rather than crashing.
            let current_wall = fx_conf.wallpaper.and_then(stamp);
            if current_wall != installed_wall {
                wall_tex = match &current_wall {
                    Some((p, _)) => match upload_wallpaper(&gpu, p) {
                        Ok(t) => {
                            println!("rill-compositor: wallpaper {}", p.display());
                            Some(t)
                        }
                        Err(e) => {
                            eprintln!("rill-compositor: wallpaper rejected: {e}");
                            None
                        }
                    },
                    None => None,
                };
                installed_wall = current_wall;
                state.needs_redraw = true;
            }
            // Background (shader wallpaper) slot, same lifecycle as the
            // effect: act only when (path, mtime) changes.
            let current_bg = fx_conf.background.and_then(stamp);
            if current_bg != installed_bg {
                match &current_bg {
                    Some((p, _)) => match std::fs::read_to_string(p) {
                        Ok(src) => match renderer.set_background(Some(&src)) {
                            Ok(animated) => {
                                bg_animated = animated;
                                bg_param_decls = rill_appkit::params::shader_params(&src);
                                bg_label = format!(
                                    "{} ({})",
                                    p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
                                    if animated { "animated" } else { "static" }
                                );
                                println!("rill-compositor: background shader {}", p.display());
                            }
                            Err(e) => eprintln!("rill-compositor: background rejected: {e}"),
                        },
                        Err(e) => eprintln!("rill-compositor: background unreadable: {e}"),
                    },
                    None => {
                        let _ = renderer.set_background(None);
                        bg_animated = false;
                        bg_param_decls.clear();
                        bg_label = "off".into();
                        println!("rill-compositor: background shader cleared");
                    }
                }
                installed_bg = current_bg;
                state.needs_redraw = true;
            }
            // Model layer: reload when either the mesh or its shader moves.
            // The OBJ parse (~100ms for a 580k-triangle car) runs only then.
            let current_model = (
                fx_conf.model.clone().and_then(stamp),
                fx_conf.model_shader.clone().and_then(stamp),
            );
            if current_model != installed_model {
                match &current_model {
                    (Some((mp, _)), Some((sp, _))) => {
                        let loaded = rill_gpu::mesh::load(mp).and_then(|mesh| {
                            let src = std::fs::read_to_string(sp).map_err(|e| e.to_string())?;
                            renderer.set_model(Some(&src), Some(&mesh))?;
                            Ok(mesh.vertices.len() / 3)
                        });
                        match loaded {
                            Ok(tris) => println!(
                                "rill-compositor: model {} ({tris} triangles)",
                                mp.display()
                            ),
                            Err(e) => eprintln!("rill-compositor: model rejected: {e}"),
                        }
                    }
                    _ => {
                        let _ = renderer.set_model(None, None);
                        if installed_model != (None, None) {
                            println!("rill-compositor: model cleared");
                        }
                    }
                }
                installed_model = current_model;
                state.needs_redraw = true;
            }
            let warp = fx_conf.warp;
            let current = fx_conf.shader.and_then(stamp);
            if current != installed_shader {
                match &current {
                    Some((p, _)) => match std::fs::read_to_string(p) {
                        Ok(src) => match renderer.set_effect(Some(&src)) {
                            Ok(animated) => {
                                fx_animated = animated;
                                fx_param_decls = rill_appkit::params::shader_params(&src);
                                state.pointer_warp = warp;
                                effect_label = format!(
                                    "{} ({}{})",
                                    p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
                                    if animated { "animated" } else { "static" },
                                    if warp.is_some() { ", warped input" } else { "" }
                                );
                                println!(
                                    "rill-compositor: effect shader {} ({}{})",
                                    p.display(),
                                    if animated { "animated" } else { "static" },
                                    if warp.is_some() { ", input warped" } else { "" }
                                );
                            }
                            // Keep the previous effect: a broken hot-reload
                            // must never take the desktop down.
                            Err(e) => eprintln!("rill-compositor: shader rejected: {e}"),
                        },
                        Err(e) => eprintln!("rill-compositor: shader unreadable: {e}"),
                    },
                    None => {
                        let _ = renderer.set_effect(None);
                        fx_animated = false;
                        fx_param_decls.clear();
                        state.pointer_warp = None;
                        effect_label = "off".into();
                        println!("rill-compositor: effect shader cleared");
                    }
                }
                installed_shader = current;
                state.needs_redraw = true;
            }
            // The per-window effect, on the same hot-reload contract as the
            // grader above: a rejected shader leaves the previous one
            // installed rather than taking the desktop down.
            let current_wfx = fx_conf.window_shader.and_then(stamp);
            if current_wfx != installed_window_shader {
                match &current_wfx {
                    Some((p, _)) => match std::fs::read_to_string(p) {
                        Ok(src) => match renderer.set_window_fx(Some(&src)) {
                            Ok(animated) => {
                                window_fx_animated = animated;
                                window_param_decls = rill_appkit::params::shader_params(&src);
                                println!(
                                    "rill-compositor: window shader {} ({})",
                                    p.display(),
                                    if animated { "animated" } else { "static" }
                                );
                            }
                            Err(e) => {
                                eprintln!("rill-compositor: window shader rejected: {e}")
                            }
                        },
                        Err(e) => eprintln!("rill-compositor: window shader unreadable: {e}"),
                    },
                    None => {
                        let _ = renderer.set_window_fx(None);
                        window_fx_animated = false;
                        window_param_decls.clear();
                        println!("rill-compositor: window shader cleared");
                    }
                }
                installed_window_shader = current_wfx;
                state.needs_redraw = true;
            }
            // The particle passes. Both are reloaded together because they
            // share one state buffer: a new update shader with the old draw
            // shader over it looks like a bug in neither.
            let current_particles = (
                fx_conf.particle_shader.and_then(stamp),
                fx_conf.particle_render.and_then(stamp),
                fx_conf.particle_diffuse.and_then(stamp),
            );
            if current_particles != installed_particles {
                let read = |slot: &Option<(std::path::PathBuf, std::time::SystemTime)>| {
                    slot.as_ref().and_then(|(p, _)| std::fs::read_to_string(p).ok())
                };
                let (cs, rs, ds) = (
                    read(&current_particles.0),
                    read(&current_particles.1),
                    read(&current_particles.2),
                );
                match renderer.set_particle_shaders_with(
                    cs.as_deref(),
                    rs.as_deref(),
                    ds.as_deref(),
                ) {
                    Ok(()) => println!(
                        "rill-compositor: particles {} / {}",
                        current_particles
                            .0
                            .as_ref()
                            .map(|(p, _)| p.display().to_string())
                            .unwrap_or_else(|| "built-in flock".into()),
                        current_particles
                            .1
                            .as_ref()
                            .map(|(p, _)| p.display().to_string())
                            .unwrap_or_else(|| "built-in draw".into()),
                    ),
                    Err(e) => eprintln!("rill-compositor: particle shader rejected: {e}"),
                }
                installed_particles = current_particles;
                state.needs_redraw = true;
            }
            // Declared parameter values: theme overrides laid over each
            // slot's declared defaults, clamped to the declared range and
            // uploaded only when the packed rows actually change — which is
            // what makes a studio slider move the desktop within a poll.
            let overlay = |decls: &[rill_appkit::params::ShaderParam],
                           installed: &Option<(std::path::PathBuf, std::time::SystemTime)>|
             -> [[f32; 4]; 8] {
                let stem = installed
                    .as_ref()
                    .and_then(|(p, _)| p.file_stem())
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let overrides = fx_conf.shader_params.get(&stem);
                let values: Vec<f32> = decls
                    .iter()
                    .map(|d| {
                        overrides
                            .and_then(|list| list.iter().find(|(n, _)| *n == d.name))
                            .map(|(_, v)| (*v as f32).clamp(d.min, d.max))
                            .unwrap_or(d.default)
                    })
                    .collect();
                rill_appkit::params::pack(&values)
            };
            let uploads: [ParamUpload<'_>; 3] = [
                (0, overlay(&bg_param_decls, &installed_bg), &|r| {
                    renderer.set_background_params(r)
                }),
                (1, overlay(&fx_param_decls, &installed_shader), &|r| {
                    renderer.set_effect_params(r)
                }),
                (2, overlay(&window_param_decls, &installed_window_shader), &|r| {
                    renderer.set_window_fx_params(r)
                }),
            ];
            for (slot, rows, send) in uploads {
                if sent_param_rows[slot] != Some(rows) {
                    send(rows);
                    sent_param_rows[slot] = Some(rows);
                    state.needs_redraw = true;
                }
            }
        }
        // An animated shader (either slot) or a live flock needs continuous
        // frames; the frame budget paces the loop. Static shaders keep the
        // damage-gated idle win.
        if fx_animated
            || window_fx_animated
            || bg_animated
            || installed_boids > 0
            || renderer.model_active()
        {
            state.needs_redraw = true;
        }
        // Sound-reactive shaders read `fx_audio`, not `time`, so the
        // reads-time probe alone would let the desktop idle mid-song.
        // Advance the analysis every iteration (30Hz-gated internally, so
        // this is nearly free) — polling only inside the composite path
        // would leave `active()` stale while idle, and music starting in
        // silence could then never wake the loop. While anything is audible
        // and any shader slot is live, keep frames coming; silence hands
        // the idle win straight back.
        let _ = audio_tap.fx();
        if audio_tap.active()
            && (installed_shader.is_some()
                || installed_window_shader.is_some()
                || installed_bg.is_some()
                || installed_boids > 0)
        {
            state.needs_redraw = true;
        }
        // Idle pacing: when nothing needs compositing, block in the host
        // event pump briefly instead of spinning — input still wakes us
        // within ~4ms, and wayland requests are dispatched every iteration.
        // Adaptive: tight while anything animated recently (client commits
        // wake us only via this timeout), relaxed when truly quiet. Host
        // input always interrupts the wait, so idle input latency is unhurt.
        // Frame budget: even with damage pending, render at most ~66 fps.
        // Frame callbacks are the only pacing clients get (present here is
        // non-blocking), so an unthrottled commit→render→callback cycle
        // would let one eager client spin the whole compositor at 100%.
        let since_render = last_render.elapsed();
        let pump_timeout = if state.needs_redraw {
            frame_budget.saturating_sub(since_render)
        } else if since_render < std::time::Duration::from_millis(200) {
            std::time::Duration::from_millis(4)
        } else {
            std::time::Duration::from_millis(32)
        };
        let status = event_loop.pump_app_events(Some(pump_timeout), &mut host);
        if shutting_down() {
            println!("rill-compositor: shutting down");
            if state.recorder.is_some() {
                println!("rill-compositor: {}", state.toggle_recording());
            }
            frame_report(total_frames, heartbeat_frames, start_time.elapsed(), &frame_times, &acquire_times, state.total_commits);
            return Ok(());
        }
        if let PumpStatus::Exit(_) = status {
            if state.recorder.is_some() {
                println!("rill-compositor: {}", state.toggle_recording());
            }
            frame_report(total_frames, heartbeat_frames, start_time.elapsed(), &frame_times, &acquire_times, state.total_commits);
            return Ok(());
        }
        let inner = window.inner_size();
        let size: Size<i32, Physical> = (inner.width as i32, inner.height as i32).into();
        for event in std::mem::take(&mut host.events) {
            if handle_window_event(
                event,
                &mut state,
                &keyboard,
                &pointer,
                &mut pointer_loc,
                &start_time,
                &window,
            ) {
                if state.recorder.is_some() {
                    println!("rill-compositor: {}", state.toggle_recording());
                }
                frame_report(total_frames, heartbeat_frames, start_time.elapsed(), &frame_times, &acquire_times, state.total_commits);
                return Ok(());
            }
        }

        // Stats HUD: sample /proc for the compositor + every client at 2 Hz
        // while visible. Sampling marks damage, so the HUD live-updates even
        // on an otherwise idle desktop (that ~2 fps *is* its cost).
        if state.show_hud && hud_last_sample.elapsed() > std::time::Duration::from_millis(500) {
            let dt = hud_last_sample.elapsed().as_secs_f32();
            hud_last_sample = std::time::Instant::now();

            // Window count per client pid (credentials off the wl connection).
            let mut windows_of: HashMap<i32, u32> = HashMap::new();
            for window in state.space.elements() {
                if let Some(toplevel) = window.toplevel()
                    && let Some(client) = toplevel.wl_surface().client()
                    && let Ok(creds) = client.get_credentials(&state.display_handle)
                {
                    *windows_of.entry(creds.pid).or_default() += 1;
                }
            }
            let mut pids: Vec<i32> = windows_of.keys().copied().collect();
            pids.sort_unstable();
            pids.retain(|p| *p != own_pid);
            pids.insert(0, own_pid);

            let mut procs: Vec<HudProc> = Vec::new();
            let mut seen: HashMap<i32, u64> = HashMap::new();
            for pid in pids {
                let Some((ticks, rss)) = proc_sample(pid) else { continue };
                seen.insert(pid, ticks);
                let prev = hud_prev.get(&pid).copied().unwrap_or(ticks);
                // Linux USER_HZ is 100: ticks are centiseconds of CPU.
                let cpu_pct = if dt > 0.0 {
                    (ticks.saturating_sub(prev)) as f32 / dt
                } else {
                    0.0
                };
                let name = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| pid.to_string());
                procs.push(HudProc {
                    name,
                    pid,
                    cpu_pct,
                    rss_mb: rss as f32 / (1024.0 * 1024.0),
                    windows: windows_of.get(&pid).copied().unwrap_or(0),
                });
            }
            hud_prev = seen;

            let stream_windows = state.streams.values().filter(|s| s.current.is_some()).count();
            let stream_bytes: usize =
                state.streams.values().filter_map(|s| s.current.as_ref()).map(|f| f.wire_len).sum();
            let (cpu_total, rss_total) =
                procs.iter().fold((0.0, 0.0), |(c, r), p| (c + p.cpu_pct, r + p.rss_mb));

            hud_lines.clear();
            hud_lines.push(format!(
                "{:<17}{:>6}  {:>8}  {:>4}",
                "process", "cpu", "rss", "win"
            ));
            for p in &procs {
                let name = if p.pid == own_pid {
                    format!("{} *", p.name)
                } else {
                    p.name.clone()
                };
                hud_lines.push(format!(
                    "{:<17}{:>5.1}%  {:>6.1}MB  {:>4}",
                    if name.len() > 17 { name[..17].to_string() } else { name },
                    p.cpu_pct,
                    p.rss_mb,
                    p.windows,
                ));
            }
            hud_lines.push(format!(
                "{:<17}{:>5.1}%  {:>6.1}MB  {:>4}",
                "desktop total",
                cpu_total,
                rss_total,
                windows_of.values().sum::<u32>(),
            ));
            hud_lines.push(String::new());
            hud_lines.push(format!(
                "frames  {:>3}/s composited · {:>3}/s client commits",
                (render_count as f32 / dt).round() as u32,
                (state.commit_count as f32 / dt).round() as u32,
            ));
            hud_lines.push(format!(
                "streams {} vector windows · {:.1} KB live scene",
                stream_windows,
                stream_bytes as f32 / 1024.0,
            ));
            hud_lines.push(format!("effect  {effect_label}"));
            hud_lines.push(format!("bg      {bg_label}"));
            if installed_boids > 0 {
                hud_lines.push(format!(
                    "boids   {installed_boids} agents · GPU compute · windows as terrain"
                ));
            }
            hud_lines.push(format!("gpu     {}", renderer.adapter_name()));
            render_count = 0;
            state.commit_count = 0;
            state.needs_redraw = true;
        }

        // Damage gate: composite only when something visible changed; a
        // quiet desktop renders nothing. A 1 Hz heartbeat self-heals any
        // missed damage flag.
        let mut by_heartbeat = false;
        if !state.needs_redraw && last_render.elapsed() > std::time::Duration::from_secs(1) {
            state.needs_redraw = true;
            by_heartbeat = true;
        }
        // Render only when damage exists *and* the frame budget allows (input
        // can wake the pump early; the redraw then waits for its slot).
        if state.needs_redraw && last_render.elapsed() >= frame_budget {
        // A gap is only news when it is *late*. Measured against the frame
        // threshold it fired on every ordinary 60 Hz interval — hundreds of
        // "frame gap 15.9ms" lines saying nothing but "we are running at the
        // refresh rate", which buried the handful of lines that meant
        // something. A missed frame is a gap past the budget, not a gap.
        if let (Some(threshold), Some(prev)) = (frame_log_threshold, last_frame_end) {
            let gap = prev.elapsed();
            if gap > frame_budget + threshold {
                println!(
                    "rill-compositor: frame gap {:.1}ms at t={:.1}s (budget {:.1}ms)",
                    gap.as_secs_f64() * 1000.0,
                    start_time.elapsed().as_secs_f64(),
                    frame_budget.as_secs_f64() * 1000.0,
                );
            }
        }
        state.needs_redraw = false;
        last_render = std::time::Instant::now();
        render_count = render_count.saturating_add(1);
        total_frames += 1;
        if by_heartbeat {
            heartbeat_frames += 1;
        }
        // (Re)configure the swapchain when the window size changes. Clamp to
        // the device's texture cap so an oversized window can't panic the
        // configure (belt to the adapter-limits suspenders).
        let max_dim = gpu.device.limits().max_texture_dimension_2d as i32;
        let size: Size<i32, Physical> = (size.w.min(max_dim), size.h.min(max_dim)).into();
        if size.w > 0 && size.h > 0 && configured_size != Some(size) {
            let reconfigure_started = std::time::Instant::now();
            wgpu_surface.configure(
                &gpu.device,
                &wgpu::SurfaceConfiguration {
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    format: surface_format,
                    width: size.w as u32,
                    height: size.h as u32,
                    present_mode: present_mode(&caps.present_modes),
                    desired_maximum_frame_latency: 2,
                    alpha_mode: wgpu::CompositeAlphaMode::Auto,
                    view_formats: vec![],
                },
            );
            configured_size = Some(size);
            // Every step of a window drag lands here, rebuilding the
            // swapchain, and it is the one part of a resize the frame log
            // could not see: it happens before `acquire` and is charged to
            // neither phase. A drag that feels heavy while the frames either
            // side of it read a millisecond is this, or it is not in the
            // compositor at all.
            if let Some(threshold) = frame_log_threshold
                && reconfigure_started.elapsed() > threshold
            {
                println!(
                    "rill-compositor: slow reconfigure {:.1}ms at t={:.1}s ({}x{})",
                    reconfigure_started.elapsed().as_secs_f64() * 1000.0,
                    start_time.elapsed().as_secs_f64(),
                    size.w,
                    size.h,
                );
            }
        }
        let acquire_started = std::time::Instant::now();
        let frame = match wgpu_surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                configured_size = None; // reconfigure next iteration
                continue;
            }
            Err(e) => {
                eprintln!("rill-compositor: surface error: {e}");
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
        };
        let acquire_took = acquire_started.elapsed();
        acquire_times.record(acquire_took);
        let frame_view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Gather every mapped window's content bottom→top (the space iterates
        // bottom-up; composite draws in list order). A window is either
        // vector-native (a decoded command stream, translated to its spot) or
        // ordinary (a buffer imported/uploaded as a texture).
        enum WinContent {
            Tex { bundle: Arc<TexBundle>, x: f32, y: f32, scale: f32, alpha: f32 },
            /// Command content, and which surface's attached images its
            /// `Image` commands name. `None` for the compositor's own
            /// drawing (backdrops, the cursor), which refers to no images.
            Cmds { commands: Vec<DrawCommand>, surface: Option<ObjectId> },
            /// The per-window effect, at this window's z.
            Fx { window: u32, bounds: UiRect },
        }
        // Which index each window occupies in the `fx_windows` uniform array
        // built further down. The two have to agree exactly, or a window's
        // effect would be drawn using another window's geometry — so this
        // mirrors that loop's filter (skip the background, require a rect)
        // rather than approximating it. It cannot simply reuse that loop's
        // output: this is needed *while* the scene is assembled, and that
        // loop also samples window velocity, which must happen once a frame.
        let fx_index: std::collections::HashMap<ObjectId, u32> = state
            .space
            .elements()
            .filter(|w| Some(*w) != state.background.as_ref())
            .filter(|w| state.window_rect(w).is_some())
            .enumerate()
            .filter_map(|(i, w)| {
                w.toplevel().map(|t| (t.wl_surface().id(), i as u32))
            })
            .collect();
        let window_fx_on = renderer.has_window_fx();

        // Upload any images clients attached since the last pass. Dispatch
        // validated them; this is where the GPU device lives, and where the
        // CPU copies stop existing.
        for stream in state.streams.values_mut() {
            let clock = stream.image_clock;
            for image in stream.pending_images.drain(..) {
                let bundle =
                    upload_rgba(&gpu, &image.pixels, image.w, image.h, "stream image");
                let bytes = (image.w as usize) * (image.h as usize) * 4;
                stream.images.insert(image.source, HeldImage { bundle, bytes, used: clock });
            }
        }

        let mut contents: Vec<WinContent> = Vec::new();
        let mut seen: Vec<ObjectId> = Vec::new();
        // Where the shader wallpaper slots in: directly above the shell's
        // background window (so it replaces the pixel wallpaper visually)
        // but below every app window.
        let mut shader_slot = 0usize;
        let focused_window = state.top_app_window();
        let mut anim_running = false;
        // The dock walks last regardless of stacking: its stream may paint a
        // menu above the strip, and that overlay must land on top of every
        // app window — z is draw order.
        let mut walk_order: Vec<&Window> = Vec::new();
        let mut dock_last: Vec<&Window> = Vec::new();
        for w in state.space.elements() {
            if Some(w) == state.dock.as_ref() {
                dock_last.push(w);
            } else {
                walk_order.push(w);
            }
        }
        walk_order.extend(dock_last);
        for window in walk_order {
            let is_background = Some(window) == state.background.as_ref();
            let is_dock = Some(window) == state.dock.as_ref();
            let is_shell = is_background || is_dock;
            let Some(toplevel) = window.toplevel() else { continue };
            let loc = state.space.element_location(window).unwrap_or_default();
            let surface = toplevel.wl_surface();
            // Spawn animation: new windows scale and fade in over 220ms
            // (eased); shell surfaces map at boot and are exempt.
            let (anim_scale, anim_alpha) = if anims_enabled && !is_shell {
                state
                    .spawn_times
                    .get(&surface.id())
                    .map(|t0| {
                        let t = (t0.elapsed().as_secs_f32() / 0.22).min(1.0);
                        if t < 1.0 {
                            anim_running = true;
                        }
                        let e = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);
                        (0.90 + 0.10 * e, e)
                    })
                    .unwrap_or((1.0, 1.0))
            } else {
                (1.0, 1.0)
            };
            // Depth: every app window floats on a drop shadow the compositor
            // paints beneath it (clients can't paint outside their bounds).
            // The focused window additionally gets an accent Glow — an
            // edge-only ring, so nothing bleeds through translucent glass.
            // The window's own frame declares its shape: a glass window
            // opens with a full-surface rounded Backdrop; everything else is
            // square. Shadow, glow, and the content mask all follow this one
            // radius — which is what keeps the emphasis ring and the clipped
            // content the same shape.
            let win_radius = state
                .streams
                .get(&surface.id())
                .and_then(|s| s.current.as_ref())
                .and_then(|f| match f.commands.first() {
                    Some(DrawCommand::Backdrop { rect: b, corner_radius, .. })
                        if b.w >= f.width as f32 - 1.0
                            && b.h >= f.height as f32 - 1.0 =>
                    {
                        Some(*corner_radius)
                    }
                    _ => None,
                })
                .unwrap_or(0.0);
            // The per-window effect layer, if one is installed. Emitted
            // directly *above* this window's own content, which is the whole
            // point: it is then occluded by anything stacked higher for real,
            // and a glass window in front of it blurs it, because a backdrop
            // samples the scene as it stands when the backdrop is reached.
            // Shell surfaces are exempt — the wallpaper and the dock are not
            // things a person moves, and burning the dock reads as a bug.
            let fx_layer = (window_fx_on && !is_shell)
                .then(|| fx_index.get(&surface.id()).copied())
                .flatten()
                .and_then(|index| {
                    state.window_rect(window).map(|r| WinContent::Fx {
                        window: index,
                        bounds: UiRect {
                            x: r.loc.x as f32 - WINDOW_FX_REACH,
                            y: r.loc.y as f32 - WINDOW_FX_REACH,
                            w: r.size.w as f32 + WINDOW_FX_REACH * 2.0,
                            h: r.size.h as f32 + WINDOW_FX_REACH * 2.0,
                        },
                    })
                });
            if !is_shell
                && let Some(rect) = state.window_rect(window)
            {
                let focused = Some(window) == focused_window.as_ref();
                let radius = win_radius;
                let wrect = UiRect {
                    x: rect.loc.x as f32,
                    y: rect.loc.y as f32,
                    w: rect.size.w as f32,
                    h: rect.size.h as f32,
                };
                let mut under = vec![DrawCommand::Shadow {
                    rect: wrect,
                    color: window_dress.1,
                    blur: window_dress.3,
                    spread: 0.0,
                    corner_radius: radius,
                }];
                if focused {
                    under.push(DrawCommand::Glow {
                        rect: wrect,
                        color: window_dress.0,
                        blur: window_dress.2,
                        corner_radius: radius,
                    });
                }
                contents.push(WinContent::Cmds { commands: under, surface: None });
            }
            // Vector-native window: its frame is DrawCommands, not pixels.
            // Clip to the declared bounds — a client must not paint outside
            // its own window (and mid-resize frames would otherwise bleed
            // past the shrinking rect).
            if let Some(frame) =
                state.streams.get(&surface.id()).and_then(|s| s.current.as_ref())
            {
                // Spawn scale is a pure command-space transform about the
                // window centre — vectors animate crisp, never resampled.
                let (fw, fh) = (frame.width as f32, frame.height as f32);
                let dx = loc.x as f32 + fw * (1.0 - anim_scale) * 0.5;
                let dy = loc.y as f32 + fh * (1.0 - anim_scale) * 0.5;
                let mut clipped = Vec::with_capacity(frame.commands.len() + 2);
                // A client must not paint outside its own window — except the
                // dock, whose menu escapes the strip: the trusted shell
                // surface clips to the output instead, and input follows its
                // painted extent (window_under).
                let clip_rect = if is_dock {
                    UiRect {
                        x: 0.0,
                        y: 0.0,
                        w: state.output_size.w as f32,
                        h: state.output_size.h as f32,
                    }
                } else {
                    UiRect { x: dx, y: dy, w: fw * anim_scale, h: fh * anim_scale }
                };
                clipped.push(DrawCommand::PushClip {
                    rect: clip_rect,
                    radius: win_radius * anim_scale,
                });
                let scaled;
                let body = if anim_scale < 1.0 {
                    scaled = rill_ui::stream::scale_commands(&frame.commands, anim_scale);
                    &scaled
                } else {
                    &frame.commands
                };
                clipped.extend(rill_ui::stream::offset_commands(body, dx, dy));
                clipped.push(DrawCommand::PopClip);
                contents.push(WinContent::Cmds { commands: clipped, surface: Some(surface.id()) });
                contents.extend(fx_layer);
                continue;
            }
            // Pixel-native window: a foreign Wayland client. Its content is
            // a *tree* of surfaces, not one buffer — GTK and Firefox park
            // the actual pixels on subsurfaces and often leave the root
            // unbuffered, so reading only the root drew them as hollow
            // frames. Walk the tree in paint order (the upward walk is
            // back-to-front, honouring place_above/below), accumulating
            // each subsurface's offset from its parent, and import every
            // attached buffer through the same dmabuf/shm paths.
            let mut tree: Vec<(Arc<TexBundle>, f32, f32)> = Vec::new();
            collect_content_tree(
                surface,
                Point::default(),
                &gpu,
                &mut dmabuf_cache,
                &mut seen,
                &mut tree,
            );
            // Popups float above the window's own content — pushed after
            // it, because draw order is z. Their tracked offsets are
            // relative to the root surface; subtracting the popup's own
            // geometry offset lands on its surface origin.
            for (popup, off) in PopupManager::popups_for_surface(surface) {
                collect_content_tree(
                    popup.wl_surface(),
                    off - popup.geometry().loc,
                    &gpu,
                    &mut dmabuf_cache,
                    &mut seen,
                    &mut tree,
                );
            }
            // One trail event per *change* in a window's buffer count, not
            // per frame: "buffers=0 forever" or "never left 1" is exactly
            // the shape the empty-frame bug had.
            if rill_log::dev_active() {
                let id = surface.id();
                if tree_counts.get(&id) != Some(&tree.len()) {
                    rill_log::dev!(
                        "rill-compositor",
                        "foreign_tree",
                        surface = id,
                        buffers = tree.len(),
                    );
                    tree_counts.insert(id, tree.len());
                }
            }
            if tree.is_empty() {
                continue;
            }
            // Glass reaches the shell: frost under the dock and let the
            // desktop show through its pixels.
            let tex_alpha = if is_dock && glass_on {
                if let Some(rect) = state.window_rect(window) {
                    contents.push(WinContent::Cmds { surface: None, commands: vec![DrawCommand::Backdrop {
                        rect: UiRect {
                            x: rect.loc.x as f32,
                            y: rect.loc.y as f32,
                            w: rect.size.w as f32,
                            h: rect.size.h as f32,
                        },
                        blur: 22.0,
                        corner_radius: 0.0,
                    }] });
                }
                0.85 * anim_alpha
            } else {
                anim_alpha
            };
            for (bundle, ox, oy) in tree {
                contents.push(WinContent::Tex {
                    bundle,
                    x: loc.x as f32 + ox * anim_scale,
                    y: loc.y as f32 + oy * anim_scale,
                    scale: anim_scale,
                    alpha: tex_alpha,
                });
            }
            contents.extend(fx_layer);
            if is_background {
                shader_slot = contents.len();
            }
        }
        if anim_running {
            state.needs_redraw = true;
        }
        dmabuf_cache.retain(|k, _| seen.contains(k));
        if !tree_counts.is_empty() {
            tree_counts.retain(|k, _| {
                state
                    .space
                    .elements()
                    .any(|w| w.toplevel().is_some_and(|t| t.wl_surface().id() == *k))
            });
        }

        // The images each command layer may name, resolved before `scene` so
        // the borrows outlive it. A window's images are its own: an image
        // source is a bare string, and two windows saying "/logo.png" mean
        // different files if they are talking to different servers.
        let layer_images: Vec<Option<StreamImages<'_>>> = contents
            .iter()
            .map(|content| match content {
                WinContent::Cmds { surface: Some(id), .. } => {
                    state.streams.get(id).map(|s| StreamImages(&s.images))
                }
                _ => None,
            })
            .collect();

        let mut scene: Vec<SceneLayer> = contents
            .iter()
            .enumerate()
            .map(|(i, content)| match content {
                WinContent::Tex { bundle, x, y, scale, alpha } => {
                    let (w, h) = (bundle.w as f32, bundle.h as f32);
                    SceneLayer::Texture {
                        view: &bundle.view,
                        rect: UiRect {
                            x: x + w * (1.0 - scale) * 0.5,
                            y: y + h * (1.0 - scale) * 0.5,
                            w: w * scale,
                            h: h * scale,
                        },
                        alpha: *alpha,
                    }
                }
                WinContent::Cmds { commands, .. } => match &layer_images[i] {
                    Some(images) => SceneLayer::Commands { commands, images },
                    None => SceneLayer::commands(commands),
                },
                WinContent::Fx { window, bounds } => {
                    SceneLayer::WindowFx { window: *window, bounds: *bounds }
                }
            })
            .collect();
        // Pixel wallpaper: the scene's bottom layer, cover-fitted — scaled
        // by the larger axis ratio and centred, so the image keeps its
        // aspect and the overflow is cropped by the pass rather than the
        // picture being stretched to the output's shape. The shader layer
        // sits directly above it, so an installed background shader
        // replaces it visually — same stacking as when the shell's
        // background window carried the image.
        if let Some(t) = &wall_tex {
            let (out_w, out_h) = (size.w as f32, size.h as f32);
            let scale = (out_w / t.w.max(1) as f32).max(out_h / t.h.max(1) as f32);
            let (w, h) = (t.w as f32 * scale, t.h as f32 * scale);
            scene.insert(
                0,
                SceneLayer::Texture {
                    view: &t.view,
                    rect: UiRect {
                        x: (out_w - w) / 2.0,
                        y: (out_h - h) / 2.0,
                        w,
                        h,
                    },
                    alpha: 1.0,
                },
            );
            shader_slot += 1;
        }
        // Shader wallpaper (a no-op layer unless a background is installed),
        // then the model showcase just above it — both under every window.
        scene.insert(shader_slot.min(scene.len()), SceneLayer::Shader);
        scene.insert((shader_slot + 1).min(scene.len()), SceneLayer::Model);
        // The flock: step the compute sim with the live window layout as
        // its obstacle field, then split it around the windows — back half
        // above the wallpaper, front half over everything.
        // The live window layout: obstacle field for the flock, uniform
        // array for window-aware wallpaper/effect shaders.
        let mut window_rects: Vec<[f32; 4]> = Vec::new();
        // Scene semantics beside the geometry: spawn age, focus, kind, and
        // speed — what lets a wallpaper aura the active window, ripple at a
        // spawn, or wake behind a drag. Same order as the rects (bottom→top).
        let mut window_meta: Vec<[f32; 4]> = Vec::new();
        let mut window_vel: Vec<[f32; 4]> = Vec::new();
        let fx_focused = state.top_app_window();
        let now_inst = std::time::Instant::now();
        for window in state.space.elements() {
            if Some(window) == state.background.as_ref() {
                continue;
            }
            if let Some(rect) = state.window_rect(window) {
                window_rects.push([
                    rect.loc.x as f32,
                    rect.loc.y as f32,
                    rect.size.w as f32,
                    rect.size.h as f32,
                ]);
                let surface_id = window.toplevel().map(|t| t.wl_surface().id());
                let spawn_age = surface_id
                    .as_ref()
                    .and_then(|id| state.spawn_times.get(id))
                    .map(|t| t.elapsed().as_secs_f32())
                    .unwrap_or(f32::MAX);
                let focused = (Some(window) == fx_focused.as_ref()) as u32 as f32;
                // kind: 0 = app window, 1 = the dock strip, 2 = a desktop
                // widget. A shader that reacts to windows should react to
                // the things a person moves, not to the furniture.
                let kind = if Some(window) == state.dock.as_ref() {
                    1.0
                } else if state.is_widget(window) {
                    2.0
                } else {
                    0.0
                };
                let (x, y) = (rect.loc.x as f32, rect.loc.y as f32);
                let velocity = surface_id
                    .and_then(|id| {
                        let prev = state.prev_window_pos.insert(id.clone(), (x, y, now_inst))?;
                        let dt = now_inst.duration_since(prev.2).as_secs_f32();
                        if dt <= 1e-4 {
                            return state.window_velocity.get(&id).copied();
                        }
                        let instant = ((x - prev.0) / dt, (y - prev.1) / dt);
                        // Exponential trail: fast enough to follow a drag,
                        // slow enough that letting go coasts to a stop over
                        // roughly a fifth of a second.
                        let alpha = 1.0 - (-dt / 0.12f32).exp();
                        let was = state.window_velocity.get(&id).copied().unwrap_or((0.0, 0.0));
                        let now = (
                            was.0 + (instant.0 - was.0) * alpha,
                            was.1 + (instant.1 - was.1) * alpha,
                        );
                        state.window_velocity.insert(id, now);
                        Some(now)
                    })
                    .unwrap_or((0.0, 0.0));
                let speed = (velocity.0.powi(2) + velocity.1.powi(2)).sqrt();
                window_meta.push([spawn_age, focused, kind, speed]);
                window_vel.push([velocity.0, velocity.1, 0.0, 0.0]);
            }
        }
        if installed_boids > 0 {
            let dt = last_boid_step.elapsed().as_secs_f32();
            last_boid_step = std::time::Instant::now();
            renderer.step_boids(
                dt,
                start_time.elapsed().as_secs_f32(),
                &window_rects,
                &window_vel,
                [pointer_loc.x as f32, pointer_loc.y as f32],
                size.w as u32,
                size.h as u32,
            );
            scene.insert(
                (shader_slot + 1).min(scene.len()),
                SceneLayer::Boids { front: false },
            );
            scene.push(SceneLayer::Boids { front: true });
        }

        // Focus is indicated by the accent glow painted *under* the top app
        // window (see the shadow layer in the gather loop) — light, not a
        // box. The overlay slot remains for future on-top chrome.
        let overlay: Vec<DrawCommand> = Vec::new();
        if !overlay.is_empty() {
            scene.push(SceneLayer::commands(&overlay));
        }

        // Stats HUD: a frosted vector panel the compositor draws itself —
        // the same Backdrop primitive clients use (D6, dogfooded).
        let mut hud_overlay: Vec<DrawCommand> = Vec::new();
        // The recording indicator — always drawn while history is on,
        // because an always-on recorder whose indicator can be hidden is the
        // shape that got Recall in trouble (specs/history.md decision 1: the
        // indicator never lies). Deliberately quiet: a dot and three
        // letters, top-right, under the HUD if both are up.
        let mut badge = Vec::new();
        if state.history.is_some() {
            // The indicator (specs/history.md decision 1: it never lies).
            // Not a red dot — red-dot-plus-"rec" reads as a camera pointed
            // at you, and what this marks is the machine keeping its own
            // memory. A small clock in phosphor green instead, with the
            // faint afterglow of the CRTs the colour is named for: quiet
            // enough to live in the corner all day, present enough that
            // its absence would be noticed.
            let size_px = 13.0f32;
            let x = size.w as f32 - size_px - 10.0;
            // Vertically centred in the dock strip: the badge lives on the
            // same shelf as the clock and the launchers, so it should sit
            // on their line, not float near it.
            let y = ((state.dock_height as f32 - size_px) / 2.0).max(4.0);
            let phosphor = UiColor { r: 0x7d, g: 0xe8, b: 0xa8, a: 0xB4 };
            badge.push(DrawCommand::Glow {
                rect: UiRect { x: x - 2.0, y: y - 2.0, w: size_px + 4.0, h: size_px + 4.0 },
                color: UiColor { a: 0x2E, ..phosphor },
                blur: 7.0,
                corner_radius: size_px,
            });
            if let Some(glyph) = rill_ui::icons::icon("clock-fill") {
                let (points, contours) = glyph.at(x, y, size_px);
                badge.push(DrawCommand::FillPath { points, contours, color: phosphor });
            }
            scene.push(SceneLayer::commands(&badge));
        }
        if state.show_hud && !hud_lines.is_empty() {
            let (line_h, pad, font) = (16.0f32, 14.0f32, 11.0f32);
            let panel_w = 420.0f32;
            let panel_h = pad * 2.0 + 22.0 + hud_lines.len() as f32 * line_h;
            let x = size.w as f32 - panel_w - 14.0;
            let y = 14.0f32;
            hud_overlay.push(DrawCommand::Backdrop {
                rect: UiRect { x, y, w: panel_w, h: panel_h },
                blur: 26.0,
                corner_radius: 12.0,
            });
            hud_overlay.push(DrawCommand::Rect {
                rect: UiRect { x, y, w: panel_w, h: panel_h },
                color: UiColor { r: 16, g: 18, b: 30, a: 0x96 },
                corner_radius: 12.0,
            });
            hud_overlay.push(DrawCommand::Text {
                rect: UiRect { x: x + pad, y: y + pad - 2.0, w: panel_w - 2.0 * pad, h: 16.0 },
                text: "rill · live".into(),
                color: UiColor { r: 110, g: 168, b: 255, a: 255 },
                font_size: 12.0,
                font_weight: 700,
                font_family: "sans-serif".into(),
            });
            for (i, line) in hud_lines.iter().enumerate() {
                hud_overlay.push(DrawCommand::Text {
                    rect: UiRect {
                        x: x + pad,
                        y: y + pad + 22.0 + i as f32 * line_h,
                        w: panel_w - 2.0 * pad,
                        h: line_h,
                    },
                    text: line.clone(),
                    color: UiColor { r: 223, g: 228, b: 242, a: 235 },
                    font_size: font,
                    font_weight: 400,
                    font_family: "monospace".into(),
                });
            }
            scene.push(SceneLayer::commands(&hud_overlay));
        }
        // The pointer, drawn rather than borrowed: last in the scene, so it
        // sits above every window, the dock, and the stats readout.
        let cursor_cmds = if cursor_style.draw {
            let over = state.edge_cursor();
            let icon = over.unwrap_or(match &state.cursor_status {
                CursorImageStatus::Named(icon) => *icon,
                _ => CursorIcon::Default,
            });
            let hidden =
                over.is_none() && matches!(state.cursor_status, CursorImageStatus::Hidden);
            if hidden {
                Vec::new()
            } else {
                cursor_shape(
                    icon,
                    (pointer_loc.x as f32, pointer_loc.y as f32),
                    cursor_style,
                )
            }
        } else {
            Vec::new()
        };
        if !cursor_cmds.is_empty() {
            scene.push(SceneLayer::commands(&cursor_cmds));
        }
        let composite_started = std::time::Instant::now();
        renderer.composite_scene(
            &frame_view,
            size.w as u32,
            size.h as u32,
            clear_color, // `[desktop] background_color`, else the GLES near-black
            &scene,
            rill_gpu::FxInputs {
                time: start_time.elapsed().as_secs_f32(),
                clock: seconds_since_local_midnight(),
                scene: scene_params,
                cursor: [pointer_loc.x as f32, pointer_loc.y as f32],
                windows: window_rects,
                window_meta,
                window_velocity: window_vel,
                audio: audio_tap.fx(),
            },
        );
        let composite_took = composite_started.elapsed();
        let present_started = std::time::Instant::now();
        frame.present();
        let present_took = present_started.elapsed();
        let frame_took = last_render.elapsed();
        frame_times.record(frame_took);
        // A *slow frame* is not the same event as a long *gap between*
        // frames, and RILL_FRAME_LOG only reported the latter — so a 144 ms
        // frame, which is exactly the visible hitch anyone would complain
        // about, produced no line at all. Same threshold, both directions.
        if let Some(threshold) = frame_log_threshold
            && frame_took > threshold
        {
            // Split by phase: a stall in `composite` is our drawing, one in
            // `present` is the display or the driver, and one in neither is
            // the acquire or something between.
            println!(
                "rill-compositor: slow frame {:.1}ms at t={:.1}s \
                 (acquire {:.1} composite {:.1} present {:.1})",
                frame_took.as_secs_f64() * 1000.0,
                start_time.elapsed().as_secs_f64(),
                acquire_took.as_secs_f64() * 1000.0,
                composite_took.as_secs_f64() * 1000.0,
                present_took.as_secs_f64() * 1000.0
            );
        }
        last_frame_end = Some(std::time::Instant::now());

        let now = start_time.elapsed().as_millis() as u32;
        for window in state.space.elements() {
            if let Some(toplevel) = window.toplevel() {
                send_frames_surface_tree(toplevel.wl_surface(), now);
                // Popups animate too (menu hover states); starve their
                // frame callbacks and they draw once and freeze.
                for (popup, _) in PopupManager::popups_for_surface(toplevel.wl_surface()) {
                    send_frames_surface_tree(popup.wl_surface(), now);
                }
            }
        }
        } // damage gate

        // The socket layer must not be able to end the session. A client that
        // dies mid-connect, or a moment of fd pressure, surfaces here as an
        // error on accept or dispatch — and returning it from main() takes
        // down the compositor, meaning every window on the desktop, over one
        // peer's bad luck. Same rule the shader hot-reload already follows:
        // report it and keep drawing.
        match listener.accept() {
            Ok(Some(stream)) => {
                let _ = display.handle().insert_client(stream, Arc::new(ClientState::default()));
            }
            Ok(None) => {}
            Err(e) => eprintln!("rill-compositor: accept failed: {e}"),
        }
        if let Err(e) = display.dispatch_clients(&mut state) {
            eprintln!("rill-compositor: client dispatch failed: {e}");
        }
        if let Err(e) = display.flush_clients() {
            eprintln!("rill-compositor: client flush failed: {e}");
        }
        state.space.refresh();
        state.popups.cleanup();
        // A dead grabbing popup — the client closed its own menu — passes
        // the grab to the deepest popup still open in the chain, or ends
        // it. Keyboard focus never moved, so there is nothing to restore.
        if let Some((popup_s, root)) = state.popup_grab.take() {
            if smithay::utils::IsAlive::alive(&popup_s) {
                state.popup_grab = Some((popup_s, root));
            } else {
                state.popup_grab = PopupManager::popups_for_surface(&root)
                    .last()
                    .map(|(p, _)| (p.wl_surface().clone(), root));
                state.needs_redraw = true;
            }
        }
        // Window destruction has no handler; refresh() culls dead elements —
        // a count change means the picture changed. The widget list has to
        // be culled with it, or a closed widget keeps a slot that nothing
        // will ever place again.
        state.widgets.retain(|w| smithay::utils::IsAlive::alive(&w.window));
        // A count change means the picture changed.
        let window_count = state.space.elements().count();
        if window_count != last_window_count {
            last_window_count = window_count;
            state.needs_redraw = true;
        }

        // The widget list is not the only thing keyed by a window that has to
        // be culled with it. Each map below holds one entry per surface and
        // has no destroy path of its own — `record_ids` has one, but only for
        // rill_stream clients, so an ordinary buffer-backed window (a
        // terminal, anything foreign) leaves its entry behind forever. On a
        // compositor that stays up for days, closed windows are precisely
        // what accumulates.
        //
        // Every one of these maps holds at most one entry per mapped window,
        // so a length above the window count is the whole test for whether
        // there is anything to drop — and it costs nothing on the frames when
        // there isn't, which is nearly all of them.
        if state.spawn_times.len() > window_count
            || state.prev_window_pos.len() > window_count
            || state.window_velocity.len() > window_count
            || state.record_ids.len() > window_count
        {
            let live: std::collections::HashSet<ObjectId> = state
                .space
                .elements()
                .filter_map(|w| w.toplevel().map(|t| t.wl_surface().id()))
                .collect();
            state.spawn_times.retain(|id, _| live.contains(id));
            state.prev_window_pos.retain(|id, _| live.contains(id));
            state.window_velocity.retain(|id, _| live.contains(id));
            state.record_ids.retain(|id, _| live.contains(id));
        }

        // Feed the recorder the settled desktop once a tick — after refresh()
        // has culled dead windows, so a closed window is recorded as closed
        // rather than as a stale rectangle. The recorder writes only what
        // changed, so a still desktop costs nothing.
        if state.recorder.is_some() || state.history.is_some() {
            let snapshots = state.record_snapshots();
            let (px, py) = (pointer_loc.x as f32, pointer_loc.y as f32);
            if let Some(rec) = state.recorder.as_mut() {
                rec.sync(&snapshots);
                rec.pointer(px, py);
            }
            // The history writer gets the same settled desktop. No pointer
            // motion — history records clicks and outcomes, never the mouse
            // trail (specs/history.md, observation boundary) — and the
            // handle rate-limits, so per-damage-tick is fine to call.
            if let Some(hist) = state.history.as_mut() {
                let windows = snapshots
                    .iter()
                    .map(|s| rill_history::event::WindowState {
                        id: s.id,
                        x: s.x,
                        y: s.y,
                        w: s.w,
                        h: s.h,
                        title: s.title.clone(),
                        app: s.app.clone(),
                        vector: s.vector,
                        // The window's tier is its surface's latched tier —
                        // a T2 window's title must ride at T2, or the title
                        // leaks into the routine index.
                        tier: s.tier,
                    })
                    .collect();
                hist.tick(windows);
            }
        }

        // Reflect the focused client's requested cursor onto the host window
        // (the nested compositor has no cursor of its own to draw).
        let want = match &state.cursor_status {
            CursorImageStatus::Hidden => (false, CursorIcon::Default),
            CursorImageStatus::Named(icon) => (true, *icon),
            CursorImageStatus::Surface(_) => (true, CursorIcon::Default),
        };
        let want = if let Some(icon) = state.edge_cursor() { (true, icon) } else { want };
        let want = if cursor_style.draw { (false, want.1) } else { want };
        if want != applied_cursor {
            applied_cursor = want;
            window.set_cursor_visible(want.0);
            if want.0 {
                window.set_cursor(want.1);
            }
        }
    }
}

/// The winit side of the compositor: one window, events queued for the main
/// loop to translate (winit wants callback-style handling; the compositor
/// wants a loop).
struct Host {
    window: Option<Arc<WinitWindow>>,
    events: Vec<WindowEvent>,
}

impl ApplicationHandler for Host {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            // `RILL_FULLSCREEN=1` starts borderless-fullscreen on whatever
            // monitor the window lands on. For recording, mostly: a host
            // titlebar in the shot is the one thing that says "this is a
            // window on someone else's desktop". F11 toggles it live.
            let fullscreen = std::env::var_os("RILL_FULLSCREEN")
                .is_some_and(|v| v != "0")
                .then_some(Fullscreen::Borderless(None));
            let attrs = WinitWindow::default_attributes()
                .with_title("rill")
                .with_fullscreen(fullscreen)
                .with_inner_size({
                    let (w, h) = nested_output_size();
                    LogicalSize::new(w as f64, h as f64)
                });
            match event_loop.create_window(attrs) {
                Ok(window) => self.window = Some(Arc::new(window)),
                Err(e) => eprintln!("rill-compositor: create_window failed: {e}"),
            }
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        self.events.push(event);
    }
}

/// One imported/uploaded client buffer as a composite-ready texture.
struct TexBundle {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    w: u32,
    h: u32,
}

/// Collect every attached buffer in `root`'s surface tree — the surface
/// and its subsurfaces, transitively — into `tree` as (texture, x, y),
/// offsets relative to `base`, in paint order (the upward walk is
/// back-to-front and honours place_above/below). GTK and Firefox park
/// their pixels on subsurfaces and often leave the root unbuffered, so a
/// walk is the difference between a window and a hollow frame. Called
/// once per toplevel and once per popup. dmabufs import once, cached by
/// buffer id (`seen` keeps entries retained); shm re-uploads per call —
/// the same per-frame cost the cache comment above accepts.
fn collect_content_tree(
    root: &WlSurface,
    base: Point<i32, Logical>,
    gpu: &DmabufDevice,
    dmabuf_cache: &mut HashMap<ObjectId, Arc<TexBundle>>,
    seen: &mut Vec<ObjectId>,
    tree: &mut Vec<(Arc<TexBundle>, f32, f32)>,
) {
    with_surface_tree_upward(
        root,
        base,
        |_, states, off| {
            let mut off = *off;
            if states.role == Some("subsurface") {
                off += states.cached_state.get::<SubsurfaceCachedState>().current().location;
            }
            TraversalAction::DoChildren(off)
        },
        |child, states, off| {
            // The fold value is the *parent's* accumulated offset: this
            // surface's own offset applies both here and — via the filter
            // above — to its children.
            let mut off = *off;
            if states.role == Some("subsurface") {
                off += states.cached_state.get::<SubsurfaceCachedState>().current().location;
            }
            // NOT `with_renderer_surface_state`: the walk holds this
            // surface's lock while calling us, and that helper is
            // `with_states` inside — re-locking here is self-deadlock.
            // The walk's own `states` is the already-locked view.
            let Some(buffer) = states
                .data_map
                .get::<RendererSurfaceStateUserData>()
                .and_then(|d| d.lock().unwrap().buffer().cloned())
            else {
                return;
            };
            let bundle = if let Ok(dmabuf) = get_dmabuf(&buffer) {
                let id = buffer.id();
                match dmabuf_cache.get(&id) {
                    Some(bundle) => {
                        seen.push(id);
                        Some(bundle.clone())
                    }
                    None => match import_dmabuf(gpu, dmabuf) {
                        Ok(bundle) => {
                            let bundle = Arc::new(bundle);
                            rill_log::dev!(
                                "rill-compositor",
                                "dmabuf_import",
                                surface = child.id(),
                                w = bundle.w,
                                h = bundle.h,
                            );
                            dmabuf_cache.insert(id.clone(), bundle.clone());
                            seen.push(id);
                            Some(bundle)
                        }
                        Err(e) => {
                            eprintln!("rill-compositor: dmabuf import failed: {e}");
                            rill_log::dev!(
                                "rill-compositor",
                                "dmabuf_import_failed",
                                surface = child.id(),
                                error = e,
                            );
                            None
                        }
                    },
                }
            } else {
                match upload_shm(gpu, &buffer) {
                    Ok(bundle) => Some(Arc::new(bundle)),
                    Err(e) => {
                        eprintln!("rill-compositor: shm upload failed: {e}");
                        rill_log::dev!(
                            "rill-compositor",
                            "shm_upload_failed",
                            surface = child.id(),
                            error = e,
                        );
                        None
                    }
                }
            };
            if let Some(bundle) = bundle {
                tree.push((bundle, off.x as f32, off.y as f32));
            }
        },
        |_, _, _| true,
    );
}

/// Bind a client's dmabuf as a wgpu texture (single-plane ARGB/XRGB — what
/// the advertised format list promises).
fn import_dmabuf(gpu: &DmabufDevice, dmabuf: &Dmabuf) -> Result<TexBundle, String> {
    if dmabuf.num_planes() != 1 {
        return Err(format!("{}-plane dmabuf unsupported", dmabuf.num_planes()));
    }
    let format = dmabuf.format();
    let size = dmabuf.size();
    let fd = dmabuf
        .handles()
        .next()
        .ok_or("dmabuf has no fd")?
        .try_clone_to_owned()
        .map_err(|e| format!("dup fd: {e}"))?;
    let plan = DmabufPlan {
        width: size.w as u32,
        height: size.h as u32,
        fourcc: format.code as u32,
        modifier: format.modifier.into(),
        offset: dmabuf.offsets().next().unwrap_or(0) as u64,
        stride: dmabuf.strides().next().unwrap_or(0) as u64,
    };
    let texture = gpu.import(&plan, fd)?;
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Ok(TexBundle { _texture: texture, view, w: size.w as u32, h: size.h as u32 })
}

/// Upload a client's shm buffer as a wgpu texture.
/// Decode a wallpaper image file and upload it as a sampleable texture.
/// Same shape as [`upload_shm`], but sourced from disk and RGBA.
fn upload_wallpaper(gpu: &DmabufDevice, path: &std::path::Path) -> Result<TexBundle, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let rgba = image::load_from_memory(&bytes).map_err(|e| e.to_string())?.into_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    if w == 0 || h == 0 {
        return Err("empty image".into());
    }
    Ok(upload_rgba(gpu, &rgba, w, h, "wallpaper"))
}

/// Tightly packed RGBA8 → a sampled texture. `pixels` must be `w * h * 4`.
fn upload_rgba(gpu: &DmabufDevice, pixels: &[u8], w: u32, h: u32, label: &str) -> TexBundle {
    let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        size,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    TexBundle { _texture: texture, view, w, h }
}

fn upload_shm(gpu: &DmabufDevice, buffer: &wl_buffer::WlBuffer) -> Result<TexBundle, String> {
    with_buffer_contents(buffer, |ptr, len, data| {
        match data.format {
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => {}
            f => return Err(format!("shm format {f:?} unsupported")),
        }
        let (w, h) = (data.width as u32, data.height as u32);
        let (stride, offset) = (data.stride as usize, data.offset as usize);
        if w == 0 || h == 0 || offset + stride * h as usize > len {
            return Err("shm buffer out of bounds".into());
        }
        // Safety: with_buffer_contents guarantees ptr..ptr+len maps the pool.
        let bytes = unsafe { std::slice::from_raw_parts(ptr.add(offset), stride * h as usize) };
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shm"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride as u32),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Ok(TexBundle { _texture: texture, view, w, h })
    })
    .map_err(|e| format!("shm access: {e}"))?
}

/// Translate one winit window event into the same seat calls smithay's winit
/// backend made (scancode+8 keycodes, BTN_* button codes, real timestamps —
/// all-zeros mangled multi-byte sequences like the arrow keys). Returns `true`
/// when the compositor should exit.
fn handle_window_event(
    event: WindowEvent,
    state: &mut Rill,
    keyboard: &smithay::input::keyboard::KeyboardHandle<Rill>,
    pointer: &PointerHandle<Rill>,
    pointer_loc: &mut Point<f64, Logical>,
    start_time: &std::time::Instant,
    window: &WinitWindow,
) -> bool {
    let time = start_time.elapsed().as_millis() as u32;
    match event {
        WindowEvent::CloseRequested => return true,
        WindowEvent::Resized(new) => {
            state.needs_redraw = true;
            state.reflow_shell((new.width as i32, new.height as i32).into());
        }
        WindowEvent::Focused(focused) => {
            // The host took the keyboard mid-hold — alt-tab is the
            // canonical case: the Alt press was forwarded, but its release
            // lands in the host, so from every client's view (and our xkb
            // state's) Alt is held forever and each later letter arrives
            // as a chord instead of typing. Release whatever is still down
            // the moment the keys walk away.
            if !focused {
                let held: Vec<Keycode> = keyboard.pressed_keys().into_iter().collect();
                for key in &held {
                    keyboard.input::<(), _>(
                        state,
                        *key,
                        KeyState::Released,
                        SERIAL_COUNTER.next_serial(),
                        time,
                        |_, _, _| FilterResult::Forward,
                    );
                }
                rill_log::dev!("rill-compositor", "host_focus_lost", released = held.len());
            } else {
                rill_log::dev!("rill-compositor", "host_focus_gained");
            }
        }
        WindowEvent::KeyboardInput {
            event: KeyEvent { physical_key, state: key_state, repeat, .. },
            is_synthetic,
            ..
        } => {
            // Clients do their own key repeat (wl_keyboard repeat_info).
            // Synthetic events are winit's replay of keys already held when
            // focus arrives — forwarded, they type a spurious character.
            if repeat || is_synthetic {
                return false;
            }
            let scancode = physical_key.to_scancode().unwrap_or(0);
            let keycode = Keycode::from(scancode + 8);
            let pressed = match key_state {
                ElementState::Pressed => KeyState::Pressed,
                ElementState::Released => KeyState::Released,
            };
            let serial = SERIAL_COUNTER.next_serial();
            // Compositor-level shortcuts, intercepted here so they work
            // whatever has focus and never reach the client.
            //   Ctrl+Alt+R    toggle session recording
            //   Ctrl+Shift+R  cycle to the next saved rice
            let mut toggle = false;
            let mut cycle_rice = false;
            let mut toggle_fullscreen = false;
            keyboard.input::<(), _>(state, keycode, pressed, serial, time, |_, mods, handle| {
                let sym = handle.modified_sym();
                // F11 on its own, the convention everywhere else.
                if sym == Keysym::F11 {
                    toggle_fullscreen = pressed == KeyState::Pressed;
                    return FilterResult::Intercept(());
                }
                // Shift makes it `R`; accept both so the binding does not
                // depend on which symbol the layout reports.
                let is_r = sym == Keysym::r || sym == Keysym::R;
                if !is_r || !mods.ctrl {
                    return FilterResult::Forward;
                }
                if mods.alt {
                    // Swallow the release too, or the client sees a key it
                    // never saw pressed.
                    toggle = pressed == KeyState::Pressed;
                    return FilterResult::Intercept(());
                }
                if mods.shift {
                    cycle_rice = pressed == KeyState::Pressed;
                    return FilterResult::Intercept(());
                }
                FilterResult::Forward
            });
            if toggle {
                let note = state.toggle_recording();
                println!("rill-compositor: {note}");
            }
            if cycle_rice {
                cycle_to_next_rice();
            }
            if toggle_fullscreen {
                // Borderless on the monitor the window is already on, so it
                // never jumps screens on the way in.
                let next = match window.fullscreen() {
                    Some(_) => None,
                    None => Some(Fullscreen::Borderless(None)),
                };
                window.set_fullscreen(next);
                state.needs_redraw = true;
            }
        }
        WindowEvent::CursorMoved { position, .. } => {
            let mut pos: Point<f64, Logical> = (position.x, position.y).into();
            // A distorting effect shader shows scene point f(p) at screen p —
            // run the pointer through the same forward map (the shader's
            // barrel, mirrored from crt.wgsl) so input meets what's visible.
            if let Some(barrel) = state.pointer_warp {
                let (w, h) = (state.output_size.w as f64, state.output_size.h as f64);
                if w > 0.0 && h > 0.0 {
                    let nx = pos.x / w * 2.0 - 1.0;
                    let ny = pos.y / h * 2.0 - 1.0;
                    let scale = 1.0 + barrel * (nx * nx + ny * ny);
                    pos = ((nx * scale * 0.5 + 0.5) * w, (ny * scale * 0.5 + 0.5) * h).into();
                }
            }
            *pointer_loc = pos;
            if state.draw_cursor {
                state.needs_redraw = true;
            }
            // While a resize grab runs the hover is pinned — the pointer
            // may briefly outrun the band mid-drag, and the cursor must
            // not flicker back to an arrow.
            if state.resize_state.is_none() {
                state.edge_hover = state.foreign_edge_at(pos);
            }
            let under = state.surface_under(pos);
            pointer.motion(
                state,
                under,
                &MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time },
            );
            pointer.frame(state);
        }
        WindowEvent::MouseInput { state: btn_state, button, .. } => {
            let serial = SERIAL_COUNTER.next_serial();
            let button_state = match btn_state {
                ElementState::Pressed => ButtonState::Pressed,
                ElementState::Released => ButtonState::Released,
            };
            // On press: raise the window under the cursor and give it keyboard
            // focus, so clicks and typing go to the same window. Raise and
            // focus-border both change the picture.
            state.needs_redraw = true;
            // First-click-away for popup grabs: a press outside every popup
            // dismisses the whole chain (popup_done cascades down it) and
            // keyboard focus returns to the toplevel. The press then goes
            // through to whatever is under it — the menu is gone by the
            // time the click lands, which is how it reads on screen too.
            if button_state == ButtonState::Pressed
                && state.popup_grab.is_some()
                && state.popup_under(*pointer_loc).is_none()
                && let Some((_, root)) = state.popup_grab.take()
            {
                let popups: Vec<_> = PopupManager::popups_for_surface(&root).collect();
                for (popup, _) in popups {
                    let _ = PopupManager::dismiss_popup(&root, &popup);
                }
            }
            if button_state == ButtonState::Pressed
                && let Some(window) = state.window_under(*pointer_loc)
            {
                state.space.raise_element(&window, true);
                if let Some(toplevel) = window.toplevel() {
                    let surface = toplevel.wl_surface().clone();
                    // Give the same client keyboard *and* data-device focus, so
                    // the focused window can own the clipboard selection
                    // (copy/paste). Without the latter, clipboard is inert.
                    let client = surface.client();
                    keyboard.set_focus(state, Some(surface), serial);
                    set_data_device_focus(&state.display_handle, &state.seat, client);
                }
            }
            // A left press in a foreign window's resize band starts the
            // same grab a client's xdg resize_request would. It has to
            // start here: the client never receives pointer events in the
            // band, so it can never ask. The grab is set before the button
            // event, which then lands in the grab (focus cleared), not in
            // any client.
            if button_state == ButtonState::Pressed
                && button == MouseButton::Left
                && state.resize_state.is_none()
                && let Some((window, edges)) = state.edge_hover.clone()
            {
                state.space.raise_element(&window, true);
                let loc = state.space.element_location(&window).unwrap_or_default();
                let size = state
                    .window_rect(&window)
                    .map(|r| r.size)
                    .unwrap_or_else(|| window.geometry().size);
                let initial_rect = Rectangle::new(loc, size);
                let (top, _bottom, left, _right) = edge_bools(edges);
                state.resize_state = Some(ResizeState {
                    window: window.clone(),
                    top,
                    left,
                    anchor_right: loc.x + size.w,
                    anchor_bottom: loc.y + size.h,
                    initial_loc: loc,
                });
                let grab = ResizeGrab {
                    start_data: GrabStartData { focus: None, button: 0x110, location: *pointer_loc },
                    window,
                    edges,
                    initial_rect,
                };
                pointer.set_grab(state, grab, serial, Focus::Clear);
            }
            // The BTN_* codes smithay's winit backend mapped to.
            let button = match button {
                MouseButton::Left => 0x110,
                MouseButton::Right => 0x111,
                MouseButton::Middle => 0x112,
                MouseButton::Forward => 0x115,
                MouseButton::Back => 0x116,
                MouseButton::Other(b) => b as u32,
            };
            pointer.button(state, &ButtonEvent { serial, time, button, state: button_state });
            pointer.frame(state);
        }
        WindowEvent::MouseWheel { delta, .. } => {
            let mut frame = AxisFrame::new(time);
            match delta {
                // Continuous (touchpad) — same sign convention as smithay's
                // winit backend.
                MouseScrollDelta::PixelDelta(d) => {
                    frame = frame.source(AxisSource::Continuous);
                    if d.x != 0.0 {
                        frame = frame.value(Axis::Horizontal, -d.x);
                    }
                    if d.y != 0.0 {
                        frame = frame.value(Axis::Vertical, -d.y);
                    }
                }
                // Discrete wheel: value in scroll-step pixels + v120 detents.
                MouseScrollDelta::LineDelta(x, y) => {
                    frame = frame.source(AxisSource::Wheel);
                    if x != 0.0 {
                        frame = frame
                            .value(Axis::Horizontal, f64::from(-x) * 15.0)
                            .v120(Axis::Horizontal, (-x * 120.0) as i32);
                    }
                    if y != 0.0 {
                        frame = frame
                            .value(Axis::Vertical, f64::from(-y) * 15.0)
                            .v120(Axis::Vertical, (-y * 120.0) as i32);
                    }
                }
            }
            pointer.axis(state, frame);
            pointer.frame(state);
        }
        _ => {}
    }
    false
}

/// Read a toplevel's app_id (used to classify shell surfaces into roles).
/// Clip a window title to the codec's short-string cap, on a char boundary.
/// A pathological title costs its own tail, not the recording.
fn truncate_title(title: &str) -> String {
    const CAP: usize = rill_ui::stream::MAX_SHORT_STRING;
    if title.len() <= CAP {
        return title.to_string();
    }
    let mut end = CAP;
    while end > 0 && !title.is_char_boundary(end) {
        end -= 1;
    }
    title[..end].to_string()
}

/// Where a new recording goes: `$XDG_DATA_HOME/rill/recordings/<secs>.rillrec`.
/// Seconds since the epoch keeps names sortable and collision-free without
/// pulling in a date library.
/// The wall clock as seconds since local midnight, for scene shaders with
/// a time of day. localtime_r so the scene agrees with the user's timezone.
fn seconds_since_local_midnight() -> f32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    (tm.tm_hour * 3600 + tm.tm_min * 60 + tm.tm_sec) as f32
}

/// A watched file as (path, mtime) — `None` until it exists. The shader,
/// wallpaper, and model slots all reload on a change to this pair.
type Stamped = Option<(std::path::PathBuf, std::time::SystemTime)>;

fn recording_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    base.join("rill/recordings").join(format!("{stamp}.rillrec"))
}

/// Where the system-of-record lives: `$XDG_DATA_HOME/rill/history/*.rhs`.
/// Sibling of the demo recordings, deliberately not the same directory —
/// one is a video you chose to make, the other is the machine's memory.
/// Where the owner's tier policy lives — beside the theme, because both are
/// the owner talking to their own machine.
fn history_policy_path() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("rill/history.toml")
}

fn history_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("rill/history")
}

fn toplevel_app_id(toplevel: &ToplevelSurface) -> Option<String> {
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().unwrap().app_id.clone())
    })
}

/// Fire frame callbacks so clients keep drawing.
fn send_frames_surface_tree(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surf, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}

#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}

    /// Say so when a window goes away.
    ///
    /// This was silent, and silence is the worst possible answer to "did that
    /// window just crash?" — the surface disappears, the desktop carries on,
    /// and nothing anywhere says whether the client exited, panicked, or was
    /// killed by the compositor for a protocol violation. A `post_error` here
    /// is *this* process deciding to kill a window, which is exactly the thing
    /// worth printing.
    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        match reason {
            DisconnectReason::ConnectionClosed => {
                println!("rill-compositor: client {client_id:?} disconnected")
            }
            DisconnectReason::ProtocolError(e) => println!(
                "rill-compositor: killed client {client_id:?} — protocol error on \
                 {} (code {}): {}",
                e.object_interface, e.code, e.message
            ),
        }
    }
}

// ------------------------------------------------------------------- grabs

/// A pointer grab that drags a window: while the grab button is held, the
/// window follows the pointer. Focus is cleared during the drag so the
/// content underneath doesn't receive motion.
struct MoveGrab {
    start_data: GrabStartData<Rill>,
    window: Window,
    initial_window_location: Point<i32, Logical>,
}

impl PointerGrab<Rill> for MoveGrab {
    fn motion(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let delta = event.location - self.start_data.location;
        let new_location = self.initial_window_location.to_f64() + delta;
        // Confine the window to the usable desktop area (can't drag off-screen
        // or under the dock). window_rect covers bufferless vector windows.
        let size = data
            .window_rect(&self.window)
            .map(|r| r.size)
            .unwrap_or_else(|| self.window.geometry().size);
        let clamped = data.clamp_to_usable(new_location.to_i32_round(), size);
        data.needs_redraw = true;
        data.space.map_element(self.window.clone(), clamped, true);
    }

    fn relative_motion(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        // Drop the grab once the button that started it is released.
        if !handle.current_pressed().contains(&self.start_data.button) {
            // A dropped widget records where it landed. Done here rather than
            // per motion event: the theme file is not a thing to rewrite
            // sixty times a second while someone is still dragging.
            if data.is_widget(&self.window) {
                data.widget_dropped(&self.window);
            }
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(&mut self, data: &mut Rill, handle: &mut PointerInnerHandle<'_, Rill>, details: AxisFrame) {
        handle.axis(data, details);
    }
    fn frame(&mut self, data: &mut Rill, handle: &mut PointerInnerHandle<'_, Rill>) {
        handle.frame(data);
    }
    fn gesture_swipe_begin(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }
    fn gesture_swipe_update(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }
    fn gesture_swipe_end(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }
    fn gesture_pinch_begin(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }
    fn gesture_pinch_update(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }
    fn gesture_pinch_end(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }
    fn gesture_hold_begin(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }
    fn gesture_hold_end(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<Rill> {
        &self.start_data
    }
    fn unset(&mut self, _data: &mut Rill) {}
}

const MIN_W: f64 = 240.0;
const MIN_H: f64 = 160.0;

/// Which edges a resize affects, as (top, bottom, left, right).
fn edge_bools(edges: xdg_toplevel::ResizeEdge) -> (bool, bool, bool, bool) {
    use xdg_toplevel::ResizeEdge::*;
    match edges {
        Top => (true, false, false, false),
        Bottom => (false, true, false, false),
        Left => (false, false, true, false),
        Right => (false, false, false, true),
        TopLeft => (true, false, true, false),
        TopRight => (true, false, false, true),
        BottomLeft => (false, true, true, false),
        BottomRight => (false, true, false, true),
        _ => (false, false, false, false),
    }
}

/// A pointer grab that resizes a window: the pointer drags an edge/corner, the
/// window's size (and, for top/left edges, its origin) follows. Sends the new
/// size to the client via xdg configures.
struct ResizeGrab {
    start_data: GrabStartData<Rill>,
    window: Window,
    edges: xdg_toplevel::ResizeEdge,
    initial_rect: Rectangle<i32, Logical>,
}

impl PointerGrab<Rill> for ResizeGrab {
    fn motion(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let Some(toplevel) = self.window.toplevel().cloned() else { return };
        let (top, bottom, left, right) = edge_bools(self.edges);
        let delta = event.location - self.start_data.location;
        let mut w = self.initial_rect.size.w as f64;
        let mut h = self.initial_rect.size.h as f64;
        if left {
            w -= delta.x;
        } else if right {
            w += delta.x;
        }
        if top {
            h -= delta.y;
        } else if bottom {
            h += delta.y;
        }
        // Respect the client's min-size hint on top of the compositor floor,
        // so the drag can't request sizes the client would refuse anyway.
        let hint = with_states(toplevel.wl_surface(), |states| {
            states.cached_state.get::<SurfaceCachedState>().current().min_size
        });
        // And cap growth at the usable area: a fixed edge anchors the window,
        // so the moving edge can reach the desktop bounds but not the dock
        // strip or off-screen.
        let (uw, uh) = data.usable_area();
        let max_w = if left {
            self.initial_rect.loc.x + self.initial_rect.size.w
        } else {
            uw - self.initial_rect.loc.x
        };
        let max_h = if top {
            self.initial_rect.loc.y + self.initial_rect.size.h
        } else {
            uh - self.initial_rect.loc.y
        };
        let w = w.max(MIN_W).max(hint.w as f64).min((max_w.max(MIN_W as i32)) as f64);
        let h = h.max(MIN_H).max(hint.h as f64).min((max_h.max(MIN_H as i32)) as f64);
        // Only request the new size here; the origin is repositioned on commit
        // (in `Rill::commit`) using the size the client actually applies, so the
        // window can't drift ahead of its rendered content.
        data.needs_redraw = true;
        toplevel.with_pending_state(|state| {
            state.size = Some((w as i32, h as i32).into());
            state.states.set(xdg_toplevel::State::Resizing);
        });
        toplevel.send_configure();
    }

    fn relative_motion(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if !handle.current_pressed().contains(&self.start_data.button) {
            handle.unset_grab(self, data, event.serial, event.time, true);
            data.resize_state = None;
            if let Some(toplevel) = self.window.toplevel().cloned() {
                toplevel.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Resizing);
                });
                toplevel.send_configure();
            }
        }
    }

    fn axis(&mut self, data: &mut Rill, handle: &mut PointerInnerHandle<'_, Rill>, details: AxisFrame) {
        handle.axis(data, details);
    }
    fn frame(&mut self, data: &mut Rill, handle: &mut PointerInnerHandle<'_, Rill>) {
        handle.frame(data);
    }
    fn gesture_swipe_begin(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }
    fn gesture_swipe_update(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }
    fn gesture_swipe_end(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }
    fn gesture_pinch_begin(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }
    fn gesture_pinch_update(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }
    fn gesture_pinch_end(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }
    fn gesture_hold_begin(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }
    fn gesture_hold_end(
        &mut self,
        data: &mut Rill,
        handle: &mut PointerInnerHandle<'_, Rill>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<Rill> {
        &self.start_data
    }
    fn unset(&mut self, _data: &mut Rill) {}
}

// ------------------------------------------------------------------ handlers

impl CompositorHandler for Rill {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }
    fn commit(&mut self, surface: &WlSurface) {
        // Damage only on commits that carry content: a buffer attach/remove
        // or client damage (read *before* on_commit_buffer_handler consumes
        // the assignment), or a latched stream frame below. gpui clients
        // ping-pong empty frame-callback commits at ~60/s per surface —
        // treating those as damage kept the whole desktop compositing
        // full-time. (A synced-subsurface edge case could slip through this
        // filter; the 1 Hz heartbeat self-heals any miss.)
        let content = with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            let attrs = guard.current();
            attrs.buffer.is_some() || !attrs.damage.is_empty()
        });
        if content {
            self.needs_redraw = true;
            self.commit_count = self.commit_count.saturating_add(1);
            self.total_commits = self.total_commits.saturating_add(1);
        }
        on_commit_buffer_handler::<Self>(surface);
        // xdg popups: move a newly-committed popup from unmapped to its
        // parent's tree, and send the initial configure — the protocol
        // forbids the client attaching a buffer before it arrives, so a
        // popup that never gets one is an invisible menu holding a grab.
        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface) {
            let initial_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgPopupSurfaceData>()
                    .map(|d| d.lock().unwrap().initial_configure_sent)
            });
            if initial_sent == Some(false) {
                let _ = popup.send_configure();
            }
        }
        // Latch a staged command-stream frame (rill_stream_v1 attach+commit).
        // A frame only exists between attach and latch, so a recording has to
        // capture it here — and captures the client's own bytes, not a
        // re-encoding of the decoded commands.
        // The recording id is resolved before the stream borrow: record_id
        // takes &mut self, and the frame must be attributed to the same
        // compositor-wide id the snapshots use (protocol_id collides across
        // clients — see the record_ids field).
        let recording = self.recorder.is_some() || self.history.is_some();
        let record_id = recording.then(|| self.record_id(&surface.id()));
        // The app behind this surface, for the owner's tier ratchet. Looked
        // up once and cached on the stream: the walk is per-commit and the
        // answer never changes for a live surface.
        let app_for_policy = if self.streams.get(&surface.id()).is_some_and(|st| st.app.is_none())
        {
            self.space
                .elements()
                .find(|w| w.toplevel().map(|t| t.wl_surface() == surface).unwrap_or(false))
                .and_then(|w| w.toplevel().and_then(toplevel_app_id))
        } else {
            None
        };
        if let Some(stream) = self.streams.get_mut(&surface.id())
            && let Some(mut frame) = stream.pending.take()
        {
            if stream.app.is_none()
                && let Some(app) = app_for_policy
            {
                // The id up to the first '#': widgets carry their placement
                // and source after it (`rill-shell-widget#<place>#<src>`),
                // and a policy pin names the app, not where it was parked.
                stream.app = Some(app.split('#').next().unwrap_or(&app).to_string());
            }
            // The ratchet (specs/history.md decisions 1 and 4): the owner's
            // pin and floor raise what the document declared, and nothing
            // lowers it.
            frame.tier = frame.tier.max(self.tier_policy.min_for(stream.app.as_deref()));
            // Taken, not cloned: the recorders are the only consumers, and
            // once they have them the bytes are dead weight on a frame that
            // stays resident until the window's next commit.
            let raw = std::mem::take(&mut frame.raw);
            let raw = recording.then_some(raw);
            // The transcript, extracted while the decoded commands are in
            // hand — the whole reason the index never decodes a frame. Text
            // is deduplicated on the writer thread; this only collects.
            let text = self.history.is_some().then(|| {
                let mut out = String::new();
                for c in &frame.commands {
                    if let DrawCommand::Text { text, .. } = c {
                        if !out.is_empty() {
                            out.push(' ');
                        }
                        out.push_str(text);
                    }
                }
                (!out.is_empty()).then_some(out)
            });
            // The tier this frame latched with becomes the surface's
            // recording tier from here on (set_tier semantics).
            let frame_tier = frame.tier;
            stream.tier = frame.tier;

            // The working set, refreshed from the frame that is about to be
            // on screen: these are the images eviction must not touch, and
            // everything holding one is stamped so the rest can be ordered by
            // how recently it was shown.
            stream.image_clock += 1;
            let clock = stream.image_clock;
            stream.live_images.clear();
            for c in &frame.commands {
                if let DrawCommand::Image { source, .. } = c {
                    stream.live_images.insert(source.clone());
                    if let Some(held) = stream.images.get_mut(source) {
                        held.used = clock;
                    }
                }
            }
            stream.current = Some(frame);
            self.needs_redraw = true;
            // A latched stream frame *is* a content-carrying commit, and it
            // was counted nowhere: the check above only sees a wl_buffer or
            // client damage, and a vector window delivers its content through
            // the stream instead. So the HUD's "client commits/s" has been
            // reading zero on a desktop made entirely of vector windows —
            // which is every Rill desktop.
            self.commit_count = self.commit_count.saturating_add(1);
            self.total_commits = self.total_commits.saturating_add(1);
            if let (Some(raw), Some(id)) = (raw, record_id) {
                // Both consumers want the bytes; the clone happens only when
                // both are actually running.
                if let Some(hist) = self.history.as_mut() {
                    let for_rec = self.recorder.is_some().then(|| raw.clone());
                    hist.frame(id, raw, text.flatten(), frame_tier);
                    if let (Some(rec), Some(bytes)) = (self.recorder.as_mut(), for_rec) {
                        rec.frame(id, bytes);
                    }
                } else if let Some(rec) = self.recorder.as_mut() {
                    rec.frame(id, raw);
                }
            }
        }
        if let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == surface).unwrap_or(false))
            .cloned()
        {
            window.on_commit();
        }
        // If this surface belongs to a resizing window, reposition it now that
        // a new size has actually been applied — anchoring the opposite edge.
        let repos = self.resize_state.as_ref().and_then(|rs| {
            if rs.window.toplevel().map(|t| t.wl_surface() == surface).unwrap_or(false) {
                let size = self
                    .window_rect(&rs.window)
                    .map(|r| r.size)
                    .unwrap_or_else(|| rs.window.geometry().size);
                let mut loc = rs.initial_loc;
                if rs.left {
                    loc.x = rs.anchor_right - size.w;
                }
                if rs.top {
                    loc.y = rs.anchor_bottom - size.h;
                }
                Some((rs.window.clone(), loc))
            } else {
                None
            }
        });
        if let Some((window, loc)) = repos {
            self.space.map_element(window, loc, false);
        }
        self.space.refresh();
    }
}

impl XdgShellHandler for Rill {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.spawn_times.insert(surface.wl_surface().id(), std::time::Instant::now());
        // Cascade new *app* windows (the shell background/dock are placed by
        // role once their app_id arrives, and don't count toward the cascade).
        let n = self
            .space
            .elements()
            .filter(|w| Some(*w) != self.background.as_ref() && Some(*w) != self.dock.as_ref())
            .count() as i32;
        let window = Window::new_wayland_window(surface.clone());
        let loc = self.clamp_to_usable(
            (60 + n * 48, self.dock_height + 24 + n * 40).into(),
            window.geometry().size,
        );
        self.needs_redraw = true;
        self.space.map_element(window, loc, true);
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
    }
    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, serial: Serial) {
        // A client (its titlebar drag) asked to move its window. Start a
        // pointer grab that repositions the window as the pointer moves.
        let Some(pointer) = self.seat.get_pointer() else { return };
        if !pointer.has_grab(serial) {
            return;
        }
        let Some(start_data) = pointer.grab_start_data() else { return };
        let wl_surface = surface.wl_surface().clone();
        // The grab must have started on this surface's client.
        let same_client = start_data
            .focus
            .as_ref()
            .is_some_and(|(focus, _)| focus.id().same_client_as(&wl_surface.id()));
        if !same_client {
            return;
        }
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == &wl_surface).unwrap_or(false))
            .cloned()
        else {
            return;
        };
        let initial_window_location = self.space.element_location(&window).unwrap_or_default();
        let grab = MoveGrab { start_data, window, initial_window_location };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }
    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        if !pointer.has_grab(serial) {
            return;
        }
        let Some(start_data) = pointer.grab_start_data() else { return };
        let wl_surface = surface.wl_surface().clone();
        let same_client = start_data
            .focus
            .as_ref()
            .is_some_and(|(focus, _)| focus.id().same_client_as(&wl_surface.id()));
        if !same_client {
            return;
        }
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == &wl_surface).unwrap_or(false))
            .cloned()
        else {
            return;
        };
        let loc = self.space.element_location(&window).unwrap_or_default();
        // window_rect covers bufferless vector windows (empty xdg geometry).
        let size = self
            .window_rect(&window)
            .map(|r| r.size)
            .unwrap_or_else(|| window.geometry().size);
        let initial_rect = Rectangle::new(loc, size);
        let (top, _bottom, left, _right) = edge_bools(edges);
        self.resize_state = Some(ResizeState {
            window: window.clone(),
            top,
            left,
            anchor_right: loc.x + size.w,
            anchor_bottom: loc.y + size.h,
            initial_loc: loc,
        });
        let grab = ResizeGrab { start_data, window, edges, initial_rect };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }
    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.needs_redraw = true;
        // Classify shell surfaces into desktop roles: the wallpaper sits below
        // apps (Background z-index) and fills the output; the dock sits above
        // apps (Top z-index) pinned to the bottom edge. Everything else is an
        // ordinary app window at the default (Shell) z-index.
        let app_id = toplevel_app_id(&surface);
        let Some(window) = self.window_for_surface(surface.wl_surface()) else { return };
        match app_id.as_deref() {
            Some(SHELL_BACKGROUND_APP_ID) => {
                self.background = Some(window.clone());
                window.override_z_index(RenderZindex::Background as u8);
                let size = self.output_size;
                surface.with_pending_state(|s| s.size = Some(size));
                surface.send_configure();
                self.space.map_element(window, (0, 0), false);
            }
            Some(SHELL_DOCK_APP_ID) => {
                self.dock = Some(window.clone());
                window.override_z_index(RenderZindex::Top as u8);
                let w = self.output_size.w;
                let h = self.dock_height;
                surface.with_pending_state(|s| s.size = Some((w, h).into()));
                surface.send_configure();
                // Top edge — the same placement reflow_shell uses, which is
                // the point: the dock's position lives in one decision, not
                // one per code path.
                self.space.map_element(window, (0, 0), false);
            }
            Some(id) if id.starts_with(SHELL_WIDGET_APP_ID) => {
                // `rill-shell-widget#<place>[#<app url>]`. A widget with no
                // placement still gets parked — top-left, at whatever size it
                // asked for — because its app id already said what it is.
                let mut parts = id.splitn(3, '#').skip(1);
                let place = parts
                    .next()
                    .and_then(WidgetPlace::parse)
                    .unwrap_or(WidgetPlace {
                        anchor: Anchor::TopLeft,
                        w: 320,
                        h: 160,
                        x: 16,
                        y: 16,
                    });
                let app = parts.next().unwrap_or_default().to_string();
                // No z override: a widget stacks like the window it is. It
                // still *starts* underneath everything, because widgets map
                // at login before any app window exists — which is the
                // behaviour the old pin to the bottom was really after, and
                // this way clicking one can still bring it forward.
                surface.with_pending_state(|s| s.size = Some((place.w, place.h).into()));
                surface.send_configure();
                let origin = place.origin(self.output_size, self.dock_height);
                self.space.map_element(window.clone(), origin, false);
                self.widgets.retain(|w| w.window != window);
                self.widgets.push(DesktopWidget { window, place, app });
            }
            _ => {}
        }
    }
    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        // The positioner's geometry is the popup's place relative to its
        // parent; the initial configure that carries it is sent from the
        // commit handler, once the client has committed its role state.
        surface.with_pending_state(|state| state.geometry = positioner.get_geometry());
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
        self.needs_redraw = true;
    }
    fn grab(&mut self, surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // An explicit grab arms MouseInput's first-click-away against the
        // chain's root. Keyboard focus deliberately STAYS on the toplevel:
        // moving it to the popup sends the toplevel wl_keyboard.leave, and
        // Firefox reads that as "window lost focus" and rolls the menu up
        // the same instant it opened — and keys delivered to a menu
        // surface get dropped by its widget code (the URL-bar autocomplete
        // grab ate all typing this way). The client routes menu keys
        // internally from its toplevel focus; it does not need ours.
        let kind = PopupKind::Xdg(surface.clone());
        let Ok(root) = find_popup_root_surface(&kind) else { return };
        self.popup_grab = Some((surface.wl_surface().clone(), root));
    }
    fn popup_destroyed(&mut self, _surface: PopupSurface) {
        self.needs_redraw = true;
    }
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
        self.needs_redraw = true;
    }
}

impl ShmHandler for Rill {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl BufferHandler for Rill {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl SeatHandler for Rill {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        // Remember what the focused client wants; applied to the host window
        // in the render loop (a nested compositor draws no cursor itself).
        self.cursor_status = image;
    }
}

// ---- rill_stream_v1: vector-native window content -------------------------

impl GlobalDispatch<RillStreamManagerV1, ()> for Rill {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<RillStreamManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<RillStreamManagerV1, ()> for Rill {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &RillStreamManagerV1,
        request: rill_stream_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            rill_stream_manager_v1::Request::GetStream { id, surface } => {
                let surface_id = surface.id();
                if state.streams.contains_key(&surface_id) {
                    manager.post_error(
                        rill_stream_manager_v1::Error::StreamExists,
                        "surface already has a stream",
                    );
                    return;
                }
                state.streams.insert(surface_id.clone(), StreamWindow::default());
                data_init.init(id, StreamUserData { surface_id });
            }
            rill_stream_manager_v1::Request::Destroy => {}
        }
    }
}

impl Dispatch<RillStreamV1, StreamUserData> for Rill {
    fn request(
        state: &mut Self,
        _client: &Client,
        stream: &RillStreamV1,
        request: rill_stream_v1::Request,
        data: &StreamUserData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            rill_stream_v1::Request::Attach { fd, size, width, height } => {
                // Read the frame now (the fd is the client's memfd); it
                // becomes surface content on the next commit. Same strictness
                // as every Rill parser: cap first, then strict decode.
                let size = size as usize;
                if size > rill_ui::stream::MAX_STREAM_SIZE {
                    stream.post_error(
                        rill_stream_v1::Error::Oversized,
                        format!("{size} byte frame exceeds the stream cap"),
                    );
                    return;
                }
                // The declared layout size is also the window's effective
                // geometry — reject nonsense before it reaches WM math.
                if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
                    stream.post_error(
                        rill_stream_v1::Error::InvalidStream,
                        format!("bad declared frame size {width}x{height}"),
                    );
                    return;
                }
                use std::os::unix::fs::FileExt;
                let file = std::fs::File::from(fd);
                let mut bytes = vec![0u8; size];
                if let Err(e) = file.read_exact_at(&mut bytes, 0) {
                    stream.post_error(
                        rill_stream_v1::Error::InvalidStream,
                        format!("frame read failed: {e}"),
                    );
                    return;
                }
                match rill_ui::stream::decode(&bytes) {
                    Ok(commands) => {
                        if let Some(window) = state.streams.get_mut(&data.surface_id) {
                            let tier = window.declared_tier;
                            window.pending = Some(StreamFrame {
                                commands,
                                tier,
                                width,
                                height,
                                wire_len: bytes.len(),
                                raw: bytes,
                            });
                        }
                    }
                    Err(e) => {
                        stream.post_error(rill_stream_v1::Error::InvalidStream, e.to_string())
                    }
                }
            }
            rill_stream_v1::Request::AttachImage { fd, size, width, height, source } => {
                // Raw RGBA8, tightly packed, and checked by arithmetic rather
                // than parsed: the compositor must never run an image decoder
                // over bytes a client handed it. The client already decoded
                // this — it had to, the layout box came from the image's
                // natural size — so what arrives here is pixels, not a format.
                let size = size as usize;
                let expected = (width as usize)
                    .checked_mul(height as usize)
                    .and_then(|n| n.checked_mul(4));
                if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
                    stream.post_error(
                        rill_stream_v1::Error::ImageMalformed,
                        format!("bad image size {width}x{height}"),
                    );
                    return;
                }
                if expected != Some(size) {
                    stream.post_error(
                        rill_stream_v1::Error::ImageMalformed,
                        format!("{size} bytes is not {width}x{height} RGBA"),
                    );
                    return;
                }
                let Some(window) = state.streams.get_mut(&data.surface_id) else { return };
                // Replacing a source releases what it held, so re-attaching a
                // changed image is not a leak.
                let freed = window.images.get(&source).map_or(0, |h| h.bytes);
                let mut held = window.image_bytes - freed;

                let inventory: Vec<(&str, usize, u64)> = window
                    .images
                    .iter()
                    .map(|(s, h)| (s.as_str(), h.bytes, h.used))
                    .collect();
                let plan = plan_release(
                    &inventory,
                    &window.live_images,
                    held,
                    &source,
                    size,
                    MAX_SURFACE_IMAGE_BYTES,
                );
                drop(inventory);
                let Some(release) = plan else {
                    // Everything droppable is gone and it still does not fit,
                    // so one frame is asking for more pixels than a surface
                    // may hold. That is a refusal, not a shortage.
                    stream.post_error(
                        rill_stream_v1::Error::ImageBudget,
                        format!(
                            "one frame needs more than {MAX_SURFACE_IMAGE_BYTES} bytes of images \
                             ({held} already held by what is on screen)"
                        ),
                    );
                    return;
                };
                for victim in release {
                    if let Some(h) = window.images.remove(&victim) {
                        held -= h.bytes;
                        // Tell the client, or the picture is simply gone: it
                        // believes it has already sent this and would never
                        // send it again.
                        stream.image_released(victim);
                    }
                }

                use std::os::unix::fs::FileExt;
                let file = std::fs::File::from(fd);
                let mut pixels = vec![0u8; size];
                if let Err(e) = file.read_exact_at(&mut pixels, 0) {
                    stream.post_error(
                        rill_stream_v1::Error::ImageMalformed,
                        format!("image read failed: {e}"),
                    );
                    return;
                }
                window.image_bytes = held + size;
                window.pending_images.push(PendingImage {
                    source,
                    pixels,
                    w: width,
                    h: height,
                });
                state.needs_redraw = true;
            }
            rill_stream_v1::Request::SetTier { tier } => {
                // The closed set, enforced at the wire: an unknown tier must
                // fail closed — killing the window — never record as
                // routine. The same rule the document codec applies.
                if tier > 2 {
                    stream.post_error(
                        rill_stream_v1::Error::InvalidStream,
                        format!("unknown sensitivity tier {tier}"),
                    );
                    return;
                }
                if let Some(window) = state.streams.get_mut(&data.surface_id) {
                    window.declared_tier = tier as u8;
                }
            }
            rill_stream_v1::Request::Destroy => {
                state.streams.remove(&data.surface_id);
                // Safe to forget the recording id only once the surface is
                // genuinely gone: dropping it while the window merely has no
                // size (the zero-area snapshot filter) would hand it a fresh
                // id on return, and a replay would see a different window.
                state.record_ids.remove(&data.surface_id);
            }
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        _resource: &RillStreamV1,
        data: &StreamUserData,
    ) {
        state.streams.remove(&data.surface_id);
        state.record_ids.remove(&data.surface_id);
    }
}

impl DmabufHandler for Rill {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }
    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        // Accept what the wgpu importer can bind: single-plane ARGB/XRGB
        // (the advertised format list). The actual import happens lazily at
        // composite time, per wl_buffer.
        let supported = dmabuf.num_planes() == 1
            && matches!(dmabuf.format().code, Fourcc::Argb8888 | Fourcc::Xrgb8888);
        if supported {
            let _ = notifier.successful::<Rill>();
        } else {
            notifier.failed();
        }
    }
}

impl OutputHandler for Rill {}

impl smithay::wayland::tablet_manager::TabletSeatHandler for Rill {}

impl XdgDecorationHandler for Rill {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        set_client_decoration(&toplevel);
    }
    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        // We don't draw server-side decorations, so always ask the client to
        // draw its own titlebar/borders (like gpui already does).
        set_client_decoration(&toplevel);
    }
    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        set_client_decoration(&toplevel);
    }
}

/// `[desktop]` effect config from `~/.config/rill/theme.toml` — the
/// whole-output shader (D5) plus its declared input warp. `~/` expands; bare
/// relative shader paths resolve against the config dir.
///
/// `warp_barrel` exists because a distorting shader moves what the user
/// *sees*: the pixel shown at screen `p` was sampled from scene `f(p)`, so
/// pointer input must run through the same forward map or clicks land beside
/// what they aim at. Arbitrary WGSL can't be inverted/introspected, so the
/// warp is declared as data next to the shader (the CRT's barrel factor).
/// How the compositor draws the pointer. `draw = false` hands the cursor
/// back to the host window, which is the old behaviour.
#[derive(Clone, Copy)]
struct CursorStyle {
    draw: bool,
    fill: UiColor,
    outline: UiColor,
    size: f32,
    shadow: u8,
}

impl Default for CursorStyle {
    fn default() -> CursorStyle {
        CursorStyle {
            draw: true,
            fill: UiColor { r: 0x12, g: 0x12, b: 0x18, a: 0xFF },
            outline: UiColor { r: 0xF2, g: 0xF2, b: 0xF6, a: 0xFF },
            size: 22.0,
            shadow: 90,
        }
    }
}

/// The pointer as geometry: a classic arrow in a unit box, an I-beam for
/// text, a double-headed arrow for resize edges. Drawn rather than blitted,
/// so it scales to any size and takes the theme's colours.
fn cursor_shape(icon: CursorIcon, at: (f32, f32), style: CursorStyle) -> Vec<DrawCommand> {
    let s = style.size.clamp(8.0, 96.0);
    let (x, y) = at;
    // Unit-box outlines, y down, hotspot at (0,0) unless noted.
    let arrow: Vec<(f32, f32)> = vec![
        (0.00, 0.00), (0.00, 1.00), (0.26, 0.74), (0.42, 1.06),
        (0.60, 0.99), (0.44, 0.68), (0.72, 0.62),
    ];
    // I-beam: hotspot at its centre, so shift it half a size up and back.
    let beam: Vec<(f32, f32)> = vec![
        (-0.22, -0.50), (0.22, -0.50), (0.22, -0.40), (0.06, -0.40),
        (0.06, 0.40), (0.22, 0.40), (0.22, 0.50), (-0.22, 0.50),
        (-0.22, 0.40), (-0.06, 0.40), (-0.06, -0.40), (-0.22, -0.40),
    ];
    // A double arrow, drawn vertical and rotated per edge below.
    let double: Vec<(f32, f32)> = vec![
        (0.00, -0.55), (0.24, -0.22), (0.09, -0.22), (0.09, 0.22),
        (0.24, 0.22), (0.00, 0.55), (-0.24, 0.22), (-0.09, 0.22),
        (-0.09, -0.22), (-0.24, -0.22),
    ];
    let rot = |pts: &[(f32, f32)], deg: f32| -> Vec<(f32, f32)> {
        let (sn, cs) = deg.to_radians().sin_cos();
        pts.iter().map(|(px, py)| (px * cs - py * sn, px * sn + py * cs)).collect()
    };
    let pts = match icon {
        CursorIcon::Text | CursorIcon::VerticalText => beam,
        CursorIcon::NsResize | CursorIcon::NResize | CursorIcon::SResize => double,
        CursorIcon::EwResize | CursorIcon::EResize | CursorIcon::WResize => rot(&double, 90.0),
        CursorIcon::NwseResize | CursorIcon::NwResize | CursorIcon::SeResize => {
            rot(&double, 45.0)
        }
        CursorIcon::NeswResize | CursorIcon::NeResize | CursorIcon::SwResize => {
            rot(&double, -45.0)
        }
        // Everything else — arrow, pointer, grab — wears the arrow. Honest:
        // a hand would need its own outline, and an arrow that is always an
        // arrow beats a shape that guesses.
        _ => arrow,
    };
    let place = |scale: f32| -> Vec<UiPoint> {
        pts.iter().map(|(px, py)| UiPoint { x: x + px * s * scale, y: y + py * s * scale }).collect()
    };
    let ring = vec![pts.len() as u32];
    let mut out = Vec::with_capacity(3);
    if style.shadow > 0 {
        let shadow: Vec<UiPoint> = place(1.10)
            .into_iter()
            .map(|p| UiPoint { x: p.x + s * 0.06, y: p.y + s * 0.08 })
            .collect();
        out.push(DrawCommand::FillPath {
            points: shadow,
            contours: ring.clone(),
            color: UiColor { r: 0, g: 0, b: 0, a: style.shadow },
        });
    }
    // Outline first, fill over it — the Bibata trick: a light rim keeps the
    // pointer legible on any wallpaper.
    out.push(DrawCommand::FillPath {
        points: place(1.16),
        contours: ring.clone(),
        color: style.outline,
    });
    out.push(DrawCommand::FillPath { points: place(1.0), contours: ring, color: style.fill });
    out
}

/// The compositor-facing `[desktop]` effect config.
#[derive(Clone)]
struct DesktopFx {
    shader: Option<std::path::PathBuf>,
    /// `[desktop] window_shader`: the per-window effect, drawn once per
    /// window at that window's own z rather than over the finished frame.
    /// That is what lets glass in front of it blur it.
    window_shader: Option<std::path::PathBuf>,
    /// `[desktop] particle_shader` / `particle_render`: the update and draw
    /// passes over the particle state buffer. Either missing falls back to
    /// the built-in flock's half.
    particle_shader: Option<std::path::PathBuf>,
    particle_render: Option<std::path::PathBuf>,
    /// `[desktop] particle_diffuse`: the field pass over the trail, run once
    /// per pixel. A simulation whose agents leave something behind needs it;
    /// one that does not, does not.
    particle_diffuse: Option<std::path::PathBuf>,
    /// `[desktop.dock] height`: the strip's height, which is the
    /// compositor's business because it reserves the space.
    dock_height: i32,
    /// `[desktop] model` + `model_shader`: a 3D showcase object rendered
    /// between the wallpaper and the windows through its own depth pass.
    model: Option<std::path::PathBuf>,
    model_shader: Option<std::path::PathBuf>,
    /// `[desktop.showroom]`: the scene's lights, spin, camera and colours,
    /// handed to the background shader and the model pass alike.
    scene: rill_gpu::SceneParams,
    /// `[cursor]`: the pointer the compositor draws itself. A nested
    /// compositor normally borrows the host's bitmap cursor; a vector
    /// desktop can draw its own, which makes the pointer themeable like
    /// everything else — colour, outline, size — instead of a downloaded
    /// icon set.
    cursor: CursorStyle,
    /// `[window]`: how the compositor dresses a window — the focus ring
    /// around the active one and the drop shadow under every one. These
    /// were constants; they are the user's to set.
    focus_glow: UiColor,
    focus_glow_blur: f32,
    shadow: UiColor,
    shadow_blur: f32,
    warp: Option<f64>,
    hud: bool,
    background: Option<std::path::PathBuf>,
    /// Pixel wallpaper (`[desktop] wallpaper`), compositor-painted at the
    /// bottom of the scene since the gpui shell retired.
    wallpaper: Option<std::path::PathBuf>,
    /// `[desktop] background_color`: the desktop's floor — the clear colour
    /// the scene composites over. It shows bare, under a transparent
    /// wallpaper, and whenever image and shader are both off. `None` keeps
    /// the built-in near-black.
    background_color: Option<UiColor>,
    /// `[desktop.shader_params.<stem>]`: stored values for a shader's
    /// declared `// @param` knobs, keyed by the shader file's stem. Values
    /// only — the declarations (range, default, order) live in the shader.
    shader_params: std::collections::HashMap<String, Vec<(String, f64)>>,
    boids: u32,
    glass: bool,
    animations: bool,
}

impl Default for DesktopFx {
    fn default() -> DesktopFx {
        DesktopFx {
            cursor: CursorStyle::default(),
            shader: None,
            window_shader: None,
            particle_shader: None,
            particle_render: None,
            particle_diffuse: None,
            model: None,
            model_shader: None,
            scene: rill_gpu::SceneParams::default(),
            focus_glow: UiColor { r: 110, g: 168, b: 255, a: 230 },
            focus_glow_blur: 18.0,
            shadow: UiColor { r: 0, g: 0, b: 0, a: 140 },
            shadow_blur: 26.0,
            warp: None,
            hud: false,
            background: None,
            wallpaper: None,
            background_color: None,
            dock_height: DOCK_HEIGHT,
            boids: 0,
            glass: false,
            animations: true,
            shader_params: std::collections::HashMap::new(),
        }
    }
}

/// Read every `[[desktop.widgets]]` placement, as (app URL, place) rows in
/// file order — the read half of [`save_widget_place`], spelled with the
/// same defaults the dock uses when it spawns the widget, so the two ways a
/// placement reaches the compositor (app id at map, this re-read) agree.
fn theme_widget_places() -> Vec<(String, WidgetPlace)> {
    let Some(list) = std::fs::read_to_string(theme_path())
        .ok()
        .and_then(|s| s.parse::<toml::Table>().ok())
        .and_then(|root| root.get("desktop")?.get("widgets")?.as_array().cloned())
    else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|entry| {
            let t = entry.as_table()?;
            let app = t.get("app")?.as_str()?.to_string();
            let num =
                |k: &str, d: i64| t.get(k).and_then(|v| v.as_integer()).unwrap_or(d);
            let anchor = t.get("anchor").and_then(|v| v.as_str()).unwrap_or("top-right");
            let place = WidgetPlace::parse(&format!(
                "{anchor}:{}x{}+{}+{}",
                num("width", 320),
                num("height", 140),
                num("x", 16),
                num("y", 16),
            ))?;
            Some((app, place))
        })
        .collect()
}

/// Write a widget's new placement into its `[[desktop.widgets]]` entry.
///
/// Format-preserving on purpose. `theme.toml` is a hand-written rice file
/// full of comments and deliberate ordering, and a drag is a casual, frequent
/// gesture — reserializing the whole document through `toml::Value` would
/// quietly delete someone's comments every time they nudged a widget. So this
/// edits the three fields in place and leaves every byte it did not mean to
/// touch exactly where it was.
///
/// Missing file, missing table, or an entry that does not match: nothing to
/// do, and not an error. The widget still moved; it just has nowhere to
/// record that it did.
fn save_widget_place(app: &str, place: WidgetPlace) -> std::io::Result<()> {
    save_widget_place_in(&theme_path(), app, place)
}

/// Replace a key's value, keeping the trivia around it.
///
/// A plain `table["x"] = value` in toml_edit swaps the whole item, and the
/// item is what owns its *decor* — the whitespace and comments on either
/// side. So the obvious spelling silently eats `anchor = "top-right"  # why`.
/// Lifting the old decor onto the new value keeps the line looking exactly
/// as it was written, minus the number that changed.
fn set_keeping_decor(table: &mut toml_edit::Table, key: &str, new: toml_edit::Value) {
    match table.get_mut(key).and_then(|item| item.as_value_mut()) {
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = new;
            *existing.decor_mut() = decor;
        }
        // Key absent (an entry that never spelled out its anchor): add it.
        None => table[key] = toml_edit::Item::Value(new),
    }
}

/// [`save_widget_place`], against a named file — the seam the tests use, so
/// the round trip can be checked without a real desktop or a real `$HOME`.
fn save_widget_place_in(path: &std::path::Path, app: &str, place: WidgetPlace) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    let text = std::fs::read_to_string(path)?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?;
    let Some(widgets) = doc
        .get_mut("desktop")
        .and_then(|d| d.get_mut("widgets"))
        .and_then(|w| w.as_array_of_tables_mut())
    else {
        return Ok(());
    };
    let Some(entry) = widgets
        .iter_mut()
        .find(|t| t.get("app").and_then(|v| v.as_str()) == Some(app))
    else {
        return Ok(());
    };
    set_keeping_decor(entry, "anchor", place.anchor.name().into());
    set_keeping_decor(entry, "x", (place.x as i64).into());
    set_keeping_decor(entry, "y", (place.y as i64).into());
    // Write beside the target and rename over it: the compositor re-reads
    // this file on a 300ms poll, and it must never observe a half-written
    // theme — that would flash a default desktop for one frame.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(&tmp, path)
}

/// Load the next saved rice — the Ctrl+Shift+R cycle.
///
/// Applying one is a file copy over `theme.toml` and nothing else: the
/// compositor already polls that file, and so does every vector client, so
/// the whole desktop re-skins itself from the write. That is the entire
/// reason a rice is stored as a theme rather than as a format of its own.
fn cycle_to_next_rice() {
    let theme = theme_path();
    let Some(config) = theme.parent() else { return };
    match rill_appkit::rices::next(config, &theme) {
        Some(name) => match rill_appkit::rices::load(config, &theme, &name) {
            Ok(()) => println!("rill-compositor: rice {name}"),
            Err(e) => eprintln!("rill-compositor: rice {name} failed to load: {e}"),
        },
        None => println!(
            "rill-compositor: no saved rices in {}",
            rill_appkit::rices::dir(config).display()
        ),
    }
}

/// Where the desktop theme lives.
///
/// This must agree with `rill_viewport::theme::default_path()`, which is the
/// canonical implementation — the studio writes the file through that path
/// and every client reads it through that path. The compositor cannot call
/// it without taking a dependency on rill-viewport (and so on tokio and the
/// client stack) to read one `[desktop]` table, so the rule is mirrored here
/// instead. Change one, change the other; `theme_path_matches_viewport`
/// in tests/theme-path.rs is the tripwire.
fn theme_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".config"));
    base.join("rill").join("theme.toml")
}

/// `#rrggbb` or `#rrggbbaa` (leading `#` optional) as the compositor's
/// colour type. Anything else is no colour — a theme typo keeps the
/// default floor rather than painting garbage.
fn parse_hex_color(s: &str) -> Option<UiColor> {
    let hex = s.trim().trim_start_matches('#');
    if !matches!(hex.len(), 6 | 8) || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(UiColor {
        r: byte(0)?,
        g: byte(2)?,
        b: byte(4)?,
        a: if hex.len() == 8 { byte(6)? } else { 255 },
    })
}

/// `theme_desktop_fx`, but parsed only when the file actually moved.
///
/// The loop polls this every 300ms so that editing theme.toml re-skins the
/// desktop live — but "poll" was doing a read plus a full TOML parse three
/// times a second for the entire life of the session, to answer "no" almost
/// every time. An mtime stat answers the same question for a fraction of the
/// work. The stamps *inside* the returned config (shader, model, wallpaper
/// mtimes) are still checked every poll by the caller, so editing a .wgsl
/// without touching theme.toml still hot-reloads.
fn theme_desktop_fx_cached() -> DesktopFx {
    static MEMO: std::sync::Mutex<Option<(Option<std::time::SystemTime>, DesktopFx)>> =
        std::sync::Mutex::new(None);
    let path = theme_path();
    let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut memo = MEMO.lock().unwrap();
    if let Some((cached_stamp, fx)) = memo.as_ref()
        && *cached_stamp == stamp
    {
        return fx.clone();
    }
    let fx = theme_desktop_fx();
    *memo = Some((stamp, fx.clone()));
    fx
}

fn theme_desktop_fx() -> DesktopFx {
    // One rule decides where the theme lives, and it is the viewport's:
    // every client, the studio that writes the file, and this — the process
    // that reads it three times a second — have to agree, or a desktop can
    // be skinned from two files at once. This used to hardcode
    // `$HOME/.config/rill`, which ignored XDG_CONFIG_HOME and so disagreed
    // with every client the moment that variable was set.
    let path = theme_path();
    let config = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    let Some(root) =
        std::fs::read_to_string(&path).ok().and_then(|s| s.parse::<toml::Value>().ok())
    else {
        return DesktopFx::default();
    };
    let Some(desktop) = root.get("desktop") else { return DesktopFx::default() };
    let resolve = |s: &str| {
        let p = match s.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => std::path::PathBuf::from(s),
        };
        if p.is_absolute() { p } else { config.join(p) }
    };
    // The showroom sub-table. Directions are authored as azimuth/elevation
    // in degrees (a person can reason about "light from the left, high up");
    // colours as hex, converted to linear because they are radiance here,
    // not pixels.
    let mut scene = rill_gpu::SceneParams::default();
    if let Some(sr) = desktop.get("showroom").and_then(|v| v.as_table()) {
        let num = |key: &str| -> Option<f32> {
            sr.get(key)
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .map(|v| v as f32)
        };
        let flag = |key: &str| sr.get(key).and_then(|v| v.as_bool());
        let color = |key: &str| -> Option<[f32; 3]> {
            let hex = sr.get(key)?.as_str()?;
            let c = rill_ui::Color::parse_hex(hex)?;
            let lin = |b: u8| ((b as f32 / 255.0).powf(2.2)).clamp(0.0, 1.0);
            Some([lin(c.r), lin(c.g), lin(c.b)])
        };
        let dir_of = |az: f32, el: f32| {
            let (az, el) = (az.to_radians(), el.to_radians());
            [az.sin() * el.cos(), el.sin(), az.cos() * el.cos()]
        };
        // Defaults expressed as angles, so a partial table still makes sense.
        let key_dir = dir_of(
            num("key_azimuth").unwrap_or(-42.0),
            num("key_elevation").unwrap_or(55.0),
        );
        scene.key = [key_dir[0], key_dir[1], key_dir[2], num("key_intensity").unwrap_or(7.2)];
        if let Some(c) = color("key_color") {
            scene.key_color = [c[0], c[1], c[2], 0.0];
        }
        let fill_dir = dir_of(
            num("fill_azimuth").unwrap_or(60.0),
            num("fill_elevation").unwrap_or(18.0),
        );
        let fill_on = flag("fill").unwrap_or(true);
        scene.fill = [
            fill_dir[0],
            fill_dir[1],
            fill_dir[2],
            if fill_on { num("fill_intensity").unwrap_or(1.8) } else { 0.0 },
        ];
        if let Some(c) = color("fill_color") {
            scene.fill_color = [c[0], c[1], c[2], 0.0];
        }
        if let Some(c) = color("rim_color") {
            scene.rim_color = [c[0], c[1], c[2], num("rim_intensity").unwrap_or(2.6)];
        } else if let Some(i) = num("rim_intensity") {
            scene.rim_color[3] = i;
        }
        if let Some(c) = color("body_color") {
            // A colour present *is* the override; absent leaves the model's
            // own materials alone.
            scene.body_color = [c[0], c[1], c[2], 1.0];
        }
        if let Some(c) = color("ground_color") {
            scene.ground_color = [c[0], c[1], c[2], 0.0];
        }
        if let Some(c) = color("backdrop_color") {
            scene.backdrop_color = [c[0], c[1], c[2], scene.backdrop_color[3]];
        }
        scene.backdrop_color[3] = num("rings").unwrap_or(1.0).clamp(0.0, 3.0);
        scene.fit = [
            match sr.get("model_up").and_then(|v| v.as_str()) {
                Some("z") => 1.0,
                Some("-y") => 2.0,
                Some("-z") => 3.0,
                _ => 0.0,
            },
            num("model_scale").unwrap_or(1.0).clamp(0.1, 4.0),
            num("model_lift").unwrap_or(0.0).clamp(-2.0, 2.0),
            0.0,
        ];
        scene.finish = [
            num("reflection").unwrap_or(0.30).clamp(0.0, 1.0),
            num("reflection_fade").unwrap_or(0.42).clamp(0.05, 2.0),
            num("backdrop_glow").unwrap_or(0.45).clamp(0.0, 2.0),
            num("vignette").unwrap_or(0.55).clamp(0.0, 1.5),
        ];
        scene.motion = [
            num("spin").unwrap_or(0.08),
            num("spin_phase").unwrap_or(0.44),
            num("distance").unwrap_or(3.88).clamp(1.2, 14.0),
            num("exposure").unwrap_or(1.0).clamp(0.05, 4.0),
        ];
    }

    // [window] is a sibling table, not part of [desktop].
    let window = root.get("window").and_then(|v| v.as_table());
    let wnum = |key: &str, default: f32| -> f32 {
        window
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
            .map(|v| v as f32)
            .unwrap_or(default)
    };
    let wcolor = |key: &str, default: UiColor, alpha_key: &str, alpha_default: f32| -> UiColor {
        let base = window
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_str())
            .and_then(rill_ui::Color::parse_hex)
            .unwrap_or(default);
        UiColor { a: wnum(alpha_key, alpha_default).clamp(0.0, 255.0) as u8, ..base }
    };

    let cursor_table = root.get("cursor").and_then(|v| v.as_table());
    let cnum = |key: &str, default: f32| -> f32 {
        cursor_table
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
            .map(|v| v as f32)
            .unwrap_or(default)
    };
    let ccolor = |key: &str, default: UiColor| -> UiColor {
        cursor_table
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_str())
            .and_then(rill_ui::Color::parse_hex)
            .unwrap_or(default)
    };
    let cursor_default = CursorStyle::default();
    let cursor = CursorStyle {
        draw: cursor_table
            .and_then(|t| t.get("draw"))
            .and_then(|v| v.as_bool())
            .unwrap_or(cursor_default.draw),
        fill: ccolor("color", cursor_default.fill),
        outline: ccolor("outline", cursor_default.outline),
        size: cnum("size", cursor_default.size).clamp(8.0, 96.0),
        shadow: cnum("shadow", cursor_default.shadow as f32).clamp(0.0, 255.0) as u8,
    };

    DesktopFx {
        cursor,
        focus_glow: wcolor(
            "focus_glow",
            UiColor { r: 110, g: 168, b: 255, a: 230 },
            "focus_glow_alpha",
            230.0,
        ),
        focus_glow_blur: wnum("focus_glow_blur", 18.0).clamp(0.0, 90.0),
        shadow: wcolor("shadow_color", UiColor { r: 0, g: 0, b: 0, a: 140 }, "shadow_alpha", 140.0),
        shadow_blur: wnum("shadow_blur", 26.0).clamp(0.0, 120.0),
        scene,
        shader: desktop.get("shader").and_then(|v| v.as_str()).map(resolve),
        window_shader: desktop.get("window_shader").and_then(|v| v.as_str()).map(resolve),
        particle_shader: desktop.get("particle_shader").and_then(|v| v.as_str()).map(resolve),
        particle_render: desktop.get("particle_render").and_then(|v| v.as_str()).map(resolve),
        particle_diffuse: desktop.get("particle_diffuse").and_then(|v| v.as_str()).map(resolve),
        model: desktop.get("model").and_then(|v| v.as_str()).map(resolve),
        model_shader: desktop.get("model_shader").and_then(|v| v.as_str()).map(resolve),
        warp: desktop.get("warp_barrel").and_then(|v| v.as_float()),
        hud: desktop.get("hud").and_then(|v| v.as_bool()).unwrap_or(false),
        background: desktop.get("background_shader").and_then(|v| v.as_str()).map(resolve),
        wallpaper: desktop.get("wallpaper").and_then(|v| v.as_str()).map(resolve),
        background_color: desktop
            .get("background_color")
            .and_then(|v| v.as_str())
            .and_then(parse_hex_color),
        shader_params: desktop
            .get("shader_params")
            .and_then(|v| v.as_table())
            .map(|shaders| {
                shaders
                    .iter()
                    .filter_map(|(stem, vals)| {
                        let t = vals.as_table()?;
                        let pairs = t
                            .iter()
                            .filter_map(|(name, v)| {
                                let n = v
                                    .as_float()
                                    .or_else(|| v.as_integer().map(|i| i as f64))?;
                                Some((name.clone(), n))
                            })
                            .collect();
                        Some((stem.clone(), pairs))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        dock_height: desktop
            .get("dock")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("height"))
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
            .map(|n| (n as i32).clamp(20, 200))
            .unwrap_or(DOCK_HEIGHT),
        // `particles` is the name that fits what this now drives; `boids`
        // still works, because themes in the wild use it.
        boids: desktop
            .get("particles")
            .or_else(|| desktop.get("boids"))
            .and_then(|v| v.as_integer())
            .unwrap_or(0)
            .clamp(0, rill_gpu::MAX_PARTICLES as i64)
            as u32,
        glass: desktop.get("glass").and_then(|v| v.as_bool()).unwrap_or(false),
        animations: desktop.get("animations").and_then(|v| v.as_bool()).unwrap_or(true),
    }
}

/// One process row for the stats HUD, sampled from /proc.
struct HudProc {
    name: String,
    pid: i32,
    cpu_pct: f32,
    rss_mb: f32,
    windows: u32,
}

/// Read (utime+stime ticks, rss bytes) for a pid. Linux ticks at 100 Hz.
fn proc_sample(pid: i32) -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 2 (comm) may contain spaces — parse after the closing paren.
    let rest = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some((utime + stime, rss_pages * 4096))
}

fn set_client_decoration(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(DecorationMode::ClientSide);
    });
    toplevel.send_configure();
}

impl SelectionHandler for Rill {
    type SelectionUserData = ();
}
impl DataDeviceHandler for Rill {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for Rill {}
impl ServerDndGrabHandler for Rill {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Self>) {}
}

delegate_compositor!(Rill);
delegate_xdg_shell!(Rill);
delegate_shm!(Rill);
delegate_seat!(Rill);
delegate_data_device!(Rill);
delegate_dmabuf!(Rill);
delegate_output!(Rill);
delegate_cursor_shape!(Rill);
delegate_xdg_decoration!(Rill);

#[cfg(test)]
mod theme_path_tests {
    /// The compositor mirrors `rill_viewport::theme::default_path()` rather
    /// than depending on it. A mirrored rule is a rule that drifts, and the
    /// failure is quiet and nasty: the compositor skins the desktop from one
    /// file while every client reads another. Assert they agree — including
    /// under XDG_CONFIG_HOME, which is exactly where they used to disagree.
    #[test]
    fn theme_path_matches_viewport() {
        // Serialised against the other env-mutating test in this binary by
        // running them in one test; env is process-global.
        let restore = (
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        );

        unsafe { std::env::set_var("HOME", "/tmp/rill-test-home") };
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(
            super::theme_path(),
            rill_viewport::theme::default_path(),
            "HOME-only case"
        );

        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/rill-test-xdg") };
        assert_eq!(
            super::theme_path(),
            rill_viewport::theme::default_path(),
            "XDG_CONFIG_HOME must win for both — this is the case that regressed"
        );
        assert!(
            super::theme_path().starts_with("/tmp/rill-test-xdg"),
            "and it must actually be honoured, not merely agreed upon"
        );

        match restore.0 {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        if let Some(v) = restore.1 {
            unsafe { std::env::set_var("HOME", v) }
        }
    }
}

#[cfg(test)]
mod widget_tests {
    use super::*;

    /// The placement spec is written by hand in a theme file and read back
    /// off an app id, so both ends of that trip have to hold.
    #[test]
    fn a_placement_spec_round_trips() {
        let place = WidgetPlace::parse("top-right:320x140+16+24").expect("parses");
        assert_eq!(place.anchor, Anchor::TopRight);
        assert_eq!((place.w, place.h, place.x, place.y), (320, 140, 16, 24));

        assert!(WidgetPlace::parse("centre:100x100+0+0").is_some(), "both spellings");
        assert!(WidgetPlace::parse("sideways:100x100+0+0").is_none());
        assert!(WidgetPlace::parse("top-left:100x100").is_none(), "a size is not a placement");
        assert!(WidgetPlace::parse("garbage").is_none());
    }

    /// Every anchor puts the widget in its own corner of the *usable* area —
    /// below the dock, never off the edge.
    #[test]
    fn anchors_land_in_their_corners_below_the_dock() {
        let output = Size::<i32, Logical>::from((1000, 800));
        let top = 44; // the dock's reserved strip
        let at = |anchor: &str| {
            WidgetPlace::parse(&format!("{anchor}:200x100+10+20"))
                .unwrap()
                .origin(output, top)
        };
        assert_eq!(at("top-left"), Point::from((10, 64)));
        assert_eq!(at("top-right"), Point::from((790, 64)));
        assert_eq!(at("bottom-left"), Point::from((10, 680)));
        assert_eq!(at("bottom-right"), Point::from((790, 680)));
        // Centred in the *usable* area, not the output: 44 + (800-44-100)/2.
        assert_eq!(at("center"), Point::from((400, 372)));
    }

    /// Dropping a widget has to be the exact inverse of placing one, or a
    /// drag lands the widget somewhere and the next reflow puts it somewhere
    /// else — the "appeared to work but reverted" failure that dragging was
    /// refused over in the first place.
    #[test]
    fn a_dropped_widget_lands_exactly_where_it_was_dropped() {
        let output = Size::<i32, Logical>::from((1000, 800));
        let top = 44;
        let place = WidgetPlace::parse("top-left:200x100+10+20").unwrap();
        // Every quadrant, including points that change which corner anchors.
        for drop_at in [(10, 64), (700, 100), (30, 600), (760, 650), (400, 300)] {
            let at = Point::<i32, Logical>::from(drop_at);
            let moved = place.placed_at(at, output, top);
            assert_eq!(
                moved.origin(output, top),
                at,
                "{drop_at:?} did not survive the round trip as {:?}",
                moved.anchor
            );
            assert!(moved.x >= 0 && moved.y >= 0, "margins stay positive: {moved:?}");
            assert_eq!((moved.w, moved.h), (200, 100), "a move is not a resize");
        }
    }

    /// The anchor is re-chosen from where the widget ended up, so a widget
    /// dragged into a corner keeps hugging *that* corner when the screen
    /// changes size. A kept anchor would send it back across the desktop.
    #[test]
    fn dropping_re_anchors_to_the_nearest_corner() {
        let output = Size::<i32, Logical>::from((1000, 800));
        let top = 44;
        let place = WidgetPlace::parse("top-left:200x100+10+20").unwrap();
        let anchor_at = |x, y| place.placed_at(Point::from((x, y)), output, top).anchor;
        assert_eq!(anchor_at(10, 60), Anchor::TopLeft);
        assert_eq!(anchor_at(780, 60), Anchor::TopRight);
        assert_eq!(anchor_at(10, 690), Anchor::BottomLeft);
        assert_eq!(anchor_at(780, 690), Anchor::BottomRight);
        // And a centred widget, once dragged, is no longer centred — it has
        // to become a real corner or it would snap back to the middle.
        let centred = WidgetPlace::parse("center:200x100+0+0").unwrap();
        assert_ne!(centred.placed_at(Point::from((20, 70)), output, top).anchor, Anchor::Center);
    }

    /// Saving a dragged widget must edit three fields and touch nothing
    /// else. `theme.toml` is hand-written and full of comments, and a drag
    /// is a casual gesture someone might make fifty times while arranging a
    /// desktop — reserializing the document would delete their file's
    /// comments and reorder its tables, fifty times, silently.
    #[test]
    fn saving_a_drag_keeps_every_comment_and_only_moves_the_one_widget() {
        let dir = std::env::temp_dir().join(format!("rill-widget-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("theme.toml");
        let original = "\
# top-of-file comment
[colors]
page = \"#0a0a0a\"

# the meter belongs up there
[[desktop.widgets]]
app = \"rill://host/meter\"
anchor = \"top-right\"   # trailing comment
width = 300
height = 160
x = 20
y = 20

[[desktop.widgets]]
app = \"rill://host/ascii\"
anchor = \"bottom-left\"
width = 380
height = 240
x = 20
y = 20
";
        std::fs::write(&path, original).unwrap();

        let moved = WidgetPlace {
            anchor: Anchor::BottomRight,
            w: 300,
            h: 160,
            x: 44,
            y: 55,
        };
        save_widget_place_in(&path, "rill://host/meter", moved).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        // The edit landed.
        assert!(after.contains("anchor = \"bottom-right\""), "anchor moved:\n{after}");
        assert!(after.contains("x = 44") && after.contains("y = 55"), "offsets moved:\n{after}");
        // Every comment survived.
        assert!(after.contains("# top-of-file comment"), "lost the header comment");
        assert!(after.contains("# the meter belongs up there"), "lost a section comment");
        assert!(after.contains("# trailing comment"), "lost the trailing comment");
        // The *other* widget is untouched, byte for byte.
        assert!(
            after.contains("anchor = \"bottom-left\""),
            "the widget nobody dragged was rewritten:\n{after}"
        );
        // Sizes are not a move's business.
        assert!(after.contains("width = 300") && after.contains("height = 160"));

        // An app that isn't in the file is a no-op, not a corruption.
        save_widget_place_in(&path, "rill://host/nothing-like-this", moved).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after, "no entry, no rewrite");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A widget larger than the screen it landed on still has a top-left
    /// corner inside the desktop rather than off it.
    #[test]
    fn an_oversized_widget_stays_on_screen() {
        let output = Size::<i32, Logical>::from((320, 240));
        let place = WidgetPlace::parse("bottom-right:800x600+40+40").unwrap();
        let origin = place.origin(output, 44);
        assert_eq!(origin.x, 0);
        assert!(origin.y >= 44, "never above the dock: {origin:?}");
    }
}

#[cfg(test)]
mod image_budget_tests {
    use super::plan_release;
    use std::collections::HashSet;

    fn live(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// Held images, as (source, bytes, last-shown).
    const HELD: &[(&str, usize, u64)] =
        &[("a", 10, 1), ("b", 10, 5), ("c", 10, 3), ("onscreen", 10, 9)];

    /// Within budget, nothing is disturbed.
    #[test]
    fn nothing_is_released_while_there_is_room() {
        assert_eq!(plan_release(HELD, &live(&[]), 40, "new", 10, 100), Some(vec![]));
    }

    /// The picture on screen is never the one that goes — otherwise a frame
    /// would release what it is displaying and the client would immediately
    /// re-attach it, forever.
    #[test]
    fn what_is_on_screen_is_never_released() {
        // 40 held plus 10 incoming against a 45 budget: 5 must go, and the
        // smallest release that covers it is one image.
        let plan = plan_release(HELD, &live(&["onscreen"]), 40, "new", 10, 45)
            .expect("a droppable image exists");
        assert!(!plan.contains(&"onscreen".to_string()), "released a visible image: {plan:?}");
        assert_eq!(plan, vec!["a".to_string()], "least recently shown goes first");
    }

    /// Oldest first, and only as many as it takes.
    #[test]
    fn the_coldest_go_first_and_no_more_than_needed() {
        // 40 held plus 15 incoming against a 40 budget: 15 must go, so the
        // two coldest droppable (10 each) are taken and the third is not.
        let plan = plan_release(HELD, &live(&["onscreen"]), 40, "new", 15, 40).expect("fits");
        assert_eq!(plan, vec!["a".to_string(), "c".to_string()]);
    }

    /// When even releasing everything droppable is not enough, that is a
    /// refusal rather than a shortage: one frame is asking for more than a
    /// surface may hold, and dropping more would not help.
    #[test]
    fn a_frame_bigger_than_the_budget_is_refused() {
        assert_eq!(plan_release(HELD, &live(&["onscreen"]), 40, "new", 95, 100), None);
        // ...and the refusal is because of what is *on screen*: with nothing
        // pinned, the same request fits.
        assert!(plan_release(HELD, &live(&[]), 40, "new", 95, 100).is_some());
    }

    /// Re-attaching a source already held does not count itself as a victim.
    #[test]
    fn replacing_an_image_does_not_release_it() {
        let plan = plan_release(HELD, &live(&[]), 30, "b", 20, 50).expect("fits");
        assert!(!plan.contains(&"b".to_string()), "released the image being replaced");
    }
}

#[cfg(test)]
mod frame_time_tests {
    use super::FrameTimes;
    use std::time::Duration;

    fn ms(v: f64) -> Duration {
        Duration::from_secs_f64(v / 1000.0)
    }

    /// Percentiles over a known distribution. The bucket is 0.25 ms wide and
    /// a percentile reports its upper edge, so a 1.0 ms sample answers 1.25 —
    /// rounding *up* is deliberate: a frame-time budget that under-reports is
    /// worse than useless.
    #[test]
    fn percentiles_are_right_and_never_understate() {
        let mut t = FrameTimes::new();
        for _ in 0..99 {
            t.record(ms(1.0));
        }
        t.record(ms(50.0)); // one outlier in a hundred

        assert_eq!(t.count, 100);
        assert!((t.percentile_ms(0.50) - 1.25).abs() < 0.001, "{}", t.percentile_ms(0.50));
        assert!((t.percentile_ms(0.95) - 1.25).abs() < 0.001, "p95 is still the body");
        assert!(
            t.percentile_ms(0.99) >= 1.0 && t.percentile_ms(0.99) <= 1.25,
            "p99 sits at the last body sample: {}",
            t.percentile_ms(0.99)
        );
        assert!((t.max.as_secs_f64() * 1000.0 - 50.0).abs() < 0.01, "max is exact");
        // The mean is dragged by the outlier; the median is not. That
        // difference is the whole reason this exists.
        assert!(t.mean_ms() > 1.4, "mean feels the outlier: {}", t.mean_ms());
    }

    /// Anything past the last bucket reports the real maximum rather than the
    /// bucket edge, so a 400 ms stall cannot be reported as "≥64".
    #[test]
    fn the_overflow_bucket_reports_the_true_maximum() {
        let mut t = FrameTimes::new();
        for _ in 0..9 {
            t.record(ms(1.0));
        }
        t.record(ms(400.0));
        assert!((t.percentile_ms(1.0) - 400.0).abs() < 0.01, "{}", t.percentile_ms(1.0));
        assert!((t.max.as_secs_f64() * 1000.0 - 400.0).abs() < 0.01);
    }

    /// An empty histogram answers zero rather than dividing by nothing — the
    /// state every run starts in, and the one a crash-on-startup would report.
    #[test]
    fn an_empty_histogram_is_answerable() {
        let t = FrameTimes::new();
        assert_eq!(t.count, 0);
        assert_eq!(t.percentile_ms(0.5), 0.0);
        assert_eq!(t.mean_ms(), 0.0);
    }

    /// Memory is fixed regardless of how long the session runs — the property
    /// that makes this safe in a compositor that may be up for days.
    #[test]
    fn memory_does_not_grow_with_frames() {
        let before = std::mem::size_of::<FrameTimes>();
        let mut t = FrameTimes::new();
        for i in 0..100_000 {
            t.record(ms((i % 97) as f64 * 0.5));
        }
        assert_eq!(std::mem::size_of_val(&t), before, "fixed size");
        assert_eq!(t.count, 100_000);
    }
}
