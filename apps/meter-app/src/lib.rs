//! A resource meter, served.
//!
//! Four gauges — CPU, memory, VRAM, storage — each drawn as one bar with
//! two readings in it: how much of the machine is in use, and how much of
//! that is Rill's. The accent segment is Rill; the dim segment is
//! everything else; the track is what's free. Pointed at by
//! `[[desktop.widgets]]`:
//!
//! ```toml
//! [[desktop.widgets]]
//! app = "rill://127.0.0.1:7420/meter"
//! anchor = "top-right"
//! width = 300
//! height = 210
//! ```
//!
//! "Rill" is the process family (`rill*` binaries plus the demo's
//! files-app server) for CPU, memory and VRAM, and the runtime directories
//! (`~/.local/share/rill*`, `~/.cache/rill`, `~/.config/rill`) for
//! storage — the footprint the *product* has, not this machine's build
//! caches. VRAM comes from nvidia-smi where it exists (with per-process
//! attribution), the amdgpu sysfs otherwise (machine total only); a box
//! with neither simply has no VRAM row.
//!
//! There is no refresh mechanism here. The page carries `live`, so the
//! *client* re-reads it on a clock. The slow readings are cached on their
//! own clocks — VRAM asks a subprocess, storage walks directories — so the
//! 1s tick stays cheap.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rill_appkit::Metrics;
use rill_auth::Identity;
use rill_protocol::{ActionValue, Status};
use rill_server::AppHandler;

/// How often the page asks to be re-read. A meter that moves faster than
/// this is noise on a desktop; one that moves slower feels stuck.
const LIVE_MS: u16 = 1000;

/// How many samples the spark line keeps. At one a second this is the last
/// minute, which is the window a person actually looks back over.
const HISTORY: usize = 60;

/// VRAM asks nvidia-smi (a subprocess); storage walks directories. Neither
/// belongs on the 1s tick.
const VRAM_EVERY: Duration = Duration::from_secs(3);
const DISK_EVERY: Duration = Duration::from_secs(30);

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
/// person means by "free" — unlike MemFree, which ignores reclaimable cache.
fn read_mem() -> Option<(u64, u64)> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let (mut total, mut available) = (0u64, 0u64);
    for line in info.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        let Some(value) = parts.next().and_then(|v| v.parse::<u64>().ok()) else { continue };
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

/// `(load1, running, total)` from `/proc/loadavg`.
fn read_load() -> Option<(f32, u32, u32)> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut fields = text.split_whitespace();
    let load1: f32 = fields.next()?.parse().ok()?;
    let procs = fields.nth(2)?;
    let (running, total) = procs.split_once('/')?;
    Some((load1, running.parse().ok()?, total.parse().ok()?))
}

/// `(used_bytes, total_bytes)` for the root filesystem.
fn read_disk() -> Option<(u64, u64)> {
    let path = std::ffi::CString::new("/").ok()?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut vfs) } != 0 {
        return None;
    }
    let frsize = if vfs.f_frsize > 0 { vfs.f_frsize } else { vfs.f_bsize } as u64;
    let total = vfs.f_blocks as u64 * frsize;
    let free = vfs.f_bfree as u64 * frsize;
    (total > 0).then(|| (total.saturating_sub(free), total))
}

/// The Rill process family: every `rill*` binary, plus the demo server
/// (files-app), matched by `/proc/<pid>/comm`. comm truncates at 15
/// characters, which every name here fits inside.
fn rill_pids() -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<i32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| {
                let c = c.trim();
                c.starts_with("rill") || c == "files-app"
            })
        })
        .collect()
}

/// Total jiffies (utime + stime) the family has spent, summed. The comm
/// field can contain spaces, so parse from after the last ')'.
fn rill_jiffies(pids: &[i32]) -> u64 {
    pids.iter()
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let rest = stat.rsplit_once(')')?.1;
            let mut f = rest.split_whitespace();
            let utime: u64 = f.nth(11)?.parse().ok()?;
            let stime: u64 = f.next()?.parse().ok()?;
            Some(utime + stime)
        })
        .sum()
}

/// Resident memory the family holds, summed, in bytes.
fn rill_rss(pids: &[i32]) -> u64 {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    pids.iter()
        .filter_map(|pid| {
            let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
            statm.split_whitespace().nth(1)?.parse::<u64>().ok()
        })
        .sum::<u64>()
        * page
}

