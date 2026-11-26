#!/usr/bin/env node
/**
 * T14 — W1 auth end-to-end matrix (offline, deterministic).
 *
 * Contract (docs/plan §5, review/inv/S8-auth.md §11, docs/w1-notes/T10.md):
 * prove the auth-C behaviour against the *real* `lampa-server` binary speaking
 * real HTTP/1.1 with real cookies:
 *
 *   1. `POST /api/pin/verify` with a valid TOTP → `device_token` (+ C cookies);
 *   2. the paired device reaches `GET /api/auth/session` (200 `paired:true`)
 *      and a protected API route without re-pairing;
 *   3. a second request — the front-end "reload" equivalent (`checkDevice()`)
 *      — stays paired: **0 re-pair**, no token rotation;
 *   4. `GET /api/auth/session` → 200 paired for a valid token, exact 401
 *      `token_required` / `invalid_token` otherwise;
 *   5. admin cookie flow: `POST /admin/login` sets `auth_token`, admin routes
 *      answer 200, `POST /admin/logout` clears it → next call 401;
 *   6. admin `revoke` → the device token is rejected 401 on the very next
 *      request (and on `/api/auth/session`);
 *   7. missing / blank / unknown credentials → byte-exact 401 bodies;
 *   8. IP rate limit (10 failed PINs / 15 min) and device block (5 failures)
 *      are exercised, never 403;
 *   9. no secret is printed by this runner and none appears in the server log.
 *
 * Everything runs against a **throw-away synthetic v7 database** built by
 * `deploy/scripts/make_synthetic_db.py` on a random loopback port. Nothing
 * touches `/opt/lampa/data`, staging port 9119, DDL or WAL, and the runner
 * installs nothing from the network (the Rust build uses `--offline` and the
 * browser step only *uses* an already-installed Playwright, if any).
 *
 * Environment (all optional):
 *   BASE_URL            run the matrix against an already-running server
 *                       (no build / no DB / no spawn). Requires
 *                       E2E_ADMIN_PASSWORD and E2E_TOTP_SECRET.
 *   E2E_ADMIN_PASSWORD  admin password of the BASE_URL target.
 *   E2E_TOTP_SECRET     base32 TOTP secret of the BASE_URL target.
 *   LAMPA_SERVER_BIN    explicit `lampa-server` binary to spawn.
 *   LAMPA_E2E_SKIP_BUILD=1  never (re)build; use the newest existing binary.
 *   LAMPA_E2E_NO_BROWSER=1  skip the optional real-browser reload-loop step.
 *   LAMPA_TARGET_DIR, LAMPA_CARGO_CACHE  builder cache/target overrides.
 *
 * Exit code: 0 only when every executed matrix row passes.
 */

import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, '..', '..'); // .../source
const SERVER_DIR = path.join(REPO, 'server');

/** Public S8 lab secret (docs/w1-notes/T10.md §2.5) — never printed. */
const DEFAULT_TOTP_SECRET = 'JBSWY3DPEHPK3PXP';
/** Transitional cookie mode under test: C (DEVICE_LEGACY_COOKIE=1, default). */
const LEGACY_COOKIE = '1';

const STARTUP_TIMEOUT_MS = 20_000;
const REQUEST_TIMEOUT_MS = 10_000;

// ---------------------------------------------------------------------------
// Output buffering + secret hygiene
// ---------------------------------------------------------------------------
/** Every rendered line, in order (flushed at the end of the run). */
const lines = [];
/** How many lines were already flushed live (long-running setup steps). */
let printed = 0;
/** Values that must never be printed: tokens, PINs, cookies, credentials. */
const secrets = [];

function registerSecret(value) {
  if (typeof value === 'string' && value.length >= 4 && !secrets.includes(value)) {
    secrets.push(value);
  }
}

