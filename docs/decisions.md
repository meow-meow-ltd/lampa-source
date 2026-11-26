# Рішення D1–D13 і W0-рішення / відкриті питання (P0)

Статус: W0 (P0.0), 2026-11-25 · Власник: architect · Джерело істини: `review/20-g0-results.md` (D1–D13), повні звіти — `review/inv/S1…S12`, план — `review/30-final-plan.md`.

> `review/` = `/opt/lampa/review` (доказова база G0 поза git-репо). Записи нижче — короткі ADR: рішення, evidence, наслідки, статус. Повні обґрунтування не дублюються — вони в S-файлах і плані. Рішення, яких немає в evidence, сюди не вносяться; суперечності — у таблиці відкритих питань (`О*`).

## 1. Зведення D1–D13

| ID | Рішення | Статус |
|---|---|---|
| D1 | Публічний контракт = upstream-сумісний same-origin фасад | fixed (G0) |
| D2 | Медіарушій = ffmpeg (дочірній процес), не GStreamer | fixed (G0) |
| D3 | Торрент-рушій = rqbit @f23bacd + порт RAM-патчу TSL (варіант B) | fixed (G0) |
| D4 | Бюджет = контейнер 2 GiB / 4 vCPU, кеш 256 MiB (512 opt-in) | fixed (G0); enforcement — О12 |
| D5 | Життєвий цикл = одна довгоживуча Session + lease на сесію | fixed (G0) |
| D6 | Auth = B (`lampa_session` HttpOnly + `/api/auth/session`) через перехідний C | fixed (G0) |
| D7 | Доступ = LAN-first playback + Tailscale remote; публічний tunnel лише UI/API | fixed (G0); дії власника — О4 |
| D8 | Трекери = власні Rust-адаптери (Torznab-сумісні), без Jackett/JacRed/Prowlarr | fixed (G0) |
| D9 | Дані = SQLite без зміни схеми в першому релізі; drift 0 | fixed (G0) |
| D10 | Доставка = pinned digest + rollback-тег; prune прибрати | fixed (G0) |
| D11 | Ліцензійна межа = приватне використання; репо/образ приватні | fixed (owner, 2026-11-25) |
| D12 | Пошук = live-пошук власними адаптерами, без локального індексу | fixed (G0) |
| D13 | CF-транспорт (FlareSolverr) = відкладено (P2) | fixed (G0) |

---

## D1 — Публічний контракт: upstream-сумісний same-origin фасад

- **Рішення.** Клієнт Lampa не переписуємо; публічний контракт = TorrServer-сумісний same-origin фасад (12 P0-позицій S12 + топологія S1 = 16 маршрутів) плюс паритет 27 legacy `/api/*`. Розбіжності S1 vs S12 вирішуються на користь живого reference (S1), кожна — свідомо, з причиною й тестом.
- **Evidence.** `review/inv/S12-contract.md` §2–§4 (21 позиція: 12 P0 + 9 defer), §8 (чекер: `21 endpoints: 12 p0, 9 defer; 1 marker` → 0 errors/0 warnings, exit 0; 20 JSON-фікстур), §9/§11; `review/inv/S1-oracle.md` §1 (141 wire-запис), §2–§3 (живі відповіді та 13 розбіжностей; головні — `/gst/echo`, `/stream?preload|stat`, `/audio`, HLS-топологія).
- **Наслідки.** `compat/` відтворює S1-поведінку, не «ідеальний» DTO S12; кожен ендпойнт має фікстуру; drift-checker у CI; нові поля/кейсинг без рішення заборонені (план §15.3 п.2). 27 legacy API — окремий golden-набір (О1).
- **Статус.** fixed (G0, 2026-11-25).

## D2 — Медіарушій: ffmpeg як дочірній процес, не GStreamer

