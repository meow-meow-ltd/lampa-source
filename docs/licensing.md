# Ліцензування та ліцензійні обов'язки (P0)

Статус: W0 (P0.0), 2026-11-25 · Власник: architect · Джерела: `review/20-g0-results.md` (D11), `review/inv/S5-engine.md` §9, `review/inv/S9-adapters.md`, `review/15-investigation-plan.md` §S13, `review/10-synthesis.md` (D-lampac), файли репозиторію `/opt/lampa/source`.

> Це інженерний облік ліцензій і обов'язків, не юридичний висновок. Рішення про публікацію/розповсюдження ухвалює власник після аудиту (D11). Префікс `review/` = `/opt/lampa/review` (доказова база G0 поза git-репо).

## 1. Поточний стан репозиторію

| Артефакт | Факт | Доказ |
|---|---|---|
| `LICENSE` | Текст **GNU GPL v2** (June 1991), успадкований від upstream Lampa | `head -1 LICENSE` → `GNU GENERAL PUBLIC LICENSE`, `Version 2, June 1991` |
| `package.json` | `"license": "ISC"` — суперечить `LICENSE` | `package.json:13` |
| Новий Rust-код (`server/`) | Власних SPDX-заголовків немає; ліцензія продукту ще не обрана | `server/Cargo.toml` (`publish = false`), `server/rust-toolchain.toml` |
| Призначення репо | Приватне використання (D11); публікація коду/образу заборонена без аудиту | `review/10-synthesis.md` D-lampac; `review/15-investigation-plan.md` §S13 |

**Розбіжність GPL-2 vs ISC** — спадщина upstream: `LICENSE` описує весь репозиторій, а `package.json` (frontend-маніфест) декларує ISC. Для P0 це не змінюється механічно: не можна «переліцензувати» чужий код одним рядком. Новий Rust-код пишеться як окремий твір; його ліцензія — рішення власника (L1 у §6), потрібне лише перед розповсюдженням.

## 2. Рішення D11: приватне використання

**Рішення власника (2026-11-25):** проєкт приватний, для себе; нічого не продається і не розповсюджується. Тому:

- можна читати й адаптувати код Lampac NextGen (AGPL-3.0), JacRed (AGPL-3.0), Jackett (GPL-2.0), зокрема YAML-дефініції — як довідку/дані; clean-room не потрібен;
- **репо й образ залишаються приватними**; публікація (код, образ, GHCR-пакет, релізи) — лише після ліцензійного аудиту й окремого рішення власника;
- похідні AGPL/GPL-частини не можна публікувати під іншою ліцензією без перегляду всього проєкту (AGPL-3.0 або видалення похідних частин);
- у приватному використанні додаткових дій (source offer, notices для зовнішніх отримувачів) не потрібно, але provenance зберігаємо для майбутнього аудиту.

Доказова база: `review/10-synthesis.md` (рядок D-lampac), `review/15-investigation-plan.md` §S13 (рядки 265–293). **Прогалина:** G0 посилається на «S13», але файлу `review/inv/S13-lampac.md` у доказовій базі немає; рішення зафіксоване лише в синтезі/плані. Окремий S13-звіт не вигадуємо — це відкрите питання доказової бази (О14 у `docs/decisions.md`).

## 3. Вендорені та runtime-компоненти

| Компонент | Версія / коміт | Ліцензія | Що беремо | Куди |
|---|---|---|---|---|
| rqbit / librqbit | `f23bacd` (workspace 9.1.0) | **Apache-2.0** | engine + патч B (RAM-режим, ~930 рядків / 12 файлів) | `server/vendor/rqbit/` |
| TorrStream Lite (TSL) | `d007de1` (v0.2.11) | **MIT** | порт `cache.rs`/`range.rs` та ідеї хуків | `server/src/torrent/` (+ NOTICE) |
| dman-search | 0.1.0, git `786d6064` | **MIT OR Apache-2.0** | декларативні трекери + патчі charset/`torrent_url` | `server/vendor/dman-search/` |
| Трекер-дефініції (наші) | W0–W1 | наш конфіг (факти/селектори) | 6 TOML-дефініцій | `server/src/trackers/definitions/` |
| ffmpeg / ffprobe | Debian 5.1.9-0+deb12u1 | GPL-2+ (Debian copyright: бінарники GPL v2+) | окремий процес, не лінкування | runtime-образ |
| Debian base, tini, Rust-крейти | bookworm-slim; lockfile | різні (MIT/Apache-2.0/BSD тощо) | runtime/збірка | аудит перед публікацією |

Джерела: `review/inv/S5-engine.md` §1/§8.1/§9 (rqbit, TSL, обсяг патчу), `review/inv/S9-adapters.md` (dman-search 0.1.0, ліцензія, патчі), `review/30-final-plan.md` §2.5/§7.1/§8.2.

### 3.1 rqbit — Apache-2.0

Обов'язки за Apache-2.0 §4:

1. дати отримувачам копію ліцензії — зберегти `LICENSE` upstream у `server/vendor/rqbit/LICENSE`;
2. зберегти copyright/patent/trademark/attribution-повідомлення (Copyright 2021 Igor Katson);
3. **змінені файли супроводити помітним повідомленням, що їх змінено** (§4(b)) — шапка в кожному з 12 файлів патчу B + `server/vendor/UPSTREAM.md` з переліком змін;
4. `NOTICE`-файлу в upstream немає (`S5` §9), тому додаткового NOTICE-поширення не вимагається; наш власний `NOTICE` з атрибуцією та переліком змін — добра практика, не заміна upstream-ліцензії.

