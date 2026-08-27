//! `rill app` subcommands (application-model.md §5): install, list, update,
//! remove.

use std::path::PathBuf;
use std::process::ExitCode;

use rill_app::{InstallStore, Manifest};
use rill_client::{Client, ClientConfig, RillUrl, util};

pub fn run(args: &[String]) -> ExitCode {
    let result = match args.split_first().map(|(c, r)| (c.as_str(), r)) {
        Some(("install", rest)) => install(rest),
        Some(("list", rest)) => list(rest),
        Some(("update", rest)) => update(rest),
        Some(("remove", rest)) => remove(rest),
        _ => {
            eprintln!("usage: rill app install <rill://host[:port]/path-to-manifest>");
            eprintln!("       rill app list");
            eprintln!("       rill app update [KEY]");
            eprintln!("       rill app remove <KEY>");
            eprintln!("flags: [--identity DIR] [--data DIR]");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("rill app: {message}");
            ExitCode::FAILURE
        }
    }
}

struct Dirs {
    identity: PathBuf,
    data: PathBuf,
}

fn parse_flags(args: &[String]) -> Result<(Vec<String>, Dirs), String> {
    let mut positional = Vec::new();
    let mut dirs = Dirs {
        identity: util::default_identity_dir(),
        data: rill_app::default_data_dir(),
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--identity" => {
                dirs.identity = args.get(i + 1).ok_or("--identity needs a value")?.into();
                i += 2;
            }
            "--data" => {
                dirs.data = args.get(i + 1).ok_or("--data needs a value")?.into();
                i += 2;
            }
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }
    Ok((positional, dirs))
}

/// Fetch `paths` from one server over a single connection.
fn fetch_all(
    dirs: &Dirs,
    host: &str,
    port: u16,
    paths: &[&str],
) -> Result<Vec<Vec<u8>>, String> {
    let (fingerprint, device) = util::client_identity_for(&dirs.identity, host, port)?;
    let mut cfg = ClientConfig::new(fingerprint);
    cfg.device = device;
    cfg.cache_dir = Some(util::default_cache_dir());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let mut client =
            Client::connect(host, port, cfg).await.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for path in paths {
            out.push(client.get(path).await.map_err(|e| e.to_string())?.data);
        }
        client.close().await;
        Ok(out)
    })
}

fn install(args: &[String]) -> Result<(), String> {
    let (positional, dirs) = parse_flags(args)?;
    let [url] = positional.as_slice() else {
        return Err("install needs a manifest URL".into());
    };
    let url = RillUrl::parse(url).map_err(|e| e.to_string())?;
    let (fingerprint, _) = util::client_identity_for(&dirs.identity, &url.host, url.port)?;

    let manifest_bytes = fetch_all(&dirs, &url.host, url.port, &[&url.path])?.remove(0);
    let manifest_text =
        String::from_utf8(manifest_bytes).map_err(|_| "manifest is not UTF-8".to_string())?;
    let manifest = Manifest::parse(&manifest_text).map_err(|e| e.to_string())?;
    println!(
        "{} ({}) from rill://{}:{} — fetching pack {}",
        manifest.name, manifest.app_id, url.host, url.port, manifest.pack
    );
    if !manifest.permissions.is_empty() {
        println!("requests permissions (not yet enforced):");
        for (perm, wanted) in &manifest.permissions {
            println!("  {perm} = {wanted}");
        }
    }
    let pack_bytes = fetch_all(&dirs, &url.host, url.port, &[&manifest.pack])?.remove(0);

    let store = InstallStore::open(&dirs.data).map_err(|e| e.to_string())?;
    let installed = store
        .install(&url.host, url.port, &fingerprint, &url.path, &manifest_text, &pack_bytes)
        .map_err(|e| e.to_string())?;
    println!(
        "installed {} as {} (version {})",
        installed.name,
        installed.key,
        &installed.current.to_hex()[..12]
    );
    println!("launch it: rill-view   (launcher)  — or rill-view --app {}", installed.key);
    Ok(())
}

fn list(args: &[String]) -> Result<(), String> {
    let (_, dirs) = parse_flags(args)?;
    let store = InstallStore::open(&dirs.data).map_err(|e| e.to_string())?;
    let apps = store.list().map_err(|e| e.to_string())?;
    if apps.is_empty() {
        println!("no applications installed (rill app install <manifest-url>)");
        return Ok(());
    }
    for app in apps {
        println!(
            "{:24}  {:16}  rill://{}:{}  v {}",
            app.key,
            app.name,
            app.host,
            app.port,
            &app.current.to_hex()[..12]
        );
    }
    Ok(())
}

fn update(args: &[String]) -> Result<(), String> {
    let (positional, dirs) = parse_flags(args)?;
    let store = InstallStore::open(&dirs.data).map_err(|e| e.to_string())?;
    let apps = store.list().map_err(|e| e.to_string())?;
    let targets: Vec<_> = match positional.as_slice() {
        [] => apps,
        [key] => apps.into_iter().filter(|a| &a.key == key).collect(),
        _ => return Err("update takes at most one key".into()),
    };
    if targets.is_empty() {
        return Err("no matching installed apps".into());
    }
    for app in targets {
        let manifest_bytes =
            match fetch_all(&dirs, &app.host, app.port, &[&app.manifest_path]) {
                Ok(mut v) => v.remove(0),
                Err(e) => {
                    println!("{}: check failed: {e}", app.key);
                    continue;
                }
            };
        let manifest_text = match String::from_utf8(manifest_bytes) {
            Ok(t) => t,
            Err(_) => {
                println!("{}: manifest not UTF-8", app.key);
                continue;
            }
        };
        let manifest = match Manifest::parse(&manifest_text) {
            Ok(m) => m,
            Err(e) => {
                println!("{}: {e}", app.key);
                continue;
            }
        };
        if manifest.pack_hash == app.current {
            println!("{}: up to date (v {})", app.key, &app.current.to_hex()[..12]);
            continue;
        }
        let pack_bytes = fetch_all(&dirs, &app.host, app.port, &[&manifest.pack])?.remove(0);
        store
            .stage_update(&app.key, &manifest_text, &pack_bytes)
            .map_err(|e| e.to_string())?;
        println!(
            "{}: staged v {} (applies on next launch)",
            app.key,
            &manifest.pack_hash.to_hex()[..12]
        );
    }
    Ok(())
}

fn remove(args: &[String]) -> Result<(), String> {
    let (positional, dirs) = parse_flags(args)?;
    let [key] = positional.as_slice() else {
        return Err("remove needs an app key (see rill app list)".into());
    };
    let store = InstallStore::open(&dirs.data).map_err(|e| e.to_string())?;
    if store.remove(key).map_err(|e| e.to_string())? {
        println!("removed {key}");
        Ok(())
    } else {
        Err(format!("{key}: not installed"))
    }
}
