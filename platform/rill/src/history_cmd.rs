//! `rill history` — the user-facing half of semantic history
//! (specs/history.md).
//!
//! ```text
//! rill history list              what exists, and what it costs
//! rill history grep <text>       find a moment by what was on screen
//! rill history show <segment>    a segment's transcript as a timeline
//! rill history tail [n]          the recent transcript (the agent's view)
//! ```
//!
//! The demo this exists for: type a phrase you saw yesterday, and be
//! looking at the moment it appeared. `grep` prints the timestamp and the
//! segment; `replay` (once the player learns to seek `.rhs`) opens it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rill_history::corpus::Corpus;
use rill_history::crypt::Kek;
use rill_history::event::{T0_ROUTINE, Tier};

/// Where history lives by default. Beside the rest of Rill's state, not in
/// a cache directory — this is the least disposable data on the machine.
pub fn default_history_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".local/share/rill/history")
}

pub fn run(args: &[String]) -> ExitCode {
    let mut dir = default_history_dir();
    let mut tier: Tier = T0_ROUTINE;
    // The device identity, for unlocking encrypted segments. Same default
    // and RILL_IDENTITY override as every other rill command; --identity
    // points elsewhere. History written before encryption stays readable
    // with no identity at all.
    let mut identity = rill_client::util::default_identity_dir();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--identity" => match args.get(i + 1) {
                Some(v) => {
                    identity = PathBuf::from(v);
                    i += 2;
                }
                None => return fail("--identity needs a value"),
            },
            "--dir" => match args.get(i + 1) {
                Some(v) => {
                    dir = PathBuf::from(v);
                    i += 2;
                }
                None => return fail("--dir needs a value"),
            },
            "--tier" => match args.get(i + 1).and_then(|v| v.parse::<Tier>().ok()) {
                Some(t) => {
                    tier = t;
                    i += 2;
                }
                None => return fail("--tier needs a number"),
            },
            other => {
                rest.push(other.to_string());
                i += 1;
            }
        }
    }

    let kek = Kek::from_identity_dir(&identity);
    match rest.split_first() {
        Some((cmd, a)) if cmd == "list" => list(&dir, kek, a),
        Some((cmd, a)) if cmd == "grep" && !a.is_empty() => grep(&dir, kek, &a.join(" "), tier),
        Some((cmd, a)) if cmd == "show" && !a.is_empty() => show(Path::new(&a[0]), kek, tier),
        Some((cmd, a)) if cmd == "tail" => {
            let n = a.first().and_then(|v| v.parse().ok()).unwrap_or(20);
            tail(&dir, kek, n, tier)
        }
        Some((cmd, a)) if cmd == "age" => age(&dir, kek, a),
        Some((cmd, a)) if cmd == "pin" && !a.is_empty() => pin(&dir, &a[0], true),
        Some((cmd, a)) if cmd == "unpin" && !a.is_empty() => pin(&dir, &a[0], false),
        Some((cmd, a)) if cmd == "delete" && a.len() >= 2 => delete(&dir, kek, a),
        _ => {
            eprintln!("usage: rill history list [--dir DIR]");
            eprintln!("       rill history grep <text> [--tier N]");
            eprintln!("       rill history show <segment.rhs>");
            eprintln!("       rill history tail [n]");
            eprintln!("       rill history age [--days N] [--apply]");
            eprintln!("       rill history pin|unpin <segment.rhs>");
            eprintln!("       rill history delete <from> <to> [--yes]");
            eprintln!("         times: unix seconds, or age like 3d / 12h / 45m");
            ExitCode::FAILURE
        }
    }
}

/// A point in time from the command line: unix seconds, or an age — `3d`,
/// `12h`, `45m` — measured back from now. Returns wall-clock ms.
fn parse_when(s: &str) -> Option<u64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    if let Some(n) = s.strip_suffix('d').and_then(|v| v.parse::<u64>().ok()) {
        return Some(now.saturating_sub(n * 86_400_000));
    }
    if let Some(n) = s.strip_suffix('h').and_then(|v| v.parse::<u64>().ok()) {
        return Some(now.saturating_sub(n * 3_600_000));
    }
    if let Some(n) = s.strip_suffix('m').and_then(|v| v.parse::<u64>().ok()) {
        return Some(now.saturating_sub(n * 60_000));
    }
    s.parse::<u64>().ok().map(|secs| secs * 1000)
}

