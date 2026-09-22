//! `@nubjs/microbe`: the crate's `install_all`, exposed to Node. One function, promise and
//! sync forms, taking either a list of specs or a `dependencies`-shaped object. Everything
//! else is the crate's own behaviour, so the README of the crate is the reference.

use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

#[napi(object)]
#[derive(Default)]
pub struct Options {
    /// Registry URL; default `https://registry.npmjs.org`.
    pub registry: Option<String>,
    /// Path of an `.npmrc` to apply. Nothing is discovered; `${VAR}` is an error.
    pub npmrc: Option<String>,
    /// Contents of an `.npmrc` the host already holds, applied after `npmrc`.
    pub npmrc_contents: Option<String>,
    /// Parallel fetches; default 16.
    pub concurrency: Option<u32>,
}

#[napi(object)]
pub struct Root {
    pub name: String,
    pub version: String,
    /// `<dir>/node_modules/<name>`.
    pub dir: String,
}

#[napi(object)]
pub struct Installation {
    /// The requested packages, in request order for a list and in name order for an object.
    pub roots: Vec<Root>,
    /// Command → absolute script path; the same commands are linked in `node_modules/.bin`.
    pub bins: HashMap<String, String>,
    /// Tarballs extracted by this call.
    pub packages: u32,
    /// `name@version` of every package whose install script was not run.
    pub skipped_install_scripts: Vec<String>,
}

/// `["eslint@^9", "prettier"]` or `{ eslint: "^9", prettier: "*" }`.
type Deps = Either<Vec<String>, HashMap<String, String>>;

fn pairs(deps: Deps) -> Vec<(String, String)> {
    match deps {
        Either::A(specs) => specs
            .into_iter()
            .map(|s| match s.rfind('@') {
                Some(i) if i > 0 => (s[..i].to_string(), s[i + 1..].to_string()),
                _ => (s, String::new()),
            })
            .collect(),
        Either::B(map) => map.into_iter().collect::<BTreeMap<_, _>>().into_iter().collect(),
    }
}

fn js(e: microbe::Error) -> Error {
    Error::new(Status::GenericFailure, e.to_string())
}

fn run(deps: &[(String, String)], dir: &str, opts: &Options) -> Result<Installation> {
    let mut m = microbe::Microbe::new().map_err(js)?;
    if let Some(r) = &opts.registry {
        m = m.registry(r);
    }
    if let Some(p) = &opts.npmrc {
        m = m.npmrc(Path::new(p)).map_err(js)?;
    }
    if let Some(c) = &opts.npmrc_contents {
        m = m.npmrc_contents(c).map_err(js)?;
    }
    if let Some(n) = opts.concurrency {
        m = m.concurrency(n as usize);
    }
    let done = m
        .install_all(deps.iter().map(|(n, r)| (n.as_str(), r.as_str())), Path::new(dir))
        .map_err(js)?;
    Ok(Installation {
        roots: done
            .roots
            .into_iter()
            .map(|r| Root {
                name: r.name,
                version: r.version,
                dir: r.dir.to_string_lossy().into_owned(),
            })
            .collect(),
        bins: done
            .bins
            .into_iter()
            .map(|(k, v)| (k, v.to_string_lossy().into_owned()))
            .collect(),
        packages: done.packages as u32,
        skipped_install_scripts: done.skipped_install_scripts,
    })
}

pub struct InstallTask {
    deps: Vec<(String, String)>,
    dir: String,
    opts: Options,
}

impl Task for InstallTask {
    type Output = Installation;
    type JsValue = Installation;

    fn compute(&mut self) -> Result<Installation> {
        run(&self.deps, &self.dir, &self.opts)
    }

    fn resolve(&mut self, _env: Env, output: Installation) -> Result<Installation> {
        Ok(output)
    }
}

/// Install into `<dir>/node_modules`, off the main thread; resolves to the installation.
#[napi(ts_args_type = "deps: string[] | Record<string, string>, dir: string, options?: Options")]
pub fn install(deps: Deps, dir: String, options: Option<Options>) -> AsyncTask<InstallTask> {
    AsyncTask::new(InstallTask {
        deps: pairs(deps),
        dir,
        opts: options.unwrap_or_default(),
    })
}

/// [`install`], blocking the calling thread.
#[napi(ts_args_type = "deps: string[] | Record<string, string>, dir: string, options?: Options")]
pub fn install_sync(deps: Deps, dir: String, options: Option<Options>) -> Result<Installation> {
    run(&pairs(deps), &dir, &options.unwrap_or_default())
}
