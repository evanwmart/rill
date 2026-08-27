//! Rill command-line client.
//!
//! * `rill get rill://host[:port]/path [-o FILE|-] [--identity DIR] [--dump-frames DIR]`
//! * `rill auth …` — identity and enrollment (see auth_cmd.rs)
//! * `rill inspect <file>...` — decode and print raw protocol bytes, or a
//!   `.rillrec` session recording as a timeline.
//! * `rill history list|grep|show|tail` — semantic history (specs/history.md).

mod app_cmd;
mod auth_cmd;
mod doc_cmd;
mod history_cmd;
mod pack_cmd;

use std::process::ExitCode;

use rill_client::{Client, ClientConfig, RillUrl};
use rill_protocol::{FLAG_MORE, Frame, HEADER_LEN, decode_header, decode_payload};

fn main() -> ExitCode {
    // `rill history grep | head` closes our stdout mid-print; Rust turns the
    // resulting EPIPE into a panic by default. Restoring SIGPIPE's default
    // disposition makes a closed pipe end the process quietly, the way every
    // other CLI in the pipeline behaves.
    // SAFETY: setting a signal disposition before anything else runs.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.split_first() {
        Some((cmd, files)) if cmd == "inspect" && !files.is_empty() => {
            let mut ok = true;
            // `-v` on a recording prints the fills each frame opens with,
            // which is what "why does this window look like that" turns on.
            let verbose = files.iter().any(|f| *f == "-v");
            let files: Vec<&String> = files.iter().filter(|f| **f != "-v").collect();
            for file in &files {
                if files.len() > 1 {
                    println!("== {file}");
                }
                if let Err(message) = inspect(file, verbose) {
                    eprintln!("{file}: {message}");
                    ok = false;
                }
            }
            if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Some((cmd, rest)) if cmd == "get" && !rest.is_empty() => cmd_get(rest),
        Some((cmd, rest)) if cmd == "head" && !rest.is_empty() => cmd_head(rest),
        Some((cmd, rest)) if cmd == "action" && !rest.is_empty() => cmd_action(rest),
        Some((cmd, rest)) if cmd == "auth" => auth_cmd::run(rest),
        Some((cmd, rest)) if cmd == "cache" => cmd_cache(rest),
        Some((cmd, rest)) if cmd == "pack" => pack_cmd::run(rest),
        Some((cmd, rest)) if cmd == "doc" => doc_cmd::run(rest),
        Some((cmd, rest)) if cmd == "app" => app_cmd::run(rest),
        Some((cmd, rest)) if cmd == "history" => history_cmd::run(rest),
        _ => {
            eprintln!(
                "usage: rill get <rill://host[:port]/path> [-o FILE|-] [--identity DIR] [--cache DIR|--no-cache] [--dump-frames DIR]"
            );
            eprintln!(
                "       rill head <rill://host[:port]/path>          size and content hash"
            );
            eprintln!(
                "       rill action <rill://host[:port]/path> [name=string] [name:=number|bool] ... [--expect HASH]"
            );
            eprintln!("       rill auth init|init-server|fingerprint|enroll|trust ...");
            eprintln!("       rill cache stats|verify|clear [--cache DIR]");
            eprintln!("       rill pack build|inspect|extract|verify ...");
            eprintln!("       rill doc compile|inspect ...");
            eprintln!("       rill app install|list|update|remove ...");
            eprintln!("       rill history list|grep|show|tail ...");
            eprintln!("       rill inspect [-v] <file>...");
            ExitCode::FAILURE
        }
    }
}

