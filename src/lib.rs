//! microbe — install npm packages and their dependency trees into a directory: one package
//! by spec, or a `package.json`-shaped map of names to ranges.
//!
//! ```no_run
//! let installed = microbe::Microbe::new()?.install("esbuild@^0.25", std::path::Path::new("/tmp/x"))?;
//! println!("{} {} {:?}", installed.name, installed.version, installed.bins);
//! # Ok::<(), microbe::Error>(())
//! ```
//!
//! Two phases. **Plan**: a breadth-first walk over `dependencies` and platform-matching
//! `optionalDependencies`, fetching each level's packuments in parallel and deciding every
//! package's directory deterministically — flat under `<dir>/node_modules/`, with a version
//! conflict nested under its dependent, exactly as Node's resolver expects. **Materialize**:
//! every planned tarball downloaded, verified and extracted in parallel. The split is what
//! makes the install latency-bound on the slowest single fetch rather than on their sum.
//!
//! Optionality is a property of the GRAPH, not of a package: the plan records every edge, a
//! package is required when the root reaches it over non-optional edges alone, and a failure
//! climbs toward the root until an optional edge absorbs it (the branch is dropped, as npm
//! does) or it reaches something required (the install fails). See [`settle`].
//!
//! What it deliberately does not do: run lifecycle scripts (reported instead, see
//! [`Installation::skipped_install_scripts`]), honour `peerDependencies`, write a lockfile,
//! keep a cache or store, or reconcile with an existing `node_modules` beyond replacing a
//! package it is about to install at another version. Those are the parts of a
//! package manager that take up the space.

mod error;
mod extract;
mod npmrc;
pub mod registry;
pub mod transport;

pub use error::Error;
pub use transport::Transport;

use registry::{Manifest, Packument};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";
const ABBREVIATED: &str = "application/vnd.npm.install-v1+json";
/// Matches npm's and pnpm's default network concurrency.
const DEFAULT_CONCURRENCY: usize = 16;

pub struct Microbe {
    transport: Box<dyn Transport>,
    registry: String,
    /// `@scope` → registry URL.
    scoped: BTreeMap<String, String>,
    /// `(URL prefix, Authorization value)`; the longest matching prefix wins.
    auth: Vec<(String, String)>,
    concurrency: usize,
    packuments: Mutex<HashMap<String, Packument>>,
}

/// What [`Microbe::install`] produced. `bins` maps each command the package declares to the
/// absolute path of its script, with the executable bit set; the same commands are linked
/// under `<dir>/node_modules/.bin`.
#[derive(Debug)]
pub struct Installed {
    pub name: String,
    pub version: String,
    /// `<dir>/node_modules/<name>`.
    pub dir: PathBuf,
    pub bins: BTreeMap<String, PathBuf>,
    /// Packages extracted, the root included.
    pub packages: usize,
    /// `name@version` of every package whose install script was NOT run.
    pub skipped_install_scripts: Vec<String>,
}

/// What [`Microbe::install_all`] produced.
#[derive(Debug)]
pub struct Installation {
    /// The requested packages, in request order.
    pub roots: Vec<Root>,
    /// Every command linked under `<dir>/node_modules/.bin`, mapped to the absolute path of
    /// the script it runs. Requested packages win a name clash with a dependency.
    pub bins: BTreeMap<String, PathBuf>,
    /// Packages extracted.
    pub packages: usize,
    /// `name@version` of every package whose install script was NOT run.
    pub skipped_install_scripts: Vec<String>,
}

#[derive(Debug)]
pub struct Root {
    pub name: String,
    pub version: String,
    /// `<dir>/node_modules/<name>`.
    pub dir: PathBuf,
}

impl Microbe {
    /// Uses the first transport the host provides; see [`transport::detect`].
    pub fn new() -> Result<Self, Error> {
        Ok(Self::from_boxed(transport::detect()?))
    }

    pub fn with_transport(transport: impl Transport + 'static) -> Self {
        Self::from_boxed(Box::new(transport))
    }

    fn from_boxed(transport: Box<dyn Transport>) -> Self {
        Microbe {
            transport,
            registry: DEFAULT_REGISTRY.to_string(),
            scoped: BTreeMap::new(),
            auth: Vec::new(),
            concurrency: DEFAULT_CONCURRENCY,
            packuments: Mutex::new(HashMap::new()),
        }
    }

    pub fn registry(mut self, url: &str) -> Self {
        self.registry = url.trim_end_matches('/').to_string();
        self
    }

