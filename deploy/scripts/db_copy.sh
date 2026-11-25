#!/usr/bin/env bash
# db_copy.sh - consistent copy of the production SQLite DB for the staging stand.
#
# Why not "cp": copying a live SQLite file can capture a torn state. This script
# uses the SQLite backup API (python3 sqlite3.Connection.backup) with the source
# opened read-only (mode=ro). Ported from poc/s11-delivery/scripts/backup_hot.py
# (S11 §3) and extended with the schema/journal assertions from S2 / S11 §4.2.
# Attribution: docs/staging.md §11.
#
# Usage:
#   bash deploy/scripts/db_copy.sh \
#     --src /opt/lampa/data/database.sqlite \
#     --dest /opt/lampa/staging-data/database.sqlite
#
# Guarantees:
#   * the source is opened read-only; the script refuses any --dest inside the
#     production data directory, so /opt/lampa/data can never be written;
#   * the artifact is a self-contained DB: integrity_check=ok, quick_check=ok,
#     schema_version=7, journal_mode=delete, exactly the 10 application tables
#     plus sqlite_sequence;
#   * the artifact is mode 0600, its directory 0700, replaced atomically (tmp+mv);
#   * if the staging container is running it is stopped for the swap and started
#     again afterwards (so the stand never serves the previous DB inode); use
#     --no-restart to refuse instead.
#
# Exit codes: 0 ok, 2 usage/guard error, 4 verification failed.
set -euo pipefail

PROD_DIR=/opt/lampa/data
SRC=${PROD_DIR}/database.sqlite
DEST=/opt/lampa/staging-data/database.sqlite
STAGING_CONTAINER=lampa-staging
NO_RESTART=0

usage() {
    sed -n '2,/^set -euo pipefail/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
}

die() {
    echo "db_copy: ERROR: $*" >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --src)
            [ $# -ge 2 ] || die "--src needs a value"
            SRC=$2
            shift 2
            ;;
        --dest)
            [ $# -ge 2 ] || die "--dest needs a value"
            DEST=$2
            shift 2
            ;;
        --no-restart)
            NO_RESTART=1
            shift
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

command -v python3 >/dev/null 2>&1 || die "python3 is required"
SRC_ABS=$(realpath -m -- "$SRC")
DEST_ABS=$(realpath -m -- "$DEST")

