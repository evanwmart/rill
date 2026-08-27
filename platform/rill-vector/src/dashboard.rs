//! A live system monitor drawn as a command stream — the first Rill app whose
//! content *moves*, and the first real consumer of [`DrawCommand::Path`].
//!
//! Everything here is built programmatically rather than laid out from a
//! document: charts are geometry, not flow. Ring gauges are arcs, sparklines
//! are polylines, and the area under each line is a run of thin columns — the
//! rect and path primitives working together.
//!
//! Data comes from `/proc` directly (the same source the compositor's stats
//! HUD samples), so the window needs no server, no capability grant, and no
//! transport: it is a self-contained thing to watch.

use std::time::{Duration, Instant};

use rill_ui::{Color, DrawCommand, Point, Rect};

const BG: Color = Color { r: 16, g: 18, b: 28, a: 255 };
const CARD: Color = Color { r: 26, g: 30, b: 44, a: 255 };
const TRACK: Color = Color { r: 44, g: 50, b: 68, a: 255 };
const INK: Color = Color { r: 226, g: 232, b: 245, a: 255 };
const DIM: Color = Color { r: 138, g: 148, b: 170, a: 255 };
const ACCENT: Color = Color { r: 138, g: 180, b: 255, a: 255 };
const GREEN: Color = Color { r: 123, g: 216, b: 143, a: 255 };
const AMBER: Color = Color { r: 224, g: 164, b: 88, a: 255 };

/// How often to re-read `/proc`. Fast enough to feel live, slow enough that
/// the CPU delta is meaningful (and that the window itself stays cheap).
const SAMPLE_EVERY: Duration = Duration::from_millis(500);
/// Samples kept per series — the width of the history you can see.
const HISTORY: usize = 96;

/// A fixed-length ring of recent readings, oldest first once full.
struct Series {
    values: Vec<f32>,
}

impl Series {
    fn new() -> Series {
        Series { values: Vec::with_capacity(HISTORY) }
    }

    fn push(&mut self, v: f32) {
        if self.values.len() == HISTORY {
            self.values.remove(0);
        }
        self.values.push(v);
    }

    fn last(&self) -> f32 {
        self.values.last().copied().unwrap_or(0.0)
    }
}

/// One aggregate CPU reading from `/proc/stat`: busy and total jiffies.
#[derive(Clone, Copy)]
struct CpuTotals {
    busy: u64,
    total: u64,
}

fn read_cpu() -> Option<CpuTotals> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let nums: Vec<u64> = fields.filter_map(|f| f.parse().ok()).collect();
    if nums.len() < 5 {
        return None;
    }
    let total: u64 = nums.iter().sum();
    // idle + iowait are the two "not working" buckets.
    let idle = nums[3] + nums[4];
    Some(CpuTotals { busy: total.saturating_sub(idle), total })
}

/// `(used_bytes, total_bytes)` from `/proc/meminfo`. MemAvailable is the
/// kernel's own estimate of what a new allocation could get, which is what a
/// human means by "free" — unlike MemFree, which ignores reclaimable cache.
fn read_mem() -> Option<(u64, u64)> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = 0u64;
    let mut available = 0u64;
    for line in info.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next()?;
        let value: u64 = match parts.next().and_then(|v| v.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        match key {
            "MemTotal:" => total = value * 1024,
            "MemAvailable:" => available = value * 1024,
            _ => {}
        }
        if total > 0 && available > 0 {
            break;
        }
    }
    (total > 0).then(|| (total.saturating_sub(available), total))
}

/// One process in the Rill desktop.
pub struct Proc {
    pid: i32,
    name: String,
    /// Share of whole-machine CPU capacity, matching the system gauge.
    cpu: f32,
    rss: u64,
}

/// Everything `/proc` says about one process, read in a single pass.
struct Raw {
    ppid: i32,
    name: String,
    jiffies: u64,
    rss: u64,
}

