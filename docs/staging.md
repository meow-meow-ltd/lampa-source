# Staging stand — Node on 127.0.0.1:9119 (P0.0 / T02)

Ізольований Node-стенд для golden-фікстур legacy `/api/*` (T03), differential-тестів
(P0.1), репетицій cutover і memory-вимірів. Працює на **копії** прод-БД, з тестовими
креденшелами; прод (`lampa`, 9118, `/opt/lampa/data`) не монтується і не змінюється.

Джерела фактів: `review/30-final-plan.md` §11.4, `review/inv/S11-delivery.md` §1–§4.1/§9,
`review/20-g0-results.md` (D9/D10), `review/00-repo-state.md` §1/§3.

## 1. Факти стенду

| Параметр | Значення | Доказ / як перевірити |
|---|---|---|
| Compose project | `lampa-staging` (`name:` у файлі) | `deploy/compose.staging.yaml` |
| Compose file | `deploy/compose.staging.yaml` | — |
| Container | `lampa-staging` | `docker ps --filter name=lampa-staging` |
| Порт | `127.0.0.1:9119` → `3000` (лише loopback) | `ss -ltn \| grep 9119` |
| Образ | `ghcr.io/meow-meow-ltd/lampa-source@sha256:d678c142ba58ff1707f5081a73c8b85393e75c36638754b1a6873fbffc21add7` | pinned digest = `RepoDigests[0]` прод-образу; image id `sha256:d678…21add7` (той самий, що в прода) |
| `pull_policy` | `never` (образ мусить бути локально; випадковий `pull :latest` неможливий) | `deploy/compose.staging.yaml:23` |
| `restart` | `"no"` | там же |
| Volume | `/opt/lampa/staging-data:/data` (єдиний mount) | `docker inspect -f '{{json .Mounts}}' lampa-staging` |
| env_file | `/opt/lampa/staging-data/env.staging` (mode 0600, поза git) | `ls -l /opt/lampa/staging-data` |
| Ліміти | 2 GiB RAM, 4 vCPU | `mem_limit: 2g`, `cpus: 4.0` |
| DB copy | `/opt/lampa/staging-data/database.sqlite` (0600, 8 110 080 B, 1980 стор.) | §4 нижче |
| Логи | json-file, 10m × 2 | `docker logs lampa-staging` |

Перевизначення образу (навмисна репетиція, не для звичайного запуску):

```bash
LAMPA_STAGING_IMAGE=ghcr.io/meow-meow-ltd/lampa-source@sha256:<digest> \
  docker compose -p lampa-staging -f deploy/compose.staging.yaml up -d
```

## 2. Ізоляція від прода

Правила (не порушувати):

1. `/opt/lampa/data` **не монтується** у стенд. Єдиний bind-mount — `/opt/lampa/staging-data`.
   `db_copy.sh` відмовляється писати `--dest` усередину `/opt/lampa/data` (guard у коді).
2. Прод-БД відкривається лише `mode=ro` (URI `file:…?mode=ro`) і лише на час backup API.
3. `/opt/lampa/compose.yaml`, `/opt/lampa/.env`, прод-образ, Cloudflare, systemd не змінюються.
4. Ніяких `docker exec` у прод; для staging — можна (`docker exec lampa-staging …`).
5. Прод і staging — різні compose-проєкти, контейнери, порти (9118 vs 9119) і дані.

Перевірка незмінності прода (до і після будь-яких робіт):

```bash
docker inspect -f '{{.Id}} {{.State.StartedAt}} {{.RestartCount}}' lampa
docker ps --filter name=lampa
ls -la /opt/lampa/data     # очікується лише database.sqlite, без -wal/-shm/-journal
```

Факт T02 (2026-11-25, до і після ідентично):

