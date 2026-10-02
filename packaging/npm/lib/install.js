"use strict";
// Fetch the oomtop release binary for this platform, verify it against the release's SHA256SUMS, and cache it.
// Zero dependencies: Node >= 18 (global fetch, zlib); the .tar.gz is read in-process (no system tar needed).
//
// Environment (same names as packaging/install.sh where they overlap):
//   OOMTOP_BINARY_PATH  use this binary instead of downloading (e.g. a Homebrew or cargo install)
//   OOMTOP_BASE_URL     mirror base: <base>/v<version>/<archive> and <base>/v<version>/SHA256SUMS (https:// or file://)
//   OOMTOP_FLAVOR       Linux only: gnu | musl (default: gnu when glibc >= 2.28, else musl)
//   OOMTOP_CACHE_DIR    where to cache when the package directory is read-only

const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const path = require("path");
const zlib = require("zlib");

const PKG_DIR = path.join(__dirname, "..");
const VERSION = require(path.join(PKG_DIR, "package.json")).version;
const REPO = "iprajax/oomtop";

function glibcAtLeast(major, minor) {
  try {
    const v = process.report.getReport().header.glibcVersionRuntime; // undefined on musl
    if (!v) return false;
    const [a, b] = v.split(".").map(Number);
    return a > major || (a === major && b >= minor);
  } catch {
    return false;
  }
}

/** Release target triple for this machine, matching .github/workflows/release.yml. */
function target() {
  const arch = { x64: "x86_64", arm64: "aarch64" }[process.arch];
  if (process.platform === "darwin" && arch) return "universal-apple-darwin";
  if (process.platform === "linux" && arch) {
    let flavor = process.env.OOMTOP_FLAVOR;
    if (flavor !== "gnu" && flavor !== "musl") flavor = glibcAtLeast(2, 28) ? "gnu" : "musl";
    return `${arch}-unknown-linux-${flavor}`;
  }
  throw new Error(
    `oomtop has no prebuilt binary for ${process.platform}/${process.arch} ` +
      `(Linux and macOS on x64/arm64 only). Build from source: cargo install oomtop-cli --locked`
  );
}

function candidateDirs(t) {
  const cacheRoot =
    process.env.OOMTOP_CACHE_DIR ||
    (process.platform === "darwin"
      ? path.join(os.homedir(), "Library", "Caches", "oomtop")
      : path.join(process.env.XDG_CACHE_HOME || path.join(os.homedir(), ".cache"), "oomtop"));
  return [path.join(PKG_DIR, "vendor", t), path.join(cacheRoot, "npm", VERSION, t)];
}

/** Path of an already-installed binary, or null. */
function existingBinary() {
  if (process.env.OOMTOP_BINARY_PATH) return process.env.OOMTOP_BINARY_PATH;
  const t = target();
  for (const d of candidateDirs(t)) {
    const p = path.join(d, "oomtop");
    if (fs.existsSync(p)) return p;
  }
  return null;
}

async function get(url) {
  if (url.startsWith("file://")) return fs.readFileSync(new URL(url));
  if (!url.startsWith("https://")) throw new Error(`refusing non-https URL: ${url}`);
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) throw new Error(`GET ${url}: HTTP ${res.status}`);
  return Buffer.from(await res.arrayBuffer());
}

/** Minimal ustar/pax reader: returns the bytes of the entry whose path is `want`. */
function extractEntry(tgz, want) {
  const tar = zlib.gunzipSync(tgz);
  let off = 0;
  let paxPath = null;
  let longName = null;
  while (off + 512 <= tar.length) {
    const h = tar.subarray(off, off + 512);
    if (h.every((b) => b === 0)) break;
    const str = (s, n) => h.subarray(s, s + n).toString("utf8").replace(/\0.*$/s, "");
    const size = parseInt(str(124, 12).trim() || "0", 8);
    const type = String.fromCharCode(h[156] || 48); // NUL means a regular file
    const prefix = str(345, 155);
    let name = prefix ? `${prefix}/${str(0, 100)}` : str(0, 100);
    const body = tar.subarray(off + 512, off + 512 + size);
    off += 512 + Math.ceil(size / 512) * 512;
    if (type === "x") {
      // pax extended header: "<len> path=<value>\n" records
      const m = /(?:^|\n)\d+ path=([^\n]*)\n/.exec(body.toString("utf8"));
      paxPath = m ? m[1] : null;
      continue;
    }
    if (type === "L") {
      longName = body.toString("utf8").replace(/\0.*$/s, "");
      continue;
    }
    if (type === "g") continue;
    name = paxPath || longName || name;
    paxPath = longName = null;
    if ((type === "0" || type === "\0") && name.replace(/^\.\//, "") === want) return Buffer.from(body);
  }
  throw new Error(`${want} not found in archive`);
}

/** Download, verify, and install the binary. Returns its path. */
async function install({ quiet = false } = {}) {
  const have = existingBinary();
  if (have) return have;
  const t = target();
  const archive = `oomtop-${VERSION}-${t}.tar.gz`;
  const base = (process.env.OOMTOP_BASE_URL || `https://github.com/${REPO}/releases/download`).replace(/\/+$/, "");
  const log = (m) => quiet || process.stderr.write(`oomtop: ${m}\n`);

  log(`downloading ${archive}`);
  const [tgz, sums] = await Promise.all([get(`${base}/v${VERSION}/${archive}`), get(`${base}/v${VERSION}/SHA256SUMS`)]);
  const line = sums
    .toString("utf8")
    .split("\n")
    .find((l) => l.trim().split(/\s+\*?/)[1] === archive);
  if (!line) throw new Error(`${archive} is not listed in SHA256SUMS; refusing to install unverified`);
  const expected = line.trim().split(/\s+/)[0].toLowerCase();
  const actual = crypto.createHash("sha256").update(tgz).digest("hex");
  if (expected !== actual) throw new Error(`checksum mismatch for ${archive}: expected ${expected}, got ${actual}`);
  log(`sha256 ok (${actual.slice(0, 12)}…)`);

  const bin = extractEntry(tgz, `oomtop-${VERSION}-${t}/oomtop`);
  let lastErr;
  for (const dir of candidateDirs(t)) {
    try {
      fs.mkdirSync(dir, { recursive: true });
      const tmp = path.join(dir, `.oomtop.${process.pid}.tmp`);
      fs.writeFileSync(tmp, bin, { mode: 0o755 });
      const dest = path.join(dir, "oomtop");
      fs.renameSync(tmp, dest); // atomic: concurrent first runs never see a half-written binary
      log(`installed ${dest}`);
      return dest;
    } catch (e) {
      lastErr = e;
    }
  }
  throw new Error(`could not write the binary anywhere: ${lastErr && lastErr.message}`);
}

module.exports = { install, existingBinary, target, extractEntry, VERSION };
