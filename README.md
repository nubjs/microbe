# microbe

The smallest embeddable npm package installer. One crate, pure Rust, no async runtime: it fetches a package and its dependency tree from the registry into a directory the caller names, verifies integrity, and reports where the bin entries landed.

It is built for a tool that needs to install something and then run it — the `npx`-shaped job. It does not reconcile with an existing `node_modules`, keep a store, write a lockfile, or run lifecycle scripts. Those are the parts of a package manager that take up the space.

```rust
let installed = microbe::Microbe::new()?.install("esbuild@^0.25", Path::new("/tmp/tools"))?;
// installed.bins["esbuild"] -> /tmp/tools/node_modules/esbuild/bin/esbuild
```

```
microbe install <name[@spec]>... --dir <path> [--registry <url>]
microbe install --from package.json --dir /tmp/tools    # its `dependencies` map; other keys are ignored
```

## Everything is explicit

microbe is an embedder-facing tool, not a human CLI, so it never guesses. The target directory is always given. Nothing is read from environment variables. No configuration file is discovered by walking up the filesystem: a registry is passed as a URL, and an `.npmrc` is passed as an explicit path when that support lands. What the embedder does not pass, microbe does not know about. The one thing detected at run time is which HTTPS client the host has, and an embedder that supplies a `Transport` opts out of that too.

## Size

Stripped, `opt-level = "z"` with fat LTO, measured by CI on 2026-09-19 with the stable toolchain:

| Target | default build | TLS in it |
| --- | --- | --- |
| aarch64-apple-darwin | 853 KB | Security.framework, always |
| x86_64-pc-windows-msvc | 784 KB | SChannel, always |
| x86_64-unknown-linux-gnu | 682 KB | none; `--features tls` adds rustls for about 1.1 MB |

The budget is decided by TLS and nothing else. The resolve, verify and extract core is about 60 KB of code. On macOS and Windows the operating system's TLS is reachable through Rust bindings for about 300 KB, with no C compiled and no process spawned, so it is always in. On Linux there is no system TLS to bind to, a rustls stack costs about 1.1 MB, and so the Linux build links none by default and borrows an HTTPS client the host already has. The CI size job fails if any default build reaches 1 MB.

## Speed

Two phases. The plan phase walks the dependency graph breadth-first, fetching each level's packuments in parallel and deciding every package's directory before anything is downloaded. The materialize phase then downloads, verifies and extracts every planned tarball in parallel, sixteen at a time by default (`Microbe::concurrency`). Cold installs into an empty directory, measured back to back on one machine in one minute, so the numbers are comparable to each other and to nothing else:

| Package | microbe | npm 11 | pnpm 10 |
| --- | --- | --- | --- |
| express (71 packages) | 2.2 s | 2.2 s | 1.7 s |
| eslint (77 packages) | 5.7 s | 8.5 s | 8.6 s |
| vite (16 packages, native binaries) | 27.8 s | 51.5 s | 23.0 s |

## How it reaches the network

`Transport` is a one-method trait. An embedder that already links an HTTP client implements it and pays nothing:

```rust
Microbe::with_transport(MyClient::new())
```

Otherwise `Microbe::new()` uses the in-binary client on macOS and Windows, or on Linux when built with `--features tls`. A Linux build without it tries, in order:

1. **`node`** — one long-lived child running `fetch`, with requests multiplexed over its stdio so the parallel install actually runs in parallel and undici reuses connections. This is the anchor, because whatever gets installed is about to be run by Node anyway.
2. **`curl`** — most full Linux distributions.
3. **`wget`** — busybox, so Alpine.
4. **`python3`** — the Python container images, which carry neither `curl` nor `wget` but do carry Python with its `ssl` module and a CA bundle.

**The default Linux build requires one of those four programs on `PATH`, or a `Transport` supplied by the embedder.** Node comes first because it is the only one the use case guarantees. A survey of 16 popular container base images found `curl` on 4 of them, and 8 carried neither `curl` nor `wget`; every Node image carries Node. The Debian and Ubuntu slim images carry none of the four and no CA bundle either, so on those the answer is `--features tls` or an embedder-supplied `Transport`. Every host client is told to refuse a redirect off HTTPS: a tarball is protected by its integrity hash, but a packument is not.

## What it implements

Packages land flat under `<dir>/node_modules`. A version conflict nests the loser under its dependent, which is what Node's resolver walks up to find, and placement is deterministic: the same request always produces the same tree. Resolution is first-wins over `dependencies` plus platform-matching `optionalDependencies`, one abbreviated packument fetch per package name. A name listed under `optionalDependencies` is optional even when it also appears under `dependencies`, because `npm publish` mirrors it there. Optionality covers the whole branch: when anything an optional dependency itself requires cannot be resolved or fails its integrity check, that optional dependency is dropped along with every package only it needed, and the install succeeds, unless a required path reaches the same package, in which case the install fails. A name listed under `bundleDependencies` ships inside its parent's tarball and is never fetched. Tarballs are checked against `dist.integrity`, falling back to the pre-SRI `dist.shasum`. A tarball entry whose path would escape its package directory is refused.

`peerDependencies` are ignored and install scripts are not run. Packages that declare one are named in `Installed::skipped_install_scripts` so the caller can decide what that means — for a prebuilt-binary package like esbuild or biome the postinstall is a no-op, because the platform package carrying the binary is an optional dependency that microbe already installed.

## Tests

`cargo test` runs against an in-memory registry serving real gzipped tarballs with real integrity strings, so the whole install path runs with no network.
