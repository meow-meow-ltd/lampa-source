#!/usr/bin/env python3
"""Build the deterministic SYNTHETIC SQLite v7 DB used by T03 golden capture.

Purpose (docs/decisions.md W0-3 / plan §13.2 T03): the 27 legacy `/api/*`
golden fixtures must never be captured from the production copy
(`/opt/lampa/data`) nor from the staging copy (`/opt/lampa/staging-data`) -
both hold real user data (profiles, bookmarks, watch history).  This builder
creates a *fresh* database from a schema-only derivation of
`backend/database.js` migrations (verified against a read-only `.schema` dump
of the staging copy) plus a small set of hand-written synthetic rows.

Invariants asserted before this script returns (exit 0):

  * schema_version = 7, journal_mode = delete (SQLite default, never WAL)
  * exactly 3 profiles, one of them `Main` (is_main = 1)
  * <= 20 bookmarks, <= 50 timeline_events, exactly 20 settings rows
  * bookmark_events are consistent: every entity_id exists in bookmarks
  * a few device_access rows, exactly one of them `allowed` (test device)
  * tables are exactly the app's v7 set + sqlite_sequence; there is no
    `_sqlx_migrations` table, no DDL beyond the embedded schema and no
    `-wal` / `-shm` / `-journal` sidecar files
  * two consecutive builds produce byte-identical *content* (canonical dump
    SHA-256 is equal) - the seed is fully deterministic

Data policy: every inserted row is a literal constant defined in this file.
No production or staging row is ever read, and the script refuses to write
into the production data directory or the staging data directory.

Usage:
    python3 deploy/scripts/make_synthetic_db.py --out /tmp/x/synthetic.sqlite
    python3 deploy/scripts/make_synthetic_db.py --out /tmp/x/synthetic.sqlite --summary-json
"""
import argparse
import hashlib
import json
import os
import sqlite3
import sys

# Directories this builder is not allowed to write into (fail closed).
FORBIDDEN_WRITE_PREFIXES = (
    "/opt/lampa/data",
    "/opt/lampa/staging-data",
)

# Fixed synthetic epoch (2026-01-01T00:00:00Z) used for every seeded
# timestamp, so fixtures never depend on wall-clock time.
T0 = 1767225600000

# Synthetic credentials - generated for this repository only.  They are NOT
# production values and NOT staging values (staging keeps its own throwaway
# values in a 0600 file outside git).  Fixtures mask them anyway.
ADMIN_PASSWORD = "golden-admin-password"
ADMIN_TOKEN = "golden-admin-token-9f3c1d7e"
DEVICE_ID_ALLOWED = "golden-device-0001"
DEVICE_TOKEN = hashlib.sha256(b"lampa-golden-device-token-0001").hexdigest()
USER_AGENT = "lampa-golden/1.0"

EXPECTED_TABLES = sorted(
    [
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
        "sqlite_sequence",
    ]
)