/// Kernel clock ticks per second — the unit `/proc/<pid>/stat` counts CPU in.
/// Queried rather than assumed to be 100; a wrong constant here would scale
/// every process's CPU by a silent factor.
fn clock_ticks() -> f32 {
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz > 0 { hz as f32 } else { 100.0 }
}

fn read_proc(pid: i32) -> Option<Raw> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm sits in parens and may contain spaces — split after the last ')'.
    let (head, rest) = stat.rsplit_once(')')?;
    let name = head.split_once('(')?.1.to_string();
    let f: Vec<&str> = rest.split_whitespace().collect();
    // Fields after comm: [0]=state, [1]=ppid, [11]=utime, [12]=stime.
    let ppid: i32 = f.get(1)?.parse().ok()?;
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(Raw { ppid, name, jiffies: utime + stime, rss: rss_pages * 4096 })
}

/// Snapshot every process on the box, keyed by pid.
fn read_all_procs() -> std::collections::HashMap<i32, Raw> {
    let mut out = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else { return out };
    for entry in entries.flatten() {
        if let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok())
            && let Some(raw) = read_proc(pid)
        {
            out.insert(pid, raw);
        }
    }
    out
}

/// The pids making up the Rill desktop.
///
/// The compositor spawns its clients, so it is an ancestor of this window:
/// walk up from ourselves to find it, then take it and everything below it.
/// That beats matching on names — it picks up exactly *this* desktop and
/// ignores a second compositor running next to it.
///
/// Launched outside a compositor (`rill-vector --dashboard` from a shell)
/// there is no such ancestor, and we fall back to matching `rill-*` so the
/// window still reports something meaningful.
fn desktop_pids(procs: &std::collections::HashMap<i32, Raw>) -> Vec<i32> {
    const ROOT: &str = "rill-compositor";
    let mut pid = std::process::id() as i32;
    let mut root = None;
    // Bounded walk: a corrupted ppid chain must not spin.
    for _ in 0..64 {
        let Some(raw) = procs.get(&pid) else { break };
        if raw.name == ROOT {
            root = Some(pid);
            break;
        }
        if raw.ppid <= 1 {
            break;
        }
        pid = raw.ppid;
    }

    let Some(root) = root else {
        let mut loose: Vec<i32> = procs
            .iter()
            .filter(|(_, raw)| raw.name.starts_with("rill-"))
            .map(|(pid, _)| *pid)
            .collect();
        loose.sort();
        return loose;
    };

    let mut tree = vec![root];
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, raw) in procs {
            if raw.ppid == parent && !tree.contains(pid) {
                tree.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    tree.sort();
    tree
}

/// `(load1, running, total)` from `/proc/loadavg`.
fn read_load() -> Option<(f32, u32, u32)> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut fields = text.split_whitespace();
    let load1: f32 = fields.next()?.parse().ok()?;
    let procs = fields.nth(2)?;
    let (running, total) = procs.split_once('/')?;
    Some((load1, running.parse().ok()?, total.parse().ok()?))
}

pub struct Dashboard {
    cpu: Series,
    mem: Series,
    last_cpu: Option<CpuTotals>,
    last_sample: Option<Instant>,
    mem_used: u64,
    mem_total: u64,
    load: (f32, u32, u32),
    /// The Rill desktop's own processes — the number this window exists to
    /// make the case for.
    desktop: Vec<Proc>,
    desktop_rss: u64,
    desktop_cpu: f32,
    /// Previous per-pid jiffies, for the CPU delta.
    prev_jiffies: std::collections::HashMap<i32, u64>,
}

impl Dashboard {
    pub fn new() -> Dashboard {
        Dashboard {
            cpu: Series::new(),
            mem: Series::new(),
            last_cpu: read_cpu(),
            last_sample: None,
            mem_used: 0,
            mem_total: 0,
            load: (0.0, 0, 0),
            desktop: Vec::new(),
            desktop_rss: 0,
            desktop_cpu: 0.0,
            prev_jiffies: std::collections::HashMap::new(),
        }
    }

