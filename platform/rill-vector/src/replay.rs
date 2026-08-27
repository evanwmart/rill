//! Session replay: a recorded desktop played back *as vectors*, inside an
//! ordinary vector window.
//!
//! This is the other half of the recording arc, and the reason the format is
//! semantic rather than a video. Playback rebuilds the desktop's state at time
//! t — which windows exist, where they are, how they stack, what each one last
//! drew — and re-renders it at whatever size this window happens to be. The
//! recorded frames are the client's own command streams, so a replay of a
//! 1280x800 session shown in a 600px window is not a scaled image: it is the
//! same drawing commands, re-rasterized. Text stays text at every size.
//!
//! Seeking backwards replays from the start rather than keeping snapshots. A
//! session is thousands of events, not millions, and re-applying them is
//! cheaper than the bookkeeping — the frame blobs are decoded once as they are
//! applied, not once per drawn frame.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use rill_ui::recording::{RecEvent, Stamped, decode_lossy};
use rill_ui::{Color, DrawCommand, Rect};

const BAR: f32 = 44.0;
const BG: Color = Color { r: 12, g: 14, b: 22, a: 255 };
const STAGE: Color = Color { r: 20, g: 23, b: 34, a: 255 };
const CHROME: Color = Color { r: 26, g: 30, b: 44, a: 255 };
const TRACK: Color = Color { r: 48, g: 54, b: 74, a: 255 };
const INK: Color = Color { r: 226, g: 232, b: 245, a: 255 };
const DIM: Color = Color { r: 138, g: 148, b: 170, a: 255 };
const ACCENT: Color = Color { r: 138, g: 180, b: 255, a: 255 };
const PIXEL: Color = Color { r: 40, g: 44, b: 60, a: 255 };

/// One window as the recording last described it.
struct Window {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    title: String,
    vector: bool,
    /// The latest decoded frame. Decoded when the event is applied, so drawing
    /// is just a transform.
    frame: Option<Vec<DrawCommand>>,
}

pub struct Replay {
    /// The recorded output size — the coordinate space every event speaks in.
    out_w: f32,
    out_h: f32,
    events: Vec<Stamped>,
    duration_ms: u32,
    /// Index of the next event to apply.
    next: usize,
    t_ms: u32,
    playing: bool,
    /// Wall-clock anchor: `t_ms` at the moment playback last started.
    anchor: Option<(Instant, u32)>,
    windows: HashMap<u32, Window>,
    order: Vec<u32>,
    pointer: Option<(f32, f32)>,
    /// Set when the file ended mid-event — a session that was killed rather
    /// than stopped. Shown rather than hidden: the viewer should know the
    /// tail is missing.
    truncated: Option<String>,
    name: String,
}

