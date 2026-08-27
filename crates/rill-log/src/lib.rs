//! The log vocabulary every Rill process shares, lifted out of the server
//! the day the second consumer arrived (the TODO called this trigger years
//! before it fired). Two outputs, two audiences:
//!
//! * **stderr lines** — for a person watching a process. Levelled, off-ish
//!   by default (`Debug` costs a comparison and nothing else), formatted
//!   as `[proc] LEVEL event key=value`.
//! * **the dev trail** — for an agent reconstructing what happened. One
//!   JSONL file merging every process's events in wall-clock order, gated
//!   on `RILL_DEV_LOG=<path>`: absent, the whole facility is a `None`
//!   check. This is scaffolding, not product — distinct on purpose from
//!   the history substrate, which is tiered, recorded, and owned by the
//!   person. The trail is opt-in exhaust for a machine that is being
//!   debugged, and it never carries typed text — key *names*, paths,
//!   labels, errors; never the contents of an input field.
//!
//! The debugging sessions that justified it: a week of bugs where the fix
//! took three screenshots and a guess each, because the ordered causal
//! trail — click, action, page, error — existed nowhere.

use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    /// Something failed and the connection or the process is affected.
    Error,
    /// Refused, denied, or unrecognised — the security-relevant lines.
    Warn,
    /// Lifecycle: bound, connected, closed.
    Info,
    /// One line per request/event. Off by default, and deliberately so.
    Debug,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
        }
    }
}

/// The stderr threshold, read once. `RILL_LOG=requests|debug|trace` opens
/// the per-request lines; `RILL_LOG=quiet` is warnings and errors only —
/// the appliance-service setting.
pub fn level() -> Level {
    static LEVEL: OnceLock<Level> = OnceLock::new();
    *LEVEL.get_or_init(|| match std::env::var("RILL_LOG").as_deref() {
        Ok("requests") | Ok("debug") | Ok("trace") => Level::Debug,
        Ok("quiet") => Level::Warn,
        _ => Level::Info,
    })
}

/// Append `key=value`, quoting values that would break the field split.
/// Values are application text — device names, error strings — so plenty
/// contain spaces, and unquoted they would turn one field into several.
pub fn push_field(line: &mut String, key: &str, value: &str) {
    line.push(' ');
    line.push_str(key);
    line.push('=');
    if value.is_empty() || value.contains([' ', '"', '=', '\n']) {
        line.push('"');
        for c in value.chars() {
            match c {
                '"' | '\\' => {
                    line.push('\\');
                    line.push(c);
                }
                '\n' => line.push_str("\\n"),
                _ => line.push(c),
            }
        }
        line.push('"');
    } else {
        line.push_str(value);
    }
}

/// The dev-trail sink: the file named by `RILL_DEV_LOG`, opened once,
/// appended forever. `None` when unset, which must stay the cheap path.
fn dev_sink() -> Option<&'static Mutex<std::fs::File>> {
    static SINK: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    SINK.get_or_init(|| {
        let path = std::env::var_os("RILL_DEV_LOG")?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    })
    .as_ref()
}

/// Whether the trail is live — for callers that would do work to *build*
/// an event (formatting a path, counting nodes) and want to skip it cold.
pub fn dev_active() -> bool {
    dev_sink().is_some()
}

fn json_escape(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
}

/// One trail event: process, event name, fields — timestamped here so
/// every process's clock is the same clock.
pub fn dev_emit(proc_name: &str, event: &str, fields: &[(&str, &str)]) {
    let Some(sink) = dev_sink() else { return };
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    let mut line = String::with_capacity(96);
    line.push_str("{\"t\":");
    line.push_str(&t.to_string());
    line.push_str(",\"proc\":\"");
    json_escape(&mut line, proc_name);
    line.push_str("\",\"event\":\"");
    json_escape(&mut line, event);
    line.push('"');
    for (k, v) in fields {
        line.push_str(",\"");
        json_escape(&mut line, k);
        line.push_str("\":\"");
        json_escape(&mut line, v);
        line.push('"');
    }
    line.push_str("}\n");
    if let Ok(mut f) = sink.lock() {
        let _ = f.write_all(line.as_bytes());
    }
}

/// One stderr line, and — when the trail is live — the same event into the
/// trail whatever the stderr threshold says: the trail exists precisely
/// for the lines nobody had turned on when the bug happened.
pub fn emit(proc_name: &str, level: Level, conn: u64, event: &str, fields: &str) {
    if level <= self::level() {
        if conn == 0 {
            eprintln!("[{proc_name}] {} {event}{fields}", level.name());
        } else {
            eprintln!("[{proc_name}] {} conn={conn} {event}{fields}", level.name());
        }
    }
    if dev_active() {
        let conn_s = conn.to_string();
        let mut fs: Vec<(&str, &str)> = vec![("level", level.name())];
        if conn != 0 {
            fs.push(("conn", &conn_s));
        }
        // The stderr fields ride along pre-rendered: the trail's consumer
        // is a machine that greps, and `fields="path=/x result=ok"` greps.
        fs.push(("fields", fields.trim_start()));
        dev_emit(proc_name, event, &fs);
    }
}

/// A trail-only event: never stderr, no level — the navigation, the key
/// *name*, the tick that failed. Free when the trail is cold.
#[macro_export]
macro_rules! dev {
    ($proc:expr, $event:expr $(, $key:ident = $value:expr)* $(,)?) => {
        if $crate::dev_active() {
            $crate::dev_emit($proc, $event, &[
                $( (stringify!($key), &$value.to_string() as &str), )*
            ]);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole sink contract in one test (one test on purpose: the sink
    /// is a process-wide OnceLock, so whoever initialises it first wins).
    #[test]
    fn the_trail_is_jsonl_and_never_typed_text() {
        let path = std::env::temp_dir().join(format!("rill-devlog-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // SAFETY: single-threaded at this point in this test binary; the
        // sink reads the variable exactly once.
        unsafe { std::env::set_var("RILL_DEV_LOG", &path) };

        assert!(dev_active());
        dev_emit("test-proc", "navigate", &[("path", "/edit/open/a.rs")]);
        dev_emit("test-proc", "key", &[("key", "s"), ("ctrl", "true")]);
        emit("test-proc", Level::Warn, 3, "refused", " path=/x reason=\"no grant\"");

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        for l in &lines {
            assert!(l.starts_with("{\"t\":") && l.ends_with('}'), "not JSONL: {l}");
            assert!(l.contains("\"proc\":\"test-proc\""));
        }
        assert!(lines[0].contains("\"event\":\"navigate\""));
        assert!(lines[2].contains("\"level\":\"WARN\"") && lines[2].contains("no grant"));
        let _ = std::fs::remove_file(&path);
    }
}