fn cmd_cache(args: &[String]) -> ExitCode {
    let sub = args.first().map(String::as_str);
    let cache_dir = match args.iter().position(|a| a == "--cache") {
        Some(i) => match args.get(i + 1) {
            Some(v) => v.into(),
            None => {
                eprintln!("rill cache: --cache needs a value");
                return ExitCode::FAILURE;
            }
        },
        None => default_cache_dir(),
    };
    let fail = |m: String| {
        eprintln!("rill cache: {m}");
        ExitCode::FAILURE
    };
    match sub {
        Some("stats") => {
            let cache = match rill_store::Cache::open(&cache_dir) {
                Ok(c) => c,
                Err(e) => return fail(e.to_string()),
            };
            let objects = match cache.objects.verify_all() {
                Ok(v) => v,
                Err(e) => return fail(e.to_string()),
            };
            let refs = cache.refs.count().unwrap_or(0);
            let bytes = cache.objects.total_bytes();
            println!("cache: {}", cache.root().display());
            println!("objects: {} ({:.1} MiB)", objects.len(), bytes as f64 / 1048576.0);
            println!("refs: {refs}");
            // The number that matters is not how big the cache is but how
            // much of it anything can still reach: a content-addressed store
            // with moving refs accumulates orphans, and "objects: 7845" reads
            // as a healthy cache when 95% of it is garbage.
            println!(
                "budget: {:.0} MiB (sweeps on connect above this)",
                rill_store::Cache::DEFAULT_BUDGET as f64 / 1048576.0
            );
            ExitCode::SUCCESS
        }
        Some("sweep") => {
            let cache = match rill_store::Cache::open(&cache_dir) {
                Ok(c) => c,
                Err(e) => return fail(e.to_string()),
            };
            let before = cache.objects.total_bytes();
            let swept = match cache.sweep() {
                Ok(s) => s,
                Err(e) => return fail(e.to_string()),
            };
            println!(
                "swept {} unreferenced object(s), freed {:.1} MiB; {} kept ({:.1} MiB → {:.1} MiB)",
                swept.removed,
                swept.freed_bytes as f64 / 1048576.0,
                swept.kept,
                before as f64 / 1048576.0,
                cache.objects.total_bytes() as f64 / 1048576.0,
            );
            ExitCode::SUCCESS
        }
        Some("verify") => {
            let cache = match rill_store::Cache::open(&cache_dir) {
                Ok(c) => c,
                Err(e) => return fail(e.to_string()),
            };
            let results = match cache.objects.verify_all() {
                Ok(v) => v,
                Err(e) => return fail(e.to_string()),
            };
            let mut bad = 0;
            for (hash, ok) in &results {
                if !ok {
                    bad += 1;
                    println!("CORRUPT blake3:{hash}");
                }
            }
            println!("{} object(s) verified, {bad} corrupt", results.len());
            if bad == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Some("clear") => {
            if !cache_dir.exists() {
                println!("cache already empty: {}", cache_dir.display());
                return ExitCode::SUCCESS;
            }
            // Only remove the two directories the cache owns — never the
            // root wholesale, in case someone points --cache somewhere odd.
            for sub in ["objects", "refs"] {
                let dir = cache_dir.join(sub);
                if dir.exists()
                    && let Err(e) = std::fs::remove_dir_all(&dir)
                {
                    return fail(format!("{}: {e}", dir.display()));
                }
            }
            println!("cleared {}", cache_dir.display());
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: rill cache stats|verify|sweep|clear [--cache DIR]");
            ExitCode::FAILURE
        }
    }
}

use rill_client::util::default_cache_dir;

/// Address plus credentials for a one-shot command: parse the URL, look up
/// the pinned server fingerprint and this device's identity, build the
/// client config. Every verb needs exactly this, and a verb that skipped it
/// would be a verb that talks to an unverified server.
fn connection_for(
    cmd: &str,
    url_str: &str,
    identity_dir: &std::path::Path,
    cache_dir: Option<std::path::PathBuf>,
    dump_frames: Option<String>,
) -> Option<(RillUrl, ClientConfig)> {
    let url = match RillUrl::parse(url_str) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("rill {cmd}: {e}");
            return None;
        }
    };
    // Pinned server fingerprint + optional device identity (security.md §4).
    let (fingerprint, device) =
        match auth_cmd::client_identity_for(identity_dir, &url.host, url.port) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("rill {cmd}: {e}");
                return None;
            }
        };
    let mut cfg = ClientConfig::new(fingerprint);
    cfg.device = device;
    cfg.dump_frames = dump_frames.map(Into::into);
    cfg.cache_dir = cache_dir;
    Some((url, cfg))
}

