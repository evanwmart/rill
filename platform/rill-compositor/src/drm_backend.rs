//! The bare-metal present path — milestone 15a (docs/bare-metal-plan.md).
//!
//! Rung 15a is deliberately small: one output, no input, no hotplug —
//! open the card, pick the connected connector's preferred mode, put
//! rendered pixels on the glass with a page flip. This module currently
//! delivers the *light-up diagnostic*: run `rill-compositor --backend drm`
//! on a machine whose VT you own and it reports every card, connector and
//! mode it can see, then flips a two-buffer color cycle on the chosen
//! output for a few seconds. That is the whole export → framebuffer →
//! flip path on real hardware; wiring the full compositor loop behind it
//! is the rest of 15a and happens against the reference device.
//!
//! Two decisions inherited from measurement rather than taste:
//!
//! * Buffers are filled through the **copy path** (`write_texture`), not a
//!   render pass. rill-gpu's scanout tests measure both per driver, and
//!   copy-present is the one that works everywhere the import path works —
//!   NVIDIA's driver claims linear render-present and then wedges its
//!   queue (found 2026-09-02, `dmabuf.rs` tests). The real compositor loop
//!   renders offscreen anyway; whether it can skip the final copy is a
//!   per-driver upgrade, decided by the same tests on the target hardware.
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
//! Card access is a direct open of `/dev/dri/card*`. libseat arrives with
//! 15b (input needs the seat anyway); for 15a the process is the only
//! thing on its VT and either owns the render node's group or runs where
//! DRM master falls to it naturally.

use std::os::fd::AsFd;
use std::time::Duration;

use drm::buffer::DrmFourcc;
use drm::control::{Device as ControlDevice, PageFlipFlags, connector, crtc};
use rill_gpu::dmabuf::{DmabufDevice, ScanoutImage};

/// A DRM card: the drm crate's traits over a plain opened device file.
struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
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

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let gpu = DmabufDevice::new()
        .ok_or("no Vulkan adapter with dmabuf export — the metal needs a real driver")?;
    println!("rill-compositor[drm]: wgpu on {}", gpu.adapter_name());

    let card = open_card()?;

    // Degrade-and-wait, from day one: no connected connector is a state to
    // sit out, not a reason to die. Backoff caps at 5s — a kiosk whose
    // screen comes back should light up within a breath of the plug — and
    // the wait honors SIGTERM, because an unkillable service is its own
    // class of appliance bug (found by this module's first smoke run).
    let (conn, mode) = loop {
        match pick_output(&card)? {
            Some(picked) => break picked,
            None => {
                println!("rill-compositor[drm]: no connected connector — waiting");
                for _ in 0..10 {
                    if crate::shutting_down() {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    };
    let (width, height) = (mode.size().0 as u32, mode.size().1 as u32);
    let refresh = mode.vrefresh();
    println!(
        "rill-compositor[drm]: lighting {}-{} at {width}x{height}@{refresh}",
        interface_name(conn.interface()),
        conn.interface_id(),
    );

    let crtc = pick_crtc(&card, &conn)?;

    // Two buffers in rotation — the minimum that neither tears nor stalls
    // (bare-metal-plan.md "Buffer rotation and tearing"). Filled once with
    // tell-them-apart colors; the flip cycle is the proof, not the picture.
    let buffers: Vec<(ScanoutImage, drm::control::framebuffer::Handle)> = [
        [0x20u8, 0x30, 0xc0, 0xff], // BGRA: warm rust red
        [0xc0u8, 0x80, 0x20, 0xff], // BGRA: cold steel blue
    ]
    .iter()
    .map(|color| {
        let img = gpu.alloc_scanout(width, height).map_err(std::io::Error::other)?;
        fill(&gpu, &img, *color);
        let handle = card.prime_fd_to_buffer(img.fd.as_fd())?;
        let fb = card.add_planar_framebuffer(
            &ScanoutFb { plan: &img.plan, handle },
            drm::control::FbCmd2Flags::MODIFIERS,
        )?;
        Ok::<_, Box<dyn std::error::Error>>((img, fb))
    })
    .collect::<Result<_, _>>()?;

    // First frame goes up with a full modeset; every one after is a flip.
    card.set_crtc(crtc, Some(buffers[0].1), (0, 0), &[conn.handle()], Some(mode))?;
    println!("rill-compositor[drm]: modeset up — flipping");

    let seconds: u64 = std::env::var("RILL_DRM_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let flips = seconds * 2;
    for i in 0..flips {
        if crate::shutting_down() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
        let fb = buffers[(i as usize + 1) % 2].1;
        card.page_flip(crtc, fb, PageFlipFlags::EVENT, None)?;
        // Wait for the flip-done event before queueing another — the flip
        // completion bookkeeping the real loop will hang presentation on.
        for event in card.receive_events()? {
            if let drm::control::Event::PageFlip(_) = event {
                break;
            }
        }
    }
    println!("rill-compositor[drm]: {flips} flips clean — 15a light-up holds");
    Ok(())
}

fn open_card() -> Result<Card, Box<dyn std::error::Error>> {
    // Prefer the card whose connector is actually plugged in — on a
    // multi-GPU box the first KMS card is not necessarily the one wired to
    // the monitor (this workstation's discrete card has four dark
    // connectors; the display hangs off the other one). A card with
    // connectors but nothing plugged is kept as the fallback so the
    // degrade-and-wait loop still has something to watch.
    let mut fallback: Option<(String, Card)> = None;
    for n in 0..8 {
        let path = format!("/dev/dri/card{n}");
        let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        let card = Card(file);
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
    let res = card.resource_handles()?;
    for &handle in res.connectors() {
        // force_probe: a cold connector on a just-plugged kiosk screen may
        // not have probed yet, and this path only runs on state changes.
        let Ok(conn) = card.get_connector(handle, true) else { continue };
        if conn.state() != connector::State::Connected {
            continue;
        }
        let mode = conn
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(drm::control::ModeTypeFlags::PREFERRED))
            .or_else(|| conn.modes().first())
            .copied();
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

/// Solid-fill a scanout image through the copy path (see module docs for
/// why copy and not a render pass) and wait until the bytes are really in
/// the buffer — KMS reads memory, not queues.
fn fill(gpu: &DmabufDevice, img: &ScanoutImage, bgra: [u8; 4]) {
    let (w, h) = (img.plan.width, img.plan.height);
    let pixels: Vec<u8> = bgra.iter().copied().cycle().take((w * h * 4) as usize).collect();
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &img.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    let _ = gpu.device.poll(wgpu::PollType::Wait);
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