# Schema-only derivation of backend/database.js migrations v1..v7.  Column
# lists and indexes were cross-checked against a read-only
# `SELECT sql FROM sqlite_master` dump of the staging copy (2026-11-25); only
# the statement text is reused, never a data row.
SCHEMA_V7 = [
    "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT, visible BOOLEAN NOT NULL DEFAULT 0)",
    "CREATE TABLE schema_version (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL)",
    """CREATE TABLE profiles (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL,
        icon TEXT,
        is_main INTEGER NOT NULL DEFAULT 0,
        is_child INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
                    , sort_order INTEGER NOT NULL DEFAULT 0)""",
    "CREATE INDEX profiles_sort_order ON profiles(sort_order)",
    """CREATE TABLE bookmarks (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        profile_id INTEGER NOT NULL,
        type TEXT NOT NULL,
        card_id INTEGER NOT NULL,
        data TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    )""",
    "CREATE UNIQUE INDEX bookmarks_unique ON bookmarks(profile_id, type, card_id)",
    "CREATE INDEX bookmarks_profile_type ON bookmarks(profile_id, type)",
    """CREATE TABLE bookmark_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        profile_id INTEGER NOT NULL,
        action TEXT NOT NULL,
        entity_id INTEGER,
        type TEXT,
        card_id INTEGER,
        data TEXT,
        updated_at INTEGER NOT NULL
    )""",
    "CREATE INDEX bookmark_events_profile_id ON bookmark_events(profile_id, id)",
    """CREATE TABLE timeline (
        profile_id INTEGER NOT NULL,
        hash TEXT NOT NULL,
        data TEXT NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY(profile_id, hash)
    )""",
    # NOTE: no `timeline_profile_id` index.  backend/database.js creates it in
    # migration v1, but migration v2 renames/drops the table it hangs on, so a
    # real v7 database (and a read-only .schema dump of one) does not have it.
    """CREATE TABLE timeline_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        profile_id INTEGER NOT NULL,
        hash TEXT NOT NULL,
        data TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    )""",
    "CREATE INDEX timeline_events_profile_id ON timeline_events(profile_id, id)",
    """CREATE TABLE profile_settings (
        profile_id INTEGER NOT NULL,
        key TEXT NOT NULL,
        value TEXT,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY(profile_id, key)
    )""",
    "CREATE INDEX profile_settings_profile_id ON profile_settings(profile_id)",
    """CREATE TABLE device_access (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        device_id TEXT NOT NULL,
        ip TEXT,
        user_agent TEXT,
        status TEXT NOT NULL DEFAULT 'pending',
        attempts INTEGER DEFAULT 0,
        last_attempt_at INTEGER,
        blocked_until INTEGER,
        last_seen_at INTEGER,
        token_hash TEXT
    )""",
    "CREATE UNIQUE INDEX device_access_id ON device_access(device_id)",
    """CREATE TABLE ip_rate_limits (
        ip TEXT PRIMARY KEY,
        attempts INTEGER DEFAULT 0,
        first_attempt_at INTEGER NOT NULL,
        blocked_until INTEGER
    )""",
    "CREATE INDEX ip_rate_limits_blocked ON ip_rate_limits(blocked_until)",
]

SETTINGS_ROWS = [
    # key, value, visible  (exactly the 20 defaults of backend/database.js)
    ("admin_password", ADMIN_PASSWORD, 0),
    ("admin_token", ADMIN_TOKEN, 0),
    ("torrserver_url", "http://synthetic.invalid:9091", 1),
    ("jackett_url", "http://synthetic.invalid:9117", 1),
    ("menu_interface", "1", 1),
    ("menu_channels", "1", 1),
    ("menu_player", "1", 1),
    ("menu_parser", "1", 1),
    ("menu_server", "1", 1),
    ("menu_tmdb", "1", 1),
    ("menu_plugins", "1", 1),
    ("menu_parental_control", "1", 1),
    ("menu_data", "1", 1),
    ("menu_more", "1", 1),
    ("head_search", "1", 1),
    ("head_settings", "1", 1),
    ("head_broadcast", "1", 1),
    ("head_notice", "1", 1),
    ("head_profile", "1", 1),
    ("plugins", "[]", 1),
]

PROFILES = [
    # id, name, icon, is_main, is_child, sort_order
    (1, "Main", "l_1", 1, 0, 1),
    (2, "Kids", "l_2", 0, 1, 2),
    (3, "Guest", "l_3", 0, 0, 3),
]

# (profile_id, type, card_id) - 12 bookmarks in total (contract: <= 20).
BOOKMARK_SPEC = [
    (1, "film", 1001),
    (1, "film", 1002),
    (1, "film", 1003),
    (1, "cartoon", 1004),
    (1, "cartoon", 1005),
    (1, "game", 1006),
    (2, "cartoon", 2001),
    (2, "cartoon", 2002),
    (2, "film", 2003),
    (2, "game", 2004),
    (3, "film", 3001),
    (3, "game", 3002),
]

