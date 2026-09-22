# microbe

The smallest embeddable npm package installer. One crate, pure Rust, no async runtime: it fetches packages and their dependency trees from the registry into a directory the caller names, verifies integrity, links the bins, and reports where everything landed.

```rust
use microbe::Microbe;
use std::path::Path;

let done = Microbe::new()?.install("esbuild@^0.25", Path::new("/tmp/tools"))?;
let esbuild = &done.bins["esbuild"]; // /tmp/tools/node_modules/.bin/esbuild -> esbuild/bin/esbuild
```

```
microbe install <name[@spec]>... --dir <path> [--registry <url>] [--npmrc <file>]
microbe install --from package.json --dir /tmp/tools    # its `dependencies` map; other keys are ignored
```

## From Node

The same installer is on npm as [`@nubjs/microbe`](napi/README.md), a Node-API addon with the platform builds as optional dependencies:

```js
import { install } from "@nubjs/microbe";
const done = await install({ eslint: "^9" }, "/tmp/tools"); // or install(["eslint@^9"], dir)
```

## Who it is for

Microbe does the `npx`-shaped job for a tool that is not a package manager: install a package, or a small map of them, then run what was installed. Typical hosts are a language server launcher, an editor or agent that needs a formatter or a linter on demand, a build tool that pulls a plugin, or a CLI that ships without Node dependencies but needs one at run time.

It is not a replacement for npm, pnpm or bun in a project checkout. There is no lockfile, no store, no `node_modules` reconciliation, no lifecycle scripts, no workspaces. Those are the parts of a package manager that take up the space, and a host that needs them should call a package manager.

## Status

Beta. The API is small and settled enough to build on, and CI verifies every change on Linux, macOS and Windows against a real registry. Until 1.0 a minor release may add a field to `Installation`, a variant to `Error`, or a method to `Microbe`; every such type is `#[non_exhaustive]` so that is not a breaking change for a caller. A change that breaks a caller bumps the minor version and is listed in [`CHANGELOG.md`](CHANGELOG.md).

## API

```rust
use microbe::{Microbe, Transport};

let m = Microbe::new()?                      // in-binary TLS, or the first HTTPS client on the host
    .registry("https://registry.example.com") // default is registry.npmjs.org
    .npmrc(Path::new("/etc/tool/.npmrc"))?    // explicit path only; nothing is discovered
    .concurrency(8);                          // parallel fetches; default 16

// One package by spec: `name`, `name@tag`, `name@1.2.3`, `name@^1`, `@scope/name@^1`.
let one = m.install("eslint@^9", dir)?;
// A dependency map, the shape of package.json#/dependencies. Duplicate names take the first range.
let many = m.install_all([("eslint", "^9"), ("prettier", "3")], dir)?;

many.roots;                    // Vec<Root { name, version, dir }>, in request order
many.bins;                     // BTreeMap<command, absolute script path>; also linked in node_modules/.bin
many.packages;                 // tarballs extracted by this call; a package already present is not counted
many.skipped_install_scripts;  // "name@version" of every package whose install script was not run

// An embedder that already links an HTTP client supplies it and pays for no TLS.
struct MyClient;
impl Transport for MyClient {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>, microbe::Error> { todo!() }
}
let m = Microbe::with_transport(MyClient);
```

A second install into the same directory fetches only what is missing or at the wrong version. A package about to be installed at another version is removed first, never overlaid.

## Everything is explicit

Microbe is an embedder-facing tool, not a human CLI, so it never guesses. The target directory is always given. Nothing is read from environment variables. No configuration file is discovered by walking up the filesystem: a registry is passed as a URL, and an `.npmrc` is passed as an explicit path. What the embedder does not pass, Microbe does not know about. The one thing detected at run time is which HTTPS client the host has, and an embedder that supplies a `Transport` opts out of that too.

## Registry and credentials

An `.npmrc` is applied only from an explicit path (`Microbe::npmrc`, or `--npmrc <file>`), or from contents the embedder already holds (`Microbe::npmrc_contents`). Four keys are read:

```ini
registry=https://registry.example.com
@acme:registry=https://npm.acme.dev/
//npm.acme.dev/:_authToken=...          # Bearer
//legacy.example.com/:_auth=...         # Basic, as npm stores it
//other.example.com/:username=...       # Basic, with the base64 _password below
//other.example.com/:_password=...
```

Credentials are keyed by URL prefix exactly as npm keys them, and the longest matching prefix wins. A `${VAR}` reference is not expanded, because nothing is read from the environment; a consumed key that still holds one is an error, so a placeholder is never sent as a token.