- **Рішення.** HLS-сегменти генерує `ffmpeg` (дочірній процес), відео — `-c:v copy` (без перекодування), аудіо — AAC-LC; ffprobe — для probe/доріжок. GStreamer у продукт не входить.
- **Evidence.** `review/inv/S6-image.md` §3 (ffmpeg-образ: 763 MB / 62.0 с холодна збірка; GStreamer: 969 MB / 82.8 с), §6 (конверсія `-c:v copy -c:a aac` — відео не перекодовується); `review/inv/S3-media.md` §5 (VCL SHA-256 збігається з джерелом), §7 (seek TTFB p95 = 355.8 мс, повний сегмент p95 ≤ 397.3 мс), §9 (ресурси сегмента).
- **Наслідки.** В образі `ffmpeg/ffprobe 5.1.9`; один тулчейн; `/gst/echo` віддає 200 лише після реального self-test (інакше 404 — R5); CI перевіряє VCL SHA-256, а не `codec_name` (план §6.7); GStreamer-кандидат відхилено.
- **Статус.** fixed (G0, 2026-11-25).

## D3 — Торрент-рушій: rqbit @f23bacd + RAM-патч TSL (варіант B)

- **Рішення.** Вендоримо upstream `librqbit` @`f23bacd` (workspace 9.1.0) і портуємо RAM-патч TorrStream Lite у **варіанті B** (~930 рядків / 12 файлів): eviction/HAVE, reservation-before-request, quiet peers, prune/takeover, `ipv4_only`. Без DHT, upload, inbound listen.
- **Evidence.** `review/inv/S5-engine.md` §4 (upstream не інвалідує HAVE → без патчу неможливо), §8.1 (точна дельта), §9 (ліцензії), §10 (варіанти A/B/C/D), §11 (рекомендація B); `review/inv/S4-seekable-source.md` §3.2–§3.3 (0 stale HAVE, 0 байтових розбіжностей SHA-256, 2226 витіснень), §3.4 (seek p95 4.6 мс loopback / 721 мс при 100 Mbps на 8 MiB piece).
- **Наслідки.** `server/vendor/rqbit/` + `UPSTREAM.md` + modified-шапки (Apache §4(b), див. `docs/vendor-notices.md`); пін не гнати за upstream (R3); `ipv4_only` обов'язковий у Docker; piece-гранулярність seek — прийнятий ризик R1/R2 зі stop-condition.
- **Статус.** fixed (G0, 2026-11-25).

## D4 — Бюджет RAM/CPU: 2 GiB / 4 vCPU, кеш 256 MiB (512 opt-in)

- **Рішення.** Контейнер: `memory.max=2 GiB`, `cpu.max=400000/100000` (4 vCPU), `pids.max=256`, `nofile=8192`, swap ≤256 MiB; застосунок: `cache_global_max=256 MiB` (512 — opt-in), `max_active_playbacks=5` (6-та → 503 + `Retry-After: 30`), `max_concurrent_transcodes=3` + 1 probe, 1 сегмент/сесію; idle-ціль ≤64 MiB.
- **Evidence.** `review/inv/S7-budget.md` §6.2 (5 стрес: 646 MiB при кеші 256 / 902 MiB при 512; steady 581/837), §7.1 (таблиця лімітів і обґрунтування 2 GiB = 2.2× над 902), §7.2 (внутрішній soft-бюджет), §8 (вердикт: 902 MiB = 11.0 % від 8 GiB).
- **Наслідки.** Admission-контроль і семплер cgroup у P0; `memory.high=1280 MiB` через compose напряму не виставити (Docker 29: `--memory-reservation` → `memory.low`, а не `memory.high`) — спосіб фіксації в staging/prod є відкритим питанням О12; перемір G6 (план §15.2).
- **Статус.** fixed (G0); enforcement — О12.

## D5 — Життєвий цикл: одна довгоживуча Session + lease на сесію

- **Рішення.** Один `EngineHandle` з однією довгоживучою `rqbit Session`; на кожне відтворення — lease (резервація ресурсів, stop/drop/rem за семантикою S1). Окремі сесії на запит не створюються.
- **Evidence.** `review/inv/S5-engine.md` §6.1–§6.3 (SessionOptions, зупинка/видалення без перезапуску сесії, робоча конфігурація TSL), §6.2 (drop/rem); `review/inv/S4-seekable-source.md` §3.8 (lifecycle delete + re-add: S4-9 PASS).
- **Наслідки.** Reaper лизів кожні 5 с; graceful shutdown: перестати приймати запити → скасувати ffmpeg → дочекатися лизів ≤10 с → закрити пул (план §2.4); stop/drop/rem перевіряються на S1-семантиці (tombstone, lazy reopen).
- **Статус.** fixed (G0, 2026-11-25).