| Показник | До | Після |
|---|---|---|
| `lampa` Id | `eb7112be6356…` | `eb7112be6356…` |
| `StartedAt` | `2026-11-24T21:49:38.064780035Z` | те саме |
| `RestartCount` | 0 | 0 |
| `/opt/lampa/data` | `database.sqlite` (mtime 19:32) | те саме, без `-wal/-shm/-journal` |

Прод-БД змінюється **сама** (застосунком, ним користуються) — тому «незмінність» доводиться
Id/StartedAt/RestartCount і відсутністю journal-файлів, а не хешем вмісту.

## 3. Шляхи й права

| Шлях | Призначення | Права |
|---|---|---|
| `deploy/compose.staging.yaml` | compose стенда | git, 0644 |
| `deploy/scripts/db_copy.sh` | копія прод-БД через backup API + перевірки | git, 0755 |
| `deploy/scripts/prepare_staging_db.py` | тестові креденшели + test device + env-файл | git, 0755 |
| `deploy/scripts/smoke.sh` | 9 перевірок | git, 0755 |
| `/opt/lampa/staging-data/` | дані стенда | 0700, **поза git** |
| `/opt/lampa/staging-data/database.sqlite` | копія прод-БД | 0600 |
| `/opt/lampa/staging-data/env.staging` | staging-only значення | 0600, **поза git** |

`staging-data` не входить у репозиторій (лежить поза `/opt/lampa/source`); у `.gitignore` нічого
додавати не потрібно.

## 4. Створення стенда з нуля

Передумови: локально є прод-образ (pinned digest); вільно `127.0.0.1:9119`; є `python3` з `sqlite3`
(стандартна бібліотека) і `docker compose`.

```bash
cd /opt/lampa/source

# 1. Консистентна копія прод-БД (SQLite backup API, джерело mode=ro) -> 0600
bash deploy/scripts/db_copy.sh \
  --src /opt/lampa/data/database.sqlite \
  --dest /opt/lampa/staging-data/database.sqlite

# 2. Тестові admin_password/admin_token, test device, env.staging (0600)
python3 deploy/scripts/prepare_staging_db.py \
  --db /opt/lampa/staging-data/database.sqlite \
  --env-out /opt/lampa/staging-data/env.staging

# 3. Підняти стенд
docker compose -p lampa-staging -f deploy/compose.staging.yaml up -d

# 4. Smoke 9/9
bash deploy/scripts/smoke.sh \
  --base http://127.0.0.1:9119 \
  --env-file /opt/lampa/staging-data/env.staging
```

`db_copy.sh` сам зупиняє `lampa-staging` на час заміни файлу і піднімає його назад
(щоб контейнер не тримав старий inode); `--no-restart` забороняє це. Тому весь ланцюжок
можна повторювати навіть коли стенд уже працює — `smoke.sh` сам чекає readiness до 60 с.

Оновлення копії даних (наприклад, свіжіші прод-дані для T03):

```bash
bash deploy/scripts/db_copy.sh        # stop -> backup -> mv -> start
python3 deploy/scripts/prepare_staging_db.py   # повертає тестові значення
bash deploy/scripts/smoke.sh
```

`prepare_staging_db.py` за замовчуванням **переюзує** значення з наявного `env.staging`
(ідемпотентно, без рестарту). `--rotate` генерує нові (потрібен рестарт:
`docker compose … up -d --force-recreate`).

## 5. Керування

```bash
cd /opt/lampa/source
docker compose -p lampa-staging -f deploy/compose.staging.yaml up -d          # старт
docker compose -p lampa-staging -f deploy/compose.staging.yaml stop          # стоп
docker compose -p lampa-staging -f deploy/compose.staging.yaml restart       # рестарт
docker compose -p lampa-staging -f deploy/compose.staging.yaml down          # прибрати контейнер+мережу (дані лишаються)
docker ps --filter name=lampa-staging
docker inspect -f '{{.State.Status}} {{.RestartCount}} {{.Config.Image}}' lampa-staging
docker logs --tail 50 lampa-staging
```

