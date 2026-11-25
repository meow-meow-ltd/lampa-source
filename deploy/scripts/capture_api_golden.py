#!/usr/bin/env python3
"""T03 golden capture for the 27 legacy Lampa routes (docs/decisions.md W0-3).

Captures status/headers/body for the 27 legacy routes - 25 under ``/api/*``
plus ``POST /admin/login`` and ``POST /admin/logout`` (``GET /admin`` is a
page and is deliberately NOT one of the 27) - from the *pinned* production
Node image running against a deterministic SYNTHETIC SQLite v7 database built
by ``deploy/scripts/make_synthetic_db.py``.

Never captured from real data: the production copy and the staging copy of the
database are never opened by the capture path.  The only access to either copy
is an optional read-only (``file:...?mode=ro``) row-count probe performed
before and after a run, purely as invariance evidence; it cannot write.

Capture mechanics
-----------------
* one throwaway container, name ``lampa-golden-<random>``, published on
  ``127.0.0.1:9121`` (never 9119 - that is the staging stand, never touched),
  bind-mounted to a throwaway work directory, removed with ``docker rm -f -v``
  in a ``finally`` block together with the work directory;
* environment handed to the container: ``DB_PATH``, ``HOST``, ``PORT`` and a
  freshly generated *synthetic* ``TOTP_SECRET`` - no value is read from any
  env file and no staging/production credential is used anywhere;
* requests are issued in the fixed order 1..27 declared in ``ROUTES``;
* every response and every request is normalized (see ``RULES``) before it is
  written or compared, so a re-run on a fresh synthetic DB reports
  "normalized diff 0".

Modes
-----
* (default / ``--write``) regenerate ``server/tests/fixtures/api/``;
* ``--check``   re-run the capture on a fresh synthetic DB into a temp dir and
  diff it against the committed fixtures; also runs the secret scan and the
  staging invariance probe.  Prints ``27/27 fixtures, normalized diff 0,
  secrets 0``;
* ``--audit``   everything ``--check`` does, plus the builder invariants of
  the fresh synthetic DB and the post-capture DB (profiles=3, bookmarks<=20,
  timeline_events<=50, schema_version=7, journal_mode=delete, no
  ``_sqlx_migrations``, no WAL sidecars).

Exit codes: 0 pass, 1 verification failure, 2 setup/usage error.
"""
import argparse
import base64
import hashlib
import hmac
import http.client
import json
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.parse

# --------------------------------------------------------------------------
# Fixed constants (single source of truth for the 27-route contract)
# --------------------------------------------------------------------------

# Pinned digest of the production image (same digest as deploy/compose.staging.yaml).
# The tag ``latest`` is mutable, the digest is not.  Never pulled: the script
# fails closed when the image is absent from the local store.
PINNED_IMAGE = (
    "ghcr.io/meow-meow-ltd/lampa-source@"
    "sha256:d678c142ba58ff1707f5081a73c8b85393e75c36638754b1a6873fbffc21add7"
)
BASE_HOST = "127.0.0.1"
BASE_PORT = 9121                      # throwaway capture stand; 9119 is staging
USER_AGENT = "lampa-golden/1.0"       # fixed: keeps device_access.user_agent stable
CONTAINER_PREFIX = "lampa-golden-"

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FIXTURE_DIR = os.path.join(REPO_ROOT, "server", "tests", "fixtures", "api")
BUILDER = os.path.join(REPO_ROOT, "deploy", "scripts", "make_synthetic_db.py")
WORK_ROOT = os.path.join(os.path.dirname(REPO_ROOT), ".golden-capture-tmp")

# Read-only invariance probes (never opened by the capture itself).
STAGING_DB = "/opt/lampa/staging-data/database.sqlite"
PROD_DB = "/opt/lampa/data/database.sqlite"

# Synthetic TOTP secret, derived from a literal defined in this file.  It is not
# a production value and not a staging value (staging keeps its throwaway
# values in a 0600 file outside git, which this script never reads).
TOTP_SECRET = base64.b32encode(b"lampa-golden-totp-secret-t03-2026").decode("ascii")

ADMIN_PASSWORD = "golden-admin-password"     # must match make_synthetic_db.py
GOLDEN_PIN_DEVICE = "golden-device-0001"

AUTH_NONE = "none"
AUTH_ADMIN = "admin"
AUTH_DEVICE = "device"

AUTH_TEXT = {
    AUTH_NONE: "no cookies",
    AUTH_ADMIN: "cookie auth_token (admin, from setup POST /admin/login)",
    AUTH_DEVICE: "cookie device_token (issued by fixture 01)",
}

# Normalization rules.  Ids are what the README/manifest "sanitization"
# column records; a rule is listed for a route only when it actually fired.
RULES = [
    ("R1", "response header Date -> \"<date>\""),
    ("R2", "response Set-Cookie value -> \"<redacted>\" (auth_token never stored)"),
    ("R3", "timestamp fields (created_at/updated_at/last_attempt_at/"
           "last_seen_at/blocked_until/first_attempt_at) -> \"<ts>\""),
    ("R4", "ip fields -> \"<ip>\" (docker gateway address is environment dependent)"),
    ("R5", "settings rows keyed admin_password/admin_token -> value \"***\""),
    ("R6", "secret fields (admin_password, admin_token, device_token, auth_token,"
           " password, pin, totp, token, session, secret) -> \"***\""),
    ("R7", "any 64-hex token in the body -> \"***\""),
    ("R8", "request Cookie header -> cookie names only, values \"<redacted>\""),
    ("R9", "response Content-Length recomputed over the normalized body"
           " (legacy API answers are chunked, so this normally does not fire)"),
]

RULE_IDS = [rid for rid, _ in RULES]

TS_FIELDS = {
    "created_at", "updated_at", "last_attempt_at", "last_seen_at",
    "blocked_until", "first_attempt_at",
}
SECRET_FIELDS = {
    "admin_password", "admin_token", "device_token", "auth_token",
    "password", "pin", "totp", "token", "session", "secret",
}
SECRET_SETTING_KEYS = {"admin_password", "admin_token"}
HEX64_RE = re.compile(r"\b[A-Fa-f0-9]{64}\b")

MANIFEST_NAME = "manifest"          # extensionless JSON: `ls api/*.json` = 27 fixtures
MANIFEST_SCHEMA = "lampa-golden-fixtures/1"
README_NAME = "README.md"