## Size

Stripped, `opt-level = "z"` with fat LTO, measured by CI on 2026-09-22 with the stable toolchain:

| Target | default build | TLS in it |
| --- | --- | --- |
| aarch64-apple-darwin | 853 KB | Security.framework, always |
| x86_64-pc-windows-msvc | 800 KB | SChannel, always |
| x86_64-unknown-linux-gnu | 702 KB | none; `--features tls` adds rustls for about 1.1 MB |

The budget is decided by TLS and nothing else. The resolve, verify and extract core is about 60 KB of code. On macOS and Windows the operating system's TLS is reachable through Rust bindings for about 300 KB, with no C compiled and no process spawned, so it is always in. On Linux there is no system TLS to bind to, a rustls stack costs about 1.1 MB, and so the Linux build links none by default and borrows an HTTPS client the host already has. The CI size job fails if any default build reaches 1 MB.

## Speed

Two phases. The plan phase walks the dependency graph breadth-first, fetching each level's packuments in parallel and deciding every package's directory before anything is downloaded. The materialize phase then downloads, verifies and extracts every planned tarball in parallel, sixteen at a time by default. Cold installs into an empty directory, measured back to back on one machine in one minute, so the numbers are comparable to each other and to nothing else:

| Package | microbe | npm 11 | pnpm 10 |
| --- | --- | --- | --- |
| express (71 packages) | 2.2 s | 2.2 s | 1.7 s |
| eslint (77 packages) | 5.7 s | 8.5 s | 8.6 s |
| vite (16 packages, native binaries) | 27.8 s | 51.5 s | 23.0 s |

## How it reaches the network

The in-binary client is used on macOS and Windows, and on Linux when built with `--features tls`. A Linux build without it tries, in order:

1. **`node`** — one long-lived child running `fetch`, with requests multiplexed over its stdio so the parallel install actually runs in parallel and undici reuses connections. This is the anchor, because whatever gets installed is about to be run by Node anyway.
2. **`curl`** — most full Linux distributions.
3. **`wget`** — busybox, so Alpine.
4. **`python3`** — the Python container images, which carry neither `curl` nor `wget` but do carry Python with its `ssl` module and a CA bundle.

**The default Linux build requires one of those four programs on `PATH`, or a `Transport` supplied by the embedder.** A survey of 16 popular container base images found `curl` on 4 of them, and 8 carried neither `curl` nor `wget`; every Node image carries Node. The Debian and Ubuntu slim images carry none of the four and no CA bundle either, so on those the answer is `--features tls` or an embedder-supplied `Transport`.

Every request is bounded by a 300 second timeout, npm's default, and a transport failure or a 429 or 5xx response is retried twice. Every host client is told to refuse a redirect off HTTPS: a tarball is protected by its integrity hash, but a packument is not.

## What it implements

Packages land flat under `<dir>/node_modules`. A version conflict nests the loser under its dependent, which is what Node's resolver walks up to find, and placement is deterministic: the same request always produces the same tree. Resolution is first-wins over `dependencies` plus platform-matching `optionalDependencies`, one abbreviated packument fetch per package name. A name listed under `optionalDependencies` is optional even when it also appears under `dependencies`, because `npm publish` mirrors it there. Optionality covers the whole branch: when anything an optional dependency itself requires cannot be resolved or fails its integrity check, that optional dependency is dropped along with every package only it needed, and the install succeeds, unless a required path reaches the same package, in which case the install fails. A name listed under `bundleDependencies` ships inside its parent's tarball and is never fetched. Tarballs are checked against `dist.integrity`, falling back to the pre-SRI `dist.shasum`. A tarball entry whose path would escape its package directory is refused.

Peer dependencies are ignored and install scripts are not run. Packages that declare one are named in `Installation::skipped_install_scripts` so the caller can decide what that means; for a prebuilt-binary package like esbuild or biome the postinstall is a no-op, because the platform package carrying the binary is an optional dependency that Microbe already installed.

Every command a top-level package declares is linked under `node_modules/.bin`: a relative symlink on Unix, a `.cmd` shim that runs the script with `node` on Windows. A requested package wins a name clash with a dependency.

## Tests

`cargo test` runs against an in-memory registry serving real gzipped tarballs with real integrity strings, so the whole install path runs with no network. The `Sweep` workflow, run on demand, builds the release binary on Linux, macOS and Windows and installs real packages from the registry with it.
