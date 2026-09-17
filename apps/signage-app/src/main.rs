//! Signage: the display profile, served. One server, three documents,
//! each proving a different axis of showing a document on a piece of
//! glass that nobody is sitting at:
//!
//! * `/gate` — one gate's screen: the flight boarding there, its groups
//!   and standby list, the next flight, a gate-change strip. The demo
//!   target. `/airport` is the same schedule as the terminal-wide
//!   departures board, fourteen rows that churn on a clock. Both re-read
//!   every second and are honest about a dropped feed (see `airport.rs`).
//! * `/ad` — a store-window rotation. A playlist on the server's clock,
//!   so every window shows the same slide; polls cost nothing between
//!   slides (see `ad.rs`).
//! * `/museum` — an exhibit label. Words with structure, three languages,
//!   two sizes, no motion; the document a screen reader or a cache can
//!   hold (see `museum.rs`).
//!
//! ```bash
//! signage-app --identity <server-dir> [--data DIR] [--content DIR] [--port 7440] [--bind 127.0.0.1] [--demo]
//! ```
//!
//! On the glass, `~/kiosk.url` names one of the three (deploy/pi/
//! rill-session.sh) and the compositor gives that document the whole
//! output. Nothing here knows it is on a kiosk: these are ordinary pages
//! any Rill client can open in a window.

mod ad;
mod airport;
mod museum;

use std::path::PathBuf;
use std::sync::Arc;

use rill_server::{Server, ServerConfig};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut identity: Option<PathBuf> = None;
    let mut data: Option<PathBuf> = None;
    let mut content: Option<PathBuf> = None;
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 7440;
    let mut seed: u64 = 20_260_914;
    let mut timeline = airport::Timeline::REAL;
    let mut i = 0;
    while i < args.len() {
        let value = |i: usize| -> String {
            args.get(i + 1).cloned().unwrap_or_else(|| {
                eprintln!("signage-app: {} needs a value", args[i]);
                std::process::exit(2);
            })
        };
        match args[i].as_str() {
            "--identity" => identity = Some(value(i).into()),
            "--data" => data = Some(value(i).into()),
            "--content" => content = Some(value(i).into()),
            "--bind" => bind = value(i),
            "--port" => port = value(i).parse().expect("--port N"),
            "--seed" => seed = value(i).parse().expect("--seed N"),
            // The scripted timeline: one gate state a minute, looping.
            "--demo" => {
                timeline = airport::Timeline::DEMO;
                i -= 1;
            }
            other => {
                eprintln!("signage-app: unexpected argument {other}");
                std::process::exit(2);
            }
        }
        i += 2;
    }
    let Some(identity) = identity else {
        eprintln!(
            "usage: signage-app --identity <server-dir> [--data DIR] [--content DIR] [--port N] [--bind ADDR] [--seed N] [--demo]"
        );
        std::process::exit(2);
    };
    let data = data.unwrap_or_else(|| PathBuf::from("signage-data"));
    let content = content.unwrap_or_else(|| data.join("content"));
    for dir in [&data, &content] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("signage-app: {}: {e}", dir.display());
            std::process::exit(1);
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    runtime.block_on(async move {
        let cfg = ServerConfig::new(content, identity);
        let mut server = Server::bind(&bind, port, cfg).await.expect("bind");
        let board = Arc::new(airport::Board::new(data.clone(), seed, timeline));
        server.dynamic("/airport", board.clone());
        server.dynamic("/gate", board);
        server.dynamic("/ad", Arc::new(ad::Ad));
        server.dynamic("/museum", Arc::new(museum::Label));
        eprintln!(
            "signage-app: serving /gate /airport /ad /museum on {bind}:{port} (feed switch: {})",
            data.join("feed-down").display()
        );
        server.run().await.expect("run");
    });
}
