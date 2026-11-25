#!/usr/bin/env bash
# smoke.sh - 9-check smoke test for the Node staging stand on 127.0.0.1:9119.
#
# Checks (S11 §4.1 / §9):
#   1. GET /                         -> 200 HTML shell (BrowserSync/Lampa)
#   2. POST /admin/login             -> 200 success + auth_token cookie (test password)
#   3. GET /api/value?key=...        -> 200 JSON (admin cookie)
#   4. GET /api/profiles/all         -> 200, 3 profiles, main profile present
#   5. GET /api/admin/devices        -> 200 JSON list (admin cookie)
#   6. GET /api/bookmarks/dump       -> 200 text/plain, body is valid JSON
#   7. GET /api/timeline/changelog   -> 200 JSON with numeric version
#   8. POST /api/pin/verify          -> 200 ok=true with staging TOTP secret
#   9. GET /api/value via the static test device_token -> 200 (guard negative
#      control without any cookie must be 401 token_required)
#
# Idempotent: all requests are reads except the PIN verify, which only refreshes
# the dedicated "staging-pin-probe" device row with a fresh random token. The
# static test device token from the env file is never invalidated, so the script
# can be re-run any number of times.
#
# Secrets are read from --env-file (mode 0600) and never printed or logged.
# Exit codes: 0 all checks pass, 1 at least one check failed, 2 setup error.
#
# Usage:
#   bash deploy/scripts/smoke.sh \
#     --base http://127.0.0.1:9119 \
#     --env-file /opt/lampa/staging-data/env.staging
set -euo pipefail

BASE=http://127.0.0.1:9119
ENV_FILE=/opt/lampa/staging-data/env.staging
JSON_OUT=
TIMEOUT=20
READY_TIMEOUT=60

usage() {
    sed -n '2,/^set -euo pipefail/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
}

die() {
    echo "smoke: ERROR: $*" >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --base)
            [ $# -ge 2 ] || die "--base needs a value"
            BASE=$2
            shift 2
            ;;
        --env-file)
            [ $# -ge 2 ] || die "--env-file needs a value"
            ENV_FILE=$2
            shift 2
            ;;
        --json-out)
            [ $# -ge 2 ] || die "--json-out needs a value"
            JSON_OUT=$2
            shift 2
            ;;
        --timeout)
            [ $# -ge 2 ] || die "--timeout needs a value"
            TIMEOUT=$2
            shift 2
            ;;
        --ready-timeout)
            [ $# -ge 2 ] || die "--ready-timeout needs a value"
            READY_TIMEOUT=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1 (try --help)"
            ;;
    esac
done

[ -r "${ENV_FILE}" ] || die "cannot read env file: ${ENV_FILE} (run deploy/scripts/prepare_staging_db.py first)"

exec python3 - "${BASE}" "${ENV_FILE}" "${JSON_OUT:-}" "${TIMEOUT}" "${READY_TIMEOUT}" <<'PY'
import base64
import hashlib
import hmac
import http.cookiejar
import json
import struct
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = sys.argv[1].rstrip("/")
ENV_FILE = sys.argv[2]
JSON_OUT = sys.argv[3] or None
TIMEOUT = float(sys.argv[4])
READY_TIMEOUT = float(sys.argv[5])
EXPECTED_PROFILES = 3
PIN_DEVICE_ID = "staging-pin-probe"
PIN_PROBE_UA = "lampa-staging-smoke"


def read_env(path):
    values = {}
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, _, value = line.partition("=")
            values[key.strip()] = value.strip()
    return values


ENV = read_env(ENV_FILE)
required = ["TOTP_SECRET", "LAMPA_STAGING_ADMIN_PASSWORD", "LAMPA_STAGING_DEVICE_TOKEN"]
missing = [key for key in required if not ENV.get(key)]
if missing:
    print("smoke: ERROR: " + ENV_FILE + " is missing " + ", ".join(missing), file=sys.stderr)
    raise SystemExit(2)

ADMIN_PASSWORD = ENV["LAMPA_STAGING_ADMIN_PASSWORD"]
TOTP_SECRET = ENV["TOTP_SECRET"]
DEVICE_TOKEN = ENV["LAMPA_STAGING_DEVICE_TOKEN"]
DEVICE_ID = ENV.get("LAMPA_STAGING_DEVICE_ID") or "staging-smoke-device"


def totp(secret, digits=6, period=30):
    padding = "=" * ((8 - len(secret) % 8) % 8)
    key = base64.b32decode(secret.upper() + padding)
    counter = int(time.time()) // period
    mac = hmac.new(key, struct.pack(">Q", counter), hashlib.sha1).digest()
    offset = mac[-1] & 0x0F
    code = (struct.unpack(">I", mac[offset:offset + 4])[0] & 0x7FFFFFFF) % (10 ** digits)
    return str(code).zfill(digits)


class Client:
    def __init__(self, base):
        self.base = base
        self.jar = http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(self.jar))

    def cookie_names(self):
        return sorted(cookie.name for cookie in self.jar)

    def request(self, method, path, headers=None, json_body=None, cookie=None):
        hdrs = {"User-Agent": "lampa-staging-smoke/1"}
        hdrs.update(headers or {})
        data = None
        if json_body is not None:
            data = json.dumps(json_body).encode()
            hdrs["Content-Type"] = "application/json"
        if cookie:
            hdrs["Cookie"] = cookie
        req = urllib.request.Request(self.base + path, data=data, headers=hdrs, method=method)
        start = time.perf_counter()
        try:
            with self.opener.open(req, timeout=TIMEOUT) as resp:
                status, resp_headers, body = resp.status, resp.headers, resp.read()
        except urllib.error.HTTPError as exc:
            status, resp_headers, body = exc.code, exc.headers, exc.read()
        except Exception as exc:
            return None, {}, str(exc).encode(), (time.perf_counter() - start) * 1000.0
        return status, resp_headers, body, (time.perf_counter() - start) * 1000.0