case "${DEST_ABS}" in
    "${PROD_DIR}"|"${PROD_DIR}"/*)
        die "refusing --dest inside the production data directory (${PROD_DIR})"
        ;;
esac
[ "${SRC_ABS}" = "${DEST_ABS}" ] && die "src and dest are the same file"
[ -f "${SRC_ABS}" ] || die "source database not found: ${SRC_ABS}"

case "${SRC_ABS}" in
    "${PROD_DIR}"/*) : ;;
    *) echo "db_copy: WARN: source is not ${PROD_DIR}/database.sqlite (${SRC_ABS})" >&2 ;;
esac

DEST_DIR=$(dirname -- "${DEST_ABS}")
mkdir -p -- "${DEST_DIR}"
chmod 700 -- "${DEST_DIR}"

STOPPED_BY_US=0
if command -v docker >/dev/null 2>&1 \
   && docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "${STAGING_CONTAINER}"; then
    if [ "${NO_RESTART}" -eq 1 ]; then
        die "container ${STAGING_CONTAINER} is running and holds the DB copy;
       stop the stand first or drop --no-restart so this script can restart it"
    fi
    echo "db_copy: stopping ${STAGING_CONTAINER} for the DB swap (it is started again afterwards)"
    docker stop -- "${STAGING_CONTAINER}" >/dev/null
    STOPPED_BY_US=1
fi

TMP="${DEST_ABS}.tmp.$$"
cleanup() {
    rm -f -- "${TMP}" "${TMP}-journal" "${TMP}-wal" "${TMP}-shm"
    if [ "${STOPPED_BY_US:-0}" -eq 1 ]; then
        docker start -- "${STAGING_CONTAINER}" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

SRC_MTIME_BEFORE=$(stat -c '%Y' -- "${SRC_ABS}")
SRC_SIZE_BEFORE=$(stat -c '%s' -- "${SRC_ABS}")
SRC_SHA_BEFORE=$(sha256sum -- "${SRC_ABS}" | cut -d' ' -f1)

echo "db_copy: backing up ${SRC_ABS} (mode=ro) -> ${DEST_ABS} via SQLite backup API"

COPIED=$(python3 - "${SRC_ABS}" "${TMP}" <<'PY'
import json
import os
import sqlite3
import sys
import time

APP_TABLES = [
    "bookmark_events",
    "bookmarks",
    "device_access",
    "ip_rate_limits",
    "profile_settings",
    "profiles",
    "schema_version",
    "settings",
    "timeline",
    "timeline_events",
]
EXPECTED_TABLES = sorted(APP_TABLES + ["sqlite_sequence"])


def verify(path):
    con = sqlite3.connect("file:" + path + "?mode=ro", uri=True, timeout=15)
    con.execute("PRAGMA busy_timeout=15000")
    try:
        integrity = con.execute("PRAGMA integrity_check").fetchone()[0]
        quick = con.execute("PRAGMA quick_check").fetchone()[0]
        journal = con.execute("PRAGMA journal_mode").fetchone()[0]
        version_row = con.execute("SELECT version FROM schema_version WHERE id=1").fetchone()
        version = version_row[0] if version_row else None
        tables = [r[0] for r in con.execute(
            "SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name").fetchall()]
        counts = {t: con.execute('SELECT COUNT(*) FROM "' + t + '"').fetchone()[0]
                  for t in tables}
        pages = con.execute("PRAGMA page_count").fetchone()[0]
        page_size = con.execute("PRAGMA page_size").fetchone()[0]
    finally:
        con.close()
    errors = []
    if integrity != "ok":
        errors.append("integrity_check=" + str(integrity))
    if quick != "ok":
        errors.append("quick_check=" + str(quick))
    if version != 7:
        errors.append("schema_version=" + str(version) + " (expected 7)")
    if journal != "delete":
        errors.append("journal_mode=" + str(journal) + " (expected delete)")
    if sorted(tables) != EXPECTED_TABLES:
        errors.append("tables mismatch: " + ",".join(sorted(tables)))
    return {
        "ok": not errors,
        "errors": errors,
        "integrity_check": integrity,
        "quick_check": quick,
        "schema_version": version,
        "journal_mode": journal,
        "tables": sorted(tables),
        "table_count": len(tables),
        "row_counts": counts,
        "pages": pages,
        "page_size": page_size,
        "size_bytes": os.path.getsize(path),
    }


def main():
    src_path, tmp_path = sys.argv[1], sys.argv[2]
    t0 = time.perf_counter()
    src = sqlite3.connect("file:" + src_path + "?mode=ro", uri=True, timeout=15)
    src.execute("PRAGMA busy_timeout=15000")
    dst = sqlite3.connect(tmp_path)
    dst.execute("PRAGMA busy_timeout=15000")
    last_err = None
    for attempt in range(1, 7):
        try:
            src.backup(dst, pages=-1, sleep=0.05)
            last_err = None
            break
        except sqlite3.OperationalError as exc:
            last_err = str(exc)
            time.sleep(0.5 * attempt)
    dst.close()
    src.close()
    elapsed_ms = (time.perf_counter() - t0) * 1000.0
    if last_err is not None:
        print(json.dumps({"ok": False, "error": last_err}))
        return 1
    result = verify(tmp_path)
    result["elapsed_ms"] = round(elapsed_ms, 1)
    print(json.dumps(result))
    return 0 if result["ok"] else 1


raise SystemExit(main())
PY
)

python3 -c '
import json, sys
data = json.loads(sys.argv[1])
if not data.get("ok"):
    print("db_copy: backup FAILED: " + "; ".join(data.get("errors") or [data.get("error", "?")]), file=sys.stderr)
    raise SystemExit(4)
' "${COPIED}" || exit 4

chmod 600 -- "${TMP}"
mv -f -- "${TMP}" "${DEST_ABS}"

if [ "${STOPPED_BY_US}" -eq 1 ]; then
    docker start -- "${STAGING_CONTAINER}" >/dev/null
    echo "db_copy: started ${STAGING_CONTAINER} again (Node needs ~1.5 s to answer on 9119)"
fi
trap - EXIT

python3 - "${SRC_ABS}" "${DEST_ABS}" "${SRC_MTIME_BEFORE}" "${SRC_SIZE_BEFORE}" "${SRC_SHA_BEFORE}" "${COPIED}" <<'PY'
import hashlib
import json
import os
import stat
import sys

src_path, dest_path = sys.argv[1], sys.argv[2]
mtime_before, size_before, sha_before = int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
copied = json.loads(sys.argv[6])


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


mtime_after = int(os.stat(src_path).st_mtime)
size_after = os.stat(src_path).st_size
sha_after = sha256(src_path)
mode = stat.S_IMODE(os.stat(dest_path).st_mode)
counts = copied["row_counts"]

ok = copied["ok"] and mode == 0o600
print("")
print("db_copy: " + ("PASS" if ok else "FAIL"))
print("  src              " + src_path + " (opened mode=ro, " + str(size_after) + " B)")
print("  dest             " + dest_path + " (" + str(copied["size_bytes"]) + " B, mode=" + oct(mode)[2:].zfill(4) + ")")
print("  backup_api_ms    " + str(copied["elapsed_ms"]) + " (SQLite backup API, not cp)")
print("  integrity_check  " + copied["integrity_check"])
print("  quick_check      " + copied["quick_check"])
print("  schema_version   " + str(copied["schema_version"]))
print("  journal_mode     " + copied["journal_mode"])
print("  tables           " + str(copied["table_count"]) + " (10 app + sqlite_sequence)")
print("  pages/size       " + str(copied["pages"]) + " x " + str(copied["page_size"]) + " B")
print("  rows             " + " ".join(k + "=" + str(v) for k, v in sorted(counts.items())))
same = (mtime_before == mtime_after and size_before == size_after and sha_before == sha_after)
if same:
    print("  prod_untouched   yes (src mtime/size/sha256 unchanged by this run)")
else:
    print("  prod_untouched   src changed during the run (prod app is live); not written by this script")
print("  src_sha256       " + sha_before[:16] + "... -> " + sha_after[:16] + "...")
raise SystemExit(0 if ok else 4)
PY