# ---------------------------------------------------------------------------
# The 27 routes, in the fixed capture order.  `path` is the request target,
# `route` is the contract pattern shown in the README/manifest.
# A body may be a dict or a callable taking the capture context.
# ---------------------------------------------------------------------------

def _bookmarks_sync_payload():
    return json.dumps(
        {
            "film": [
                {"id": 1001, "title": "Golden Fixture Card 1001", "synthetic": True},
                {"id": 1002, "title": "Golden Fixture Card 1002", "synthetic": True},
            ],
            "cartoon": [
                {"id": 1004, "title": "Golden Fixture Card 1004", "synthetic": True},
            ],
            "game": [
                {"id": 9007, "title": "Golden Fixture Card 9007", "synthetic": True},
            ],
        },
        sort_keys=True,
        separators=(",", ":"),
    )


def _backup_payload():
    return json.dumps(
        {"golden": True, "items": [1, 2, 3], "note": "synthetic backup"},
        sort_keys=True,
        separators=(",", ":"),
    )


ROUTES = (
    dict(index=1, method="POST", route="POST /api/pin/verify",
         path="/api/pin/verify", auth=AUTH_NONE,
         body=lambda ctx: {"pin": ctx.totp(), "device_id": GOLDEN_PIN_DEVICE},
         expect=200,
         note="issues a random device_token; TOTP code is masked (R6)"),
    dict(index=2, method="GET", route="GET /api/admin/devices",
         path="/api/admin/devices", auth=AUTH_ADMIN, expect=200,
         note="admin cookie required; device_access.ip/user_agent/timestamps masked"),
    dict(index=3, method="POST", route="POST /api/admin/device/action",
         path="/api/admin/device/action", auth=AUTH_ADMIN,
         body={"id": 2, "action": "unblock"}, expect=200,
         note="unblocks the synthetic blocked device (id=2)"),
    dict(index=4, method="GET", route="GET /api/settings",
         path="/api/settings", auth=AUTH_ADMIN, expect=200,
         note="legacy returns ALL settings; admin_password/admin_token masked (R5)"),
    dict(index=5, method="PUT", route="PUT /api/settings",
         path="/api/settings", auth=AUTH_ADMIN,
         body={"key": "torrserver_url", "value": "http://synthetic-updated.invalid:9091"},
         expect=200, note="mutates the synthetic settings row read back by fixture 07"),
    dict(index=6, method="DELETE", route="DELETE /api/settings/{key}",
         path="/api/settings/head_notice", auth=AUTH_ADMIN, expect=200,
         note="legacy takes the key from the last path segment"),
    dict(index=7, method="POST", route="POST /api/values",
         path="/api/values", auth=AUTH_DEVICE,
         body={"keys": ["menu_data", "admin_password", "torrserver_url"]},
         expect=200,
         note="invisible keys come back as null (admin_password is not leaked)"),
    dict(index=8, method="GET", route="GET /api/value",
         path="/api/value?key=admin_password", auth=AUTH_DEVICE, expect=200,
         note="visible=0 key -> body is the JSON literal null"),
    dict(index=9, method="GET", route="GET /api/profiles/all",
         path="/api/profiles/all", auth=AUTH_DEVICE, expect=200,
         note="3 synthetic profiles, Main first (is_main DESC, sort ASC, id ASC)"),
    dict(index=10, method="POST", route="POST /api/profiles/create",
         path="/api/profiles/create", auth=AUTH_DEVICE,
         body={"name": "Golden Created", "icon": "l_9"}, expect=200,
         note="new profile gets id=4; deleted again by fixture 12"),
    dict(index=11, method="POST", route="POST /api/profiles/update",
         path="/api/profiles/update", auth=AUTH_DEVICE,
         body={"id": 3, "icon": "l_7", "child": True}, expect=200,
         note="legacy boolean `child` maps to is_child"),
    dict(index=12, method="POST", route="POST /api/profiles/delete",
         path="/api/profiles/delete", auth=AUTH_DEVICE,
         body={"id": 4}, expect=200,
         note="removes the profile created by fixture 10 -> profiles back to 3"),
    dict(index=13, method="GET", route="GET /api/profile/settings",
         path="/api/profile/settings", auth=AUTH_DEVICE,
         headers={"profile": "1"}, expect=200,
         note="profile comes from the `profile` header; payload uses `secuses`"),
    dict(index=14, method="POST", route="POST /api/profile/settings",
         path="/api/profile/settings", auth=AUTH_DEVICE,
         headers={"profile": "1"},
         body={"key": "golden_flag", "value": "on"}, expect=200,
         note="upsert keyed by (profile_id, key)"),
    dict(index=15, method="GET", route="GET /api/bookmarks/dump",
         path="/api/bookmarks/dump", auth=AUTH_DEVICE,
         headers={"profile": "1"}, expect=200,
         note="QUIRK: Content-Type is text/plain while the body is JSON"),
    dict(index=16, method="GET", route="GET /api/bookmarks/changelog",
         path="/api/bookmarks/changelog?since=0", auth=AUTH_DEVICE,
         headers={"profile": "1"}, expect=200,
         note="version = MAX(bookmark_events.id) for the profile"),
    dict(index=17, method="POST", route="POST /api/bookmarks/add",
         path="/api/bookmarks/add", auth=AUTH_DEVICE,
         headers={"profile": "1"},
         body={"type": "film", "card_id": 9001,
               "data": json.dumps({"id": 9001, "synthetic": True,
                                   "title": "Golden Fixture Card 9001"},
                                  sort_keys=True, separators=(",", ":"))},
         expect=200, note="adds a bookmark plus an `add` changelog event"),
    dict(index=18, method="POST", route="POST /api/bookmarks/remove",
         path="/api/bookmarks/remove", auth=AUTH_DEVICE,
         headers={"profile": "1"},
         body={"type": "film", "card_id": 9001}, expect=200,
         note="removes the bookmark added by fixture 17"),
    dict(index=19, method="POST", route="POST /api/bookmarks/clear",
         path="/api/bookmarks/clear", auth=AUTH_DEVICE,
         headers={"profile": "1"},
         body={"type": "group", "group": "film"}, expect=200,
         note="QUIRK: type must be the literal `group`, `group` holds the type"),
    dict(index=20, method="POST", route="POST /api/bookmarks/sync",
         path="/api/bookmarks/sync", auth=AUTH_DEVICE,
         headers={"profile": "1"},
         body={"data": _bookmarks_sync_payload()}, expect=200,
         note="replaces all bookmarks of the profile inside a transaction"),
    dict(index=21, method="GET", route="GET /api/timeline/dump",
         path="/api/timeline/dump", auth=AUTH_DEVICE,
         headers={"profile": "1"}, expect=200,
         note="QUIRK: Content-Type is text/plain while the body is JSON"),
    dict(index=22, method="GET", route="GET /api/timeline/changelog",
         path="/api/timeline/changelog?since=0", auth=AUTH_DEVICE,
         headers={"profile": "1"}, expect=200,
         note="version = MAX(timeline_events.id) for the profile"),
    dict(index=23, method="POST", route="POST /api/timeline/update",
         path="/api/timeline/update", auth=AUTH_DEVICE,
         headers={"profile": "1"},
         body={"hash": "golden-hash-000a", "percent": 80, "time": 4800,
               "duration": 6000}, expect=200,
         note="upserts timeline + appends a timeline_events row (+1 version)"),
    dict(index=24, method="POST", route="POST /api/users/backup/export",
         path="/api/users/backup/export", auth=AUTH_DEVICE,
         body={"data": _backup_payload()}, expect=200,
         note="QUIRK: `export` is the write side and answers {secuses: true}"),
    dict(index=25, method="GET", route="GET /api/users/backup/import",
         path="/api/users/backup/import", auth=AUTH_DEVICE, expect=200,
         note="QUIRK: `import` is the read side (inverted vs. the name)"),
    dict(index=26, method="POST", route="POST /admin/login",
         path="/admin/login", auth=AUTH_NONE,
         body={"password": ADMIN_PASSWORD}, expect=200,
         note="sets auth_token cookie (value masked, R2)"),
    dict(index=27, method="POST", route="POST /admin/logout",
         path="/admin/logout", auth=AUTH_NONE, expect=200,
         note="clears auth_token (empty value, Max-Age=0; kept verbatim)"),
)