def as_json(body):
    try:
        return json.loads(body)
    except Exception:
        return None


def short(value, limit=120):
    text = str(value)
    return text if len(text) <= limit else text[: limit - 3] + "..."


client = Client(BASE)
state = {"profiles": [], "main_profile": None}
results = []


def wait_ready(timeout_s):
    deadline = time.time() + timeout_s
    start = time.perf_counter()
    last_error = None
    while time.time() < deadline:
        status, headers, body, ms = client.request("GET", "/")
        if status is not None:
            return True, round((time.perf_counter() - start) * 1000.0, 1), status
        last_error = body.decode("utf-8", "replace")[:100]
        time.sleep(0.2)
    return False, round((time.perf_counter() - start) * 1000.0, 1), last_error


ready, ready_ms, ready_detail = wait_ready(READY_TIMEOUT)
if not ready:
    print("smoke: ERROR: " + BASE + " is not answering after " + str(READY_TIMEOUT) +
          " s (last: " + str(ready_detail) + ")", file=sys.stderr)
    print("smoke: hint: docker ps --filter name=lampa-staging; docker logs --tail 50 lampa-staging",
          file=sys.stderr)
    raise SystemExit(2)


def run(name, fn):
    start = time.perf_counter()
    try:
        ok, detail = fn()
    except Exception as exc:
        ok, detail = False, "exception: " + short(exc)
    elapsed = (time.perf_counter() - start) * 1000.0
    results.append({"name": name, "ok": bool(ok), "ms": round(elapsed, 1),
                    "detail": detail})
    return ok


def check_root():
    status, headers, body, ms = client.request("GET", "/")
    lowered = body.lower()
    ok = status == 200 and (b"<html" in lowered or b"lampa" in lowered)
    return ok, "status=" + str(status) + " bytes=" + str(len(body)) + " html=" + \
        ("yes" if b"<html" in lowered else "no")


def check_login():
    status, headers, body, ms = client.request(
        "POST", "/admin/login", json_body={"password": ADMIN_PASSWORD})
    payload = as_json(body) or {}
    cookie = "auth_token" in client.cookie_names()
    ok = status == 200 and payload.get("success") is True and cookie
    return ok, "status=" + str(status) + " success=" + str(payload.get("success")).lower() + \
        " cookie=" + ("yes" if cookie else "no")


def check_value():
    status, headers, body, ms = client.request("GET", "/api/value?key=head_broadcast")
    payload = as_json(body)
    ok = status == 200 and (payload is not None or body.strip() == b"null")
    kind = "null" if payload is None else type(payload).__name__
    return ok, "status=" + str(status) + " json=" + kind + " bytes=" + str(len(body))


def check_profiles():
    status, headers, body, ms = client.request("GET", "/api/profiles/all")
    payload = as_json(body) or {}
    profiles = payload.get("profiles") or []
    main = next((p for p in profiles if p.get("main") or p.get("is_main")), None)
    state["profiles"] = profiles
    state["main_profile"] = main
    ok = status == 200 and payload.get("secuses") is True and \
        len(profiles) == EXPECTED_PROFILES and main is not None
    return ok, "status=" + str(status) + " profiles=" + str(len(profiles)) + \
        " main=" + str(main.get("id") if main else None) + \
        " names=" + ",".join(str(p.get("name")) for p in profiles)


def profile_id():
    main = state.get("main_profile")
    if main and main.get("id") is not None:
        return int(main["id"])
    profiles = state.get("profiles") or []
    return int(profiles[0]["id"]) if profiles else 1


def check_devices():
    status, headers, body, ms = client.request("GET", "/api/admin/devices")
    payload = as_json(body)
    ok = status == 200 and isinstance(payload, list)
    statuses = sorted({d.get("status") for d in payload}) if isinstance(payload, list) else []
    count = len(payload) if isinstance(payload, list) else 0
    return ok, "status=" + str(status) + " devices=" + str(count) + \
        " statuses=" + ",".join(str(s) for s in statuses)


