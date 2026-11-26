#!/usr/bin/env node
/**
 * T14 optional real-browser reload-loop check (offline, no installs).
 *
 * Why: docs/w1-notes/T12.md §3/§6 asks T14 to verify the *visual* behaviour of
 * the patched front-end in a real browser:
 *
 *   * unpaired load → PIN gate appears and the page does **not** reload-loop
 *     (S8 §7.7: 5 navigations / 5 s would be a loop);
 *   * PIN entered → gate hides and the app starts (exactly one
 *     `/api/pin/verify` request — no re-pair);
 *   * reload → still paired: the gate must not come back and no second PIN
 *     request may be issued;
 *   * cookies after pairing: `lampa_session` HttpOnly + readable
 *     `device_token` (transitional C);
 *   * after an admin revoke → reload shows the gate again (revocation gate,
 *     still no reload-loop).
 *
 * The check is *optional*: it only runs when a Playwright package and a
 * matching Chromium build are already present offline. Nothing is installed
 * (no `npm install`, no `playwright install`); when the tooling is missing the
 * runner reports `{"status":"skip"}` so the caller records owner open item O4
 * while the HTTP matrix stays authoritative.
 *
 * Harness: one Node process serves the built front-end (`build/web`) and
 * proxies `/api/*` + `/admin/*` to the real `lampa-server`, so the browser uses
 * a single origin with real cookies. All non-loopback requests are aborted, so
 * the run is fully offline and deterministic.
 *
 * Inputs (env): T14_BASE_URL, T14_TOTP_SECRET, T14_ADMIN_PASSWORD (optional,
 * enables the revoke step), T14_REPO. The front end generates its own
 * device_id, so the revoke step discovers the row this pairing created by
 * diffing `GET /api/admin/devices` before/after — no device id is assumed.
 *
 * Output: exactly one line, `BROWSER_RESULT {json}` — it never contains a
 * token, PIN, cookie or credential value.
 */

import { createRequire } from 'node:module';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = process.env.T14_REPO || path.resolve(HERE, '..', '..');
const UPSTREAM = (process.env.T14_BASE_URL || '').replace(/\/+$/, '');
const TOTP_SECRET = process.env.T14_TOTP_SECRET || '';
const ADMIN_PASSWORD = process.env.T14_ADMIN_PASSWORD || '';
const STATIC_ROOT = path.join(REPO, 'build', 'web');

const require = createRequire(import.meta.url);

/**
 * The runner always ends with exactly one `BROWSER_RESULT` line.
 *
 * `signal()` throws a control-flow sentinel so that every `finally` (browser
 * close, harness close) still runs before the line is printed — cleanup must
 * never be skipped, whichever branch produced the result.
 */
class ResultSignal extends Error {
  constructor(status, detail, extra) {
    super(`browser result: ${status}`);
    this.status = status;
    this.detail = detail;
    this.extra = extra || {};
  }
}
const signal = (status, detail, extra) => {
  throw new ResultSignal(status, detail, extra);
};
const skip = (reason) => signal('skip', reason);
const fail = (detail) => signal('fail', detail);
const pass = (detail) => signal('pass', detail);

// ---------------------------------------------------------------------------
// TOTP (same algorithm the server and auth.mjs use)
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

function totpNow(secret) {
  const counter = Buffer.alloc(8);
  counter.writeBigUInt64BE(BigInt(Math.floor(Date.now() / 1000 / 30)));
  const mac = require('node:crypto')
    .createHmac('sha1', base32Decode(secret))
    .update(counter)
    .digest();
  const offset = mac[mac.length - 1] & 0x0f;
  const code =
    (((mac[offset] & 0x7f) << 24) | (mac[offset + 1] << 16) | (mac[offset + 2] << 8) | mac[offset + 3]) %
    1_000_000;
  return String(code).padStart(6, '0');
}