/// The flags every one-shot verb shares.
struct CommonFlags {
    identity_dir: std::path::PathBuf,
    cache_dir: Option<std::path::PathBuf>,
    dump_frames: Option<String>,
    output: Option<String>,
}

impl Default for CommonFlags {
    fn default() -> CommonFlags {
        CommonFlags {
            identity_dir: auth_cmd::default_identity_dir(),
            cache_dir: Some(default_cache_dir()),
            dump_frames: None,
            output: None,
        }
    }
}

impl CommonFlags {
    /// Consume a recognised flag at `i`, returning how many arguments it
    /// took. `None` means "not one of mine" — the caller decides whether that
    /// is its own flag or an error.
    fn take(&mut self, args: &[String], i: usize) -> Option<usize> {
        match args[i].as_str() {
            "--no-cache" => {
                self.cache_dir = None;
                Some(1)
            }
            flag @ ("-o" | "--dump-frames" | "--identity" | "--cache") => {
                let value = args.get(i + 1)?;
                match flag {
                    "-o" => self.output = Some(value.clone()),
                    "--dump-frames" => self.dump_frames = Some(value.clone()),
                    "--identity" => self.identity_dir = value.into(),
                    _ => self.cache_dir = Some(value.into()),
                }
                Some(2)
            }
            _ => None,
        }
    }
}

/// `rill head rill://host/path` — size and content hash, without the body.
///
/// The hash is the revision a caller observed: hold onto it, and an action
/// can later be made conditional on the resource not having moved since.
fn cmd_head(args: &[String]) -> ExitCode {
    let mut flags = CommonFlags::default();
    let mut i = 1;
    while i < args.len() {
        match flags.take(args, i) {
            Some(n) => i += n,
            None => {
                eprintln!("rill head: unknown flag {}", args[i]);
                return ExitCode::FAILURE;
            }
        }
    }
    let Some((url, cfg)) =
        connection_for("head", &args[0], &flags.identity_dir, flags.cache_dir, flags.dump_frames)
    else {
        return ExitCode::FAILURE;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let mut client = match Client::connect(&url.host, url.port, cfg).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("rill head: {e}");
                return ExitCode::FAILURE;
            }
        };
        let result = client.head(&url.path).await;
        client.close().await;
        match result {
            Ok(meta) => {
                println!("size: {}", meta.size);
                match meta.hash {
                    // METADATA v2. Printed in the form every other hash in
                    // the system is written in, so it can be pasted straight
                    // into `rill action --expect`.
                    Some(hash) => println!("hash: {hash}"),
                    None => println!("hash: (server sent METADATA v1)"),
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("rill head: {e}");
                ExitCode::FAILURE
            }
        }
    })
}