assert len(ROUTES) == 27, "the contract is exactly 27 routes"


def fixture_name(spec):
    """01_post_api_pin_verify.json ... (derived from the contract pattern)."""
    slug = re.sub(r"[^A-Za-z0-9]+", "_", spec["route"]).strip("_").lower()
    return "%02d_%s.json" % (spec["index"], slug)


# ---------------------------------------------------------------------------
# small helpers
# ---------------------------------------------------------------------------

def log(message):
    print("capture_api_golden: %s" % message, flush=True)


def die(message, code=2):
    print("capture_api_golden: ERROR: %s" % message, file=sys.stderr, flush=True)
    raise SystemExit(code)


def run(cmd, **kwargs):
    proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          universal_newlines=True, **kwargs)
    if proc.returncode != 0:
        raise RuntimeError(
            "%s failed (%d): %s%s" % (" ".join(cmd), proc.returncode,
                                      proc.stdout[-2000:], proc.stderr[-2000:])
        )
    return proc.stdout


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def db_probe(path):
    """Read-only row counts of a SQLite file.  Returns None if unavailable.

    Opens the file with ``mode=ro`` only - no query can modify it and no
    journal/WAL sidecar can be created.  Used purely as before/after
    invariance evidence.
    """
    if not path or not os.path.exists(path):
        return None
    import sqlite3

    uri = "file:%s?mode=ro" % path
    conn = sqlite3.connect(uri, uri=True, timeout=10)
    try:
        tables = [r[0] for r in conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
        )]
        counts = {}
        for table in tables:
            counts[table] = conn.execute(
                'SELECT COUNT(*) FROM "%s"' % table
            ).fetchone()[0]
        journal = conn.execute("PRAGMA journal_mode").fetchone()[0]
        return {"tables": tables, "counts": counts, "journal_mode": str(journal)}
    finally:
        conn.close()


def probe_line(probe):
    """One-line, secret-free payload of a read-only DB probe."""
    if probe is None:
        return "unavailable (skipped)"
    return "journal_mode=%s rows=%s" % (
        probe["journal_mode"],
        ",".join("%s=%d" % (k, v) for k, v in sorted(probe["counts"].items())),
    )


# ---------------------------------------------------------------------------
# TOTP (speakeasy-compatible: HMAC-SHA1, 30 s step, 6 digits)
# ---------------------------------------------------------------------------

