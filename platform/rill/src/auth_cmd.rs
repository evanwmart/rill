//! `rill auth` subcommands (security.md §5): init, init-server, fingerprint,
//! enroll, pending, trust.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rill_auth::{
    DeviceRegistry, PendingDevices, Pins, TlsConnector, fingerprint_hex, generate_identity,
    load_pem_identity, parse_cert_pem, probe_tls_config, server_name,
};

pub use rill_client::util::default_identity_dir;

pub fn run(args: &[String]) -> ExitCode {
    let Some((sub, rest)) = args.split_first() else {
        return usage();
    };
    let result = match sub.as_str() {
        "init" => init(rest),
        "init-server" => init_server(rest),
        "fingerprint" => fingerprint(rest),
        "enroll" => enroll(rest),
        "pending" => pending(rest),
        "trust" => trust(rest),
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("rill auth: {message}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: rill auth init [--identity DIR] [--name NAME]");
    eprintln!("       rill auth init-server <DIR> [--name NAME]");
    eprintln!("       rill auth fingerprint [--identity DIR]");
    eprintln!("       rill auth enroll <SERVER-DIR> <DEVICE-NAME> <FINGERPRINT>");
    eprintln!("       rill auth pending <SERVER-DIR>");
    eprintln!("       rill auth trust rill://host[:port] [--identity DIR] [--yes]");
    ExitCode::FAILURE
}

/// Arguments that aren't flags or flag values (all our flags take a value
/// except --yes).
fn positionals(args: &[String]) -> Vec<&String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--yes" {
            continue;
        }
        if arg.starts_with("--") {
            it.next(); // skip the flag's value
            continue;
        }
        out.push(arg);
    }
    out
}

fn flag_value(args: &[String], flag: &str) -> Result<Option<String>, String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return match it.next() {
                Some(v) => Ok(Some(v.clone())),
                None => Err(format!("{flag} needs a value")),
            };
        }
    }
    Ok(None)
}

fn write_identity(dir: &Path, prefix: &str, name: &str) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let key_path = dir.join(format!("{prefix}-key.pem"));
    let cert_path = dir.join(format!("{prefix}-cert.pem"));
    if key_path.exists() || cert_path.exists() {
        return Err(format!("identity already exists in {} — refusing to overwrite", dir.display()));
    }
    let id = generate_identity(name).map_err(|e| e.to_string())?;
    // Created 0600, not created-then-chmodded. Writing under the process
    // umask makes the key world-readable for the moment between the two
    // calls, and if the chmod fails it stays that way — which the discarded
    // result meant nobody would ever hear about. A private key that was
    // briefly readable was readable.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)
            .map_err(|e| format!("{}: {e}", key_path.display()))?;
        f.write_all(id.key_pem.as_bytes())
            .map_err(|e| format!("{}: {e}", key_path.display()))?;
    }
    #[cfg(not(unix))]
    std::fs::write(&key_path, &id.key_pem).map_err(|e| format!("{}: {e}", key_path.display()))?;
    std::fs::write(&cert_path, &id.cert_pem)
        .map_err(|e| format!("{}: {e}", cert_path.display()))?;
    let cert = parse_cert_pem(&id.cert_pem).map_err(|e| e.to_string())?;
    Ok(fingerprint_hex(&cert))
}

fn init(args: &[String]) -> Result<(), String> {
    let dir = flag_value(args, "--identity")?.map(PathBuf::from).unwrap_or_else(default_identity_dir);
    let name = flag_value(args, "--name")?.unwrap_or_else(|| {
        std::env::var("HOSTNAME").unwrap_or_else(|_| "rill-device".into())
    });
    let fp = write_identity(&dir, "device", &name)?;
    println!("device identity created in {}", dir.display());
    println!("fingerprint: {fp}");
    println!("enroll this device on a server with:");
    println!("  rill auth enroll <server-dir> <device-name> {fp}");
    Ok(())
}