/// Fidelity decay (specs/history.md decision 3): frames past the window go,
/// transcripts stay. Dry-run by default — the plan prints, `--apply` runs it.
fn age(dir: &Path, kek: Option<Kek>, args: &[String]) -> ExitCode {
    let unlock = kek.clone();
    use rill_history::retention;
    let mut days = retention::DEFAULT_FRAME_DAYS;
    let mut apply = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--days" => match args.get(i + 1).and_then(|v| v.parse().ok()) {
                Some(d) => {
                    days = d;
                    i += 2;
                }
                None => return fail("--days needs a number"),
            },
            "--apply" => {
                apply = true;
                i += 1;
            }
            other => return fail(&format!("age: unknown argument {other}")),
        }
    }
    let cands = retention::age_candidates(dir, days);
    if cands.is_empty() {
        println!("nothing past the {days}-day frame window in {}", dir.display());
        return ExitCode::SUCCESS;
    }
    if !apply {
        println!("would age {} segment(s) past {days} days (--apply to run):", cands.len());
        for c in &cands {
            println!(
                "  {}  ends {}{}",
                c.path.file_name().unwrap_or_default().to_string_lossy(),
                stamp(c.end_ms),
                if c.pinned { "  PINNED — will be skipped" } else { "" }
            );
        }
        return ExitCode::SUCCESS;
    }
    for (path, result) in retention::age_older_than(dir, days, unlock.as_ref()) {
        match result {
            Ok(r) => println!(
                "aged {}: {} -> {} events, {} -> {} bytes",
                path.file_name().unwrap_or_default().to_string_lossy(),
                r.events_before,
                r.events_after,
                r.bytes_before,
                r.bytes_after
            ),
            Err(e) => eprintln!("could not age {}: {e}", path.display()),
        }
    }
    ExitCode::SUCCESS
}

/// Pin: kept whole, forever, however old — and aging says so when it skips.
fn pin(dir: &Path, name: &str, on: bool) -> ExitCode {
    use rill_history::retention;
    let path = if Path::new(name).exists() { PathBuf::from(name) } else { dir.join(name) };
    if !path.exists() {
        return fail(&format!("{}: no such segment", path.display()));
    }
    let marker = retention::pin_path(&path);
    if on {
        if let Err(e) = std::fs::write(&marker, b"") {
            return fail(&format!("could not pin: {e}"));
        }
        println!("pinned {} (kept whole, aging skips it)", path.display());
    } else {
        if let Err(e) = std::fs::remove_file(&marker) {
            return fail(&format!("could not unpin: {e}"));
        }
        println!("unpinned {}", path.display());
    }
    ExitCode::SUCCESS
}

