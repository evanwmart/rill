//! The Rill-owned wgpu renderer (plan: specs/wgpu-renderer.md, milestone W1).
//!
//! `rill-ui` emits [`DrawCommand`]s; this crate paints them on a wgpu device
//! with **no gpui, no window, no compositor** — a headless renderer that draws
//! into an offscreen texture, so the whole thing is deterministic and
//! unit-testable by hashing pixels. A windowed/compositor target comes later
//! (W3/W4); the pipelines here are the shared core.
//!
//! Slices landed:
//! * W1.1 — device bring-up + offscreen readback.
//! * W1.2 — one **SDF quad** pipeline covering `Rect` (sharp/rounded, AA) and
//!   `Shadow` (blur falloff), plus an **ordered executor** that walks the
//!   command list in paint order and applies `PushClip`/`PopClip` as scissor
//!   rects.
//! * W1.3 — **images**: an [`ImageSource`] supplies RGBA pixels per resource
//!   path; each `Image` draws as a textured quad in paint order, and missing
//!   images paint the documented placeholder box.
//! * W1.4 — **text**: cosmic-text shapes (measurement shares the wrap
//!   arithmetic in `rill_ui::text` — D3's parity core), swash rasterizes into
//!   the coverage [`atlas`], and a glyph pipeline paints atlas quads tinted by
//!   the text color. The full `DrawCommand` vocabulary now renders.

mod atlas;
pub mod mesh;
pub mod dmabuf;
pub mod text;

use std::sync::{Arc, Mutex};

use bytemuck::{Pod, Zeroable};
use rill_ui::text::{LINE_HEIGHT_FACTOR, wrap_segments};
use rill_ui::{Color, DrawCommand, Point, Rect};
use wgpu::util::DeviceExt;

use atlas::{ATLAS_SIZE, GlyphAtlas};
use text::TextEngine;

/// Headless-target format. **Linear** (non-sRGB) on purpose: a color written
/// as `channel/255` reads back as exactly `channel`, so pixel-hash tests are
/// bit-exact and independent of any gamma curve. A windowed/compositor
/// renderer passes its surface format to [`Renderer::with_device`] instead.
const HEADLESS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// One rounded-box primitive in logical-pixel space. A sharp rect is
/// `radius = 0, blur = 0`; a rounded rect sets `radius`; a shadow sets `blur`
/// (and folds `spread` into `half`). All three run through one pipeline so they
/// stay in paint order without cross-pipeline flushing.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct QuadInstance {
    center: [f32; 2],
    half: [f32; 2],
    radius: f32,
    blur: f32,
    color: [f32; 4],
    /// Stroke width for an outline; 0 fills the shape. A border is the same
    /// signed distance the fill already computes, kept near zero instead of
    /// below it — so it costs one attribute and one branch, not a pipeline.
    stroke: f32,
    _pad: [f32; 3],
}

/// One filled path, drawn by `shaders/fill.wgsl`: a bounding-box quad whose
/// fragments ray-cast a slice of the frame's shared segment buffer.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct FillInstance {
    bbox: [f32; 4],
    color: [f32; 4],
    seg: [u32; 2],
    _pad: [u32; 2],
}

/// One segment of a [`DrawCommand::Path`], drawn as a capsule by
/// `shaders/line.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct LineInstance {
    p0: [f32; 2],
    p1: [f32; 2],
    width: f32,
    color: [f32; 4],
}

/// Expand a polyline into per-segment instances. A lone point becomes a
/// zero-length segment, which the SDF renders as a dot — the documented
/// meaning of a single-point path.
fn line_instances(points: &[Point], color: Color, width: f32, closed: bool) -> Vec<LineInstance> {
    let color = color_to_linear(color);
    let seg = |a: &Point, b: &Point| LineInstance {
        p0: [a.x, a.y],
        p1: [b.x, b.y],
        width,
        color,
    };
    match points {
        [] => Vec::new(),
        [only] => vec![seg(only, only)],
        _ => {
            let mut out: Vec<LineInstance> =
                points.windows(2).map(|w| seg(&w[0], &w[1])).collect();
            if closed && points.len() > 2 {
                out.push(seg(&points[points.len() - 1], &points[0]));
            }
            out
        }
    }
}

fn color_to_linear(c: Color) -> [f32; 4] {
    [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0, c.a as f32 / 255.0]
}

fn quad_from_rect(rect: Rect, color: Color, corner_radius: f32) -> QuadInstance {
    QuadInstance {
        center: [rect.x + rect.w / 2.0, rect.y + rect.h / 2.0],
        half: [rect.w / 2.0, rect.h / 2.0],
        radius: corner_radius,
        blur: 0.0,
        color: color_to_linear(color),
        stroke: 0.0,
        _pad: [0.0; 3],
    }
}

/// A hairline outline: the same rounded-box distance field, drawn where the
/// distance is near zero rather than below it.
fn quad_from_border(rect: Rect, color: Color, width: f32, corner_radius: f32) -> QuadInstance {
    QuadInstance {
        center: [rect.x + rect.w / 2.0, rect.y + rect.h / 2.0],
        half: [rect.w / 2.0, rect.h / 2.0],
        radius: corner_radius,
        blur: 0.0,
        color: color_to_linear(color),
        stroke: width.max(0.0),
        _pad: [0.0; 3],
    }
}

fn quad_from_shadow(
    rect: Rect,
    color: Color,
    blur: f32,
    spread: f32,
    corner_radius: f32,
) -> QuadInstance {
    QuadInstance {
        center: [rect.x + rect.w / 2.0, rect.y + rect.h / 2.0],
        half: [rect.w / 2.0 + spread, rect.h / 2.0 + spread],
        radius: corner_radius,
        blur: blur.max(0.0),
        color: color_to_linear(color),
        stroke: 0.0,
        _pad: [0.0; 3],
    }
}

/// A glow rides the quad pipeline with a *negative* blur as the marker: the
/// shader renders coverage only outside the shape, peaking at the edge.
fn quad_from_glow(rect: Rect, color: Color, blur: f32, corner_radius: f32) -> QuadInstance {
    QuadInstance {
        center: [rect.x + rect.w / 2.0, rect.y + rect.h / 2.0],
        half: [rect.w / 2.0, rect.h / 2.0],
        radius: corner_radius,
        blur: -blur.max(0.5),
        color: color_to_linear(color),
        stroke: 0.0,
        _pad: [0.0; 3],
    }
}

/// Decoded RGBA8 pixels for one image resource (tightly packed, row-major).
pub struct ImageData {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Backend image lookup (mirrors the gpui backend's `ImageProvider`): the host
/// resolves/loads image resources itself; sources that return `None` paint the
/// placeholder box.
pub trait ImageSource {
    /// Pixels for `source`, for the renderer to upload.
    ///
    /// Prefer [`ImageSource::texture`] where the caller already owns a
    /// texture: this path re-uploads on every frame that names the image.
    fn rgba(&self, _source: &str) -> Option<ImageData> {
        None
    }

    /// An already-uploaded texture for `source`.
    ///
    /// Tried first. A host that receives pixels once and keeps them — the
    /// compositor, which takes them over `attach_image` and uploads them on
    /// arrival — answers here, and the frame costs a bind group instead of a
    /// texture creation and a full copy per frame.
    fn texture(&self, _source: &str) -> Option<&wgpu::TextureView> {
        None
    }
}

/// The no-images source, as a value that can be borrowed for `'static` —
/// what [`SceneLayer::commands`] hands to a layer that has none.
static NO_IMAGES: NoImageSource = NoImageSource;

/// Source with no images (placeholders everywhere).
pub struct NoImageSource;

impl ImageSource for NoImageSource {
    fn rgba(&self, _source: &str) -> Option<ImageData> {
        None
    }
}

/// Placeholder-box colors — same as the gpui backend paints, so a backend swap
/// doesn't change what a missing image looks like.
const PLACEHOLDER_OUTER: Color = Color { r: 0xD8, g: 0xD8, b: 0xE2, a: 0xFF };
const PLACEHOLDER_INNER: Color = Color { r: 0xC2, g: 0xC2, b: 0xD2, a: 0xFF };

/// One image draw, as uploaded to the GPU (logical-pixel space; the texture is
/// stretched across the rect). `alpha` fades the whole texture — window
/// spawn animations and glass-dimmed shell surfaces.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ImageInstance {
    pos: [f32; 2],
    size: [f32; 2],
    alpha: f32,
}

/// One draw: a contiguous instance range under a scissor rectangle. A `None`
/// scissor means the current clip is empty — the range is skipped entirely.
struct Span {
    scissor: Option<[u32; 4]>,
    start: u32,
    end: u32,
    /// Index into the frame's mask table (0 = no rounded clip active).
    mask: u32,
}

/// One glyph quad: an atlas rectangle placed on screen, tinted by the text
/// color (the atlas stores coverage only).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GlyphInstance {
    pos: [f32; 2],
    size: [f32; 2],
    uv_pos: [f32; 2],
    uv_size: [f32; 2],
    color: [f32; 4],
}

/// One executor step, in paint order.
enum DrawItem {
    /// Draw quad instances `start..end` (rects + shadows).
    Quads(Span),
    /// Draw line instances `start..end` (path segments).
    Lines(Span),
    /// Draw fill instances `start..end` (filled contours; icons).
    Fills(Span),
    /// Draw image instance `index` with its own texture bind group.
    Image { scissor: Option<[u32; 4]>, index: u32, mask: u32 },
    /// Draw glyph instances `start..end` against the shared atlas.
    Glyphs(Span),
    /// Frosted glass: blur what has accumulated behind `rect` and paint the
    /// result as a rounded pane. Honored only by the fx composite path
    /// ([`Renderer::composite_scene`]) — a plain [`Renderer::paint_frame`]
    /// has no accumulation to sample and skips it.
    Backdrop { scissor: Option<[u32; 4]>, rect: [f32; 4], blur: f32, corner_radius: f32 },
}

/// The showcase scene's knobs, shared by the background shader and the
/// model pass so the room and the object cannot disagree: lights, spin,
/// camera distance, and the two colors the scene owns. Laid out as vec4
/// rows on purpose — WGSL's 16-byte alignment makes anything else a
/// silent buffer-overrun waiting to happen.
#[derive(Clone, Copy, Debug)]
pub struct SceneParams {
    /// Key light: direction (unit) + intensity in `w`.
    pub key: [f32; 4],
    pub key_color: [f32; 4],
    /// Fill light; `w <= 0` turns it off.
    pub fill: [f32; 4],
    pub fill_color: [f32; 4],
    /// Rim/back light colour, intensity in `w`.
    pub rim_color: [f32; 4],
    /// Model surface override: rgb, and `w >= 0.5` to apply it.
    pub body_color: [f32; 4],
    /// The studio floor's colour.
    pub ground_color: [f32; 4],
    /// (spin_rate — signed, so negative reverses; spin_phase; camera
    /// distance; exposure).
    pub motion: [f32; 4],
    /// The backdrop's own colour — independent of the lights, so a warm key
    /// doesn't drag the whole room orange. `w` is the turntable ring
    /// strength.
    pub backdrop_color: [f32; 4],
    /// (floor reflection strength; its fade distance; how much light bounces
    /// onto the backdrop; vignette).
    pub finish: [f32; 4],
    /// How a generic model is fitted: (up axis — 0 = Y, 1 = Z; scale, where
    /// 1.0 is "comfortably framed" for the current camera; lift above the
    /// floor; spare). The axis is declared rather than guessed: "taller than
    /// it is deep" is not a reliable test, and a mis-guess stands a model on
    /// its face.
    pub fit: [f32; 4],
}

impl Default for SceneParams {
    fn default() -> SceneParams {
        SceneParams {
            key: [-0.55, 0.82, 0.62, 7.2],
            key_color: [1.0, 0.71, 0.54, 0.0],
            fill: [0.82, 0.28, 0.48, 1.8],
            fill_color: [0.45, 0.61, 1.0, 0.0],
            rim_color: [0.40, 0.53, 1.0, 2.6],
            body_color: [0.19, 0.19, 0.19, 0.0],
            ground_color: [0.030, 0.031, 0.040, 0.0],
            motion: [0.08, 0.44, 3.88, 1.0],
            backdrop_color: [0.048, 0.050, 0.068, 1.0],
            finish: [0.30, 0.42, 0.45, 0.55],
            fit: [0.0, 1.0, 0.0, 0.0],
        }
    }
}

impl SceneParams {
    fn as_rows(&self) -> [[f32; 4]; 11] {
        [
            self.key,
            self.key_color,
            self.fill,
            self.fill_color,
            self.rim_color,
            self.body_color,
            self.ground_color,
            self.motion,
            self.backdrop_color,
            self.finish,
            self.fit,
        ]
    }
}

/// What the desktop currently sounds like, as the shaders see it. Filled
/// by the compositor's tap on the system output monitor; all-zero when no
/// tap is running, which every reactive shader must read as silence.
///
/// The smoothing lives on the *producer* side on purpose: raw FFT frames
/// strobe at frame rate, and every shader author would otherwise
/// re-implement the same envelope follower badly. A shader receives values
/// that already move like music.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct AudioFx {
    /// (bass, mid, treble, level), each 0..~1 — attack/decay smoothed and
    /// slow-AGC normalised, so a quiet track still moves a wallpaper.
    pub bands: [f32; 4],
    /// x = beat: 1.0 on a bass onset, decaying to 0 — the difference
    /// between a wallpaper that dances and one that meters. y = raw
    /// unsmoothed level this instant. z = beats heard so far, a monotonic
    /// counter — what lets a stateless shader change per beat (cycle a
    /// palette, reseed sparks) rather than merely pulse with one.
    /// w = raw kick-band energy (~40–120Hz, unsmoothed, AGC-normalised) —
    /// the thump itself, for shaders that want punch rather than pulse.
    pub pulse: [f32; 4],
    /// 32 log-spaced spectrum bands, low → high, 4 per row — smoothed and
    /// normalised like `bands`.
    pub spectrum: [[f32; 4]; 8],
}

impl AudioFx {
    /// The uniform layout the preambles document: row 0 `bands`, row 1
    /// `pulse`, rows 2..9 the spectrum.
    fn as_rows(&self) -> [[f32; 4]; 10] {
        let mut rows = [[0.0; 4]; 10];
        rows[0] = self.bands;
        rows[1] = self.pulse;
        rows[2..].copy_from_slice(&self.spectrum);
        rows
    }
}

/// Per-frame inputs for the effect/background shaders (D5): wall-clock
/// time, the cursor position, and the live window layout (screen-pixel
/// rects, up to 64 used) — wallpapers can react to where windows are.
#[derive(Clone, Default)]
pub struct FxInputs {
    pub time: f32,
    /// Seconds since local midnight — the wall clock, for scenes with a
    /// time of day (an environmental clock, a sky that knows evening).
    pub clock: f32,
    pub cursor: [f32; 2],
    pub windows: Vec<[f32; 4]>,
    /// The showcase scene's knobs (lights, spin, camera, colours).
    pub scene: SceneParams,
    /// Per-window scene semantics, parallel to `windows`: (spawn_age_secs,
    /// focused, kind, speed_px_per_sec). kind: 0 = app, 1 = dock/shell.
    /// What turns a wallpaper from scenery into a scene — it can aura the
    /// focused window, ripple at a spawn, wake behind a drag. Read-only by
    /// design: shaders observe the desktop, they never become UI.
    pub window_meta: Vec<[f32; 4]>,
    /// Per-window velocity in screen pixels per second, parallel to
    /// `windows`: (vx, vy, _, _). `window_meta`'s speed is a magnitude and
    /// cannot say which way a window went — a flame leaning away from a
    /// drag, a wake trailing behind one, need the direction.
    pub window_velocity: Vec<[f32; 4]>,
    /// What the desktop sounds like right now — see [`AudioFx`]. Default
    /// (all zero) is silence.
    pub audio: AudioFx,
}

/// A compiled whole-output effect (user WGSL fragment stage).
struct EffectState {
    pipeline: wgpu::RenderPipeline,
    animated: bool,
}

/// An installed 3D model layer: a flat vertex buffer and the pipeline built
/// from its cinematic shader (the shader owns camera, materials, lighting —
/// the renderer owns geometry, depth, and the pass).
struct ModelState {
    pipeline: wgpu::RenderPipeline,
    vertex_buf: wgpu::Buffer,
    vertex_count: u32,
    frame_bind_group_layout: wgpu::BindGroupLayout,
    /// The mesh's axis-aligned bounds, handed to the shader each frame.
    bounds_min: [f32; 3],
    bounds_max: [f32; 3],
}

/// Cached fx render targets, recreated when the output size changes: the
/// full-res accumulation the scene composites into, plus the half/quarter-res
/// pair the dual-Kawase blur chain ping-pongs through.
struct FxCache {
    w: u32,
    h: u32,
    // Views keep their textures alive; no need to hold the textures too.
    accum_view: wgpu::TextureView,
    half_view: wgpu::TextureView,
    half_w: u32,
    half_h: u32,
    quarter_view: wgpu::TextureView,
    quarter_w: u32,
    quarter_h: u32,
    /// The model layer's own pass targets: color (transparent-cleared,
    /// composited as a texture layer) and depth — the only depth buffer in
    /// the renderer; the 2D pipelines stay depthless.
    model_view: wgpu::TextureView,
    model_depth_view: wgpu::TextureView,
}

/// One backdrop-pane draw (pos, size, corner radius) — vertex instance data.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BackdropInstance {
    pos: [f32; 2],
    size: [f32; 2],
    radius: f32,
}

/// Kawase pass uniform: source texel size + sample offset in texels.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct KawaseUniform {
    texel: [f32; 2],
    offset: f32,
    _pad: f32,
}

/// A command list prepared for painting: GPU buffers plus the ordered item
/// list. Opaque — produced by [`Renderer::build_frame`], painted by
/// [`Renderer::paint_frame`].
pub struct FrameData {
    items: Vec<DrawItem>,
    mask_bind_group: wgpu::BindGroup,
    quad_buf: wgpu::Buffer,
    line_buf: wgpu::Buffer,
    fill_buf: wgpu::Buffer,
    fill_bind_group: wgpu::BindGroup,
    image_buf: wgpu::Buffer,
    glyph_buf: wgpu::Buffer,
    image_bind_groups: Vec<wgpu::BindGroup>,
    viewport_bind_group: wgpu::BindGroup,
}

/// One layer of a composited output frame, painted in list order.
pub enum SceneLayer<'a> {
    /// DrawCommands already translated into output coordinates — a
    /// vector-native window's frame, the wallpaper, or an overlay.
    ///
    /// `images` belongs to *this* layer, not the scene: an image source is a
    /// bare string, and two windows naming `/logo.png` mean their own — they
    /// may be talking to different servers. Use [`SceneLayer::commands`] for
    /// a layer that has no images.
    Commands { commands: &'a [DrawCommand], images: &'a dyn ImageSource },
    /// An external texture (imported dmabuf or shm upload) stretched across
    /// its on-screen rect — an ordinary client window. `alpha` fades the
    /// whole surface (1.0 = opaque).
    Texture { view: &'a wgpu::TextureView, rect: Rect, alpha: f32 },
    /// The installed background shader ([`Renderer::set_background`]) as a
    /// fullscreen generative pass at this z — a shader wallpaper. Painted as
    /// nothing when no background shader is installed. The preamble's
    /// `scene` texture is undefined in this pass — background shaders
    /// generate, they don't sample.
    Shader,
    /// The installed model ([`Renderer::set_model`]) rendered through its
    /// own depth pass and composited here — a rotating showcase object
    /// between wallpaper and windows. Paints nothing when no model is set.
    Model,
    /// The installed per-window effect ([`Renderer::set_window_fx`]) for one
    /// window, drawn at this z and blended into the scene.
    ///
    /// Place it directly above the window it belongs to. That position is
    /// the entire feature: an effect painted here is occluded by anything
    /// stacked higher, and is picked up by a glass window's backdrop blur,
    /// neither of which a whole-output grader can do — a grader runs after
    /// the frame is finished and can only guess at occlusion by testing
    /// window rects.
    ///
    /// `bounds` scissors the pass to the region the effect can reach: its
    /// window plus however far the effect spills beyond it. The renderer
    /// cannot know that reach, so the caller states it; a wrong-but-larger
    /// rect costs fill rate, a too-small one clips the effect.
    WindowFx { window: u32, bounds: Rect },
    /// One half of the compute-simulated boid flock
    /// ([`Renderer::set_boids`]): `front == false` draws the back half
    /// (between wallpaper and windows), `front == true` the front half
    /// (over the windows). Paints nothing when the flock is off.
    Boids { front: bool },
}