    /// Re-read `/proc` if the interval has elapsed. Returns whether a new
    /// sample landed, so the caller only redraws when the picture changed.
    pub fn sample(&mut self) -> bool {
        let dt = match self.last_sample {
            Some(at) if at.elapsed() < SAMPLE_EVERY => return false,
            Some(at) => at.elapsed().as_secs_f32(),
            None => 0.0,
        };
        self.last_sample = Some(Instant::now());

        if let Some(now) = read_cpu() {
            if let Some(prev) = self.last_cpu {
                let total_delta = now.total.saturating_sub(prev.total);
                let busy = now.busy.saturating_sub(prev.busy);
                // A zero delta means two reads landed in the same jiffy —
                // hold the previous value rather than dividing by zero.
                if total_delta > 0 {
                    self.cpu.push((busy as f32 / total_delta as f32 * 100.0).clamp(0.0, 100.0));
                }
            }
            self.last_cpu = Some(now);
        }
        self.sample_desktop(dt);
        if let Some((used, total)) = read_mem() {
            self.mem_used = used;
            self.mem_total = total;
            self.mem.push((used as f32 / total as f32 * 100.0).clamp(0.0, 100.0));
        }
        if let Some(load) = read_load() {
            self.load = load;
        }
        true
    }

    /// Re-measure the desktop's own processes over `dt` seconds.
    ///
    /// Per-process CPU is a percentage of **one core**, which is what `top`
    /// reports and what the compositor's own HUD shows — so a busy process
    /// can read over 100% and the desktop total can exceed it too. Measuring
    /// against the machine-wide jiffy total instead would divide by the core
    /// count (32 here), and the same compositor that the HUD calls 29% would
    /// show as 0.9%. Two panels on one screen disagreeing by 32x is worse
    /// than either convention.
    fn sample_desktop(&mut self, dt: f32) {
        let procs = read_all_procs();
        let pids = desktop_pids(&procs);
        let mut next = std::collections::HashMap::with_capacity(pids.len());
        let mut out = Vec::with_capacity(pids.len());
        let (mut rss_total, mut cpu_total) = (0u64, 0.0f32);

        for pid in pids {
            let Some(raw) = procs.get(&pid) else { continue };
            let cpu = match self.prev_jiffies.get(&pid) {
                Some(prev) if dt > 0.0 => {
                    let secs = raw.jiffies.saturating_sub(*prev) as f32 / clock_ticks();
                    secs / dt * 100.0
                }
                // First sighting: no delta yet, so claim nothing rather than
                // charging a process its whole lifetime of CPU at once.
                _ => 0.0,
            };
            next.insert(pid, raw.jiffies);
            rss_total += raw.rss;
            cpu_total += cpu;
            out.push(Proc { pid, name: raw.name.clone(), cpu, rss: raw.rss });
        }

        // Heaviest first — that is the one you want to see without scrolling.
        out.sort_by(|a, b| b.rss.cmp(&a.rss).then(a.pid.cmp(&b.pid)));
        self.desktop = out;
        self.desktop_rss = rss_total;
        self.desktop_cpu = cpu_total;
        self.prev_jiffies = next;
    }

