//! `rill pack` subcommands (plan § Resource Phase 3):
//! build, inspect, extract, verify.

use std::path::Path;
use std::process::ExitCode;

use rill_pack::{ENCODING_ZSTD, Pack, PackBuilder};

pub fn run(args: &[String]) -> ExitCode {
    let Some((sub, rest)) = args.split_first() else {
        return usage();
    };
    let result = match sub.as_str() {
        "build" => build(rest),
        "inspect" => inspect(rest),
        "extract" => extract(rest),
        "verify" => verify(rest),
        "hash" => hash(rest),
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("rill pack: {message}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: rill pack build <dir> --output <file.rillpack>");
    eprintln!("       rill pack inspect <file.rillpack>");
    eprintln!("       rill pack extract <file.rillpack> <path> [-o FILE|-]");
    eprintln!("       rill pack verify <file.rillpack>");
    eprintln!("       rill pack hash <file>");
    ExitCode::FAILURE
}

fn build(args: &[String]) -> Result<(), String> {
    let (mut dir, mut output) = (None, None);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--output" => {
                output = Some(args.get(i + 1).ok_or("--output needs a value")?.clone());
                i += 2;
            }
            other if dir.is_none() => {
                dir = Some(other.to_string());
                i += 1;
            }
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    let dir = dir.ok_or("build needs a directory")?;
    let output = output.ok_or("build needs --output <file>")?;

    let mut builder = PackBuilder::new();
    builder.add_dir(Path::new(&dir)).map_err(|e| e.to_string())?;
    builder.write_to(Path::new(&output)).map_err(|e| e.to_string())?;

    let mut pack = Pack::open(Path::new(&output)).map_err(|e| e.to_string())?;
    let (mut raw, mut enc) = (0u64, 0u64);
    for e in pack.entries() {
        raw += e.decoded_size;
        enc += e.encoded_size;
    }
    pack.verify().map_err(|e| e.to_string())?;
    let packed_len = std::fs::metadata(&output).map_err(|e| e.to_string())?.len();
    println!(
        "{output}: {} resources, {raw} bytes → {enc} encoded ({packed_len} total), verified",
        pack.entries().len()
    );
    Ok(())
}

fn inspect(args: &[String]) -> Result<(), String> {
    let [file] = args else { return Err("inspect needs a pack file".into()) };
    let pack = Pack::open(Path::new(file)).map_err(|e| e.to_string())?;
    println!("Pack: {file}");
    println!("Resources: {}", pack.entries().len());
    for e in pack.entries() {
        println!(
            "  {}  {}  {} → {} bytes  {}",
            e.path,
            if e.encoding == ENCODING_ZSTD { "zstd" } else { "raw " },
            e.decoded_size,
            e.encoded_size,
            e.hash,
        );
    }
    Ok(())
}

fn extract(args: &[String]) -> Result<(), String> {
    let (file, path) = match args {
        [file, path, rest @ ..] => {
            if !rest.is_empty() && (rest.len() != 2 || rest[0] != "-o") {
                return Err("usage: extract <pack> <path> [-o FILE|-]".into());
            }
            (file, path)
        }
        _ => return Err("extract needs <pack> <path>".into()),
    };
    let output = match args.get(2).map(String::as_str) {
        Some("-o") => args[3].clone(),
        _ => path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("resource").to_string(),
    };
    let mut pack = Pack::open(Path::new(file)).map_err(|e| e.to_string())?;
    let data = pack
        .get(path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{path}: not in pack"))?;
    if output == "-" {
        use std::io::Write;
        std::io::stdout().write_all(&data).map_err(|e| e.to_string())?;
    } else {
        std::fs::write(&output, &data).map_err(|e| e.to_string())?;
        eprintln!("{path} → {output} ({} bytes, hash verified)", data.len());
    }
    Ok(())
}

fn hash(args: &[String]) -> Result<(), String> {
    let [file] = args else { return Err("hash needs a file".into()) };
    let bytes = std::fs::read(file).map_err(|e| format!("{file}: {e}"))?;
    println!("{}", rill_store::Hash::of(&bytes));
    Ok(())
}

fn verify(args: &[String]) -> Result<(), String> {
    let [file] = args else { return Err("verify needs a pack file".into()) };
    let mut pack = Pack::open(Path::new(file)).map_err(|e| e.to_string())?;
    pack.verify().map_err(|e| e.to_string())?;
    println!("{file}: footer hash and all {} resources verified", pack.entries().len());
    Ok(())
}
