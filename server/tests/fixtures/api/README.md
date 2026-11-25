# T03 golden fixtures - 27 legacy routes

Captured from the **pinned** production Node image running against a
**deterministic synthetic SQLite v7 database** - never from a production or
staging copy of the data (docs/decisions.md W0-3, plan §13.2 T03).

* routes: 27 = 25 `/api/*` + `POST /admin/login` + `POST /admin/logout`
  (`GET /admin` is a page and is not part of the 27)
* files: 27 JSON fixtures (`NN_<method>_<path>.json`), this README and
  `manifest` (JSON document kept extensionless so that
  `ls server/tests/fixtures/api/*.json` counts exactly the 27 fixtures)
* image: `ghcr.io/meow-meow-ltd/lampa-source@sha256:d678c142ba58ff1707f5081a73c8b85393e75c36638754b1a6873fbffc21add7`
* stand: http://127.0.0.1:9121 (throwaway container, never 9119) - one throwaway container, removed after every run
* database: synthetic SQLite v7, deterministic seed, no real data
* request order: fixed, index 1..27

Regenerate / verify:

```bash
python3 deploy/scripts/capture_api_golden.py           # rewrite fixtures
python3 deploy/scripts/capture_api_golden.py --check   # 27/27, diff 0, secrets 0
python3 deploy/scripts/capture_api_golden.py --audit   # + synthetic DB invariants
```

## Route -> file -> auth -> sanitization (27/27)

| # | Route | File | Auth | Sanitization |
|---|---|---|---|---|
| 1 | `POST /api/pin/verify` | `01_post_api_pin_verify.json` | no cookies | R1, R6 |
| 2 | `GET /api/admin/devices` | `02_get_api_admin_devices.json` | cookie auth_token (admin, from setup POST /admin/login) | R1, R3, R4, R8 |
| 3 | `POST /api/admin/device/action` | `03_post_api_admin_device_action.json` | cookie auth_token (admin, from setup POST /admin/login) | R1, R8 |
| 4 | `GET /api/settings` | `04_get_api_settings.json` | cookie auth_token (admin, from setup POST /admin/login) | R1, R5, R8 |
| 5 | `PUT /api/settings` | `05_put_api_settings.json` | cookie auth_token (admin, from setup POST /admin/login) | R1, R8 |
| 6 | `DELETE /api/settings/{key}` | `06_delete_api_settings_key.json` | cookie auth_token (admin, from setup POST /admin/login) | R1, R8 |
| 7 | `POST /api/values` | `07_post_api_values.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 8 | `GET /api/value` | `08_get_api_value.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 9 | `GET /api/profiles/all` | `09_get_api_profiles_all.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 10 | `POST /api/profiles/create` | `10_post_api_profiles_create.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 11 | `POST /api/profiles/update` | `11_post_api_profiles_update.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 12 | `POST /api/profiles/delete` | `12_post_api_profiles_delete.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 13 | `GET /api/profile/settings` | `13_get_api_profile_settings.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 14 | `POST /api/profile/settings` | `14_post_api_profile_settings.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 15 | `GET /api/bookmarks/dump` | `15_get_api_bookmarks_dump.json` | cookie device_token (issued by fixture 01) | R1, R3, R8 |
| 16 | `GET /api/bookmarks/changelog` | `16_get_api_bookmarks_changelog.json` | cookie device_token (issued by fixture 01) | R1, R3, R8 |
| 17 | `POST /api/bookmarks/add` | `17_post_api_bookmarks_add.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 18 | `POST /api/bookmarks/remove` | `18_post_api_bookmarks_remove.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 19 | `POST /api/bookmarks/clear` | `19_post_api_bookmarks_clear.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 20 | `POST /api/bookmarks/sync` | `20_post_api_bookmarks_sync.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 21 | `GET /api/timeline/dump` | `21_get_api_timeline_dump.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 22 | `GET /api/timeline/changelog` | `22_get_api_timeline_changelog.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 23 | `POST /api/timeline/update` | `23_post_api_timeline_update.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 24 | `POST /api/users/backup/export` | `24_post_api_users_backup_export.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 25 | `GET /api/users/backup/import` | `25_get_api_users_backup_import.json` | cookie device_token (issued by fixture 01) | R1, R8 |
| 26 | `POST /admin/login` | `26_post_admin_login.json` | no cookies | R1, R2, R6 |
| 27 | `POST /admin/logout` | `27_post_admin_logout.json` | no cookies | R1 |

## Fixture file shape

* `index` - position in the fixed request order (1..27), also the file prefix
* `route` - contract pattern (`DELETE /api/settings/{key}` shows the pattern,
  `request.path` the concrete target used)
* `request` - `method`, `path` (with query string), non-secret `headers`,
  `cookies` (names only) and the normalized `body` (object or `null`)
* `response` - `status`, ordered `[name, value]` header pairs and `body`: the
  **legacy response bytes as a string**, normalized by the rules below
* `auth` / `sanitization` - same values as the mapping table above
* `note` - legacy quirk or intent of the route (optional)

## Normalization rules

| Id | Rule |
|---|---|
| R1 | response header Date -> "<date>" |
| R2 | response Set-Cookie value -> "<redacted>" (auth_token never stored) |
| R3 | timestamp fields (created_at/updated_at/last_attempt_at/last_seen_at/blocked_until/first_attempt_at) -> "<ts>" |
| R4 | ip fields -> "<ip>" (docker gateway address is environment dependent) |
| R5 | settings rows keyed admin_password/admin_token -> value "***" |
| R6 | secret fields (admin_password, admin_token, device_token, auth_token, password, pin, totp, token, session, secret) -> "***" |
| R7 | any 64-hex token in the body -> "***" |
| R8 | request Cookie header -> cookie names only, values "<redacted>" |
| R9 | response Content-Length recomputed over the normalized body (legacy API answers are chunked, so this normally does not fire) |

A route lists only the rules that actually fired for it; `R1` (the `Date`
header) fires for every response.

## Notes

* Requests carry `User-Agent: lampa-golden/1.0` and are sent in the fixed order
  1..27; the synthetic credentials (`TOTP_SECRET`, admin password, device
  token) are generated by the capture scripts and never read from any env file.
* Fixture bodies are stored as strings so the legacy bytes - including the
  `secuses` typo, the `text/plain` dump content type and the inverted
  backup `export`/`import` naming - are captured as-is, not "fixed".
* Mutating routes are part of the contract and are replayed on a fresh
  synthetic database every run, which is what makes `--check` idempotent.