`down` не видаляє `/opt/lampa/staging-data` (bind mount). Повне прибирання —
лише явно: `docker compose … down` + видалення вмісту `/opt/lampa/staging-data`.

## 6. Тестові креденшели (лише staging)

Значення **не наводяться** ні в git, ні в документації, ні в логах. Вони лежать тільки у
`/opt/lampa/staging-data/env.staging` (0600) і ніколи не збігаються з прод-значеннями.

| Ключ env.staging | Що це | Хто споживає |
|---|---|---|
| `HOST`, `PORT`, `DB_PATH` | `0.0.0.0`, `3000`, `/data/database.sqlite` | Node-контейнер (env_file) |
| `TOTP_SECRET` | тестовий base32 TOTP-секрет (не прод) | Node (`/api/pin/verify`) + smoke |
| `LAMPA_STAGING_ADMIN_PASSWORD` | тестовий admin-пароль (plaintext, як у legacy) | smoke (login) |
| `LAMPA_STAGING_ADMIN_TOKEN` | тестовий `settings.admin_token` (cookie `auth_token`) | Node + smoke |
| `LAMPA_STAGING_DEVICE_TOKEN` | тестовий device token; у БД зберігається лише `sha256` | smoke (device guard) |
| `LAMPA_STAGING_DEVICE_ID` | `staging-smoke-device` (`status='allowed'`) | smoke |

У БД стенда: `settings.admin_password`/`settings.admin_token` замінені на тестові;
доданий `device_access` рядок `staging-smoke-device` (`allowed`, `token_hash=sha256(test token)`).
Схема не змінюється (DML-only, без DDL); інші рядки — з прод-копії (profiles=3, bookmarks=232,
timeline_events=42 467 — станом на 2026-11-25 20:44 UTC).

Застереження:

* `docker compose config` **розкриває** вміст `env_file` у stdout — не запускайте його в сесіях,
  що логуються; для валідації достатньо `docker compose … up -d`.
* `GET /api/settings` у legacy віддає **всі** settings, включно з `admin_password`/`admin_token`.
  T03 і будь-які фікстури **зобов'язані** маскувати ці поля (`***`); у staging там тестові
  значення, але фікстури не повинні містити навіть їх.

## 7. Smoke: 9 перевірок

```bash
bash deploy/scripts/smoke.sh --base http://127.0.0.1:9119 --env-file /opt/lampa/staging-data/env.staging
```

| # | Перевірка | Очікування | Факт T02 |
|---|---|---|---|
| 1 | `GET /` | 200, HTML-шел | 200, 895 B |
| 2 | `POST /admin/login` (тестовий пароль) | 200 `success=true` + cookie `auth_token` | 200, cookie=yes |
| 3 | `GET /api/value?key=head_broadcast` (admin cookie) | 200 JSON | 200, `str` |
| 4 | `GET /api/profiles/all` | 200, 3 профілі, main присутній | 3 (Home main=1) |
| 5 | `GET /api/admin/devices` | 200, JSON-список | 7–8, усі `allowed` |
| 6 | `GET /api/bookmarks/dump` (header `profile`) | 200 `text/plain`, тіло — валідний JSON | 5 bookmarks, version=206 |
| 7 | `GET /api/timeline/changelog?since=…` | 200 JSON, числовий `version` | version=4413 |
| 8 | `POST /api/pin/verify` (staging TOTP) | 200 `ok=true` + `device_token` | 200, token issued |
| 9 | `GET /api/value?key=menu_data` з test `device_token` | 200; без cookie — 401 `token_required` | 200, control=401 |

Час: readiness ≈0.8–1.5 с після старту, сам smoke ≈74–81 мс (9/9).
Повторний прогін — також 9/9 (перевірено 2026-11-25T20:44:49Z).

