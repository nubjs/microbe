# @nubjs/microbe

The smallest embeddable npm package installer, as a Node-API addon. It installs one package, or a `dependencies`-shaped map, and its dependency tree into a directory, verifies integrity, links the bins, and reports where everything landed. No lockfile, no store, no lifecycle scripts, no `node_modules` reconciliation.

```js
import { install } from "@nubjs/microbe";

const done = await install(["esbuild@^0.25"], "/tmp/tools");
done.bins.esbuild; // /tmp/tools/node_modules/esbuild/bin/esbuild, also linked in node_modules/.bin
```

```js
// A dependency map, the shape of package.json#/dependencies.
const done = await install({ eslint: "^9", prettier: "3" }, dir, {
  registry: "https://registry.example.com", // default registry.npmjs.org
  npmrc: "/etc/tool/.npmrc",                // explicit path only; nothing is discovered
  concurrency: 8,                           // parallel fetches; default 16
});

done.roots;                  // [{ name, version, dir }], in request order for a list
done.packages;               // tarballs extracted by this call; a package already present is not counted
done.skippedInstallScripts;  // "name@version" of every package whose install script was not run
```

A second install into the same directory fetches only what is missing or at the wrong version. Nothing is read from environment variables and no file is discovered by walking the filesystem; an `.npmrc` is applied only from the path given, and a `${VAR}` in it is an error rather than a token sent as written. The synchronous form is `installSync`, with the same arguments.

The addon reaches the registry itself: the platform TLS on macOS and Windows, rustls on Linux. Every request times out after 300 seconds and a transport failure or a 429 or 5xx response is retried twice. The full behaviour, the placement rules and the optional-dependency semantics are documented in the [microbe crate](https://github.com/nubjs/microbe).

Platform builds ship as optional dependencies, one per `<os>-<arch>[-musl]`, selected by npm at install time. Node 18.19 or later.