def totp_code(secret_b32, at_time=None):
    key = base64.b32decode(secret_b32, casefold=True)
    counter = struct.pack(">Q", int((time.time() if at_time is None else at_time) // 30))
    digest = hmac.new(key, counter, hashlib.sha1).digest()
    offset = digest[-1] & 0x0F
    code = (struct.unpack(">I", digest[offset:offset + 4])[0] & 0x7FFFFFFF) % 1000000
    return "%06d" % code


# ---------------------------------------------------------------------------
# HTTP
# ---------------------------------------------------------------------------

class Ctx(object):
    """Capture context: synthetic credentials only, never staging values."""

    def __init__(self):
        self.totp_secret = TOTP_SECRET
        self.auth_token = None    # from the setup login
        self.device_token = None  # from fixture 01

    def totp(self):
        return totp_code(self.totp_secret)


def http_request(method, path, body=None, headers=None, timeout=20):
    """Issue one request against the capture stand; returns raw result."""
    conn = http.client.HTTPConnection(BASE_HOST, BASE_PORT, timeout=timeout)
    out_headers = {"User-Agent": USER_AGENT, "Accept": "*/*"}
    out_headers.update(headers or {})
    payload = None
    if body is not None:
        payload = json.dumps(body, separators=(",", ":"), ensure_ascii=False)
        payload = payload.encode("utf-8")
        out_headers.setdefault("Content-Type", "application/json")
    try:
        conn.request(method, path, body=payload, headers=out_headers)
        response = conn.getresponse()
        raw = response.read()
        status = response.status
        items = [[name, value] for name, value in response.getheaders()]
    finally:
        conn.close()
    text = raw.decode("utf-8", errors="replace")
    return {
        "status": status,
        "header_items": items,
        "body": text,
        "sent": {
            "method": method,
            "path": path,
            "headers": out_headers,
            "body": body,
        },
        "raw_body": text,
    }


def wait_ready(timeout=60):
    deadline = time.time() + timeout
    last_error = None
    while time.time() < deadline:
        try:
            result = http_request("GET", "/", timeout=5)
            if result["status"] == 200:
                return
            last_error = "status %s" % result["status"]
        except Exception as exc:  # noqa: BLE001 - readiness probing
            last_error = str(exc)
        time.sleep(0.25)
    die("capture stand did not become ready within %ss: %s" % (timeout, last_error))


# ---------------------------------------------------------------------------
# normalization (R1..R9)
# ---------------------------------------------------------------------------

def mask_cookie(value, fired):
    match = re.match(r"^(?P<name>[^=]+)=(?P<value>[^;]*)(?P<rest>.*)$", value, re.S)
    if not match:
        return value
    if match.group("value").strip() == "":
        return value  # logout clearing cookie: empty value, no secret
    fired.add("R2")
    return "%s=<redacted>%s" % (match.group("name"), match.group("rest"))


def mask_headers(items, fired):
    out = []
    for name, value in items:
        low = name.lower()
        if low == "date":
            fired.add("R1")
            out.append([name, "<date>"])
        elif low == "set-cookie":
            out.append([name, mask_cookie(value, fired)])
        elif low == "content-length":
            out.append([name, value])  # recomputed in finish_headers (R9)
        else:
            out.append([name, value])
    return out


def mask_value(key, value, fired):
    if key in TS_FIELDS and isinstance(value, (int, float)) and not isinstance(value, bool):
        fired.add("R3")
        return "<ts>"
    if key == "ip" and isinstance(value, str) and value:
        fired.add("R4")
        return "<ip>"
    if key in SECRET_FIELDS and isinstance(value, str) and value:
        fired.add("R6")
        return "***"
    return value


def mask_structure(node, fired):
    if isinstance(node, dict):
        # settings rows carry the secret name in `key` and the value in `value`
        secret_row = (
            isinstance(node.get("key"), str)
            and node.get("key") in SECRET_SETTING_KEYS
            and isinstance(node.get("value"), str)
            and node.get("value") != ""
        )
        out = {}
        if secret_row:
            fired.add("R5")
        for key, value in node.items():
            if secret_row and key == "value":
                out[key] = "***"
                continue
            if key in TS_FIELDS and isinstance(value, (int, float)) and not isinstance(value, bool):
                fired.add("R3")
                out[key] = "<ts>"
            elif key == "ip" and isinstance(value, str) and value:
                fired.add("R4")
                out[key] = "<ip>"
            elif key in SECRET_FIELDS and isinstance(value, str) and value:
                fired.add("R6")
                out[key] = "***"
            else:
                out[key] = mask_structure(value, fired)
        return out
    if isinstance(node, list):
        return [mask_structure(item, fired) for item in node]
    return node


def mask_body(text, fired):
    """Normalize a response/request body: structural masks first, R7 last."""
    parsed = None
    try:
        parsed = json.loads(text)
    except (ValueError, TypeError):
        parsed = None
    if parsed is not None or text.strip() in ("null", "true", "false"):
        if parsed is None and text.strip() in ("null", "true", "false"):
            parsed = json.loads(text.strip())
        masked = mask_structure(parsed, fired)
        out = json.dumps(masked, separators=(",", ":"), ensure_ascii=False)
    else:
        out = text
    new_out, count = HEX64_RE.subn("***", out)
    if count:
        fired.add("R7")
    return new_out


def finish_headers(items, body, fired):
    """R9: keep Content-Length consistent with the normalized body."""
    out = []
    for name, value in items:
        if name.lower() == "content-length":
            fired.add("R9")
            out.append([name, str(len(body.encode("utf-8")))])
        else:
            out.append([name, value])
    return out


def mask_request(spec, result, fired):
    """Secret-free view of the request we sent (cookie values never stored)."""
    sent = result["sent"]
    headers = {}
    for name, value in sent["headers"].items():
        low = name.lower()
        if low == "cookie":
            continue
        headers[low] = value
    cookie_names = []
    if "Cookie" in sent["headers"] or "cookie" in sent["headers"]:
        raw = sent["headers"].get("Cookie") or sent["headers"].get("cookie")
        cookie_names = [part.split("=", 1)[0].strip() for part in raw.split(";") if part.strip()]
    if cookie_names:
        fired.add("R8")
    request = {
        "method": sent["method"],
        "path": sent["path"],
        "headers": headers,
        "cookies": cookie_names,
        "body": None,
    }
    if sent["body"] is not None:
        request["body"] = mask_body(
            json.dumps(sent["body"], separators=(",", ":"), ensure_ascii=False), fired
        )
        try:
            request["body"] = json.loads(request["body"])
        except (ValueError, TypeError):
            pass
    return request


# ---------------------------------------------------------------------------
# container lifecycle
# ---------------------------------------------------------------------------

def docker_available():
    return shutil.which("docker") is not None


def check_image():
    if not docker_available():
        die("docker is not available in PATH")
    proc = subprocess.run(
        ["docker", "image", "inspect", PINNED_IMAGE],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, universal_newlines=True,
    )
    if proc.returncode != 0:
        die("pinned image %s is not in the local store (never pulled by this "
            "script): %s" % (PINNED_IMAGE, proc.stderr.strip()[:300]))


def port_free(port):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        sock.bind((BASE_HOST, port))
        return True
    except OSError:
        return False
    finally:
        sock.close()


def start_container(db_dir):
    name = "%s%d-%04x" % (CONTAINER_PREFIX, os.getpid(), int.from_bytes(os.urandom(2), "big"))
    cmd = [
        "docker", "create",
        "--name", name,
        "-p", "127.0.0.1:%d:3000" % BASE_PORT,
        "-e", "DB_PATH=/data/database.sqlite",
        "-e", "HOST=0.0.0.0",
        "-e", "PORT=3000",
        "-e", "TOTP_SECRET=%s" % TOTP_SECRET,
        "-v", "%s:/data" % db_dir,
        PINNED_IMAGE,
    ]
    run(cmd)
    run(["docker", "start", name])
    return name


def stop_container(name):
    if not name:
        return
    subprocess.run(["docker", "rm", "-f", "-v", name],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                   universal_newlines=True)
    left = subprocess.run(
        ["docker", "ps", "-a", "--filter", "name=^/%s$" % name, "--format", "{{.Names}}"],
        stdout=subprocess.PIPE, universal_newlines=True,
    ).stdout.strip()
    if left:
        die("container %s could not be removed (leftover: %s)" % (name, left))


# ---------------------------------------------------------------------------
# capture
# ---------------------------------------------------------------------------

def build_synthetic_db(db_path):
    out = run([sys.executable, BUILDER, "--out", db_path, "--summary-json"])
    return json.loads(out.strip().splitlines()[-1])


def capture(work_dir, strict_status=True):
    """Build the synthetic DB, run the 27 routes, tear everything down.

    Returns (fixtures, builder_summary, post_db_path, base_url, image).
    """
    db_dir = os.path.join(work_dir, "run")
    os.makedirs(db_dir, exist_ok=True)
    db_path = os.path.join(db_dir, "database.sqlite")
    summary = build_synthetic_db(db_path)
    log("synthetic DB: schema_version=%s journal_mode=%s profiles=%s bookmarks=%s "
        "timeline_events=%s settings=%s content_sha256=%s" % (
            summary["schema_version"], summary["journal_mode"], summary["profiles"],
            summary["bookmarks"], summary["timeline_events"], summary["settings"],
            summary["content_sha256"][:16]))

    if not port_free(BASE_PORT):
        die("127.0.0.1:%d is already in use - refusing to touch any other port "
            "(9119 is the staging stand and must stay untouched)" % BASE_PORT)
    check_image()

    container = None
    base_url = "http://%s:%d" % (BASE_HOST, BASE_PORT)
    try:
        container = start_container(db_dir)
        log("container %s up on %s (image %s)" % (container, base_url, PINNED_IMAGE))
        wait_ready()

        ctx = Ctx()
        # setup: obtain the admin cookie (not one of the 27; fixture 26 repeats
        # the login so it is captured as part of the contract).
        login = http_request("POST", "/admin/login",
                             body={"password": ADMIN_PASSWORD})
        if login["status"] != 200:
            die("setup login failed with status %d" % login["status"], 2)
        cookie = ""
        for name, value in login["header_items"]:
            if name.lower() == "set-cookie" and value.startswith("auth_token="):
                cookie = value.split(";", 1)[0].split("=", 1)[1]
        if not cookie:
            die("setup login did not return an auth_token cookie", 2)
        ctx.auth_token = cookie

        fixtures = []
        for spec in ROUTES:
            headers = dict(spec.get("headers") or {})
            auth = spec["auth"]
            if auth == AUTH_ADMIN:
                headers["Cookie"] = "auth_token=%s" % ctx.auth_token
            elif auth == AUTH_DEVICE:
                if not ctx.device_token:
                    die("device_token is missing before fixture %d" % spec["index"], 2)
                headers["Cookie"] = "device_token=%s" % ctx.device_token
            body = spec.get("body")
            if callable(body):
                body = body(ctx)
            result = http_request(spec["method"], spec["path"],
                                  body=body, headers=headers)

            if spec["index"] == 1:
                try:
                    raw = json.loads(result["raw_body"])
                except ValueError:
                    raw = {}
                if not raw.get("ok") or not raw.get("device_token"):
                    die("POST /api/pin/verify did not issue a device_token "
                        "(got %s) - check the system clock (TOTP window) or the "
                        "synthetic device row" % sorted(raw), 2)
                ctx.device_token = raw["device_token"]

            fired = set()
            headers_out = mask_headers(result["header_items"], fired)
            body_out = mask_body(result["body"], fired)
            headers_out = finish_headers(headers_out, body_out, fired)
            fired.add("R1")  # Date is present on every HTTP/1.1 response

            if strict_status and result["status"] != spec["expect"]:
                die("fixture %02d %s: status %d, expected %d (body: %s)" % (
                    spec["index"], spec["route"], result["status"], spec["expect"],
                    mask_body(result["body"], set())[:200]), 1)

            fixture = {
                "index": spec["index"],
                "route": spec["route"],
                "request": mask_request(spec, result, fired),
                "response": {
                    "status": result["status"],
                    "headers": headers_out,
                    "body": body_out,
                },
                "auth": AUTH_TEXT[auth],
                "sanitization": sorted(fired, key=RULE_IDS.index),
            }
            if spec.get("note"):
                fixture["note"] = spec["note"]
            fixtures.append(fixture)
            log("  %02d %-45s -> %d (%s)" % (
                spec["index"], spec["route"], result["status"],
                ",".join(fixture["sanitization"])))
    finally:
        stop_container(container)

    return fixtures, summary, db_path, base_url


# ---------------------------------------------------------------------------
# fixtures on disk
# ---------------------------------------------------------------------------

def render_manifest(fixtures, base_url):
    return {
        "schema": MANIFEST_SCHEMA,
        "route_count": len(fixtures),
        "generator": "deploy/scripts/capture_api_golden.py",
        "builder": "deploy/scripts/make_synthetic_db.py",
        "capture": {
            "image": PINNED_IMAGE,
            "base_url": "%s (throwaway container, never 9119)" % base_url,
            "database": "synthetic SQLite v7, deterministic seed, no real data",
            "request_order": "fixed, index 1..27",
            "teardown": "docker rm -f -v + work directory removed",
        },
        "normalization_rules": [{"id": rid, "rule": text} for rid, text in RULES],
        "routes": [
            {
                "index": fixture["index"],
                "route": fixture["route"],
                "file": fixture_name(ROUTES[fixture["index"] - 1]),
                "method": fixture["request"]["method"],
                "path": fixture["request"]["path"],
                "auth": fixture["auth"],
                "sanitization": fixture["sanitization"],
            }
            for fixture in fixtures
        ],
    }


def render_readme(manifest):
    lines = []
    lines.append("# T03 golden fixtures - 27 legacy routes")
    lines.append("")
    lines.append("Captured from the **pinned** production Node image running against a")
    lines.append("**deterministic synthetic SQLite v7 database** - never from a production or")
    lines.append("staging copy of the data (docs/decisions.md W0-3, plan §13.2 T03).")
    lines.append("")
    lines.append("* routes: 27 = 25 `/api/*` + `POST /admin/login` + `POST /admin/logout`")
    lines.append("  (`GET /admin` is a page and is not part of the 27)")
    lines.append("* files: 27 JSON fixtures (`NN_<method>_<path>.json`), this README and")
    lines.append("  `manifest` (JSON document kept extensionless so that")
    lines.append("  `ls server/tests/fixtures/api/*.json` counts exactly the 27 fixtures)")
    lines.append("* image: `%s`" % manifest["capture"]["image"])
    lines.append("* stand: %s - one throwaway container, removed after every run"
                 % manifest["capture"]["base_url"])
    lines.append("* database: %s" % manifest["capture"]["database"])
    lines.append("* request order: %s" % manifest["capture"]["request_order"])
    lines.append("")
    lines.append("Regenerate / verify:")
    lines.append("")
    lines.append("```bash")
    lines.append("python3 deploy/scripts/capture_api_golden.py           # rewrite fixtures")
    lines.append("python3 deploy/scripts/capture_api_golden.py --check   # 27/27, diff 0, secrets 0")
    lines.append("python3 deploy/scripts/capture_api_golden.py --audit   # + synthetic DB invariants")
    lines.append("```")
    lines.append("")
    lines.append("## Route -> file -> auth -> sanitization (27/27)")
    lines.append("")
    lines.append("| # | Route | File | Auth | Sanitization |")
    lines.append("|---|---|---|---|---|")
    for entry in manifest["routes"]:
        lines.append("| %d | `%s` | `%s` | %s | %s |" % (
            entry["index"], entry["route"], entry["file"], entry["auth"],
            ", ".join(entry["sanitization"])))
    lines.append("")
    lines.append("## Fixture file shape")
    lines.append("")
    lines.append("* `index` - position in the fixed request order (1..27), also the file prefix")
    lines.append("* `route` - contract pattern (`DELETE /api/settings/{key}` shows the pattern,")
    lines.append("  `request.path` the concrete target used)")
    lines.append("* `request` - `method`, `path` (with query string), non-secret `headers`,")
    lines.append("  `cookies` (names only) and the normalized `body` (object or `null`)")
    lines.append("* `response` - `status`, ordered `[name, value]` header pairs and `body`: the")
    lines.append("  **legacy response bytes as a string**, normalized by the rules below")
    lines.append("* `auth` / `sanitization` - same values as the mapping table above")
    lines.append("* `note` - legacy quirk or intent of the route (optional)")
    lines.append("")
    lines.append("## Normalization rules")
    lines.append("")
    lines.append("| Id | Rule |")
    lines.append("|---|---|")
    for entry in manifest["normalization_rules"]:
        lines.append("| %s | %s |" % (entry["id"], entry["rule"]))
    lines.append("")
    lines.append("A route lists only the rules that actually fired for it; `R1` (the `Date`")
    lines.append("header) fires for every response.")
    lines.append("")
    lines.append("## Notes")
    lines.append("")
    lines.append("* Requests carry `User-Agent: %s` and are sent in the fixed order" % USER_AGENT)
    lines.append("  1..27; the synthetic credentials (`TOTP_SECRET`, admin password, device")
    lines.append("  token) are generated by the capture scripts and never read from any env file.")
    lines.append("* Fixture bodies are stored as strings so the legacy bytes - including the")
    lines.append("  `secuses` typo, the `text/plain` dump content type and the inverted")
    lines.append("  backup `export`/`import` naming - are captured as-is, not \"fixed\".")
    lines.append("* Mutating routes are part of the contract and are replayed on a fresh")
    lines.append("  synthetic database every run, which is what makes `--check` idempotent.")
    lines.append("")
    return "\n".join(lines) + "\n"


def json_text(value):
    return json.dumps(value, indent=2, ensure_ascii=False, sort_keys=False) + "\n"


def write_fixtures(out_dir, fixtures, manifest, readme):
    os.makedirs(out_dir, exist_ok=True)
    wanted = {fixture_name(ROUTES[i]) for i in range(len(fixtures))}
    wanted.add(MANIFEST_NAME)
    wanted.add(README_NAME)
    for name in os.listdir(out_dir):
        if name not in wanted:
            path = os.path.join(out_dir, name)
            if os.path.isfile(path):
                os.unlink(path)
    for fixture in fixtures:
        name = fixture_name(ROUTES[fixture["index"] - 1])
        with open(os.path.join(out_dir, name), "w", encoding="utf-8") as handle:
            handle.write(json_text(fixture))
    with open(os.path.join(out_dir, MANIFEST_NAME), "w", encoding="utf-8") as handle:
        handle.write(json_text(manifest))
    with open(os.path.join(out_dir, README_NAME), "w", encoding="utf-8") as handle:
        handle.write(readme)


def load_fixtures(out_dir):
    """Read a fixture directory back as {relative name: parsed object}."""
    data = {}
    if not os.path.isdir(out_dir):
        return data
    for name in sorted(os.listdir(out_dir)):
        path = os.path.join(out_dir, name)
        if not os.path.isfile(path):
            continue
        if not (name.endswith(".json") or name in (MANIFEST_NAME, README_NAME)):
            continue
        with open(path, "r", encoding="utf-8") as handle:
            text = handle.read()
        if name == README_NAME:
            data[name] = text
        else:
            data[name] = json.loads(text)
    return data


def diff_report(expected, actual, path="", limit=20, out=None):
    if out is None:
        out = []
    if len(out) >= limit:
        return out
    if type(expected) is not type(actual):
        out.append("%s: type %s != %s" % (path or "<root>",
                                          type(expected).__name__, type(actual).__name__))
        return out
    if isinstance(expected, dict):
        for key in list(expected) + [k for k in actual if k not in expected]:
            if key not in actual:
                out.append("%s.%s: missing in capture" % (path, key))
            elif key not in expected:
                out.append("%s.%s: unexpected in capture" % (path, key))
            else:
                diff_report(expected[key], actual[key], "%s.%s" % (path, key), limit, out)
            if len(out) >= limit:
                break
    elif isinstance(expected, list):
        if len(expected) != len(actual):
            out.append("%s: length %d != %d" % (path, len(expected), len(actual)))
        for index in range(min(len(expected), len(actual))):
            diff_report(expected[index], actual[index], "%s[%d]" % (path, index), limit, out)
            if len(out) >= limit:
                break
    elif expected != actual:
        out.append("%s: %r != %r" % (path, expected, actual))
    return out


# ---------------------------------------------------------------------------
# secret scan
# ---------------------------------------------------------------------------

FORBIDDEN_SUBSTRINGS = (
    "staging-data",
    "LAMPA_STAGING_",
    "env.staging",
    "lampa-staging",
    "/opt/lampa/data",
)
SECRET_VALUES = (
    ADMIN_PASSWORD,
    TOTP_SECRET,
    base64.b32encode(b"lampa-golden-totp-secret-t03-2026").decode("ascii"),
)


def scan_secrets(out_dir):
    """Return a list of findings: forbidden substrings, raw secrets, 64-hex."""
    findings = []
    for root, _dirs, files in os.walk(out_dir):
        for name in sorted(files):
            path = os.path.join(root, name)
            rel = os.path.relpath(path, out_dir)
            try:
                with open(path, "r", encoding="utf-8") as handle:
                    text = handle.read()
            except UnicodeDecodeError:
                findings.append("%s: not UTF-8" % rel)
                continue
            for needle in FORBIDDEN_SUBSTRINGS:
                if needle in text:
                    findings.append("%s: contains %r" % (rel, needle))
            for needle in SECRET_VALUES:
                if needle and needle in text:
                    findings.append("%s: contains a capture secret" % rel)
            # the pinned image digest is a public identifier, not a secret
            scan_text = text.replace(PINNED_IMAGE.split("@", 1)[1], "<image-digest>")
            for match in HEX64_RE.finditer(scan_text):
                findings.append("%s: 64-hex token at offset %d" % (rel, match.start()))
    return findings


def structural_secret_check(fixtures):
    """admin_password/admin_token must be masked in fixture 04 (GET /api/settings)."""
    findings = []
    for fixture in fixtures:
        if fixture["route"] != "GET /api/settings":
            continue
        try:
            rows = json.loads(fixture["response"]["body"])
        except ValueError:
            findings.append("GET /api/settings: body is not JSON")
            continue
        for row in rows:
            if isinstance(row, dict) and row.get("key") in SECRET_SETTING_KEYS:
                if row.get("value") != "***":
                    findings.append("GET /api/settings: %s is not masked" % row["key"])
    return findings


# ---------------------------------------------------------------------------
# modes
# ---------------------------------------------------------------------------

def new_work_dir(keep):
    os.makedirs(WORK_ROOT, exist_ok=True)
    work = tempfile.mkdtemp(prefix="capture-", dir=WORK_ROOT)
    return work


def cleanup_work(work, keep):
    if keep:
        log("work directory kept: %s" % work)
        return
    try:
        shutil.rmtree(work)
    except OSError as exc:
        log("WARNING: could not remove work directory %s: %s" % (work, exc))
    try:
        if not os.listdir(WORK_ROOT):
            os.rmdir(WORK_ROOT)
    except OSError:
        pass


def verify_fixtures(actual_dir_data, committed_dir_data, require_manifest=True):
    """Compare captured fixtures with the committed ones. Returns (ok, report)."""
    lines = []
    fixture_keys = sorted(k for k in actual_dir_data if k.endswith(".json"))
    committed_fixtures = sorted(k for k in committed_dir_data if k.endswith(".json"))
    if len(fixture_keys) != 27:
        lines.append("captured fixtures: %d (expected 27)" % len(fixture_keys))
    if len(committed_fixtures) != 27:
        lines.append("committed fixtures: %d (expected 27)" % len(committed_fixtures))
    only_actual = sorted(set(fixture_keys) - set(committed_fixtures))
    only_committed = sorted(set(committed_fixtures) - set(fixture_keys))
    if only_actual:
        lines.append("only in capture: %s" % ", ".join(only_actual))
    if only_committed:
        lines.append("only in committed fixtures: %s" % ", ".join(only_committed))

    diff_count = 0
    for name in sorted(set(fixture_keys) & set(committed_fixtures)):
        diffs = diff_report(committed_dir_data[name], actual_dir_data[name])
        if diffs:
            diff_count += 1
            lines.append("%s: %d difference(s)" % (name, len(diffs)))
            lines.extend("    %s" % item for item in diffs[:5])

    for name in ([MANIFEST_NAME, README_NAME] if require_manifest else []):
        if name not in actual_dir_data:
            lines.append("%s: missing from capture" % name)
        elif name not in committed_dir_data:
            lines.append("%s: missing from the committed fixtures" % name)
        elif actual_dir_data[name] != committed_dir_data[name]:
            diff_count += 1
            diffs = diff_report(committed_dir_data[name], actual_dir_data[name])
            lines.append("%s: %d difference(s)" % (name, len(diffs)))
            lines.extend("    %s" % item for item in diffs[:5])

    ok = not lines and diff_count == 0
    return ok, lines, diff_count


def mode_write(out_dir, keep):
    work = new_work_dir(keep)
    try:
        fixtures, summary, _db, base_url = capture(work)
        manifest = render_manifest(fixtures, base_url)
        readme = render_readme(manifest)
        write_fixtures(out_dir, fixtures, manifest, readme)
    finally:
        cleanup_work(work, keep)
    findings = scan_secrets(out_dir)
    findings.extend(structural_secret_check(fixtures))
    log("wrote %d fixtures + %s + %s to %s" % (
        len(fixtures), MANIFEST_NAME, README_NAME, os.path.relpath(out_dir, REPO_ROOT)))
    for item in findings:
        log("FAIL: %s" % item)
    if findings:
        log("WRITE FAIL (%d secret finding(s))" % len(findings))
        return 1
    log("secrets 0")
    return 0


def run_verified_check(out_dir, keep, want_audit):
    """Shared body of --check/--audit. Returns exit code."""
    findings = []
    mask_findings = []
    staging_before = db_probe(STAGING_DB)
    prod_before = db_probe(PROD_DB)
    builder_summary = None
    post_summary = None
    fixtures = []
    actual = {}
    diff_count = -1

    work = new_work_dir(keep)
    try:
        if want_audit:
            # fresh standalone build to audit the builder itself
            audit_db = os.path.join(work, "builder-audit.sqlite")
            builder_summary = build_synthetic_db(audit_db)
            if builder_summary["failures"]:
                findings.append("builder invariants: %s" % builder_summary["failures"])

        fixtures, summary, db_path, base_url = capture(work)
        manifest = render_manifest(fixtures, base_url)
        readme = render_readme(manifest)
        captured_dir = os.path.join(work, "fixtures")
        write_fixtures(captured_dir, fixtures, manifest, readme)

        actual = load_fixtures(captured_dir)
        committed = load_fixtures(out_dir)
        ok, report, diff_count = verify_fixtures(actual, committed)
        if not ok:
            findings.extend(report)

        mask_findings.extend(structural_secret_check(fixtures))

        if want_audit:
            import sqlite3 as _sqlite3
            conn = _sqlite3.connect("file:%s?mode=ro" % db_path, uri=True)
            try:
                post_summary = audit_post_capture(conn, db_path)
            finally:
                conn.close()
            for item in post_summary.get("failures", []):
                findings.append("post-capture DB: %s" % item)
    finally:
        cleanup_work(work, keep)

    staging_after = db_probe(STAGING_DB)
    prod_after = db_probe(PROD_DB)

    # invariance assertions (read-only mode=ro probes, no writes possible)
    if STAGING_DB is not None:
        if staging_before is None or staging_after is None:
            findings.append("staging DB probe unavailable before/after the run")
        elif staging_before["counts"] != staging_after["counts"]:
            for key in sorted(set(staging_before["counts"]) | set(staging_after["counts"])):
                before = staging_before["counts"].get(key)
                after = staging_after["counts"].get(key)
                if before != after:
                    findings.append("staging DB table %s changed: %s -> %s"
                                    % (key, before, after))

    secrets = scan_secrets(out_dir)
    findings.extend("secret scan: %s" % item for item in secrets)
    findings.extend("masking: %s" % item for item in mask_findings)

    # ---- report -----------------------------------------------------------
    mode_name = "audit" if want_audit else "check"
    json_files = len([k for k in actual if k.endswith(".json")])
    log("mode            %s" % mode_name)
    log("image           %s" % PINNED_IMAGE)
    log("stand           http://%s:%d (throwaway container; 9119 and 9118 untouched)"
        % (BASE_HOST, BASE_PORT))
    log("routes          %d/27 captured in fixed order" % len(fixtures))
    log("fixtures        %d/27 present, normalized diff %d" % (json_files, diff_count))
    log("secrets         %d findings" % len(secrets))
    if mask_findings:
        log("masking         %d finding(s)" % len(mask_findings))
    if builder_summary:
        log("builder audit   schema_version=%s journal_mode=%s profiles=%s bookmarks=%s "
            "timeline_events=%s settings=%s integrity=%s content_sha256=%s" % (
                builder_summary["schema_version"], builder_summary["journal_mode"],
                builder_summary["profiles"], builder_summary["bookmarks"],
                builder_summary["timeline_events"], builder_summary["settings"],
                builder_summary["integrity_check"],
                builder_summary["content_sha256"]))
    if post_summary:
        log("post-capture DB profiles=%s bookmarks=%s timeline_events=%s "
            "schema_version=%s journal_mode=%s integrity=%s" % (
                post_summary["profiles"], post_summary["bookmarks"],
                post_summary["timeline_events"], post_summary["schema_version"],
                post_summary["journal_mode"], post_summary["integrity_check"]))
    if STAGING_DB is None:
        log("staging rows    skipped (--no-staging-probe)")
    elif staging_before is not None and staging_after is not None:
        same = staging_before["counts"] == staging_after["counts"]
        log("staging rows    %s (before == after)" % ("unchanged" if same else "CHANGED"))
        log("  staging after:  %s" % probe_line(staging_after))
    else:
        log("staging rows    probe unavailable")
    log("  prod before:    %s" % probe_line(prod_before))
    log("  prod after:     %s" % probe_line(prod_after))

    if findings:
        for item in findings:
            log("FAIL: %s" % item)
        log("%s FAIL (%d finding(s))" % (mode_name.upper(), len(findings)))
        return 1
    if want_audit:
        log("AUDIT PASS (profiles=3, bookmarks<=20, timeline_events<=50, "
            "schema_version=7, journal_mode=delete, secrets 0, staging unchanged)")
    else:
        log("CHECK PASS (27/27 fixtures, normalized diff 0, secrets 0)")
    return 0


def audit_post_capture(conn, path):
    """Invariants asserted on the synthetic DB *after* the 27-route run."""
    import os as _os

    failures = []
    journal = str(conn.execute("PRAGMA journal_mode").fetchone()[0]).lower()
    if journal != "delete":
        failures.append("journal_mode=%s (expected delete)" % journal)
    version = conn.execute("SELECT version FROM schema_version WHERE id=1").fetchone()
    if not version or int(version[0]) != 7:
        failures.append("schema_version=%s (expected 7)" % (version and version[0]))
    tables = sorted(r[0] for r in conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table'"))
    if "_sqlx_migrations" in tables:
        failures.append("_sqlx_migrations table present")
    profiles = int(conn.execute("SELECT COUNT(*) FROM profiles").fetchone()[0])
    if profiles != 3:
        failures.append("profiles=%d (expected 3)" % profiles)
    bookmarks = int(conn.execute("SELECT COUNT(*) FROM bookmarks").fetchone()[0])
    if bookmarks > 20:
        failures.append("bookmarks=%d (expected <=20)" % bookmarks)
    timeline_events = int(
        conn.execute("SELECT COUNT(*) FROM timeline_events").fetchone()[0])
    if timeline_events > 50:
        failures.append("timeline_events=%d (expected <=50)" % timeline_events)
    integrity = str(conn.execute("PRAGMA integrity_check").fetchone()[0])
    if integrity.lower() != "ok":
        failures.append("integrity_check=%s" % integrity)
    for suffix in ("-wal", "-shm"):
        if _os.path.exists(path + suffix):
            failures.append("sidecar file present: %s%s" % (path, suffix))
    return {
        "profiles": profiles,
        "bookmarks": bookmarks,
        "timeline_events": timeline_events,
        "settings": int(conn.execute("SELECT COUNT(*) FROM settings").fetchone()[0]),
        "schema_version": int(version[0]) if version else None,
        "journal_mode": journal,
        "integrity_check": integrity,
        "failures": failures,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Capture/check/audit the 27 legacy golden fixtures (T03).")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--write", action="store_true",
                      help="regenerate the fixtures (default when no mode is given)")
    mode.add_argument("--check", action="store_true",
                      help="re-capture on a fresh synthetic DB and diff against the fixtures")
    mode.add_argument("--audit", action="store_true",
                      help="--check plus synthetic DB invariants and staging invariance")
    parser.add_argument("--out", default=FIXTURE_DIR,
                        help="fixture directory (default: server/tests/fixtures/api)")
    parser.add_argument("--keep-work", action="store_true",
                        help="keep the throwaway work directory for debugging")
    parser.add_argument("--no-staging-probe", action="store_true",
                        help="skip the read-only staging row-count invariance probe")
    args = parser.parse_args(argv)

    global STAGING_DB
    if args.no_staging_probe:
        STAGING_DB = None
    out_dir = os.path.abspath(args.out)

    if args.check or args.audit:
        if not os.path.isdir(out_dir):
            die("fixture directory does not exist: %s" % out_dir, 2)
        return run_verified_check(out_dir, args.keep_work, args.audit)

    return mode_write(out_dir, args.keep_work)


if __name__ == "__main__":
    sys.exit(main())