## D6 — Auth: варіант B через перехідний C

- **Рішення.** Цільовий стан — B: HttpOnly `lampa_session` + `GET /api/auth/session` + мінімальний патч клієнта (3 файли, ~10 рядків). Викатка — через C: сервер приймає і legacy `device_token`, і `lampa_session`, видає обидва тим самим значенням; після ≥7 днів і підтвердження нового бандла — `DEVICE_LEGACY_COOKIE=0`.
- **Evidence.** `review/inv/S8-auth.md` §4.3 (B: сервер + `/api/auth/session`), §4.4 (C: обидва cookie, старі клієнти працюють), §6 (варіанти/ризики/відкат), §9 (план без re-pair; `token_hash` не змінюється), §10 (Safari/TV). Мінімальний патч: `src/app.js:788-794`, `src/utils/reguest.js:560`, `src/interaction/pin_gate.js:202`.
- **Наслідки.** Немає re-pair; `HttpOnly` **не** на імені `device_token` (антипатерн B-naive); `SameSite=Lax`, `Secure` лише на HTTPS; media-auth на кожен запит (`/gst/*`, `/stream/*`, сегменти), `last_seen_at` ≤1/30 с; revoke на живому сегменті — гейт G2.
- **Статус.** fixed (G0, 2026-11-25).

## D7 — Доступ: LAN-first playback + Tailscale; публічний tunnel лише UI/API

- **Рішення.** Вдома playback — LAN `http://192.168.100.x:9118` (без CF/TLS). Remote playback — Tailscale subnet route на ту саму LAN-URL (той самий origin → той самий PIN, без re-pair). Публічний Cloudflare tunnel — лише UI/API/адмінка; відео через CF — заборонено. CF private route + WARP — відхилено для P0.
- **Evidence.** `review/inv/S10-remote.md` §0 (TL;DR), §4.1 (CF Free: відео-політика), §6.1/§6.2 (порівняння варіантів і origin/cookie-матриця), §7.1 (рекомендація P0), §8.1 (що потрібно від власника: TV, route/ACL, hotspot).
- **Наслідки.** Медіа-запити з `CF-Connecting-IP` → 403 (план §3.6 п.11); tunnel не чіпаємо в W0; власник має встановити Tailscale на TV і підтвердити route/ACL; Tailscale Serve/Funnel не використовуємо.
- **Статус.** fixed (G0); дії власника — О4.

## D8 — Трекери: власні Rust-адаптери на dman-search, без Jackett/JacRed/Prowlarr

