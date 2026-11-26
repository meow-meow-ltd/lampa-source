#!/usr/bin/env python3
"""T13 — differential parity: pinned Node implementation vs Rust server.

Runs the fixed 27-route legacy API flow (request order, headers, cookies and
TOTP handling copied from ``deploy/scripts/capture_api_golden.py``, which is
imported read-only) against two stands:

* **Node**  — the pinned image ``ghcr.io/meow-meow-ltd/lampa-source@sha256:...``
  as a throwaway container on ``127.0.0.1:9122`` with synthetic DB copy #1;
* **Rust**  — the release binary ``lampa-server`` on ``127.0.0.1:9123`` with
  synthetic DB copy #2.

Both DB copies are built from scratch by ``deploy/scripts/make_synthetic_db.py``
(temp dirs only — never the prod DB, never the staging DB) and must be
byte-identical before the flow starts.

Every response is normalized with the T03 rules R1..R9 (see
``docs/w0-notes/T03.md`` and the capture script's ``RULES`` table) before the
comparison of **status, content-type, cache-control and body**.  The
``device_access`` content of route 02 (``GET /api/admin/devices``) is excluded
from the contract (plan §13.3, §4.2) and reported as informational only.
Cache-control: Node never sends it; Rust may add exactly ``no-store`` on
``/api/*`` (plan §3.6 п.8, documented in T08 §5) — that direction is
sanctioned, any other difference fails.

Ports 9118 (prod) and 9119 (staging) are never bound or probed; no prod or
staging file is read or written.  Containers and temp dirs are cleaned up in
``finally`` on every path; re-running the script is idempotent.

Exit codes: 0 = parity, 1 = mismatch, 2 = setup error (missing/stale Rust
binary, missing pinned image, busy port, failed readiness, ...).  The host has
no Rust toolchain, so a missing binary prints the builder recipe instead of
building.
"""

import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import capture_api_golden as cap  # noqa: E402  read-only reuse of the T03 contract

NODE_PORT = 9122                      # throwaway Node stand; 9119 = staging
RUST_PORT = 9123                      # throwaway Rust stand
CONTAINER_PREFIX = "api-parity-node-"
DEVICE_ACCESS_ROUTE = 2                # GET /api/admin/devices: body excluded
SANCTIONED_CC = "no-store"             # the only cache-control Rust may add
REPO_ROOT = cap.REPO_ROOT
# Temp data MUST live on the shared /opt/lampa filesystem: the node container
# bind-mounts this path from the dockerd mount namespace, and a session-private
# /tmp (tmpfs) is invisible to the daemon — that silently hands the container
# an empty directory.  /opt/lampa/tmp is the location sanctioned for this task.
TEMP_BASE = os.environ.get("LAMPA_PARITY_TMPDIR") or "/opt/lampa/tmp"
RUST_BIN = os.environ.get("LAMPA_RUST_SERVER") or os.path.join(
    os.path.dirname(REPO_ROOT), ".cargo-target", "release", "lampa-server")
BUILD_HINT = (
    "cd /opt/lampa/source && docker run --rm --user \"$(id -u):$(id -g)\" "
    "-v /opt/lampa/source:/work -v /opt/lampa/poc/cargo-cache:/cargo "
    "-v /opt/lampa/.cargo-target:/target -e CARGO_HOME=/cargo "
    "-e CARGO_TARGET_DIR=/target -w /work/server lampa-rust-builder:1.100.0 "
    "bash -c 'cargo build --release --locked --offline'"
)


def log(message):
    print("api_parity_check: %s" % message, flush=True)


def fail(message, code=2):
    print("api_parity_check: ERROR: %s" % message, file=sys.stderr, flush=True)
    raise SystemExit(code)


def port_free(port):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        sock.bind(("127.0.0.1", port))
        return True
    except OSError:
        return False
    finally:
        sock.close()


def check_docker_image():
    """Fail closed unless the pinned image is in the local store (never pull)."""
    if not any(os.access(os.path.join(path, "docker"), os.X_OK)
               for path in os.environ.get("PATH", "").split(os.pathsep) if path):
        fail("docker is not available in PATH; the pinned Node stand cannot start")
    proc = subprocess.run(
        ["docker", "image", "inspect", cap.PINNED_IMAGE],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, universal_newlines=True,
    )
    if proc.returncode != 0:
        fail("pinned image %s is not in the local store (this script never "
             "pulls): %s" % (cap.PINNED_IMAGE, proc.stderr.strip()[:300]))