    /// Apply an `.npmrc` at an EXPLICIT path: `registry`, `@scope:registry`, and credentials
    /// (`_authToken`, `_auth`, `username` with `_password`) keyed by URL prefix, as npm keys
    /// them. Nothing is discovered, and `${VAR}` is not expanded — resolve it and use
    /// [`Microbe::npmrc_contents`].
    pub fn npmrc(self, path: &Path) -> Result<Self, Error> {
        let contents = std::fs::read_to_string(path)?;
        self.npmrc_contents(&contents)
    }

    /// [`Microbe::npmrc`] for contents the embedder already holds.
    pub fn npmrc_contents(mut self, contents: &str) -> Result<Self, Error> {
        let rc = npmrc::parse(contents)?;
        if let Some(registry) = rc.registry {
            self.registry = registry;
        }
        self.scoped.extend(rc.scoped);
        self.auth.extend(rc.auth);
        Ok(self)
    }

    /// Simultaneous registry requests, for both packuments and tarballs.
    pub fn concurrency(mut self, n: usize) -> Self {
        self.concurrency = n.max(1);
        self
    }

    /// `spec` is `name`, `name@tag`, `name@version` or `name@range` (`@scope/name@^1` works).
    /// `dir` is created if needed; packages go under `dir/node_modules/`.
    pub fn install(&self, spec: &str, dir: &Path) -> Result<Installed, Error> {
        let (name, range) = split_spec(spec);
        let mut all = self.install_all([(name, range)], dir)?;
        let root = all.roots.remove(0);
        Ok(Installed {
            bins: all
                .bins
                .into_iter()
                .filter(|(_, path)| path.starts_with(&root.dir))
                .collect(),
            name: root.name,
            version: root.version,
            dir: root.dir,
            packages: all.packages,
            skipped_install_scripts: all.skipped_install_scripts,
        })
    }