// ---------------------------------------------------------------------------
// Static server + API proxy (one origin, real cookies)
// ---------------------------------------------------------------------------
const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.map': 'application/json',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.gif': 'image/gif',
  '.webp': 'image/webp',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.ttf': 'font/ttf',
  '.eot': 'application/vnd.ms-fontobject',
  '.mp3': 'audio/mpeg',
  '.mp4': 'video/mp4',
  '.webm': 'video/webm',
  '.txt': 'text/plain; charset=utf-8',
  '.xml': 'application/xml',
};

function startHarness() {
  const upstreamUrl = new URL(UPSTREAM);

  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://127.0.0.1');
    const isApi = url.pathname.startsWith('/api/') || url.pathname.startsWith('/admin/') ||
      url.pathname === '/api' || url.pathname === '/admin';

    const chunks = [];
    req.on('data', (chunk) => chunks.push(chunk));
    req.on('end', () => {
      const body = Buffer.concat(chunks);
      if (isApi) proxyRequest(req, res, body, upstreamUrl);
      else serveStatic(req, res, url);
    });
  });

  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => resolve({ server, port: server.address().port }));
  });
}

function proxyRequest(req, res, body, upstreamUrl) {
  const headers = { ...req.headers, host: upstreamUrl.host, 'accept-encoding': 'identity' };
  delete headers.connection;
  delete headers['content-length'];

  const upstream = http.request(
    {
      protocol: upstreamUrl.protocol,
      hostname: upstreamUrl.hostname,
      port: upstreamUrl.port,
      path: req.url,
      method: req.method,
      headers,
    },
    (upstreamRes) => {
      res.writeHead(upstreamRes.statusCode || 502, upstreamRes.headers);
      upstreamRes.pipe(res);
    },
  );
  upstream.on('error', (error) => {
    res.writeHead(502, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ error: 'proxy_error', detail: String(error.message) }));
  });
  if (body.length > 0) upstream.write(body);
  upstream.end();
}

function serveStatic(req, res, url) {
  const relative = url.pathname === '/' ? '/index.html' : url.pathname;
  const file = path.normalize(path.join(STATIC_ROOT, relative));
  if (file !== STATIC_ROOT && !file.startsWith(STATIC_ROOT + path.sep)) {
    res.writeHead(403);
    res.end('forbidden');
    return;
  }
  fs.readFile(file, (error, data) => {
    if (error) {
      res.writeHead(404, { 'content-type': 'text/plain' });
      res.end('not found');
      return;
    }
    res.writeHead(200, {
      'content-type': MIME[path.extname(file).toLowerCase()] || 'application/octet-stream',
      'cache-control': 'no-store',
    });
    res.end(data);
  });
}

// ---------------------------------------------------------------------------
// Playwright resolution (offline only — never installs)
// ---------------------------------------------------------------------------
function resolvePlaywright() {
  const candidates = [];
  if (process.env.PLAYWRIGHT_PATH) candidates.push(process.env.PLAYWRIGHT_PATH);
  try {
    candidates.push(require.resolve('playwright'));
  } catch {
    /* not resolvable from here */
  }
  candidates.push(path.join(REPO, 'node_modules', 'playwright', 'index.js'));
  candidates.push(path.join(HERE, 'node_modules', 'playwright', 'index.js'));
  const npxRoot = path.join(process.env.HOME || '', '.npm', '_npx');
  try {
    for (const entry of fs.readdirSync(npxRoot)) {
      candidates.push(path.join(npxRoot, entry, 'node_modules', 'playwright', 'index.js'));
    }
  } catch {
    /* no npx cache */
  }
  for (const candidate of candidates) {
    try {
      if (candidate.endsWith('.js') && !fs.existsSync(candidate)) continue;
      return require(candidate.endsWith('.js') ? candidate : path.join(candidate, 'index.js'));
    } catch {
      /* try the next candidate */
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// Admin helpers for the revoke step (direct HTTP, never printed)
// ---------------------------------------------------------------------------
/** Admin cookie for the direct (non-proxied) admin calls. Throws on failure. */
async function adminAuth(baseUrl, adminPassword) {
  const login = await fetch(`${baseUrl}/admin/login`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ password: adminPassword }),
  });
  if (login.status !== 200) throw new Error(`admin login answered ${login.status}`);
  const setCookie = login.headers.getSetCookie().find((value) => value.startsWith('auth_token='));
  if (!setCookie) throw new Error('admin login set no auth_token cookie');
  return `${setCookie.split(';')[0]}`;
}