def rust_binary_check():
    if not os.path.isfile(RUST_BIN) or not os.access(RUST_BIN, os.X_OK):
        fail("Rust release binary not found: %s\n  host has no Rust toolchain; "
             "build it with:\n  %s" % (RUST_BIN, BUILD_HINT))
    newest, newest_path = 0.0, ""
    for root_dir, _dirs, files in os.walk(os.path.join(REPO_ROOT, "server", "src")):
        for name in files:
            if name.endswith(".rs"):
                path = os.path.join(root_dir, name)
                mtime = os.path.getmtime(path)
                if mtime > newest:
                    newest, newest_path = mtime, path
    for rel in ("Cargo.toml", "Cargo.lock"):
        path = os.path.join(REPO_ROOT, "server", rel)
        if os.path.isfile(path):
            mtime = os.path.getmtime(path)
            if mtime > newest:
                newest, newest_path = mtime, path
    if newest > os.path.getmtime(RUST_BIN):
        fail("Rust release binary is stale (newer input: %s); rebuild it with:\n  %s"
             % (newest_path, BUILD_HINT))


def sha256_file(path):
    import hashlib
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build_db(path, label):
    """Build one synthetic v7 DB via make_synthetic_db.py; validate invariants."""
    try:
        summary = cap.build_synthetic_db(path)
    except RuntimeError as exc:
        fail("%s DB build failed: %s" % (label, exc))
    if summary.get("failures"):
        fail("%s builder invariants: %s" % (label, summary["failures"]))
    if summary.get("schema_version") != 7 or summary.get("journal_mode") != "delete":
        fail("%s unexpected DB shape: schema_version=%s journal_mode=%s"
             % (label, summary.get("schema_version"), summary.get("journal_mode")))
    return summary