### 3.2 TorrStream Lite — MIT

Обов'язки MIT: зберегти copyright (`Copyright (c) 2026 Andrei Eliseev`) і текст дозволу в усіх копіях/істотних частинах. Для порту `cache.rs`/`range.rs`:

- покласти MIT-текст у `server/src/torrent/NOTICE` (або `server/vendor/TSL-LICENSE`) і шапку в портовані файли;
- вказати, що це порт, а не оригінал (`Ported from TorrStream Lite … @ d007de1`);
- не тягнути решту TSL (macOS UI, Keenetic, `memory.rs`) — це не наш продукт.

### 3.3 dman-search — MIT OR Apache-2.0

- Обрати **Apache-2.0** як основний маршрут для вендореної копії (сумісний з rqbit) і зберегти обидва тексти, якщо беремо код під обома;
- у crates.io-пакеті `LICENSE-MIT`/`LICENSE-APACHE` **не постачаються** (у `dman-search/` їх немає, README посилається на файли репо) — при вендорингу взяти їх з upstream `https://github.com/rajput-hemant/dman` на коміті `786d6064f3f3952536928d66bad078db5b4f03e1`, `path_in_vcs=crates/dman-search`;
- патчі (`encoding_rs` ~15 рядків, `torrent_url` ~10 рядків) позначити як modified (Apache §4(b)); `server/vendor/UPSTREAM.md` — перелік;
- вбудовані дефініції (`builtin.toml`, зокрема Knaben) входять у крейт і покриваються його ліцензією; наші `providers.toml` — окремий твір.

### 3.4 Трекер-дефініції та довідники

- Наші 6 TOML-дефініцій (`rutor`, `nnmclub`, `megapeer`, `bitru`, `toloka`, `mazepa`) написані за публічною HTML/JSON-структурою сайтів; це конфіг, а не скопійований код.
- **Jackett-дефініції — GPL-2.0**: у P0 не копіюємо (лише довідка). Якщо колись копіювати — це похідний GPL-2.0-матеріал; рішення власника (L2), репо лишається приватним.
- **JacRed-каталог 25 трекерів — AGPL-3.0**: лише довідник назв/пріоритетів; код/модель індексу не беремо (`review/15-investigation-plan.md` §S13; D12).

### 3.5 ffmpeg (runtime)

- Debian-пакет `ffmpeg 5.1.9-0+deb12u1`; `/usr/share/doc/ffmpeg/copyright` у образі: «resulting binaries are licensed under GPL v2+» (окремі частини — GPL-3+, документація — GPL-3+).
- Ми запускаємо ffmpeg **дочірнім процесом** (D2), не лінкуємо; для приватного використання обов'язків розповсюдження немає.
- **Перед публікацією образу** — окремий пункт аудиту: GPL source offer / corresponding source для ffmpeg-бінарників, повний перелік пакетів і їхніх ліцензій, `/usr/share/doc/*/copyright`.

## 4. Обов'язки під час вендорингу (P0/W2)

1. **Копія ліцензії** кожного вендореного компонента поруч із кодом (`server/vendor/<name>/LICENSE`).
2. **Збереження copyright/attribution** — не видаляти шапки upstream.
3. **Позначка modified** для кожного зміненого файлу (Apache §4(b)); єдиний формат — у `docs/vendor-notices.md`.
4. **`server/vendor/UPSTREAM.md`** — точний коміт, `git diff --stat upstream..vendor`, перелік файлів/змін; оновлюється при кожному оновленні піна.
5. **Агрегований `THIRD_PARTY_NOTICES`** (за потреби) + посилання на ліцензії в README/образі.
6. **Не змішувати GPL-код** (Lampac/JacRed/Jackett) з нашим без рішення власника; AGPL-похідні не потрапляють у публічний образ.
7. **Не публікувати** репо/образ/GHCR без аудиту (D11).
8. **Секрети** (cookie трекерів, токени) — не в репо/логах/фікстурах; це і ліцензійна гігієна даних.

## 5. Публікація: gate-чеклист (не для W0)

- [ ] Обрано ліцензію продукту (L1) і додано SPDX/COPYING.
- [ ] Аудит усіх пакетів runtime-образу (Debian + ffmpeg + tini + Rust-крейти) з `copyright`-файлів.
- [ ] GPL/AGPL-похідні або видалені, або проєкт повністю переліцензовано (AGPL-3.0).
- [ ] Повні тексти MIT/Apache-2.0 для rqbit/TSL/dman; modified-позначки; `UPSTREAM.md`.
- [ ] Source offer / corresponding source для GPL-бінарників; перевірка SumOfParts.
- [ ] Рішення власника про публікацію зафіксоване в `docs/decisions.md`.

## 6. Відкриті питання (ліцензійні)

| ID | Питання | Власник | Наступний крок |
|---|---|---|---|
| L1 | Ліцензія продукту для нового Rust-коду (GPL-2? ISC? proprietary?) | власник | лише перед публікацією; зараз — private use (D11) |
| L2 | Jackett-дефініції, якщо колись копіювати (GPL-2.0) | власник | рішення до копіювання; інакше — власні TOML |
| L3 | Механіка GPL source offer для ffmpeg в образі | власник + devops | аудит перед публікацією (§5) |
| L4 | Тексти `LICENSE-MIT`/`LICENSE-APACHE` dman-search | implementer-torrent | взяти з upstream-коміту при вендорингу (T15/T28) |
| L5 | Відсутній `review/inv/S13-lampac.md` (доказова база D11) | architect/капітан | не вигадувати; зафіксовано як прогалину (О14) |