    /// Build the window's frame for a `w x h` content area.
    pub fn draw(&self, w: f32, h: f32) -> Vec<DrawCommand> {
        let mut out = Vec::new();
        out.push(DrawCommand::Rect {
            rect: Rect { x: 0.0, y: 0.0, w, h },
            color: BG,
            corner_radius: 0.0,
        });

        let pad = 20.0;
        let mut y = pad;

        out.push(text(
            Rect { x: pad, y, w: w - pad * 2.0, h: 26.0 },
            "System",
            INK,
            20.0,
            700,
        ));
        y += 34.0;

        // --- gauges -------------------------------------------------------
        // Two rings side by side. An arc is exactly what the rect primitives
        // could never draw, so this is the part that earns the new command.
        //
        // Height is proportional rather than fixed: with a fixed 132 the
        // gauges ate a small window and left the charts a ~20px plot, which
        // rendered every history as a flat line. The charts are the part
        // that has to breathe.
        let gauge_h = (h * 0.22).clamp(88.0, 140.0);
        let half = (w - pad * 2.0 - 12.0) / 2.0;
        let cpu_pct = self.cpu.last();
        let mem_pct = self.mem.last();
        // Under pressure the ring goes amber — the one place colour carries
        // information rather than decoration.
        let hot = |pct: f32, cool: Color| if pct >= 85.0 { AMBER } else { cool };
        gauge(
            &mut out,
            Rect { x: pad, y, w: half, h: gauge_h },
            "CPU",
            cpu_pct,
            &format!("{cpu_pct:.0}%"),
            hot(cpu_pct, ACCENT),
        );
        gauge(
            &mut out,
            Rect { x: pad + half + 12.0, y, w: half, h: gauge_h },
            "MEMORY",
            mem_pct,
            &format_bytes(self.mem_used),
            hot(mem_pct, GREEN),
        );
        y += gauge_h + 12.0;

        // --- the desktop's own footprint ----------------------------------
        // Reserved before the charts so it never gets squeezed out: this is
        // the number the whole window is an argument about.
        let rows = self.desktop.len().min(6);
        // A hidden row is a lie about the total, so leave space to say so.
        let hidden = self.desktop.len().saturating_sub(rows);
        let desk_h = 30.0 + rows as f32 * 16.0 + if hidden > 0 { 16.0 } else { 0.0 } + 20.0;

        // --- history ------------------------------------------------------
        let chart_h = ((h - y - pad - 34.0 - desk_h - 24.0) / 2.0).max(48.0);
        chart(
            &mut out,
            Rect { x: pad, y, w: w - pad * 2.0, h: chart_h },
            "CPU history",
            &self.cpu.values,
            ACCENT,
        );
        y += chart_h + 12.0;
        chart(
            &mut out,
            Rect { x: pad, y, w: w - pad * 2.0, h: chart_h },
            "Memory history",
            &self.mem.values,
            GREEN,
        );
        y += chart_h + 12.0;

        self.desktop_card(&mut out, Rect { x: pad, y, w: w - pad * 2.0, h: desk_h }, rows);
        y += desk_h + 12.0;

        // --- footer -------------------------------------------------------
        let (load1, running, total) = self.load;
        out.push(text(
            Rect { x: pad, y: y + 4.0, w: w - pad * 2.0, h: 20.0 },
            &format!(
                "load {load1:.2}   {running} running of {total} processes   {} total memory",
                format_bytes(self.mem_total)
            ),
            DIM,
            12.0,
            400,
        ));
        out
    }

