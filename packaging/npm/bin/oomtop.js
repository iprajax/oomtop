#!/usr/bin/env node
"use strict";
// npm shim: run the native oomtop binary, fetching it on first use if postinstall was skipped
// (--ignore-scripts, pnpm >= 10 default). Exit code and signals pass through unchanged.
const { spawnSync } = require("child_process");
const { install, existingBinary } = require("../lib/install.js");

async function main() {
  let bin = existingBinary();
  if (!bin) bin = await install();
  const r = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
  if (r.error) throw r.error;
  if (r.signal) process.kill(process.pid, r.signal);
  process.exit(r.status ?? 1);
}

main().catch((e) => {
  process.stderr.write(`oomtop: ${e.message}\n`);
  process.exit(1);
});