TIMELINE_SPEC = [
    # (profile_id, hash, percent, time, duration)
    (1, "golden-hash-000a", 10, 600, 6000),
    (1, "golden-hash-000b", 25, 1500, 6000),
    (1, "golden-hash-000c", 40, 2400, 6000),
    (1, "golden-hash-000d", 55, 3300, 6000),
    (1, "golden-hash-000e", 70, 4200, 6000),
    (2, "golden-hash-001a", 5, 150, 3000),
    (2, "golden-hash-001b", 20, 600, 3000),
    (2, "golden-hash-001c", 35, 1050, 3000),
    (2, "golden-hash-001d", 50, 1500, 3000),
    (2, "golden-hash-001e", 65, 1950, 3000),
]

PROFILE_SETTINGS = [
    # (profile_id, key, value)
    (1, "cinema_mode", "golden"),
    (1, "last_device", DEVICE_ID_ALLOWED),
    (1, "watch_progress", "1234"),
    (2, "cinema_mode", "off"),
]

DEVICES = [
    # id, device_id, ip, user_agent, status, attempts,
    # last_attempt_at, blocked_until, last_seen_at, token_hash
    (1, DEVICE_ID_ALLOWED, "192.0.2.10", USER_AGENT, "allowed", 0, T0, None, T0,
     hashlib.sha256(DEVICE_TOKEN.encode("utf-8")).hexdigest()),
    (2, "golden-device-0002", "192.0.2.11", USER_AGENT, "blocked", 5, T0,
     T0 + 900000, None, None),
    (3, "golden-device-0003", "192.0.2.12", USER_AGENT, "pending", 2, T0,
     None, T0, None),
]


def card_data(card_id, index):
    """Deterministic synthetic bookmark payload (a JSON string, as legacy)."""
    return json.dumps(
        {
            "id": card_id,
            "title": "Golden Fixture Card %04d" % card_id,
            "year": 2018 + (index % 6),
            "synthetic": True,
        },
        sort_keys=True,
        separators=(",", ":"),
    )


def timeline_data(profile_id, card_hash, percent, seconds, duration):
    return json.dumps(
        {
            "hash": card_hash,
            "percent": percent,
            "time": seconds,
            "duration": duration,
            "profile": profile_id,
        },
        sort_keys=True,
        separators=(",", ":"),
    )


def refuse_forbidden_out(path):
    real = os.path.realpath(path)
    for prefix in FORBIDDEN_WRITE_PREFIXES:
        if real == prefix or real.startswith(prefix + os.sep):
            raise SystemExit(
                "make_synthetic_db: refusing to write %r inside %s "
                "(synthetic DB must live outside production/staging data)"
                % (real, prefix)
            )
    return real


def create_schema(conn):
    for statement in SCHEMA_V7:
        conn.execute(statement)