fn init_server(args: &[String]) -> Result<(), String> {
    let positional = positionals(args);
    let [dir] = positional.as_slice() else {
        return Err("init-server needs exactly one directory argument".into());
    };
    let dir = PathBuf::from(dir);
    let name = flag_value(args, "--name")?.unwrap_or_else(|| "rill-server".into());
    let fp = write_identity(&dir, "server", &name)?;

    let devices = dir.join("devices.toml");
    if !devices.exists() {
        std::fs::write(&devices, "# device-name = \"sha256 certificate fingerprint (hex)\"\n")
            .map_err(|e| format!("{}: {e}", devices.display()))?;
    }
    let policy = dir.join("policy.toml");
    if !policy.exists() {
        std::fs::write(
            &policy,
            "default_access = \"deny\"\n\n[[rule]]\npath = \"/public/**\"\nallow = [\"anonymous\"]\n",
        )
        .map_err(|e| format!("{}: {e}", policy.display()))?;
    }
    println!("server identity created in {}", dir.display());
    println!("fingerprint: {fp}");
    println!("clients pin it with: rill auth trust rill://<host>:<port>");
    Ok(())
}

fn fingerprint(args: &[String]) -> Result<(), String> {
    let dir = flag_value(args, "--identity")?.map(PathBuf::from).unwrap_or_else(default_identity_dir);
    let id = load_pem_identity(&dir, "device")
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no device identity in {} — run: rill auth init", dir.display()))?;
    let cert = parse_cert_pem(&id.cert_pem).map_err(|e| e.to_string())?;
    println!("{}", fingerprint_hex(&cert));
    Ok(())
}

fn enroll(args: &[String]) -> Result<(), String> {
    let positional = positionals(args);
    let [server_dir, name, fp] = positional.as_slice() else {
        return Err("enroll needs: <SERVER-DIR> <DEVICE-NAME> <FINGERPRINT>".into());
    };
    // The name lands as a bare TOML key and the fingerprint inside quotes.
    // Validating the *file* afterwards is not enough: a name like
    // `a = "<fp>"\nb` splices a second, well-formed enrollment right past
    // that check. Refuse anything that is not a plain bare key up front.
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "device name {name:?} — letters, digits, '-' and '_' only (it becomes a TOML key)"
        ));
    }
    if fp.is_empty() || !fp.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("fingerprint {fp:?} is not hex"));
    }
    let path = Path::new(server_dir).join("devices.toml");
    let mut text = std::fs::read_to_string(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    text.push_str(&format!("{name} = \"{}\"\n", fp.to_ascii_lowercase()));
    // Validate the result before writing — a bad enroll must not corrupt the
    // registry the server refuses to start on.
    DeviceRegistry::parse(&text).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    // Approving a device answers the question its pending entry asked, so
    // drop it — the list should only ever show what still needs a decision.
    // Best-effort: enrollment succeeded either way, and the server rewrites
    // this file on its own.
    let dir = Path::new(server_dir);
    if let Ok(mut waiting) = PendingDevices::load(dir)
        && waiting.remove(fp)
    {
        let _ = waiting.save(dir);
    }
    println!("enrolled {name}; restart rill-server to pick it up");
    Ok(())
}

/// Devices that offered a certificate the server does not know.
///
/// This exists so enrollment is not "read the server's stderr": the server
/// records each unknown fingerprint, and this reads the file back. It works
/// with the log off, with stderr going nowhere, and days after the fact.
fn pending(args: &[String]) -> Result<(), String> {
    let positional = positionals(args);
    let [server_dir] = positional.as_slice() else {
        return Err("pending needs: <SERVER-DIR>".into());
    };
    let dir = Path::new(server_dir);
    let waiting = PendingDevices::load(dir).map_err(|e| e.to_string())?;
    if waiting.is_empty() {
        println!("no unknown devices have connected.");
        println!("(a device gets here by trying to connect: rill auth trust rill://<host>:<port>)");
        return Ok(());
    }
    println!("unknown devices, most recent first:\n");
    for device in waiting.list() {
        let times = match device.count {
            1 => "once".to_string(),
            n => format!("{n} times"),
        };
        println!("  {}", device.fingerprint);
        println!("    seen {times}, last {}", ago(device.last_seen));
    }
    println!("\nenroll one with:");
    println!("  rill auth enroll {server_dir} <device-name> <fingerprint>");
    Ok(())
}