/// `(used, total, rill_used)` in bytes. nvidia-smi where it exists —
/// including per-process framebuffer via `pmon`, matched by pid — else the
/// amdgpu sysfs (no per-process attribution there), else `None`: a machine
/// with no discrete VRAM has no VRAM row rather than a zero one.
fn read_vram(rill: &[i32]) -> Option<(u64, u64, u64)> {
    if let Some(out) = run(&["nvidia-smi", "--query-gpu=memory.used,memory.total", "--format=csv,noheader,nounits"]) {
        let line = out.lines().next()?;
        let mut parts = line.split(',').map(str::trim);
        let used_mib: u64 = parts.next()?.parse().ok()?;
        let total_mib: u64 = parts.next()?.parse().ok()?;
        // Per-process framebuffer. Best-effort: pmon can be absent or
        // refuse; the machine numbers stand on their own without it.
        let rill_mib: u64 = run(&["nvidia-smi", "pmon", "-c", "1", "-s", "m"])
            .map(|pm| {
                pm.lines()
                    .filter(|l| !l.trim_start().starts_with('#'))
                    .filter_map(|l| {
                        let mut f = l.split_whitespace();
                        let pid: i32 = f.nth(1)?.parse().ok()?;
                        let fb: u64 = f.nth(1)?.parse().ok()?;
                        rill.contains(&pid).then_some(fb)
                    })
                    .sum()
            })
            .unwrap_or(0);
        return Some((used_mib << 20, total_mib << 20, rill_mib << 20));
    }
    // amdgpu: bytes straight from sysfs, machine-wide only.
    for card in std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten() {
        let dev = card.path().join("device");
        let read_num = |name: &str| -> Option<u64> {
            std::fs::read_to_string(dev.join(name)).ok()?.trim().parse().ok()
        };
        if let (Some(used), Some(total)) =
            (read_num("mem_info_vram_used"), read_num("mem_info_vram_total"))
        {
            return Some((used, total, 0));
        }
    }
    None
}

fn run(cmd: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd[0]).args(&cmd[1..]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Bytes on disk under Rill's runtime directories: data, cache, config.
/// Deliberately *not* `~/.cache/rill-*` — this machine's build caches are
/// the workshop's, not the product's.
fn rill_disk() -> u64 {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return 0 };
    let mut roots: Vec<PathBuf> = vec![home.join(".cache/rill"), home.join(".config/rill")];
    for entry in std::fs::read_dir(home.join(".local/share")).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with("rill") {
            roots.push(entry.path());
        }
    }
    roots.iter().map(|r| du(r)).sum()
}

/// Recursive size of a tree, symlinks skipped so a link out of the tree
/// cannot count the world.
fn du(path: &std::path::Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if meta.is_symlink() {
        return 0;
    }
    if meta.is_file() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| du(&e.path()))
        .sum()
}

/// Bytes as a person reads them: three significant figures and a unit.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 || unit == 0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

struct Samples {
    last_cpu: Option<CpuTotals>,
    last_rill: u64,
    /// Whole-machine CPU busy fraction, oldest first.
    history: Vec<f32>,
    /// Rill's share of the machine at the last sample.
    rill_cpu: f32,
    /// When the last sample was taken, so a page fetched twice in the same
    /// tick does not report a CPU figure computed over no time at all.
    last_at: Option<Instant>,
    /// The last assembled reading, and when. The CPU gate below is not
    /// enough on its own: memory, load and disk are read per call, so
    /// `/meter` and `/meter/data` fetched a millisecond apart described two
    /// different machines — the very thing the facts/views split promises
    /// they cannot do.
    last_snapshot: Option<(Instant, Snapshot)>,
    /// The slow readings, on their own clocks.
    vram: Option<(u64, u64, u64)>,
    vram_at: Option<Instant>,
    disk_rill: u64,
    disk_at: Option<Instant>,
}

