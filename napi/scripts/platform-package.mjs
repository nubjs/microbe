// Writes one `@nubjs/microbe-<platform>` package around a built addon:
//   node scripts/platform-package.mjs <platform> <path-to-built-cdylib> <out-dir>
// The platform is `<process.platform>-<process.arch>[-musl]`, the version is read from
// package.json, and the addon is renamed to `microbe.<platform>.node`.
import fs from "node:fs";
import path from "node:path";

const [platform, built, out] = process.argv.slice(2);
if (!platform || !built || !out) throw new Error("usage: platform-package.mjs <platform> <cdylib> <out-dir>");
const version = JSON.parse(fs.readFileSync(new URL("../package.json", import.meta.url), "utf8")).version;
const [os, cpu, libc] = platform.split("-");
fs.mkdirSync(out, { recursive: true });
fs.copyFileSync(built, path.join(out, `microbe.${platform}.node`));
fs.copyFileSync(new URL("../../LICENSE", import.meta.url), path.join(out, "LICENSE"));
const pkg = {
  name: `@nubjs/microbe-${platform}`,
  version,
  description: `@nubjs/microbe addon for ${platform}`,
  license: "MIT",
  repository: "https://github.com/nubjs/microbe",
  main: `microbe.${platform}.node`,
  files: [`microbe.${platform}.node`, "LICENSE"],
  os: [os],
  cpu: [cpu],
  ...(libc ? { libc: [libc] } : {}),
};
fs.writeFileSync(path.join(out, "package.json"), JSON.stringify(pkg, null, 2) + "\n");
console.log(`${pkg.name}@${version} -> ${out}`);