impl<'a> SceneLayer<'a> {
    /// A commands layer with no images of its own.
    pub fn commands(commands: &'a [DrawCommand]) -> SceneLayer<'a> {
        SceneLayer::Commands { commands, images: &NO_IMAGES }
    }
}

/// Intersect the clip stack with the target bounds → a pixel scissor rect, or
/// `None` when the intersection is empty. (DPR is 1 for now; snapping to the
/// pixel grid is a renderer concern — the pixels-vs-vectors invariant.)
fn compute_scissor(clip: &[[f32; 4]], w: u32, h: u32) -> Option<[u32; 4]> {
    let (mut x0, mut y0, mut x1, mut y1) = (0.0f32, 0.0f32, w as f32, h as f32);
    for c in clip {
        x0 = x0.max(c[0]);
        y0 = y0.max(c[1]);
        x1 = x1.min(c[2]);
        y1 = y1.min(c[3]);
    }
    let xi = x0.floor().clamp(0.0, w as f32) as u32;
    let yi = y0.floor().clamp(0.0, h as f32) as u32;
    let xe = x1.ceil().clamp(0.0, w as f32) as u32;
    let ye = y1.ceil().clamp(0.0, h as f32) as u32;
    (xe > xi && ye > yi).then_some([xi, yi, xe - xi, ye - yi])
}

/// Open (or reopen) the fx accumulation pass: the first open clears to the
/// scene's clear color, later reopens load what's already accumulated.
/// `forget_lifetime` releases the encoder borrow so the pass can be held in
/// an `Option` across blur-chain breaks.
fn begin_accum(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    first: &mut bool,
    clear: Color,
) -> wgpu::RenderPass<'static> {
    let load = if *first {
        wgpu::LoadOp::Clear(Renderer::wgpu_clear(clear))
    } else {
        wgpu::LoadOp::Load
    };
    *first = false;
    encoder
        .begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("accum"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        })
        .forget_lifetime()
}

/// A headless wgpu renderer. Owns the device/queue and the pipelines; reused
/// across frames.
pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    quad_pipeline: wgpu::RenderPipeline,
    line_pipeline: wgpu::RenderPipeline,
    fill_pipeline: wgpu::RenderPipeline,
    fill_bind_group_layout: wgpu::BindGroupLayout,
    image_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    viewport_bind_group_layout: wgpu::BindGroupLayout,
    /// Per-span rounded-clip mask params (dynamic-offset uniform).
    mask_bind_group_layout: wgpu::BindGroupLayout,
    mask_stride: u32,
    /// The permanent "no rounded clip" entry, for paths that drive masked
    /// pipelines outside a frame's span list (window textures in composite).
    no_mask_bind_group: wgpu::BindGroup,
    image_bind_group_layout: wgpu::BindGroupLayout,
    image_sampler: wgpu::Sampler,
    glyph_bind_group: wgpu::BindGroup,
    text_engine: Arc<TextEngine>,
    atlas: Mutex<GlyphAtlas>,
    format: wgpu::TextureFormat,
    adapter_name: String,
    // The fx path (D5/D6): backdrop panes, the Kawase blur chain, and the
    // whole-output effect slot.
    backdrop_pipeline: wgpu::RenderPipeline,
    kawase_bind_group_layout: wgpu::BindGroupLayout,
    kawase_down_pipeline: wgpu::RenderPipeline,
    kawase_up_pipeline: wgpu::RenderPipeline,
    fx_bind_group_layout: wgpu::BindGroupLayout,
    /// The desktop-audio uniform ([`AudioFx::as_rows`]), shared by the fx
    /// and particle bind groups and rewritten once per composite. One
    /// persistent buffer so the cached particle bind groups never go stale.
    audio_buf: wgpu::Buffer,
    /// Declared `@param` values (`fx_params`, 8 vec4 rows), one buffer per
    /// effect pass so the background, the screen effect and a window effect
    /// each read their own shader's knobs. Written by the `set_*_params`
    /// setters when the theme or shader changes; zero until someone does.
    bg_params_buf: wgpu::Buffer,
    fx_params_buf: wgpu::Buffer,
    window_params_buf: wgpu::Buffer,
    identity_pipeline: wgpu::RenderPipeline,
    effect: Mutex<Option<EffectState>>,
    background: Mutex<Option<EffectState>>,
    /// The per-window effect: drawn once per window, immediately above that
    /// window in the scene, alpha-blended. See [`SceneLayer::WindowFx`].
    window_fx: Mutex<Option<EffectState>>,
    model: Mutex<Option<ModelState>>,
    fx_cache: Mutex<Option<FxCache>>,
    boid_compute_pipeline: wgpu::ComputePipeline,
    boid_render_pipeline: wgpu::RenderPipeline,
    boid_compute_layout: wgpu::BindGroupLayout,
    boid_render_layout: wgpu::BindGroupLayout,
    boids: Mutex<Option<BoidsState>>,
    /// Theme-supplied particle shaders, replacing the built-in flock. `None`
    /// on either side means "use the built-in".
    particle_compute: Mutex<Option<wgpu::ComputePipeline>>,
    particle_render: Mutex<Option<wgpu::RenderPipeline>>,
    /// The field pass: blur and decay over the trail, dispatched per pixel.
    /// Optional — a particle wallpaper that leaves no trail installs none.
    particle_diffuse: Mutex<Option<wgpu::ComputePipeline>>,
}

/// GPU state of the boid flock: double-buffered storage stepped by a
/// compute pass, read by an instanced render pass.
struct BoidsState {
    count: u32,
    params_buf: wgpu::Buffer,
    obstacles_buf: wgpu::Buffer,
    /// Window velocity, parallel to `obstacles_buf`.
    window_vel_buf: wgpu::Buffer,
    /// The ping-pong pair. `bufs[current]` holds the latest step, which is
    /// what [`Renderer::read_particles`] copies out.
    bufs: [wgpu::Buffer; 2],
    /// The trail field, one f32 per pixel, double buffered alongside the
    /// particles. `None` until a size is known.
    trail: Option<[wgpu::Buffer; 2]>,
    /// The output size the trail was allocated for; a resize reallocates.
    trail_size: (u32, u32),
    /// `[a→b, b→a]` compute bind groups; `render_binds[current][layer]`
    /// reads the buffer holding the latest state for the back (0) or front
    /// (1) half of the depth band. Empty until the first step knows the
    /// output size, because they name the trail field.
    compute_binds: Vec<wgpu::BindGroup>,
    render_binds: Vec<Vec<wgpu::BindGroup>>,
    current: usize,
}

/// Uniforms for one boid step (std140-friendly: 32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BoidParams {
    count: u32,
    nwin: u32,
    dt: f32,
    time: f32,
    size: [f32; 2],
    cursor: [f32; 2],
}

/// Cap on window-obstacle rects fed to the flock per step.
pub const MAX_BOID_OBSTACLES: usize = 64;

/// Ceiling on simulated particles. High enough for a field simulation to
/// have structure, bounded so a typo in a theme cannot ask for a gigabyte.
pub const MAX_PARTICLES: u32 = 1_000_000;

impl Renderer {
    /// Bring up a headless device. Prefers a real GPU, falls back to any
    /// adapter (llvmpipe/software) so tests run on GPU-less machines too.
    /// Returns `None` if no wgpu adapter exists at all.
    pub fn new_headless() -> Option<Renderer> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let mut adapters = instance.enumerate_adapters(wgpu::Backends::all());
        if adapters.is_empty() {
            return None;
        }
        adapters.sort_by_key(|a| match a.get_info().device_type {
            wgpu::DeviceType::DiscreteGpu => 0,
            wgpu::DeviceType::IntegratedGpu => 1,
            wgpu::DeviceType::VirtualGpu => 2,
            wgpu::DeviceType::Cpu => 3,
            wgpu::DeviceType::Other => 4,
        });
        let adapter = adapters.into_iter().next()?;
        let adapter_name = adapter.get_info().name;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("rill-gpu"),
            required_features: wgpu::Features::empty(),
            // Downlevel defaults, but let buffers grow to whatever the
            // adapter really allows: a print-quality mesh is hundreds of MB
            // of vertices, and the 256MB default rejects it with a
            // validation error rather than an honest "too big".
            required_limits: wgpu::Limits {
                max_buffer_size: adapter.limits().max_buffer_size,
                ..wgpu::Limits::downlevel_defaults()
            },
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .ok()?;