/// One reading of the machine — everything the meter knows, before anything
/// decides how to show it.
///
/// This is the *fact*: it is what `/meter/data` serves and what `/meter`
/// draws, and there is exactly one way to obtain it, so the two can never
/// disagree about the machine. Which matters more than it sounds: the CPU
/// figure is a rate between two samples, and two code paths sampling
/// independently would be two different readings of the same instant.
#[derive(Clone)]
struct Snapshot {
    /// Whole-machine CPU busy fraction, 0.0–1.0; Rill's share of the same.
    cpu: f32,
    rill_cpu: f32,
    mem_used: u64,
    mem_total: u64,
    mem_rill: u64,
    /// Zero total = no VRAM reading on this machine; the row is omitted.
    vram_used: u64,
    vram_total: u64,
    vram_rill: u64,
    disk_used: u64,
    disk_total: u64,
    disk_rill: u64,
    load1: f32,
    running: u32,
    procs: u32,
    /// Recent CPU history, oldest first — at most [`HISTORY`] entries.
    history: Vec<f32>,
}

impl Snapshot {
    /// The document's own arithmetic, so the page and the data agree on it
    /// rather than each rounding for itself.
    fn mem_fraction(&self) -> f32 {
        self.mem_used as f32 / self.mem_total.max(1) as f32
    }

    /// TOML, hand-formatted: the fact as any client can read it without
    /// knowing what a Rill document is.
    fn to_toml(&self) -> String {
        let mut out = String::new();
        out.push_str("# rill meter — machine state (see /meter for the same facts, drawn)\n");
        out.push_str(&format!("cpu = {:.4}\n", self.cpu));
        out.push_str(&format!("rill_cpu = {:.4}\n", self.rill_cpu));
        out.push_str(&format!("mem_used_bytes = {}\n", self.mem_used));
        out.push_str(&format!("mem_total_bytes = {}\n", self.mem_total));
        out.push_str(&format!("mem_rill_bytes = {}\n", self.mem_rill));
        out.push_str(&format!("mem_fraction = {:.4}\n", self.mem_fraction()));
        out.push_str(&format!("vram_used_bytes = {}\n", self.vram_used));
        out.push_str(&format!("vram_total_bytes = {}\n", self.vram_total));
        out.push_str(&format!("vram_rill_bytes = {}\n", self.vram_rill));
        out.push_str(&format!("disk_used_bytes = {}\n", self.disk_used));
        out.push_str(&format!("disk_total_bytes = {}\n", self.disk_total));
        out.push_str(&format!("disk_rill_bytes = {}\n", self.disk_rill));
        out.push_str(&format!("load1 = {:.2}\n", self.load1));
        out.push_str(&format!("procs_running = {}\n", self.running));
        out.push_str(&format!("procs_total = {}\n", self.procs));
        out.push_str("history = [");
        for (i, v) in self.history.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("{v:.4}"));
        }
        out.push_str("]\n");
        out
    }
}

pub struct Meter {
    samples: Mutex<Samples>,
    theme: PathBuf,
}

impl Meter {
    pub fn new(theme: PathBuf) -> Meter {
        Meter {
            samples: Mutex::new(Samples {
                last_cpu: read_cpu(),
                last_rill: rill_jiffies(&rill_pids()),
                history: Vec::new(),
                rill_cpu: 0.0,
                last_at: None,
                last_snapshot: None,
                vram: None,
                vram_at: None,
                disk_rill: 0,
                disk_at: None,
            }),
            theme,
        }
    }

    /// Take a reading, or reuse the last one if barely any time has passed.
    /// CPU is a *rate*: it only exists between two samples, so sampling
    /// twice in the same instant would divide by nothing. Returns
    /// (machine busy fraction, rill's fraction of the machine).
    /// How long one reading of the machine stays current. Two fetches
    /// inside this window are the same sample by construction.
    const FRESH: Duration = Duration::from_millis(200);

    fn sample(&self, pids: &[i32]) -> (f32, f32) {
        let mut s = match self.samples.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let fresh_enough = s
            .last_at
            .is_some_and(|t| t.elapsed() < Meter::FRESH);
        if fresh_enough {
            return (s.history.last().copied().unwrap_or(0.0), s.rill_cpu);
        }
        let rill_now = rill_jiffies(pids);
        let busy = match (read_cpu(), s.last_cpu) {
            (Some(now), Some(prev)) => {
                let dt = now.total.saturating_sub(prev.total);
                let db = now.busy.saturating_sub(prev.busy);
                // A pid dying between samples shrinks the family sum; that
                // reads as zero for one tick, never as negative.
                let dr = rill_now.saturating_sub(s.last_rill);
                s.last_cpu = Some(now);
                s.last_rill = rill_now;
                if dt == 0 {
                    s.history.last().copied().unwrap_or(0.0)
                } else {
                    s.rill_cpu = (dr as f32 / dt as f32).clamp(0.0, 1.0);
                    db as f32 / dt as f32
                }
            }
            (Some(now), None) => {
                s.last_cpu = Some(now);
                s.last_rill = rill_now;
                0.0
            }
            _ => 0.0,
        };
        s.last_at = Some(Instant::now());
        s.history.push(busy.clamp(0.0, 1.0));
        let overflow = s.history.len().saturating_sub(HISTORY);
        s.history.drain(..overflow);
        (busy.clamp(0.0, 1.0), s.rill_cpu)
    }