    /// Install every `(name, range)` pair — the shape of a `package.json` `dependencies`
    /// map — into one `dir/node_modules`, and link every command a top-level package
    /// declares under `dir/node_modules/.bin`. A name given twice is taken once, at its
    /// first range. `peerDependencies` are ignored throughout.
    pub fn install_all<'a>(
        &self,
        deps: impl IntoIterator<Item = (&'a str, &'a str)>,
        dir: &Path,
    ) -> Result<Installation, Error> {
        let mut seen = HashSet::new();
        let deps: Vec<(&str, &str)> = deps
            .into_iter()
            .filter(|(name, _)| seen.insert(*name))
            .collect();
        std::fs::create_dir_all(dir)?;
        let root = dir.canonicalize()?;
        let mut plan = self.plan(&root, &deps)?;
        let live = self.materialize(&mut plan)?;
        let bins = link_bins(&root, &plan, &live)?;
        let kept = || plan.packages.iter().zip(&live).filter(|(_, l)| **l);
        Ok(Installation {
            roots: plan
                .roots
                .iter()
                .map(|&i| {
                    let p = &plan.packages[i];
                    Root {
                        name: p.name.clone(),
                        version: p.version.clone(),
                        dir: p.dir.clone(),
                    }
                })
                .collect(),
            bins,
            packages: kept().filter(|(p, _)| p.fetch).count(),
            skipped_install_scripts: kept()
                .filter(|(p, _)| p.manifest.has_install_script)
                .map(|(p, _)| format!("{}@{}", p.name, p.version))
                .collect(),
        })
    }

    /// Phase one. Breadth-first so that placement is deterministic: whichever version of a
    /// name is reached first from the requested packages takes the flat slot, and later
    /// conflicting versions nest under their dependents. Each level's packuments are fetched
    /// together before any of that level is placed.
    fn plan(&self, root: &Path, deps: &[(&str, &str)]) -> Result<Plan, Error> {
        let mut plan = Plan::default();
        let mut level: VecDeque<Want> = deps
            .iter()
            .map(|(name, range)| Want {
                parent: None,
                parent_dir: root.to_path_buf(),
                name: name.to_string(),
                range: range.to_string(),
                optional_edge: false,
                soft: false,
            })
            .collect();
        while !level.is_empty() {
            self.prefetch(level.iter().map(|w| w.name.as_str()));
            let mut next = VecDeque::new();
            for want in level.drain(..) {
                let placed = match self.place(root, &mut plan, &want)? {
                    Some(i) => Some(i),
                    // A requested package already on disk at a satisfying version is planned
                    // unfetched, so its bins are still linked and its own tree still checked.
                    None if want.parent.is_none() => {
                        Some(self.plan_present(root, &mut plan, &want)?)
                    }
                    None => None,
                };
                if let Some(i) = placed {
                    if want.parent.is_none() {
                        plan.roots.push(i);
                    }
                    next.extend(plan.packages[i].wants(i, want.soft));
                }
            }
            level = next;
        }
        Ok(plan)
    }

    fn plan_present(&self, root: &Path, plan: &mut Plan, want: &Want) -> Result<usize, Error> {
        let (version, dir) = plan
            .satisfied(root, root, &want.name, &want.range)?
            .expect("a requested package is either placed or already present");
        let manifest = self.manifest_for(&want.name, &version)?;
        Ok(plan.push(Planned {
            name: want.name.clone(),
            version,
            dir,
            manifest,
            fetch: false,
            children: Vec::new(),
        }))
    }

    /// Decide where one wanted package goes. Returns the index of a newly planned package so
    /// the caller can enqueue its dependencies; `None` when the want was already satisfied,
    /// was skipped, or failed softly.
    fn place(&self, root: &Path, plan: &mut Plan, want: &Want) -> Result<Option<usize>, Error> {
        if let Some((_, dir)) = plan.satisfied(root, &want.parent_dir, &want.name, &want.range)? {
            // An edge to a package planned earlier. It is what lets a package first reached
            // through an optional branch turn out to be required after all.
            if let Some(&child) = plan.index.get(&dir) {
                plan.link(want, child);
            }
            return Ok(None);
        }
        let picked = self.with_packument(&want.name, |p| {
            registry::pick(p, &want.name, &want.range).map(|(v, m)| (v.to_string(), m.clone()))
        });
        let (version, manifest) = match picked {
            Ok(Ok(vm)) => vm,
            // An optional dependency that is unpublished or unresolvable is simply absent.
            Err(_) | Ok(Err(_)) if want.optional_edge => return Ok(None),
            // A required dependency of a package that is itself only optionally reachable
            // (so far): charge the failure to that package and let `settle` decide.
            Err(e) | Ok(Err(e)) if want.soft => {
                if let Some(parent) = want.parent {
                    plan.failures.push((parent, e));
                }
                return Ok(None);
            }
            Err(e) | Ok(Err(e)) => return Err(e),
        };
        if want.optional_edge && !registry::platform_allowed(&manifest) {
            return Ok(None);
        }
        let flat = root.join("node_modules").join(&want.name);
        let dir = if plan.index.contains_key(&flat) || installed_version(&flat)?.is_some() {
            want.parent_dir.join("node_modules").join(&want.name)
        } else {
            flat
        };
        let child = plan.push(Planned {
            name: want.name.clone(),
            version,
            dir,
            manifest,
            fetch: true,
            children: Vec::new(),
        });
        plan.link(want, child);
        Ok(Some(child))
    }

    /// Phase two: every live planned tarball, `concurrency` at a time, then one more
    /// [`settle`] with the fetch failures folded in. Returns which packages are live, and
    /// removes from disk whatever a dropped branch had already extracted.
    fn materialize(&self, plan: &mut Plan) -> Result<Vec<bool>, Error> {
        let failures = std::mem::take(&mut plan.failures);
        let before = settle(plan, failures)?;
        let todo: Vec<usize> = (0..plan.packages.len())
            .filter(|&i| before.live[i] && plan.packages[i].fetch)
            .collect();
        // A directory about to receive another version is replaced, not overlaid, and before
        // the parallel phase: a package nested under it may be extracting at the same time.
        for &i in &todo {
            let dir = &plan.packages[i].dir;
            if dir.symlink_metadata().is_ok() {
                std::fs::remove_dir_all(dir)?;
            }
        }
        let next = AtomicUsize::new(0);
        let failed: Mutex<Vec<(usize, Error)>> = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..self.concurrency.min(todo.len()) {
                s.spawn(|| {
                    loop {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&i) = todo.get(n) else { break };
                        if let Err(e) = self.fetch_one(&plan.packages[i])
                            && let Ok(mut f) = failed.lock()
                        {
                            f.push((i, e));
                        }
                    }
                });
            }
        });
        let mut failed = failed.into_inner().unwrap_or_default();
        if failed.is_empty() {
            return Ok(before.live);
        }
        // Completion order is nondeterministic; report the same failure every time.
        failed.sort_by_key(|(i, _)| *i);
        let seeds = before.dropped_seeds.into_iter().map(|i| (i, None));
        let after = settle_seeded(
            plan,
            seeds.chain(failed.into_iter().map(|(i, e)| (i, Some(e)))),
        )?;
        for &i in &todo {
            if !after.live[i] {
                let _ = std::fs::remove_dir_all(&plan.packages[i].dir);
            }
        }
        Ok(after.live)
    }

    fn fetch_one(&self, p: &Planned) -> Result<(), Error> {
        let tgz = self.fetch(&p.manifest.dist.tarball, "application/octet-stream")?;
        extract::verify(&tgz, &p.manifest.dist, &p.name, &p.version)?;
        extract::extract(&tgz, &p.dir)
    }

    /// Fetch every packument in `names` that is not cached yet, `concurrency` at a time.
    /// Failures are not cached: the later serial lookup refetches and reports them.
    fn prefetch<'a>(&self, names: impl Iterator<Item = &'a str>) {
        let missing: Vec<&str> = {
            let Ok(cache) = self.packuments.lock() else {
                return;
            };
            let mut seen = HashSet::new();
            names
                .filter(|n| !cache.contains_key(*n) && seen.insert(*n))
                .collect()
        };
        if missing.len() < 2 {
            return;
        }
        let next = AtomicUsize::new(0);
        let fetched: Mutex<Vec<(String, Packument)>> = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..self.concurrency.min(missing.len()) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(name) = missing.get(i) else { break };
                        if let Ok(p) = self.fetch_packument(name)
                            && let Ok(mut f) = fetched.lock()
                        {
                            f.push((name.to_string(), p));
                        }
                    }
                });
            }
        });
        if let Ok(mut cache) = self.packuments.lock() {
            for (name, p) in fetched.into_inner().unwrap_or_default() {
                cache.entry(name).or_insert(p);
            }
        }
    }

    fn fetch_packument(&self, name: &str) -> Result<Packument, Error> {
        let url = format!("{}/{}", self.registry_for(name), name.replace('/', "%2f"));
        let body = self.fetch(&url, ABBREVIATED)?;
        registry::parse(name, &body)
    }

    fn registry_for(&self, name: &str) -> &str {
        name.starts_with('@')
            .then(|| name.split('/').next())
            .flatten()
            .and_then(|scope| self.scoped.get(scope))
            .map_or(self.registry.as_str(), String::as_str)
    }

    /// Every request goes through here: the `accept` header, plus the credential whose URL
    /// prefix is the longest match for `url`, if any.
    fn fetch(&self, url: &str, accept: &str) -> Result<Vec<u8>, Error> {
        let nerf = npmrc::nerf(url);
        let mut headers = vec![("accept", accept)];
        if let Some((_, value)) = self
            .auth
            .iter()
            .filter(|(prefix, _)| nerf.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
        {
            headers.push(("x-authorization", value.as_str()));
        }
        self.transport.get(url, &headers)
    }

    fn manifest_for(&self, name: &str, version: &str) -> Result<Manifest, Error> {
        self.with_packument(name, |p| {
            p.versions
                .get(version)
                .cloned()
                .ok_or_else(|| Error::NoVersion {
                    name: name.to_string(),
                    spec: version.to_string(),
                })
        })?
    }

    /// Run `f` over the abbreviated packument for `name`, fetching it on first use. One
    /// registry round-trip per package name per install, however many dependents share it.
    fn with_packument<R>(&self, name: &str, f: impl FnOnce(&Packument) -> R) -> Result<R, Error> {
        let mut cache = self
            .packuments
            .lock()
            .map_err(|_| Error::Transport("packument cache poisoned".into()))?;
        if !cache.contains_key(name) {
            let p = self.fetch_packument(name)?;
            cache.insert(name.to_string(), p);
        }
        Ok(f(&cache[name]))
    }
}

