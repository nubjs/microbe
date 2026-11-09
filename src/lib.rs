//! microbe — install npm packages and their dependency trees into a directory: one package
//! by spec, or a `package.json`-shaped map of names to ranges.
//!
//! ```no_run
//! let done = microbe::Microbe::new()?.install("esbuild@^0.25", std::path::Path::new("/tmp/x"))?;
//! let root = &done.roots[0];
//! println!("{}@{} in {} with bins {:?}", root.name, root.version, root.dir.display(), done.bins);
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
//! does) or it reaches something required (the install fails).
//!
//! What it deliberately does not do: run lifecycle scripts (reported instead, see
//! [`Installation::skipped_install_scripts`]), honour `peerDependencies`, write a lockfile,
//! keep a cache or store, or reconcile with an existing `node_modules` beyond replacing a
//! package it is about to install at another version. Those are the parts of a
//! package manager that take up the space. A package already on disk at a satisfying
//! version is left alone and costs no request; an install that did not finish leaves a
//! marker that makes the next one replace everything it needs. Two installs must not run
//! in one directory at the same time.

mod error;
mod extract;
mod npmrc;
mod registry;
// Builds with in-binary TLS never call the host clients; they stay compiled so every
// platform type-checks the Linux path.
#[cfg_attr(
    any(feature = "tls", target_os = "macos", target_os = "windows"),
    allow(dead_code)
)]
mod transport;

pub use error::Error;
pub use transport::{TIMEOUT, Transport};

use registry::{Manifest, Packument};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The registry used until [`Microbe::registry`] or an `.npmrc` names another.
pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";
/// npm's own value: the abbreviated document, or the full one from a registry without it.
const ABBREVIATED: &str =
    "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*";
/// Written to `node_modules` before anything on disk changes and removed once the install
/// has finished, so that a crash or a failed download cannot pass for a complete tree.
const INCOMPLETE: &str = ".microbe-incomplete";
/// Matches npm's and pnpm's default network concurrency.
const DEFAULT_CONCURRENCY: usize = 16;
const RETRIES: usize = 2;
const RETRY_BACKOFF: [std::time::Duration; RETRIES] = [
    std::time::Duration::from_millis(500),
    std::time::Duration::from_secs(2),
];

/// An installer: a transport plus where to find packages and which credentials to send.
/// Configure it with the builder methods, then call [`Microbe::install`] or
/// [`Microbe::install_all`] as often as needed. Each call reads the registry afresh.
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

/// What an install produced. [`Microbe::install`] yields exactly one [`Root`]. Serializes as
/// camel-cased JSON, the shape `microbe --json` prints and the Node addon returns.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Installation {
    /// The requested packages, in request order.
    pub roots: Vec<Root>,
    /// Every command linked under `<dir>/node_modules/.bin`, mapped to the absolute path of
    /// the script it runs, with the executable bit set. Requested packages win a name clash
    /// with a dependency.
    pub bins: BTreeMap<String, PathBuf>,
    /// Packages extracted by this call; a package already present at a satisfying version
    /// is not counted.
    pub packages: usize,
    /// `name@version` of every package whose install script was NOT run.
    pub skipped_install_scripts: Vec<String>,
}

/// A requested package, as installed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct Root {
    /// The name it was requested under, which for an `npm:` alias is the alias.
    pub name: String,
    pub version: String,
    /// `<dir>/node_modules/<name>`.
    pub dir: PathBuf,
}

impl Microbe {
    /// Uses in-binary TLS where the build has it (macOS, Windows, Linux with `--features tls`),
    /// else the first of `node`, `curl`, `wget`, `python3` on the host; [`Error::NoTransport`]
    /// names them when none is found. [`Microbe::with_transport`] skips the detection.
    pub fn new() -> Result<Self, Error> {
        Ok(Self::from_boxed(transport::detect()?))
    }

    /// An installer that makes every request through `transport`.
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

    /// The registry for packages no [`Microbe::scoped_registry`] claims.
    pub fn registry(mut self, url: &str) -> Self {
        self.registry = url.trim_end_matches('/').to_string();
        self
    }

    /// Route packages under `@scope` to `url`, as an `@scope:registry` key does.
    pub fn scoped_registry(mut self, scope: &str, url: &str) -> Self {
        let scope = if scope.starts_with('@') {
            scope.to_string()
        } else {
            format!("@{scope}")
        };
        self.scoped
            .insert(scope, url.trim_end_matches('/').to_string());
        self
    }