    fn lock_samples(&self) -> std::sync::MutexGuard<'_, Samples> {
        match self.samples.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn history(&self) -> Vec<f32> {
        self.lock_samples().history.clone()
    }

    /// The slow readings, refreshed on their own clocks: VRAM spawns a
    /// subprocess, storage walks trees. Returns (vram, rill_disk).
    fn slow(&self, pids: &[i32]) -> (Option<(u64, u64, u64)>, u64) {
        let mut s = match self.samples.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if s.vram_at.is_none_or(|t| t.elapsed() >= VRAM_EVERY) {
            s.vram = read_vram(pids);
            s.vram_at = Some(Instant::now());
        }
        if s.disk_at.is_none_or(|t| t.elapsed() >= DISK_EVERY) {
            s.disk_rill = rill_disk();
            s.disk_at = Some(Instant::now());
        }
        (s.vram, s.disk_rill)
    }

    /// Read the machine. The one place either representation comes from.
    fn snapshot(&self) -> Snapshot {
        if let Some((at, snap)) = self.lock_samples().last_snapshot.as_ref()
            && at.elapsed() < Meter::FRESH
        {
            return snap.clone();
        }
        let pids = rill_pids();
        let (cpu, rill_cpu) = self.sample(&pids);
        let (mem_used, mem_total) = read_mem().unwrap_or((0, 1));
        let mem_rill = rill_rss(&pids);
        let (vram, disk_rill) = self.slow(&pids);
        let (vram_used, vram_total, vram_rill) = vram.unwrap_or((0, 0, 0));
        let (disk_used, disk_total) = read_disk().unwrap_or((0, 1));
        let (load1, running, procs) = read_load().unwrap_or((0.0, 0, 0));
        let snap = Snapshot {
            cpu,
            rill_cpu,
            mem_used,
            mem_total,
            mem_rill,
            vram_used,
            vram_total,
            vram_rill,
            disk_used,
            disk_total,
            disk_rill,
            load1,
            running,
            procs,
            history: self.history(),
        };
        self.lock_samples().last_snapshot = Some((Instant::now(), snap.clone()));
        snap
    }