/// One dependency edge waiting to be placed.
struct Want {
    /// Index of the dependent in the plan; `None` for a requested package.
    parent: Option<usize>,
    parent_dir: PathBuf,
    name: String,
    range: String,
    /// The edge itself is an `optionalDependencies` entry.
    optional_edge: bool,
    /// The dependent was reached only through an optional edge SO FAR, so a failure here
    /// must not abort planning. Provisional: a later required path can still claim it, which
    /// is why failures are recorded for [`settle`] rather than swallowed.
    soft: bool,
}

struct Planned {
    name: String,
    version: String,
    dir: PathBuf,
    manifest: Manifest,
    /// False for a package already on disk at a satisfying version.
    fetch: bool,
    /// `(child index, edge is optional)`.
    children: Vec<(usize, bool)>,
}

impl Planned {
    /// The edges this package adds to the next level. A name listed under
    /// `optionalDependencies` is optional even when it also appears under `dependencies`,
    /// because `npm publish` mirrors it there; a bundled name ships inside the tarball.
    fn wants(&self, index: usize, soft: bool) -> Vec<Want> {
        let m = &self.manifest;
        let bundled = |n: &str| m.bundle_dependencies.contains(n);
        let want = |n: &String, r: &String, optional_edge: bool| Want {
            parent: Some(index),
            parent_dir: self.dir.clone(),
            name: n.clone(),
            range: r.clone(),
            optional_edge,
            soft: soft || optional_edge,
        };
        m.dependencies
            .iter()
            .filter(|(n, _)| !m.optional_dependencies.contains_key(*n) && !bundled(n))
            .map(|(n, r)| want(n, r, false))
            .chain(
                m.optional_dependencies
                    .iter()
                    .filter(|(n, _)| !bundled(n))
                    .map(|(n, r)| want(n, r, true)),
            )
            .collect()
    }
}