def check_bookmarks():
    pid = profile_id()
    status, headers, body, ms = client.request(
        "GET", "/api/bookmarks/dump", headers={"profile": str(pid)})
    payload = as_json(body) or {}
    content_type = headers.get("Content-Type", "") if headers else ""
    bookmarks = payload.get("bookmarks")
    version = payload.get("version")
    ok = status == 200 and content_type.startswith("text/plain") and \
        isinstance(bookmarks, list) and isinstance(version, int)
    return ok, "status=" + str(status) + " ct=" + (content_type or "?") + \
        " bookmarks=" + str(len(bookmarks) if isinstance(bookmarks, list) else "?") + \
        " version=" + str(version) + " bytes=" + str(len(body))


def check_timeline():
    pid = profile_id()
    status, headers, body, ms = client.request(
        "GET", "/api/timeline/changelog?since=999999999", headers={"profile": str(pid)})
    payload = as_json(body) or {}
    version = payload.get("version")
    ok = status == 200 and isinstance(version, int)
    return ok, "status=" + str(status) + " version=" + str(version) + \
        " events=" + str(len(payload.get("timelines") or {})) + " bytes=" + str(len(body))


def check_pin():
    code = totp(TOTP_SECRET)
    status, headers, body, ms = client.request(
        "POST", "/api/pin/verify",
        json_body={"pin": code, "device_id": PIN_DEVICE_ID})
    payload = as_json(body) or {}
    issued = bool(payload.get("device_token"))
    ok = status == 200 and payload.get("ok") is True and issued
    return ok, "status=" + str(status) + " ok=" + str(payload.get("ok")).lower() + \
        " token_issued=" + ("yes" if issued else "no") + " tol=window1"


def check_device_token():
    anon = Client(BASE)
    ctrl_status, ctrl_headers, ctrl_body, ctrl_ms = anon.request(
        "GET", "/api/value?key=menu_data")
    ctrl_error = (as_json(ctrl_body) or {}).get("error")
    status, headers, body, ms = anon.request(
        "GET", "/api/value?key=menu_data",
        headers={"User-Agent": PIN_PROBE_UA},
        cookie="device_token=" + DEVICE_TOKEN)
    payload = as_json(body)
    ok = (status == 200 and ctrl_status == 401 and ctrl_error == "token_required" and
          b"invalid_token" not in body and
          (payload is not None or body.strip() == b"null"))
    return ok, "status=" + str(status) + " guard_without_cookie=" + str(ctrl_status) + \
        " device=" + DEVICE_ID


checks = [
    ("GET / (HTML shell)", check_root),
    ("POST /admin/login (test password)", check_login),
    ("GET /api/value?key=head_broadcast", check_value),
    ("GET /api/profiles/all (3 profiles)", check_profiles),
    ("GET /api/admin/devices", check_devices),
    ("GET /api/bookmarks/dump (text/plain)", check_bookmarks),
    ("GET /api/timeline/changelog", check_timeline),
    ("POST /api/pin/verify (staging TOTP)", check_pin),
    ("GET /api/value (test device_token)", check_device_token),
]

started = time.time()
for name, fn in checks:
    run(name, fn)

total_ms = round((time.time() - started) * 1000.0, 1)
width = max(len(name) for name, _ in checks)
print("smoke: Node staging @ " + BASE + " | env file " + ENV_FILE + " (values not printed)")
print("smoke: ready in " + str(ready_ms) + " ms (http=" + str(ready_detail) + ")")
print("")
print("  #  " + "check".ljust(width) + "  result  http      ms  detail")
print("  -- " + "-" * width + "  ------  ----  ------  ------")
for index, item in enumerate(results, start=1):
    name = item["name"]
    status_text = item["detail"].split("status=")[1].split()[0] if "status=" in item["detail"] else "?"
    print("  " + str(index).rjust(1) + "  " + name.ljust(width) + "  " +
          ("PASS" if item["ok"] else "FAIL").ljust(6) + "  " +
          status_text.rjust(4) + "  " +
          str(item["ms"]).rjust(6) + "  " + item["detail"])

passed = sum(1 for item in results if item["ok"])
failed = len(results) - passed
summary = {
    "base": BASE,
    "env_file": ENV_FILE,
    "ok": failed == 0,
    "passed": passed,
    "total": len(results),
    "total_ms": total_ms,
    "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "checks": results,
}
print("")
if failed == 0:
    print("smoke: " + str(passed) + "/" + str(len(results)) + " PASS, total " +
          str(total_ms) + " ms, " + summary["at"] + " -> exit 0")
else:
    print("smoke: " + str(passed) + "/" + str(len(results)) + " PASS, " + str(failed) +
          " FAIL, total " + str(total_ms) + " ms, " + summary["at"] + " -> exit 1")

if JSON_OUT:
    with open(JSON_OUT, "w", encoding="utf-8") as fh:
        json.dump(summary, fh, ensure_ascii=False, indent=1, sort_keys=True)

raise SystemExit(0 if failed == 0 else 1)
PY