impl Replay {
    pub fn open(path: &Path) -> Result<Replay, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (out_w, out_h, events, truncated) =
            decode_lossy(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let duration_ms = events.last().map(|e| e.t_ms).unwrap_or(0);
        Ok(Replay {
            out_w: out_w as f32,
            out_h: out_h as f32,
            events,
            duration_ms,
            next: 0,
            t_ms: 0,
            playing: true,
            anchor: None,
            windows: HashMap::new(),
            order: Vec::new(),
            pointer: None,
            truncated,
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "recording".into()),
        })
    }

    pub fn title(&self) -> String {
        format!("Rill — Replay — {}", self.name)
    }

    /// Advance to wall-clock now. Returns whether anything changed, so an
    /// idle (paused, or finished) replay costs nothing.
    pub fn advance(&mut self) -> bool {
        if !self.playing {
            return false;
        }
        let Some((at, base)) = self.anchor else {
            self.anchor = Some((Instant::now(), self.t_ms));
            return false;
        };
        let target = base.saturating_add(at.elapsed().as_millis().min(u32::MAX as u128) as u32);
        if target == self.t_ms {
            return false;
        }
        self.t_ms = target.min(self.duration_ms);
        let changed = self.apply_until(self.t_ms);
        if self.t_ms >= self.duration_ms {
            // Hold on the last frame rather than looping — a replay that
            // restarts on its own is hard to look at.
            self.playing = false;
            self.anchor = None;
            return true;
        }
        changed
    }

    /// Apply every event at or before `t`. Returns whether any landed.
    fn apply_until(&mut self, t: u32) -> bool {
        let mut applied = false;
        while self.next < self.events.len() && self.events[self.next].t_ms <= t {
            let event = self.events[self.next].event.clone();
            self.apply(event);
            self.next += 1;
            applied = true;
        }
        applied
    }

    fn apply(&mut self, event: RecEvent) {
        match event {
            RecEvent::Window { id, x, y, w, h, title, vector } => {
                let entry = self.windows.entry(id).or_insert(Window {
                    x,
                    y,
                    w,
                    h,
                    title: title.clone(),
                    vector,
                    frame: None,
                });
                entry.x = x;
                entry.y = y;
                entry.w = w;
                entry.h = h;
                entry.title = title;
                entry.vector = vector;
            }
            RecEvent::Closed { id } => {
                self.windows.remove(&id);
                self.order.retain(|o| *o != id);
            }
            RecEvent::Order { ids } => self.order = ids,
            RecEvent::Frame { id, bytes } => {
                // Decode once, here — not once per drawn frame. A malformed
                // blob drops that frame and leaves the previous one up rather
                // than failing the whole replay.
                if let Some(window) = self.windows.get_mut(&id)
                    && let Ok(commands) = rill_ui::stream::decode(&bytes)
                {
                    window.frame = Some(commands);
                }
            }
            RecEvent::Pointer { x, y } => self.pointer = Some((x, y)),
        }
    }

    fn rewind(&mut self) {
        self.next = 0;
        self.windows.clear();
        self.order.clear();
        self.pointer = None;
    }

    pub fn toggle(&mut self) {
        if self.t_ms >= self.duration_ms {
            self.restart();
            return;
        }
        self.playing = !self.playing;
        self.anchor = self.playing.then(|| (Instant::now(), self.t_ms));
    }

    pub fn restart(&mut self) {
        self.rewind();
        self.t_ms = 0;
        self.playing = true;
        self.anchor = Some((Instant::now(), 0));
    }

    /// Jump by `delta_ms`, clamped to the recording. Backwards means
    /// replaying from the start — see the module note.
    pub fn seek(&mut self, delta_ms: i64) {
        let target = (self.t_ms as i64 + delta_ms).clamp(0, self.duration_ms as i64) as u32;
        if target < self.t_ms {
            self.rewind();
        }
        self.t_ms = target;
        self.apply_until(target);
        self.anchor = self.playing.then(|| (Instant::now(), self.t_ms));
    }

    /// Seek to a fraction of the duration — the scrub bar.
    pub fn seek_fraction(&mut self, f: f32) {
        let target = (self.duration_ms as f32 * f.clamp(0.0, 1.0)) as u32;
        self.seek(target as i64 - self.t_ms as i64);
    }

    /// Where the scrub bar lives, in window coordinates.
    pub fn scrub_rect(&self, w: f32, h: f32) -> Rect {
        Rect { x: 12.0, y: h - BAR + 26.0, w: w - 24.0, h: 10.0 }
    }

    /// Build the frame: the recorded desktop scaled to fit, plus controls.
    pub fn draw(&self, w: f32, h: f32) -> Vec<DrawCommand> {
        let mut out = vec![DrawCommand::Rect {
            rect: Rect { x: 0.0, y: 0.0, w, h },
            color: BG,
            corner_radius: 0.0,
        }];

        // Fit the recorded output into the space above the control bar,
        // preserving aspect so geometry stays truthful.
        let stage_h = (h - BAR).max(1.0);
        let scale = (w / self.out_w).min(stage_h / self.out_h).max(0.01);
        let (sw, sh) = (self.out_w * scale, self.out_h * scale);
        let ox = (w - sw) / 2.0;
        let oy = (stage_h - sh) / 2.0;

        out.push(DrawCommand::Rect {
            rect: Rect { x: ox, y: oy, w: sw, h: sh },
            color: STAGE,
            corner_radius: 0.0,
        });
        out.push(DrawCommand::PushClip { rect: Rect { x: ox, y: oy, w: sw, h: sh }, radius: 0.0 });

        // Bottom → top. Windows the order never mentioned still draw, so a
        // recording that lost its Order events is not a blank screen.
        let mut painted: Vec<u32> = Vec::new();
        for id in self.order.iter().chain(self.windows.keys()) {
            if painted.contains(id) {
                continue;
            }
            painted.push(*id);
            let Some(window) = self.windows.get(id) else { continue };
            self.paint_window(&mut out, window, scale, ox, oy);
        }

        if let Some((px, py)) = self.pointer {
            out.push(DrawCommand::Path {
                points: vec![rill_ui::Point::new(ox + px * scale, oy + py * scale)],
                color: INK,
                width: 7.0,
                closed: false,
            });
        }
        out.push(DrawCommand::PopClip);

        self.controls(&mut out, w, h);
        out
    }

    fn paint_window(
        &self,
        out: &mut Vec<DrawCommand>,
        window: &Window,
        scale: f32,
        ox: f32,
        oy: f32,
    ) {
        let rect = Rect {
            x: ox + window.x as f32 * scale,
            y: oy + window.y as f32 * scale,
            w: window.w as f32 * scale,
            h: window.h as f32 * scale,
        };
        if rect.w <= 0.0 || rect.h <= 0.0 {
            return;
        }

        match &window.frame {
            Some(commands) => {
                // Window-local commands → recorded output space → replay
                // space. Clipped to the window, exactly as the compositor
                // does, so a frame cannot paint outside its own bounds.
                out.push(DrawCommand::PushClip { rect, radius: 0.0 });
                let placed = rill_ui::stream::offset_commands(
                    &rill_ui::stream::scale_commands(commands, scale),
                    rect.x,
                    rect.y,
                );
                out.extend(placed);
                out.push(DrawCommand::PopClip);
            }
            None => {
                // A pixel window, or a vector window whose first frame has
                // not landed yet. Its content was never recorded — say so
                // rather than drawing an empty box that looks like a bug.
                out.push(DrawCommand::Rect { rect, color: PIXEL, corner_radius: 2.0 });
                let label = if window.vector { &window.title } else { "pixel window" };
                out.push(text(
                    Rect { x: rect.x + 8.0, y: rect.y + 6.0, w: (rect.w - 16.0).max(8.0), h: 16.0 },
                    label,
                    DIM,
                    11.0,
                    600,
                ));
            }
        }
    }

    fn controls(&self, out: &mut Vec<DrawCommand>, w: f32, h: f32) {
        let bar = Rect { x: 0.0, y: h - BAR, w, h: BAR };
        out.push(DrawCommand::Rect { rect: bar, color: CHROME, corner_radius: 0.0 });

        let state = if self.playing {
            "playing"
        } else if self.t_ms >= self.duration_ms {
            "ended"
        } else {
            "paused"
        };
        out.push(text(
            Rect { x: 12.0, y: h - BAR + 7.0, w: w * 0.4, h: 16.0 },
            &format!("{state}   {}  /  {}", clock(self.t_ms), clock(self.duration_ms)),
            INK,
            11.0,
            600,
        ));

        let hint = match &self.truncated {
            Some(_) => "space play/pause · r restart · ← → 5s · recording ends mid-event",
            None => "space play/pause · r restart · ← → 5s · click to scrub",
        };
        out.push(text(
            Rect { x: w * 0.42, y: h - BAR + 7.0, w: w * 0.58 - 12.0, h: 16.0 },
            hint,
            DIM,
            11.0,
            400,
        ));

        let track = self.scrub_rect(w, h);
        out.push(DrawCommand::Rect { rect: track, color: TRACK, corner_radius: 5.0 });
        let played = if self.duration_ms > 0 {
            self.t_ms as f32 / self.duration_ms as f32
        } else {
            0.0
        };
        if played > 0.0 {
            out.push(DrawCommand::Rect {
                rect: Rect { w: track.w * played, ..track },
                color: ACCENT,
                corner_radius: 5.0,
            });
        }
    }
}