/// `rill action rill://host/path name=value name:=42 ...`
///
/// The write verb, headless. `=` sends a string; `:=` sends a number or a
/// bool — the three types the wire has, named the way the shell can express
/// them without quoting games.
fn cmd_action(args: &[String]) -> ExitCode {
    let mut flags = CommonFlags::default();
    let mut fields: Vec<(String, rill_protocol::ActionValue)> = Vec::new();
    let mut expect: Option<rill_store::Hash> = None;
    let mut i = 1;
    while i < args.len() {
        if let Some(n) = flags.take(args, i) {
            i += n;
            continue;
        }
        if args[i] == "--expect" {
            let Some(raw) = args.get(i + 1) else {
                eprintln!("rill action: --expect needs a hash (see `rill head`)");
                return ExitCode::FAILURE;
            };
            let Some(hash) = rill_store::Hash::from_hex(raw) else {
                eprintln!("rill action: --expect {raw} is not a blake3:<64 hex> hash");
                return ExitCode::FAILURE;
            };
            expect = Some(hash);
            i += 2;
            continue;
        }
        let arg = &args[i];
        let parsed = match arg.split_once(":=") {
            Some((name, raw)) => match typed_value(raw) {
                Some(value) => Some((name.to_string(), value)),
                None => {
                    eprintln!(
                        "rill action: {name}:={raw} — `:=` takes a number or true/false \
                         (use `=` for a string)"
                    );
                    return ExitCode::FAILURE;
                }
            },
            None => arg
                .split_once('=')
                .map(|(name, raw)| (name.to_string(), rill_protocol::ActionValue::Str(raw.into()))),
        };
        match parsed {
            Some((name, _)) if name.is_empty() => {
                eprintln!("rill action: a field needs a name");
                return ExitCode::FAILURE;
            }
            Some(field) => fields.push(field),
            None => {
                eprintln!("rill action: unknown flag or malformed field {arg}");
                eprintln!("            fields are name=string or name:=number|bool");
                return ExitCode::FAILURE;
            }
        }
        i += 1;
    }
    if fields.len() > rill_protocol::MAX_ACTION_FIELDS {
        eprintln!(
            "rill action: {} fields, the protocol allows {}",
            fields.len(),
            rill_protocol::MAX_ACTION_FIELDS
        );
        return ExitCode::FAILURE;
    }

    // An action is never cached — it is not a fetch — so the cache is not
    // opened for it at all.
    let Some((url, cfg)) =
        connection_for("action", &args[0], &flags.identity_dir, None, flags.dump_frames)
    else {
        return ExitCode::FAILURE;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let mut client = match Client::connect(&url.host, url.port, cfg).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("rill action: {e}");
                return ExitCode::FAILURE;
            }
        };
        let result = match expect {
            Some(hash) => client.action_if(&url.path, fields, hash).await,
            None => client.action(&url.path, fields).await,
        };
        client.close().await;
        match result {
            Ok(bytes) => write_response(&bytes, flags.output.as_deref()),
            Err(rill_client::ClientError::Server {
                status: rill_protocol::Status::Conflict,
                ..
            }) => {
                // Worth its own message: the action did not happen, nothing
                // was written, and the caller's next move is to re-read.
                eprintln!(
                    "rill action: CONFLICT — the resource changed since the revision you \
                     passed. Nothing was written; re-read it (`rill head`) and try again."
                );
                ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("rill action: {e}");
                ExitCode::FAILURE
            }
        }
    })
}

/// `:=` values: a finite number, or a bool. Anything else is a string and
/// should have been written with `=`.
fn typed_value(raw: &str) -> Option<rill_protocol::ActionValue> {
    match raw {
        "true" => return Some(rill_protocol::ActionValue::Bool(true)),
        "false" => return Some(rill_protocol::ActionValue::Bool(false)),
        _ => {}
    }
    raw.parse::<f64>().ok().filter(|n| n.is_finite()).map(rill_protocol::ActionValue::Num)
}

/// Write a response body where the caller asked for it.
///
/// With no `-o`, bytes that are a compiled document are summarised rather
/// than dumped: a `.rill` file is not something a terminal should be asked
/// to display, and the common headless case — an adapter answering with
/// text — still goes to stdout where a pipe can reach it.
fn write_response(bytes: &[u8], output: Option<&str>) -> ExitCode {
    use std::io::Write;
    let is_document = bytes.starts_with(b"RDOC");
    let write = |target: &str| -> std::io::Result<()> {
        if target == "-" {
            std::io::stdout().write_all(bytes)
        } else {
            std::fs::write(target, bytes)
        }
    };
    let target = match output {
        Some(t) => t,
        None if is_document => {
            eprintln!(
                "rill: {} bytes — a Rill document (use -o FILE to save it, \
                 or `rill doc inspect` to read it)",
                bytes.len()
            );
            return ExitCode::SUCCESS;
        }
        None => "-",
    };
    if let Err(e) = write(target) {
        eprintln!("rill: writing {target}: {e}");
        return ExitCode::FAILURE;
    }
    if target != "-" {
        eprintln!("rill: {} bytes → {target}", bytes.len());
    }
    ExitCode::SUCCESS
}

