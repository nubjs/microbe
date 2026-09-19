//! `microbe install [<name[@spec]>...] [--from <file|->] --dir <path> [--registry <url>]`
//! installs into `<dir>/node_modules` and prints what landed. Everything is explicit: the
//! target directory is required, nothing is read from the environment, and no file is
//! discovered by walking the filesystem — the embedder decides where configuration comes
//! from. This is an embedder-facing tool, not a human CLI. Specs name packages directly; `--from` reads a JSON file (or stdin with `-`) and
//! takes its `dependencies` map — the `package.json#/dependencies` shape — so a whole
//! `package.json` is a valid input and its other keys are ignored. The library is the
//! product; this binary exists to measure it and to try it from a shell.

use std::path::Path;
use std::process::ExitCode;

const USAGE: &str =
    "usage: microbe install [<name[@spec]>...] [--from <file|->] --dir <path> [--registry <url>]";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut specs = Vec::new();
    let mut from = None;
    let mut dir = None;
    let mut registry = None;
    let mut verb = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--registry" => registry = args.next(),
            "--dir" => dir = args.next(),
            "--from" => from = args.next(),
            _ if verb.is_none() => verb = Some(a),
            _ => specs.push(a),
        }
    }
    let (Some("install"), Some(dir)) = (verb.as_deref(), dir) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if specs.is_empty() && from.is_none() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let run = || -> Result<microbe::Installation, microbe::Error> {
        let mut m = microbe::Microbe::new()?;
        if let Some(r) = &registry {
            m = m.registry(r);
        }
        let mut deps: Vec<(String, String)> = specs.iter().map(|s| split(s)).collect();
        if let Some(source) = &from {
            deps.extend(read_manifest(source)?);
        }
        m.install_all(
            deps.iter().map(|(n, r)| (n.as_str(), r.as_str())),
            Path::new(&dir),
        )
    };
    match run() {
        Ok(all) => {
            for r in &all.roots {
                println!("{}@{} -> {}", r.name, r.version, r.dir.display());
            }
            println!("{} packages", all.packages);
            for (cmd, path) in &all.bins {
                println!("  bin {cmd} -> {}", path.display());
            }
            if !all.skipped_install_scripts.is_empty() {
                println!(
                    "  install scripts not run: {}",
                    all.skipped_install_scripts.join(", ")
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("microbe: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `name[@range]`, split on the last `@` that is not the scope's.
fn split(spec: &str) -> (String, String) {
    match spec.rfind('@') {
        Some(i) if i > 0 => (spec[..i].to_string(), spec[i + 1..].to_string()),
        _ => (spec.to_string(), String::new()),
    }
}

/// The `dependencies` map of a JSON file, or of stdin for `-`. Nothing else in the file is
/// read: no `devDependencies`, overrides, catalogs, workspaces or `peerDependencies`.
fn read_manifest(source: &str) -> Result<Vec<(String, String)>, microbe::Error> {
    let json = if source == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        s
    } else {
        std::fs::read_to_string(source)?
    };
    #[derive(serde::Deserialize)]
    struct Manifest {
        dependencies: Option<std::collections::BTreeMap<String, String>>,
    }
    let bad = |detail: String| microbe::Error::Registry {
        name: source.to_string(),
        detail,
    };
    let manifest: Manifest = serde_json::from_str(&json).map_err(|e| bad(e.to_string()))?;
    let map = manifest
        .dependencies
        .ok_or_else(|| bad("no `dependencies` key".to_string()))?;
    Ok(map.into_iter().collect())
}