def sweep_leftover_containers():
    """Remove containers left behind by a crashed earlier run (our prefix only)."""
    out = subprocess.run(
        ["docker", "ps", "-aq", "--filter", "name=^/%s" % CONTAINER_PREFIX],
        stdout=subprocess.PIPE, universal_newlines=True,
    ).stdout.split()
    if out:
        subprocess.run(["docker", "rm", "-f"] + out,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        log("cleanup         removed %d leftover container(s) from a previous run"
            % len(out))


def start_node(db_dir):
    """Throwaway pinned-image container on 127.0.0.1:9122 (T03 mechanics)."""
    name = "%s%d-%04x" % (CONTAINER_PREFIX, os.getpid(),
                          int.from_bytes(os.urandom(2), "big"))
    try:
        cap.run(["docker", "create", "--name", name,
                 "-p", "127.0.0.1:%d:3000" % NODE_PORT,
                 "-e", "DB_PATH=/data/database.sqlite",
                 "-e", "HOST=0.0.0.0",
                 "-e", "PORT=3000",
                 "-e", "TOTP_SECRET=%s" % cap.TOTP_SECRET,
                 "-v", "%s:/data" % db_dir,
                 cap.PINNED_IMAGE])
        cap.run(["docker", "start", name])
    except RuntimeError as exc:
        subprocess.run(["docker", "rm", "-f", name],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        fail("node container failed to start: %s" % exc)
    return name


def stop_node(name):
    """Remove our container; returns an error string or None."""
    if not name:
        return None
    subprocess.run(["docker", "rm", "-f", "-v", name],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                   universal_newlines=True)
    left = subprocess.run(
        ["docker", "ps", "-a", "--filter", "name=^/%s$" % name,
         "--format", "{{.Names}}"],
        stdout=subprocess.PIPE, universal_newlines=True,
    ).stdout.strip()
    if left:
        return "container %s could not be removed (leftover: %s)" % (name, left)
    return None


def remove_tree(path):
    try:
        shutil.rmtree(path)
    except FileNotFoundError:
        return None
    except PermissionError:
        for root_dir, dirs, _files in os.walk(path):
            for name in dirs:
                try:
                    os.chmod(os.path.join(root_dir, name), 0o700)
                except OSError:
                    pass
        shutil.rmtree(path, ignore_errors=True)
    return None if not os.path.exists(path) else "temp dir %s survived cleanup" % path


def file_tail(path, lines=25):
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            return "".join(handle.readlines()[-lines:])
    except OSError:
        return "<no log>"


def wait_ready(port, path, label, timeout, proc=None, log_path=None):
    """Poll GET path until `expect` (200); fail closed on timeout/early exit."""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        if proc is not None and proc.poll() is not None:
            fail("%s exited early (code %s); log tail:\n%s"
                 % (label, proc.returncode, file_tail(log_path or "")))
        cap.BASE_PORT = port
        cap.BASE_HOST = "127.0.0.1"
        try:
            result = cap.http_request("GET", path, timeout=5)
            if result["status"] == 200:
                return
            last = "status %s" % result["status"]
        except Exception as exc:  # noqa: BLE001 - readiness probing
            last = str(exc)
        time.sleep(0.25)
    fail("%s not ready within %ss: %s" % (label, timeout, last))


def run_side(port, label):
    """Setup login + the fixed 1..27 flow against one stand (T03 sequence)."""
    prev_host, prev_port = cap.BASE_HOST, cap.BASE_PORT
    cap.BASE_HOST, cap.BASE_PORT = "127.0.0.1", port
    meta = {"setup": 0, "pin": False}
    rows = []
    try:
        ctx = cap.Ctx()
        try:
            login = cap.http_request("POST", "/admin/login",
                                     body={"password": cap.ADMIN_PASSWORD})
        except Exception as exc:  # noqa: BLE001 - transport diagnostics
            fail("%s stand: setup POST /admin/login failed: %s" % (label, exc))
        meta["setup"] = login["status"]
        for name, value in login["header_items"]:
            if name.lower() == "set-cookie" and value.startswith("auth_token="):
                ctx.auth_token = value.split(";", 1)[0].split("=", 1)[1]
        for spec in cap.ROUTES:
            headers = dict(spec.get("headers") or {})
            if spec["auth"] == cap.AUTH_ADMIN and ctx.auth_token:
                headers["Cookie"] = "auth_token=%s" % ctx.auth_token
            elif spec["auth"] == cap.AUTH_DEVICE and ctx.device_token:
                headers["Cookie"] = "device_token=%s" % ctx.device_token
            body = spec.get("body")
            if callable(body):
                body = body(ctx)          # fresh TOTP code, same secret both stands
            try:
                result = cap.http_request(spec["method"], spec["path"],
                                          body=body, headers=headers)
            except Exception as exc:  # noqa: BLE001 - transport diagnostics
                fail("%s stand: %s %s failed mid-flow: %s"
                     % (label, spec["method"], spec["path"], exc))
            if spec["index"] == 1:
                try:
                    raw = json.loads(result["raw_body"])
                except ValueError:
                    raw = {}
                ctx.device_token = raw.get("device_token") or ""
                meta["pin"] = bool(raw.get("ok")) and bool(raw.get("device_token"))
            fired = set()
            headers_out = cap.mask_headers(result["header_items"], fired)
            body_out = cap.mask_body(result["body"], fired)
            headers_out = cap.finish_headers(headers_out, body_out, fired)
            rows.append({
                "index": spec["index"], "route": spec["route"],
                "expect": spec["expect"], "status": result["status"],
                "headers": headers_out, "body": body_out, "fired": fired,
            })
        return rows, meta
    finally:
        cap.BASE_HOST, cap.BASE_PORT = prev_host, prev_port


def header_value(headers, wanted):
    for name, value in headers:
        if name.lower() == wanted:
            return value.strip()
    return None


def body_diffs(node_masked, rust_masked):
    """T03 --check semantics: structural diff of parsed JSON, else raw strings."""
    try:
        node_val = json.loads(node_masked)
        node_json = True
    except (ValueError, TypeError):
        node_json = False
    try:
        rust_val = json.loads(rust_masked)
        rust_json = True
    except (ValueError, TypeError):
        rust_json = False
    if node_json and rust_json:
        return cap.diff_report(node_val, rust_val)
    return cap.diff_report(node_masked, rust_masked)


def cc_class(node_cc, rust_cc):
    """'=' | '+no-store' (sanctioned, plan §3.6 п.8) | 'DIFF' (failure)."""
    if node_cc == rust_cc:
        return "="
    if node_cc is None and rust_cc is not None and rust_cc.lower() == SANCTIONED_CC:
        return "+no-store"
    return "DIFF"


def compare_and_report(node_rows, rust_rows, node_meta, rust_meta):
    """Compare the two normalized runs, print the 27-row report, return 0/1."""
    if len(node_rows) != 27 or len(rust_rows) != 27:
        fail("internal: expected 27 responses per stand, got node=%d rust=%d"
             % (len(node_rows), len(rust_rows)))
    fired = set()
    for row in node_rows + rust_rows:
        fired |= row["fired"]
    log("flow            node setup=%d pin=%s | rust setup=%d pin=%s"
        % (node_meta["setup"], "yes" if node_meta["pin"] else "no",
           rust_meta["setup"], "yes" if rust_meta["pin"] else "no"))
    log("  #  %-31s %3s %3s  %-4s %-11s %-8s %s"
        % ("route", "node", "rust", "ct", "cc", "body", "verdict"))
    failures = 0
    status_issues = ct_issues = cc_issues = body_issues = 0
    sanctioned = []
    details = []
    excl_note = "route %d body not compared (device_access excluded)" % DEVICE_ACCESS_ROUTE
    for node, rust in zip(node_rows, rust_rows):
        if node["index"] != rust["index"] or node["route"] != rust["route"]:
            fail("internal: route order diverged at index %s" % node["index"])
        idx, route, expect = node["index"], node["route"], node["expect"]
        status_ok = node["status"] == rust["status"] == expect
        node_ct = header_value(node["headers"], "content-type")
        rust_ct = header_value(rust["headers"], "content-type")
        ct_ok = node_ct == rust_ct
        node_cc = header_value(node["headers"], "cache-control")
        rust_cc = header_value(rust["headers"], "cache-control")
        cc = cc_class(node_cc, rust_cc)
        if cc == "+no-store":
            sanctioned.append(idx)
        diffs = body_diffs(node["body"], rust["body"])
        excluded = idx == DEVICE_ACCESS_ROUTE
        if excluded:
            body_ok = True
            body_col = "EXCL"
            paths = ", ".join(d[:60] for d in diffs[:3])
            excl_note = ("route %d (GET /api/admin/devices) body excluded from "
                         "the contract; informational masked diff=%d%s"
                         % (idx, len(diffs), " (%s)" % paths if diffs else ""))
        else:
            body_ok = not diffs
            body_col = "diff=%d" % len(diffs)
        issues = []
        if not status_ok:
            status_issues += 1
            issues.append("status node=%d rust=%d (expected %d)"
                          % (node["status"], rust["status"], expect))
        if not ct_ok:
            ct_issues += 1
            issues.append("content-type node=%r rust=%r" % (node_ct, rust_ct))
        if cc == "DIFF":
            cc_issues += 1
            issues.append("cache-control node=%r rust=%r" % (node_cc, rust_cc))
        if not body_ok:
            body_issues += 1
            issues.append("body %d difference(s)" % len(diffs))
            issues.extend("    %s" % item for item in diffs[:5])
            issues.append("    node: %s" % node["body"][:220])
            issues.append("    rust: %s" % rust["body"][:220])
        ok = not issues
        if not ok:
            failures += 1
            details.append("%02d %s: %s" % (idx, route, "; ".join(
                i.strip().split("\n")[0] for i in issues[:1])))
            details.extend(i for i in issues[1:])
        log("%2d  %-31s %3d %3d  %-4s %-11s %-8s %s"
            % (idx, route, node["status"], rust["status"],
               "ok" if ct_ok else "DIFF", cc, body_col,
               "PASS" if ok else "FAIL"))
    log("routes          27 compared: status-issue=%d content-type-issue=%d "
        "cache-control-issue=%d body-issue=%d"
        % (status_issues, ct_issues, cc_issues, body_issues))
    if sanctioned:
        log("cache-control   sanctioned +%s (plan §3.6 п.8) on %d route(s): %s"
            % (SANCTIONED_CC, len(sanctioned),
               ",".join(str(item) for item in sanctioned)))
    else:
        log("cache-control   no sanctioned deltas observed")
    log("device_access   %s" % excl_note)
    log("masking         rules fired: %s"
        % ",".join(rule for rule in cap.RULE_IDS if rule in fired))
    if failures:
        for line in details:
            log("FAIL: %s" % line)
        log("RESULT          %d/27 PASS — diff %d, failures %d"
            % (27 - failures, failures, failures))
        return 1
    log("RESULT          27/27 PASS — diff 0, failures 0")
    return 0


def main():
    log("image           %s" % cap.PINNED_IMAGE)
    check_docker_image()
    rust_binary_check()
    for port in (NODE_PORT, RUST_PORT):
        if not port_free(port):
            fail("127.0.0.1:%d is already in use - refusing to touch any other "
                 "port (9118 prod / 9119 staging are never used by this script)"
                 % port)
    sweep_leftover_containers()
    created_base = not os.path.isdir(TEMP_BASE)
    try:
        os.makedirs(TEMP_BASE, mode=0o755, exist_ok=True)
    except OSError as exc:
        fail("cannot create temp base %s: %s" % (TEMP_BASE, exc))
    tmp = tempfile.mkdtemp(prefix="api-parity-", dir=TEMP_BASE)
    os.chmod(tmp, 0o755)               # dockerd mounts it; contents stay non-secret
    node_dir = os.path.join(tmp, "node")
    rust_dir = os.path.join(tmp, "rust")
    os.makedirs(node_dir)
    os.makedirs(rust_dir)
    node_db = os.path.join(node_dir, "database.sqlite")
    rust_db = os.path.join(rust_dir, "database.sqlite")
    container = None
    rust_proc = None
    rust_log = None
    rust_log_path = os.path.join(tmp, "rust-server.log")
    cleanup_errors = []
    rc = 2
    try:
        summary1 = build_db(node_db, "node")
        summary2 = build_db(rust_db, "rust")
        comparable1 = {k: v for k, v in summary1.items() if k != "path"}
        comparable2 = {k: v for k, v in summary2.items() if k != "path"}
        if comparable1 != comparable2:
            fail("the two builder summaries differ (copies must be identical): "
                 "%s != %s" % (comparable1, comparable2))
        if sha256_file(node_db) != sha256_file(rust_db):
            fail("the two synthetic DB copies are not byte-identical "
                 "(builder determinism broken)")
        log("db              schema_version=%s journal=%s content_sha256=%s "
            "byte-identical=yes"
            % (summary1["schema_version"], summary1["journal_mode"],
               summary1["content_sha256"]))
        log("node stand      127.0.0.1:%d (pinned image container, db copy 1)"
            % NODE_PORT)
        log("rust stand      127.0.0.1:%d (release binary, db copy 2)" % RUST_PORT)

        cap.BASE_HOST, cap.BASE_PORT = "127.0.0.1", NODE_PORT
        container = start_node(node_dir)
        wait_ready(NODE_PORT, "/", "node stand", 60)

        rust_log = open(rust_log_path, "w", encoding="utf-8")
        env = dict(os.environ)
        env.update({
            "HOST": "127.0.0.1",
            "PORT": str(RUST_PORT),
            "DATABASE_URL": "sqlite:%s?mode=rw" % rust_db,
            "TOTP_SECRET": cap.TOTP_SECRET,
            "DEVICE_LEGACY_COOKIE": "1",
            "RUST_LOG": "info",
        })
        rust_proc = subprocess.Popen([RUST_BIN], env=env, stdout=rust_log,
                                     stderr=subprocess.STDOUT)
        wait_ready(RUST_PORT, "/readyz", "rust stand", 30,
                   proc=rust_proc, log_path=rust_log_path)

        node_rows, node_meta = run_side(NODE_PORT, "node")
        rust_rows, rust_meta = run_side(RUST_PORT, "rust")
        rc = compare_and_report(node_rows, rust_rows, node_meta, rust_meta)
    finally:
        if rust_proc is not None:
            rust_proc.terminate()
            try:
                rust_proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                rust_proc.kill()
                rust_proc.wait(timeout=5)
        if rust_log is not None:
            rust_log.close()
        err = stop_node(container)
        if err:
            cleanup_errors.append(err)
        err = remove_tree(tmp)
        if err:
            cleanup_errors.append(err)
        if created_base:
            try:
                os.rmdir(TEMP_BASE)    # only if we created it and it is empty
            except OSError:
                pass
        if cleanup_errors:
            log("cleanup         FAILED: %s" % "; ".join(cleanup_errors))
        else:
            log("cleanup         container removed, server stopped, "
                "temp dir removed")
    if cleanup_errors:
        rc = 2
    log("exit %d" % rc)
    return rc


if __name__ == "__main__":
    sys.exit(main())
