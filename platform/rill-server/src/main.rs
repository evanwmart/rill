//! `rill-server serve <root> --identity <dir> [--bind ADDR] [--port N] [--dump-frames DIR]`

use std::process::ExitCode;

use rill_server::{Server, ServerConfig};

/// Request-serving threads, and with them the ceiling on malloc arenas.
/// Four is plenty for serving documents, and choosing this number is
/// choosing the footprint.
const WORKERS: usize = 4;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(("serve", rest)) = args.split_first().map(|(c, r)| (c.as_str(), r)) else {
        return usage();
    };
    let Some((root, flags)) = rest.split_first() else {
        return usage();
    };

    let mut identity: Option<String> = None;
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 7331;
    let mut dump_frames: Option<String> = None;
    let mut it = flags.iter();
    while let Some(flag) = it.next() {
        match (flag.as_str(), it.next()) {
            ("--identity", Some(v)) => identity = Some(v.clone()),
            ("--bind", Some(v)) => bind = v.clone(),
            ("--port", Some(v)) => match v.parse() {
                Ok(p) => port = p,
                Err(_) => return usage(),
            },
            ("--dump-frames", Some(v)) => dump_frames = Some(v.clone()),
            _ => return usage(),
        }
    }
    let Some(identity) = identity else {
        eprintln!("rill-server: --identity <dir> is required (there is no plaintext mode)");
        return usage();
    };
    let mut cfg = ServerConfig::new(root, identity);
    cfg.dump_frames = dump_frames.map(Into::into);

    // See files-app for the measurement behind these two. glibc hands each
    // contending thread an arena of its own, up to 64 MiB, and never gives
    // one back; one worker per core therefore turns a burst of concurrent
    // requests into a permanently expensive process. Serving a document is
    // not compute-bound, so the parallelism bought nothing.
    //
    // SAFETY: before any worker thread exists, which is the only moment the
    // arena limit can still be set.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, WORKERS as libc::c_int);
    }
    std::thread::Builder::new()
        .name("trim".into())
        .spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3));
                // SAFETY: no arguments; returns freed pages at the top of
                // each arena to the kernel.
                unsafe {
                    libc::malloc_trim(0);
                }
            }
        })
        .expect("trim thread");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        match Server::bind(&bind, port, cfg).await {
            Ok(server) => {
                if let Err(e) = server.run().await {
                    eprintln!("rill-server: {e}");
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(e) => {
                eprintln!("rill-server: {e}");
                ExitCode::FAILURE
            }
        }
    })
}

fn usage() -> ExitCode {
    eprintln!("usage: rill-server serve <root> --identity <dir> [--bind ADDR] [--port N] [--dump-frames DIR]");
    ExitCode::FAILURE
}