#[derive(Default)]
struct Plan {
    /// In placement order.
    packages: Vec<Planned>,
    /// Indices of the requested packages, in request order.
    roots: Vec<usize>,
    /// Directory → index into `packages`.
    index: HashMap<PathBuf, usize>,
    /// `(package to drop, why)`: a package whose required dependency could not be resolved.
    failures: Vec<(usize, Error)>,
}

impl Plan {
    fn push(&mut self, p: Planned) -> usize {
        self.index.insert(p.dir.clone(), self.packages.len());
        self.packages.push(p);
        self.packages.len() - 1
    }

    fn link(&mut self, want: &Want, child: usize) {
        if let Some(parent) = want.parent {
            self.packages[parent]
                .children
                .push((child, want.optional_edge));
        }
    }

    /// Walk from the dependent's directory up to the install root looking for `name` at a
    /// version satisfying `range`, in the plan first and then on disk — Node's own resolution
    /// order, so whatever is found here is what `require` will find too.
    fn satisfied(
        &self,
        root: &Path,
        parent: &Path,
        name: &str,
        range: &str,
    ) -> Result<Option<(String, PathBuf)>, Error> {
        let mut dir = Some(parent);
        while let Some(d) = dir {
            let candidate = d.join("node_modules").join(name);
            let version = match self.index.get(&candidate) {
                Some(&i) => Some(self.packages[i].version.clone()),
                None => installed_version(&candidate)?,
            };
            if let Some(v) = version
                && registry::satisfies(&v, range)
            {
                return Ok(Some((v, candidate)));
            }
            if d == root {
                break;
            }
            dir = d.parent();
        }
        Ok(None)
    }
}

struct Settled {
    /// Per package: still part of the install.
    live: Vec<bool>,
    /// The packages that were dropped for their own reasons, kept so a second pass can add
    /// more without recomputing the first.
    dropped_seeds: Vec<usize>,
}

fn settle(plan: &Plan, failures: Vec<(usize, Error)>) -> Result<Settled, Error> {
    settle_seeded(plan, failures.into_iter().map(|(i, e)| (i, Some(e))))
}

/// Decide what survives. Each seed names a package that cannot be installed. A dropped
/// package drops every dependent that reaches it over a NON-optional edge, transitively; an
/// optional edge absorbs the failure. If a seed's climb reaches a required package — one a
/// requested package reaches over non-optional edges alone — the install fails with THAT seed's error: an
/// earlier failure that an optional edge absorbed is not what made the install fatal, so it
/// is never the one reported. Whatever is then unreachable from the root through surviving
/// packages is not live, which is what removes a dropped branch's own dependencies with it.
/// "Reachable" is always from the requested packages, so a requested package is never dropped.
fn settle_seeded(
    plan: &Plan,
    seeds: impl Iterator<Item = (usize, Option<Error>)>,
) -> Result<Settled, Error> {
    let packages = &plan.packages;
    let reach = |follow_optional: bool, skip: &[bool]| {
        let mut seen = vec![false; packages.len()];
        let mut stack = plan.roots.clone();
        while let Some(i) = stack.pop() {
            if seen[i] || skip[i] {
                continue;
            }
            seen[i] = true;
            for &(c, optional) in &packages[i].children {
                if follow_optional || !optional {
                    stack.push(c);
                }
            }
        }
        seen
    };
    let required = reach(false, &vec![false; packages.len()]);
    let mut hard_dependents = vec![Vec::new(); packages.len()];
    for (i, p) in packages.iter().enumerate() {
        for &(c, optional) in &p.children {
            if !optional {
                hard_dependents[c].push(i);
            }
        }
    }
    // Climb seed by seed, in order. Sharing `dropped` across climbs is sound: a node an
    // earlier, absorbed climb already visited has nothing required above it.
    let mut dropped = vec![false; packages.len()];
    let mut dropped_seeds = Vec::new();
    for (seed, error) in seeds {
        dropped_seeds.push(seed);
        let mut stack = vec![seed];
        while let Some(i) = stack.pop() {
            if std::mem::replace(&mut dropped[i], true) {
                continue;
            }
            if required[i] {
                return Err(error.unwrap_or_else(|| {
                    Error::Transport("a required package failed to install".into())
                }));
            }
            stack.extend(&hard_dependents[i]);
        }
    }
    Ok(Settled {
        live: reach(true, &dropped),
        dropped_seeds,
    })
}