    /// The Rill desktop's own processes, and what fraction of the machine
    /// they actually occupy.
    fn desktop_card(&self, out: &mut Vec<DrawCommand>, area: Rect, rows: usize) {
        out.push(DrawCommand::Rect { rect: area, color: CARD, corner_radius: 12.0 });
        out.push(text(
            Rect { x: area.x + 12.0, y: area.y + 8.0, w: area.w * 0.5, h: 16.0 },
            "RILL DESKTOP",
            DIM,
            11.0,
            600,
        ));

        let share = if self.mem_total > 0 {
            self.desktop_rss as f64 / self.mem_total as f64 * 100.0
        } else {
            0.0
        };
        out.push(text(
            Rect { x: area.x + area.w * 0.5, y: area.y + 8.0, w: area.w * 0.5 - 12.0, h: 16.0 },
            &format!(
                "{} · {:.1}% CPU · {share:.2}% of memory",
                format_bytes(self.desktop_rss),
                self.desktop_cpu
            ),
            ACCENT,
            11.0,
            600,
        ));

        // Columns rather than a padded string: text has no alignment, so
        // each field gets its own rect at a fixed x.
        let mut row_y = area.y + 30.0;
        for p in self.desktop.iter().take(rows) {
            out.push(text(
                Rect { x: area.x + 12.0, y: row_y, w: area.w * 0.45, h: 15.0 },
                &p.name,
                INK,
                11.0,
                400,
            ));
            out.push(text(
                Rect { x: area.x + area.w * 0.58, y: row_y, w: area.w * 0.16, h: 15.0 },
                &format!("{:.1}%", p.cpu),
                DIM,
                11.0,
                400,
            ));
            out.push(text(
                Rect { x: area.x + area.w * 0.76, y: row_y, w: area.w * 0.22, h: 15.0 },
                &format_bytes(p.rss),
                DIM,
                11.0,
                400,
            ));
            row_y += 16.0;
        }
        let hidden = self.desktop.len().saturating_sub(rows);
        if hidden > 0 {
            out.push(text(
                Rect { x: area.x + 12.0, y: row_y, w: area.w - 24.0, h: 15.0 },
                &format!("+{hidden} more (counted in the total)"),
                DIM,
                11.0,
                400,
            ));
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

fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GB", b / GIB)
    } else {
        format!("{:.0} MB", b / MIB)
    }
}

/// Points along an arc, clockwise from `start_deg` (0° = +x, y grows down).
fn arc(cx: f32, cy: f32, r: f32, start_deg: f32, sweep_deg: f32, steps: usize) -> Vec<Point> {
    (0..=steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            let a = (start_deg + sweep_deg * t).to_radians();
            Point::new(cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

/// A 270° ring gauge: grey track, colored arc for the value, big number in
/// the middle, label underneath.
fn gauge(out: &mut Vec<DrawCommand>, area: Rect, label: &str, pct: f32, value: &str, color: Color) {
    out.push(DrawCommand::Rect { rect: area, color: CARD, corner_radius: 12.0 });

    // Text has no alignment in the command stream (a Text rect is the line
    // box and paints from its left edge), so the card is laid out as two
    // deliberate columns instead of pretending to centre anything: the
    // reading on the left, the ring on the right.
    let cx = area.x + area.w * 0.72;
    let cy = area.y + area.h / 2.0;
    let r = (area.h / 2.0 - 24.0).max(12.0);
    const START: f32 = 135.0;
    const SWEEP: f32 = 270.0;
    let stroke = 9.0;

    out.push(DrawCommand::Path {
        points: arc(cx, cy, r, START, SWEEP, 48),
        color: TRACK,
        width: stroke,
        closed: false,
    });
    let frac = (pct / 100.0).clamp(0.0, 1.0);
    if frac > 0.001 {
        // Keep the arc's resolution proportional to its length so a short arc
        // isn't over-tessellated and a long one doesn't go faceted.
        let steps = ((48.0 * frac).ceil() as usize).max(2);
        out.push(DrawCommand::Path {
            points: arc(cx, cy, r, START, SWEEP * frac, steps),
            color,
            width: stroke,
            closed: false,
        });
    }

    let text_x = area.x + 18.0;
    let text_w = area.w * 0.44;
    out.push(text(
        Rect { x: text_x, y: cy - 20.0, w: text_w, h: 28.0 },
        value,
        INK,
        22.0,
        700,
    ));
    out.push(text(
        Rect { x: text_x, y: cy + 10.0, w: text_w, h: 16.0 },
        label,
        DIM,
        11.0,
        600,
    ));
}

/// A history card: area columns under a stroked line, on a rounded panel.
fn chart(out: &mut Vec<DrawCommand>, area: Rect, label: &str, values: &[f32], color: Color) {
    out.push(DrawCommand::Rect { rect: area, color: CARD, corner_radius: 12.0 });
    out.push(text(
        Rect { x: area.x + 12.0, y: area.y + 8.0, w: area.w - 24.0, h: 16.0 },
        label,
        DIM,
        11.0,
        600,
    ));

    // The label sits in the card's top margin; everything below is plot, so
    // a short card spends its height on the curve rather than on chrome.
    let plot = Rect {
        x: area.x + 12.0,
        y: area.y + 26.0,
        w: area.w - 24.0,
        h: (area.h - 34.0).max(8.0),
    };
    // A 50% guide line, so the shape has something to be read against.
    out.push(DrawCommand::Rect {
        rect: Rect { x: plot.x, y: plot.y + plot.h / 2.0, w: plot.w, h: 1.0 },
        color: TRACK,
        corner_radius: 0.0,
    });
    if values.len() < 2 {
        return;
    }

    // x positions span the plot even before the history has filled, so the
    // line grows from the left rather than jumping when the ring wraps.
    let step = plot.w / (HISTORY - 1) as f32;
    let at = |i: usize, v: f32| {
        let y = plot.y + plot.h * (1.0 - (v / 100.0).clamp(0.0, 1.0));
        Point::new(plot.x + step * i as f32, y)
    };

    // Area fill: one thin column per sample, translucent. Rects and paths
    // doing the job neither could alone.
    let mut fill = color;
    fill.a = 38;
    for (i, v) in values.iter().enumerate() {
        let p = at(i, *v);
        out.push(DrawCommand::Rect {
            rect: Rect { x: p.x, y: p.y, w: step.max(1.0), h: plot.y + plot.h - p.y },
            color: fill,
            corner_radius: 0.0,
        });
    }
    out.push(DrawCommand::Path {
        points: values.iter().enumerate().map(|(i, v)| at(i, *v)).collect(),
        color,
        width: 2.0,
        closed: false,
    });
    // Head marker: a single-point path is a dot, which is exactly what a
    // "you are here" pip is.
    let head = at(values.len() - 1, values[values.len() - 1]);
    out.push(DrawCommand::Path { points: vec![head], color: INK, width: 5.0, closed: false });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_keeps_only_the_last_window() {
        let mut s = Series::new();
        for i in 0..(HISTORY + 20) {
            s.push(i as f32);
        }
        assert_eq!(s.values.len(), HISTORY);
        assert_eq!(s.last(), (HISTORY + 19) as f32);
        // Oldest retained is the (n - HISTORY)th push, not the very first.
        assert_eq!(s.values[0], 20.0);
    }

    /// The frame must always be encodable — that is the contract every
    /// vector window lives by, and charts generate commands in bulk.
    #[test]
    fn a_full_dashboard_frame_encodes() {
        let mut d = Dashboard::new();
        for i in 0..HISTORY {
            d.cpu.push((i % 101) as f32);
            d.mem.push(50.0);
        }
        let commands = d.draw(720.0, 560.0);
        let bytes = rill_ui::stream::encode(&commands).expect("dashboard frame encodes");
        // Charts are the densest thing we draw; it should still be a
        // kilobytes-scale frame, not a megabyte one.
        assert!(bytes.len() < 64 * 1024, "frame is {} bytes", bytes.len());
        assert_eq!(rill_ui::stream::decode(&bytes).unwrap(), commands);
    }

    /// Render a frame to a PPM for eyeballing — charts are the one thing a
    /// unit test cannot really judge. Same convention as the fuzz corpus:
    /// `cargo test -p rill-vector -- --ignored dashboard_preview`.
    #[test]
    #[ignore = "writes a preview image; run explicitly"]
    fn dashboard_preview() {
        let Some(renderer) = rill_gpu::Renderer::new_headless() else {
            eprintln!("skip: no wgpu adapter");
            return;
        };
        // DASHBOARD_SIZE=WxH checks how the layout holds up when squeezed.
        let (w, h) = std::env::var("DASHBOARD_SIZE")
            .ok()
            .and_then(|s| {
                let (a, b) = s.split_once('x')?;
                Some((a.parse().ok()?, b.parse().ok()?))
            })
            .unwrap_or((760u32, 566u32));
        let mut d = Dashboard::new();
        // A plausible-looking history rather than a flat line.
        for i in 0..HISTORY {
            let t = i as f32 / 6.0;
            d.cpu.push((42.0 + 30.0 * t.sin() + 12.0 * (t * 2.3).cos()).clamp(2.0, 99.0));
            d.mem.push((61.0 + 4.0 * (t / 3.0).sin()).clamp(2.0, 99.0));
        }
        d.mem_used = 9_800_000_000;
        d.mem_total = 16_000_000_000;
        d.load = (1.24, 3, 512);
        d.desktop = vec![
            Proc { pid: 1, name: "rill-compositor".into(), cpu: 15.8, rss: 265_400_000 },
            Proc { pid: 2, name: "rill-shell".into(), cpu: 0.4, rss: 41_200_000 },
            Proc { pid: 3, name: "rill-vector".into(), cpu: 0.0, rss: 9_400_000 },
            Proc { pid: 4, name: "rill-vector".into(), cpu: 0.0, rss: 8_800_000 },
        ];
        d.desktop_rss = d.desktop.iter().map(|p| p.rss).sum();
        d.desktop_cpu = d.desktop.iter().map(|p| p.cpu).sum();

        let commands = d.draw(w as f32, h as f32);
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
        let path = std::env::var("DASHBOARD_PREVIEW")
            .unwrap_or_else(|_| "dashboard-preview.ppm".to_string());
        std::fs::write(&path, ppm).expect("write preview");
        eprintln!("wrote {path}");
    }

    fn raw(ppid: i32, name: &str) -> Raw {
        Raw { ppid, name: name.into(), jiffies: 0, rss: 0 }
    }

    /// The desktop is found by walking up to the compositor and back down,
    /// so it picks up exactly this desktop — not a second one running beside
    /// it, and not unrelated processes that merely share a name.
    #[test]
    fn desktop_is_the_compositor_subtree() {
        let me = std::process::id() as i32;
        let (compositor, shell, other_compositor, other_client) = (900001, 900002, 900003, 900004);
        let mut procs = std::collections::HashMap::new();
        procs.insert(me, raw(compositor, "rill-vector"));
        procs.insert(compositor, raw(1, "rill-compositor"));
        procs.insert(shell, raw(compositor, "rill-shell"));
        // A whole separate desktop, which must not be counted.
        procs.insert(other_compositor, raw(1, "rill-compositor"));
        procs.insert(other_client, raw(other_compositor, "rill-vector"));

        let pids = desktop_pids(&procs);
        assert!(pids.contains(&compositor), "our compositor");
        assert!(pids.contains(&me), "ourselves");
        assert!(pids.contains(&shell), "a sibling client");
        assert!(!pids.contains(&other_compositor), "the other desktop leaked in");
        assert!(!pids.contains(&other_client), "the other desktop's client leaked in");
    }

    /// Run from a shell there is no compositor above us, so fall back to
    /// matching names rather than reporting an empty desktop.
    #[test]
    fn standalone_falls_back_to_name_matching() {
        let me = std::process::id() as i32;
        let mut procs = std::collections::HashMap::new();
        procs.insert(me, raw(500, "rill-vector"));
        procs.insert(500, raw(1, "bash"));
        procs.insert(501, raw(1, "firefox"));
        let pids = desktop_pids(&procs);
        assert_eq!(pids, vec![me], "only the rill-* processes");
    }

    /// A cycle in the ppid chain must not hang the sampler.
    #[test]
    fn a_broken_parent_chain_terminates() {
        let me = std::process::id() as i32;
        let mut procs = std::collections::HashMap::new();
        procs.insert(me, raw(700, "rill-vector"));
        procs.insert(700, raw(me, "weird"));
        let pids = desktop_pids(&procs);
        assert_eq!(pids, vec![me], "fell back without spinning");
    }

    /// A gauge at 0% must not emit a degenerate arc, and 100% must not
    /// overshoot the track.
    #[test]
    fn gauge_endpoints_are_well_formed() {
        for pct in [0.0, 0.4, 100.0] {
            let mut out = Vec::new();
            gauge(&mut out, Rect { x: 0.0, y: 0.0, w: 160.0, h: 132.0 }, "X", pct, "v", ACCENT);
            rill_ui::stream::encode(&out).expect("gauge encodes");
            let arcs = out
                .iter()
                .filter(|c| matches!(c, DrawCommand::Path { .. }))
                .count();
            // Track always; the value arc only once there is something to show.
            assert_eq!(arcs, if pct > 0.001 { 2 } else { 1 }, "pct {pct}");
        }
    }
}