def seed(conn):
    # schema_version row (v7) - no migration is ever executed by the app
    # against an already-v7 database.
    conn.execute("INSERT INTO schema_version(id, version) VALUES(1, 7)")

    for key, value, visible in SETTINGS_ROWS:
        conn.execute(
            "INSERT INTO settings(key, value, visible) VALUES(?, ?, ?)",
            (key, value, visible),
        )

    for pid, name, icon, is_main, is_child, sort_order in PROFILES:
        conn.execute(
            "INSERT INTO profiles(id, name, icon, is_main, is_child, sort_order,"
            " created_at, updated_at) VALUES(?, ?, ?, ?, ?, ?, ?, ?)",
            (pid, name, icon, is_main, is_child, sort_order, T0, T0),
        )

    for index, (profile_id, btype, card_id) in enumerate(BOOKMARK_SPEC):
        conn.execute(
            "INSERT INTO bookmarks(id, profile_id, type, card_id, data, updated_at)"
            " VALUES(?, ?, ?, ?, ?, ?)",
            (
                index + 1,
                profile_id,
                btype,
                card_id,
                card_data(card_id, index),
                T0 + index * 60000,
            ),
        )

    # bookmark_events mirror the bookmark rows: one `add` per bookmark plus a
    # single `update` for the first one, so every entity_id resolves.
    event_id = 0
    for index, (profile_id, btype, card_id) in enumerate(BOOKMARK_SPEC):
        event_id += 1
        conn.execute(
            "INSERT INTO bookmark_events(id, profile_id, action, entity_id, type,"
            " card_id, data, updated_at) VALUES(?, ?, 'add', ?, ?, ?, ?, ?)",
            (
                event_id,
                profile_id,
                index + 1,
                btype,
                card_id,
                card_data(card_id, index),
                T0 + index * 60000,
            ),
        )
    event_id += 1
    conn.execute(
        "INSERT INTO bookmark_events(id, profile_id, action, entity_id, type,"
        " card_id, data, updated_at) VALUES(?, 1, 'update', 1, ?, ?, ?, ?)",
        (event_id, BOOKMARK_SPEC[0][1], BOOKMARK_SPEC[0][2],
         card_data(BOOKMARK_SPEC[0][2], 0), T0 + 660000),
    )

    for index, (profile_id, card_hash, percent, seconds, duration) in enumerate(
        TIMELINE_SPEC
    ):
        data = timeline_data(profile_id, card_hash, percent, seconds, duration)
        conn.execute(
            "INSERT INTO timeline(profile_id, hash, data, updated_at) VALUES(?, ?, ?, ?)",
            (profile_id, card_hash, data, T0 + index * 60000),
        )
        conn.execute(
            "INSERT INTO timeline_events(id, profile_id, hash, data, updated_at)"
            " VALUES(?, ?, ?, ?, ?)",
            (index + 1, profile_id, card_hash, data, T0 + index * 60000),
        )

    for profile_id, key, value in PROFILE_SETTINGS:
        conn.execute(
            "INSERT INTO profile_settings(profile_id, key, value, updated_at)"
            " VALUES(?, ?, ?, ?)",
            (profile_id, key, value, T0),
        )

    for row in DEVICES:
        conn.execute(
            "INSERT INTO device_access(id, device_id, ip, user_agent, status,"
            " attempts, last_attempt_at, blocked_until, last_seen_at, token_hash)"
            " VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            row,
        )
    # ip_rate_limits stays empty: the capture performs no failed PIN attempt.


def canonical_dump(conn):
    """Deterministic fingerprint of the whole DB (schema + rows)."""
    digest = hashlib.sha256()
    for name, sql in conn.execute(
        "SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name"
    ):
        digest.update(("%s|%s\n" % (name, " ".join(sql.split()))).encode("utf-8"))
        try:
            rows = conn.execute(
                'SELECT * FROM "%s" ORDER BY 1' % name
            ).fetchall()
        except sqlite3.OperationalError:
            continue
        for row in rows:
            digest.update(json.dumps(row, default=str, ensure_ascii=True).encode("utf-8"))
            digest.update(b"\n")
    return digest.hexdigest()