fn cmd_get(args: &[String]) -> ExitCode {
    let url_str = &args[0];
    let mut flags = CommonFlags::default();
    let mut i = 1;
    while i < args.len() {
        match flags.take(args, i) {
            Some(n) => i += n,
            None => {
                eprintln!("rill get: unknown flag {}", args[i]);
                return ExitCode::FAILURE;
            }
        }
    }
    let Some((url, cfg)) = connection_for(
        "get",
        url_str,
        &flags.identity_dir,
        flags.cache_dir.clone(),
        flags.dump_frames.clone(),
    ) else {
        return ExitCode::FAILURE;
    };
    // Default output name: last path segment ("index" for the root path).
    let output = flags.output.clone().unwrap_or_else(|| {
        url.path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("index").to_string()
    });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let started = std::time::Instant::now();
        let mut client = match Client::connect(&url.host, url.port, cfg).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("rill get: {e}");
                return ExitCode::FAILURE;
            }
        };
        let result = client.get(&url.path).await;
        client.close().await;
        match result {
            Ok(fetched) => {
                let elapsed = started.elapsed();
                let write_result = if output == "-" {
                    use std::io::Write;
                    std::io::stdout().write_all(&fetched.data)
                } else {
                    std::fs::write(&output, &fetched.data)
                };
                if let Err(e) = write_result {
                    eprintln!("rill get: writing {output}: {e}");
                    return ExitCode::FAILURE;
                }
                eprintln!(
                    "rill get: {} → {} ({} bytes in {:.1?}{})",
                    url_str,
                    if output == "-" { "stdout" } else { &output },
                    fetched.data.len(),
                    elapsed,
                    if fetched.from_cache { ", NOT_MODIFIED — served from cache" } else { "" }
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("rill get: {e}");
                ExitCode::FAILURE
            }
        }
    })
}

fn inspect(path: &str, verbose: bool) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.starts_with(&rill_ui::recording::RECORDING_MAGIC) {
        return inspect_recording(&bytes, verbose);
    }
    let mut offset = 0usize;
    let mut index = 0usize;

    while offset < bytes.len() {
        index += 1;
        let remaining = &bytes[offset..];
        if remaining.len() < HEADER_LEN {
            return Err(format!(
                "frame {index} at offset {offset}: truncated header ({} of {HEADER_LEN} bytes)",
                remaining.len()
            ));
        }
        let header = decode_header(remaining[..HEADER_LEN].try_into().unwrap())
            .map_err(|e| format!("frame {index} at offset {offset}: {e}"))?;
        let payload_end = HEADER_LEN + header.payload_len as usize;
        if remaining.len() < payload_end {
            return Err(format!(
                "frame {index} at offset {offset}: truncated payload ({} of {} bytes)",
                remaining.len() - HEADER_LEN,
                header.payload_len
            ));
        }
        let frame = decode_payload(&header, &remaining[HEADER_LEN..payload_end])
            .map_err(|e| format!("frame {index} at offset {offset}: {e}"))?;

        println!("Frame: {}", header.frame_type.name());
        println!("Version: {}", header.version);
        println!("Request ID: {}", header.request_id);
        if header.flags != 0 {
            let mut names = Vec::new();
            for (mask, name) in [
                (FLAG_MORE, "MORE"),
                (rill_protocol::FLAG_CONTENT_ZSTD, "CONTENT_ZSTD"),
                (rill_protocol::FLAG_ACCEPT_ZSTD, "ACCEPT_ZSTD"),
            ] {
                if header.flags & mask != 0 {
                    names.push(name.to_string());
                }
            }
            let other = header.flags
                & !(FLAG_MORE | rill_protocol::FLAG_CONTENT_ZSTD | rill_protocol::FLAG_ACCEPT_ZSTD);
            if other != 0 {
                names.push(format!("0x{other:04X}"));
            }
            println!("Flags: {}", names.join(" | "));
        }
        println!("Payload bytes: {}", header.payload_len);
        match &frame {
            Frame::Get { path, .. } | Frame::Head { path, .. } => {
                println!("Path: {path}");
            }
            Frame::Action { path, fields, .. } => {
                println!("Path: {path}");
                for (name, value) in fields {
                    println!("Field: {name} = {value:?}");
                }
            }
            Frame::GetIf { path, hash, .. } => {
                println!("Path: {path}");
                println!("If-hash: {}", rill_store::Hash(*hash));
            }
            Frame::Metadata { size, hash, .. } => {
                println!("Size: {size}");
                if let Some(hash) = hash {
                    println!("Hash: {}", rill_store::Hash(*hash));
                }
            }
            Frame::NotModified { .. } => {}
            Frame::Error { status, message, .. } => {
                println!("Status: {status}");
                if !message.is_empty() {
                    println!("Message: {message}");
                }
            }
            Frame::Resource { more, .. } => {
                println!("Final chunk: {}", if *more { "no" } else { "yes" });
            }
            Frame::Ping { .. } | Frame::Pong { .. } | Frame::Close => {}
        }
        println!();

        offset += payload_end;
    }

    if index == 0 {
        return Err("empty file".into());
    }
    Ok(())
}