Ідемпотентність: усі перевірки — читання, крім PIN verify, який лише оновлює окремий
рядок `staging-pin-probe` новим випадковим токеном. Статичний test device token не
інвалідовується, тому smoke можна ганяти повторно.

Коди виходу: `0` — 9/9; `1` — є FAIL; `2` — немає env-файлу / стенд не відповідає.
Додатково: `--json-out FILE` (машиночитний підсумок, без секретів), `--timeout` (на запит),
`--ready-timeout` (очікування старту).

## 8. Стан для T03 (golden fixtures)

Стенд лишається піднятим після T02:

| Показник | Значення |
|---|---|
| Base URL | `http://127.0.0.1:9119` |
| Container | `lampa-staging` (`running`, `RestartCount=0`) |
| Image | pinned digest `sha256:d678c142…21add7` (= прод-образ) |
| DB | `/opt/lampa/staging-data/database.sqlite`, schema_version=7, journal_mode=delete, integrity_check=ok |
| Env | `/opt/lampa/staging-data/env.staging` (0600) |

Для захоплення фікстур: логін через `POST /admin/login` (пароль з env-файлу) → cookie
`auth_token`; для device-ендпойнтів — `Cookie: device_token=<LAMPA_STAGING_DEVICE_TOKEN>`.
Правила гігієни: маскувати `admin_password`/`admin_token` у `/api/settings`; не писати
значення з env у фікстури/логи; не робити мутацій (`bookmarks/add|remove|clear|sync`,
`timeline/update`, `profiles/*`), щоб копія лишалася стабільною для differential-тестів.

## 9. Діагностика

| Симптом | Причина / дія |
|---|---|
| `db_copy: refusing --dest inside the production data directory` | захист; вкажіть `/opt/lampa/staging-data/...` |
| `container lampa-staging is running … --no-restart` | приберіть `--no-restart` (скрипт сам зупинить/підніме) |
| smoke: `is not answering after 60 s` | `docker ps`/`docker logs --tail 50 lampa-staging`; образ локально? (`pull_policy: never`) |
| `prepare_staging_db: staging DB not found` | спершу `db_copy.sh` |
| PIN FAIL | перевірте `TOTP_SECRET` в env-файлі (base32) і системний час (вікно ±30 с) |
| 401 `invalid_token` на device-запиті | `prepare_staging_db.py` перезаписує test device; перезапустіть smoke, не змінюйте токен вручну |
| `docker compose config` показує секрети | не використовуйте; за потреби `--rotate` + recreate |

## 10. Відповідність S11 §4.1 / §9

| Вимога S11 | Реалізація |
|---|---|
| окремий project/container/порт/volume | `lampa-staging`, 9119, `/opt/lampa/staging-data` |
| копія БД через backup API, не `cp` | `db_copy.sh` (python `sqlite3.Connection.backup`, source `mode=ro`) |
| `journal_mode=delete`, schema 7, integrity ok | перевіряється `db_copy`/`prepare` (fail-closed) |
| тестові креденшели лише на стенді | `env.staging` 0600 + DML у копії |
| smoke 9/9 | `smoke.sh` (таблиця PASS/FAIL, exit 1 при FAIL) |
| прод не змінюється | §2 (Id/StartedAt/RestartCount, відсутність `-wal/-shm/-journal`) |

## 11. Атрибуція

Скрипти портовані з Proof-of-Concept S11 (`/opt/lampa/poc/s11-delivery/scripts/`):

| Тут | Джерело | Що взято |
|---|---|---|
| `db_copy.sh` | `backup_hot.py` | backup API + read-only source + retry/verify (S11 §3) |
| `prepare_staging_db.py` | `prepare_db.py` | заміна admin-значень у копії (S11 §2.1); додано test device і перевірки |
| `smoke.sh` | `smoke.py` | 9 перевірок (S11 §4.1); додано статичний test device token, негативний контроль guard, readiness і таблицю |

Деталі й цифри PoC: `review/inv/S11-delivery.md`.
