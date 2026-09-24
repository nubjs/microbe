// Loads the platform addon: the `@nubjs/microbe-<platform>` optional dependency npm
// selected for this machine, or a locally built `microbe.<platform>.node` beside this file.
"use strict";

function musl() {
  // Node's own report carries glibcVersionRuntime on glibc and not on musl; no subprocess.
  try {
    const header = process.report?.getReport?.()?.header;
    if (header && "glibcVersionRuntime" in header) return !header.glibcVersionRuntime;
  } catch {}
  try {
    const { execSync } = require("node:child_process");
    return execSync("ldd --version 2>&1", { encoding: "utf8" }).includes("musl");
  } catch (e) {
    return String(e.stdout || "").includes("musl");
  }
}

const platform = `${process.platform}-${process.arch}${process.platform === "linux" && musl() ? "-musl" : ""}`;

let native;
try {
  native = require(`@nubjs/microbe-${platform}`);
} catch (fromPackage) {
  try {
    native = require(`./microbe.${platform}.node`);
  } catch (fromFile) {
    // A build that exists but cannot load is the failure worth reporting; a missing
    // local file only means this is not a checkout.
    const cause = fromFile.code === "MODULE_NOT_FOUND" ? fromPackage : fromFile;
    const err = new Error(`@nubjs/microbe has no build for ${platform}: ${cause.message}`);
    err.cause = cause;
    throw err;
  }
}

module.exports = { install: native.install, installSync: native.installSync };
