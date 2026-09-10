//! The bare-metal backend — milestones 15a/15b (docs/bare-metal-plan.md).
//!
//! 15a (holds on the reference Pi, 2026-09-08): open the card, pick the
//! connected connector's preferred mode, put rendered pixels on the glass
//! with a page flip. 15b: the same process hosts the real desktop — the
//! main loop's frame body is shared with the nested backend and only the
//! seams differ: where input comes from (libinput over udev, devices
//! opened through the seat), what is presented to (two exported scanout
//! images in rotation, flipped with completion tracked), and what happens
//! when the VT walks away (the seat pauses us; we stop flipping and re-set
//! the mode on the way back).
//!
//! Two decisions inherited from measurement rather than taste:
//!
//! * Buffers are filled through the **copy path** (composite offscreen,
//!   copy into the scanout image), not a render pass into it. rill-gpu's
//!   scanout tests measure both per driver, and copy-present is the one
//!   that works everywhere the import path works — NVIDIA's driver claims
//!   linear render-present and then wedges its queue (found 2026-09-02,
//!   `dmabuf.rs` tests). Whether V3DV can skip the copy is a per-driver
//!   upgrade, decided by the same tests on the target hardware.
//! * **No connector is not an error.** A kiosk's screen gets unplugged;
//!   the appliance requirement is degrade-and-wait (TODO.md, filed from
//!   the 2026-08-24 soak launch, where this exact case was a wgpu panic
//!   three layers below the cause). The wait loop with backoff is here
//!   from the first day the backend exists.
//!
//! Legacy modeset + page flip, not atomic, for the first light: every KMS
//! driver speaks it and it needs no property plumbing. Atomic arrives when
//! something needs what it offers (planes, per-frame modifiers, tearing
//! control) — recorded as a step, not skipped by accident.
//!
//! Device access goes through libseat when a seat is there (logind on a
//! real console session, seatd if one runs), and falls back to opening the
//! nodes directly when it is not — which is what a run over SSH on the
//! bench Pi is, and why that run has no VT switching to survive. The log
//! says which. `LIBSEAT_BACKEND=noop` asks libseat for the same direct
//! opens without the fallback message.
//!
//! One ordering rule, load-bearing (found on the Pi with nothing else on
//! the card): **the card is opened before the Vulkan device.** DRM master
//! goes to the first file that opens the primary node while no master
//! exists, and V3DV opens the display card itself when the device comes
//! up — with the Vulkan open first, *its* fd was master and the modeset
//! was refused.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use drm::buffer::DrmFourcc;
use drm::control::{Device as ControlDevice, PageFlipFlags, connector, crtc};
use rill_gpu::dmabuf::{DmabufDevice, ScanoutImage};
use rill_gpu::{Renderer as GpuRenderer, SceneLayer};
use rill_ui::{Color as UiColor, DrawCommand, Rect as UiRect};

/// The scanout format: BGRA, non-sRGB — the same the winit path picks, and
/// what `alloc_scanout` exports. The renderer's pipelines bake it in, so it
/// must match the framebuffer's fourcc (ARGB8888).
pub const SCANOUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

/// A device node, however it was opened: through the seat (libseat owns the
/// fd and revokes it on VT switch) or directly (no seat available).
enum Node {
    Seat(libseat::Device),
    Direct(std::fs::File),
}

impl AsFd for Node {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Node::Seat(d) => d.as_fd(),
            Node::Direct(f) => f.as_fd(),
        }
    }
}

/// A DRM card: the drm crate's traits over an opened primary node.
struct Card(Node);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl ControlDevice for Card {}

/// The scanout image's plan, spoken in the drm crate's vocabulary so
/// `add_planar_framebuffer` can consume it (single plane, linear).
struct ScanoutFb<'a> {
    plan: &'a rill_gpu::dmabuf::DmabufPlan,
    handle: drm::buffer::Handle,
}

impl drm::buffer::PlanarBuffer for ScanoutFb<'_> {
    fn size(&self) -> (u32, u32) {
        (self.plan.width, self.plan.height)
    }
    fn format(&self) -> DrmFourcc {
        DrmFourcc::Argb8888
    }
    fn modifier(&self) -> Option<drm::buffer::DrmModifier> {
        Some(drm::buffer::DrmModifier::Linear)
    }
    fn pitches(&self) -> [u32; 4] {
        [self.plan.stride as u32, 0, 0, 0]
    }
    fn handles(&self) -> [Option<drm::buffer::Handle>; 4] {
        [Some(self.handle), None, None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [self.plan.offset as u32, 0, 0, 0]
    }
}