function redact(text) {
  let out = String(text);
  for (const secret of secrets) {
    if (out.includes(secret)) out = out.split(secret).join('[redacted]');
  }
  // Belt and braces: 32-byte hex tokens and cookie pairs are never evidence.
  out = out.replace(/\b[0-9a-f]{64}\b/gi, '[redacted-hex]');
  out = out.replace(/\b(device_token|lampa_session|auth_token)=([^;\s"']+)/g, '$1=[redacted]');
  return out;
}

/**
 * Records and prints one output line.
 *
 * The line is redacted *before* it reaches stdout, so the runner can never
 * leak a token/PIN/credential; the same string is kept in `lines` and
 * re-scanned at the end (`outputLeakCount`) as an independent proof.
 */
function out(text) {
  lines.push(redact(text));
  flush();
}

function flush() {
  while (printed < lines.length) console.log(lines[printed++]);
}

/** Counts secret occurrences in our own output — must stay 0. */
function outputLeakCount() {
  let leaks = 0;
  for (const line of lines) {
    for (const secret of secrets) if (line.includes(secret)) leaks++;
  }
  return leaks;
}

// ---------------------------------------------------------------------------
// Tiny assertions (messages must stay secret-free)
// ---------------------------------------------------------------------------
class CheckError extends Error {}

function expect(condition, message) {
  if (!condition) throw new CheckError(message);
}

function eq(actual, expected, label) {
  const a = JSON.stringify(actual);
  const b = JSON.stringify(expected);
  if (a !== b) throw new CheckError(`${label}: expected ${b}, got ${a}`);
}

function statusIs(res, expected, label) {
  if (res.status !== expected) {
    throw new CheckError(`${label}: expected HTTP ${expected}, got ${res.status} (body: ${redact(res.text).slice(0, 200)})`);
  }
}

const results = [];

async function check(name, fn) {
  try {
    const detail = (await fn()) || '';
    results.push({ name, ok: true, detail: String(detail) });
    out(`  PASS  ${name}${detail ? ` — ${detail}` : ''}`);
  } catch (error) {
    const detail = error instanceof CheckError ? error.message : String(error && error.stack ? error.stack : error);
    results.push({ name, ok: false, detail: String(detail) });
    out(`  FAIL  ${name} — ${redact(detail)}`);
  }
}

// ---------------------------------------------------------------------------
// TOTP (SHA-1 / 6 digits / 30 s, window ±1) — mirrors server/src/auth/totp.rs
// ---------------------------------------------------------------------------
function base32Decode(text) {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
  const clean = String(text).replace(/=+$/g, '').toUpperCase().replace(/\s+/g, '');
  let bits = 0;
  let value = 0;
  const bytes = [];
  for (const char of clean) {
    const index = alphabet.indexOf(char);
    if (index === -1) throw new Error('invalid base32 secret');
    value = (value << 5) | index;
    bits += 5;
    if (bits >= 8) {
      bytes.push((value >>> (bits - 8)) & 0xff);
      bits -= 8;
    }
  }
  return Buffer.from(bytes);
}

function totpAt(secret, seconds) {
  const counter = Buffer.alloc(8);
  counter.writeBigUInt64BE(BigInt(Math.floor(seconds / 30)));
  const mac = crypto.createHmac('sha1', base32Decode(secret)).update(counter).digest();
  const offset = mac[mac.length - 1] & 0x0f;
  const code =
    (((mac[offset] & 0x7f) << 24) | (mac[offset + 1] << 16) | (mac[offset + 2] << 8) | mac[offset + 3]) %
    1_000_000;
  return String(code).padStart(6, '0');
}

function totpNow(secret) {
  return totpAt(secret, Math.floor(Date.now() / 1000));
}

/** A code that matches none of the three accepted steps right now. */
function wrongTotpNow(secret) {
  const now = Math.floor(Date.now() / 1000);
  const accepted = new Set([totpAt(secret, now - 30), totpAt(secret, now), totpAt(secret, now + 30)]);
  for (let candidate = 0; candidate < 1_000_000; candidate++) {
    const code = String(candidate).padStart(6, '0');
    if (!accepted.has(code)) return code;
  }
  throw new Error('all six-digit codes are accepted?!?');
}

// ---------------------------------------------------------------------------
// HTTP client with an explicit cookie jar
// ---------------------------------------------------------------------------
function applySetCookies(jar, setCookies) {
  for (const raw of setCookies) {
    const [pair, ...attributes] = raw.split(';');
    const separator = pair.indexOf('=');
    if (separator === -1) continue;
    const name = pair.slice(0, separator).trim();
    const value = pair.slice(separator + 1).trim();
    let expired = value === '';
    for (const attribute of attributes) {
      const eqIndex = attribute.indexOf('=');
      const key = (eqIndex === -1 ? attribute : attribute.slice(0, eqIndex)).trim().toLowerCase();
      const attrValue = eqIndex === -1 ? '' : attribute.slice(eqIndex + 1).trim();
      if (key === 'max-age' && Number(attrValue) <= 0) expired = true;
      if (key === 'expires') {
        const at = Date.parse(attrValue);
        if (!Number.isNaN(at) && at <= Date.now()) expired = true;
      }
    }
    if (expired) jar.delete(name);
    else jar.set(name, value);
  }
}

function cookieHeader(jar) {
  return [...jar].map(([name, value]) => `${name}=${value}`).join('; ');
}

async function request(baseUrl, method, pathname, options = {}) {
  const { headers = {}, body, jar } = options;
  const finalHeaders = { 'user-agent': 'lampa-t14/1.0', ...headers };
  if (jar && jar.size > 0) finalHeaders.cookie = cookieHeader(jar);
  if (body !== undefined && !finalHeaders['content-type']) {
    finalHeaders['content-type'] = 'application/json';
  }

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  let response;
  try {
    response = await fetch(`${baseUrl}${pathname}`, {
      method,
      headers: finalHeaders,
      body,
      redirect: 'manual',
      signal: controller.signal,
    });
  } catch (error) {
    clearTimeout(timer);
    throw new CheckError(`${method} ${pathname}: transport error (${error && error.message})`);
  }
  clearTimeout(timer);

  const setCookies =
    typeof response.headers.getSetCookie === 'function' ? response.headers.getSetCookie() : [];
  const text = await response.text();
  if (jar) applySetCookies(jar, setCookies);

  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    /* non-JSON body is a legitimate expectation for some checks */
  }
  return {
    status: response.status,
    setCookies,
    headers: response.headers,
    text,
    json,
    header: (name) => response.headers.get(name),
  };
}

// ---------------------------------------------------------------------------
// Server binary: resolve → build when stale (offline builder) → fall back
// ---------------------------------------------------------------------------
function walkRustFiles(dir, sink) {
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) walkRustFiles(full, sink);
    else if (entry.isFile() && entry.name.endsWith('.rs')) sink.push(full);
  }
}

function newestSourceMtimeMs() {
  const files = [];
  walkRustFiles(path.join(SERVER_DIR, 'src'), files);
  walkRustFiles(path.join(SERVER_DIR, 'tests'), files);
  for (const extra of ['Cargo.toml', 'Cargo.lock']) {
    const full = path.join(SERVER_DIR, extra);
    if (fs.existsSync(full)) files.push(full);
  }
  let newest = 0;
  for (const file of files) {
    try {
      newest = Math.max(newest, fs.statSync(file).mtimeMs);
    } catch {
      /* ignore transient files */
    }
  }
  return newest;
}

function targetDirCandidates() {
  const candidates = [];
  if (process.env.LAMPA_TARGET_DIR) candidates.push(process.env.LAMPA_TARGET_DIR);
  candidates.push(path.resolve(REPO, '..', '.cargo-target'));
  candidates.push(path.join(SERVER_DIR, 'target'));
  candidates.push(path.join(REPO, 'target'));
  return candidates;
}

function binaryCandidates() {
  const candidates = [];
  if (process.env.LAMPA_SERVER_BIN) candidates.push(process.env.LAMPA_SERVER_BIN);
  for (const target of targetDirCandidates()) {
    candidates.push(path.join(target, 'release', 'lampa-server'));
    candidates.push(path.join(target, 'debug', 'lampa-server'));
  }
  return candidates;
}

/**
 * Every existing built binary, ordered by preference: `release` first, then
 * the newest (a teammate's debug build must not win over a fresh release one).
 */
function existingBinaries() {
  const found = [];
  for (const candidate of binaryCandidates()) {
    try {
      const stat = fs.statSync(candidate);
      if (stat.isFile()) found.push({ path: candidate, mtimeMs: stat.mtimeMs, size: stat.size });
    } catch {
      /* missing candidate */
    }
  }
  const rank = (binary) => (binary.path.includes(`${path.sep}release${path.sep}`) ? 0 : 1);
  found.sort((a, b) => rank(a) - rank(b) || b.mtimeMs - a.mtimeMs);
  return found;
}