/// Print a `.rillrec` session recording as a timeline. Uses the tolerant
/// reader: a recording whose session was killed rather than stopped ends
/// mid-event, and showing everything up to that point beats refusing the file.
fn inspect_recording(bytes: &[u8], verbose: bool) -> Result<(), String> {
    use rill_ui::recording::{RecEvent, decode_lossy};

    let (width, height, events, stopped) = decode_lossy(bytes).map_err(|e| e.to_string())?;
    println!("Recording: {width}x{height}, {} events", events.len());
    if let Some(last) = events.last() {
        let secs = last.t_ms as f64 / 1000.0;
        println!("Duration: {secs:.1}s");
    }
    println!();
    for stamped in &events {
        print!("{:>8}ms  ", stamped.t_ms);
        match &stamped.event {
            RecEvent::Window { id, x, y, w, h, title, vector } => {
                let kind = if *vector { "vector" } else { "pixel" };
                println!("window {id} {kind} {w}x{h}+{x}+{y} {title:?}");
            }
            RecEvent::Closed { id } => println!("closed {id}"),
            RecEvent::Order { ids } => println!("order {ids:?}"),
            RecEvent::Frame { id, bytes } => {
                // The blob is a stream encoding — say how much it draws, since
                // that is the whole claim of recording semantically.
                match rill_ui::stream::decode(bytes) {
                    Ok(cmds) => {
                        println!("frame {id} {} bytes, {} commands", bytes.len(), cmds.len());
                        // The fills a frame opens with are what "why does this
                        // window look like that" turns on: a frost, a body
                        // tint, a panel. Counting commands never answered it.
                        if verbose {
                            for c in cmds.iter().take(6) {
                                match c {
                                    rill_ui::DrawCommand::Backdrop { rect, blur, .. } => println!(
                                        "              backdrop {:.0}x{:.0} blur {blur:.0}",
                                        rect.w, rect.h
                                    ),
                                    rill_ui::DrawCommand::Rect { rect, color, .. } => println!(
                                        "              rect {:.0}x{:.0} rgba({},{},{},{})",
                                        rect.w, rect.h, color.r, color.g, color.b, color.a
                                    ),
                                    other => println!("              {other:?}"),
                                }
                            }
                        }
                    }
                    Err(e) => println!("frame {id} {} bytes, undecodable: {e}", bytes.len()),
                }
            }
            RecEvent::Pointer { x, y } => println!("pointer {x:.0},{y:.0}"),
        }
    }
    if let Some(why) = stopped {
        println!();
        println!("Recording ends mid-event: {why}");
    }
    Ok(())
}
