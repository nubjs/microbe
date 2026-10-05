# Changelog

## 0.1.0-beta.1 — 2026-10-05

First published version.

- **Install** one package by spec, or a `dependencies`-shaped map, into `<dir>/node_modules`: flat placement, version conflicts nested under the dependent, deterministic output.
- **Optional dependencies** are dropped as a branch when anything under them fails; a required package that fails ends the install with that package's error.
- **Integrity** is checked against `dist.integrity`, falling back to `dist.shasum`; a tarball entry that would escape its package directory is refused.
- **Bins** of every top-level package are linked under `node_modules/.bin`; requested packages win a name clash.
- **Registry routing and credentials** set directly with `scoped_registry` and `auth`, and **the `.npmrc` subset** an installer needs on top of them, applied from an explicit path only: `registry`, `@scope:registry`, and `_authToken`, `_auth`, `username` with `_password` keyed by URL prefix. `${VAR}` is an error, not expanded.
- **Transport**: platform TLS on macOS and Windows through the OS root store; on Linux the first of `node`, `curl`, `wget`, `python3` on the host, or rustls with `--features tls`; every host client refuses a redirect off HTTPS. Requests time out after 300 s and are retried twice on a transport failure or a 429 / 5xx.
- **CLI** `microbe install <name[@spec]>... --dir <path>` and `microbe install-manifest <file|-> --dir <path>`, both with `--registry <url>`, `--npmrc <file>` and `--json`, which prints the installation as one camel-cased object. Everything explicit: no environment variables, no filesystem walking. Exit 2 is a usage error, raised before any network.
- **Node-API addon** `@nubjs/microbe` in `napi/`: `install` and `installSync` taking a spec list or a `dependencies` object, with `registry`, `scopedRegistries`, `auth`, `npmrc`, `npmrcContents` and `concurrency` options, and platform packages for eight targets built by the `napi` workflow.
- **Public Rust surface**: `Microbe`, `Installation`, `Root`, `Error`, `Transport`, `TIMEOUT` and `DEFAULT_REGISTRY`. `Installation` and `Root` implement `serde::Serialize`. Transport detection is internal to `Microbe::new`.
- **Size**: 702 KB on Linux, 800 KB on Windows, 853 KB on macOS, stripped; CI fails a default build at 1 MB.
