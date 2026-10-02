"use strict";
// Best effort: prefetch the binary at install time. Never fails the install; the shim retries on first run.
require("../lib/install.js")
  .install({ quiet: false })
  .catch((e) => process.stderr.write(`oomtop: postinstall skipped (${e.message}); will retry on first run\n`));
