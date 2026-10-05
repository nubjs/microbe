# microbe

The smallest embeddable npm package installer. One crate, pure Rust, no async runtime: it installs a package and its dependency tree into a directory the caller names, verifies integrity, links the bins, and reports where everything landed.

```rust
use microbe::Microbe;
use std::path::Path;

let done = Microbe::new()?.install("esbuild@^0.25", Path::new("/tmp/tools"))?;
let esbuild = &done.bins["esbuild"]; // /tmp/tools/node_modules/.bin/esbuild -> esbuild/bin/esbuild
```

```sh
microbe install esbuild@^0.25 --dir /tmp/tools
microbe install-manifest package.json --dir /tmp/tools   # its `dependencies` map; other keys are ignored
```

## Install

```sh
cargo add microbe                 # the library
cargo install microbe             # the `microbe` binary
npm install @nubjs/microbe        # the Node-API addon; the platform build is an optional dependency
```

On Linux the library and the binary carry no TLS by default and reach the registry through `node`, `curl`, `wget` or `python3` from the host. A build with `--features tls` carries rustls instead, for about 1.1 MB. The addon always carries TLS. See [How it reaches the network](#how-it-reaches-the-network).

## Who it is for

Microbe does the `npx`-shaped job for a tool that is not a package manager: install a package, or a small map of them, then run what was installed. Typical hosts are a language server launcher, an editor or agent that needs a formatter or a linter on demand, a build tool that pulls a plugin, or a CLI that ships without Node dependencies but needs one at run time.

It is not a replacement for npm, pnpm or bun in a project checkout. There is no lockfile, no store, no `node_modules` reconciliation, no lifecycle scripts, no workspaces. Those are the parts of a package manager that take up the space, and a host that needs them should call a package manager.

## Rust API

The whole surface is one builder, one result type, one error type and one trait.

```rust
use microbe::{Microbe, Transport, DEFAULT_REGISTRY, TIMEOUT};

let m = Microbe::new()?                                    // in-binary TLS, or the first HTTPS client on the host
    .registry("https://registry.example.com")              // default is DEFAULT_REGISTRY, registry.npmjs.org
    .scoped_registry("@acme", "https://npm.acme.dev/")     // what an `@acme:registry` key does
    .auth("https://npm.acme.dev/", "Bearer tok")           // what a `//npm.acme.dev/:_authToken` key does
    .npmrc(Path::new("/etc/tool/.npmrc"))?                 // the same, read from an explicit path; nothing is discovered
    .npmrc_contents("registry=https://r.example.com\n")?   // or from contents the embedder already holds
    .concurrency(8);                                       // parallel fetches; default 16

// One package by spec: `name`, `name@tag`, `name@1.2.3`, `name@^1`, `@scope/name@^1`.
let one = m.install("eslint@^9", dir)?;
// A dependency map, the shape of package.json#/dependencies. Duplicate names take the first range.
let many = m.install_all([("eslint", "^9"), ("prettier", "3")], dir)?;

many.roots;                    // Vec<Root { name, version, dir }>, in request order
many.bins;                     // BTreeMap<command, absolute script path>; also linked in node_modules/.bin
many.packages;                 // tarballs extracted by this call; a package already present is not counted
many.skipped_install_scripts;  // "name@version" of every package whose install script was not run
serde_json::to_string(&many)?; // Installation and Root serialize, camel-cased: the shape `--json` prints

// An embedder that already links an HTTP client supplies it and pays for no TLS.
struct MyClient;
impl Transport for MyClient {
    // `headers` always carries an `accept`, and an `authorization` when one is configured.
    // A non-2xx status is an error; the installer retries on Error::Transport and 429 / 5xx.
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>, microbe::Error> { todo!() }
}
let m = Microbe::with_transport(MyClient);
TIMEOUT;                       // 300 s, the bound every built-in transport puts on one request
```

A second install into the same directory fetches only what is missing or at the wrong version. A package about to be installed at another version is removed first, never overlaid.

## Errors

Every failure is one `microbe::Error` variant, and each one displays as a sentence an embedder can show as is.

```rust
use microbe::Error;

match Microbe::new()?.install("eslint@^9", dir) {
    Ok(done) => {}
    Err(Error::NoTransport(tried)) => {}            // nothing on the host speaks HTTPS and the build has no TLS
    Err(Error::Transport(detail)) => {}             // a request failed after two retries
    Err(Error::Status { url, status }) => {}        // a non-2xx answer, after retries for 429 and 5xx
    Err(Error::NoVersion { name, spec }) => {}      // no published version satisfies the range or tag
    Err(Error::Integrity { name, version }) => {}   // the tarball does not match dist.integrity / dist.shasum
    Err(Error::UnsafePath(entry)) => {}             // a tarball entry would escape its package directory
    Err(Error::Registry { name, detail }) => {}     // the packument could not be parsed
    Err(Error::Npmrc(detail)) => {}                 // a consumed .npmrc key cannot be used as written, such as `${VAR}`
    Err(Error::Io(e)) => {}                         // a filesystem operation failed
    Err(_) => {}                                    // the enum is #[non_exhaustive]
}
```

An optional dependency never produces an error. When anything under one fails to resolve, download or verify, that branch is dropped and the install succeeds.

## Command line

The binary is the library from a shell. It exists to measure the crate and to try it; it is not a package manager for a project checkout.

```
usage: microbe install <name[@spec]>... --dir <path> [options]
       microbe install-manifest <file|-> --dir <path> [options]

options: --registry <url>   registry for unscoped packages; default https://registry.npmjs.org
         --npmrc <file>     apply this .npmrc; nothing is discovered
         --json             print the installation as JSON
```

The `install` verb takes one or more specs. The `install-manifest` verb takes the `dependencies` map of a JSON file, or of stdin for `-`; a whole `package.json` is valid input, and every other key in it is ignored. Both install into `<dir>/node_modules`, and both print one line per requested package, the count of tarballs extracted, and every bin linked:

```
$ microbe install typescript@5 --dir /tmp/tools
typescript@5.9.3 -> /tmp/tools/node_modules/typescript
1 package
  bin tsc -> /tmp/tools/node_modules/typescript/bin/tsc
  bin tsserver -> /tmp/tools/node_modules/typescript/bin/tsserver
```

With `--json` the same installation is printed as one object, the serialized `Installation`:

```json
{
  "roots": [{ "name": "typescript", "version": "5.9.3", "dir": "/tmp/tools/node_modules/typescript" }],
  "bins": {
    "tsc": "/tmp/tools/node_modules/typescript/bin/tsc",
    "tsserver": "/tmp/tools/node_modules/typescript/bin/tsserver"
  },
  "packages": 1,
  "skippedInstallScripts": []
}
```

| Exit | Meaning |
| --- | --- |
| 0 | Installed. |
| 1 | The install failed; stderr carries `microbe: <error>`. |
| 2 | Usage error, before any network: no verb, no `--dir`, no spec, a wrong positional count, or a flag the binary does not have. |

## Node API

The same installer is on npm as [`@nubjs/microbe`](napi/README.md), a Node-API addon with the platform builds as optional dependencies. It needs Node 18.19 or later.

```js
import { install, installSync } from "@nubjs/microbe";

const done = await install({ eslint: "^9", prettier: "3" }, "/tmp/tools", {
  registry: "https://registry.example.com",                 // default registry.npmjs.org
  scopedRegistries: { "@acme": "https://npm.acme.dev/" },  // what an `@acme:registry` key does
  auth: { "https://npm.acme.dev/": "Bearer tok" },         // what a `//npm.acme.dev/:_authToken` key does
  npmrc: "/etc/tool/.npmrc",                                // the same, from an explicit path; nothing is discovered
  npmrcContents: "registry=https://r.example.com\n",        // or from contents the host already holds
  concurrency: 8,                                           // parallel fetches; default 16
});
await install(["eslint@^9", "prettier"], dir);              // a spec list works too, and so does installSync

done.roots;                  // [{ name, version, dir }]
done.bins;                   // { command: absolute script path }, also linked in node_modules/.bin
done.packages;               // tarballs extracted by this call
done.skippedInstallScripts;  // "name@version" of every package whose install script was not run
```

## Everything is explicit

Microbe is an embedder-facing tool, not a human CLI, so it never guesses. The target directory is always given. Nothing is read from environment variables. No configuration file is discovered by walking up the filesystem: a registry is passed as a URL, and an `.npmrc` is passed as an explicit path. What the embedder does not pass, Microbe does not know about. The one thing detected at run time is which HTTPS client the host has, and an embedder that supplies a `Transport` opts out of that too.

## Registry and credentials

A registry for a scope and a credential for a URL prefix are set directly with `Microbe::scoped_registry` and `Microbe::auth`. An `.npmrc` is a convenience over those two: it is applied only from an explicit path (`Microbe::npmrc`, or `--npmrc <file>`), or from contents the embedder already holds (`Microbe::npmrc_contents`), and four keys are read:

```ini
registry=https://registry.example.com
@acme:registry=https://npm.acme.dev/
//npm.acme.dev/:_authToken=...          # Bearer
//legacy.example.com/:_auth=...         # Basic, as npm stores it
//other.example.com/:username=...       # Basic, with the base64 _password below
//other.example.com/:_password=...
```

Credentials are keyed by URL prefix exactly as npm keys them, and the longest matching prefix wins. A `${VAR}` reference is not expanded, because nothing is read from the environment; a consumed key that still holds one is an error, so a placeholder is never sent as a token.

## How it reaches the network

The in-binary client is used on macOS and Windows, and on Linux when built with `--features tls`. A Linux build without it tries, in order:

1. **`node`** — one long-lived child running `fetch`, with requests multiplexed over its stdio so the parallel install actually runs in parallel and undici reuses connections. This is the anchor, because whatever gets installed is about to be run by Node anyway.
2. **`curl`** — most full Linux distributions.
3. **`wget`** — busybox, so Alpine.
4. **`python3`** — the Python container images, which carry neither `curl` nor `wget` but do carry Python with its `ssl` module and a CA bundle.

**The default Linux build requires one of those four programs on `PATH`, or a `Transport` supplied by the embedder.** A survey of 16 popular container base images found `curl` on 4 of them, and 8 carried neither `curl` nor `wget`; every Node image carries Node. The Debian and Ubuntu slim images carry none of the four and no CA bundle either, so on those the answer is `--features tls` or an embedder-supplied `Transport`.

Every request is bounded by a 300 second timeout, npm's default, and a transport failure or a 429 or 5xx response is retried twice. Every host client is told to refuse a redirect off HTTPS: a tarball is protected by its integrity hash, but a packument is not.

## What it implements

Packages land flat under `<dir>/node_modules`, placed the way Node's resolver expects, and the same request always produces the same tree.

- **Resolution** is first-wins over `dependencies` plus platform-matching `optionalDependencies`, one abbreviated packument fetch per package name. A name listed under `optionalDependencies` is optional even when it also appears under `dependencies`, because `npm publish` mirrors it there.
- **Placement** is flat; a version conflict nests the loser under its dependent, which is what Node's resolver walks up to find.
- **Optionality covers the whole branch.** When anything an optional dependency itself requires cannot be resolved or fails its integrity check, that optional dependency is dropped along with every package only it needed, and the install succeeds. A required path reaching the same package fails the install.
- **Bundled dependencies** ship inside their parent's tarball and are never fetched.
- **Integrity** is checked against `dist.integrity`, falling back to the pre-SRI `dist.shasum`. A tarball entry whose path would escape its package directory is refused.
- **Bins** of every requested package are linked under `node_modules/.bin`: a relative symlink on Unix, a `.cmd` shim that runs the script with `node` on Windows. A requested package wins a name clash with a dependency.
- **Peer dependencies** are ignored.
- **Install scripts** are not run. Packages that declare one are named in `Installation::skipped_install_scripts` so the caller can decide what that means; for a prebuilt-binary package like esbuild or biome the postinstall is a no-op, because the platform package carrying the binary is an optional dependency that Microbe already installed.

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

## Status

Beta. The API is small and settled enough to build on. Until 1.0 a minor release may add a field to `Installation`, a variant to `Error`, or a method to `Microbe`; every such type is `#[non_exhaustive]`, so that is not a breaking change for a caller. A change that breaks a caller bumps the minor version and is listed in [`CHANGELOG.md`](CHANGELOG.md). The crate, the binary and the addon are released together, at one version; [`RELEASING.md`](RELEASING.md) has the procedure.

## Tests

The test suite runs against an in-memory registry serving real gzipped tarballs with real integrity strings, so the whole install path runs with no network. CI runs it on Linux, macOS and Windows, with clippy, rustfmt, rustdoc and the 1 MB size check. The `Sweep` workflow, run on demand, builds the release binary on Linux, macOS and Windows and installs real packages from the registry with it, and the `napi` workflow builds the addon for its eight platforms and installs a real package through each native one.