/// Hard delete by wall-clock range (specs/history.md decision 3: explicit,
/// destructive by intent, never disk pressure). Dry-run without `--yes`.
/// Pinned segments refuse — a pin is an explicit keep, and two explicit
/// intents in conflict is a decision for a person, not a default.
fn delete(dir: &Path, kek: Option<Kek>, args: &[String]) -> ExitCode {
    let unlock = kek.clone();
    use rill_history::retention;
    let (Some(from), Some(to)) = (parse_when(&args[0]), parse_when(&args[1])) else {
        return fail("delete: times are unix seconds or ages like 3d / 12h / 45m");
    };
    if from > to {
        return fail("delete: <from> is after <to>");
    }
    let yes = args.iter().any(|a| a == "--yes");
    let corpus = match open(dir, kek) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let mut plans: Vec<(PathBuf, bool)> = Vec::new(); // (path, whole-segment)
    for s in corpus.segments() {
        let (seg_from, seg_to) = s.wall_range();
        if seg_to < from || seg_from > to {
            continue;
        }
        plans.push((s.path.clone(), from <= seg_from && seg_to <= to));
    }
    if plans.is_empty() {
        println!("nothing recorded in {} — {}", stamp(from), stamp(to));
        return ExitCode::SUCCESS;
    }
    if !yes {
        println!(
            "would delete {} — {} (--yes to run):",
            stamp(from),
            stamp(to)
        );
        for (path, whole) in &plans {
            let pinned = retention::is_pinned(path);
            println!(
                "  {}  {}{}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                if *whole { "remove entirely" } else { "cut the range out" },
                if pinned { "  PINNED — will refuse" } else { "" }
            );
        }
        return ExitCode::SUCCESS;
    }
    for (path, whole) in plans {
        if retention::is_pinned(&path) {
            eprintln!(
                "refusing {}: pinned (unpin first if you mean it)",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            continue;
        }
        if whole {
            match std::fs::remove_file(&path) {
                Ok(()) => println!("deleted {}", path.display()),
                Err(e) => eprintln!("could not delete {}: {e}", path.display()),
            }
            continue;
        }
        match retention::rewrite(&path, unlock.as_ref(), |wall, _| !(from..=to).contains(&wall)) {
            Ok(r) => println!(
                "cut {}: {} -> {} events",
                path.file_name().unwrap_or_default().to_string_lossy(),
                r.events_before,
                r.events_after
            ),
            Err(e) => eprintln!("could not cut {}: {e}", path.display()),
        }
    }
    ExitCode::SUCCESS
}

fn fail(message: &str) -> ExitCode {
    eprintln!("rill history: {message}");
    ExitCode::FAILURE
}

fn open(dir: &Path, kek: Option<Kek>) -> Result<Corpus, ExitCode> {
    let corpus = Corpus::open_with(dir, kek).map_err(|e| {
        eprintln!("rill history: {}: {e}", dir.display());
        ExitCode::FAILURE
    })?;
    // A locked segment is skipped by the scan, and a skipped segment must
    // not look like an absent one: "0 hits" over a corpus that could not be
    // opened is the silent-empty read the Locked error exists to prevent.
    let on_disk = std::fs::read_dir(dir)
        .map(|es| {
            es.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "rhs")).count()
        })
        .unwrap_or(0);
    let seen = corpus.segments().len();
    if on_disk > seen {
        eprintln!(
            "rill history: {} of {} segment(s) locked or unreadable — encrypted history needs the device identity (--identity, default ~/.config/rill)",
            on_disk - seen,
            on_disk
        );
    }
    Ok(corpus)
}

/// Milliseconds since the epoch as a readable local-ish stamp. Deliberately
/// hand-rolled rather than pulling a date crate into the CLI for one line:
/// the CLI prints times, it does not compute with them.
fn stamp(ms: u64) -> String {
    let secs = ms / 1000;
    let (d, rem) = (secs / 86_400, secs % 86_400);
    // Days since epoch → y/m/d, civil-from-days (Howard Hinnant's algorithm).
    let z = d as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn human_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f >= K * K * K {
        format!("{:.1} GiB", f / (K * K * K))
    } else if f >= K * K {
        format!("{:.1} MiB", f / (K * K))
    } else if f >= K {
        format!("{:.0} KiB", f / K)
    } else {
        format!("{n} B")
    }
}