    fn page(&self) -> Result<Vec<u8>, Status> {
        let m = Metrics::from_theme_file(&self.theme);
        let s = self.snapshot();

        let f = m.font_size;
        let (small, p) = (f - 3.0, m.padding);
        let tall = (f * 2.2).round();
        let mut kdl = format!(
            "style \"meter\" padding={p} gap={p} height=\"fill\"\n\
             style \"row\" padding=0 gap={p} valign=\"center\"\n\
             style \"label\" color=\"text-muted\" size={small} font=\"mono\" weight={weight}\n\
             style \"value\" color=\"text\" size={small} font=\"mono\" weight={weight}\n\
             style \"quiet\" color=\"text-muted\" size={small} font=\"mono\" weight={weight}\n\
             style \"track\" background=\"surface-raised\" corner=2\n\
             style \"fill\" background=\"accent\" corner=2\n\
             style \"fill-dim\" background=\"elevation-lg\" corner=2\n\
             style \"spark\" background=\"accent\" corner=0\n\
             style \"spark-row\" padding=0 gap=1 valign=\"bottom\" height={tall}\n\n\
             column style=\"meter\" {{\n",
            weight = m.mono_weight,
        );

        // One bar per resource, three segments: Rill's share in accent,
        // the rest of what's used dimmed, the track for what's free.
        // Side-by-side rects, not overlays — layout here is flow.
        let bar_w = 86.0f32;
        let bar_h = (f * 0.5).round();
        let mut gauge = |label: &str, used: f32, rill: f32, right: String| {
            let rill = rill.clamp(0.0, 1.0).min(used.clamp(0.0, 1.0));
            let rill_px = (bar_w * rill).round();
            let other_px = (bar_w * used.clamp(0.0, 1.0)).round() - rill_px;
            let rest = (bar_w - rill_px - other_px).max(0.0);
            kdl.push_str("\trow style=\"row\" {\n");
            kdl.push_str(&format!("\t\ttext \"{label}\" style=\"label\"\n"));
            if rill_px >= 1.0 {
                kdl.push_str(&format!("\t\trect style=\"fill\" width={rill_px} height={bar_h}\n"));
            }
            if other_px >= 1.0 {
                kdl.push_str(&format!(
                    "\t\trect style=\"fill-dim\" width={other_px} height={bar_h}\n"
                ));
            }
            if rest >= 1.0 {
                kdl.push_str(&format!("\t\trect style=\"track\" width={rest} height={bar_h}\n"));
            }
            kdl.push_str("\t\tspacer\n");
            kdl.push_str(&format!("\t\ttext {} style=\"value\"\n", rill_doc::kdl_escape(&right)));
            kdl.push_str("\t}\n");
        };

        gauge(
            "cpu",
            s.cpu,
            s.rill_cpu,
            format!("{:.0}% · r {:.0}%", s.cpu * 100.0, s.rill_cpu * 100.0),
        );
        gauge(
            "mem",
            s.mem_fraction(),
            s.mem_rill as f32 / s.mem_total.max(1) as f32,
            format!(
                "{}/{} · r {}",
                human_bytes(s.mem_used),
                human_bytes(s.mem_total),
                human_bytes(s.mem_rill)
            ),
        );
        if s.vram_total > 0 {
            let rill_txt = match s.vram_rill {
                0 => String::new(),
                n => format!(" · r {}", human_bytes(n)),
            };
            gauge(
                "gpu",
                s.vram_used as f32 / s.vram_total as f32,
                s.vram_rill as f32 / s.vram_total as f32,
                format!(
                    "{}/{}{rill_txt}",
                    human_bytes(s.vram_used),
                    human_bytes(s.vram_total)
                ),
            );
        }
        gauge(
            "disk",
            s.disk_used as f32 / s.disk_total.max(1) as f32,
            s.disk_rill as f32 / s.disk_total.max(1) as f32,
            format!(
                "{}/{} · r {}",
                human_bytes(s.disk_used),
                human_bytes(s.disk_total),
                human_bytes(s.disk_rill)
            ),
        );

        // The spark line: one thin rect per sample, tallest at the right.
        // Values below a pixel still draw a pixel, so an idle machine reads
        // as a floor rather than as missing data.
        let history = &s.history;
        if history.len() > 1 {
            kdl.push_str("\trow style=\"spark-row\" {\n");
            for value in history.iter().rev().take(HISTORY).rev() {
                let h = (tall * value.clamp(0.0, 1.0)).round().max(1.0);
                kdl.push_str(&format!("\t\trect style=\"spark\" width=2 height={h}\n"));
            }
            kdl.push_str("\t}\n");
        }

        kdl.push_str("\tspacer\n");
        kdl.push_str(&format!(
            "\trow style=\"row\" {{ text \"load {load:.2}\" style=\"quiet\"; spacer; \
             text \"accent = rill\" style=\"quiet\"; spacer; \
             text \"{running}/{procs}\" style=\"quiet\" }}\n",
            load = s.load1,
            running = s.running,
            procs = s.procs,
        ));
        // The clock the widget runs on. Nothing else here refreshes anything.
        kdl.push_str(&format!("\tlive target=\"/meter\" every={LIVE_MS}\n"));
        kdl.push_str("}\n");

        rill_appkit::compile_page("meter-app", &kdl)
    }
}

impl AppHandler for Meter {
    fn get(&self, path: &str, _identity: &Identity) -> Option<Vec<u8>> {
        match path {
            "/meter" | "/meter/" => self.page().ok(),
            // The same facts, without the drawing. A meter is a machine
            // state that happens to have a picture of it; anything that
            // wants the state — a CLI, an agent, another server — should not
            // have to decode a document and read the numbers back out of the
            // text runs. Same snapshot, two representations.
            "/meter/data" => Some(self.snapshot().to_toml().into_bytes()),
            _ => None,
        }
    }

