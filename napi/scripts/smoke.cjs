// Loads the addon beside index.js and installs real packages with it, sync and async.
// Run on a runner that can load the build, natively or inside a container.
"use strict";
const path = require("node:path");
const { install, installSync } = require("../index.js");

const dir = path.join(require("node:os").tmpdir(), "napi-smoke");
const done = installSync(["is-odd@^3"], dir);
if (done.roots[0].name !== "is-odd") throw new Error(JSON.stringify(done));
install({ typescript: "5" }, dir).then((r) => {
  if (!r.bins.tsc) throw new Error(JSON.stringify(r));
  console.log(r.roots.map((x) => x.name + "@" + x.version).join(" "), Object.keys(r.bins));
});