fn list(dir: &Path, kek: Option<Kek>, _args: &[String]) -> ExitCode {
    let corpus = match open(dir, kek) {
        Ok(c) => c,
        Err(e) => return e,
    };
    if corpus.segments().is_empty() {
        println!("no history in {}", dir.display());
        return ExitCode::SUCCESS;
    }
    let total_bytes: u64 = corpus.segments().iter().map(|s| s.size).sum();
    println!("{} segments in {}", corpus.segments().len(), dir.display());
    for s in corpus.segments() {
        let (from, to) = s.wall_range();
        let tiers: Vec<String> = s.tiers.iter().map(|t| format!("T{t}")).collect();
        // Sealed or open is worth a glance: a sealed segment has verified
        // wholeness and a stored index; an open one is live (or a crash
        // waiting for its recovery seal).
        let sealed = matches!(rill_history::segment::read_seal(&s.path), Ok(Some(_)));
        println!(
            "  {:<22} {}  →  {}  {:>7} ev  {:>9}  {}  {}",
            s.path.file_name().unwrap_or_default().to_string_lossy(),
            stamp(from),
            stamp(to).split(' ').nth(1).unwrap_or(""),
            s.events,
            human_bytes(s.size),
            tiers.join(","),
            if sealed { "sealed" } else { "open" }
        );
    }
    println!(
        "  {:-<22} {} events, {}",
        "",
        corpus.total_events(),
        human_bytes(total_bytes)
    );
    ExitCode::SUCCESS
}

fn grep(dir: &Path, kek: Option<Kek>, query: &str, tier: Tier) -> ExitCode {
    let corpus = match open(dir, kek) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let started = std::time::Instant::now();
    let (hits, opened) = corpus.search(query, tier, 50);
    let elapsed = started.elapsed();

    for h in &hits {
        let snippet: String = h.text.chars().take(100).collect();
        println!(
            "{}  {}  +{}ms  {}",
            stamp(h.wall_ms),
            h.title,
            h.t_ms,
            snippet.replace('\n', " ")
        );
    }
    // The selectivity is the interesting number, and saying it honestly is
    // how the corpus-scale claim stays checkable rather than marketing.
    println!(
        "\n{} hits in {:.1?} — opened {}/{} segments",
        hits.len(),
        elapsed,
        opened,
        corpus.segments().len()
    );
    if let Some(h) = hits.first() {
        println!("replay: rill history show {} (at +{}ms)", h.segment.display(), h.t_ms);
    }
    ExitCode::SUCCESS
}

fn show(path: &Path, kek: Option<Kek>, tier: Tier) -> ExitCode {
    let seg = match rill_history::segment::read_with(path, kek.as_ref()) {
        Ok(s) => s,
        Err(e) => return fail(&format!("{}: {e}", path.display())),
    };
    if let Some(why) = &seg.stopped {
        println!("(tail torn: {why})");
    }
    // The stored index answers when the segment is sealed; the rebuild is
    // the fallback for open segments and tiers that stored nothing.
    let idx = seg
        .seal
        .as_ref()
        .and_then(|seal| seal.indexes.iter().find(|i| i.tier == tier).cloned())
        .unwrap_or_else(|| rill_history::index::build(&seg.events, tier));
    println!(
        "{}: {} events, {} transcript entries, span {:?} ms",
        path.display(),
        seg.events.len(),
        idx.transcript.len(),
        idx.span
    );
    for e in &idx.transcript {
        let snippet: String = e.text.chars().take(110).collect();
        println!("  {:>8} ms  win {:<3} {}", e.t_ms, e.window, snippet.replace('\n', " "));
    }
    ExitCode::SUCCESS
}

fn tail(dir: &Path, kek: Option<Kek>, n: usize, tier: Tier) -> ExitCode {
    let corpus = match open(dir, kek) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let started = std::time::Instant::now();
    let entries = corpus.tail(n, tier);
    let elapsed = started.elapsed();
    for h in &entries {
        let snippet: String = h.text.chars().take(110).collect();
        println!("{}  {}", stamp(h.wall_ms), snippet.replace('\n', " "));
    }
    println!("\n{} entries in {:.1?} — the agent's standing view", entries.len(), elapsed);
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stamp is hand-rolled, so it gets a test rather than trust.
    #[test]
    fn stamps_known_instants() {
        assert_eq!(stamp(0), "1970-01-01 00:00:00");
        // Cross-checked against a reference implementation, not guessed.
        assert_eq!(stamp(1_786_536_000_000), "2026-08-12 12:00:00");
        // A leap day, since the civil-from-days algorithm is where those bite.
        assert_eq!(stamp(1_709_208_000_000), "2024-02-29 12:00:00");
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