    /// Send `authorization: <value>` with every request whose URL starts with `prefix`,
    /// given as a URL (`https://npm.acme.dev/`) or in npm's `//npm.acme.dev/` form. The
    /// longest matching prefix wins. The `.npmrc` credential keys build on this.
    pub fn auth(mut self, prefix: &str, value: &str) -> Self {
        self.auth.push((npmrc::prefix(prefix), value.to_string()));
        self
    }

    /// Apply an `.npmrc` at an EXPLICIT path: `registry`, `@scope:registry`, and credentials
    /// (`_authToken`, `_auth`, `username` with `_password`) keyed by URL prefix, as npm keys
    /// them. Nothing is discovered, and `${VAR}` is not expanded — resolve it and use
    /// [`Microbe::npmrc_contents`].
    pub fn npmrc(self, path: &Path) -> Result<Self, Error> {
        let contents = std::fs::read_to_string(path).map_err(|e| at(path, e))?;
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

    /// `spec` is `name`, `name@tag`, `name@version` or `name@range` (`@scope/name@^1` works),
    /// or an alias, `name@npm:real@range`. `dir` is created if needed; packages go under
    /// `dir/node_modules/`.
    pub fn install(&self, spec: &str, dir: &Path) -> Result<Installation, Error> {
        let (name, range) = split_spec(spec);
        self.install_all([(name, range)], dir)
    }

    /// Install every `(name, range)` pair — the shape of a `package.json` `dependencies`
    /// map, so a range may be a dist-tag or `npm:real@range` — into one `dir/node_modules`,
    /// and link every command a top-level package declares under `dir/node_modules/.bin`.
    /// A name given twice is taken once, at its first range. `peerDependencies` are ignored
    /// throughout.
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
        std::fs::create_dir_all(dir).map_err(|e| at(dir, e))?;
        let root = canonical(dir)?;
        if let Ok(mut cache) = self.packuments.lock() {
            cache.clear();
        }
        let marker = root.join("node_modules").join(INCOMPLETE);
        let mut plan = self.plan(&root, &deps, marker.exists())?;
        let live = self.materialize(&mut plan, &marker)?;
        let bins = link_bins(&root, &plan, &live)?;
        if plan.packages.iter().any(|p| p.fetch) {
            let _ = std::fs::remove_file(&marker);
        }
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
                .map(|(p, _)| format!("{}@{}", p.package, p.version))
                .collect(),
        })
    }

    /// Phase one. Breadth-first so that placement is deterministic: whichever version of a
    /// name is reached first from the requested packages takes the flat slot, and later
    /// conflicting versions nest under their dependents. Each level's packuments are fetched
    /// together before any of that level is placed.
    fn plan(&self, root: &Path, deps: &[(&str, &str)], distrust_disk: bool) -> Result<Plan, Error> {
        let mut plan = Plan {
            distrust_disk,
            ..Plan::default()
        };
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
            self.prefetch(
                level
                    .iter()
                    .filter(|w| w.safe() && plan.satisfied(root, w, w.target().1).is_none())
                    .map(|w| w.target().0),
            );
            let mut next = VecDeque::new();
            for want in level.drain(..) {
                if let Some(i) = self.place(root, &mut plan, &want)? {
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

    /// Decide where one wanted package goes. Returns the index of a newly planned package so
    /// the caller can enqueue its dependencies; `None` when the want was already satisfied,
    /// was skipped, or failed softly.
    fn place(&self, root: &Path, plan: &mut Plan, want: &Want) -> Result<Option<usize>, Error> {
        if !want.safe() {
            return unresolved(plan, want, Error::UnsafePath(want.name.clone()));
        }
        let (package, spec) = want.target();
        let spec = if spec.is_empty() { "latest" } else { spec };
        // A dist-tag has no range to test a present package against, so it stands for the
        // version it points at right now.
        let tagged = if registry::is_range(spec) {
            None
        } else {
            match self.with_packument(package, |p| p.dist_tags.get(spec).cloned()) {
                Ok(version) => version,
                Err(e) => return unresolved(plan, want, e),
            }
        };
        let range = tagged.as_deref().unwrap_or(spec);
        if let Some(dir) = plan.satisfied(root, want, range) {
            if let Some(&child) = plan.index.get(&dir) {
                // An edge to a package planned earlier. It is what lets a package first
                // reached through an optional branch turn out to be required after all.
                plan.link(want, child);
            } else if want.parent.is_none()
                && let Some(found) = plan.on_disk(&dir)
            {
                // Planned unfetched, so its bins are still linked and it is still reported.
                return Ok(Some(plan.push(Planned {
                    name: want.name.clone(),
                    package: found.name,
                    version: found.version,
                    dir,
                    manifest: found.manifest,
                    fetch: false,
                    children: Vec::new(),
                })));
            }
            return Ok(None);
        }
        let picked = self.with_packument(package, |p| {
            registry::pick(p, package, range).map(|(v, m)| (v.to_string(), m.clone()))
        });
        let (version, manifest) = match picked.and_then(|p| p) {
            Ok(vm) => vm,
            Err(e) => return unresolved(plan, want, e),
        };
        if want.optional_edge && !registry::platform_allowed(&manifest) {
            return Ok(None);
        }
        let flat = root.join("node_modules").join(&want.name);
        let dir = if plan.index.contains_key(&flat) || plan.on_disk(&flat).is_some() {
            want.parent_dir.join("node_modules").join(&want.name)
        } else {
            flat
        };
        let child = plan.push(Planned {
            name: want.name.clone(),
            package: package.to_string(),
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
    fn materialize(&self, plan: &mut Plan, marker: &Path) -> Result<Vec<bool>, Error> {
        let failures = std::mem::take(&mut plan.failures);
        let before = settle(plan, failures)?;
        let todo: Vec<usize> = (0..plan.packages.len())
            .filter(|&i| before.live[i] && plan.packages[i].fetch)
            .collect();
        if !todo.is_empty()
            && let Some(node_modules) = marker.parent()
        {
            std::fs::create_dir_all(node_modules).map_err(|e| at(node_modules, e))?;
            std::fs::write(marker, "").map_err(|e| at(marker, e))?;
        }
        // A directory about to receive another version is replaced, not overlaid, and before
        // the parallel phase: a package nested under it may be extracting at the same time.
        for &i in &todo {
            let dir = &plan.packages[i].dir;
            if dir.symlink_metadata().is_ok() {
                std::fs::remove_dir_all(dir).map_err(|e| at(dir, e))?;
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
        let tgz = self.fetch(&p.manifest.dist.tarball, "*/*")?;
        extract::verify(&tgz, &p.manifest.dist, &p.package, &p.version)?;
        extract::extract(&tgz, &p.dir).map_err(|e| match e {
            Error::Io(io) => at(&p.dir, io),
            other => other,
        })
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
            headers.push(("authorization", value.as_str()));
        }
        // Two retries on a transport failure or a 429 / 5xx, as npm does by default. A 4xx
        // other than 429 is the registry's final word and is returned at once.
        let mut attempt = 0;
        loop {
            match self.transport.get(url, &headers) {
                Err(Error::Transport(_))
                | Err(Error::Status {
                    status: 429 | 500..=599,
                    ..
                }) if attempt < RETRIES => {
                    std::thread::sleep(RETRY_BACKOFF[attempt]);
                    attempt += 1;
                }
                other => return other,
            }
        }
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

/// A dependency that cannot be resolved. An optional edge simply goes without it. A required
/// dependency of a package that is itself only optionally reachable (so far) is charged to
/// that package for [`settle`] to decide. Anything else ends the install.
fn unresolved(plan: &mut Plan, want: &Want, error: Error) -> Result<Option<usize>, Error> {
    if want.optional_edge {
        return Ok(None);
    }
    match want.parent {
        Some(parent) if want.soft => {
            plan.failures.push((parent, error));
            Ok(None)
        }
        _ => Err(error),
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

impl Want {
    /// The package and range this edge resolves. For `npm:real@range` that is the real
    /// package, which lands under the alias.
    fn target(&self) -> (&str, &str) {
        registry::alias(&self.range).unwrap_or((&self.name, &self.range))
    }

    fn safe(&self) -> bool {
        registry::safe_name(&self.name) && registry::safe_name(self.target().0)
    }
}

struct Planned {
    /// The directory name under `node_modules`.
    name: String,
    /// The package itself; differs from `name` for an `npm:` alias.
    package: String,
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
    /// The last install here did not finish, so nothing on disk can be taken as complete.
    distrust_disk: bool,
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

    fn on_disk(&self, dir: &Path) -> Option<registry::Installed> {
        if self.distrust_disk {
            return None;
        }
        registry::installed(dir)
    }

    /// Walk from the dependent's directory up to the install root looking for the package at
    /// a version satisfying `range`, in the plan first and then on disk — Node's own
    /// resolution order, so whatever is found here is what `require` will find too. The
    /// directory of a package about to be replaced is not looked into: what is on disk under
    /// it goes with it.
    fn satisfied(&self, root: &Path, want: &Want, range: &str) -> Option<PathBuf> {
        let package = want.target().0;
        let mut dir = Some(want.parent_dir.as_path());
        while let Some(d) = dir {
            let candidate = d.join("node_modules").join(&want.name);
            let replaced = self.index.get(d).is_some_and(|&i| self.packages[i].fetch);
            let found = match self.index.get(&candidate) {
                Some(&i) => Some((
                    self.packages[i].package.clone(),
                    self.packages[i].version.clone(),
                )),
                None if replaced => None,
                None => self.on_disk(&candidate).map(|p| (p.name, p.version)),
            };
            if let Some((name, version)) = found
                && name == package
                && registry::satisfies(&version, range)
            {
                return Some(candidate);
            }
            if d == root {
                break;
            }
            dir = d.parent();
        }
        None
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
        for (cmd, rel) in p.manifest.bin.entries(&p.package) {
            // Both come from the registry: a bin must not be chmodded or linked outside its
            // own package.
            let inside = Path::new(&rel)
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
            let rel: PathBuf = Path::new(&rel)
                .components()
                .filter(|c| matches!(c, Component::Normal(_)))
                .collect();
            let target = p.dir.join(&rel);
            if bins.contains_key(&cmd)
                || cmd.is_empty()
                || cmd.contains(['/', '\\', ':'])
                || cmd == "."
                || cmd == ".."
                || !inside
                || !target.is_file()
            {
                continue;
            }
            make_executable(&target)?;
            std::fs::create_dir_all(&bin_dir)?;
            write_bin_link(&bin_dir, &cmd, &p.name, &rel.to_string_lossy())?;
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

/// An I/O error that names the path it happened on.
fn at(path: &Path, error: std::io::Error) -> Error {
    Error::Io(std::io::Error::new(
        error.kind(),
        format!("{}: {error}", path.display()),
    ))
}

/// [`Path::canonicalize`] without the `\\?\` verbatim prefix Windows adds: every path this
/// crate returns or prints is one a person or another tool can use unchanged.
fn canonical(dir: &Path) -> Result<PathBuf, Error> {
    let real = dir.canonicalize()?;
    if !cfg!(windows) {
        return Ok(real);
    }
    let s = real.to_string_lossy();
    Ok(match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.starts_with(r"UNC\") => PathBuf::from(format!(r"\\{}", &rest[4..])),
        Some(rest) => PathBuf::from(rest),
        None => real,
    })
}

/// `@scope/name@^1` → (`@scope/name`, `^1`); a bare name has an empty range (→ `latest`).
/// The split is on the first `@` past a scope's, because an alias's range has one of its own:
/// `tool@npm:@scope/real@^1`. Public for the binary and the addon only.
#[doc(hidden)]
pub fn split_spec(spec: &str) -> (&str, &str) {
    match spec.get(1..).and_then(|rest| rest.find('@')) {
        Some(i) => (&spec[..=i], &spec[i + 2..]),
        None => (spec, ""),
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
    fn spec_splits_on_the_first_at_sign_past_the_scope() {
        assert_eq!(split_spec("chalk"), ("chalk", ""));
        assert_eq!(split_spec("chalk@5"), ("chalk", "5"));
        assert_eq!(split_spec("@scope/name"), ("@scope/name", ""));
        assert_eq!(split_spec("@scope/name@^1.2"), ("@scope/name", "^1.2"));
        assert_eq!(split_spec("a@npm:@scope/b@^1"), ("a", "npm:@scope/b@^1"));
        assert_eq!(split_spec("@s/a@npm:b"), ("@s/a", "npm:b"));
    }
}