fn text(rect: Rect, body: &str, color: Color, size: f32, weight: u16) -> DrawCommand {
    DrawCommand::Text {
        rect,
        text: body.to_string(),
        color,
        font_size: size,
        font_weight: weight,
        font_family: "sans-serif".into(),
    }
}

fn clock(ms: u32) -> String {
    let total = ms / 1000;
    format!("{}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rill_ui::recording::{write_event, write_header};

    fn frame(color: u8) -> Vec<u8> {
        rill_ui::stream::encode(&[DrawCommand::Rect {
            rect: Rect { x: 0.0, y: 0.0, w: 40.0, h: 20.0 },
            color: Color { r: color, g: 0, b: 0, a: 255 },
            corner_radius: 0.0,
        }])
        .unwrap()
    }

    /// Tests run in parallel, so each needs its own file — one of them
    /// truncates its fixture, and a shared path would corrupt the others.
    fn recording(name: &str, events: &[Stamped]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rillrec-replay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.rillrec"));
        let mut buf = Vec::new();
        write_header(&mut buf, 1280, 800).unwrap();
        for e in events {
            write_event(&mut buf, e).unwrap();
        }
        std::fs::write(&path, buf).unwrap();
        path
    }

    fn sample() -> Vec<Stamped> {
        vec![
            Stamped {
                t_ms: 0,
                event: RecEvent::Window {
                    id: 1,
                    x: 10,
                    y: 20,
                    w: 400,
                    h: 300,
                    title: "One".into(),
                    vector: true,
                },
            },
            Stamped { t_ms: 0, event: RecEvent::Order { ids: vec![1] } },
            Stamped { t_ms: 100, event: RecEvent::Frame { id: 1, bytes: frame(1) } },
            Stamped { t_ms: 500, event: RecEvent::Frame { id: 1, bytes: frame(2) } },
            Stamped { t_ms: 900, event: RecEvent::Closed { id: 1 } },
        ]
    }

    /// State at time t is what the recording said at time t — the property
    /// the whole format exists for.
    #[test]
    fn seeking_reconstructs_state_at_that_time() {
        let mut r = Replay::open(&recording("seek", &sample())).unwrap();
        r.playing = false;

        r.seek(100);
        assert_eq!(r.windows.len(), 1, "window is open at 100ms");
        assert!(r.windows[&1].frame.is_some(), "first frame landed");

        r.seek(800); // -> 900ms, the close
        assert!(r.windows.is_empty(), "window closed by 900ms");
        assert!(r.order.is_empty(), "closing drops it from the order too");

        // Seeking backwards replays from the start rather than leaving the
        // closed state behind — the case a naive forward-only player gets
        // wrong.
        r.seek(-800);
        assert_eq!(r.windows.len(), 1, "window is open again at 100ms");
    }

    /// Frames are decoded when applied, so the window holds commands rather
    /// than bytes, and the newest frame wins.
    #[test]
    fn the_latest_frame_wins() {
        let mut r = Replay::open(&recording("latest", &sample())).unwrap();
        r.playing = false;
        r.seek(600);
        let commands = r.windows[&1].frame.as_ref().expect("a frame");
        match &commands[0] {
            DrawCommand::Rect { color, .. } => assert_eq!(color.r, 2, "the 500ms frame"),
            other => panic!("expected a rect, got {other:?}"),
        }
    }

    /// A replay window is not a video player: its output is commands, at
    /// whatever size it happens to be, and always encodable.
    #[test]
    fn draws_an_encodable_frame_at_any_size() {
        let mut r = Replay::open(&recording("sizes", &sample())).unwrap();
        r.playing = false;
        r.seek(600);
        for (w, h) in [(320.0, 240.0), (800.0, 600.0), (1920.0, 1080.0)] {
            let commands = r.draw(w, h);
            rill_ui::stream::encode(&commands)
                .unwrap_or_else(|e| panic!("{w}x{h} frame does not encode: {e}"));
        }
    }

    /// Render a real recording at a real time, for eyeballing:
    ///
    ///   RILLREC=path/to.rillrec REPLAY_AT_MS=6000 \
    ///     cargo test -p rill-vector -- --ignored replay_preview
    #[test]
    #[ignore = "writes a preview image; run explicitly"]
    fn replay_preview() {
        let Ok(file) = std::env::var("RILLREC") else {
            eprintln!("skip: set RILLREC=<file.rillrec>");
            return;
        };
        let Some(renderer) = rill_gpu::Renderer::new_headless() else {
            eprintln!("skip: no wgpu adapter");
            return;
        };
        let mut r = Replay::open(Path::new(&file)).expect("open recording");
        r.playing = false;
        let at: u32 = std::env::var("REPLAY_AT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(r.duration_ms / 2);
        r.seek(at as i64);

        let (w, h) = (900u32, 640u32);
        let commands = r.draw(w as f32, h as f32);
        let rgba = renderer.render_to_rgba(
            &commands,
            &rill_gpu::NoImageSource,
            w,
            h,
            Color { r: 0, g: 0, b: 0, a: 255 },
        );
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for px in rgba.chunks(4) {
            ppm.extend_from_slice(&px[..3]);
        }
        let out = std::env::var("REPLAY_PREVIEW").unwrap_or_else(|_| "replay-preview.ppm".into());
        std::fs::write(&out, ppm).expect("write preview");
        eprintln!("wrote {out} at {at}ms of {}ms", r.duration_ms);
    }

    /// A recording cut mid-event still opens, and says so.
    #[test]
    fn a_truncated_recording_still_plays() {
        let path = recording("truncated", &sample());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 3);
        std::fs::write(&path, bytes).unwrap();

        let r = Replay::open(&path).unwrap();
        assert!(r.truncated.is_some(), "the missing tail is reported");
        assert!(!r.events.is_empty(), "the intact prefix survived");
    }
}

#[cfg(test)]
mod doc_preview {
    /// Render a compiled .rill document headlessly, through the same layout
    /// and renderer a vector window uses. The design loop for any app page:
    ///
    ///   RILL_DOC=page.rill DOC_PREVIEW=out.ppm \
    ///     cargo test -p rill-vector -- --ignored render_document
    #[test]
    #[ignore = "writes a preview image; run explicitly"]
    fn render_document() {
        let Ok(file) = std::env::var("RILL_DOC") else {
            eprintln!("skip: set RILL_DOC=<file.rill>");
            return;
        };
        let Some(renderer) = rill_gpu::Renderer::new_headless() else {
            eprintln!("skip: no wgpu adapter");
            return;
        };
        let bytes = std::fs::read(&file).expect("read document");
        let doc = rill_doc::decode(&bytes).expect("decode document");

        // The desktop's own theme, so previews match what the window shows.
        // DOC_PALETTE=<name> renders against one of the built-in palettes,
        // which is what makes a preview useful for judging colour and not
        // only spacing.
        let mut desktop = rill_viewport::theme::builtin_dark();
        if let Ok(name) = std::env::var("DOC_PALETTE") {
            desktop.apply_runtime(&name, false);
        }
        let tree = rill_ui::resolve(&doc, desktop.defaults.clone());

        // DOC_SIZE=WxH — a specimen is taller than a window, and judging a
        // scale needs the whole page at once.
        let (w, h) = std::env::var("DOC_SIZE")
            .ok()
            .and_then(|s| {
                let (a, b) = s.split_once('x')?;
                Some((a.parse().ok()?, b.parse().ok()?))
            })
            .unwrap_or((900u32, 620u32));
        let engine = rill_gpu::text::TextEngine::new();
        let mut measurer = rill_gpu::text::EngineMeasurer(&engine);
        // The document's declared initial state, so bound inputs preview
        // with their real contents instead of their placeholders.
        let state: Vec<rill_doc::ActionValue> =
            doc.states.iter().map(|v| v.initial.clone()).collect();

        // Preview the whole *window*, not just the page: a document that
        // claims the titlebar puts its toolbar there, and a preview that
        // dropped it would be judging a design with a piece missing.
        let look = rill_viewport::theme::WindowStyle::default();
        let bar = if tree.chrome.is_some() { look.titlebar_tall } else { look.titlebar };
        let doc_h = h as f32 - bar;
        let (mut commands, _) = rill_ui::layout_document(
            &tree,
            rill_ui::LayoutOptions { viewport_width: w as f32, viewport_height: Some(doc_h) },
            &mut measurer,
            &mut rill_ui::NoImages,
            &state,
            None,
            0,
            (0, 0),
            None,
            false,
        );
        commands = rill_ui::stream::offset_commands(&commands, 0.0, bar);
        // Same material rule as the live host: a document that claims the bar
        // gets the window body (page) behind its chrome; a bare bar gets the
        // classic surface fill.
        let bar_color = if tree.chrome.is_some() {
            desktop.defaults.page_background
        } else {
            desktop.defaults.token("surface").unwrap_or(desktop.defaults.page_background)
        };
        let mut frame = vec![rill_ui::DrawCommand::Rect {
            rect: rill_ui::Rect { x: 0.0, y: 0.0, w: w as f32, h: bar },
            color: bar_color,
            corner_radius: 0.0,
        }];
        frame.extend(rill_ui::layout_chrome(
            &tree,
            rill_ui::Rect { x: 0.0, y: 0.0, w: w as f32, h: bar },
            &mut measurer,
            &mut rill_ui::NoImages,
            &state,
            None,
            0,
            (0, 0),
            None,
            false,
        ));
        frame.extend(commands);
        let commands = frame;
        let rgba = renderer.render_to_rgba(
            &commands,
            &rill_gpu::NoImageSource,
            w,
            h,
            desktop.defaults.page_background,
        );
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for px in rgba.chunks(4) {
            ppm.extend_from_slice(&px[..3]);
        }
        let out = std::env::var("DOC_PREVIEW").unwrap_or_else(|_| "doc-preview.ppm".into());
        std::fs::write(&out, ppm).expect("write preview");
        eprintln!("wrote {out} ({} commands)", commands.len());
    }
}