/// Link every command a top-level package declares into `node_modules/.bin`, requested
/// packages first so theirs win a name clash, then in placement order. A relative symlink on
/// Unix; on Windows a `.cmd` shim that runs the script with `node`, which every npm bin in
/// practice is (npm's own shim also reads the shebang; nothing here needs that yet).
fn link_bins(root: &Path, plan: &Plan, live: &[bool]) -> Result<BTreeMap<String, PathBuf>, Error> {
    let nm = root.join("node_modules");
    let bin_dir = nm.join(".bin");
    let mut bins = BTreeMap::new();
    let rest = (0..plan.packages.len()).filter(|i| !plan.roots.contains(i));
    for i in plan.roots.iter().copied().chain(rest) {
        let p = &plan.packages[i];
        if !live[i] || p.dir != nm.join(&p.name) {
            continue;
        }
        for (cmd, rel) in p.manifest.bin.entries(&p.name) {
            let target = p.dir.join(&rel);
            if bins.contains_key(&cmd)
                || cmd.is_empty()
                || cmd.contains(['/', '\\'])
                || cmd == ".."
                || !target.is_file()
            {
                continue;
            }
            make_executable(&target)?;
            std::fs::create_dir_all(&bin_dir)?;
            write_bin_link(&bin_dir, &cmd, &p.name, &rel)?;
            bins.insert(cmd, target);
        }
    }
    Ok(bins)
}

#[cfg(unix)]
fn write_bin_link(bin_dir: &Path, cmd: &str, name: &str, rel: &str) -> Result<(), Error> {
    let link = bin_dir.join(cmd);
    if link.symlink_metadata().is_ok() {
        std::fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(Path::new("..").join(name).join(rel), &link)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_bin_link(bin_dir: &Path, cmd: &str, name: &str, rel: &str) -> Result<(), Error> {
    let script = format!("{name}\\{rel}").replace('/', "\\");
    std::fs::write(
        bin_dir.join(format!("{cmd}.cmd")),
        format!("@ECHO off\r\nnode \"%~dp0\\..\\{script}\" %*\r\n"),
    )?;
    Ok(())
}

fn installed_version(pkg_dir: &Path) -> Result<Option<String>, Error> {
    let manifest = pkg_dir.join("package.json");
    if !manifest.is_file() {
        return Ok(None);
    }
    #[derive(serde::Deserialize)]
    struct V {
        version: String,
    }
    let v: V = serde_json::from_slice(&std::fs::read(&manifest)?).map_err(|e| Error::Registry {
        name: pkg_dir.display().to_string(),
        detail: e.to_string(),
    })?;
    Ok(Some(v.version))
}

/// `@scope/name@^1` → (`@scope/name`, `^1`); a bare name has an empty range (→ `latest`).
fn split_spec(spec: &str) -> (&str, &str) {
    match spec.rfind('@') {
        Some(i) if i > 0 => (&spec[..i], &spec[i + 1..]),
        _ => (spec, ""),
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_spec;

    #[test]
    fn spec_splits_on_the_last_at_sign_only() {
        assert_eq!(split_spec("chalk"), ("chalk", ""));
        assert_eq!(split_spec("chalk@5"), ("chalk", "5"));
        assert_eq!(split_spec("@scope/name"), ("@scope/name", ""));
        assert_eq!(split_spec("@scope/name@^1.2"), ("@scope/name", "^1.2"));
    }
}