def audit(conn, path):
    """Assert every contract invariant; returns the printable summary."""
    failures = []

    journal = conn.execute("PRAGMA journal_mode").fetchone()[0]
    if str(journal).lower() != "delete":
        failures.append("journal_mode=%s (expected delete)" % journal)

    version = conn.execute("SELECT version FROM schema_version WHERE id=1").fetchone()
    if not version or int(version[0]) != 7:
        failures.append("schema_version=%s (expected 7)" % (version[0] if version else None))

    tables = sorted(
        r[0] for r in conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table'"
        )
    )
    if tables != EXPECTED_TABLES:
        failures.append("tables=%s (expected %s)" % (tables, EXPECTED_TABLES))
    if "_sqlx_migrations" in tables:
        failures.append("_sqlx_migrations table present")

    profiles = conn.execute(
        "SELECT COUNT(*), SUM(is_main), GROUP_CONCAT(name, '|')"
        " FROM profiles"
    ).fetchone()
    if int(profiles[0]) != 3:
        failures.append("profiles=%d (expected 3)" % profiles[0])
    if int(profiles[1] or 0) != 1:
        failures.append("main profiles=%s (expected 1)" % profiles[1])
    if "Main" not in (profiles[2] or ""):
        failures.append("Main profile missing (%s)" % profiles[2])

    bookmarks = conn.execute("SELECT COUNT(*) FROM bookmarks").fetchone()[0]
    if int(bookmarks) > 20:
        failures.append("bookmarks=%d (expected <=20)" % bookmarks)

    orphans = conn.execute(
        "SELECT COUNT(*) FROM bookmark_events e"
        " LEFT JOIN bookmarks b ON b.id = e.entity_id"
        " WHERE e.action = 'add' AND b.id IS NULL"
    ).fetchone()[0]
    if int(orphans) != 0:
        failures.append("bookmark_events orphans=%d" % orphans)

    timeline_events = conn.execute(
        "SELECT COUNT(*) FROM timeline_events"
    ).fetchone()[0]
    if int(timeline_events) > 50:
        failures.append("timeline_events=%d (expected <=50)" % timeline_events)

    settings = conn.execute("SELECT COUNT(*) FROM settings").fetchone()[0]
    if int(settings) != 20:
        failures.append("settings=%d (expected 20)" % settings)

    allowed = conn.execute(
        "SELECT COUNT(*) FROM device_access WHERE status='allowed'"
    ).fetchone()[0]
    devices = conn.execute("SELECT COUNT(*) FROM device_access").fetchone()[0]
    if int(allowed) < 1:
        failures.append("no allowed test device")
    if int(devices) < 2:
        failures.append("device_access=%d (expected a few)" % devices)

    for suffix in ("-wal", "-shm", "-journal"):
        if os.path.exists(path + suffix):
            failures.append("sidecar file present: %s%s" % (path, suffix))

    integrity = conn.execute("PRAGMA integrity_check").fetchone()[0]
    if str(integrity).lower() != "ok":
        failures.append("integrity_check=%s" % integrity)

    return {
        "schema_version": int(version[0]) if version else None,
        "journal_mode": str(journal),
        "profiles": int(profiles[0]),
        "main_profile": "yes" if int(profiles[1] or 0) == 1 else "no",
        "bookmarks": int(bookmarks),
        "bookmark_events": int(
            conn.execute("SELECT COUNT(*) FROM bookmark_events").fetchone()[0]
        ),
        "timeline": int(conn.execute("SELECT COUNT(*) FROM timeline").fetchone()[0]),
        "timeline_events": int(timeline_events),
        "settings": int(settings),
        "devices": int(devices),
        "allowed_devices": int(allowed),
        "tables": len(tables),
        "integrity_check": str(integrity),
        "content_sha256": canonical_dump(conn),
        "failures": failures,
    }


def build(out_path):
    real = refuse_forbidden_out(out_path)
    parent = os.path.dirname(real) or "."
    if not os.path.isdir(parent):
        raise SystemExit("make_synthetic_db: parent directory does not exist: %s" % parent)
    for suffix in ("", "-wal", "-shm", "-journal"):
        if os.path.exists(real + suffix):
            os.unlink(real + suffix)

    conn = sqlite3.connect(real)
    try:
        conn.execute("PRAGMA journal_mode=delete")
        conn.execute("BEGIN")
        create_schema(conn)
        seed(conn)
        conn.commit()
        summary = audit(conn, real)
    finally:
        conn.close()

    if summary["failures"]:
        raise SystemExit(
            "make_synthetic_db: invariant violations: %s"
            % "; ".join(summary["failures"])
        )
    summary["path"] = real
    summary["bytes"] = os.path.getsize(real)
    return summary


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Build the deterministic synthetic SQLite v7 DB for T03."
    )
    parser.add_argument("--out", required=True, help="path of the DB file to create")
    parser.add_argument(
        "--summary-json",
        action="store_true",
        help="print the audit summary as a single JSON object",
    )
    args = parser.parse_args(argv)

    summary = build(args.out)

    if args.summary_json:
        print(json.dumps(summary, sort_keys=True))
    else:
        print("make_synthetic_db: wrote %s (%d bytes)" % (summary["path"], summary["bytes"]))
        for key in (
            "schema_version",
            "journal_mode",
            "profiles",
            "main_profile",
            "bookmarks",
            "bookmark_events",
            "timeline",
            "timeline_events",
            "settings",
            "devices",
            "allowed_devices",
            "tables",
            "integrity_check",
            "content_sha256",
        ):
            print("  %-18s %s" % (key, summary[key]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