- **Рішення.** Вендоримо `dman-search 0.1.0` (MIT OR Apache-2.0) + наші TOML-дефініції; P0-набір — 6 трекерів (`rutor`, `nnmclub`, `megapeer`, `bitru`, `toloka`, `mazepa`); Torznab/Jackett-JSON віддаємо самі. Jackett/Prowlarr/JacRed як сервіси не ставимо.
- **Evidence.** `review/inv/S9-adapters.md` (## Результати: rutor 100/42 мс, nnmclub 50/3.2 с, megapeer 43, bitru 30; ## Cookie-трекер `toloka` 46/752–979 мс; ## Cookie-трекер `mazepa` 31/85 мс; ## RAM-вимір: 11.7 MB peak), `review/inv/S5-engine.md` §9 (ліцензії), план §8.1/§8.2.
- **Наслідки.** Патчі dman-search: charset `encoding_rs` (~15 рядків) + `torrent_url` (~10 рядків); cookie-секрети шифруються, write-only, не в API/логах; дедлайн пошуку 4.5 с, partial results; без локального індексу (D12); `bitru` без magnet/hash — рішення в імплементації (P1-нюанс).
- **Статус.** fixed (G0, 2026-11-25).

## D9 — Дані: SQLite без зміни схеми в першому релізі, drift 0

- **Рішення.** Прод-схема v7 не змінюється: жодного DDL, WAL, `ANALYZE`, `VACUUM`, `_sqlx_migrations`, нових таблиць/стовпців. Один writer (`max_connections=1`), `busy_timeout=5000`, `foreign_keys=OFF`, `synchronous=FULL`, `cache_size=-2000`, `temp_store=FILE`; стартовий assert `journal_mode=delete`; fail-closed на schema≠7/невідому БД.
- **Evidence.** `review/inv/S2-sqlite.md` §3 (`journal_mode=delete`, `schema_version=7` vs `PRAGMA schema_version=23`), §4 (DDL тотожний «свіжій» v7: 23/23 об'єкти, 10 таблиць + `sqlite_sequence`), §6 (backup API: 0.04–0.05 с; restore 57.4 мс; `cp` небезпечний під писарем), §8 (R1–R4, R10), §10.
- **Наслідки.** SQLx runtime-запити без макросів; backup — лише SQLite backup API (`rusqlite::backup`), не `cp`; additive-only; будь-яка потреба DDL у P0 — stop (R7); Node читає Rust-записи після відкату.
- **Статус.** fixed (G0, 2026-11-25).

## D10 — Доставка: pinned digest + rollback-тег, prune прибрати

- **Рішення.** Деплой за immutable digest (`@sha256:…`), не за `:latest`; перед pull — rollback-тег `lampa:prev-<UTC>` на поточний образ; `docker image prune -f` з release-джоби прибрати; окрема maintenance-задача з `--filter "until=168h"`, ніколи `-a`.
- **Evidence.** `review/inv/S11-delivery.md` §4 (cutover: готовність 1460 мс, вікно 2421 мс; rollback: 1483 мс, вікно 2419 мс; дані після cutover ідентичні, крім `device_access`), §5 (prune після руху `:latest` видаляє тег і RepoDigest → offline-відкат неможливий: `inspect` rc=1, `run --pull=never` rc=125).
- **Наслідки.** CI має віддавати digest назовні; deploy-job чекає `/readyz`; вікно відкату 24–72 год; rollback-теги не prune-яться; S11-репетиція на Rust-образі — G6.
- **Статус.** fixed (G0, 2026-11-25).

## D11 — Ліцензійна межа: приватне використання, репо/образ приватні

- **Рішення.** Проєкт приватний, для себе; нічого не продається/не розповсюджується. Можна читати й адаптувати код Lampac/JacRed (AGPL-3.0) і Jackett-дефініції (GPL-2.0) як довідку/дані; clean-room не потрібен. **Репо й образ приватні**; будь-яка публікація — лише після ліцензійного аудиту й рішення власника.
- **Evidence.** Рішення власника 2026-11-25: `review/10-synthesis.md` (рядок D-lampac), `review/15-investigation-plan.md` §S13 (рядки 265–293). **Прогалина:** окремого `review/inv/S13-lampac.md` немає — G0 посилається на S13, але файл не створено; звіт не вигадуємо (О14).
- **Наслідки.** Ліцензійні обов'язки rqbit/TSL/dman зберігаємо для аудиту (`docs/licensing.md`, `docs/vendor-notices.md`); GPL/AGPL-код не змішуємо з нашим без рішення; публікація GHCR/репо — gate-чеклист `docs/licensing.md` §5.
- **Статус.** fixed (owner, 2026-11-25).

## D12 — Пошук: live-пошук без локального індексу

- **Рішення.** Пошук виконується на запит клієнта власними адаптерами (D8), без локального індексу/кроулінгу/БД; Jackett/Prowlarr/JacRed не ставимо; каталог 25 трекерів JacRed — лише довідник пріоритетів.
- **Evidence.** `review/inv/S9-adapters.md` (## Результати, ## Розширений прогін, ## Висновок: «готовий движок + декларативні дефініції працює»), `review/15-investigation-plan.md` §S13 (каталог — довідник, модель індексу не беремо), план §8.4/§8.5.
- **Наслідки.** Немає crawler-БД і фонових індексаторів; кеш відповідей 60–120 с; дедлайн 4.5 с; деградація до partial results (R10); Torznab-мапінг і drift-checker у CI.
- **Статус.** fixed (G0, 2026-11-25).

## D13 — CF-транспорт (FlareSolverr): відкладено (P2)

- **Рішення.** FlareSolverr/headless Chromium у P0 не беремо; `rutracker`/`kinozal` (Cloudflare-челендж) — P2; орієнтуємось на 6 робочих трекерів. `mazepa` — `cf_clearance` вручну/адмінкою.
- **Evidence.** `review/inv/S9-adapters.md` ## `rutracker` (curl/curl-impersonate → 403; FlareSolverr → 200; офлайн-парсинг 50 результатів), ## RAM-вимір і рішення щодо FlareSolverr (92 MB idle, до ~500 MB під челенджем; рішення власника — відкласти), план §8.1.
- **Наслідки.** 2 трекери поза P0; жодного Chromium-sidecar; RAM-бюджет D4 не включає FlareSolverr; повернення — P2 з окремим рішенням власника.
- **Статус.** fixed (G0, 2026-11-25).

---

## 2. W0 implementation decisions

Рішення, ухвалені під час W0 (не з G0). Кожне — з фактичним доказом; артефакти, які ще не існували на момент фіксації, позначені явно й не видаються за перевірені.

### W0-1 — Toolchain: Rust 1.100.0 у контейнері, не на хості

- **Рішення.** На хості Rust немає і не ставимо. Локальні збірки — в контейнері `rust:1-slim` (1.100.0); для кешу `target/`/`CARGO_HOME` — builder-образ на тому самому тегу (T01); CI — `rust:1.100-bookworm` (та сама glibc, що runtime). `Cargo.lock` комітиться, збірка `--locked`.
- **Evidence.** `command -v cargo` / `command -v rustc` → порожньо (хост без Rust); `docker run --rm rust:1-slim rustc --version` → `rustc 1.100.0 (0ef1bc386 2026-11-09)`, `cargo 1.100.0`; `docker image inspect rust:1-slim` → digest `sha256:3b199fd6…`; builder-образ `lampa-rust-builder:1.100.0` (id `35dce0677e34`) зібраний з `rust:1-slim` + `rustfmt`/`clippy` (`server/README.md`, «Docker-recipe»); повний локальний чек-чейн — `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && cargo build --release --locked` у цьому образі (`server/README.md`); `server/rust-toolchain.toml` → `channel = "1.100.0"`, components `rustfmt`, `clippy`; `server/Cargo.lock` існує (T01).
- **Наслідки.** Версія однакова локально/в CI; dman-search вимагає ≥1.97, TSL ≥1.94 (план §2.5) — 1.100.0 задовольняє; хост не модифікуємо.
- **Статус.** fixed; builder-образ — T01.

### W0-2 — Staging: 127.0.0.1:9119, дані поза репо, копія через SQLite backup API

- **Рішення.** Node-staging — окремий compose-проєкт `lampa-staging` на `127.0.0.1:9119`; дані — `/opt/lampa/staging-data` (**поза git-репо**); БД — копія прода через **SQLite backup API** з read-only джерела (не `cp`, не `rsync`), з `integrity_check`, `schema_version=7`, `journal_mode=delete`, mode 0600; `/opt/lampa/data` не монтується; прод-контейнер не чіпаємо.
- **Evidence.** `deploy/compose.staging.yaml` (bind `/opt/lampa/staging-data:/data`, `127.0.0.1:9119:3000`, `pull_policy: never`, 2 GiB/4 CPU); `deploy/scripts/db_copy.sh` (SQLite backup API, guard від `--dest` у прод-каталозі); факти: `docker ps` → `lampa-staging 127.0.0.1:9119->3000/tcp Up`; staging DB: 8 110 080 B, mode 600, `journal_mode=delete`, `schema_version=7`, `integrity_check=ok`, 10 таблиць; прод-БД mtime 19:32 не змінився після копії 20:44; джерело рішення — `review/inv/S11-delivery.md` §2, `review/inv/S2-sqlite.md` §6, план §11.4. Smoke 9/9 — `docs/staging.md` §7 (74–81 мс, 2026-11-25T20:44:49Z) і незалежний повторний прогін `bash deploy/scripts/smoke.sh --base http://127.0.0.1:9119 --env-file /opt/lampa/staging-data/env.staging` → `9/9 PASS`, 56.6 мс, exit 0 (2026-11-25T20:45:53Z).
- **Наслідки.** Прод-volume недосяжний зі staging; копія консистентна; staging-секрети — лише `/opt/lampa/staging-data/env.staging` (0600, поза git); `docker compose config` **розкриває** env_file у stdout — не запускати в логованих сесіях (`docs/staging.md` §6); golden-фікстури з цієї БД **не** знімаються (там прод-дані) — див. W0-3.
- **Статус.** fixed; smoke 9/9 підтверджено (T02 + повторний прогін).

### W0-3 — Golden-фікстури: синтетична БД, не прод-копія

- **Рішення.** 27 golden-фікстур legacy `/api/*` знімаються з Node-staging, піднятого на **синтетичній** БД (schema v7 + детермінований seed), а не з копії прода. Причини: privacy (у прод-БД профілі/закладки/історія), розмір фікстур, відтворюваність. Секрети/токени/cookie у фікстурах вирізаються; `device_access` — поза контрактом (S11 §4.2).
- **Evidence.** План §13.2 T03 (status/headers/body для 27, секрети вирізані), `review/inv/S12-contract.md` §1–§2 (підхід «мінімум, який споживає клієнт»), `review/inv/S11-delivery.md` §4.1–§4.2 (smoke, порівняння таблиць); рішення зафіксоване капітаном W0. Гігієна захоплення — `docs/staging.md` §8: маскувати `admin_password`/`admin_token` у `/api/settings`, не писати env-значення у фікстури/логи, не робити мутацій (bookmarks/timeline/profiles), щоб копія лишалася стабільною. Артефакт T03 (`server/tests/fixtures/api/**`, `deploy/scripts/capture_api_golden.py`) — у роботі; G1 перевіряє 27 файлів.
- **Наслідки.** У git не потрапляють прод-дані; синтетика може не покрити рідкісні прод-стани — О13; W1 differential (G2) за потреби розширює seed.
- **Статус.** fixed; артефакт — T03/G1.

### W0-4 — Vitest baseline: 4 failed files / 3 failed tests / 171 passed

- **Рішення.** Фіксуємо поточний baseline і не допускаємо **нових** падінь; лагодження upstream-тестів — не P0 (план §11.2).
- **Evidence.** `cd /opt/lampa/source && npx vitest run` (2026-11-25, Node v22.23.2) → exit 1; `Test Files 4 failed | 11 passed (15)`, `Tests 3 failed | 171 passed (174)`; failed: `spec/account_timeline.spec.js`, `spec/account_backup.spec.js`, `spec/episode_menu.spec.js`, `spec/media_loading.spec.js`.
- **Наслідки.** CI порівнює з baseline (не «зелений з нуля»); нові падіння — блокер; старі — відомі, не блокують G1.
- **Статус.** fixed.

### W0-5 — G1 «CI зелений» = локальний `scripts/ci/run.sh`, не remote Actions

- **Рішення.** У W0 push/PR заборонений, тому G1 «CI зелений» означає **локальний** прогін `scripts/ci/run.sh` (fmt, clippy `-D warnings`, `cargo test`, contract-фікстури, vitest baseline, frontend build) з exit 0. GitHub Actions workflow додається, але **не запускається**; remote-run потребує окремого дозволу власника (О11).
- **Evidence.** План §11.2 (job `pr-verify`: ті самі кроки), W0-обмеження (без push/PR); артефакт T05 (`.github/workflows/ci.yml`, `scripts/ci/run.sh`) — у роботі, G1 запускає скрипт.
- **Наслідки.** «Зелений CI» не можна заявляти за GitHub Actions у W0; reviewer фіксує команду/exit code локального скрипта; remote CI — питання О11.
- **Статус.** fixed; артефакт — T05/G1.

### W0-6 — `schema7.sql`: test-only фікстура, не міграція

- **Рішення.** SQL-файл, що відтворює схему v7 для синтетичних/тестових БД (`server/tests/fixtures/schema7.sql`, точний шлях — T04), — **тільки тестова фікстура**. Він не застосовується до прод-БД, не є міграцією, не створює `_sqlx_migrations`; runtime не виконує DDL.
- **Evidence.** `review/inv/S2-sqlite.md` §4 (DDL v7: 23/23 об'єкти, 10 таблиць + `sqlite_sequence`), §8 R4 (`_sqlx_migrations`/міграції заборонені); план §4.3/§4.5. Артефакт T04.
- **Наслідки.** Синтетична БД для golden/unit-тестів будується з фікстури; прод лишається v7; будь-який DDL у runtime — stop R7.
- **Статус.** fixed; артефакт — T04.

### W0-7 — Жодних DDL/WAL/ANALYZE; `journal_mode=delete` assert

- **Рішення.** У P0 заборонені DDL, WAL, `ANALYZE`, `VACUUM`, нові таблиці/стовпці/індекси, підняття `schema_version`; на старті й у тестах — assert `journal_mode=delete`, інакше fail-closed.
- **Evidence.** `review/inv/S2-sqlite.md` §3, §8 R3/R4 (`journal_mode=delete` у вікні відкату; WAL ламає відкат), §10; план §4.2/§4.5, R7 (stop-condition). Факт staging-копії: `journal_mode=delete` (W0-2).
- **Наслідки.** Node-відкат читає всі Rust-записи; будь-яка ідея зміни схеми — stop для капітана; CI перевіряє незмінність `journal_mode` після записів.
- **Статус.** fixed.

## 2a. W1 team conventions — parallelism and roles

Статус: W1 (P0.1), 2026-11-25 · Власник: капітан · Причина: у W1 команда помилково розмножила ролі замість паралельних сесій; фіксуємо правило, щоб не повторити.

1. **ROLES.** Команда використовує лише 8 вбудованих ролей профілю: scout, investigator, expert, implementer, test_writer, browser_tester, reviewer, final-reviewer. Роль `implementer` рівно одна. Не створюємо варіантів ролей (backend/security/frontend-implementer): пакет роботи описується в задачі, а не в новій ролі чи імені учасника.
2. **PARALLELISM.** Один учасник може виконувати кілька незалежних готових задач паралельно в окремих parallel sessions. Не створюємо дублікатів учасників для паралелізму — незалежні задачі призначаємо тому самому учаснику. Другого `implementer` створюємо лише якщо виміряний ліміт провайдера/конкурентності справді блокує паралельні сесії, і фіксуємо це evidence.
3. **CONFLICT CONTROL.** Безпека паралелізму = неперетинні write-scope + єдиний власник на спільний файл: `Cargo.toml`/`Cargo.lock` — один власник (T15); `http/mod.rs` + `api/mod.rs` — scaffolding (I0), далі read-only; роутери монтуються один раз; API-задачі пишуть у власні файли.
4. **EVIDENCE.** W0: T04 + T05 на одному implementer; W1: T10 і T11 паралельно, далі T07/T08/T09/T12 паралельно — усі на тому самому implementer.

Практичний чекліст — `docs/w1-notes/C0.md`.

## 3. Відхилені варіанти (W0)

| Варіант | Чому відхилено |
|---|---|
| GStreamer як медіарушій | важчий (969 MB/82.8 с vs 763 MB/62.0 с), AAC усе одно через FFmpeg; D2 |
| Форк TorrStream Lite як застосунок | тягне macOS/Keenetic/їхній API; беремо лише патч B + cache/range; D3 |
| rqbit без патчу (лише власний storage) | upstream не інвалідує HAVE → RAM-only неможливий; S5 §4 |
| Jackett/Prowlarr/JacRed як сервіси | зайвий контейнер/ліцензійна вага; власні адаптери; D8/D12 |
| WAL / DDL / `_sqlx_migrations` у P0 | ламає відкат на Node; R7 |
| `cp`/`rsync` прод-БД для staging | torn state під писарем; лише SQLite backup API; S2 R1 |
| FlareSolverr у P0 | 92 MB idle, +Chromium; 6 трекерів і так працюють; D13 |
| Ремонт upstream vitest-тестів у P0 | не P0-обсяг; фіксуємо baseline; план §11.2 |
| Remote GitHub Actions run у W0 | push/PR заборонений; потребує дозволу власника; О11 |
| Прод-дані у golden-фікстурах | privacy/розмір; синтетична БД; W0-3/О13 |

## 4. Відкриті питання (О1–О14)

| ID | Питання | Власник | Наступний крок | Блокує G1? |
|---|---|---|---|---|
| О1 | Немає golden для 27 legacy `/api/*` (S12 покрив лише TorrServer-контракт) | test_writer | T03: зняти status/headers/body з Node-staging, вирізати секрети; без цього T07–T09 не стартують | так (W1) |
| О2 | `docs/decisions.md` був відсутній | architect | закрито T06 (цей файл); далі оновлюється кожною задачею | ні |
| О3 | CUB/TMDB reachability з контейнера не міряно (G0 п.3) | implementer-backend | P0.4 smoke `HEAD api.themoviedb.org` + pass-through CUB; клієнт ходить напряму | ні |
| О4 | Browser/TV e2e (Safari, Android TV 11, fMP4 vs TS) + Tailscale на TV (G0 п.1–2) | власник | T37 owner-чеклист; Tailscale APK/QR, route/ACL, fallback LAN-only; S10 §8.1 | ні (G6) |
| О5 | BitTorrent v2/hybrid; HDR/HEVC/AV1 | implementer-torrent + test_writer | hybrid на 2–3 торентах (T20/T36); HEVC/4K не обіцяємо; `allow_hevc` лише після TV/Safari (P1) | ні |
| О6 | Непорожній `/subs` (sidecar-субтитри) | test_writer | P0 — `[]`; зняти фікстуру з торента з `.srt/.ass` і реалізувати лістинг у P1 | ні |
| О7 | WAL-режим і двоє одночасних глядачів | капітан + власник | WAL — P1-рішення після вікна відкату; двоє глядачів — T26/T36; S2 §9/S7 | ні (G2/G3) |
| О8 | Зовнішні аудіодоріжки (`/audio`) | власник | P0 — 404 (reference parity); рішення на P1 з оцінкою; S12 §7.3 | ні |
| О9 | Точні версії `sqlx`, `rusqlite`, `totp-rs`, `tower-http` | implementer | T04/T10 фіксують `Cargo.lock`; перевірка TOTP-векторів і backup API на копії | так (T04) |
| О10 | Android TV browser engine невідомий; `delivery-guard` блокує `Dockerfile`/CI | власник + implementer-devops | T37 фіксує рушій і cookie-прапорці; T35 — вузький виняток/`Dockerfile.rust` | ні |
| О11 | Remote CI: GitHub Actions у W0 не запускався (push/PR заборонений) | власник | дозволити push/PR або лишити G1 локальним (`scripts/ci/run.sh`); зафіксувати рішення | так (інтерпретація G1) |
| О12 | `memory.high=1280 MiB` / `memory.swap.max=256 MiB`: compose напряму не виставляє (Docker 29: `--memory-reservation` → `memory.low`) | власник + devops | systemd drop-in `MemoryHigh`/`MemorySwapMax` або soft-бюджет у застосунку; staging зараз 2 GiB/4 vCPU; S7 §7.1 | ні (G6) |
| О13 | Privacy фікстур: синтетична БД може не покрити рідкісні прод-стани | власник + test_writer | G1 — синтетика; для W1 differential власник вирішує, чи дозволити одноразовий санітизований знімок | ні (G2 fidelity) |
| О14 | Доказова база D11: `review/inv/S13-lampac.md` відсутній (G0 посилається на S13) | architect + капітан | не вигадувати звіт; або створити S13-витяг із 10-synthesis/15 §S13, або змінити посилання G0 | ні |

## 5. Пов'язані документи

- `docs/licensing.md` — ліцензійний статус, GPL-2/ISC, обов'язки rqbit/TSL/dman, publication gate.
- `docs/vendor-notices.md` — точні notices і чеклист T15/T28.
- `review/30-final-plan.md` — повний план; `review/20-g0-results.md` — зведення G0; `review/inv/S1…S12` — докази.
- `deploy/compose.staging.yaml`, `deploy/scripts/{db_copy,smoke}.sh` — staging W0 (T02).