        Some(Renderer::with_device(device, queue, HEADLESS_FORMAT, adapter_name))
    }

    /// Build the pipelines on an existing device — how the compositor mounts
    /// rill-gpu on its dmabuf-capable device, with `format` matching the
    /// surface it presents to. Every target view rendered into must have this
    /// format.
    pub fn with_device(
        device: wgpu::Device,
        queue: wgpu::Queue,
        format: wgpu::TextureFormat,
        adapter_name: String,
    ) -> Renderer {
        let viewport_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("viewport"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    // Fragment too: a field simulation colours the trail in
                    // the fragment stage and needs the output size there to
                    // index it. Widening visibility costs nothing to the
                    // pipelines that only read it in the vertex stage.
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        // The rounded-clip mask: one 32-byte params slot per active mask,
        // selected per span by dynamic offset. Fragment-only — vertices are
        // never moved by a mask, only coverage.
        let mask_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("clip-mask"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(32),
                    },
                    count: None,
                }],
            });
        let mask_stride = device.limits().min_uniform_buffer_offset_alignment.max(32);
        let no_mask_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("no-mask"),
            contents: &[0u8; 32],
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let no_mask_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("no-mask"),
            layout: &mask_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &no_mask_buf,
                    offset: 0,
                    size: wgpu::BufferSize::new(32),
                }),
            }],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quad"),
            source: wgpu::ShaderSource::Wgsl(QUAD_WGSL.into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quad"),
            bind_group_layouts: &[&viewport_bind_group_layout, &mask_bind_group_layout],
            push_constant_ranges: &[],
        });

        let quad_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quad"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<QuadInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2, 1 => Float32x2, 2 => Float32, 3 => Float32,
                        4 => Float32x4, 5 => Float32
                    ],
                }],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        let line_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("line"),
            source: wgpu::ShaderSource::Wgsl(LINE_WGSL.into()),
        });

        // Shares the viewport-only layout with the quad pipeline, so bind
        // group 0 survives the pipeline switch mid-pass.
        let line_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("line"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &line_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<LineInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2, 1 => Float32x2, 2 => Float32, 3 => Float32x4
                    ],
                }],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &line_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        let fill_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("fill-segments"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
        let fill_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("fill"),
                bind_group_layouts: &[
                    &viewport_bind_group_layout,
                    &fill_bind_group_layout,
                    &mask_bind_group_layout,
                ],
                push_constant_ranges: &[],
            });
        let fill_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fill"),
            source: wgpu::ShaderSource::Wgsl(FILL_WGSL.into()),
        });
        let fill_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fill"),
            layout: Some(&fill_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &fill_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<FillInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x4, 1 => Float32x4, 2 => Uint32x2
                    ],
                }],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &fill_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        let image_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("image"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let image_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image"),
            source: wgpu::ShaderSource::Wgsl(IMAGE_WGSL.into()),
        });
        let image_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image"),
            bind_group_layouts: &[
                &viewport_bind_group_layout,
                &image_bind_group_layout,
                &mask_bind_group_layout,
            ],
            push_constant_ranges: &[],
        });
        let image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image"),
            layout: Some(&image_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &image_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ImageInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2, 1 => Float32x2, 2 => Float32
                    ],
                }],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &image_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        let image_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Glyphs: the atlas, a nearest sampler (atlas quads are drawn 1:1 —
        // nearest avoids neighbor bleed), and a pipeline that tints coverage.
        // The bind-group layout is the same texture+sampler shape as images.
        let (glyph_atlas, atlas_view) = GlyphAtlas::new(&device);
        let glyph_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyph"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let glyph_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glyph"),
            layout: &image_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&glyph_sampler),
                },
            ],
        });
        let glyph_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glyph"),
            source: wgpu::ShaderSource::Wgsl(GLYPH_WGSL.into()),
        });
        let glyph_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glyph"),
            bind_group_layouts: &[
                &viewport_bind_group_layout,
                &image_bind_group_layout,
                &mask_bind_group_layout,
            ],
            push_constant_ranges: &[],
        });
        let glyph_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glyph"),
            layout: Some(&glyph_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &glyph_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<GlyphInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2, 1 => Float32x2, 2 => Float32x2, 3 => Float32x2,
                        4 => Float32x4
                    ],
                }],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &glyph_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        // Backdrop panes: same viewport+texture bind shape as images, but the
        // fragment samples the blurred scene at the pane's *screen* position
        // and masks to the rounded rect.
        let backdrop_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("backdrop"),
            source: wgpu::ShaderSource::Wgsl(BACKDROP_WGSL.into()),
        });
        let backdrop_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("backdrop"),
                bind_group_layouts: &[&viewport_bind_group_layout, &image_bind_group_layout],
                push_constant_ranges: &[],
            });
        let backdrop_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("backdrop"),
            layout: Some(&backdrop_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &backdrop_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<BackdropInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2, 1 => Float32x2, 2 => Float32
                    ],
                }],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &backdrop_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        // Dual-Kawase blur: one shader, down/up fragment entries, fullscreen
        // triangle vertex stage. No blending — each pass fully rewrites its
        // target.
        let kawase_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("kawase"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        let kawase_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kawase"),
            source: wgpu::ShaderSource::Wgsl(KAWASE_WGSL.into()),
        });
        let kawase_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("kawase"),
                bind_group_layouts: &[&kawase_bind_group_layout],
                push_constant_ranges: &[],
            });
        let kawase_pipeline = |entry: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&kawase_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &kawase_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &kawase_shader,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview: None,
                cache: None,
            })
        };
        let kawase_down_pipeline = kawase_pipeline("fs_down");
        let kawase_up_pipeline = kawase_pipeline("fs_up");

        // The whole-output effect slot: scene texture + uniforms in, one
        // fullscreen pass out. The identity pipeline is the no-effect blit.
        let fx_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("fx"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 4,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 5,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 6,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 7,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 8,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    // Desktop audio (AudioFx rows) — silence unless a
                    // compositor is feeding the tap.
                    wgpu::BindGroupLayoutEntry {
                        binding: 9,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    // Declared shader parameters (`// @param`) — one block
                    // per pass, so the background and a window effect tune
                    // independently.
                    wgpu::BindGroupLayoutEntry {
                        binding: 10,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        let identity_pipeline = Self::build_effect_pipeline(
            &device,
            format,
            &fx_bind_group_layout,
            &format!("{EFFECT_PREAMBLE}\n{IDENTITY_EFFECT}"),
            "identity",
            None,
        );

        // Boids: a compute pass steps the flock, an instanced render pass
        // draws it (velocity-oriented triangles pulled from storage).
        let storage_ro = |binding: u32, visibility: wgpu::ShaderStages| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let storage_rw = |binding: u32, visibility: wgpu::ShaderStages| {
            wgpu::BindGroupLayoutEntry {
                binding,
                visibility,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }
        };
        let boid_compute_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("boid-compute"),
                entries: &[
                    storage_ro(0, wgpu::ShaderStages::COMPUTE),
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: false },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    // Window velocity, parallel to the obstacle rects. A
                    // particle can be *pushed* by a drag only if it knows
                    // which way the window is going; a rect alone can only
                    // be avoided.
                    wgpu::BindGroupLayoutEntry {
                        binding: 4,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    // The trail field, one f32 per output pixel, double
                    // buffered like the particles. A simulation whose agents
                    // leave something behind — a slime mould sensing its own
                    // deposits, a fluid, an ink — needs a surface that
                    // persists between frames and can be blurred; particle
                    // state alone cannot express that.
                    storage_rw(5, wgpu::ShaderStages::COMPUTE),
                    storage_rw(6, wgpu::ShaderStages::COMPUTE),
                    // Desktop audio (AudioFx rows) — a simulation can be
                    // kicked by a beat, not just pushed by a window.
                    wgpu::BindGroupLayoutEntry {
                        binding: 7,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        let boid_compute_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("boid-compute"),
            source: wgpu::ShaderSource::Wgsl(
                format!("{PARTICLE_COMPUTE_PREAMBLE}\n{BOIDS_COMPUTE_WGSL}").into(),
            ),
        });
        let boid_compute_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("boid-compute"),
                bind_group_layouts: &[&boid_compute_layout],
                push_constant_ranges: &[],
            });
        let boid_compute_pipeline =
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("boid-compute"),
                layout: Some(&boid_compute_pipeline_layout),
                module: &boid_compute_shader,
                entry_point: Some("cs_main"),
                compilation_options: Default::default(),
                cache: None,
            });
        let boid_render_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("boid-render"),
                entries: &[
                    storage_ro(0, wgpu::ShaderStages::VERTEX),
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    // The trail, readable from the fragment stage: a field
                    // simulation is *drawn* by colouring the field, not by
                    // drawing its agents.
                    storage_ro(2, wgpu::ShaderStages::FRAGMENT),
                    // Desktop audio (AudioFx rows), both stages: size a
                    // particle by the beat in the vertex, colour it by its
                    // band in the fragment.
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::VERTEX
                            | wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        let boid_render_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("boid-render"),
            source: wgpu::ShaderSource::Wgsl(
                format!("{PARTICLE_RENDER_PREAMBLE}\n{BOIDS_RENDER_WGSL}").into(),
            ),
        });
        let boid_render_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("boid-render"),
                bind_group_layouts: &[&viewport_bind_group_layout, &boid_render_layout],
                push_constant_ranges: &[],
            });
        let boid_render_pipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("boid-render"),
                layout: Some(&boid_render_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &boid_render_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &boid_render_shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview: None,
                cache: None,
            });

        // The audio uniform starts as silence and stays silent unless a
        // compositor writes to it — a client renderer never hears anything,
        // which is exactly right.
        let audio_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fx-audio"),
            size: 160, // 10 vec4<f32> rows — see AudioFx::as_rows.
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // `@param` blocks start (and, for an undeclared shader, stay) zero.
        let params_buf = |label: &str| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: 128, // 8 vec4<f32> rows — see fx_params in the preamble.
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let bg_params_buf = params_buf("bg-params");
        let fx_params_buf = params_buf("fx-params-block");
        let window_params_buf = params_buf("window-fx-params");

        Renderer {
            device,
            queue,
            quad_pipeline,
            line_pipeline,
            fill_pipeline,
            fill_bind_group_layout,
            image_pipeline,
            glyph_pipeline,
            viewport_bind_group_layout,
            mask_bind_group_layout,
            mask_stride,
            no_mask_bind_group,
            image_bind_group_layout,
            image_sampler,
            glyph_bind_group,
            text_engine: Arc::new(TextEngine::new()),
            atlas: Mutex::new(glyph_atlas),
            format,
            adapter_name,
            backdrop_pipeline,
            kawase_bind_group_layout,
            kawase_down_pipeline,
            kawase_up_pipeline,
            fx_bind_group_layout,
            bg_params_buf,
            fx_params_buf,
            window_params_buf,
            audio_buf,
            identity_pipeline,
            effect: Mutex::new(None),
            window_fx: Mutex::new(None),
            model: Mutex::new(None),
            background: Mutex::new(None),
            fx_cache: Mutex::new(None),
            boid_compute_pipeline,
            boid_render_pipeline,
            boid_compute_layout,
            boid_render_layout,
            boids: Mutex::new(None),
            particle_compute: Mutex::new(None),
            particle_diffuse: Mutex::new(None),
            particle_render: Mutex::new(None),
        }
    }

    /// Spawn (or clear, with 0) the boid flock: `count` agents seeded
    /// deterministically, stepped on the GPU by [`Renderer::step_boids`],
    /// drawn by [`SceneLayer::Boids`] layers.
    /// Install (or clear, with `None`) the particle shaders a wallpaper
    /// drives — the update pass and the draw pass.
    ///
    /// `None` on either side restores the built-in flock for that half, so a
    /// wallpaper can replace the behaviour and keep the drawing, or the
    /// reverse. Both are compiled against the published preambles
    /// ([`PARTICLE_COMPUTE_PREAMBLE`], [`PARTICLE_RENDER_PREAMBLE`]) and
    /// validated with naga before touching the device: a bad shader returns
    /// `Err` and leaves whatever was installed alone, so a hot-reload of a
    /// half-written wallpaper never takes the desktop down.
    ///
    /// The particle *count* is [`Renderer::set_boids`]'s business; this is
    /// only what runs over them.
    pub fn set_particle_shaders(
        &self,
        compute: Option<&str>,
        render: Option<&str>,
    ) -> Result<(), String> {
        self.set_particle_shaders_with(compute, render, None)
    }

    /// As [`Renderer::set_particle_shaders`], plus the optional field pass —
    /// blur and decay over the trail, dispatched once per pixel rather than
    /// once per agent.
    pub fn set_particle_shaders_with(
        &self,
        compute: Option<&str>,
        render: Option<&str>,
        diffuse: Option<&str>,
    ) -> Result<(), String> {
        let next_diffuse = match diffuse {
            None => None,
            Some(src) => Some(self.compile_particle_compute(src)?),
        };
        *self.particle_diffuse.lock().unwrap() = next_diffuse;
        // Compile both before installing either: a wallpaper whose update
        // pass took and whose draw pass did not would render the old flock's
        // shape over new state, which looks like a bug in neither shader.
        let next_compute = match compute {
            None => None,
            Some(src) => Some(self.compile_particle_compute(src)?),
        };
        let next_render = match render {
            None => None,
            Some(src) => Some(self.compile_particle_render(src)?),
        };
        *self.particle_compute.lock().unwrap() = next_compute;
        *self.particle_render.lock().unwrap() = next_render;
        Ok(())
    }

    fn compile_particle_compute(&self, source: &str) -> Result<wgpu::ComputePipeline, String> {
        let full = format!("{PARTICLE_COMPUTE_PREAMBLE}\n{source}");
        Self::validate_wgsl(&full)?;
        if !full.contains("cs_main") {
            return Err("particle update shader must define @compute fn cs_main".into());
        }
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("particle-compute"),
            source: wgpu::ShaderSource::Wgsl(full.into()),
        });
        let layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("particle-compute"),
            bind_group_layouts: &[&self.boid_compute_layout],
            push_constant_ranges: &[],
        });
        let pipeline = self.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("particle-compute"),
            layout: Some(&layout),
            module: &module,
            entry_point: Some("cs_main"),
            compilation_options: Default::default(),
            cache: None,
        });
        match pollster::block_on(self.device.pop_error_scope()) {
            Some(e) => Err(e.to_string()),
            None => Ok(pipeline),
        }
    }

    fn compile_particle_render(&self, source: &str) -> Result<wgpu::RenderPipeline, String> {
        let full = format!("{PARTICLE_RENDER_PREAMBLE}\n{source}");
        Self::validate_wgsl(&full)?;
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("particle-render"),
            source: wgpu::ShaderSource::Wgsl(full.into()),
        });
        let layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("particle-render"),
            bind_group_layouts: &[
                &self.viewport_bind_group_layout,
                &self.boid_render_layout,
            ],
            push_constant_ranges: &[],
        });
        let pipeline = self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("particle-render"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: self.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        match pollster::block_on(self.device.pop_error_scope()) {
            Some(e) => Err(e.to_string()),
            None => Ok(pipeline),
        }
    }

    /// naga-validate a whole module, before the device ever sees it.
    fn validate_wgsl(source: &str) -> Result<(), String> {
        let module = naga::front::wgsl::parse_str(source)
            .map_err(|e| e.emit_to_string(source))?;
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        );
        validator.validate(&module).map_err(|e| format!("{e:?}")).map(|_| ())
    }

    /// Install `count` particles, scattered across a `size`-pixel output.
    ///
    /// The size matters: particle positions live in output pixels, so a
    /// scatter across a guessed extent leaves a 3440-wide desktop with all
    /// its particles crowded into one corner. Passing the real size is the
    /// difference between a field and a clump.
    pub fn set_boids(&self, count: u32, size: [f32; 2]) {
        if count == 0 {
            *self.boids.lock().unwrap() = None;
            return;
        }
        // 8192 was a flock's worth. A *field* simulation is a different
        // animal: Physarum needs agents in the hundreds of thousands before
        // the structure it builds is visible at all, because the picture is
        // made of their accumulated trails rather than of the agents. The
        // cost is 32 bytes each, twice (ping-pong) — a million agents is
        // 64 MiB of buffer, which the theme is choosing knowingly.
        let count = count.min(MAX_PARTICLES);
        // Deterministic LCG seed: scattered positions, gentle random headings.
        let mut seed = 0x9e3779b9u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / 16_777_216.0
        };
        // A zero-sized output (asked before the first frame) would put every
        // particle on top of every other; fall back to something ordinary
        // and let the next resize sort it out.
        let (w, h) = match (size[0] > 1.0, size[1] > 1.0) {
            (true, true) => (size[0], size[1]),
            _ => (1600.0, 1000.0),
        };
        let mut init: Vec<f32> = Vec::with_capacity(count as usize * 8);
        for _ in 0..count {
            // pos.xyz + pad: scattered across the screen and the depth band.
            init.push(next() * w);
            init.push(next() * h);
            init.push(0.1 + next() * 0.8);
            init.push(0.0);
            // vel.xyz + pad.
            init.push((next() - 0.5) * 200.0);
            init.push((next() - 0.5) * 200.0);
            init.push(0.0);
            init.push(0.0);
        }
        let mk_buf = |label: &str| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (count as u64) * 32,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_DST
                    // Readback, so a test can assert on the simulation
                    // rather than on pixels.
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        let buf_a = mk_buf("boids-a");
        let buf_b = mk_buf("boids-b");
        self.queue.write_buffer(&buf_a, 0, bytemuck::cast_slice(&init));
        self.queue.write_buffer(&buf_b, 0, bytemuck::cast_slice(&init));
        let params_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("boid-params"),
            size: std::mem::size_of::<BoidParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let obstacles_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("boid-obstacles"),
            size: (MAX_BOID_OBSTACLES * 16) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let window_vel_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("particle-window-vel"),
            size: (MAX_BOID_OBSTACLES * 16) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // The bind groups name the trail, which does not exist until a size
        // is known, so they are built by `compute_bind_for`/`render_binds_for`
        // and rebuilt on the first step and on every resize. Constructing
        // them here as well would be a second place for the layout to drift.
        let state = BoidsState {
            count,
            compute_binds: Vec::new(),
            render_binds: Vec::new(),
            params_buf,
            obstacles_buf,
            window_vel_buf,
            bufs: [buf_a, buf_b],
            trail: None,
            trail_size: (0, 0),
            current: 0,
        };
        *self.boids.lock().unwrap() = Some(state);
    }

    /// Whether a flock is live (the compositor uses this to keep frames
    /// coming — a flock is inherently animated).
    /// The particle state as raw floats: eight per particle, `pos.xyzw`
    /// then `vel.xyzw`. Empty when no particles are installed.
    ///
    /// For tests. A particle simulation is the one thing here that cannot be
    /// checked by looking at the output — a draw shader that jitters would
    /// pass a pixel comparison while the simulation sat still.
    pub fn read_particles(&self) -> Vec<f32> {
        let guard = self.boids.lock().unwrap();
        let Some(state) = guard.as_ref() else { return Vec::new() };
        let size = (state.count as u64) * 32;
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("particle-readback"),
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("particle-readback") },
        );
        encoder.copy_buffer_to_buffer(&state.bufs[state.current], 0, &staging, 0, size);
        self.queue.submit([encoder.finish()]);

        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        loop {
            let _ = self.device.poll(wgpu::PollType::Wait);
            match rx.try_recv() {
                Ok(result) => {
                    result.expect("particle readback map failed");
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    panic!("map channel closed")
                }
            }
        }
        let out = bytemuck::cast_slice::<u8, f32>(&slice.get_mapped_range()).to_vec();
        staging.unmap();
        out
    }

    pub fn boids_active(&self) -> bool {
        self.boids.lock().unwrap().is_some()
    }

    /// Step the flock once on the GPU: `obstacles` are screen-space rects
    /// (window geometry) the boids steer around; `cursor` attracts from a
    /// distance and repels up close.
    #[allow(clippy::too_many_arguments)]
    /// The compute bind group for ping-pong slot `i`: particles src→dst and
    /// trail src→dst swapped together, so one `current` index drives both.
    fn compute_bind_for(&self, state: &BoidsState, i: usize) -> wgpu::BindGroup {
        let (src, dst) = (&state.bufs[i], &state.bufs[1 - i]);
        // Before a size is known there is no trail; bind the particle buffer
        // twice so the layout is satisfied and a shader that ignores the
        // trail still runs.
        let fallback = [&state.bufs[0], &state.bufs[1]];
        let trail = state.trail.as_ref().map(|t| [&t[i], &t[1 - i]]).unwrap_or(fallback);
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("particle-compute"),
            layout: &self.boid_compute_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: src.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: dst.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: state.params_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: state.obstacles_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: state.window_vel_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 5, resource: trail[0].as_entire_binding() },
                wgpu::BindGroupEntry { binding: 6, resource: trail[1].as_entire_binding() },
                wgpu::BindGroupEntry { binding: 7, resource: self.audio_buf.as_entire_binding() },
            ],
        })
    }

    /// `[current][layer]` render bind groups, reading the particle buffer and
    /// the trail that the diffuse pass most recently wrote.
    fn render_binds_for(&self, state: &BoidsState) -> Vec<Vec<wgpu::BindGroup>> {
        let layer = |v: u32| {
            self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("particle-layer"),
                contents: bytemuck::bytes_of(&v),
                usage: wgpu::BufferUsages::UNIFORM,
            })
        };
        let layers = [layer(0), layer(1)];
        let make = |i: usize, l: usize| {
            let trail = state
                .trail
                .as_ref()
                .map(|t| &t[1 - i])
                .unwrap_or(&state.bufs[0]);
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("particle-render"),
                layout: &self.boid_render_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: state.bufs[i].as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: layers[l].as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: trail.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: self.audio_buf.as_entire_binding() },
                ],
            })
        };
        vec![vec![make(0, 0), make(0, 1)], vec![make(1, 0), make(1, 1)]]
    }

    // Eight arguments, and each is a distinct simulation input the caller
    // genuinely has to supply per frame. Bundling them into a struct would
    // move the same eight fields one level out and cost a construction at
    // every call site, which is renaming the problem rather than solving it.
    #[allow(clippy::too_many_arguments)]
    pub fn step_boids(
        &self,
        dt: f32,
        time: f32,
        obstacles: &[[f32; 4]],
        window_vel: &[[f32; 4]],
        cursor: [f32; 2],
        w: u32,
        h: u32,
    ) {
        let mut guard = self.boids.lock().unwrap();
        let Some(state) = guard.as_mut() else { return };
        let n = obstacles.len().min(MAX_BOID_OBSTACLES);
        let mut obs = [[0.0f32; 4]; MAX_BOID_OBSTACLES];
        obs[..n].copy_from_slice(&obstacles[..n]);
        self.queue.write_buffer(&state.obstacles_buf, 0, bytemuck::cast_slice(&obs));
        // Velocity is parallel to the rects; a short list leaves the tail at
        // zero, which reads as "not moving" rather than as stale motion.
        let mut vel = [[0.0f32; 4]; MAX_BOID_OBSTACLES];
        let vn = window_vel.len().min(n);
        vel[..vn].copy_from_slice(&window_vel[..vn]);
        self.queue.write_buffer(&state.window_vel_buf, 0, bytemuck::cast_slice(&vel));
        self.queue.write_buffer(
            &state.params_buf,
            0,
            bytemuck::bytes_of(&BoidParams {
                count: state.count,
                nwin: n as u32,
                dt: dt.clamp(0.001, 0.05),
                time,
                size: [w as f32, h as f32],
                cursor,
            }),
        );
        // The trail field is sized to the output, so a resize reallocates it
        // and the bind groups that name it. Cleared on allocation: stale
        // deposits at the old resolution would smear across the new one.
        if state.trail_size != (w, h) && w > 0 && h > 0 {
            let bytes = (w as u64) * (h as u64) * 4;
            let mk = |label: &str| {
                self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size: bytes.max(4),
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            };
            let (ta, tb) = (mk("particle-trail-a"), mk("particle-trail-b"));
            let zeros = vec![0u8; bytes.min(1 << 22) as usize];
            for buf in [&ta, &tb] {
                let mut at = 0u64;
                while at < bytes {
                    let take = (bytes - at).min(zeros.len() as u64) as usize;
                    self.queue.write_buffer(buf, at, &zeros[..take]);
                    at += take as u64;
                }
            }
            state.trail = Some([ta, tb]);
            state.trail_size = (w, h);
            state.compute_binds =
                vec![self.compute_bind_for(state, 0), self.compute_bind_for(state, 1)];
            state.render_binds = self.render_binds_for(state);
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("boids") });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("boids"),
                timestamp_writes: None,
            });
            let installed = self.particle_compute.lock().unwrap();
            pass.set_pipeline(installed.as_ref().unwrap_or(&self.boid_compute_pipeline));
            if state.compute_binds.is_empty() {
                return;
            }
            pass.set_bind_group(0, &state.compute_binds[state.current], &[]);
            pass.dispatch_workgroups(state.count.div_ceil(64), 1, 1);

            // The field pass, if one is installed: agents deposited into the
            // trail above, and this is what blurs and decays it. Dispatched
            // over *pixels* rather than agents — a different shape of work
            // over the same bindings, which is why it is a second pipeline
            // rather than more code in the first.
            if let Some(diffuse) = self.particle_diffuse.lock().unwrap().as_ref() {
                pass.set_pipeline(diffuse);
                pass.set_bind_group(0, &state.compute_binds[state.current], &[]);
                pass.dispatch_workgroups(w.div_ceil(16), h.div_ceil(16), 1);
            }
        }
        self.queue.submit([encoder.finish()]);
        state.current = 1 - state.current;
    }

    /// Compile a whole-output effect pipeline from a complete WGSL module
    /// (preamble + fragment stage).
    fn build_effect_pipeline(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        layout: &wgpu::BindGroupLayout,
        source: &str,
        label: &str,
        blend: Option<wgpu::BlendState>,
    ) -> wgpu::RenderPipeline {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(label),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(label),
            bind_group_layouts: &[layout],
            push_constant_ranges: &[],
        });
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        })
    }

    /// Install (or clear, with `None`) the whole-output effect shader. The
    /// source is the *fragment stage only* — the renderer supplies the
    /// preamble ([`EFFECT_PREAMBLE`]): scene texture/sampler, `fx` uniforms
    /// (resolution, cursor), a `time` uniform, and the fullscreen vertex
    /// stage. The module must define `@fragment fn fs_main(in: FxIn)`.
    ///
    /// Validated with naga before touching the device: a bad shader returns
    /// `Err(message)` and leaves the previous effect installed — a broken
    /// hot-reload never takes down the desktop. Returns `Ok(animated)`,
    /// where `animated` is whether `fs_main` actually reads `time` (naga
    /// `GlobalUse`) — static effects keep damage-gated idle rendering.
    pub fn set_effect(&self, source: Option<&str>) -> Result<bool, String> {
        let Some(fragment) = source else {
            *self.effect.lock().unwrap() = None;
            return Ok(false);
        };
        let state = self.compile_effect(fragment)?;
        let animated = state.animated;
        *self.effect.lock().unwrap() = Some(state);
        Ok(animated)
    }

    /// Install (or clear) the background shader — the generative fullscreen
    /// pass a [`SceneLayer::Shader`] layer paints (a shader wallpaper). Same
    /// contract and validation as [`Renderer::set_effect`], except the
    /// preamble's `scene` texture is undefined here.
    pub fn set_background(&self, source: Option<&str>) -> Result<bool, String> {
        let Some(fragment) = source else {
            *self.background.lock().unwrap() = None;
            return Ok(false);
        };
        let state = self.compile_effect(fragment)?;
        let animated = state.animated;
        *self.background.lock().unwrap() = Some(state);
        Ok(animated)
    }

    /// Install (or clear) the **per-window** effect — what a
    /// [`SceneLayer::WindowFx`] layer paints.
    ///
    /// Same contract as [`Renderer::set_effect`], with two differences that
    /// are the whole point:
    ///
    /// * It is drawn **per window**, immediately above that window in the
    ///   scene, with `fx.window` set to that window's index. So it is
    ///   occluded by whatever is stacked higher for real, and a glass window
    ///   in front of it blurs it — because the backdrop samples the scene as
    ///   accumulated *at that point*, and the effect is already in it.
    /// * It is **alpha-blended** into the scene rather than replacing it, so
    ///   it must output its own coverage. The `scene` texture is not its
    ///   input: a shader that samples it here would be reading the target it
    ///   is drawing into. Generate, return alpha, let the compositor stack it.
    pub fn set_window_fx(&self, source: Option<&str>) -> Result<bool, String> {
        let Some(fragment) = source else {
            *self.window_fx.lock().unwrap() = None;
            return Ok(false);
        };
        // Premultiplied alpha, which buys both behaviours from one blend:
        // a shader returning `vec4(colour * a, a)` composites normally, and
        // one returning `vec4(light, 0.0)` is purely additive, because the
        // destination factor is `1 - 0`. Fire is light — it brightens what
        // it is in front of and must never darken it — so it takes the
        // second form. A translucent panel would take the first.
        let state = self.compile_effect_blended(
            fragment,
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        )?;
        let animated = state.animated;
        *self.window_fx.lock().unwrap() = Some(state);
        Ok(animated)
    }

    /// Whether a per-window effect is installed at all — what tells the
    /// compositor whether to emit [`SceneLayer::WindowFx`] layers.
    pub fn has_window_fx(&self) -> bool {
        self.window_fx.lock().unwrap().is_some()
    }

    /// Whether the installed per-window effect reads `time`.
    pub fn window_fx_animated(&self) -> bool {
        self.window_fx.lock().unwrap().as_ref().is_some_and(|e| e.animated)
    }

    /// Install (or clear) the model layer: a mesh plus its cinematic WGSL.
    /// The shader supplies vs_main/fs_main against the model vertex contract
    /// (position, normal, uv, material_id) and a Frame uniform (resolution,
    /// time, exposure) at group(0) binding(0).
    ///
    /// The mesh is drawn with **two instances**: `instance_index == 0` is the
    /// object; `== 1` is the mirror slot, for a planar floor reflection.
    /// Shaders that want no reflection collapse instance 1 to a degenerate
    /// position — clipped, so it costs nothing.
    pub fn set_model(
        &self,
        source: Option<&str>,
        mesh: Option<&mesh::ModelMesh>,
    ) -> Result<(), String> {
        let (Some(source), Some(mesh)) = (source, mesh) else {
            *self.model.lock().unwrap() = None;
            return Ok(());
        };
        let module = naga::front::wgsl::parse_str(source)
            .map_err(|e| e.emit_to_string(source))?;
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        );
        validator.validate(&module).map_err(|e| format!("{e:?}"))?;

        let frame_bind_group_layout =
            self.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("model-frame"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let shader = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("model"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("model"),
            bind_group_layouts: &[&frame_bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("model"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<mesh::ModelVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x3, 1 => Float32x3, 2 => Float32x2, 3 => Uint32
                    ],
                }],
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: None, // the wild ships mixed winding; depth sorts it
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth24Plus,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: self.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        if let Some(e) = pollster::block_on(self.device.pop_error_scope()) {
            return Err(e.to_string());
        }
        let bytes = std::mem::size_of_val(&mesh.vertices[..]) as u64;
        let cap = self.device.limits().max_buffer_size;
        if bytes > cap {
            return Err(format!(
                "mesh is {:.0} MB of vertices, over this GPU's {:.0} MB buffer limit — \
                 decimate it, or split it into parts in a directory",
                bytes as f64 / 1e6,
                cap as f64 / 1e6,
            ));
        }
        let vertex_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("model-vertices"),
            contents: bytemuck::cast_slice(&mesh.vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        *self.model.lock().unwrap() = Some(ModelState {
            pipeline,
            vertex_buf,
            vertex_count: mesh.vertices.len() as u32,
            frame_bind_group_layout,
            bounds_min: mesh.min,
            bounds_max: mesh.max,
        });
        Ok(())
    }

    /// Whether a model layer is installed (it animates by contract — the
    /// shader owns time-driven motion).
    pub fn model_active(&self) -> bool {
        self.model.lock().unwrap().is_some()
    }

    /// Whether the installed background shader reads `time`.
    pub fn background_animated(&self) -> bool {
        self.background.lock().unwrap().as_ref().is_some_and(|e| e.animated)
    }

    /// Validate + compile one user fragment stage against the fx preamble.
    fn compile_effect(&self, fragment: &str) -> Result<EffectState, String> {
        self.compile_effect_blended(fragment, None)
    }

    /// [`Renderer::compile_effect`], with an explicit blend mode.
    ///
    /// A whole-output pass overwrites its target and wants `None`. A
    /// per-window layer is drawn *into* the accumulating scene and must
    /// therefore blend: it contributes a flame, a glow, a wake, over
    /// whatever its window is sitting on, and leaves the rest of the frame
    /// untouched at zero alpha.
    fn compile_effect_blended(
        &self,
        fragment: &str,
        blend: Option<wgpu::BlendState>,
    ) -> Result<EffectState, String> {
        let full = format!("{EFFECT_PREAMBLE}\n{fragment}");
        let module = naga::front::wgsl::parse_str(&full)
            .map_err(|e| e.emit_to_string(&full))?;
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        );
        let info = validator.validate(&module).map_err(|e| format!("{e:?}"))?;
        let ep_index = module
            .entry_points
            .iter()
            .position(|ep| ep.stage == naga::ShaderStage::Fragment && ep.name == "fs_main")
            .ok_or("effect must define @fragment fn fs_main")?;
        let animated = module
            .global_variables
            .iter()
            .find(|(_, gv)| gv.name.as_deref() == Some("time"))
            .map(|(handle, _)| !info.get_entry_point(ep_index)[handle].is_empty())
            .unwrap_or(false);

        // naga passed; catch anything device-level behind an error scope so a
        // failure surfaces as Err, never a panic/log-only validation error.
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let pipeline = Self::build_effect_pipeline(
            &self.device,
            self.format,
            &self.fx_bind_group_layout,
            &full,
            "effect",
            blend,
        );
        if let Some(e) = pollster::block_on(self.device.pop_error_scope()) {
            return Err(e.to_string());
        }
        Ok(EffectState { pipeline, animated })
    }

    /// Upload the background shader's declared parameter values (8 vec4
    /// rows, lane-packed in declaration order — `rill_appkit::params::pack`).
    pub fn set_background_params(&self, rows: [[f32; 4]; 8]) {
        self.queue.write_buffer(&self.bg_params_buf, 0, bytemuck::cast_slice(&rows));
    }

    /// Upload the whole-output effect shader's parameter values.
    pub fn set_effect_params(&self, rows: [[f32; 4]; 8]) {
        self.queue.write_buffer(&self.fx_params_buf, 0, bytemuck::cast_slice(&rows));
    }

    /// Upload the per-window effect shader's parameter values.
    pub fn set_window_fx_params(&self, rows: [[f32; 4]; 8]) {
        self.queue.write_buffer(&self.window_params_buf, 0, bytemuck::cast_slice(&rows));
    }

    /// Whether the installed effect reads `time` (needs continuous redraw).
    pub fn effect_animated(&self) -> bool {
        self.effect.lock().unwrap().as_ref().is_some_and(|e| e.animated)
    }

    /// The shared text engine — use it for measurement (`EngineMeasurer`) so
    /// layout and painting agree on the same shaping cache.
    pub fn text_engine(&self) -> &Arc<TextEngine> {
        &self.text_engine
    }

    /// The adapter chosen at bring-up (for logging/tests).
    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// Prepare one command list for painting: walk it in paint order into
    /// GPU-ready buffers plus an ordered item list. Paint the result into any
    /// open pass with [`Renderer::paint_frame`] — possibly stacked with other
    /// frames and window layers (see [`Renderer::composite`]).
    ///
    /// Covers the full vocabulary: `Rect`, `Shadow`, `Image`, `Text`,
    /// `PushClip`/`PopClip`; hit-region commands are never painted.
    pub fn build_frame(
        &self,
        commands: &[DrawCommand],
        images: &dyn ImageSource,
        width: u32,
        height: u32,
    ) -> FrameData {
        // --- Walk the command list in paint order --------------------------
        // Accumulate quads into one buffer; cut a new quad span whenever the
        // clip scissor changes or an image interleaves, so paint order and
        // clipping are both preserved.
        let mut quads: Vec<QuadInstance> = Vec::new();
        let mut line_instances_buf: Vec<LineInstance> = Vec::new();
        let mut fill_instances_buf: Vec<FillInstance> = Vec::new();
        let mut fill_segments: Vec<[f32; 4]> = Vec::new();
        let mut image_instances: Vec<ImageInstance> = Vec::new();
        let mut image_bind_groups: Vec<wgpu::BindGroup> = Vec::new();
        let mut glyph_instances: Vec<GlyphInstance> = Vec::new();
        let mut items: Vec<DrawItem> = Vec::new();
        let mut clip: Vec<[f32; 4]> = Vec::new();
        // Parallel to `clip`: each entry's corner radius (0 = square). The
        // innermost rounded entry is the active mask; masks change only at
        // clip boundaries, which already cut spans, so a span's mask is
        // constant by construction.
        let mut clip_radius: Vec<f32> = Vec::new();
        // Mask table for this frame; entry 0 is "no mask". Fragment params:
        // center.xy, half.xy, radius, on, pad, pad.
        let mut masks: Vec<[f32; 8]> = vec![[0.0; 8]];
        let mut cur_mask = 0u32;
        let mut scissor = compute_scissor(&clip, width, height);
        let mut span_start = 0u32;

        let cut = |quads: &Vec<QuadInstance>,
                   items: &mut Vec<DrawItem>,
                   span_start: &mut u32,
                   scissor: Option<[u32; 4]>,
                   mask: u32| {
            let end = quads.len() as u32;
            if end > *span_start {
                items.push(DrawItem::Quads(Span { scissor, start: *span_start, end, mask }));
                *span_start = end;
            }
        };
        let active_mask = |clip: &[[f32; 4]], radii: &[f32], masks: &mut Vec<[f32; 8]>| -> u32 {
            let Some(i) = (0..radii.len()).rev().find(|&i| radii[i] > 0.0) else { return 0 };
            let [x0, y0, x1, y1] = clip[i];
            let params = [
                (x0 + x1) * 0.5,
                (y0 + y1) * 0.5,
                (x1 - x0) * 0.5,
                (y1 - y0) * 0.5,
                radii[i],
                1.0,
                0.0,
                0.0,
            ];
            match masks.iter().position(|m| *m == params) {
                Some(idx) => idx as u32,
                None => {
                    masks.push(params);
                    (masks.len() - 1) as u32
                }
            }
        };

        for command in commands {
            match command {
                DrawCommand::Rect { rect, color, corner_radius } => {
                    quads.push(quad_from_rect(*rect, *color, *corner_radius));
                }
                DrawCommand::Shadow { rect, color, blur, spread, corner_radius } => {
                    quads.push(quad_from_shadow(*rect, *color, *blur, *spread, *corner_radius));
                }
                DrawCommand::Glow { rect, color, blur, corner_radius } => {
                    quads.push(quad_from_glow(*rect, *color, *blur, *corner_radius));
                }
                DrawCommand::Border { rect, color, width, corner_radius } => {
                    quads.push(quad_from_border(*rect, *color, *width, *corner_radius));
                }
                DrawCommand::Image { rect, source } => {
                    // A host that already owns the texture binds it; only one
                    // that hands over raw pixels pays for an upload, and it
                    // pays per frame. The compositor takes the first path:
                    // pixels arrive once over `attach_image` and are uploaded
                    // on arrival, so a frame naming an image costs a bind
                    // group rather than a texture creation and a full copy.
                    let bind = match images.texture(source) {
                        Some(view) => Some(self.bind_image(view)),
                        // Validate the payload; a lying source paints the
                        // placeholder rather than a garbled texture.
                        None => images
                            .rgba(source)
                            .filter(|d| {
                                d.width > 0
                                    && d.height > 0
                                    && d.pixels.len() == (d.width * d.height * 4) as usize
                            })
                            .map(|d| self.upload_image(&d)),
                    };
                    match bind {
                        Some(bind) => {
                            cut(&quads, &mut items, &mut span_start, scissor, cur_mask);
                            image_bind_groups.push(bind);
                            image_instances.push(ImageInstance {
                                pos: [rect.x, rect.y],
                                size: [rect.w, rect.h],
                                alpha: 1.0,
                            });
                            items.push(DrawItem::Image {
                                scissor,
                                index: (image_instances.len() - 1) as u32,
                                mask: cur_mask,
                            });
                        }
                        None => {
                            // Not attached yet, or refused: placeholder box —
                            // plain quads, so no span cut needed.
                            quads.push(quad_from_rect(*rect, PLACEHOLDER_OUTER, 0.0));
                            quads.push(quad_from_rect(rect.inset(4.0), PLACEHOLDER_INNER, 0.0));
                        }
                    }
                }
                DrawCommand::Path { points, color, width: stroke, closed } => {
                    // A different pipeline, so the open quad span has to close
                    // first or the paths would jump ahead of rects drawn
                    // before them.
                    let start = line_instances_buf.len() as u32;
                    line_instances_buf.extend(line_instances(points, *color, *stroke, *closed));
                    let end = line_instances_buf.len() as u32;
                    if end > start {
                        cut(&quads, &mut items, &mut span_start, scissor, cur_mask);
                        items.push(DrawItem::Lines(Span { scissor, start, end, mask: cur_mask }));
                    }
                }
                DrawCommand::FillPath { points, contours, color } => {
                    let seg_start = fill_segments.len() as u32;
                    let (mut minx, mut miny) = (f32::MAX, f32::MAX);
                    let (mut maxx, mut maxy) = (f32::MIN, f32::MIN);
                    let mut base = 0usize;
                    for len in contours {
                        let n = *len as usize;
                        let ring = &points[base..base + n];
                        base += n;
                        for i in 0..n {
                            let a = ring[i];
                            let b = ring[(i + 1) % n];
                            fill_segments.push([a.x, a.y, b.x, b.y]);
                            minx = minx.min(a.x);
                            miny = miny.min(a.y);
                            maxx = maxx.max(a.x);
                            maxy = maxy.max(a.y);
                        }
                    }
                    let count = fill_segments.len() as u32 - seg_start;
                    if count > 0 {
                        let start = fill_instances_buf.len() as u32;
                        fill_instances_buf.push(FillInstance {
                            // One pixel of slack for the AA ramp.
                            bbox: [minx - 1.0, miny - 1.0, maxx - minx + 2.0, maxy - miny + 2.0],
                            color: color_to_linear(*color),
                            seg: [seg_start, count],
                            _pad: [0; 2],
                        });
                        cut(&quads, &mut items, &mut span_start, scissor, cur_mask);
                        items.push(DrawItem::Fills(Span { scissor, start, end: start + 1, mask: cur_mask }));
                    }
                }
                DrawCommand::Backdrop { rect, blur, corner_radius } => {
                    cut(&quads, &mut items, &mut span_start, scissor, cur_mask);
                    items.push(DrawItem::Backdrop {
                        scissor,
                        rect: [rect.x, rect.y, rect.w, rect.h],
                        blur: *blur,
                        corner_radius: *corner_radius,
                    });
                }
                DrawCommand::PushClip { rect, radius } => {
                    cut(&quads, &mut items, &mut span_start, scissor, cur_mask);
                    clip.push([rect.x, rect.y, rect.x + rect.w, rect.y + rect.h]);
                    clip_radius.push(*radius);
                    scissor = compute_scissor(&clip, width, height);
                    cur_mask = active_mask(&clip, &clip_radius, &mut masks);
                }
                DrawCommand::PopClip => {
                    cut(&quads, &mut items, &mut span_start, scissor, cur_mask);
                    clip.pop();
                    clip_radius.pop();
                    cur_mask = active_mask(&clip, &clip_radius, &mut masks);
                    scissor = compute_scissor(&clip, width, height);
                }
                DrawCommand::Text { rect, text, color, font_size, font_weight, font_family } => {
                    // Phase 2 (shared wrap) at the rect's width, then per-line
                    // glyph placement + atlas lookup. Line-break numbers are
                    // the measurer's by construction — same prepare, same wrap.
                    let prepared =
                        self.text_engine.prepare(text, *font_size, *font_weight, font_family);
                    let line_height = *font_size * LINE_HEIGHT_FACTOR;
                    let mut atlas = self.atlas.lock().unwrap();
                    let glyph_start = glyph_instances.len() as u32;
                    for (i, line) in wrap_segments(&prepared.segments, rect.w.max(1.0))
                        .iter()
                        .enumerate()
                    {
                        let slice = text[line.start..line.end].trim_end_matches('\n');
                        if slice.is_empty() {
                            continue;
                        }
                        let placed =
                            self.text_engine.place_line(slice, *font_size, *font_weight, font_family);
                        let line_top = rect.y + i as f32 * line_height;
                        // A variable family gets the weight it was actually
                        // asked for: the rasterizer moves the `wght` axis and
                        // produces that weight's real outlines. Nothing is
                        // faked, so no smear is needed.
                        let vary = if self.text_engine.has_variable_weight(font_family) {
                            *font_weight
                        } else {
                            0
                        };
                        // Only a family that ships static cuts can still fall
                        // short. Then the weight it cannot supply is drawn
                        // instead: the same glyph a second time, a fraction of
                        // an em to the right, by as much as the family falls
                        // short. It is what terminals have always done for
                        // fonts that ship one weight, and it keeps the advance
                        // — a synthetic weight that widened cells would tear a
                        // grid apart.
                        let deficit = if vary != 0 {
                            0
                        } else {
                            self.text_engine.weight_deficit(font_family, *font_weight)
                        };
                        // 300 units short (Regular against an ExtraLight cut)
                        // is a light smear; 500 short (Bold) is the full one.
                        let smear = if deficit == 0 {
                            0.0
                        } else {
                            let gap = (deficit as f32 / 500.0).clamp(0.25, 1.0);
                            (font_size * 0.05 * gap).max(0.35)
                        };
                        let faux_bold = smear > 0.0;
                        self.text_engine.with_font_system(|fs| {
                            for pg in &placed {
                                let Some(slot) = atlas.slot(fs, &self.queue, pg.key, vary) else {
                                    continue;
                                };
                                if faux_bold {
                                    glyph_instances.push(GlyphInstance {
                                        pos: [
                                            rect.x + pg.x as f32 + slot.left as f32 + smear,
                                            line_top + pg.y as f32 - slot.top as f32,
                                        ],
                                        size: [slot.w as f32, slot.h as f32],
                                        uv_pos: [
                                            slot.x as f32 / ATLAS_SIZE as f32,
                                            slot.y as f32 / ATLAS_SIZE as f32,
                                        ],
                                        uv_size: [
                                            slot.w as f32 / ATLAS_SIZE as f32,
                                            slot.h as f32 / ATLAS_SIZE as f32,
                                        ],
                                        color: color_to_linear(*color),
                                    });
                                }
                                glyph_instances.push(GlyphInstance {
                                    pos: [
                                        rect.x + pg.x as f32 + slot.left as f32,
                                        line_top + pg.y as f32 - slot.top as f32,
                                    ],
                                    size: [slot.w as f32, slot.h as f32],
                                    uv_pos: [
                                        slot.x as f32 / ATLAS_SIZE as f32,
                                        slot.y as f32 / ATLAS_SIZE as f32,
                                    ],
                                    uv_size: [
                                        slot.w as f32 / ATLAS_SIZE as f32,
                                        slot.h as f32 / ATLAS_SIZE as f32,
                                    ],
                                    color: color_to_linear(*color),
                                });
                            }
                        });
                    }
                    let glyph_end = glyph_instances.len() as u32;
                    if glyph_end > glyph_start {
                        cut(&quads, &mut items, &mut span_start, scissor, cur_mask);
                        items.push(DrawItem::Glyphs(Span {
                            scissor,
                            start: glyph_start,
                            end: glyph_end,
                            mask: cur_mask,
                        }));
                    }
                }
                // Never painted (hit regions, key bindings, menus).
                DrawCommand::LinkArea { .. }
                | DrawCommand::ActionArea { .. }
                | DrawCommand::InputArea { .. }
                | DrawCommand::SliderArea { .. }
                | DrawCommand::KeyBind { .. }
                | DrawCommand::KeyCapture { .. }
                | DrawCommand::ScrollArea { .. }
                | DrawCommand::LiveRefresh { .. }
                | DrawCommand::MenuArea { .. } => {}
            }
        }
        cut(&quads, &mut items, &mut span_start, scissor, cur_mask);

        // --- GPU resources -------------------------------------------------
        let viewport = [width as f32, height as f32];
        let viewport_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("viewport"),
            contents: bytemuck::cast_slice(&viewport),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let viewport_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewport"),
            layout: &self.viewport_bind_group_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: viewport_buf.as_entire_binding() }],
        });

        // Non-empty buffers keep the vertex-buffer bindings valid even when
        // nothing draws (all clipped / no images): one zeroed instance, never
        // referenced.
        let quad_upload = if quads.is_empty() { vec![QuadInstance::zeroed()] } else { quads };
        let quad_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad-instances"),
            contents: bytemuck::cast_slice(&quad_upload),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let line_upload = if line_instances_buf.is_empty() {
            vec![LineInstance::zeroed()]
        } else {
            line_instances_buf
        };
        let line_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("line-instances"),
            contents: bytemuck::cast_slice(&line_upload),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let fill_upload = if fill_instances_buf.is_empty() {
            vec![FillInstance::zeroed()]
        } else {
            fill_instances_buf
        };
        let fill_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fill-instances"),
            contents: bytemuck::cast_slice(&fill_upload),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let seg_upload: Vec<[f32; 4]> =
            if fill_segments.is_empty() { vec![[0.0; 4]] } else { fill_segments };
        let fill_seg_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fill-segments"),
            contents: bytemuck::cast_slice(&seg_upload),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let fill_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fill-segments"),
            layout: &self.fill_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: fill_seg_buf.as_entire_binding(),
            }],
        });
        let image_upload = if image_instances.is_empty() {
            vec![ImageInstance::zeroed()]
        } else {
            image_instances
        };
        let image_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("image-instances"),
            contents: bytemuck::cast_slice(&image_upload),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let glyph_upload = if glyph_instances.is_empty() {
            vec![GlyphInstance::zeroed()]
        } else {
            glyph_instances
        };
        let glyph_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glyph-instances"),
            contents: bytemuck::cast_slice(&glyph_upload),
            usage: wgpu::BufferUsages::VERTEX,
        });

        // The frame's mask table: one aligned slot per distinct rounded clip,
        // selected per span by dynamic offset. Entry 0 is "no mask".
        let stride = self.mask_stride as usize;
        let mut mask_bytes = vec![0u8; masks.len() * stride];
        for (i, m) in masks.iter().enumerate() {
            mask_bytes[i * stride..i * stride + 32].copy_from_slice(bytemuck::cast_slice(m));
        }
        let mask_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("clip-masks"),
            contents: &mask_bytes,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let mask_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("clip-masks"),
            layout: &self.mask_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &mask_buf,
                    offset: 0,
                    size: wgpu::BufferSize::new(32),
                }),
            }],
        });

        FrameData {
            items,
            mask_bind_group,
            quad_buf,
            line_buf,
            fill_buf,
            fill_bind_group,
            image_buf,
            glyph_buf,
            image_bind_groups,
            viewport_bind_group,
        }
    }

    /// Paint a built frame into an open render pass. Scissor state is set per
    /// span, so frames can be stacked in one pass without leaking clip state
    /// into each other. `Backdrop` items are skipped — only the fx composite
    /// path has an accumulation texture to sample.
    pub fn paint_frame(&self, pass: &mut wgpu::RenderPass<'_>, frame: &FrameData) {
        self.paint_items(pass, frame, 0..frame.items.len());
    }

    /// Paint `range` of a built frame's items into an open pass.
    fn paint_items(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        frame: &FrameData,
        range: std::ops::Range<usize>,
    ) {
        // Bind group 0 (the viewport uniform) is layout-compatible across all
        // pipelines, so it survives pipeline switches.
        pass.set_bind_group(0, &frame.viewport_bind_group, &[]);
        for item in &frame.items[range] {
            match item {
                DrawItem::Backdrop { .. } => {}
                DrawItem::Quads(span) => {
                    let Some([x, y, w, h]) = span.scissor else { continue };
                    pass.set_scissor_rect(x, y, w, h);
                    pass.set_pipeline(&self.quad_pipeline);
                    pass.set_bind_group(1, &frame.mask_bind_group, &[span.mask * self.mask_stride]);
                    pass.set_vertex_buffer(0, frame.quad_buf.slice(..));
                    pass.draw(0..6, span.start..span.end);
                }
                DrawItem::Lines(span) => {
                    let Some([x, y, w, h]) = span.scissor else { continue };
                    pass.set_scissor_rect(x, y, w, h);
                    pass.set_pipeline(&self.line_pipeline);
                    pass.set_bind_group(1, &frame.mask_bind_group, &[span.mask * self.mask_stride]);
                    pass.set_vertex_buffer(0, frame.line_buf.slice(..));
                    pass.draw(0..6, span.start..span.end);
                }
                DrawItem::Fills(span) => {
                    let Some([x, y, w, h]) = span.scissor else { continue };
                    pass.set_scissor_rect(x, y, w, h);
                    pass.set_pipeline(&self.fill_pipeline);
                    pass.set_vertex_buffer(0, frame.fill_buf.slice(..));
                    pass.set_bind_group(1, &frame.fill_bind_group, &[]);
                    pass.set_bind_group(2, &frame.mask_bind_group, &[span.mask * self.mask_stride]);
                    pass.draw(0..6, span.start..span.end);
                }
                DrawItem::Image { scissor, index, mask } => {
                    let Some([x, y, w, h]) = *scissor else { continue };
                    pass.set_scissor_rect(x, y, w, h);
                    pass.set_pipeline(&self.image_pipeline);
                    pass.set_vertex_buffer(0, frame.image_buf.slice(..));
                    pass.set_bind_group(1, &frame.image_bind_groups[*index as usize], &[]);
                    pass.set_bind_group(2, &frame.mask_bind_group, &[*mask * self.mask_stride]);
                    pass.draw(0..6, *index..*index + 1);
                }
                DrawItem::Glyphs(span) => {
                    let Some([x, y, w, h]) = span.scissor else { continue };
                    pass.set_scissor_rect(x, y, w, h);
                    pass.set_pipeline(&self.glyph_pipeline);
                    pass.set_vertex_buffer(0, frame.glyph_buf.slice(..));
                    pass.set_bind_group(1, &self.glyph_bind_group, &[]);
                    pass.set_bind_group(2, &frame.mask_bind_group, &[span.mask * self.mask_stride]);
                    pass.draw(0..6, span.start..span.end);
                }
            }
        }
    }

    fn wgpu_clear(clear: Color) -> wgpu::Color {
        wgpu::Color {
            r: clear.r as f64 / 255.0,
            g: clear.g as f64 / 255.0,
            b: clear.b as f64 / 255.0,
            a: clear.a as f64 / 255.0,
        }
    }

    /// Render `commands` into a fresh `width`×`height` offscreen texture
    /// cleared to `clear`, and return the result as tightly-packed RGBA8 bytes
    /// (`width*height*4`, row-major, top-left origin). The headless test path.
    pub fn render_to_rgba(
        &self,
        commands: &[DrawCommand],
        images: &dyn ImageSource,
        width: u32,
        height: u32,
        clear: Color,
    ) -> Vec<u8> {
        assert!(width > 0 && height > 0, "zero-sized render target");
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        // Route through the composite path so backdrops and the effect slot
        // work (and are testable) headlessly too.
        self.composite_scene(
            &view,
            width,
            height,
            clear,
            &[SceneLayer::Commands { commands, images }],
            FxInputs::default(),
        );
        self.read_texture_rgba(&target, width, height)
    }

    /// Composite one output frame into `target` (the compositor's swapchain
    /// view): paint `layers` in list order — command layers (wallpaper,
    /// vector-native windows, overlays) and texture layers (ordinary client
    /// windows) interleave freely in z-order. `target`'s format must match
    /// the renderer's.
    ///
    /// With no `Backdrop` commands and no installed effect this is one render
    /// pass straight into `target` (the pre-D5 path, zero fx cost). Otherwise
    /// the scene accumulates in an offscreen texture, each `Backdrop` breaks
    /// the pass to blur what's behind it (dual Kawase), and the finished
    /// accumulation reaches `target` through the effect shader (or an
    /// identity blit).
    #[allow(clippy::too_many_arguments)]
    pub fn composite_scene(
        &self,
        target: &wgpu::TextureView,
        width: u32,
        height: u32,
        clear: Color,
        layers: &[SceneLayer<'_>],
        fx: FxInputs,
    ) {
        // The audio rows feed this frame's fx passes and the *next* particle
        // step (its bind groups are cached and read the same buffer) — one
        // analysis frame of skew, invisible at 30Hz.
        self.queue
            .write_buffer(&self.audio_buf, 0, bytemuck::cast_slice(&fx.audio.as_rows()));
        // Build per-layer GPU state up front (buffers must outlive the pass).
        // FrameData is boxed: it is ~224 bytes against the 4-byte variants,
        // so inline it would size every element of `built` (one per layer,
        // most of them Texture) to the largest.
        enum Built {
            Frame(Box<FrameData>),
            Texture { index: u32 },
            Shader,
            Boids { front: bool },
            WindowFx { window: u32, scissor: [u32; 4] },
            /// A layer that resolved to nothing this frame.
            Skip,
        }
        // Fx render targets up front: the model layer samples them during
        // the build (its color target doubles as a layer texture).
        let mut cache_guard = self.fx_cache.lock().unwrap();
        let cache = Self::ensure_fx_cache(&self.device, self.format, &mut cache_guard, width, height);

        let mut instances: Vec<ImageInstance> = Vec::new();
        let mut bind_groups: Vec<wgpu::BindGroup> = Vec::new();
        let built: Vec<Built> = layers
            .iter()
            .map(|layer| match layer {
                SceneLayer::Commands { commands, images } => {
                    Built::Frame(Box::new(self.build_frame(commands, *images, width, height)))
                }
                SceneLayer::Shader => Built::Shader,
                SceneLayer::WindowFx { window, bounds } => {
                    // Nothing installed, or the reachable region is entirely
                    // off-screen: the layer costs nothing at all.
                    match self.window_fx.lock().unwrap().is_some() {
                        false => Built::Skip,
                        true => match compute_scissor(
                            &[[bounds.x, bounds.y, bounds.x + bounds.w, bounds.y + bounds.h]],
                            width,
                            height,
                        ) {
                            Some(scissor) => Built::WindowFx { window: *window, scissor },
                            None => Built::Skip,
                        },
                    }
                }
                SceneLayer::Model => {
                    if self.model.lock().unwrap().is_none() {
                        Built::Skip
                    } else {
                        instances.push(ImageInstance {
                            pos: [0.0, 0.0],
                            size: [width as f32, height as f32],
                            alpha: 1.0,
                        });
                        bind_groups.push(self.device.create_bind_group(
                            &wgpu::BindGroupDescriptor {
                                label: Some("model-layer"),
                                layout: &self.image_bind_group_layout,
                                entries: &[
                                    wgpu::BindGroupEntry {
                                        binding: 0,
                                        resource: wgpu::BindingResource::TextureView(
                                            &cache.model_view,
                                        ),
                                    },
                                    wgpu::BindGroupEntry {
                                        binding: 1,
                                        resource: wgpu::BindingResource::Sampler(
                                            &self.image_sampler,
                                        ),
                                    },
                                ],
                            },
                        ));
                        Built::Texture { index: (instances.len() - 1) as u32 }
                    }
                }
                SceneLayer::Boids { front } => Built::Boids { front: *front },
                SceneLayer::Texture { view, rect, alpha } => {
                    instances.push(ImageInstance {
                        pos: [rect.x, rect.y],
                        size: [rect.w, rect.h],
                        alpha: *alpha,
                    });
                    bind_groups.push(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("window"),
                        layout: &self.image_bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&self.image_sampler),
                            },
                        ],
                    }));
                    Built::Texture { index: (instances.len() - 1) as u32 }
                }
            })
            .collect();
        let upload = if instances.is_empty() { vec![ImageInstance::zeroed()] } else { instances };
        let window_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("window-instances"),
            contents: bytemuck::cast_slice(&upload),
            usage: wgpu::BufferUsages::VERTEX,
        });

        // Texture segments need the viewport uniform at bind group 0 too.
        let viewport = [width as f32, height as f32];
        let viewport_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("viewport"),
            contents: bytemuck::cast_slice(&viewport),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let viewport_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewport"),
            layout: &self.viewport_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: viewport_buf.as_entire_binding(),
            }],
        });

        // A backdrop that is fully clipped out costs nothing; only visible
        // ones force the fx path.
        let has_backdrop = built.iter().any(|b| match b {
            Built::Frame(f) => f
                .items
                .iter()
                .any(|i| matches!(i, DrawItem::Backdrop { scissor: Some(_), .. })),
            _ => false,
        });
        let effect = self.effect.lock().unwrap();
        let bg = self.background.lock().unwrap();
        let boids = self.boids.lock().unwrap();
        let wants_bg =
            bg.is_some() && built.iter().any(|b| matches!(b, Built::Shader));
        // Draw one half of the flock into an open pass.
        let draw_boids = |pass: &mut wgpu::RenderPass<'_>,
                          viewport_bg: &wgpu::BindGroup,
                          front: bool| {
            if let Some(bs) = boids.as_ref() {
                // Nothing to draw until the first step has sized the trail
                // and built the bind groups that name it.
                if bs.render_binds.is_empty() {
                    return;
                }
                pass.set_scissor_rect(0, 0, width, height);
                let installed = self.particle_render.lock().unwrap();
                pass.set_pipeline(installed.as_ref().unwrap_or(&self.boid_render_pipeline));
                pass.set_bind_group(0, viewport_bg, &[]);
                pass.set_bind_group(
                    1,
                    &bs.render_binds[bs.current][usize::from(front)],
                    &[],
                );
                // 6 verts = shadow triangle + body; the shader culls boids
                // whose depth puts them in the other layer.
                pass.draw(0..6, 0..bs.count);
            }
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("composite") });

        // The model layer renders through its own depth pass first; the
        // composite then samples it like any window texture. Frame uniform
        // per the model-shader contract: resolution, time, exposure.
        let model = self.model.lock().unwrap();
        if let Some(m) = model.as_ref()
            && layers.iter().any(|l| matches!(l, SceneLayer::Model))
        {
            let frame_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("model-frame"),
                contents: bytemuck::cast_slice(&[
                    width as f32,
                    height as f32,
                    fx.time,
                    fx.scene.motion[3], // exposure
                    // The mesh's own bounds: a generic shader centres and
                    // scales from these instead of baking one model's
                    // measurements into its source.
                    m.bounds_min[0],
                    m.bounds_min[1],
                    m.bounds_min[2],
                    0.0,
                    m.bounds_max[0],
                    m.bounds_max[1],
                    m.bounds_max[2],
                    0.0,
                ]),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            let model_scene_rows = fx.scene.as_rows();
            let model_scene_buf =
                self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("model-scene"),
                    contents: bytemuck::cast_slice(&model_scene_rows),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
            let frame_bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("model-frame"),
                layout: &m.frame_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: frame_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: model_scene_buf.as_entire_binding(),
                    },
                ],
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("model"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &cache.model_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &cache.model_depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&m.pipeline);
            pass.set_bind_group(0, &frame_bind, &[]);
            pass.set_vertex_buffer(0, m.vertex_buf.slice(..));
            // Two instances by contract: 0 is the object, 1 is the mirror
            // slot — a planar reflection in the floor, which is what makes a
            // showcase sit *in* its scene instead of floating over it. A
            // shader with no floor collapses instance 1 to a degenerate
            // point (clipped, free).
            pass.draw(0..m.vertex_count, 0..2);
            drop(pass);
        }
        drop(model);

        let has_window_fx = built.iter().any(|b| matches!(b, Built::WindowFx { .. }));
        if !has_backdrop && effect.is_none() && !wants_bg && !has_window_fx {
            // Direct path: one pass straight into the target.
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(Self::wgpu_clear(clear)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            for item in &built {
                match item {
                    Built::Frame(frame) => self.paint_frame(&mut pass, frame),
                    // No background installed: a Shader layer paints nothing.
                    Built::Shader => {}
                    // Unreachable: a per-window layer forces the
                    // accumulating path above, because it has to be in the
                    // scene before anything stacked over it samples.
                    Built::WindowFx { .. } => {}
                    Built::Skip => {}
                    Built::Boids { front } => draw_boids(&mut pass, &viewport_bind_group, *front),
                    Built::Texture { index } => {
                        pass.set_scissor_rect(0, 0, width, height);
                        pass.set_pipeline(&self.image_pipeline);
                        pass.set_vertex_buffer(0, window_buf.slice(..));
                        pass.set_bind_group(0, &viewport_bind_group, &[]);
                        pass.set_bind_group(1, &bind_groups[*index as usize], &[]);
                        pass.set_bind_group(2, &self.no_mask_bind_group, &[0]);
                        pass.draw(0..6, *index..*index + 1);
                    }
                }
            }
            drop(pass);
            self.queue.submit([encoder.finish()]);
            return;
        }

        // Fx path: accumulate offscreen, breaking the pass at each backdrop.
        // (Targets were ensured above, before the layer build.)

        // Fx uniforms serve the mid-scene background pass and the final
        // effect pass alike. The background's "scene" binding points at the
        // quarter target purely to satisfy the layout — its content is
        // undefined in that pass by contract.
        let fx_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fx-uniforms"),
            contents: bytemuck::cast_slice(&[
                width as f32,
                height as f32,
                fx.cursor[0],
                fx.cursor[1],
                fx.clock,
                0.0,
                0.0,
                0.0,
            ]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let time_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fx-time"),
            contents: bytemuck::cast_slice(&[fx.time]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        // The live window layout, padded to the uniform array size.
        let mut win_rects = [[0.0f32; 4]; 64];
        let win_n = fx.windows.len().min(64);
        win_rects[..win_n].copy_from_slice(&fx.windows[..win_n]);
        let mut win_meta = [[0.0f32; 4]; 64];
        let meta_n = fx.window_meta.len().min(win_n);
        win_meta[..meta_n].copy_from_slice(&fx.window_meta[..meta_n]);
        let meta_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fx-window-meta"),
            contents: bytemuck::cast_slice(&win_meta),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let mut win_vel = [[0.0f32; 4]; 64];
        let vel_n = fx.window_velocity.len().min(win_n);
        win_vel[..vel_n].copy_from_slice(&fx.window_velocity[..vel_n]);
        let vel_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fx-window-velocity"),
            contents: bytemuck::cast_slice(&win_vel),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let scene_rows = fx.scene.as_rows();
        let scene_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fx-scene"),
            contents: bytemuck::cast_slice(&scene_rows),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let windows_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fx-windows"),
            contents: bytemuck::cast_slice(&win_rects),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let win_count_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fx-window-count"),
            contents: bytemuck::bytes_of(&(win_n as u32)),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let make_fx_bind_with = |scene_view: &wgpu::TextureView,
                                 fx_uniforms: &wgpu::Buffer,
                                 params: &wgpu::Buffer,
                                 label: &str| {
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &self.fx_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(scene_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.image_sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: fx_uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry { binding: 3, resource: time_buf.as_entire_binding() },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: windows_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: win_count_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 6,
                        resource: meta_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 7,
                        resource: scene_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 8,
                        resource: vel_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 9,
                        resource: self.audio_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 10,
                        resource: params.as_entire_binding(),
                    },
                ],
            })
        };
        let make_fx_bind = |scene_view: &wgpu::TextureView, params: &wgpu::Buffer, label: &str| {
            make_fx_bind_with(scene_view, &fx_buf, params, label)
        };
        let bg_bind =
            wants_bg.then(|| make_fx_bind(&cache.quarter_view, &self.bg_params_buf, "bg"));

        // One bind group per per-window layer, each with its own copy of the
        // fx uniforms carrying that layer's window index. Built here because
        // the pass borrows them; wgpu refcounts the buffer, so it may drop.
        // `scene` points at the quarter target only to satisfy the layout —
        // a per-window effect generates, it never samples the frame it is
        // being drawn into.
        let window_fx_binds: Vec<Option<wgpu::BindGroup>> = built
            .iter()
            .map(|b| match b {
                Built::WindowFx { window, .. } => {
                    let buf = self.device.create_buffer_init(
                        &wgpu::util::BufferInitDescriptor {
                            label: Some("window-fx-uniforms"),
                            contents: bytemuck::cast_slice(&[
                                width as f32,
                                height as f32,
                                fx.cursor[0],
                                fx.cursor[1],
                                fx.clock,
                                *window as f32,
                                0.0,
                                0.0,
                            ]),
                            usage: wgpu::BufferUsages::UNIFORM,
                        },
                    );
                    Some(make_fx_bind_with(
                        &cache.quarter_view,
                        &buf,
                        &self.window_params_buf,
                        "window-fx",
                    ))
                }
                _ => None,
            })
            .collect();

        let mut first = true;
        let mut pass: Option<wgpu::RenderPass<'static>> = None;
        for (layer_index, item) in built.iter().enumerate() {
            match item {
                Built::Frame(frame) => {
                    let mut start = 0usize;
                    for i in 0..frame.items.len() {
                        let DrawItem::Backdrop { scissor, rect, blur, corner_radius } =
                            &frame.items[i]
                        else {
                            continue;
                        };
                        if start < i {
                            let p = pass.get_or_insert_with(|| {
                                begin_accum(&mut encoder, &cache.accum_view, &mut first, clear)
                            });
                            self.paint_items(p, frame, start..i);
                        }
                        start = i + 1;
                        let Some(sc) = scissor else { continue };
                        // Break the pass; blur what has accumulated so far.
                        drop(pass.take());
                        if first {
                            // Backdrop before any paint: materialize the clear
                            // color so the blur has something to sample.
                            drop(begin_accum(&mut encoder, &cache.accum_view, &mut first, clear));
                        }
                        self.blur_chain(&mut encoder, cache, *blur);
                        // Resume, painting the frosted pane from the blurred
                        // half-res result.
                        let patch_buf =
                            self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                                label: Some("backdrop-instance"),
                                contents: bytemuck::cast_slice(&[BackdropInstance {
                                    pos: [rect[0], rect[1]],
                                    size: [rect[2], rect[3]],
                                    radius: *corner_radius,
                                }]),
                                usage: wgpu::BufferUsages::VERTEX,
                            });
                        let patch_bind =
                            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                                label: Some("backdrop"),
                                layout: &self.image_bind_group_layout,
                                entries: &[
                                    wgpu::BindGroupEntry {
                                        binding: 0,
                                        resource: wgpu::BindingResource::TextureView(
                                            &cache.half_view,
                                        ),
                                    },
                                    wgpu::BindGroupEntry {
                                        binding: 1,
                                        resource: wgpu::BindingResource::Sampler(
                                            &self.image_sampler,
                                        ),
                                    },
                                ],
                            });
                        let p = pass.get_or_insert_with(|| {
                            begin_accum(&mut encoder, &cache.accum_view, &mut first, clear)
                        });
                        p.set_scissor_rect(sc[0], sc[1], sc[2], sc[3]);
                        p.set_pipeline(&self.backdrop_pipeline);
                        p.set_bind_group(0, &frame.viewport_bind_group, &[]);
                        p.set_bind_group(1, &patch_bind, &[]);
                        p.set_vertex_buffer(0, patch_buf.slice(..));
                        p.draw(0..6, 0..1);
                    }
                    if start < frame.items.len() {
                        let p = pass.get_or_insert_with(|| {
                            begin_accum(&mut encoder, &cache.accum_view, &mut first, clear)
                        });
                        self.paint_items(p, frame, start..frame.items.len());
                    }
                }
                Built::Shader => {
                    if let (Some(bg), Some(bind)) = (bg.as_ref(), bg_bind.as_ref()) {
                        // Generative wallpaper: a fullscreen pass drawn in
                        // place — it overwrites everything below its z, so
                        // no pass break or blend is needed.
                        let p = pass.get_or_insert_with(|| {
                            begin_accum(&mut encoder, &cache.accum_view, &mut first, clear)
                        });
                        p.set_scissor_rect(0, 0, width, height);
                        p.set_pipeline(&bg.pipeline);
                        p.set_bind_group(0, bind, &[]);
                        p.draw(0..3, 0..1);
                    }
                }
                Built::Skip => {}
                // The per-window effect, painted at its window's z. Blended,
                // so it composites over whatever that window sits on; and
                // early enough that a glass window above it will blur it,
                // because a backdrop samples the accumulation as it stands
                // when the backdrop is reached — and by then this is in it.
                Built::WindowFx { scissor, .. } => {
                    if let (Some(wfx), Some(bind)) = (
                        self.window_fx.lock().unwrap().as_ref(),
                        window_fx_binds[layer_index].as_ref(),
                    ) {
                        let p = pass.get_or_insert_with(|| {
                            begin_accum(&mut encoder, &cache.accum_view, &mut first, clear)
                        });
                        p.set_scissor_rect(scissor[0], scissor[1], scissor[2], scissor[3]);
                        p.set_pipeline(&wfx.pipeline);
                        p.set_bind_group(0, bind, &[]);
                        p.draw(0..3, 0..1);
                    }
                }
                Built::Boids { front } => {
                    let p = pass.get_or_insert_with(|| {
                        begin_accum(&mut encoder, &cache.accum_view, &mut first, clear)
                    });
                    draw_boids(p, &viewport_bind_group, *front);
                }
                Built::Texture { index } => {
                    let p = pass.get_or_insert_with(|| {
                        begin_accum(&mut encoder, &cache.accum_view, &mut first, clear)
                    });
                    p.set_scissor_rect(0, 0, width, height);
                    p.set_pipeline(&self.image_pipeline);
                    p.set_vertex_buffer(0, window_buf.slice(..));
                    p.set_bind_group(0, &viewport_bind_group, &[]);
                    p.set_bind_group(1, &bind_groups[*index as usize], &[]);
                    p.set_bind_group(2, &self.no_mask_bind_group, &[0]);
                    p.draw(0..6, *index..*index + 1);
                }
            }
        }
        if first {
            // Empty scene: the accumulation still needs its clear.
            drop(begin_accum(&mut encoder, &cache.accum_view, &mut first, clear));
        }
        drop(pass.take());

        // Final pass: accumulation → target through the effect (or identity).
        let fx_bind = make_fx_bind(&cache.accum_view, &self.fx_params_buf, "fx");
        {
            let mut p = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fx"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            p.set_pipeline(
                effect.as_ref().map(|e| &e.pipeline).unwrap_or(&self.identity_pipeline),
            );
            p.set_bind_group(0, &fx_bind, &[]);
            p.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);
    }

    /// (Re)create the fx targets when the output size changes.
    fn ensure_fx_cache<'a>(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        slot: &'a mut Option<FxCache>,
        w: u32,
        h: u32,
    ) -> &'a FxCache {
        let stale = slot.as_ref().is_none_or(|c| c.w != w || c.h != h);
        if stale {
            let mk = |label: &str, tw: u32, th: u32| {
                let t = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d { width: tw, height: th, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                });
                let v = t.create_view(&wgpu::TextureViewDescriptor::default());
                (t, v)
            };
            let (half_w, half_h) = ((w / 2).max(1), (h / 2).max(1));
            let (quarter_w, quarter_h) = ((w / 4).max(1), (h / 4).max(1));
            let (_accum, accum_view) = mk("fx-accum", w, h);
            let (_half, half_view) = mk("fx-half", half_w, half_h);
            let (_quarter, quarter_view) = mk("fx-quarter", quarter_w, quarter_h);
            let (_model, model_view) = mk("model-color", w, h);
            let depth = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("model-depth"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth24Plus,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let model_depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
            *slot = Some(FxCache {
                w,
                h,
                accum_view,
                half_view,
                half_w,
                half_h,
                quarter_view,
                quarter_w,
                quarter_h,
                model_view,
                model_depth_view,
            });
        }
        slot.as_ref().unwrap()
    }

    /// Dual-Kawase blur of the accumulation: full→half→quarter down, then
    /// quarter→half up. The frosted pane samples the half-res result.
    fn blur_chain(&self, encoder: &mut wgpu::CommandEncoder, cache: &FxCache, blur: f32) {
        // Texel offset from the logical radius; the down/upsampling does most
        // of the spreading, this tunes the tail.
        let offset = (blur / 8.0).clamp(0.75, 6.0);
        let mut run = |pipeline: &wgpu::RenderPipeline,
                       src: &wgpu::TextureView,
                       src_w: u32,
                       src_h: u32,
                       dst: &wgpu::TextureView,
                       label: &str| {
            let uniform = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: bytemuck::cast_slice(&[KawaseUniform {
                    texel: [1.0 / src_w as f32, 1.0 / src_h as f32],
                    offset,
                    _pad: 0.0,
                }]),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &self.kawase_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(src),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.image_sampler),
                    },
                    wgpu::BindGroupEntry { binding: 2, resource: uniform.as_entire_binding() },
                ],
            });
            let mut p = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(label),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: dst,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            p.set_pipeline(pipeline);
            p.set_bind_group(0, &bind, &[]);
            p.draw(0..3, 0..1);
        };
        run(
            &self.kawase_down_pipeline,
            &cache.accum_view,
            cache.w,
            cache.h,
            &cache.half_view,
            "kawase-down-1",
        );
        run(
            &self.kawase_down_pipeline,
            &cache.half_view,
            cache.half_w,
            cache.half_h,
            &cache.quarter_view,
            "kawase-down-2",
        );
        run(
            &self.kawase_up_pipeline,
            &cache.quarter_view,
            cache.quarter_w,
            cache.quarter_h,
            &cache.half_view,
            "kawase-up-1",
        );
    }

    /// Read a texture back as tightly-packed RGBA8 (compositor screenshots,
    /// tests). The texture needs `COPY_SRC` and the renderer's format.
    pub fn read_texture_rgba(&self, texture: &wgpu::Texture, width: u32, height: u32) -> Vec<u8> {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
        let bytes = self.read_back(&mut encoder, texture, width, height);
        self.queue.submit([encoder.finish()]);
        self.map_readback(bytes, width, height)
    }

    /// The device the renderer runs on (for uploading external textures —
    /// shm buffers, test fixtures).
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Bind a texture the caller already owns.
    ///
    /// The cheap path, and the one the compositor takes: a bind group per
    /// frame that names the image, against a texture uploaded once when the
    /// client attached it. No creation, no copy.
    fn bind_image(&self, view: &wgpu::TextureView) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image"),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.image_sampler),
                },
            ],
            layout: &self.image_bind_group_layout,
        })
    }

    /// Upload one image as a texture and build its bind group.
    ///
    /// A fresh texture, upload and bind group *per frame that names the
    /// image* — so this is for hosts that can only hand over pixels, and it
    /// is why [`ImageSource::texture`] is tried first. The production path
    /// (the compositor) never reaches here: images arrive once over
    /// `attach_image`, are uploaded on arrival, and bind through
    /// [`Renderer::bind_image`].
    fn upload_image(&self, data: &ImageData) -> wgpu::BindGroup {
        let size =
            wgpu::Extent3d { width: data.width, height: data.height, depth_or_array_layers: 1 };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &data.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(data.width * 4),
                rows_per_image: Some(data.height),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image"),
            layout: &self.image_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.image_sampler),
                },
            ],
        })
    }

    /// Record the texture→buffer copy (rows padded to the 256-byte alignment
    /// wgpu requires). Returns the readback buffer + its padded row stride.
    fn read_back(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::Texture,
        width: u32,
        height: u32,
    ) -> (wgpu::Buffer, u32) {
        let unpadded = width * 4;
        let padded =
            unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        (readback, padded)
    }

    /// Map the readback buffer (valid only after submit) and unpad its rows into
    /// tightly-packed RGBA8.
    fn map_readback(&self, readback: (wgpu::Buffer, u32), width: u32, height: u32) -> Vec<u8> {
        let (readback, padded) = readback;
        let unpadded = (width * 4) as usize;
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        loop {
            let _ = self.device.poll(wgpu::PollType::Wait);
            match rx.try_recv() {
                Ok(result) => {
                    result.expect("readback map failed");
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => panic!("map channel closed"),
            }
        }
        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity(unpadded * height as usize);
        for row in 0..height {
            let start = (row * padded) as usize;
            out.extend_from_slice(&data[start..start + unpadded]);
        }
        drop(data);
        readback.unmap();
        out
    }
}