/// Where device nodes come from. `Seat` is libseat (logind, seatd, or its
/// builtin/noop backends by `LIBSEAT_BACKEND`); `Direct` is a plain open,
/// for a process with group access to the nodes and no seat — an SSH run
/// on the bench. Only a seat can pause us.
enum Session {
    Seat { seat: Rc<RefCell<libseat::Seat>>, active: Arc<AtomicBool> },
    Direct,
}

impl Session {
    fn open() -> Session {
        let active = Arc::new(AtomicBool::new(false));
        let flag = active.clone();
        match libseat::Seat::open(move |seat, event| match event {
            libseat::SeatEvent::Enable => {
                println!("rill-compositor[drm]: seat enabled");
                flag.store(true, Ordering::SeqCst);
            }
            libseat::SeatEvent::Disable => {
                // The VT is walking away. Acknowledge at once — logind
                // revokes our device fds either way, and a compositor that
                // does not answer holds the switch up for its timeout.
                println!("rill-compositor[drm]: seat disabled — pausing");
                flag.store(false, Ordering::SeqCst);
                let _ = seat.disable();
            }
        }) {
            Ok(mut seat) => {
                // The enable event arrives on the first dispatch, not on
                // open; drain until it does so devices can be opened.
                for _ in 0..50 {
                    if active.load(Ordering::SeqCst) {
                        break;
                    }
                    let _ = seat.dispatch(100);
                }
                if !active.load(Ordering::SeqCst) {
                    // A seat that never enables is a session that is not
                    // the active one on any seat — an SSH login, on the
                    // bench. Its device opens would all be refused, so say
                    // so once and open directly instead.
                    println!(
                        "rill-compositor[drm]: seat {} never became active (not a console session) \
                         — opening devices directly; no VT switching on this run",
                        seat.name()
                    );
                    return Session::Direct;
                }
                println!("rill-compositor[drm]: seat {}", seat.name());
                Session::Seat { seat: Rc::new(RefCell::new(seat)), active }
            }
            Err(e) => {
                println!(
                    "rill-compositor[drm]: no seat ({e}) — opening devices directly; \
                     no VT switching on this run"
                );
                Session::Direct
            }
        }
    }

    fn open_node(&self, path: &str) -> std::io::Result<Node> {
        match self {
            Session::Seat { seat, .. } => seat
                .borrow_mut()
                .open_device(&path)
                .map(Node::Seat)
                .map_err(|e| std::io::Error::from_raw_os_error(i32::from(e))),
            Session::Direct => {
                std::fs::OpenOptions::new().read(true).write(true).open(path).map(Node::Direct)
            }
        }
    }

    /// Whether the seat is ours right now. Always true without a seat.
    fn active(&self) -> bool {
        match self {
            Session::Seat { active, .. } => active.load(Ordering::SeqCst),
            Session::Direct => true,
        }
    }

    fn fd(&self) -> Option<RawFd> {
        match self {
            Session::Seat { seat, .. } => seat.borrow_mut().get_fd().ok().map(|f| f.as_raw_fd()),
            Session::Direct => None,
        }
    }

    fn dispatch(&self) {
        if let Session::Seat { seat, .. } = self {
            let _ = seat.borrow_mut().dispatch(0);
        }
    }
}

/// libinput's device opener, routed through the seat when there is one.
/// libinput wants an `OwnedFd` it will hand back to `close_restricted`;
/// libseat's `Device` owns the real fd and must be closed through the seat,
/// so libinput gets a dup and the pair is kept by the dup's number.
struct Opener {
    session: Rc<Session>,
    held: HashMap<RawFd, libseat::Device>,
}

impl input::LibinputInterface for Opener {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        match &*self.session {
            Session::Seat { seat, .. } => {
                let device = seat
                    .borrow_mut()
                    .open_device(&path)
                    .map_err(|e| -i32::from(e))?;
                let dup = device.as_fd().try_clone_to_owned().map_err(|e| -e.raw_os_error().unwrap_or(libc::EIO))?;
                self.held.insert(dup.as_raw_fd(), device);
                Ok(dup)
            }
            Session::Direct => {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .read(true)
                    .write((flags & libc::O_RDWR) != 0 || (flags & libc::O_WRONLY) != 0)
                    .custom_flags(flags & !libc::O_ACCMODE)
                    .open(path)
                    .map(OwnedFd::from)
                    .map_err(|e| -e.raw_os_error().unwrap_or(libc::EIO))
            }
        }
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        if let Some(device) = self.held.remove(&fd.as_raw_fd())
            && let Session::Seat { seat, .. } = &*self.session
        {
            let _ = seat.borrow_mut().close_device(device);
        }
        drop(fd);
    }
}