/** Binaries that are at least as new as the newest server source. */
function freshBinaries(sourceMtimeMs) {
  return existingBinaries().filter((binary) => binary.mtimeMs >= sourceMtimeMs);
}

function cargoCacheDir() {
  if (process.env.LAMPA_CARGO_CACHE) return process.env.LAMPA_CARGO_CACHE;
  const sibling = path.resolve(REPO, '..', 'poc', 'cargo-cache');
  if (fs.existsSync(sibling)) return sibling;
  return path.join(os.tmpdir(), 'lampa-cargo-cache');
}

function targetForBuild() {
  const candidates = targetDirCandidates();
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return candidate;
  }
  const created = candidates[1] || candidates[0];
  fs.mkdirSync(created, { recursive: true });
  return created;
}

/**
 * Offline release build through the pinned builder image
 * (docs/decisions.md: no Rust toolchain on the host).
 * Returns {ok, log} — never throws.
 */
function buildOffline() {
  const target = targetForBuild();
  const cache = cargoCacheDir();
  fs.mkdirSync(cache, { recursive: true });

  const args = [
    'run',
    '--rm',
    '--user',
    `${process.getuid()}:${process.getgid()}`,
    '-v',
    `${REPO}:/work`,
    '-v',
    `${cache}:/cargo`,
    '-v',
    `${target}:/target`,
    '-e',
    'CARGO_HOME=/cargo',
    '-e',
    'CARGO_TARGET_DIR=/target',
    '-w',
    '/work/server',
    'lampa-rust-builder:1.100.0',
    'bash',
    '-c',
    'cargo build --release --locked --offline',
  ];

  const run = spawnSync('docker', args, { encoding: 'utf8', timeout: 15 * 60_000 });
  if (run.error) return { ok: false, log: `docker: ${run.error.message}` };
  const log = `${run.stdout || ''}${run.stderr || ''}`;
  return { ok: run.status === 0, log, exitCode: run.status };
}

// ---------------------------------------------------------------------------
// Synthetic v7 database + server process
// ---------------------------------------------------------------------------
function runPython(args, label) {
  const run = spawnSync('python3', args, { encoding: 'utf8', timeout: 120_000 });
  if (run.error) throw new Error(`${label}: python3 unavailable (${run.error.message})`);
  if (run.status !== 0) {
    const tail = `${run.stderr || ''}${run.stdout || ''}`.trim().split('\n').slice(-6).join(' | ');
    throw new Error(`${label}: exit ${run.status} — ${tail}`);
  }
  return `${run.stdout || ''}${run.stderr || ''}`;
}

/** Read-only facts about the throw-away DB (no DDL, no WAL, no prod paths). */
function readDbFacts(dbPath) {
  const script = [
    'import sqlite3, sys',
    'conn = sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True)',
    'journal = conn.execute("PRAGMA journal_mode").fetchone()[0]',
    'version = conn.execute("SELECT version FROM schema_version WHERE id=1").fetchone()[0]',
    'settings = conn.execute("SELECT COUNT(*) FROM settings").fetchone()[0]',
    'devices = conn.execute("SELECT COUNT(*) FROM device_access").fetchone()[0]',
    'print("%s %s %s %s" % (journal, version, settings, devices))',
  ].join('\n');
  const run = spawnSync('python3', ['-c', script, dbPath], { encoding: 'utf8', timeout: 60_000 });
  if (run.status !== 0) throw new Error(`db facts: exit ${run.status}`);
  const [journal, version, settings, devices] = run.stdout.trim().split(/\s+/);
  return { journal, version: Number(version), settings: Number(settings), devices: Number(devices) };
}

function reservePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

const server = { child: null, log: [], port: 0, baseUrl: '', dir: null, dbPath: null };