/// "3 minutes ago", coarsely. A fingerprint is identified by *when it
/// knocked*, so the useful precision is "just now" versus "last week".
fn ago(unix_seconds: u64) -> String {
    let now = rill_auth::unix_now();
    let secs = now.saturating_sub(unix_seconds);
    let (n, unit) = match secs {
        0..=44 => return "just now".into(),
        45..=5399 => ((secs + 30) / 60, "minute"),
        5400..=129_599 => ((secs + 1800) / 3600, "hour"),
        _ => ((secs + 43_200) / 86_400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

fn trust(args: &[String]) -> Result<(), String> {
    let Some(url) = args.first().filter(|a| !a.starts_with("--")) else {
        return Err("trust needs a rill:// address".into());
    };
    let dir = flag_value(args, "--identity")?.map(PathBuf::from).unwrap_or_else(default_identity_dir);
    let assume_yes = args.iter().any(|a| a == "--yes");

    // Accept rill://host[:port] with or without a path.
    let bare = url.strip_prefix("rill://").ok_or("address must start with rill://")?;
    let authority = bare.split('/').next().unwrap_or("");
    let probe_url = format!("rill://{authority}/");
    let parsed = rill_client::RillUrl::parse(&probe_url).map_err(|e| e.to_string())?;

    let fp = probe_fingerprint(&parsed.host, parsed.port)?;
    println!("server {}:{}", parsed.host, parsed.port);
    println!("fingerprint: {fp}");

    let mut pins = Pins::load(&dir).map_err(|e| e.to_string())?;
    if pins.get(&parsed.host, parsed.port) == Some(fp.as_str()) {
        println!("already pinned.");
        return Ok(());
    }
    if let Some(old) = pins.get(&parsed.host, parsed.port) {
        println!("WARNING: replaces existing pin {old}");
    }
    if !assume_yes {
        print!("pin this fingerprint? [y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).map_err(|e| e.to_string())?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Err("not pinned".into());
        }
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    pins.set(&parsed.host, parsed.port, &fp).map_err(|e| e.to_string())?;
    pins.save(&dir).map_err(|e| e.to_string())?;
    println!("pinned. rill get rill://{authority}/... will now verify this server.");
    Ok(())
}

/// Connect with the accept-any probe config purely to read the certificate.
fn probe_fingerprint(host: &str, port: u16) -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    // The same bound `Client::connect` puts on both steps. Without it a
    // black-holed address hangs the command until the OS gives up, which for
    // a TCP connect can be minutes with nothing on screen.
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    runtime.block_on(async {
        let config = probe_tls_config().map_err(|e| e.to_string())?;
        let name = server_name(host).map_err(|e| e.to_string())?;
        let tcp = tokio::time::timeout(
            PROBE_TIMEOUT,
            tokio::net::TcpStream::connect((host, port)),
        )
        .await
        .map_err(|_| format!("connect {host}:{port}: timed out"))?
        .map_err(|e| format!("connect {host}:{port}: {e}"))?;
        let tls = tokio::time::timeout(
            PROBE_TIMEOUT,
            TlsConnector::from(config).connect(name, tcp),
        )
        .await
        .map_err(|_| "TLS handshake: timed out".to_string())?
        .map_err(|e| format!("TLS handshake: {e}"))?;
        let (_, session) = tls.get_ref();
        let cert = session
            .peer_certificates()
            .and_then(|c| c.first())
            .ok_or("server presented no certificate")?;
        Ok(fingerprint_hex(cert))
    })
}

pub use rill_client::util::client_identity_for;