/// Input on the metal: libinput over udev on the seat, dispatched by the
/// main loop's poll. Events come out as libinput's own types; the
/// translation into seat calls lives next to the winit translation in
/// main.rs, so the two are read side by side.
pub struct MetalInput {
    libinput: input::Libinput,
}

impl MetalInput {
    fn new(session: Rc<Session>) -> Result<MetalInput, Box<dyn std::error::Error>> {
        let mut libinput = input::Libinput::new_with_udev(Opener { session, held: HashMap::new() });
        libinput.udev_assign_seat("seat0").map_err(|()| "libinput: udev_assign_seat(seat0) failed")?;
        Ok(MetalInput { libinput })
    }

    /// Wait up to `timeout` for input (or a seat event), then hand back
    /// everything libinput has. The wait is the loop's idle pacing — the
    /// same role winit's `pump_app_events` timeout plays nested.
    fn pump(&mut self, session: &Session, timeout: Duration) -> Vec<input::Event> {
        let mut fds = vec![libc::pollfd { fd: self.libinput.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        if let Some(fd) = session.fd() {
            fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
        }
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: `fds` is a valid array for the call's duration.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        session.dispatch();
        let _ = self.libinput.dispatch();
        (&mut self.libinput).collect()
    }
}

/// One output on the metal: the connector, its mode, and two scanout
/// images in rotation. `front` is on the glass; the other is drawn into.
pub struct DrmOutput {
    card: Card,
    crtc: crtc::Handle,
    conn: connector::Info,
    mode: drm::control::Mode,
    offscreen: wgpu::Texture,
    buffers: Vec<(ScanoutImage, drm::control::framebuffer::Handle)>,
    front: usize,
    modeset_done: bool,
    pending_flip: bool,
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

impl DrmOutput {
    /// The view to composite this frame into. Blocks on the previous
    /// flip's completion first, so the buffer about to be drawn is off
    /// the glass — the two-buffer minimum that neither tears nor stalls.
    pub fn acquire(&mut self) -> wgpu::TextureView {
        self.wait_flip();
        self.offscreen.create_view(&Default::default())
    }

    /// Copy the composed frame into the off-screen buffer and put it up:
    /// a full modeset the first time, a page flip after.
    pub fn present(&mut self, gpu: &DmabufDevice) -> Result<(), Box<dyn std::error::Error>> {
        let back = (self.front + 1) % self.buffers.len();
        let (img, fb) = &self.buffers[back];
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.offscreen,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &img.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
        gpu.queue.submit([encoder.finish()]);
        // KMS reads memory, not queues: the bytes must be in the buffer
        // before the flip is queued.
        let _ = gpu.device.poll(wgpu::PollType::Wait);
        if !self.modeset_done {
            self.card.set_crtc(self.crtc, Some(*fb), (0, 0), &[self.conn.handle()], Some(self.mode))?;
            self.modeset_done = true;
        } else {
            self.card.page_flip(self.crtc, *fb, PageFlipFlags::EVENT, None)?;
            self.pending_flip = true;
        }
        self.front = back;
        Ok(())
    }

    /// The last composed frame, read back as RGBA8 — what the glass shows,
    /// for a screen nobody can see (the bench Pi's forced output has no
    /// panel; a kiosk in the field has no one standing at it). Costs a
    /// readback; only on request (SIGUSR2 in main).
    pub fn screenshot(&self, gpu: &DmabufDevice) -> Result<Vec<u8>, String> {
        let (w, h) = (self.width, self.height);
        let row = (w * 4).div_ceil(256) * 256;
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("drm-shot"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.offscreen,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        gpu.queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = gpu.device.poll(wgpu::PollType::Wait);
        rx.recv().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            let line = &data[y * row as usize..y * row as usize + (w * 4) as usize];
            for px in line.chunks_exact(4) {
                out.extend_from_slice(&[px[2], px[1], px[0], 0xff]);
            }
        }
        Ok(out)
    }

    fn wait_flip(&mut self) {
        if !self.pending_flip {
            return;
        }
        if let Ok(events) = self.card.receive_events() {
            for event in events {
                if let drm::control::Event::PageFlip(_) = event {
                    break;
                }
            }
        }
        self.pending_flip = false;
    }

    /// Coming back from a VT switch: the mode is gone with the master, so
    /// the next present is a modeset again, showing what was last drawn.
    pub fn resume(&mut self) {
        self.pending_flip = false;
        self.modeset_done = false;
        // `present` puts up the *back* buffer; point it at what is current.
        self.front = (self.front + self.buffers.len() - 1) % self.buffers.len();
    }
}

/// Everything the main loop needs from the metal, opened in the order that
/// works: seat, card (master), GPU, scanout buffers, input.
pub struct Metal {
    session: Rc<Session>,
    pub output: DrmOutput,
    pub input: MetalInput,
}

impl Metal {
    /// The GPU device comes back beside the metal rather than inside it:
    /// the main loop owns the device for both backends.
    pub fn open() -> Result<(Metal, DmabufDevice), Box<dyn std::error::Error>> {
        let session = Rc::new(Session::open());
        let card = open_card(&session)?;
        match drm::Device::acquire_master_lock(&card) {
            Ok(()) => println!("rill-compositor[drm]: DRM master acquired"),
            Err(e) => println!("rill-compositor[drm]: DRM master not acquired explicitly ({e}) — relying on first-open"),
        }
        let gpu = DmabufDevice::new()
            .ok_or("no Vulkan adapter with dmabuf export — the metal needs a real driver")?;
        println!("rill-compositor[drm]: wgpu on {}", gpu.adapter_name());

        let (conn, mode) = wait_for_output(&card)?;
        let (width, height) = (mode.size().0 as u32, mode.size().1 as u32);
        let refresh_mhz = mode.vrefresh() * 1000;
        println!(
            "rill-compositor[drm]: lighting {}-{} at {width}x{height}@{}",
            interface_name(conn.interface()),
            conn.interface_id(),
            mode.vrefresh(),
        );
        let crtc = pick_crtc(&card, &conn)?;
        let offscreen = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("drm-compose"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: SCANOUT_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let buffers = import_scanout_pair(&card, &gpu, width, height)?;
        let input = MetalInput::new(session.clone())?;
        Ok((Metal {
            session,
            output: DrmOutput {
                card,
                crtc,
                conn,
                mode,
                offscreen,
                buffers,
                front: 0,
                modeset_done: false,
                pending_flip: false,
                width,
                height,
                refresh_mhz,
            },
            input,
        }, gpu))
    }

    /// Whether the seat is ours: false while another VT has the console.
    pub fn active(&self) -> bool {
        self.session.active()
    }

    pub fn pump(&mut self, timeout: Duration) -> Vec<input::Event> {
        self.input.pump(&self.session, timeout)
    }
}

/// Degrade-and-wait, from day one: no connected connector is a state to
/// sit out, not a reason to die. Backoff caps at 5s — a kiosk whose screen
/// comes back should light up within a breath of the plug — and the wait
/// honors SIGTERM, because an unkillable service is its own class of
/// appliance bug (found by this module's first smoke run).
fn wait_for_output(
    card: &Card,
) -> Result<(connector::Info, drm::control::Mode), Box<dyn std::error::Error>> {
    loop {
        match pick_output(card)? {
            Some(picked) => return Ok(picked),
            None => {
                println!("rill-compositor[drm]: no connected connector — waiting");
                for _ in 0..10 {
                    if crate::shutting_down() {
                        return Err("shut down while waiting for a display".into());
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }
}

/// Two scanout images, imported as framebuffers. Each stage on its own
/// line: bring-up on the Pi stopped at a bare EACCES once, with no way to
/// tell an import failure from the modeset refusing a non-master.
fn import_scanout_pair(
    card: &Card,
    gpu: &DmabufDevice,
    width: u32,
    height: u32,
) -> Result<Vec<(ScanoutImage, drm::control::framebuffer::Handle)>, Box<dyn std::error::Error>> {
    (0..2)
        .map(|i| {
            let img = gpu.alloc_scanout(width, height).map_err(std::io::Error::other)?;
            let handle = card.prime_fd_to_buffer(img.fd.as_fd())?;
            let fb = card.add_planar_framebuffer(
                &ScanoutFb { plan: &img.plan, handle },
                drm::control::FbCmd2Flags::MODIFIERS,
            )?;
            println!(
                "rill-compositor[drm]:   scanout buffer {i} imported as fb {:?} (modifier {:#x})",
                fb, img.plan.modifier
            );
            Ok::<_, Box<dyn std::error::Error>>((img, fb))
        })
        .collect()
}

/// The 15a light-up diagnostic (`--backend drm-lightup`): one output, no
/// clients, a rendered card flipped between two buffers for a few seconds.
/// Kept as the bring-up report for a new board — it exercises every stage
/// the hosted path does, with nothing else in the way.
pub fn light_up() -> Result<(), Box<dyn std::error::Error>> {
    let (mut metal, gpu) = Metal::open()?;
    let renderer =
        GpuRenderer::with_device(gpu.device.clone(), gpu.queue.clone(), SCANOUT_FORMAT, gpu.adapter_name());
    let seconds: u64 = std::env::var("RILL_DRM_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let flips = seconds * 2;
    for i in 0..=flips {
        if crate::shutting_down() {
            break;
        }
        let view = metal.output.acquire();
        render_card(&renderer, &view, metal.output.width, metal.output.height, (i % 2) as f32);
        metal.output.present(&gpu).map_err(|e| {
            if e.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied) {
                "modeset refused: this process is not DRM master — another compositor \
                 (a desktop session) owns the card; stop it, or run from a VT this process owns".into()
            } else {
                e
            }
        })?;
        if i == 0 {
            println!("rill-compositor[drm]: modeset up — flipping");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("rill-compositor[drm]: {flips} flips clean — 15a light-up holds");
    Ok(())
}

fn open_card(session: &Session) -> Result<Card, Box<dyn std::error::Error>> {
    // Prefer the card whose connector is actually plugged in — on a
    // multi-GPU box the first KMS card is not necessarily the one wired to
    // the monitor (this workstation's discrete card has four dark
    // connectors; the display hangs off the other one). A card with
    // connectors but nothing plugged is kept as the fallback so the
    // degrade-and-wait loop still has something to watch.
    let mut fallback: Option<(String, Card)> = None;
    for n in 0..8 {
        let path = format!("/dev/dri/card{n}");
        let node = match session.open_node(&path) {
            Ok(node) => node,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                println!("rill-compositor[drm]: {path}: {e}");
                continue;
            }
        };
        let card = Card(node);
        let Ok(res) = card.resource_handles() else { continue };
        // A card with no connectors (a render-only node) is not a display.
        if res.connectors().is_empty() {
            continue;
        }
        let connected = res.connectors().iter().any(|&h| {
            card.get_connector(h, false)
                .map(|c| c.state() == connector::State::Connected)
                .unwrap_or(false)
        });
        if connected {
            println!("rill-compositor[drm]: {path}");
            report(&card);
            return Ok(card);
        }
        if fallback.is_none() {
            fallback = Some((path, card));
        }
    }
    if let Some((path, card)) = fallback {
        println!("rill-compositor[drm]: {path} (no card has a connected connector)");
        report(&card);
        return Ok(card);
    }
    Err("no KMS-capable /dev/dri/card* (permissions, or no display hardware)".into())
}

/// Print every connector and its modes — the diagnostic half of this
/// backend. On a new board this output *is* the bring-up report.
fn report(card: &Card) {
    let Ok(res) = card.resource_handles() else { return };
    for &handle in res.connectors() {
        let Ok(conn) = card.get_connector(handle, false) else { continue };
        let state = match conn.state() {
            connector::State::Connected => "connected",
            connector::State::Disconnected => "disconnected",
            connector::State::Unknown => "unknown",
        };
        println!(
            "rill-compositor[drm]:   {}-{} {state}, {} modes",
            interface_name(conn.interface()),
            conn.interface_id(),
            conn.modes().len(),
        );
        for mode in conn.modes().iter().take(4) {
            println!(
                "rill-compositor[drm]:     {}x{}@{}{}",
                mode.size().0,
                mode.size().1,
                mode.vrefresh(),
                if mode.mode_type().contains(drm::control::ModeTypeFlags::PREFERRED) {
                    " preferred"
                } else {
                    ""
                },
            );
        }
    }
}

/// First connected connector, preferred mode (falling back to its first).
/// One output, no hotplug — 15a's scope, kept deliberately.
fn pick_output(
    card: &Card,
) -> Result<Option<(connector::Info, drm::control::Mode)>, Box<dyn std::error::Error>> {
    // `RILL_DRM_MODE=WIDTHxHEIGHT` overrides the mode choice — a Pi's V3D
    // shades every pixel of an animated background every frame, so 4K
    // native (the EDID's preferred mode) can be far heavier than the
    // appliance wants; this caps it without a cmdline edit. Unset, the
    // connector's preferred mode is used.
    let want = std::env::var("RILL_DRM_MODE").ok().and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.trim().parse::<u16>().ok()?, h.trim().parse::<u16>().ok()?))
    });
    let res = card.resource_handles()?;
    for &handle in res.connectors() {
        // force_probe: a cold connector on a just-plugged kiosk screen may
        // not have probed yet, and this path only runs on state changes.
        let Ok(conn) = card.get_connector(handle, true) else { continue };
        if conn.state() != connector::State::Connected {
            continue;
        }
        let mode = want
            .and_then(|(w, h)| conn.modes().iter().find(|m| m.size() == (w, h)).copied())
            .or_else(|| {
                conn.modes()
                    .iter()
                    .find(|m| m.mode_type().contains(drm::control::ModeTypeFlags::PREFERRED))
                    .or_else(|| conn.modes().first())
                    .copied()
            });
        if let Some(mode) = mode {
            return Ok(Some((conn, mode)));
        }
    }
    Ok(None)
}

/// A CRTC that can drive this connector: its current one when live, else
/// the first the encoder allows.
fn pick_crtc(card: &Card, conn: &connector::Info) -> Result<crtc::Handle, Box<dyn std::error::Error>> {
    let res = card.resource_handles()?;
    if let Some(enc_handle) = conn.current_encoder()
        && let Ok(enc) = card.get_encoder(enc_handle)
        && let Some(crtc) = enc.crtc()
    {
        return Ok(crtc);
    }
    for &enc_handle in conn.encoders() {
        if let Ok(enc) = card.get_encoder(enc_handle) {
            let possible = enc.possible_crtcs();
            if let Some(&crtc) = res.filter_crtcs(possible).first() {
                return Ok(crtc);
            }
        }
    }
    Err("no CRTC available for the connected connector".into())
}

/// The light-up scene: a rendered card standing in for the wallpaper the
/// plan's demo names, drawn through the exact `composite_scene` path the
/// hosted desktop uses. `phase` (0.0 or 1.0) nudges the accent bar so the
/// two rotation buffers differ and the flip reads as live.
fn render_card(renderer: &GpuRenderer, view: &wgpu::TextureView, w: u32, h: u32, phase: f32) {
    let bg = UiColor { r: 0x14, g: 0x16, b: 0x1c, a: 0xff };
    let card = UiColor { r: 0x20, g: 0x24, b: 0x2e, a: 0xff };
    let accent = UiColor { r: 0x4c, g: 0x6e, b: 0xf5, a: 0xff };

    let cw = (w as f32 * 0.5).min(640.0);
    let ch = (h as f32 * 0.42).min(360.0);
    let cx = (w as f32 - cw) / 2.0;
    let cy = (h as f32 - ch) / 2.0;
    let bar_w = cw - 96.0;
    let travel = 24.0;
    let cmds = vec![
        DrawCommand::Rect {
            rect: UiRect { x: cx, y: cy, w: cw, h: ch },
            color: card,
            corner_radius: 28.0,
        },
        DrawCommand::Rect {
            rect: UiRect {
                x: cx + 48.0 + phase * travel,
                y: cy + ch - 72.0,
                w: bar_w - phase * travel,
                h: 18.0,
            },
            color: accent,
            corner_radius: 9.0,
        },
    ];
    renderer.composite_scene(view, w, h, bg, &[SceneLayer::commands(&cmds)], rill_gpu::FxInputs::default());
}

fn interface_name(interface: connector::Interface) -> &'static str {
    match interface {
        connector::Interface::HDMIA => "HDMI-A",
        connector::Interface::HDMIB => "HDMI-B",
        connector::Interface::DisplayPort => "DP",
        connector::Interface::EmbeddedDisplayPort => "eDP",
        connector::Interface::DSI => "DSI",
        connector::Interface::DVII => "DVI-I",
        connector::Interface::DVID => "DVI-D",
        connector::Interface::VGA => "VGA",
        connector::Interface::LVDS => "LVDS",
        _ => "connector",
    }
}
