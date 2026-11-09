//! Two verbs, both installing into `<dir>/node_modules` and printing what landed:
//!
//! - `microbe install <name[@spec]>... --dir <path>` names packages directly.
//! - `microbe install-manifest <file|-> --dir <path>` takes the `dependencies` map of a JSON
//!   file, or of stdin for `-`: the `package.json#/dependencies` shape, so a whole
//!   `package.json` is valid input and every other key is ignored.
//!
//! Both print one line per requested package, a package count and the bins, or the whole
//! [`microbe::Installation`] as JSON with `--json`.
//!
//! Everything is explicit: the target directory is required, nothing is read from the
//! environment, and no file is discovered by walking the filesystem — the embedder decides
//! where configuration comes from. This is an embedder-facing tool, not a human CLI. The
//! library is the product; this binary exists to measure it and to try it from a shell.

use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: microbe install <name[@spec]>... --dir <path> [options]
       microbe install-manifest <file|-> --dir <path> [options]

options: --registry <url>   registry for unscoped packages; default https://registry.npmjs.org
         --npmrc <file>     apply this .npmrc; nothing is discovered
         --json             print the installation as JSON
         --help, --version";

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut positionals = Vec::new();
    let mut dir = None;
    let mut registry = None;
    let mut npmrc = None;
    let mut verb = None;
    let mut json = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--version" => {
                println!("microbe {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--json" => json = true,
            "--registry" | "--dir" | "--npmrc" => {
                // A flag with its value missing must not fall back to a default silently.
                let Some(value) = args.next().filter(|v| !v.starts_with("--")) else {
                    return usage();
                };
                match a.as_str() {
                    "--registry" => registry = Some(value),
                    "--dir" => dir = Some(value),
                    _ => npmrc = Some(value),
                }
            }
            // `-` alone is stdin; anything else dashed is a flag this binary does not have,
            // never a package name to look up.
            _ if a.starts_with('-') && a != "-" => return usage(),
            _ if verb.is_none() => verb = Some(a),
            _ => positionals.push(a),
        }
    }
    let Some(dir) = dir else {
        return usage();
    };
    let deps = match (verb.as_deref(), positionals.as_slice()) {
        (Some("install"), specs) if !specs.is_empty() => Ok(specs
            .iter()
            .map(|s| {
                let (name, range) = microbe::split_spec(s);
                (name.to_string(), range.to_string())
            })
            .collect::<Vec<_>>()),
        (Some("install-manifest"), [source]) => read_manifest(source),
        _ => return usage(),
    };
    let run = || -> Result<microbe::Installation, microbe::Error> {
        let deps = deps?;
        let mut m = microbe::Microbe::new()?;
        if let Some(path) = &npmrc {
            m = m.npmrc(Path::new(path))?;
        }
        if let Some(r) = &registry {
            m = m.registry(r);
        }
        m.install_all(
            deps.iter().map(|(n, r)| (n.as_str(), r.as_str())),
            Path::new(&dir),
        )
    };
    match run() {
        Ok(all) if json => match serde_json::to_string_pretty(&all) {
            Ok(s) => {
                println!("{s}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("microbe: {e}");
                ExitCode::FAILURE
            }
        },
        Ok(all) => {
            for r in &all.roots {
                println!("{}@{} -> {}", r.name, r.version, r.dir.display());
            }
            let noun = if all.packages == 1 {
                "package"
            } else {
                "packages"
            };
            println!("{} {noun}", all.packages);
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

/// The `dependencies` map of a JSON file, or of stdin for `-`. Nothing else in the file is
/// read: no `devDependencies`, overrides, catalogs, workspaces or `peerDependencies`.
fn read_manifest(source: &str) -> Result<Vec<(String, String)>, microbe::Error> {
    let bad =
        |detail: String| microbe::Error::Io(std::io::Error::other(format!("{source}: {detail}")));
    let json = if source == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
            .map_err(|e| bad(e.to_string()))?;
        s
    } else {
        std::fs::read_to_string(source).map_err(|e| bad(e.to_string()))?
    };
    #[derive(serde::Deserialize)]
    struct Manifest {
        dependencies: Option<std::collections::BTreeMap<String, String>>,
    }
    let manifest: Manifest = serde_json::from_str(&json).map_err(|e| bad(e.to_string()))?;
    let map = manifest
        .dependencies
        .ok_or_else(|| bad("no `dependencies` key".to_string()))?;
    Ok(map.into_iter().collect())
}