    fn action(
        &self,
        _path: &str,
        _fields: &[(String, ActionValue)],
        _identity: &Identity,
    ) -> Result<Vec<u8>, Status> {
        // A meter is something to look at. It has nothing to press.
        Err(Status::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meter() -> Meter {
        Meter::new(PathBuf::from("/nonexistent/theme.toml"))
    }

    /// The design-loop hooks reach every app, not just the one they were
    /// written in. RILL_DUMP_KDL and RILL_TRACE lived in files-app's private
    /// `compile`, so pointing either at this widget did nothing at all — which
    /// looks like the tooling is broken rather than absent. Checked on a
    /// widget precisely because it is the app least likely to be remembered.
    ///
    /// Serial: the hooks are environment variables, and the environment is
    /// process-wide.
    #[test]
    fn the_page_hooks_reach_this_app_too() {
        let dir = std::env::temp_dir().join(format!("meter-hooks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dump = dir.join("page.kdl");
        let legend = dir.join("legend.txt");

        // SAFETY: single-threaded within this test; no other test in this
        // binary reads these variables.
        unsafe {
            std::env::set_var("RILL_DUMP_KDL", &dump);
            std::env::set_var("RILL_TRACE", &legend);
        }
        let page = meter().page().expect("the widget renders");
        unsafe {
            std::env::remove_var("RILL_DUMP_KDL");
            std::env::remove_var("RILL_TRACE");
        }

        assert!(rill_doc::decode(&page).is_ok(), "a traced page is still a page");
        let dumped = std::fs::read_to_string(&dump).expect("RILL_DUMP_KDL wrote the source");
        assert!(dumped.contains("column"), "the dump is the generated KDL: {dumped:.120}");
        assert!(
            std::fs::metadata(&legend).is_ok_and(|m| m.len() > 0),
            "RILL_TRACE wrote a colour legend"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The machine this runs on has a /proc, so the readers must produce
    /// something sane rather than zeroes.
    #[test]
    fn the_readers_see_a_real_machine() {
        let (used, total) = read_mem().expect("meminfo");
        assert!(total > 0 && used <= total, "{used} of {total}");
        let (load, _, procs) = read_load().expect("loadavg");
        assert!(load >= 0.0);
        assert!(procs > 0, "a machine with no processes is not running this test");
        assert!(read_cpu().is_some());
        let (dused, dtotal) = read_disk().expect("statvfs /");
        assert!(dtotal > 0 && dused <= dtotal, "{dused} of {dtotal}");
    }

    #[test]
    fn bytes_read_the_way_a_person_says_them() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1024), "1.0K");
        assert_eq!(human_bytes(1536), "1.5K");
        assert_eq!(human_bytes(16 * 1024 * 1024 * 1024), "16.0G");
    }

    /// CPU is a rate between two samples. Asking twice in the same instant
    /// must reuse the last figure rather than divide by no elapsed time.
    #[test]
    fn cpu_is_a_rate_and_survives_being_asked_twice() {
        let m = meter();
        let pids = rill_pids();
        let (first, _) = m.sample(&pids);
        let (second, _) = m.sample(&pids);
        assert_eq!(first, second, "a second reading in the same tick is the same reading");
        assert!((0.0..=1.0).contains(&first), "{first}");

        std::thread::sleep(Duration::from_millis(250));
        let (third, rill) = m.sample(&pids);
        assert!((0.0..=1.0).contains(&third), "{third}");
        assert!((0.0..=1.0).contains(&rill), "{rill}");
        assert_eq!(m.history().len(), 2, "one entry per real sample");
    }

    /// The page is a document with a clock in it, and nothing else about it
    /// is special: no refresh path, no host support, no capability.
    #[test]
    fn the_page_carries_its_own_clock() {
        let m = meter();
        let bytes = m.get("/meter", &Identity::Anonymous).expect("a page");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        let live = doc.nodes.iter().find_map(|n| match n {
            rill_doc::Node::Live { target, interval } => {
                Some((doc.string(*target).to_string(), *interval))
            }
            _ => None,
        });
        assert_eq!(live, Some(("/meter".to_string(), LIVE_MS)));
        assert!(m.get("/meter/nope", &Identity::Anonymous).is_none());
    }

    /// The data sibling is machine-readable in the plainest sense: it parses
    /// as TOML, and it is not a document — no `RDOC` magic, nothing to
    /// decode, nothing about presentation in it.
    #[test]
    fn the_data_sibling_is_data() {
        let m = meter();
        let bytes = m.get("/meter/data", &Identity::Anonymous).expect("data");
        assert_ne!(&bytes[..4.min(bytes.len())], b"RDOC", "a fact is not a document");

        let text = String::from_utf8(bytes).expect("utf-8");
        let table: toml::Table = text.parse().expect("parses as TOML");
        for key in [
            "cpu",
            "rill_cpu",
            "mem_used_bytes",
            "mem_total_bytes",
            "mem_rill_bytes",
            "mem_fraction",
            "vram_used_bytes",
            "vram_total_bytes",
            "vram_rill_bytes",
            "disk_used_bytes",
            "disk_total_bytes",
            "disk_rill_bytes",
            "load1",
            "procs_running",
            "procs_total",
            "history",
        ] {
            assert!(table.contains_key(key), "missing {key}");
        }
        let total = table["mem_total_bytes"].as_integer().expect("integer");
        let used = table["mem_used_bytes"].as_integer().expect("integer");
        assert!(total > 0 && used <= total, "{used} of {total}");
        assert!(table["history"].as_array().is_some(), "history is a series");
        // No presentation anywhere in it: no styles, no colors, no sizes.
        for leak in ["style", "color", "accent", "padding", "width"] {
            assert!(!text.contains(leak), "presentation leaked into the facts: {leak}");
        }
    }

    /// The property that makes this a facts/views split rather than two
    /// endpoints: both come from one reading, so they cannot disagree about
    /// the machine. The 200ms freshness gate makes that checkable — two
    /// fetches in the same tick are the same sample by construction.
    #[test]
    fn the_page_and_the_data_report_the_same_machine() {
        let m = meter();
        let data = String::from_utf8(m.get("/meter/data", &Identity::Anonymous).unwrap()).unwrap();
        let page = m.get("/meter", &Identity::Anonymous).unwrap();

        let table: toml::Table = data.parse().unwrap();
        let cpu = table["cpu"].as_float().unwrap() as f32;
        let mem_used = table["mem_used_bytes"].as_integer().unwrap() as u64;
        let mem_total = table["mem_total_bytes"].as_integer().unwrap() as u64;

        // The page prints the CPU percentage and the memory pair as text;
        // find them and check they are the same numbers.
        let doc = rill_doc::decode(&page).expect("decodes");
        let text: String = doc.strings.join("\u{1}");
        assert!(
            text.contains(&format!("{:.0}%", cpu * 100.0)),
            "the drawn CPU figure is the sampled one ({cpu})"
        );
        assert!(
            text.contains(&format!("{}/{}", human_bytes(mem_used), human_bytes(mem_total))),
            "the drawn memory figure is the sampled one"
        );
    }

    /// The spark line grows a bar per sample and never outgrows its window.
    #[test]
    fn the_spark_line_is_bounded() {
        let m = meter();
        for _ in 0..(HISTORY + 20) {
            {
                let mut s = m.samples.lock().unwrap();
                s.last_at = None; // force a fresh sample without sleeping
            }
            m.sample(&[]);
        }
        assert_eq!(m.history().len(), HISTORY);
    }

    /// `du` adds up exactly the bytes in the tree and follows no symlink.
    #[test]
    fn du_counts_the_tree_and_only_the_tree() {
        let dir = std::env::temp_dir().join(format!("meter-du-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("sub/b"), vec![0u8; 50]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/", dir.join("world")).unwrap();
        assert_eq!(du(&dir), 150, "files counted once, the symlink not at all");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The family scan survives an empty result and never panics on
    /// processes that vanish mid-read — both are ordinary on a busy box.
    #[test]
    fn the_rill_family_is_a_best_effort_census() {
        let pids = rill_pids();
        let _ = rill_jiffies(&pids);
        let _ = rill_rss(&pids);
        // A pid that does not exist reads as nothing, not as an error.
        assert_eq!(rill_jiffies(&[i32::MAX]), 0);
        assert_eq!(rill_rss(&[i32::MAX]), 0);
    }

    /// read_vram's pmon parse attributes by pid, so a machine with no rill
    /// processes attributes zero — and a machine with no GPU tooling reads
    /// as no VRAM row, never as an error.
    #[test]
    fn vram_attribution_without_a_family_is_zero() {
        if let Some((used, total, rill)) = read_vram(&[]) {
            assert!(used <= total, "{used} of {total}");
            assert_eq!(rill, 0, "no pids, no attribution");
        }
    }
}