/// One SDF pipeline for sharp rects, rounded rects (anti-aliased), and shadows
/// (blur falloff). The drawn quad is expanded past the shape by the blur margin
/// so the falloff has room; a plain rect expands by 1px for edge AA.
const QUAD_WGSL: &str = include_str!("shaders/quad.wgsl");

/// Stroked-polyline pipeline: one instance per segment, each drawn as a
/// capsule so caps and joins are round without any joint geometry.
const LINE_WGSL: &str = include_str!("shaders/line.wgsl");
const FILL_WGSL: &str = include_str!("shaders/fill.wgsl");

/// Textured-quad pipeline for images: the texture stretches across the rect
/// (what the gpui backend's `paint_image` did), clamp-to-edge, alpha blend.
const IMAGE_WGSL: &str = include_str!("shaders/image.wgsl");

/// Glyph pipeline: an atlas sub-rectangle placed on screen; the `.r` channel
/// is coverage, tinted by the per-instance text color.
const GLYPH_WGSL: &str = include_str!("shaders/glyph.wgsl");

/// Backdrop pane: sample the blurred scene at the pane's screen position,
/// masked to the rounded rect (same SDF + AA as the quad pipeline).
const BACKDROP_WGSL: &str = include_str!("shaders/backdrop.wgsl");

