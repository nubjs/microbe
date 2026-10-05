# @nubjs/microbe

The smallest embeddable npm package installer, as a Node-API addon. It installs one package, or a `dependencies`-shaped map, and its dependency tree into a directory, verifies integrity, links the bins, and reports where everything landed. No lockfile, no store, no lifecycle scripts, no `node_modules` reconciliation.

```js
import { install } from "@nubjs/microbe";

const done = await install(["esbuild@^0.25"], "/tmp/tools");
done.bins.esbuild; // /tmp/tools/node_modules/esbuild/bin/esbuild, also linked in node_modules/.bin
```

## Install

```sh
npm install @nubjs/microbe
```

The platform build is an optional dependency, one package per `<os>-<arch>[-musl]`, selected by npm at install time: darwin-arm64, darwin-x64, linux-x64, linux-x64-musl, linux-arm64, linux-arm64-musl, win32-x64 and win32-arm64. Node 18.19 or later.

## API

Two functions with the same arguments: `install` runs off the main thread and resolves to the installation, `installSync` blocks and returns it.

```js
import { install, installSync } from "@nubjs/microbe";

// A dependency map, the shape of package.json#/dependencies, or a list of specs.
const done = await install({ eslint: "^9", prettier: "3" }, "/tmp/tools", {
  registry: "https://registry.example.com",                 // default registry.npmjs.org
  scopedRegistries: { "@acme": "https://npm.acme.dev/" },  // what an `@acme:registry` key does
  auth: { "https://npm.acme.dev/": "Bearer tok" },         // what a `//npm.acme.dev/:_authToken` key does
  npmrc: "/etc/tool/.npmrc",                                // the same, from an explicit path; nothing is discovered
  npmrcContents: "registry=https://r.example.com\n",        // or from contents the host already holds
  concurrency: 8,                                           // parallel fetches; default 16
});
const same = installSync(["eslint@^9", "prettier"], "/tmp/tools");

done.roots;                  // [{ name, version, dir }], in request order for a list and name order for a map
done.bins;                   // { command: absolute script path }, also linked in node_modules/.bin
done.packages;               // tarballs extracted by this call; a package already present is not counted
done.skippedInstallScripts;  // "name@version" of every package whose install script was not run
```

A failure rejects, or throws from `installSync`, with the crate's error message: no version matches the range, an integrity mismatch, an HTTP status after retries, an `.npmrc` key that cannot be used as written. A second install into the same directory fetches only what is missing or at the wrong version.

## Everything is explicit

Nothing is read from environment variables and no file is discovered by walking the filesystem. An `.npmrc` is applied only from the path given, and a `${VAR}` in it is an error rather than a token sent as written. Credentials are keyed by URL prefix as npm keys them, and the longest matching prefix wins.

## Network

The addon reaches the registry itself: the platform TLS on macOS and Windows, rustls on Linux. Every request times out after 300 seconds, and a transport failure or a 429 or 5xx response is retried twice. The placement rules, the optional-dependency semantics and everything else the addon does are those of the [microbe crate](https://github.com/nubjs/microbe), which is the reference.