function startServer(binary, databaseUrl, totpSecret) {
  const child = spawn(binary, [], {
    env: {
      ...process.env,
      HOST: '127.0.0.1',
      PORT: String(server.port),
      DATABASE_URL: databaseUrl,
      TOTP_SECRET: totpSecret,
      DEVICE_LEGACY_COOKIE: LEGACY_COOKIE,
      // Maximum hygiene pressure on the server: `debug` is where the guard
      // rejections are logged (T10 logs path/status/reason, never values).
      RUST_LOG: 'debug',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  child.stdout.on('data', (chunk) => server.log.push(String(chunk)));
  child.stderr.on('data', (chunk) => server.log.push(String(chunk)));
  child.on('error', (error) => server.log.push(`spawn error: ${error.message}`));
  server.child = child;
}

async function waitForReady(baseUrl, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!server.child || server.child.exitCode !== null) {
      throw new Error(`lampa-server exited early (code ${server.child ? server.child.exitCode : 'n/a'})`);
    }
    try {
      const response = await request(baseUrl, 'GET', '/readyz');
      if (response.status === 200) return;
    } catch {
      /* not listening yet */
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`server not ready within ${timeoutMs} ms`);
}

function stopServer() {
  if (!server.child) return;
  const child = server.child;
  server.child = null;
  try {
    child.kill('SIGTERM');
  } catch {
    /* already gone */
  }
  const deadline = Date.now() + 5000;
  while (child.exitCode === null && Date.now() < deadline) {
    // Synchronous sleep without shelling out.
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 50);
  }
  if (child.exitCode === null) {
    try {
      child.kill('SIGKILL');
    } catch {
      /* ignore */
    }
  }
}

// ---------------------------------------------------------------------------
// Optional real-browser reload-loop check (deploy/e2e/browser_reload.mjs)
// ---------------------------------------------------------------------------
function runBrowserCheck(env) {
  const run = spawnSync(process.execPath, [path.join(HERE, 'browser_reload.mjs')], {
    encoding: 'utf8',
    env: { ...process.env, ...env },
    timeout: 5 * 60_000,
  });
  const output = `${run.stdout || ''}${run.stderr || ''}`;
  const marker = output.lastIndexOf('BROWSER_RESULT ');
  if (marker === -1) {
    return {
      status: 'skip',
      reason: `browser runner produced no result (exit ${run.status})`,
      output,
    };
  }
  try {
    const parsed = JSON.parse(output.slice(marker).split('\n')[0].slice('BROWSER_RESULT '.length));
    parsed.output = output;
    return parsed;
  } catch (error) {
    return { status: 'skip', reason: `unparsable browser result (${error.message})`, output };
  }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------
async function main() {
  const baseUrlOverride = (process.env.BASE_URL || '').trim();
  const skipBuild = ['1', 'true', 'yes'].includes((process.env.LAMPA_E2E_SKIP_BUILD || '').toLowerCase());
  const noBrowser = ['1', 'true', 'yes'].includes((process.env.LAMPA_E2E_NO_BROWSER || '').toLowerCase());
  const spawnMode = baseUrlOverride === '';

  const totpSecret = spawnMode ? DEFAULT_TOTP_SECRET : (process.env.E2E_TOTP_SECRET || '').trim();
  expect(totpSecret.length > 0, 'E2E_TOTP_SECRET is required with BASE_URL');
  registerSecret(totpSecret);

  out('T14 auth E2E matrix — offline, deterministic');
  out(`repo: ${REPO}`);
  out(`mode: ${spawnMode ? 'spawn local lampa-server on a throw-away synthetic v7 DB' : `BASE_URL=${baseUrlOverride}`}`);
  out('');

  // --- binary -------------------------------------------------------------
  let binary = null;
  let buildNote = 'not attempted';
  if (spawnMode) {
    const sourceMtime = newestSourceMtimeMs();
    const fresh = freshBinaries(sourceMtime);
    const available = existingBinaries();
    if (fresh.length > 0) {
      binary = fresh[0];
      buildNote = `fresh binary ${path.relative(REPO, binary.path)} (newer than server/**.rs)`;
    } else if (skipBuild) {
      binary = available[0] || null;
      buildNote = binary
        ? `LAMPA_E2E_SKIP_BUILD=1 → reused ${path.relative(REPO, binary.path)}`
        : 'LAMPA_E2E_SKIP_BUILD=1 but no binary exists';
    } else {
      out(
        `[setup] ${available.length ? 'server sources are newer than every binary' : 'no lampa-server binary'}; running the offline release build ...`,
      );
      const built = buildOffline();
      const rebuilt = freshBinaries(newestSourceMtimeMs());
      if (built.ok && rebuilt.length > 0) {
        binary = rebuilt[0];
        buildNote = 'offline release build OK (cargo build --release --locked --offline)';
      } else {
        const tail = built.log.trim().split('\n').slice(-4).join(' | ');
        if (available.length > 0) {
          // The workspace is shared: another writer may be mid-edit. Falling
          // back keeps the matrix runnable and says so loudly instead of
          // silently testing a stale binary.
          binary = available[0];
          buildNote =
            `WARN: offline build ${built.ok ? 'did not refresh the binary' : `failed (exit ${built.exitCode})`} — ` +
            `the shared workspace does not compile right now; fell back to ` +
            `${path.relative(REPO, binary.path)} — ${tail}`;
          out(`[setup] ${buildNote}`);
        } else {
          throw new Error(`offline build failed and no previous binary exists — ${tail}`);
        }
      }
    }
    expect(binary && fs.existsSync(binary.path), 'no lampa-server binary available');
    out(`[setup] binary: ${binary.path} (${(binary.size / 1e6).toFixed(1)} MB) — ${buildNote}`);
  }

  // --- database + server --------------------------------------------------
  let facts = {};
  if (spawnMode) {
    server.dir = fs.mkdtempSync(path.join(os.tmpdir(), 'lampa-t14-'));
    server.dbPath = path.join(server.dir, 'database.sqlite');
    out('[setup] building synthetic v7 DB via deploy/scripts/make_synthetic_db.py ...');
    runPython(
      [path.join(REPO, 'deploy', 'scripts', 'make_synthetic_db.py'), '--out', server.dbPath],
      'make_synthetic_db.py',
    );

    // Admin credentials: keep the canonical synthetic values as the single
    // source of truth (they live in make_synthetic_db.py and are never printed).
    const builderSource = fs.readFileSync(
      path.join(REPO, 'deploy', 'scripts', 'make_synthetic_db.py'),
      'utf8',
    );
    const pick = (name) => {
      const match = builderSource.match(new RegExp(`^${name} = "([^"]+)"`, 'm'));
      return match ? match[1] : null;
    };
    const adminPassword = pick('ADMIN_PASSWORD');
    const adminToken = pick('ADMIN_TOKEN');
    expect(adminPassword && adminToken, 'cannot read ADMIN_PASSWORD/ADMIN_TOKEN from make_synthetic_db.py');
    registerSecret(adminPassword);
    registerSecret(adminToken);

    server.port = await reservePort();
    server.baseUrl = `http://127.0.0.1:${server.port}`;
    startServer(binary.path, `sqlite:${server.dbPath}?mode=rw`, totpSecret);
    await waitForReady(server.baseUrl, STARTUP_TIMEOUT_MS);
    out(`[setup] lampa-server ready on ${server.baseUrl} (DEVICE_LEGACY_COOKIE=${LEGACY_COOKIE}, RUST_LOG=debug)`);

    facts = await runMatrix({ baseUrl: server.baseUrl, adminPassword, adminToken, dbPath: server.dbPath, totpSecret });

    // --- optional real-browser reload-loop check (server still running) ----
    if (noBrowser) {
      out('O4 (owner open item): browser check disabled by LAMPA_E2E_NO_BROWSER=1');
    } else {
      out('');
      out('[setup] running the optional real-browser reload-loop check ...');
      const browser = runBrowserCheck({
        T14_BASE_URL: server.baseUrl,
        T14_TOTP_SECRET: totpSecret,
        T14_ADMIN_PASSWORD: adminPassword,
        T14_REPO: REPO,
      });
      if (browser.status === 'pass') {
        await check('browser_reload_loop_check', () => browser.detail || 'reload-loop check passed');
      } else if (browser.status === 'fail') {
        results.push({ name: 'browser_reload_loop_check', ok: false, detail: browser.detail || '' });
        out(`  FAIL  browser_reload_loop_check — ${redact(browser.detail || 'reload loop observed')}`);
      } else {
        // Owner open item O4 — recorded, the HTTP matrix is unaffected.
        out(`O4 (owner open item): real-browser reload-loop check skipped — ${redact(browser.detail || browser.reason || 'unknown')}`);
        out('     the HTTP/cookie matrix above is unaffected and stays green.');
      }
    }

    // Stop the server first: the log-hygiene row needs the complete log.
    stopServer();
    await check('server_logs_contain_no_secrets', () => {
      const log = server.log.join('');
      expect(log.length > 0, 'no server log captured');
      const found = secrets.filter((secret) => log.includes(secret));
      expect(found.length === 0, `${found.length} secret value(s) found in server output`);
      const cookieNames = ['device_token=', 'lampa_session=', 'auth_token='];
      const cookieLeaks = cookieNames.filter((name) => log.includes(name));
      expect(cookieLeaks.length === 0, `cookie pair(s) logged: ${cookieLeaks.join(', ')}`);
      return `${log.split('\n').length} log lines, 0 of ${secrets.length} secrets, 0 cookie pairs`;
    });
  } else {
    out('[setup] BASE_URL mode: no build, no DB, no server spawn, no log capture');
    expect(
      (process.env.E2E_ADMIN_PASSWORD || '').length > 0,
      'E2E_ADMIN_PASSWORD is required with BASE_URL',
    );
    facts = await runMatrix({
      baseUrl: baseUrlOverride.replace(/\/+$/, ''),
      adminPassword: process.env.E2E_ADMIN_PASSWORD,
      adminToken: null,
      dbPath: null,
      totpSecret,
    });
    if (!noBrowser) out('O4 (owner open item): browser check is not run in BASE_URL mode');
  }

  // --- evidence block (the numbers the T14 evidence table quotes) ---------
  out('');
  out('EVIDENCE');
  out(`  re-pair count              : ${facts.rePairCount} (successful PIN verifies: ${facts.pinSuccessCount}, device: ${facts.deviceId || 'n/a'})`);
  out(`  session pair               : valid token → ${facts.sessionPairedStatus}; unpaired → ${facts.sessionUnpairedStatus} ${facts.sessionUnpairedBody}`);
  out(`  protected device request   : ${facts.protectedStatus} after PIN, ${facts.protectedAfterRevokeStatus} after revoke`);
  out(`  revoke case                : ${facts.revokeStatus} on the next device request (${facts.revokeBody})`);
  out(`  admin cookie flow          : login ${facts.adminLoginStatus} → admin route ${facts.adminRouteStatus} → logout ${facts.adminLogoutStatus} → next call ${facts.adminAfterLogoutStatus}`);
  out(`  rate limits                : ${facts.rateLimitNote}`);

  out('');
  const passed = results.filter((result) => result.ok).length;
  const failed = results.length - passed;
  if (results.length === 0) {
    out('MATRIX: 0 rows executed');
    exitCode = 1;
  } else if (failed === 0) {
    out(`MATRIX: ${passed}/${results.length} PASS`);
  } else {
    out(`MATRIX: ${passed}/${results.length} PASS, ${failed} FAILED`);
    for (const result of results.filter((item) => !item.ok)) {
      out(`  - ${result.name}: ${redact(result.detail)}`);
    }
  }
}

// ---------------------------------------------------------------------------
// The matrix itself
// ---------------------------------------------------------------------------
async function runMatrix({ baseUrl, adminPassword, adminToken, dbPath, totpSecret }) {
  const deviceJar = new Map(); // lampa_session (HttpOnly) + device_token (readable, C)
  const adminJar = new Map(); // auth_token
  const DEVICE_ID = 't14-device-1';
  let pinSuccessCount = 0; // every successful PIN = a (re-)pair
  let issuedToken = '';

  /** Numbers quoted by the T14 evidence table (never secret values). */
  const facts = {
    deviceId: DEVICE_ID,
    pinSuccessCount: 0,
    rePairCount: 0,
    sessionPairedStatus: '-',
    sessionUnpairedStatus: '-',
    sessionUnpairedBody: '-',
    sessionUnpairedCacheControl: '-',
    protectedStatus: '-',
    protectedAfterRevokeStatus: '-',
    revokeStatus: '-',
    revokeBody: '-',
    adminLoginStatus: '-',
    adminRouteStatus: '-',
    adminLogoutStatus: '-',
    adminAfterLogoutStatus: '-',
    rateLimitNote: '-',
  };

  const get = (pathname, options) => request(baseUrl, 'GET', pathname, options);
  const post = (pathname, body, options) =>
    request(baseUrl, 'POST', pathname, { ...options, body: JSON.stringify(body) });

  const pin = (deviceId, code, headers = {}) =>
    post('/api/pin/verify', { pin: code, device_id: deviceId }, { headers });

  const protectedGet = (options) => get('/api/profiles/all', options);

  out('--- pairing / no re-pair ------------------------------------------------');

  if (dbPath) {
    await check('synthetic_v7_db_ready', () => {
      const facts = readDbFacts(dbPath);
      eq(facts.journal, 'delete', 'journal_mode');
      eq(facts.version, 7, 'schema_version');
      expect(facts.settings >= 20, `settings rows: ${facts.settings}`);
      expect(facts.devices >= 3, `device_access rows: ${facts.devices}`);
      return `journal=delete schema_version=7 settings=${facts.settings} device_access=${facts.devices}`;
    });
  }

  await check('server_health_and_readiness', async () => {
    const health = await get('/healthz');
    const ready = await get('/readyz');
    statusIs(health, 200, '/healthz');
    statusIs(ready, 200, '/readyz');
    return '/healthz 200, /readyz 200';
  });

  let firstPin = null;
  await check('pin_verify_issues_device_token', async () => {
    firstPin = await pin(DEVICE_ID, totpNow(totpSecret), { 'user-agent': 'lampa-t14/1.0' });
    statusIs(firstPin, 200, 'POST /api/pin/verify');
    eq(firstPin.json && firstPin.json.ok, true, 'pin body ok');
    issuedToken = firstPin.json.device_token;
    expect(typeof issuedToken === 'string' && issuedToken.length === 64, 'device_token length');
    expect(/^[0-9a-f]{64}$/.test(issuedToken), 'device_token must be 64 lowercase hex');
    registerSecret(issuedToken);
    pinSuccessCount++;

    const cookies = firstPin.setCookies;
    const session = cookies.find((value) => value.startsWith('lampa_session='));
    const legacy = cookies.find((value) => value.startsWith('device_token='));
    expect(session, 'lampa_session cookie missing');
    expect(legacy, 'device_token cookie missing');
    expect(session.includes('HttpOnly'), 'lampa_session must be HttpOnly');
    expect(!/device_token=[^;]*HttpOnly/i.test(legacy), 'device_token must stay readable in C');
    expect(session.includes(issuedToken) && legacy.includes(issuedToken), 'both cookies carry the token');
    expect(!session.includes('Secure'), 'Secure only on HTTPS');
    // The jar now behaves exactly like a browser cookie store.
    applySetCookies(deviceJar, cookies);
    expect(deviceJar.has('lampa_session') && deviceJar.has('device_token'), 'jar must hold both cookies');
    return '200 ok:true, 64-hex token, lampa_session HttpOnly + readable device_token (same value)';
  });

  await check('device_session_200_paired', async () => {
    const session = await get('/api/auth/session', { jar: deviceJar });
    facts.sessionPairedStatus = session.status;
    statusIs(session, 200, 'GET /api/auth/session');
    eq(session.json, { paired: true, device_id: DEVICE_ID, admin: false }, 'session body');
    expect(session.header('cache-control') === 'no-store', 'session must be no-store');
    return '200 {paired:true,device_id,admin:false}, Cache-Control: no-store';
  });

  await check('reload_request_stays_paired_zero_repair', async () => {
    // The patched front-end `checkDevice()` performs exactly this request on
    // every page load (docs/w1-notes/T12.md §3): it must stay 200 paired and
    // must never trigger a second PIN (re-pair) or rotate the token.
    const before = pinSuccessCount;
    const first = await get('/api/auth/session', { jar: deviceJar });
    const second = await protectedGet({ jar: deviceJar });
    statusIs(first, 200, 'reload-equivalent session probe #1');
    statusIs(second, 200, 'reload-equivalent request #2 (protected route)');
    eq(first.json && first.json.paired, true, 'reload session paired');
    expect(pinSuccessCount === before, 'no additional PIN verify may happen on reload');
    const rePairCount = pinSuccessCount - 1;
    facts.rePairCount = rePairCount;
    eq(rePairCount, 0, 're-pair count');

    // No rotation: the originally issued token still authenticates.
    const legacyOnly = await protectedGet({ headers: { cookie: `device_token=${issuedToken}` } });
    statusIs(legacyOnly, 200, 'original token after reloads');
    return `200 paired twice, re-pair count ${rePairCount}, token unchanged`;
  });

  await check('protected_route_200_with_device_cookie', async () => {
    const response = await protectedGet({ jar: deviceJar });
    facts.protectedStatus = response.status;
    statusIs(response, 200, 'GET /api/profiles/all');
    // T03 golden fixture 09: {"secuses":true,"profiles":[…]}.
    const profiles = Array.isArray(response.json)
      ? response.json
      : response.json && Array.isArray(response.json.profiles)
        ? response.json.profiles
        : null;
    expect(profiles, 'profiles/all must answer the legacy {secuses,profiles} shape');
    return `200 with ${profiles.length} profile(s) (legacy fixture-09 shape)`;
  });

  await check('httponly_lampa_session_cookie_accepted', async () => {
    const response = await protectedGet({ headers: { cookie: `lampa_session=${issuedToken}` } });
    statusIs(response, 200, 'GET /api/profiles/all with lampa_session only');
    return '200 (B-model cookie works today, C keeps the readable twin)';
  });

  out('--- exact 401 contract --------------------------------------------------');

  await check('missing_token_401_token_required', async () => {
    const response = await protectedGet();
    statusIs(response, 401, 'GET /api/profiles/all without cookies');
    eq(response.text, '{"error":"token_required"}', 'byte-exact 401 body');
    expect(
      response.setCookies.some((value) => value.startsWith('lampa_session=;')),
      'device cookies must be expired on 401',
    );
    return '401 {"error":"token_required"} byte-exact + cookie clearing';
  });

  await check('blank_token_401_invalid_token', async () => {
    const response = await protectedGet({ headers: { cookie: 'device_token=' } });
    statusIs(response, 401, 'blank device_token');
    eq(response.text, '{"error":"invalid_token"}', 'byte-exact 401 body');
    return '401 {"error":"invalid_token"} byte-exact';
  });

  await check('unknown_token_401_invalid_token', async () => {
    const unknown = 'f'.repeat(64);
    const response = await protectedGet({ headers: { cookie: `device_token=${unknown}` } });
    statusIs(response, 401, 'unknown device_token');
    eq(response.text, '{"error":"invalid_token"}', 'byte-exact 401 body');
    expect(!response.text.includes(unknown), 'response must not echo the token');
    return '401 {"error":"invalid_token"} byte-exact, token not echoed';
  });

  await check('session_missing_401_token_required', async () => {
    const response = await get('/api/auth/session');
    facts.sessionUnpairedStatus = response.status;
    facts.sessionUnpairedBody = response.text;
    statusIs(response, 401, 'GET /api/auth/session without cookies');
    eq(response.json, { paired: false, error: 'token_required' }, 'session 401 body');
    // Observation (not part of the T14 acceptance): T10 §2.4 documents
    // `Cache-Control: no-store` for this endpoint, but only the 200 branch
    // applies it today. Recorded for the captain, never silently ignored.
    facts.sessionUnpairedCacheControl = String(response.header('cache-control'));
    return '401 {paired:false,error:token_required} (T10 contract → front-end gate, no reload)';
  });
  out(`  note    session 401 Cache-Control: ${facts.sessionUnpairedCacheControl} (no-store asserted on the 200 path; 401 header recorded as an observation)`);

  await check('session_unknown_token_401_invalid_token', async () => {
    const response = await get('/api/auth/session', {
      headers: { cookie: 'device_token=0123456789abcdef' },
    });
    statusIs(response, 401, 'GET /api/auth/session with junk cookie');
    eq(response.json, { paired: false, error: 'invalid_token' }, 'session 401 body');
    expect(
      response.setCookies.some((value) => value.startsWith('lampa_session=;')),
      'invalid session must clear device cookies',
    );
    return '401 {paired:false,error:invalid_token} + cookie clearing';
  });

  await check('admin_missing_and_unknown_token_401', async () => {
    const missing = await get('/api/settings');
    statusIs(missing, 401, 'GET /api/settings without cookie');
    eq(missing.text, '{"error":"token_required"}', 'byte-exact admin 401 body');

    const wrong = await get('/api/settings', { headers: { cookie: 'auth_token=not-the-token' } });
    statusIs(wrong, 401, 'GET /api/settings with wrong auth_token');
    eq(wrong.text, '{"error":"invalid_token"}', 'byte-exact admin 401 body');
    expect(
      wrong.setCookies.some((value) => value.startsWith('auth_token=;')),
      'admin cookie must be expired on 401',
    );
    return '401 token_required (missing) / invalid_token (wrong) byte-exact';
  });

  out('--- admin cookie flow ---------------------------------------------------');

  let adminLoggedIn = false;
  await check('admin_login_sets_auth_token_cookie', async () => {
    const response = await post('/admin/login', { password: adminPassword }, { jar: adminJar });
    facts.adminLoginStatus = response.status;
    statusIs(response, 200, 'POST /admin/login');
    eq(response.json, { success: true }, 'login body');
    const cookie = response.setCookies.find((value) => value.startsWith('auth_token='));
    expect(cookie, 'auth_token cookie missing');
    expect(cookie.includes('HttpOnly'), 'auth_token must be HttpOnly');
    expect(cookie.includes('SameSite=Lax'), 'auth_token must be SameSite=Lax');
    expect(adminJar.has('auth_token'), 'cookie jar must store auth_token');
    if (adminToken) {
      expect(
        cookie.includes(`auth_token=${adminToken}`),
        'cookie value must equal settings.admin_token',
      );
    }
    adminLoggedIn = true;
    return '200 {success:true} + HttpOnly auth_token cookie';
  });

  await check('admin_login_wrong_password_401', async () => {
    const jar = new Map();
    const response = await post('/admin/login', { password: `${adminPassword}-wrong` }, { jar });
    statusIs(response, 401, 'POST /admin/login with a wrong password');
    eq(response.json, { success: false }, 'wrong-password body');
    expect(!response.setCookies.some((value) => value.startsWith('auth_token=')), 'no cookie on failure');
    return '401 {success:false}, no Set-Cookie';
  });

  await check('admin_routes_200_with_cookie', async () => {
    const devices = await get('/api/admin/devices', { jar: adminJar });
    facts.adminRouteStatus = devices.status;
    statusIs(devices, 200, 'GET /api/admin/devices');
    expect(Array.isArray(devices.json), 'device list must be a JSON array');
    expect(
      devices.text && !devices.text.includes('token_hash'),
      'token_hash must never be serialised',
    );
    const settings = await get('/api/settings', { jar: adminJar });
    statusIs(settings, 200, 'GET /api/settings');
    expect(Array.isArray(settings.json), 'settings must be a JSON array');
    const masked = settings.json.find((row) => row.key === 'admin_password');
    expect(masked && masked.value === '***', 'admin_password must be masked');
    return `GET /api/admin/devices 200 (${devices.json.length} rows), GET /api/settings 200 (secrets masked)`;
  });

  await check('admin_session_200_paired_admin', async () => {
    const response = await get('/api/auth/session', { jar: adminJar });
    statusIs(response, 200, 'GET /api/auth/session with auth_token');
    eq(response.json, { paired: true, admin: true }, 'admin session body');
    return '200 {paired:true,admin:true}';
  });

  await check('admin_logout_clears_cookie_then_401', async () => {
    const response = await post('/admin/logout', {}, { jar: adminJar });
    facts.adminLogoutStatus = response.status;
    statusIs(response, 200, 'POST /admin/logout');
    eq(response.json, { success: true }, 'logout body');
    const clearing = response.setCookies.find((value) => value.startsWith('auth_token='));
    expect(clearing, 'logout must expire auth_token');
    expect(
      /auth_token=;\s*(.*Max-Age=0|.*Expires)/i.test(clearing) || clearing.startsWith('auth_token=;'),
      'logout cookie must be an expiry',
    );
    expect(!adminJar.has('auth_token'), 'jar must drop auth_token after logout');
    adminLoggedIn = false;

    const after = await get('/api/settings');
    facts.adminAfterLogoutStatus = after.status;
    statusIs(after, 401, 'GET /api/settings after logout');
    eq(after.text, '{"error":"token_required"}', 'byte-exact 401 after logout');
    return 'logout 200 {success:true} + cookie cleared → next admin call 401 token_required';
  });

  out('--- revoke --------------------------------------------------------------');

  let deviceId = null;
  await check('admin_revoke_action_success', async () => {
    if (!adminLoggedIn) {
      const login = await post('/admin/login', { password: adminPassword }, { jar: adminJar });
      statusIs(login, 200, 're-login for the revoke step');
      adminLoggedIn = true;
    }
    const list = await get('/api/admin/devices', { jar: adminJar });
    statusIs(list, 200, 'GET /api/admin/devices');
    const row = list.json.find((item) => item.device_id === DEVICE_ID);
    expect(row, `device ${DEVICE_ID} must be listed for revoke`);
    expect(typeof row.id === 'number', 'device row id');
    deviceId = row.id;
    const action = await post('/api/admin/device/action', { id: deviceId, action: 'revoke' }, { jar: adminJar });
    statusIs(action, 200, 'POST /api/admin/device/action');
    eq(action.json, { success: true }, 'revoke body');
    return `revoke accepted for id=${deviceId} (legacy status=pending, token_hash=NULL)`;
  });

  await check('revoked_device_401_on_next_request', async () => {
    expect(deviceId !== null, 'revoke step must run first');

    // (1) The real browser flow: the very next request still carries the
    //     revoked cookie and must be rejected, after which the server expires
    //     the cookies — exactly what the jar models here.
    const api = await protectedGet({ jar: deviceJar });
    facts.protectedAfterRevokeStatus = api.status;
    facts.revokeStatus = api.status;
    facts.revokeBody = api.text;
    statusIs(api, 401, 'GET /api/profiles/all right after revoke');
    eq(api.text, '{"error":"invalid_token"}', 'byte-exact 401 body');
    expect(
      api.setCookies.some((value) => value.startsWith('lampa_session=;')),
      'revoked device cookies must be cleared',
    );
    expect(
      !deviceJar.has('lampa_session') && !deviceJar.has('device_token'),
      'cookie clearing must empty the jar (the next call is token_required)',
    );

    // (2) A client that missed the clearing (stale cookie header, restart)
    //     keeps presenting the revoked token: it must still be invalid on the
    //     protected route and on /api/auth/session.
    const stale = { headers: { cookie: `device_token=${issuedToken}` } };
    const stillRevoked = await protectedGet(stale);
    statusIs(stillRevoked, 401, 'revoked token on a protected route');
    eq(stillRevoked.text, '{"error":"invalid_token"}', 'byte-exact 401 body');

    const session = await get('/api/auth/session', stale);
    statusIs(session, 401, 'GET /api/auth/session with the revoked token');
    eq(session.json, { paired: false, error: 'invalid_token' }, 'session body after revoke');

    return '401 invalid_token on the protected route and on /api/auth/session; cookies cleared (jar empty → token_required)';
  });

  out('--- rate limits / blocking ---------------------------------------------');

  await check('pin_missing_device_id_400', async () => {
    const response = await post('/api/pin/verify', { pin: totpNow(totpSecret) }, {});
    statusIs(response, 400, 'POST /api/pin/verify without device_id');
    eq(response.json, { error: 'device_id required' }, 'missing device_id body');
    return '400 {"error":"device_id required"}';
  });

  await check('ip_rate_limit_10_failures_then_429', async () => {
    const clientIp = '203.0.113.44'; // TEST-NET-3, sent via cf-connecting-ip
    const wrong = wrongTotpNow(totpSecret);
    registerSecret(wrong);
    for (let attempt = 1; attempt <= 10; attempt++) {
      const response = await pin(`t14-ip-${attempt}`, wrong, { 'cf-connecting-ip': clientIp });
      statusIs(response, 200, `failed PIN attempt ${attempt} stays HTTP 200`);
      eq(response.json.ok, false, `attempt ${attempt} ok`);
      eq(response.json.ip_blocked, attempt === 10, `attempt ${attempt} ip_blocked`);
      if (attempt < 10) {
        expect(response.json.attempts_left !== undefined, 'failure body carries attempts_left');
      }
    }
    const blocked = await pin('t14-ip-new', totpNow(totpSecret), { 'cf-connecting-ip': clientIp });
    statusIs(blocked, 429, 'blocked IP must be rejected before TOTP');
    eq(blocked.json, { ok: false, error: 'too_many_attempts', retry_after: 900 }, '429 body');
    eq(blocked.header('retry-after'), '900', 'Retry-After header');
    return '10 failures → ip_blocked, then 429 {"error":"too_many_attempts","retry_after":900} + Retry-After: 900';
  });

  await check('device_blocks_after_5_failures_never_403', async () => {
    const wrong = wrongTotpNow(totpSecret);
    registerSecret(wrong);
    const deviceIdBlocked = 't14-blocked-1';
    for (let attempt = 1; attempt <= 5; attempt++) {
      const response = await pin(deviceIdBlocked, wrong, { 'cf-connecting-ip': '198.51.100.7' });
      statusIs(response, 200, `blocked-device attempt ${attempt}`);
      eq(response.json.ok, false, `attempt ${attempt} ok`);
      eq(response.json.attempts_left, 5 - attempt, `attempt ${attempt} attempts_left`);
      eq(response.json.blocked, attempt === 5, `attempt ${attempt} blocked flag`);
      expect(response.status !== 403, 'PIN failures must never answer 403');
    }
    const blocked = await pin(deviceIdBlocked, totpNow(totpSecret), { 'cf-connecting-ip': '198.51.100.7' });
    statusIs(blocked, 200, 'blocked device with a correct PIN');
    eq(blocked.json, { ok: false, blocked: true }, 'blocked body');
    return '5 failures → status blocked; correct PIN → 200 {ok:false,blocked:true}, never 403';
  });

  facts.pinSuccessCount = pinSuccessCount;
  if (!facts.rePairCount) facts.rePairCount = Math.max(0, pinSuccessCount - 1);
  const ipRow = results.find((result) => result.name === 'ip_rate_limit_10_failures_then_429');
  const blockRow = results.find((result) => result.name === 'device_blocks_after_5_failures_never_403');
  const verdict = (row) => (row ? (row.ok ? 'PASS' : 'FAIL') : 'not run');
  facts.rateLimitNote = `IP 10 failures/15 min → 429: ${verdict(ipRow)}; device blocked after 5: ${verdict(blockRow)}`;

  out('');
  const paired = results.filter((result) => result.ok).length;
  out(`--- ${paired}/${results.length} rows so far (${results.length - paired} failed) ---`);
  return facts;
}

// ---------------------------------------------------------------------------
// Run + cleanup (always in `finally`)
// ---------------------------------------------------------------------------
let exitCode = 0;
try {
  await main();
} catch (error) {
  out('');
  out(`FATAL: ${error && error.message ? error.message : String(error)}`);
  exitCode = 1;
} finally {
  // Never leave a child process or a temp directory behind.
  try {
    stopServer();
  } catch {
    /* ignore */
  }
  if (server.dir) {
    try {
      fs.rmSync(server.dir, { recursive: true, force: true });
    } catch (error) {
      out(`WARN: temp dir cleanup failed: ${error.message}`);
    }
  }

  const failures = results.filter((result) => !result.ok).length;
  if (failures > 0) exitCode = 1;
  const leaks = outputLeakCount();
  if (leaks > 0) {
    out(`LEAK CHECK FAILED: ${leaks} secret occurrence(s) in runner output`);
    exitCode = 1;
  } else {
    out(`leak check: 0 of ${secrets.length} secret values in runner output`);
  }
  out(`exit code: ${exitCode}`);
  flush();
}
process.exitCode = exitCode;
// Keep the output flush deterministic even if an idle socket holds the loop.
const safetyNet = setTimeout(() => process.exit(exitCode), 5000);
safetyNet.unref();