/// Dual-Kawase blur, one shader: fullscreen-triangle vertex stage plus the
/// classic down/up fragment kernels.
const KAWASE_WGSL: &str = include_str!("shaders/kawase.wgsl");

/// Prepended to every whole-output effect (identity and user WGSL alike):
/// the finished scene as a texture, resolution + cursor uniforms, a `time`
/// uniform (reading it marks the effect as animated), and the fullscreen
/// vertex stage. User source supplies `@fragment fn fs_main(in: FxIn)`.
pub const EFFECT_PREAMBLE: &str = include_str!("shaders/effect_preamble.wgsl");

/// Boid flock step: classic separation/alignment/cohesion, plus window
/// rects as obstacles and the cursor as a curiosity/personal-space field.
const BOIDS_COMPUTE_WGSL: &str = include_str!("shaders/boids_compute.wgsl");

/// The contract a particle *update* shader is compiled against — state
/// buffer, params, window rects and window velocity. Public so a wallpaper
/// author can read what they are writing against.
pub const PARTICLE_COMPUTE_PREAMBLE: &str =
    include_str!("shaders/particle_compute_preamble.wgsl");

/// The contract a particle *draw* shader is compiled against.
pub const PARTICLE_RENDER_PREAMBLE: &str =
    include_str!("shaders/particle_render_preamble.wgsl");