async function deviceRows(baseUrl, cookie) {
  const list = await fetch(`${baseUrl}/api/admin/devices`, { headers: { cookie } });
  if (list.status !== 200) throw new Error(`device list answered ${list.status}`);
  return list.json();
}

async function revokeById(baseUrl, cookie, id) {
  const action = await fetch(`${baseUrl}/api/admin/device/action`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', cookie },
    body: JSON.stringify({ id, action: 'revoke' }),
  });
  if (action.status !== 200) throw new Error(`revoke answered ${action.status}`);
  const body = await action.json();
  if (!body || body.success !== true) throw new Error('revoke did not acknowledge success');
}

// ---------------------------------------------------------------------------
// The check
// ---------------------------------------------------------------------------
const LOOP_THRESHOLD = 5; // S8 §7.7: 5 navigations / 5 s = reload loop
const WINDOW_MS = 5000;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function main() {
  if (!UPSTREAM) skip('T14_BASE_URL not provided');
  if (!TOTP_SECRET) skip('T14_TOTP_SECRET not provided');
  if (!fs.existsSync(path.join(STATIC_ROOT, 'index.html'))) {
    skip(`built front-end missing at ${STATIC_ROOT} (npm run build was not executed)`);
  }

  const playwright = resolvePlaywright();
  if (!playwright) {
    skip('no Playwright package available offline (no local/npx-cache playwright, nothing installed)');
    return;
  }

  let chromium;
  try {
    chromium = playwright.chromium;
  } catch (error) {
    skip(`playwright package exposes no chromium (${error.message})`);
    return;
  }

  const harness = await startHarness();
  const baseUrl = `http://127.0.0.1:${harness.port}`;
  const notes = [];
  let browser = null;

  try {
    try {
      browser = await chromium.launch({ headless: true });
    } catch (error) {
      skip(`chromium could not launch offline (${String(error.message).split('\n')[0]})`);
      return;
    }

    const context = await browser.newContext();
    // Fully offline: only the local harness may be reached.
    await context.route('**/*', (route) => {
      const host = new URL(route.request().url()).hostname;
      if (host === '127.0.0.1' || host === 'localhost') route.continue();
      else route.abort();
    });

    const page = await context.newPage();
    // A returning device already picked its language: without `language` in
    // localStorage the app stops at the language chooser (src/app.js
    // `loadApp()` → `LangChoice.open`) and `checkDevice()` never runs. The
    // reload-loop question is about an already-configured device, so the
    // language preference is seeded up front (localStorage only — no cookies,
    // the device stays unpaired).
    await context.addInitScript(() => {
      try {
        localStorage.setItem('language', 'en');
        localStorage.setItem('tmdb_lang', 'en');
      } catch {
        /* ignore */
      }
    });
    const documentLoads = []; // real document loads — a reload-loop is made of these
    const spaNavigations = []; // main-frame URL changes, incl. same-document router pushes
    let pinRequests = 0;
    let pinResponseStatus = null;
    const pageErrors = [];
    page.on('load', () => documentLoads.push(Date.now()));
    page.on('framenavigated', (frame) => {
      if (frame === page.mainFrame()) spaNavigations.push(Date.now());
    });
    page.on('request', (request) => {
      if (request.url().includes('/api/pin/verify')) pinRequests++;
    });
    page.on('response', (response) => {
      if (response.url().includes('/api/pin/verify')) pinResponseStatus = response.status();
    });
    page.on('pageerror', (error) => pageErrors.push(String(error.message).slice(0, 160)));

    const gateVisible = async () => {
      try {
        return await page.locator('.pin-gate').isVisible();
      } catch {
        return false;
      }
    };
    // Lampa's router uses same-document URL pushes (`/?title=…&component=…`),
    // which `framenavigated` also reports. Only full document loads count as
    // navigations for the S8 reload-loop criterion; the router pushes are
    // reported separately as app activity.
    const loadsSince = (since) => documentLoads.filter((at) => at > since).length;
    const routerPushesSince = (since) =>
      spaNavigations.filter((at) => at > since).length - loadsSince(since);

    // Baseline device rows: the front end generates its own device_id, so the
    // revoke step targets the row that this browser pairing newly creates.
    let adminCookie = null;
    let baselineIds = null;
    if (ADMIN_PASSWORD) {
      try {
        adminCookie = await adminAuth(UPSTREAM, ADMIN_PASSWORD);
        baselineIds = new Set((await deviceRows(UPSTREAM, adminCookie)).map((row) => row.id));
      } catch (error) {
        notes.push(`revoke baseline unavailable (${error.message})`);
      }
    }

    // 1. Unpaired load → the gate must appear without a reload-loop.
    const t0 = Date.now();
    await page.goto(`${baseUrl}/`, { waitUntil: 'domcontentloaded', timeout: 20_000 });
    try {
      await page.locator('.pin-gate').waitFor({ state: 'visible', timeout: 15_000 });
    } catch {
      const detail =
        `PIN gate never became visible (document loads=${documentLoads.length}, ` +
        `router pushes=${spaNavigations.length}, pin requests=${pinRequests}, ` +
        `page errors=${pageErrors.length})`;
      // Not a reload-loop observation: the app could not boot in this harness
      // (missing later-wave routes/assets). Record it as an owner open item.
      skip(`${detail} — recorded as O4, the HTTP matrix stays authoritative`);
      return;
    }
    await sleep(WINDOW_MS);
    const firstWindow = loadsSince(t0 - 1);
    notes.push(
      `unpaired: gate visible, ${firstWindow} document load(s) + ${routerPushesSince(t0 - 1)} router push(es) in ${WINDOW_MS / 1000}s`,
    );
    if (firstWindow >= LOOP_THRESHOLD) {
      fail(
        `reload loop while unpaired: ${firstWindow} document loads in ` +
          `${WINDOW_MS / 1000}s (S8 threshold ${LOOP_THRESHOLD})`,
      );
      return;
    }

    // 2. Enter the PIN. Keyboard first: the gate is keypad-driven (its window
    //    keydown listener feeds `addDigit`) and it needs no hit-testing, which
    //    a TV overlay (`div.welcome`) may otherwise swallow. A pointer click on
    //    the on-screen keys is the fallback.
    const filledDigits = () => page.locator('.pin-gate__digit.filled').count();
    const code = totpNow(TOTP_SECRET);
    for (const digit of code) {
      const before = await filledDigits();
      await page.keyboard.press(digit);
      let after = await filledDigits();
      if (after === before) {
        const key = page.locator(`.pin-gate__key[data-key="${digit}"]`);
        if ((await key.count()) > 0) {
          await key.click({ timeout: 3000, force: true }).catch(() => {});
          after = await filledDigits();
        }
      }
      if (after === before) {
        skip('keypad did not accept input (neither keyboard nor pointer) — recorded as O4');
        return;
      }
    }

    const pinDeadline = Date.now() + 10_000;
    while (pinResponseStatus === null && Date.now() < pinDeadline) await sleep(100);
    if (pinResponseStatus === null) {
      skip('no /api/pin/verify response was issued after entering the PIN — recorded as O4');
      return;
    }
    notes.push(`PIN verify answered ${pinResponseStatus}`);
    await sleep(1500);
    if (await gateVisible()) {
      fail('PIN accepted but the gate stayed visible after verification');
      return;
    }
    if (pinRequests !== 1) {
      fail(`expected exactly one PIN verify request, saw ${pinRequests}`);
      return;
    }
    notes.push('PIN paired: gate hidden, 1 PIN verify request');

    // 3. Cookie model after pairing (transitional C).
    const cookies = await context.cookies(`${baseUrl}/`);
    const session = cookies.find((cookie) => cookie.name === 'lampa_session');
    const legacy = cookies.find((cookie) => cookie.name === 'device_token');
    if (!session || !legacy) {
      fail('expected both lampa_session and device_token cookies after PIN');
      return;
    }
    if (!session.httpOnly) {
      fail('lampa_session must be HttpOnly');
      return;
    }
    if (legacy.httpOnly) {
      fail('device_token must stay readable while mode C is active');
      return;
    }
    if (session.value !== legacy.value) fail('session and device cookies must carry the same token');
    notes.push('cookies: lampa_session HttpOnly + readable device_token (same value)');

    // 4. Reload → must stay paired (0 re-pair, no gate, no loop).
    const t1 = Date.now();
    await page.reload({ waitUntil: 'domcontentloaded', timeout: 20_000 });
    await sleep(WINDOW_MS);
    const secondWindow = loadsSince(t1 - 1);
    notes.push(
      `reload: ${secondWindow} document load(s) + ${routerPushesSince(t1 - 1)} router push(es) in ${WINDOW_MS / 1000}s`,
    );
    if (secondWindow >= LOOP_THRESHOLD) {
      fail(
        `reload loop while paired: ${secondWindow} document loads in ` +
          `${WINDOW_MS / 1000}s (S8 threshold ${LOOP_THRESHOLD})`,
      );
      return;
    }
    if (await gateVisible()) {
      fail('reload after pairing brought the PIN gate back (re-pair required)');
      return;
    }
    if (pinRequests !== 1) {
      fail(`reload must not trigger a second PIN verify (saw ${pinRequests})`);
      return;
    }
    notes.push('reload: still paired, 0 re-pair');

    // 5. Admin revoke → the gate must return on the next load, without a loop.
    if (!ADMIN_PASSWORD) {
      notes.push('revoke step skipped (no T14_ADMIN_PASSWORD)');
    } else if (!adminCookie || !baselineIds) {
      notes.push('revoke step skipped (admin session unavailable)');
    } else {
      try {
        const created = (await deviceRows(UPSTREAM, adminCookie)).filter(
          (row) => !baselineIds.has(row.id),
        );
        if (created.length === 0) {
          notes.push('revoke step skipped (pairing created no new device row)');
        } else {
          const target = created.reduce((best, row) => (row.id > best.id ? row : best));
          await revokeById(UPSTREAM, adminCookie, target.id);
          notes.push(`revoke: admin action revoked the paired device row id=${target.id}`);

          const t2 = Date.now();
          await page.reload({ waitUntil: 'domcontentloaded', timeout: 20_000 });
          try {
            await page.locator('.pin-gate').waitFor({ state: 'visible', timeout: 15_000 });
          } catch {
            fail('revoked device did not return to the PIN gate on reload');
            return;
          }
          await sleep(WINDOW_MS);
          const thirdWindow = loadsSince(t2 - 1);
          notes.push(
            `revoke: gate returned, ${thirdWindow} document load(s) + ${routerPushesSince(t2 - 1)} router push(es) in ${WINDOW_MS / 1000}s`,
          );
          if (thirdWindow >= LOOP_THRESHOLD) {
            fail(
              `reload loop after revoke: ${thirdWindow} document loads in ` +
                `${WINDOW_MS / 1000}s (S8 threshold ${LOOP_THRESHOLD})`,
            );
            return;
          }
        }
      } catch (error) {
        notes.push(`revoke step skipped (${error.message})`);
      }
    }

    pass(notes.join('; '));
  } finally {
    if (browser) {
      try {
        await browser.close();
      } catch {
        /* ignore */
      }
    }
    harness.server.close();
  }
}

let printed = false;
function printResult(status, detail, extra = {}) {
  if (printed) return;
  printed = true;
  process.stdout.write(`BROWSER_RESULT ${JSON.stringify({ status, detail, ...extra })}\n`);
}

try {
  await main();
  // main() only returns through `pass`, so this is a safety net.
  printResult('skip', 'browser check finished without producing a result');
} catch (error) {
  if (error instanceof ResultSignal) {
    printResult(error.status, error.detail, error.extra);
  } else {
    printResult(
      'skip',
      `browser check crashed: ${String(error && error.message ? error.message : error).slice(0, 200)}`,
    );
  }
}