/// Boid rendering: one velocity-oriented triangle per instance, pulled
/// straight from the simulation's storage buffer — no CPU round-trip.
const BOIDS_RENDER_WGSL: &str = include_str!("shaders/boids_render.wgsl");

/// The no-effect blit for the fx path's final pass.
const IDENTITY_EFFECT: &str = include_str!("shaders/identity_effect.wgsl");

#[cfg(test)]
mod tests {
    use super::*;

    fn px(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };
    const RED: Color = Color { r: 255, g: 0, b: 0, a: 255 };
    const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };
    const BLUE: Color = Color { r: 0, g: 0, b: 255, a: 255 };

    /// Composite a scene into a fresh target and read it back — the
    /// `render_to_rgba` convenience, for layer stacks rather than commands.
    impl Renderer {
        fn composite_to_rgba(
            &self,
            width: u32,
            height: u32,
            clear: Color,
            layers: &[SceneLayer<'_>],
        ) -> Vec<u8> {
            let target = self.device().create_texture(&wgpu::TextureDescriptor {
                label: Some("composite-test-target"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = target.create_view(&Default::default());
            self.composite_scene(
                &view,
                width,
                height,
                clear,
                layers,
                FxInputs::default(),
            );
            self.read_texture_rgba(&target, width, height)
        }
    }

    /// A `Renderer` that holds the suite's serialization guard for as long
    /// as the test holds it. The NVIDIA driver deadlocks under concurrent
    /// headless device creation (2 test threads pass, 4 wedge every thread
    /// in futex waits — 2026-08-30), so GPU tests run one at a time no
    /// matter what `--test-threads` says. `Deref` keeps the call sites
    /// untouched; field order drops the device before releasing the lock.
    struct TestRenderer {
        r: Renderer,
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    impl std::ops::Deref for TestRenderer {
        type Target = Renderer;
        fn deref(&self) -> &Renderer {
            &self.r
        }
    }

    fn renderer() -> Option<TestRenderer> {
        static SERIAL: Mutex<()> = Mutex::new(());
        let guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let r = Renderer::new_headless();
        if r.is_none() {
            eprintln!("skip: no wgpu adapter available");
        }
        Some(TestRenderer { r: r?, _serial: guard })
    }

    /// A rounded PushClip masks content to its curve: the corner pixel of a
    /// full-bleed rect vanishes, its centre stays, and a rect drawn *after*
    /// PopClip (how the compositor draws glow/shadow, outside the clip pair)
    /// keeps its square corner.
    #[test]
    fn a_rounded_clip_masks_corners_and_spares_what_is_outside() {
        let Some(r) = renderer() else { return };
        let full = Rect { x: 0.0, y: 0.0, w: 32.0, h: 32.0 };
        let cmds = vec![
            DrawCommand::PushClip { rect: full, radius: 10.0 },
            DrawCommand::Rect { rect: full, color: RED, corner_radius: 0.0 },
            DrawCommand::PopClip,
        ];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 32, 32, BLACK);
        assert_eq!(px(&buf, 32, 16, 16), [255, 0, 0, 255], "centre survives the mask");
        assert_eq!(px(&buf, 32, 16, 1), [255, 0, 0, 255], "edge midpoint survives");
        assert_eq!(px(&buf, 32, 1, 1), [0, 0, 0, 255], "corner is masked away");
        assert_eq!(px(&buf, 32, 30, 30), [0, 0, 0, 255], "every corner is masked");

        // A command drawn after the pair is byte-identical to one drawn with
        // no clips at all — the mask does not leak out of its pair. (Compared
        // whole-buffer against a baseline rather than to literal white: a
        // full-bleed quad has its own frame-edge AA quirk, mask or not.)
        let base = vec![DrawCommand::Rect { rect: full, color: WHITE, corner_radius: 0.0 }];
        let baseline = r.render_to_rgba(&base, &NoImageSource, 32, 32, BLACK);
        let probe = vec![
            DrawCommand::PushClip { rect: full, radius: 10.0 },
            DrawCommand::PopClip,
            DrawCommand::Rect { rect: full, color: WHITE, corner_radius: 0.0 },
        ];
        let after = r.render_to_rgba(&probe, &NoImageSource, 32, 32, BLACK);
        assert_eq!(after, baseline, "outside the pair is unmasked");
    }

    /// Weight has to reach the glyphs, not just the style struct.
    ///
    /// The bundled mono is a variable font whose only registered face is
    /// ExtraLight (200), so for a long time every weight request rendered the
    /// same thin outlines — the style said Bold and the pixels said 200.
    /// The renderer now moves the font's `wght` axis, so this measures the
    /// only thing that actually settles it: **ink**. Heavier text covers more
    /// of the target, and the same glyph at two weights is two different
    /// rasters rather than one cached under a key that ignores weight.
    #[test]
    fn heavier_weight_puts_more_ink_on_the_screen() {
        let Some(r) = renderer() else { return };
        let (w, h) = (220u32, 40u32);
        // How much of the target the glyphs cover, weighted by coverage:
        // antialiasing means a heavier stem shows up as brighter edge pixels
        // as well as more of them, and summing catches both.
        let ink = |weight: u16| -> u64 {
            let cmds = vec![DrawCommand::Text {
                rect: Rect { x: 2.0, y: 2.0, w: w as f32 - 4.0, h: h as f32 - 4.0 },
                text: "Hamburgefonstiv".to_string(),
                color: WHITE,
                font_size: 22.0,
                font_weight: weight,
                font_family: "mono".to_string(),
            }];
            let buf = r.render_to_rgba(&cmds, &NoImageSource, w, h, BLACK);
            buf.as_chunks::<4>().0.iter().map(|p| p[0] as u64).sum()
        };

        let (thin, regular, bold) = (ink(200), ink(400), ink(700));
        assert!(thin > 0, "the probe text did not render at all");
        assert!(
            regular > thin,
            "400 must be heavier than 200 (got {regular} vs {thin}) — the wght \
             axis is not reaching the rasterizer"
        );
        assert!(
            bold > regular,
            "700 must be heavier than 400 (got {bold} vs {regular})"
        );
        // And the atlas must not be serving one weight's raster for another:
        // that was the trap when the glyph cache keyed on cosmic-text's
        // CacheKey alone, which cannot see that the axis moved.
        assert!(
            bold as f64 > thin as f64 * 1.1,
            "700 vs 200 is barely a difference ({bold} vs {thin}) — glyphs are \
             probably being reused across weights"
        );
    }

    #[test]
    fn solid_rect_paints_where_expected() {
        let Some(r) = renderer() else { return };
        eprintln!("rill-gpu test adapter: {}", r.adapter_name());
        // Opaque red rect (3,3,10,10) on black; sample ≥2px inside/outside so
        // the 1px AA band never touches the assertions.
        let cmds = vec![DrawCommand::Rect {
            rect: Rect { x: 3.0, y: 3.0, w: 10.0, h: 10.0 },
            color: RED,
            corner_radius: 0.0,
        }];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 16, 16, BLACK);
        assert_eq!(buf.len(), 16 * 16 * 4);
        assert_eq!(px(&buf, 16, 8, 8), [255, 0, 0, 255], "centre is red");
        assert_eq!(px(&buf, 16, 0, 0), [0, 0, 0, 255], "far corner is clear");
        assert_eq!(px(&buf, 16, 15, 15), [0, 0, 0, 255], "past the rect is clear");
    }

    /// The diagonal a rect pipeline cannot draw: on the line is red, and the
    /// corners the stroke passes *between* stay clear.
    #[test]
    fn diagonal_path_paints_off_axis() {
        let Some(r) = renderer() else { return };
        let cmds = vec![DrawCommand::Path {
            points: vec![Point::new(2.0, 2.0), Point::new(30.0, 30.0)],
            color: RED,
            width: 3.0,
            closed: false,
        }];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 32, 32, BLACK);
        for (x, y) in [(8u32, 8u32), (16, 16), (24, 24)] {
            let p = px(&buf, 32, x, y);
            assert!(p[0] > 200, "on the diagonal at {x},{y} should be red, got {p:?}");
        }
        // Off-diagonal corners: a bounding-box fill would have covered these.
        for (x, y) in [(28u32, 4u32), (4, 28)] {
            let p = px(&buf, 32, x, y);
            assert!(p[0] < 40, "off the diagonal at {x},{y} should be clear, got {p:?}");
        }
    }

    /// A polyline's shared endpoint is covered — the round caps overlapping
    /// *are* the join, so a corner has no notch in it.
    #[test]
    fn polyline_joins_are_filled() {
        let Some(r) = renderer() else { return };
        let cmds = vec![DrawCommand::Path {
            points: vec![Point::new(4.0, 16.0), Point::new(16.0, 16.0), Point::new(16.0, 4.0)],
            color: RED,
            width: 4.0,
            closed: false,
        }];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 32, 32, BLACK);
        let corner = px(&buf, 32, 16, 16);
        assert!(corner[0] > 200, "the join should be solid, got {corner:?}");
        assert!(px(&buf, 32, 10, 16)[0] > 200, "first leg painted");
        assert!(px(&buf, 32, 16, 10)[0] > 200, "second leg painted");
    }

    /// Paths interleave with rects in list order rather than batching to the
    /// end — the property that pipeline switching has to preserve.
    #[test]
    fn paths_paint_in_list_order() {
        let Some(r) = renderer() else { return };
        let line = DrawCommand::Path {
            points: vec![Point::new(0.0, 8.0), Point::new(16.0, 8.0)],
            color: RED,
            width: 8.0,
            closed: false,
        };
        let cover = DrawCommand::Rect {
            rect: Rect { x: 0.0, y: 0.0, w: 16.0, h: 16.0 },
            color: WHITE,
            corner_radius: 0.0,
        };
        // Line first, then an opaque rect over it: the rect wins.
        let buf = r.render_to_rgba(&[line.clone(), cover.clone()], &NoImageSource, 16, 16, BLACK);
        assert_eq!(px(&buf, 16, 8, 8), [255, 255, 255, 255], "rect painted after the path");
        // Reversed: the line wins.
        let buf = r.render_to_rgba(&[cover, line], &NoImageSource, 16, 16, BLACK);
        assert_eq!(px(&buf, 16, 8, 8), [255, 0, 0, 255], "path painted after the rect");
    }

    /// Design reference for the application-UI vocabulary: a file manager
    /// drawn straight in DrawCommands, at the quality the document layer
    /// should be able to reach. Run it, look at it, and the missing
    /// primitives name themselves.
    ///
    ///   UI_MOCK=out.ppm cargo test -p rill-gpu -- --ignored ui_mock
    #[test]
    #[ignore = "writes a design mock; run explicitly"]
    fn ui_mock() {
        use rill_ui::Point;
        let Some(r) = renderer() else { return };
        let (w, h) = (900.0f32, 620.0f32);

        // Dark theme tokens (crates/rill-viewport/src/theme.rs).
        let page = Color { r: 0x12, g: 0x12, b: 0x19, a: 255 };
        let surface = Color { r: 0x1b, g: 0x1b, b: 0x28, a: 255 };
        let raised = Color { r: 0x24, g: 0x24, b: 0x38, a: 255 };
        let ink = Color { r: 0xe8, g: 0xe8, b: 0xf0, a: 255 };
        let muted = Color { r: 0x9a, g: 0x9a, b: 0xb0, a: 255 };
        let border = Color { r: 0x33, g: 0x33, b: 0x4a, a: 255 };
        let accent = Color { r: 0x7c, g: 0x5c, b: 0xff, a: 255 };

        let engine = r.text_engine().clone();
        let width_of = |s: &str, size: f32, weight: u16| -> f32 {
            let p = engine.prepare(s, size, weight, "sans-serif");
            p.segments.iter().map(|seg| seg.width).sum::<f32>()
        };
        let text = |x: f32, y: f32, s: &str, c: Color, size: f32, weight: u16| DrawCommand::Text {
            rect: Rect { x, y, w: 600.0, h: size * 1.4 },
            text: s.to_string(),
            color: c,
            font_size: size,
            font_weight: weight,
            font_family: "sans-serif".into(),
        };
        let fill = |x: f32, y: f32, w: f32, h: f32, c: Color, radius: f32| DrawCommand::Rect {
            rect: Rect { x, y, w, h },
            color: c,
            corner_radius: radius,
        };

        let mut out = vec![fill(0.0, 0.0, w, h, page, 0.0)];

        // --- header bar ---------------------------------------------------
        let head_h = 52.0;
        out.push(fill(0.0, 0.0, w, head_h, surface, 0.0));
        out.push(fill(0.0, head_h - 1.0, w, 1.0, border, 0.0));
        out.push(text(20.0, 17.0, "Files", ink, 15.0, 700));
        out.push(text(88.0, 19.0, "/  apps", muted, 13.0, 400));

        // --- sidebar --------------------------------------------------------
        let side_w = 190.0;
        out.push(fill(0.0, head_h, side_w, h - head_h, surface, 0.0));
        out.push(fill(side_w - 1.0, head_h, 1.0, h - head_h, border, 0.0));
        out.push(text(20.0, head_h + 18.0, "PLACES", muted, 10.0, 700));
        for (i, (label, selected)) in
            [("Root", false), ("apps", true), ("public", false)].iter().enumerate()
        {
            let y = head_h + 44.0 + i as f32 * 34.0;
            if *selected {
                out.push(fill(10.0, y - 6.0, side_w - 20.0, 30.0, accent, 8.0));
            }
            // Folder glyph: body + tab, both rounded rects.
            let c = if *selected { ink } else { muted };
            out.push(fill(22.0, y + 4.0, 8.0, 5.0, c, 1.5));
            out.push(fill(22.0, y + 6.0, 16.0, 11.0, c, 2.5));
            out.push(text(48.0, y, label, ink, 13.0, 500));
        }

        // --- list header ------------------------------------------------------
        let list_x = side_w + 24.0;
        let list_r = w - 24.0;
        out.push(text(list_x, head_h + 20.0, "Name", muted, 10.0, 700));
        let size_label = "SIZE";
        out.push(text(list_r - width_of(size_label, 10.0, 700), head_h + 20.0, size_label, muted, 10.0, 700));
        out.push(fill(list_x, head_h + 42.0, list_r - list_x, 1.0, border, 0.0));

        // --- rows -------------------------------------------------------------
        let rows: [(&str, &str, bool); 4] = [
            ("apps", "—", true),
            ("public", "—", true),
            ("notice.txt", "1.2 KB", false),
            ("manifest", "291 B", false),
        ];
        for (i, (name, size, is_dir)) in rows.iter().enumerate() {
            let y = head_h + 52.0 + i as f32 * 42.0;
            // Hover state on the third row.
            if i == 2 {
                out.push(fill(list_x - 10.0, y - 4.0, list_r - list_x + 20.0, 38.0, raised, 8.0));
            }
            let icon_x = list_x + 2.0;
            let icon_y = y + 6.0;
            if *is_dir {
                out.push(fill(icon_x, icon_y - 2.0, 9.0, 5.0, accent, 1.5));
                out.push(fill(icon_x, icon_y, 18.0, 13.0, accent, 2.5));
            } else {
                // Document: page body plus a folded corner drawn as a stroke.
                out.push(fill(icon_x + 2.0, icon_y - 3.0, 14.0, 17.0, muted, 2.0));
                out.push(DrawCommand::Path {
                    points: vec![
                        Point::new(icon_x + 11.0, icon_y - 3.0),
                        Point::new(icon_x + 16.0, icon_y + 2.0),
                    ],
                    color: page,
                    width: 1.5,
                    closed: false,
                });
            }
            out.push(text(list_x + 34.0, y, name, ink, 13.0, if *is_dir { 600 } else { 400 }));
            out.push(text(list_r - width_of(size, 12.0, 400), y + 1.0, size, muted, 12.0, 400));
            if i + 1 < rows.len() {
                out.push(fill(list_x + 34.0, y + 37.0, list_r - list_x - 34.0, 1.0, border, 0.0));
            }
        }

        // --- footer -----------------------------------------------------------
        out.push(text(list_x, h - 34.0, "1 item hidden by policy", muted, 11.0, 400));

        let rgba = r.render_to_rgba(&out, &NoImageSource, w as u32, h as u32, page);
        let mut ppm = format!("P6\n{} {}\n255\n", w as u32, h as u32).into_bytes();
        for px in rgba.chunks(4) {
            ppm.extend_from_slice(&px[..3]);
        }
        let path = std::env::var("UI_MOCK").unwrap_or_else(|_| "ui-mock.ppm".into());
        std::fs::write(&path, ppm).unwrap();
        eprintln!("wrote {path}");
    }

    /// A border is a ring: the edge is painted, the middle is not. That is
    /// the shape neither a fill nor a glow could make.
    #[test]
    fn border_paints_the_edge_and_not_the_middle() {
        let Some(r) = renderer() else { return };
        let cmds = vec![DrawCommand::Border {
            rect: Rect { x: 4.0, y: 4.0, w: 24.0, h: 24.0 },
            color: RED,
            width: 3.0,
            corner_radius: 0.0,
        }];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 32, 32, BLACK);
        assert!(px(&buf, 32, 16, 4)[0] > 180, "top edge is drawn");
        assert!(px(&buf, 32, 4, 16)[0] > 180, "left edge is drawn");
        assert_eq!(px(&buf, 32, 16, 16), [0, 0, 0, 255], "the middle stays clear");
        assert_eq!(px(&buf, 32, 0, 0), [0, 0, 0, 255], "outside stays clear");
    }

    #[test]
    fn empty_command_list_is_just_the_clear() {
        let Some(r) = renderer() else { return };
        let blue = Color { r: 0, g: 0, b: 255, a: 255 };
        let buf = r.render_to_rgba(&[], &NoImageSource, 2, 2, blue);
        for p in 0..4 {
            assert_eq!(&buf[p * 4..p * 4 + 4], &[0, 0, 255, 255]);
        }
    }

    #[test]
    fn rounded_corners_are_cut_away() {
        let Some(r) = renderer() else { return };
        // A full-canvas red rect with a big radius: centre stays red, the very
        // corner is rounded off to the black clear.
        let cmds = vec![DrawCommand::Rect {
            rect: Rect { x: 0.0, y: 0.0, w: 16.0, h: 16.0 },
            color: RED,
            corner_radius: 6.0,
        }];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 16, 16, BLACK);
        assert_eq!(px(&buf, 16, 8, 8), [255, 0, 0, 255], "centre is red");
        let corner = px(&buf, 16, 0, 0);
        assert!(corner[0] < 40, "rounded corner is (near) clear, got {corner:?}");
    }

    #[test]
    fn push_clip_limits_drawing() {
        let Some(r) = renderer() else { return };
        // Clip to the left half, then draw a full-canvas rect: only the left
        // half survives.
        let cmds = vec![
            DrawCommand::PushClip { rect: Rect { x: 0.0, y: 0.0, w: 8.0, h: 16.0 }, radius: 0.0 },
            DrawCommand::Rect {
                rect: Rect { x: 0.0, y: 0.0, w: 16.0, h: 16.0 },
                color: RED,
                corner_radius: 0.0,
            },
            DrawCommand::PopClip,
        ];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 16, 16, BLACK);
        assert_eq!(px(&buf, 16, 3, 8), [255, 0, 0, 255], "inside clip is red");
        assert_eq!(px(&buf, 16, 12, 8), [0, 0, 0, 255], "outside clip is clipped away");
    }

    #[test]
    fn shadow_is_bright_at_centre_and_fades_out() {
        let Some(r) = renderer() else { return };
        // White shadow of a small box, blurred, on black. Centre is bright; a
        // point well outside the blur radius stays black.
        let cmds = vec![DrawCommand::Shadow {
            rect: Rect { x: 12.0, y: 12.0, w: 8.0, h: 8.0 },
            color: WHITE,
            blur: 6.0,
            spread: 0.0,
            corner_radius: 2.0,
        }];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 32, 32, BLACK);
        let centre = px(&buf, 32, 16, 16);
        assert!(centre[0] > 200, "shadow centre is bright, got {centre:?}");
        let far = px(&buf, 32, 0, 0);
        assert!(far[0] < 20, "far from the shadow stays black, got {far:?}");
    }

    /// A 2×2 checker (red/green over blue/white) under the name "checker".
    struct OneImage;

    impl ImageSource for OneImage {
        fn rgba(&self, source: &str) -> Option<ImageData> {
            (source == "checker").then(|| ImageData {
                width: 2,
                height: 2,
                #[rustfmt::skip]
                pixels: vec![
                    255, 0, 0, 255,   0, 255, 0, 255,
                    0, 0, 255, 255,   255, 255, 255, 255,
                ],
            })
        }
    }

    #[test]
    fn image_stretches_over_its_rect() {
        let Some(r) = renderer() else { return };
        let cmds = vec![DrawCommand::Image {
            rect: Rect { x: 0.0, y: 0.0, w: 16.0, h: 16.0 },
            source: "checker".into(),
        }];
        let buf = r.render_to_rgba(&cmds, &OneImage, 16, 16, BLACK);
        // Clamp-to-edge magnification: pixels near the corners sample one pure
        // texel (both linear taps clamp to the same texel there).
        assert_eq!(px(&buf, 16, 1, 1), [255, 0, 0, 255], "top-left texel red");
        assert_eq!(px(&buf, 16, 14, 1), [0, 255, 0, 255], "top-right texel green");
        assert_eq!(px(&buf, 16, 1, 14), [0, 0, 255, 255], "bottom-left texel blue");
        assert_eq!(px(&buf, 16, 14, 14), [255, 255, 255, 255], "bottom-right texel white");
    }

    /// A host that already owns the texture — what the compositor is, once a
    /// client has attached an image. It answers with a view, so nothing is
    /// uploaded while painting.
    struct UploadedImage {
        _texture: wgpu::Texture,
        view: wgpu::TextureView,
    }

    impl ImageSource for UploadedImage {
        fn texture(&self, source: &str) -> Option<&wgpu::TextureView> {
            (source == "checker").then_some(&self.view)
        }
    }

    /// The texture path must paint exactly what the pixel path paints.
    ///
    /// This is the path production takes: pixels reach the compositor once,
    /// over `attach_image`, and are uploaded on arrival — so a frame naming
    /// an image binds an existing texture instead of creating one and copying
    /// into it, every frame, forever. If the two paths ever disagree, the
    /// cheap one is the one nobody is looking at.
    #[test]
    fn an_already_uploaded_texture_paints_the_same_as_raw_pixels() {
        let Some(r) = renderer() else { return };
        let cmds = vec![DrawCommand::Image {
            rect: Rect { x: 0.0, y: 0.0, w: 16.0, h: 16.0 },
            source: "checker".into(),
        }];
        let from_pixels = r.render_to_rgba(&cmds, &OneImage, 16, 16, BLACK);

        // Upload the same 2x2 the way the compositor does on attach.
        let data = OneImage.rgba("checker").unwrap();
        let size =
            wgpu::Extent3d { width: data.width, height: data.height, depth_or_array_layers: 1 };
        let texture = r.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("attached"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        r.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &data.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(data.width * 4),
                rows_per_image: Some(data.height),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let attached = UploadedImage { _texture: texture, view };

        let from_texture = r.render_to_rgba(&cmds, &attached, 16, 16, BLACK);
        assert_eq!(from_pixels, from_texture, "the two image paths disagree");
        assert_eq!(px(&from_texture, 16, 1, 1), [255, 0, 0, 255], "and it actually painted");
    }

    /// A source nobody supplied paints the placeholder rather than nothing —
    /// the state every document image was stuck in before images had a
    /// transport at all.
    #[test]
    fn an_unattached_source_paints_the_placeholder() {
        let Some(r) = renderer() else { return };
        let cmds = vec![DrawCommand::Image {
            rect: Rect { x: 0.0, y: 0.0, w: 16.0, h: 16.0 },
            source: "never-sent".into(),
        }];
        let buf = r.render_to_rgba(&cmds, &OneImage, 16, 16, BLACK);
        // Asserted by shape rather than by exact value: the target is sRGB,
        // so mid-tones come back transformed and pinning the constants here
        // would be pinning the colour space instead of the behaviour.
        let edge = px(&buf, 16, 1, 1);
        let middle = px(&buf, 16, 8, 8);
        assert!(edge[0] > 128, "the placeholder box is drawn, got {edge:?}");
        assert_ne!(edge, middle, "the placeholder has an inner panel");
        assert_ne!(edge, [0, 0, 0, 255], "something was painted");
    }

    /// Count pixels matching `pred` in a sub-rectangle.
    fn ink(buf: &[u8], w: u32, x0: u32, y0: u32, x1: u32, y1: u32, pred: fn([u8; 4]) -> bool) -> usize {
        let mut n = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                if pred(px(buf, w, x, y)) {
                    n += 1;
                }
            }
        }
        n
    }

    fn reddish(p: [u8; 4]) -> bool {
        p[0] >= 100 && p[1] < 60 && p[2] < 60
    }

    fn text_cmd(rect: Rect, text: &str, size: f32) -> DrawCommand {
        DrawCommand::Text {
            rect,
            text: text.to_string(),
            color: RED,
            font_size: size,
            font_weight: 400,
            font_family: "sans-serif".to_string(),
        }
    }

    #[test]
    fn text_paints_ink_inside_its_rect() {
        let Some(r) = renderer() else { return };
        let cmds =
            vec![text_cmd(Rect { x: 4.0, y: 4.0, w: 120.0, h: 24.0 }, "Hello Rill", 16.0)];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 128, 32, BLACK);
        // Real ink lands inside the text rect...
        let inside = ink(&buf, 128, 4, 4, 124, 28, reddish);
        assert!(inside > 30, "expected glyph ink inside the rect, got {inside} px");
        // ...and none above it or left of it.
        assert_eq!(ink(&buf, 128, 0, 0, 128, 3, reddish), 0, "ink above the rect");
        assert_eq!(ink(&buf, 128, 0, 0, 3, 32, reddish), 0, "ink left of the rect");
    }

    #[test]
    fn empty_text_paints_nothing() {
        let Some(r) = renderer() else { return };
        let cmds = vec![text_cmd(Rect { x: 0.0, y: 0.0, w: 64.0, h: 24.0 }, "", 16.0)];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 64, 24, BLACK);
        assert_eq!(ink(&buf, 64, 0, 0, 64, 24, reddish), 0);
    }

    #[test]
    fn text_wraps_to_more_lines_in_a_narrow_rect() {
        let Some(r) = renderer() else { return };
        // Same text: wide rect = 1 line, narrow rect = several. Compare the
        // vertical extent of ink rows.
        let wide = vec![text_cmd(Rect { x: 0.0, y: 0.0, w: 200.0, h: 96.0 }, "aaa bbb ccc", 16.0)];
        let narrow = vec![text_cmd(Rect { x: 0.0, y: 0.0, w: 34.0, h: 96.0 }, "aaa bbb ccc", 16.0)];
        let rows_with_ink = |buf: &[u8]| {
            (0..96).filter(|&y| ink(buf, 208, 0, y, 208, y + 1, reddish) > 0).count()
        };
        let wide_buf = r.render_to_rgba(&wide, &NoImageSource, 208, 96, BLACK);
        let narrow_buf = r.render_to_rgba(&narrow, &NoImageSource, 208, 96, BLACK);
        let (wide_rows, narrow_rows) = (rows_with_ink(&wide_buf), rows_with_ink(&narrow_buf));
        assert!(wide_rows > 0);
        assert!(
            narrow_rows > wide_rows * 2,
            "narrow rect should wrap to ~3 lines: wide={wide_rows} rows, narrow={narrow_rows} rows"
        );
    }

    #[test]
    fn text_is_clipped_by_push_clip() {
        let Some(r) = renderer() else { return };
        let cmds = vec![
            DrawCommand::PushClip { rect: Rect { x: 0.0, y: 0.0, w: 24.0, h: 32.0 }, radius: 0.0 },
            text_cmd(Rect { x: 0.0, y: 4.0, w: 120.0, h: 24.0 }, "Hello Rill", 16.0),
            DrawCommand::PopClip,
        ];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 128, 32, BLACK);
        assert!(ink(&buf, 128, 0, 0, 24, 32, reddish) > 0, "ink inside the clip");
        assert_eq!(ink(&buf, 128, 25, 0, 128, 32, reddish), 0, "ink escaped the clip");
    }

    #[test]
    fn composite_stacks_background_windows_overlay() {
        let Some(r) = renderer() else { return };
        // A 4x4 solid-green "client buffer" (stand-in for an imported dmabuf).
        let device = r.device();
        let window_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        r.queue().write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &window_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[0u8, 255, 0, 255].repeat(16),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(16),
                rows_per_image: Some(4),
            },
            wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
        );
        let window_view = window_tex.create_view(&Default::default());

        // Output target.
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: 32, height: 32, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&Default::default());

        let blue = Color { r: 0, g: 0, b: 255, a: 255 };
        // Background wallpaper, then a texture window, then a command layer —
        // the vector-native shape: command frames interleave with texture
        // windows in z-order.
        let wallpaper = [DrawCommand::Rect {
            rect: Rect { x: 0.0, y: 0.0, w: 32.0, h: 32.0 },
            color: blue,
            corner_radius: 0.0,
        }];
        let overlay = [DrawCommand::Rect {
            rect: Rect { x: 0.0, y: 0.0, w: 8.0, h: 8.0 },
            color: RED,
            corner_radius: 0.0,
        }];
        r.composite_scene(
            &target_view,
            32,
            32,
            BLACK,
            &[
                SceneLayer::commands(&wallpaper),
                SceneLayer::Texture {
                    view: &window_view,
                    rect: Rect { x: 8.0, y: 8.0, w: 16.0, h: 16.0 },
                    alpha: 1.0,
                },
                SceneLayer::commands(&overlay),
            ],
            FxInputs::default(),
        );

        let buf = r.read_texture_rgba(&target, 32, 32);
        assert_eq!(px(&buf, 32, 16, 16), [0, 255, 0, 255], "window pixel is the client green");
        assert_eq!(px(&buf, 32, 28, 28), [0, 0, 255, 255], "outside the window is wallpaper");
        // (4,4): overlay interior, clear of the corner AA band.
        assert_eq!(px(&buf, 32, 4, 4), [255, 0, 0, 255], "overlay paints on top");
    }

    /// The whole reason per-window effects exist: an effect drawn at its
    /// window's z is **occluded for real** by whatever is stacked above it,
    /// and a glass window in front of it picks it up in its backdrop blur.
    ///
    /// A whole-output grader can do neither. It runs once, after the frame
    /// is composited, so it can only guess at occlusion by testing window
    /// rects — which is why nothing could ever be painted in front of it,
    /// and why glass could not blur it.
    ///
    /// This also checks the `fx.window` uniform actually arrives: the test
    /// effect paints only when told it belongs to window 1, so if the index
    /// never reached the shader the frame comes back with no effect at all.
    #[test]
    fn a_window_effect_is_occluded_by_what_is_drawn_above_it() {
        let Some(r) = renderer() else { return };
        // Paints solid red, but only for window 1 — premultiplied opaque.
        r.set_window_fx(Some(
            "@fragment
             fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
                 if (i32(fx.window) != 1) { return vec4<f32>(0.0); }
                 return vec4<f32>(1.0, 0.0, 0.0, 1.0);
             }",
        ))
        .expect("window fx compiles");
        assert!(r.has_window_fx());

        let full = Rect { x: 0.0, y: 0.0, w: 32.0, h: 32.0 };
        let wallpaper = [DrawCommand::Rect { rect: full, color: BLUE, corner_radius: 0.0 }];
        // A window painted *after* the effect, covering the right half.
        let above = [DrawCommand::Rect {
            rect: Rect { x: 16.0, y: 0.0, w: 16.0, h: 32.0 },
            color: WHITE,
            corner_radius: 0.0,
        }];
        let buf = r.composite_to_rgba(32, 32, BLACK, &[
            SceneLayer::commands(&wallpaper),
            SceneLayer::WindowFx { window: 1, bounds: full },
            SceneLayer::commands(&above),
        ]);
        assert_eq!(px(&buf, 32, 4, 16), [255, 0, 0, 255], "the effect paints at its own z");
        assert_eq!(
            px(&buf, 32, 28, 16),
            [255, 255, 255, 255],
            "a window drawn above the effect covers it — real occlusion, not a rect test"
        );

        // Wrong index: the layer is addressed to window 1, so an effect that
        // only paints for window 0 must leave the frame alone. This is the
        // assertion that fails if every window's effect is handed the same
        // uniforms.
        r.set_window_fx(Some(
            "@fragment
             fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
                 if (i32(fx.window) != 0) { return vec4<f32>(0.0); }
                 return vec4<f32>(1.0, 0.0, 0.0, 1.0);
             }",
        ))
        .unwrap();
        let buf = r.composite_to_rgba(32, 32, BLACK, &[
            SceneLayer::commands(&wallpaper),
            SceneLayer::WindowFx { window: 1, bounds: full },
        ]);
        assert_eq!(px(&buf, 32, 4, 16), [0, 0, 255, 255], "wrong window index paints nothing");

        r.set_window_fx(None).unwrap();
        assert!(!r.has_window_fx());
    }

    #[test]
    fn backdrop_blurs_what_is_behind() {
        let Some(r) = renderer() else { return };
        // Left half red, right half blue; a frosted pane straddles the seam.
        let cmds = vec![
            DrawCommand::Rect {
                rect: Rect { x: 0.0, y: 0.0, w: 20.0, h: 40.0 },
                color: RED,
                corner_radius: 0.0,
            },
            DrawCommand::Rect {
                rect: Rect { x: 20.0, y: 0.0, w: 20.0, h: 40.0 },
                color: Color { r: 0, g: 0, b: 255, a: 255 },
                corner_radius: 0.0,
            },
            DrawCommand::Backdrop {
                rect: Rect { x: 8.0, y: 8.0, w: 24.0, h: 24.0 },
                blur: 12.0,
                corner_radius: 0.0,
            },
        ];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 40, 40, BLACK);
        // Outside the pane the halves stay pure.
        assert_eq!(px(&buf, 40, 2, 20), [255, 0, 0, 255], "outside pane stays sharp red");
        assert_eq!(px(&buf, 40, 38, 20), [0, 0, 255, 255], "outside pane stays sharp blue");
        // Inside the pane, 4px into the red side, blue has bled across the
        // seam — the pixel is a mix, not pure red.
        let [pr, _, pb, _] = px(&buf, 40, 16, 20);
        assert!(pb > 10 && pr < 250, "pane pixel should be blurred mix, got r={pr} b={pb}");
    }

    #[test]
    fn backdrop_rounded_corner_leaves_outside_untouched() {
        let Some(r) = renderer() else { return };
        // White ground, a sharp red square exactly under the pane. The pane's
        // rounded mask excludes its own square corner, so the sharp red must
        // survive there while the pane interior frosts toward white.
        let cmds = vec![
            DrawCommand::Rect {
                rect: Rect { x: 0.0, y: 0.0, w: 40.0, h: 40.0 },
                color: WHITE,
                corner_radius: 0.0,
            },
            DrawCommand::Rect {
                rect: Rect { x: 8.0, y: 8.0, w: 16.0, h: 16.0 },
                color: RED,
                corner_radius: 0.0,
            },
            DrawCommand::Backdrop {
                rect: Rect { x: 8.0, y: 8.0, w: 16.0, h: 16.0 },
                blur: 6.0,
                corner_radius: 8.0,
            },
        ];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 40, 40, BLACK);
        // (8,8): the pane's square corner, ~2.6px outside the rounded edge —
        // clear of the mask's AA band (the SDF's diagonal fwidth is ~1.4px).
        assert_eq!(px(&buf, 40, 8, 8), [255, 0, 0, 255], "outside the rounded mask: sharp red");
        // 3px inside the pane's left edge the white ground bleeds in hard —
        // even after the vibrancy boost the mix stays visibly washed.
        let [er, eg, _, _] = px(&buf, 40, 11, 16);
        assert!(eg > 10 && er > 100, "pane edge should be a red/white frost, got r={er} g={eg}");
    }

    const IDENTITY_FS: &str = "@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    return textureSample(scene, scene_samp, in.uv);
}";

    #[test]
    fn effect_identity_is_bit_exact() {
        let Some(r) = renderer() else { return };
        let cmds = vec![DrawCommand::Rect {
            rect: Rect { x: 4.0, y: 4.0, w: 12.0, h: 12.0 },
            color: RED,
            corner_radius: 0.0,
        }];
        let plain = r.render_to_rgba(&cmds, &NoImageSource, 32, 32, BLACK);
        let animated = r.set_effect(Some(IDENTITY_FS)).unwrap();
        assert!(!animated, "identity must not read time");
        let through = r.render_to_rgba(&cmds, &NoImageSource, 32, 32, BLACK);
        assert_eq!(plain, through, "identity effect must be a bit-exact pass-through");
        r.set_effect(None).unwrap();
        assert!(!r.effect_animated());
    }

    #[test]
    fn effect_transforms_output() {
        let Some(r) = renderer() else { return };
        let invert = "@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    let c = textureSample(scene, scene_samp, in.uv);
    return vec4<f32>(vec3<f32>(1.0) - c.rgb, 1.0);
}";
        r.set_effect(Some(invert)).unwrap();
        let cmds = vec![DrawCommand::Rect {
            rect: Rect { x: 0.0, y: 0.0, w: 16.0, h: 32.0 },
            color: RED,
            corner_radius: 0.0,
        }];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 32, 32, BLACK);
        assert_eq!(px(&buf, 32, 8, 16), [0, 255, 255, 255], "red inverts to cyan");
        assert_eq!(px(&buf, 32, 24, 16), [255, 255, 255, 255], "black clear inverts to white");
    }

    #[test]
    fn glow_rings_outside_only() {
        let Some(r) = renderer() else { return };
        // A glow around a rect must light the outside edge and leave the
        // interior untouched — that's its whole contract (focus rings on
        // translucent windows).
        let cmds = vec![
            DrawCommand::Rect {
                rect: Rect { x: 10.0, y: 10.0, w: 20.0, h: 20.0 },
                color: Color { r: 40, g: 40, b: 40, a: 255 },
                corner_radius: 0.0,
            },
            DrawCommand::Glow {
                rect: Rect { x: 10.0, y: 10.0, w: 20.0, h: 20.0 },
                color: Color { r: 0, g: 255, b: 0, a: 255 },
                blur: 8.0,
                corner_radius: 0.0,
            },
        ];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 40, 40, BLACK);
        assert_eq!(px(&buf, 40, 20, 20), [40, 40, 40, 255], "interior untouched by glow");
        let [_, og, _, _] = px(&buf, 40, 32, 20);
        assert!(og > 60, "outside edge should glow, got g={og}");
        assert_eq!(px(&buf, 40, 39, 0), [0, 0, 0, 255], "far corner beyond falloff stays clear");
    }

    #[test]
    fn boids_step_and_render() {
        let Some(r) = renderer() else { return };
        r.set_boids(128, [200.0, 200.0]);
        assert!(r.boids_active());
        // Step the flock with one window-obstacle in the middle.
        for i in 0..30 {
            r.step_boids(
                0.016,
                i as f32 * 0.016,
                &[[60.0, 60.0, 80.0, 80.0]],
                &[[0.0, 0.0, 0.0, 0.0]],
                [10.0, 10.0],
                200,
                200,
            );
        }
        let target = r.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("t"),
            size: wgpu::Extent3d { width: 200, height: 200, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: HEADLESS_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        r.composite_scene(
            &view,
            200,
            200,
            BLACK,
            &[SceneLayer::Boids { front: false }, SceneLayer::Boids { front: true }],
            FxInputs::default(),
        );
        let buf = r.read_texture_rgba(&target, 200, 200);
        let lit = buf.as_chunks::<4>().0.iter().filter(|p| p[0] > 40 || p[1] > 40 || p[2] > 40).count();
        assert!(lit > 100, "the flock should be visible, {lit} lit pixels");
        r.set_boids(0, [200.0, 200.0]);
        assert!(!r.boids_active());
    }

    #[test]
    fn background_shader_paints_under_windows() {
        let Some(r) = renderer() else { return };
        let green = "@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    return vec4<f32>(0.0, 1.0, 0.0, 1.0);
}";
        assert!(!r.set_background(Some(green)).unwrap());
        let win = [DrawCommand::Rect {
            rect: Rect { x: 8.0, y: 8.0, w: 8.0, h: 8.0 },
            color: RED,
            corner_radius: 0.0,
        }];
        let target = r.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("t"),
            size: wgpu::Extent3d { width: 24, height: 24, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: HEADLESS_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        r.composite_scene(
            &view,
            24,
            24,
            BLACK,
            &[SceneLayer::Shader, SceneLayer::commands(&win)],
            FxInputs::default(),
        );
        let buf = r.read_texture_rgba(&target, 24, 24);
        assert_eq!(px(&buf, 24, 2, 2), [0, 255, 0, 255], "shader wallpaper under");
        assert_eq!(px(&buf, 24, 12, 12), [255, 0, 0, 255], "window paints over it");
        r.set_background(None).unwrap();
        // With the slot cleared, the same scene falls back to the clear color.
        r.composite_scene(
            &view,
            24,
            24,
            BLACK,
            &[SceneLayer::Shader, SceneLayer::commands(&win)],
            FxInputs::default(),
        );
        let buf = r.read_texture_rgba(&target, 24, 24);
        assert_eq!(px(&buf, 24, 2, 2), [0, 0, 0, 255], "cleared slot paints nothing");
    }

    #[test]
    fn effect_rejects_bad_wgsl_and_keeps_running() {
        let Some(r) = renderer() else { return };
        assert!(r.set_effect(Some("not wgsl at all")).is_err(), "garbage must be rejected");
        assert!(
            r.set_effect(Some("fn helper() -> f32 { return 1.0; }")).is_err(),
            "missing fs_main must be rejected"
        );
        // A rejected shader must not break rendering.
        let cmds = vec![DrawCommand::Rect {
            rect: Rect { x: 0.0, y: 0.0, w: 8.0, h: 8.0 },
            color: RED,
            corner_radius: 0.0,
        }];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 16, 16, BLACK);
        assert_eq!(px(&buf, 16, 4, 4), [255, 0, 0, 255]);
    }

    #[test]
    fn effect_animation_detected_from_time_use() {
        let Some(r) = renderer() else { return };
        let pulsing = "@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    let c = textureSample(scene, scene_samp, in.uv);
    return vec4<f32>(c.rgb * (0.5 + 0.5 * sin(time)), c.a);
}";
        assert!(r.set_effect(Some(pulsing)).unwrap(), "time-reading shader is animated");
        assert!(r.effect_animated());
        assert!(!r.set_effect(Some(IDENTITY_FS)).unwrap(), "identity is static");
        assert!(!r.effect_animated());
    }

    /// The cold open's whole premise: a *moving* window displaces the field
    /// and a still one does not. Which is the distinction a rect alone
    /// cannot make — avoid-the-rectangle gives you a force field with a hole
    /// in it, identical whether the window is being dragged or parked.
    ///
    /// Checked by reading the state buffer back rather than by looking at
    /// pixels: this is about the simulation, and pixels would also pass if
    /// the draw shader happened to jitter.
    #[test]
    fn a_dragged_window_pushes_the_dust_and_a_parked_one_does_not() {
        let Some(r) = renderer() else { return };
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/shaders");
        let update = std::fs::read_to_string(format!("{dir}/dust_update.wgsl")).unwrap();
        r.set_particle_shaders(Some(&update), None).expect("dust compiles");

        // One window in the middle, and the same window again with velocity.
        let window = [[80.0f32, 80.0, 120.0, 120.0]];
        let still = [[0.0f32; 4]];
        let dragged = [[900.0f32, 0.0, 0.0, 0.0]];

        let settle = |vel: &[[f32; 4]]| -> Vec<f32> {
            r.set_boids(256, [400.0, 400.0]);
            for i in 0..40 {
                r.step_boids(0.016, i as f32 * 0.016, &window, vel, [-500.0, -500.0], 400, 400);
            }
            r.read_particles()
        };

        let parked = settle(&still);
        let shoved = settle(&dragged);
        assert_eq!(parked.len(), shoved.len());

        // Mean speed across the field: the drag has to move the air.
        let mean_speed = |state: &[f32]| -> f32 {
            let n = state.len() / 8;
            let total: f32 = (0..n)
                .map(|i| {
                    let (vx, vy) = (state[i * 8 + 4], state[i * 8 + 5]);
                    (vx * vx + vy * vy).sqrt()
                })
                .sum();
            total / n as f32
        };
        let (calm, stirred) = (mean_speed(&parked), mean_speed(&shoved));
        assert!(
            stirred > calm * 3.0,
            "a dragged window barely stirred the dust (calm {calm:.1}, stirred {stirred:.1}) \
             — window_vel is probably not reaching the update pass"
        );
        // And the push is *directional*: the window is travelling +x, so the
        // field's net motion must be too, not merely agitated.
        let mean_vx = |state: &[f32]| -> f32 {
            let n = state.len() / 8;
            (0..n).map(|i| state[i * 8 + 4]).sum::<f32>() / n as f32
        };
        assert!(
            mean_vx(&shoved) > 0.0,
            "the field was stirred but not pushed downwind ({:.1})",
            mean_vx(&shoved)
        );
    }

    /// Particles live in output pixels, so both ends of that have to survive
    /// a screen that is not the size the renderer guessed.
    ///
    /// The spawn half: scattering across a hardcoded extent leaves a wide
    /// desktop with every particle crowded into one corner. The resize half
    /// is subtler and is why `dust_update` keeps its home *normalised* — a
    /// home in pixels is a home on the old screen the moment the output
    /// changes, and the whole field would drift back to a rectangle the size
    /// of whatever the desktop used to be.
    #[test]
    fn particles_fill_the_output_they_were_given_and_survive_a_resize() {
        let Some(r) = renderer() else { return };
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/shaders");
        let update = std::fs::read_to_string(format!("{dir}/dust_update.wgsl")).unwrap();
        r.set_particle_shaders(Some(&update), None).expect("dust compiles");

        // A wide output, of the shape a real monitor actually has.
        let (w, h) = (3440.0f32, 1440.0f32);
        r.set_boids(512, [w, h]);
        let spread = |state: &[f32]| -> (f32, f32) {
            let n = state.len() / 8;
            let xs: Vec<f32> = (0..n).map(|i| state[i * 8]).collect();
            (
                xs.iter().cloned().fold(f32::MAX, f32::min),
                xs.iter().cloned().fold(f32::MIN, f32::max),
            )
        };
        let (lo, hi) = spread(&r.read_particles());
        assert!(
            hi > w * 0.75,
            "particles only reach x={hi:.0} on a {w:.0}-wide output — the \
             scatter is ignoring the size it was given"
        );
        assert!(lo < w * 0.25, "and they should start near the left edge too (lo {lo:.0})");

        // Let them settle at this size, then halve the output and settle
        // again — the field has to follow, not stay where the old screen was.
        // Long enough to actually arrive: after a halving a mote may have
        // most of the screen to cross, and at this drag that is seconds of
        // simulated time, not frames.
        let settle = |w: f32, h: f32| {
            for i in 0..700 {
                r.step_boids(
                    0.016,
                    i as f32 * 0.016,
                    &[],
                    &[],
                    [-1000.0, -1000.0],
                    w as u32,
                    h as u32,
                );
            }
        };
        settle(w, h);
        let before = r.read_particles();
        let (half_w, half_h) = (w * 0.5, h * 0.5);
        settle(half_w, half_h);

        // The property that actually distinguishes the two is *shape*, not
        // settling and not staying on screen. The wrap keeps motes on screen
        // either way, and it happens to fold a pixel-space home back into
        // range too, so both eventually come to rest. What a pixel-space
        // home cannot do is keep the field's arrangement: a mote that was
        // three-quarters of the way across has to still be three-quarters of
        // the way across, rather than wherever wrapping happened to drop it.
        let after = r.read_particles();
        let n = after.len() / 8;
        let mut moved: Vec<f32> = (0..n)
            .map(|i| {
                let before_n = before[i * 8] / w;
                let after_n = after[i * 8] / half_w;
                (after_n - before_n).abs()
            })
            .collect();
        moved.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = moved[n / 2];
        assert!(
            median < 0.12,
            "the field lost its shape across the resize (median mote moved \
             {:.0}% of the screen width) — home is probably in pixels, so \
             which mote ends up where is decided by wrapping rather than by \
             where it belonged",
            median * 100.0
        );
    }

    /// The audio rows are readable from all three preambles, through the
    /// helpers — the contract every sound-reactive wallpaper stands on.
    /// Compiling against the real device also proves the bind group
    /// layouts carry the new binding, not just the WGSL.
    #[test]
    fn audio_contract_reaches_all_three_preambles() {
        let Some(r) = renderer() else { return };
        r.set_effect(Some(
            "@fragment fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
                 let a = audio_levels();
                 return vec4<f32>(a.x, a.w, audio_beat() + audio_band(7u), 1.0);
             }",
        ))
        .expect("an effect can read fx_audio");
        r.set_particle_shaders(
            Some(
                "@compute @workgroup_size(64)
                 fn cs_main(@builtin(global_invocation_id) id: vec3<u32>) {
                     if (id.x >= params.count) { return; }
                     var p = src[id.x];
                     p.vel.y += audio_beat() * audio_levels().x + audio_band(0u);
                     dst[id.x] = p;
                 }",
            ),
            Some(
                "@vertex fn vs_main(
                     @builtin(vertex_index) vi: u32,
                     @builtin(instance_index) ii: u32,
                 ) -> VsOut {
                     var out: VsOut;
                     out.clip = to_clip(particles[ii].pos.xy * (1.0 + audio_beat()));
                     out.color = vec4<f32>(audio_levels().xyz, audio_band(31u));
                     return out;
                 }
                 @fragment fn fs_main(v: VsOut) -> @location(0) vec4<f32> {
                     return v.color;
                 }",
            ),
        )
        .expect("particle passes can read fx_audio");
    }

    /// Declared `// @param` values reach every effect pass: `param(i)`
    /// compiles against the real bind group layout, and the setters accept
    /// packed rows. The contract the studio's sliders stand on.
    #[test]
    fn param_block_reaches_the_effect_passes() {
        let Some(r) = renderer() else { return };
        r.set_background(Some(
            "@fragment fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
                 return vec4<f32>(param(0u), param(1u), param(31u), 1.0);
             }",
        ))
        .expect("a background can read fx_params");
        r.set_window_fx(Some(
            "@fragment fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
                 return vec4<f32>(param(2u), 0.0, 0.0, param(0u));
             }",
        ))
        .expect("a window effect can read fx_params");
        r.set_effect(Some(
            "@fragment fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
                 return textureSample(scene, scene_samp, in.uv) * param(3u);
             }",
        ))
        .expect("a whole-output effect can read fx_params");
        let mut rows = [[0.0f32; 4]; 8];
        rows[0] = [0.5, 1.0, 0.25, 0.0];
        r.set_background_params(rows);
        r.set_window_fx_params(rows);
        r.set_effect_params(rows);
    }

    #[test]
    fn example_shaders_validate() {
        let Some(r) = renderer() else { return };
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/shaders");
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "wgsl") {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            // An empty file is a shader someone is about to write, not a
            // broken one — skip it rather than failing the suite over a
            // placeholder.
            if src.trim().is_empty() {
                continue;
            }
            // Three preambles now, so a shader is checked against the one it
            // is written for. The naming is the contract: `*_update.wgsl` is
            // a particle compute pass, `*_draw.wgsl` a particle draw pass,
            // everything else an fx shader (grader, wallpaper, window fx).
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if name.ends_with("_update.wgsl") || name.ends_with("_diffuse.wgsl") {
                r.set_particle_shaders(Some(&src), None)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                checked += 1;
                continue;
            }
            if name.ends_with("_draw.wgsl") {
                r.set_particle_shaders(None, Some(&src))
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                checked += 1;
                continue;
            }
            let animated =
                r.set_effect(Some(&src)).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            // Only the graders are pinned. They must stay static or the
            // compositor never idles: a grader is meant to sit over a still
            // desktop costing nothing. Wallpapers are expected to move and
            // are chosen deliberately, so listing them here bought nothing
            // and made every new background a two-file change.
            let must_be_static = ["vignette.wgsl", "night.wgsl", "pixel.wgsl"];
            if path.file_name().is_some_and(|n| must_be_static.iter().any(|s| n == *s)) {
                assert!(!animated, "{} must not read time", path.display());
            }
            checked += 1;
        }
        assert!(checked >= 3, "expected the shipped example shaders, found {checked}");
    }

    #[test]
    fn missing_image_paints_placeholder_box() {
        let Some(r) = renderer() else { return };
        let cmds = vec![DrawCommand::Image {
            rect: Rect { x: 0.0, y: 0.0, w: 16.0, h: 16.0 },
            source: "not-loaded".into(),
        }];
        let buf = r.render_to_rgba(&cmds, &NoImageSource, 16, 16, BLACK);
        assert_eq!(px(&buf, 16, 8, 8), [0xC2, 0xC2, 0xD2, 255], "inner placeholder fill");
        // (2,2): inside the outer ring, clear of the corner's ~2px AA band.
        assert_eq!(px(&buf, 16, 2, 2), [0xD8, 0xD8, 0xE2, 255], "outer placeholder ring");
    }
}
