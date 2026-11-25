# Vendor notices: rqbit, TorrStream Lite, dman-search (P0)

Статус: W0 (P0.0), 2026-11-25 · Власник: architect · Джерела: `review/inv/S5-engine.md` §1/§8.1/§9/§10/§11, `review/inv/S9-adapters.md`, `review/30-final-plan.md` §7.1/§8.2, PoC-артефакти `/opt/lampa/poc/{rqbit,torrstreamlite,dman-search}`.

Призначення: точні тексти/посилання та чеклист для T15 (rqbit + патч B) і T28 (dman-search + трекери) у W2. Поки нічого не вендорено — файли нижче створюються в W2; у W0 зафіксовано формат, щоб не вигадувати його під час вендорингу.

## 1. rqbit / librqbit — Apache-2.0

| Факт | Значення |
|---|---|
| Upstream | https://github.com/ikatson/rqbit |
| Пін | `f23bacdddcc5318caa813418b8eb1b1d7307faaa` (workspace 9.1.0, 2026-11-03) |
| Ліцензія | Apache License 2.0; `LICENSE` upstream: «Copyright 2021 Igor Katson» |
| `NOTICE` upstream | **відсутній** (`S5` §9) |
| Що вендоримо | workspace `librqbit` + патч B (~930 рядків / 12 файлів) |

**Точний текст атрибуції** (у `server/vendor/rqbit/NOTICE`; це наш супровідний notice, не upstream-NOTICE):

```text
rqbit / librqbit
Copyright 2021 Igor Katson
Licensed under the Apache License, Version 2.0
https://github.com/ikatson/rqbit
Pinned at f23bacdddcc5318caa813418b8eb1b1d7307faaa (workspace 9.1.0).

This copy contains modifications by the lampa-rust-p0 project for RAM-only
streaming (patch "B", 2026-11-25). See server/vendor/UPSTREAM.md for the
complete list of changed files and the reason for each change.
```

**Текст у шапці кожного зміненого файлу** (вимога Apache-2.0 §4(b)):

```rust
// Modified by the lampa-rust-p0 project (2026-11-25):
// <one-line reason>. Original: rqbit @ f23bacd, Apache-2.0.
```

**Ліцензія**: скопіювати `LICENSE` upstream без змін у `server/vendor/rqbit/LICENSE` (повний текст Apache-2.0; онлайн-копія: https://www.apache.org/licenses/LICENSE-2.0).

**`server/vendor/UPSTREAM.md`** (обов'язково; T15):
- upstream URL + точний коміт `f23bacd`;
- `git diff --stat f23bacd..vendor` (12 файлів, ~930 рядків);
- перелік файлів і що змінено: `piece_tracker`, `live/mod`, `streaming`, `storage/mod`, `chunk_tracker`, `read_buf`, `peer_connection`, `session`, `initializing` + інші з S5 §10/§11;
- чому (RAM-only eviction/HAVE, quiet peers, prune, ipv4_only);
- як оновлювати пін (ризик R3: не гнатися за upstream).

Чеклист T15:

- [ ] `server/vendor/rqbit/LICENSE` (verbatim upstream).
- [ ] `server/vendor/rqbit/NOTICE` (текст вище).
- [ ] `server/vendor/UPSTREAM.md` (коміт, diffstat, файли, причини).
- [ ] Шапки modified у всіх змінених файлах.
- [ ] `cargo build --locked`; патч-тести RAM-режиму (S4-сценарії) — T20.
- [ ] Оновлено таблицю в `docs/licensing.md` (якщо шляхи/пін змінилися).

## 2. TorrStream Lite (TSL) — MIT

| Факт | Значення |
|---|---|
| Upstream | https://github.com/andeliseev/TorrStreamLite |
| Пін | `d007de1593a1e83a14a98664cab3fe14948c39c5` (v0.2.11, 2026-11-15) |
| Ліцензія | MIT, «Copyright (c) 2026 Andrei Eliseev» |
| Що беремо | порт `src/cache.rs` (~418 рядків) і `src/range.rs` (~102 рядки), ідеї хуків — **не** застосунок TSL цілком (`S5` §9/§11) |

**Повний MIT-текст** (покласти в `server/src/torrent/NOTICE` або `server/vendor/TSL-LICENSE`):

```text
MIT License

Copyright (c) 2026 Andrei Eliseev

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

**Шапка портованого файлу**:

```rust
// Ported from TorrStream Lite (MIT), Copyright (c) 2026 Andrei Eliseev.
// https://github.com/andeliseev/TorrStreamLite @ d007de1 (v0.2.11).
// Changes: <one line>. See server/src/torrent/NOTICE.
```

Чеклист T16/T15:

- [ ] MIT-текст + copyright у `server/src/torrent/NOTICE`.
- [ ] Шапки в `cache.rs`/`range.rs` (наші файли).
- [ ] Не тягнути macOS/Keenetic/`memory.rs`/router-TLS із TSL.
- [ ] Оновлено `docs/licensing.md`, якщо додано нові портовані файли.

## 3. dman-search — MIT OR Apache-2.0

| Факт | Значення |
|---|---|
| Upstream | https://github.com/rajput-hemant/dman (`crates/dman-search`) |
| Версія | 0.1.0 (crates.io), `git sha1 786d6064f3f3952536928d66bad078db5b4f03e1` |
| Ліцензія | `MIT OR Apache-2.0` (`Cargo.toml:36`); README посилається на `LICENSE-MIT`/`LICENSE-APACHE` |
| Особливість | у crates.io-пакеті текстів ліцензій **немає** — взяти з upstream-коміту |
| Що беремо | декларативні трекери + патчі charset (`encoding_rs` ~15 рядків) і `torrent_url` (~10 рядків) |

**Текст атрибуції** (у `server/vendor/dman-search/NOTICE`):

```text
dman-search 0.1.0
Copyright the dman contributors
Licensed under either of MIT or Apache-2.0, at your option.
https://github.com/rajput-hemant/dman (crates/dman-search)
Pinned at git 786d6064f3f3952536928d66bad078db5b4f03e1.

This copy contains modifications by the lampa-rust-p0 project (charset
decoding for cp1251 trackers; torrent_url support). See UPSTREAM.md.
```

**Шапка modified-файлу** (Apache §4(b), якщо обрано Apache-маршрут):

```rust
// Modified by the lampa-rust-p0 project (2026-11-25):
// <one-line reason>. Original: dman-search 0.1.0 @ 786d6064, MIT OR Apache-2.0.
```

Чеклист T28:

- [ ] `server/vendor/dman-search/LICENSE-MIT` + `LICENSE-APACHE` (з upstream-коміту; перевірити, що це саме ті тексти).
- [ ] `server/vendor/dman-search/NOTICE` (текст вище).
- [ ] `server/vendor/dman-search/UPSTREAM.md` (коміт, патчі, файли).
- [ ] Шапки modified у запатчених файлах (`def.rs`, `html.rs`, validation тощо).
- [ ] Наші `providers.toml` — окремі, з provenance-коментарем (див. §4).
- [ ] Оновлено `docs/licensing.md`.

## 4. Трекер-дефініції: provenance

- Наші дефініції (`rutor`, `nnmclub`, `megapeer`, `bitru`, `toloka`, `mazepa`) — **власний TOML-конфіг**, написаний за публічною HTML/JSON-структурою; джерело — `/opt/lampa/poc/tracker-poc/providers.toml` (S9). У шапці кожного файлу: `// Original definition for lampa-rust-p0; written from public site structure (S9, 2026-11-25).`
- Вбудований `builtin.toml` dman-search (зокрема Knaben) входить у крейт і покривається його ліцензією — не переписувати його в наші файли без потреби.
- **Jackett YAML (`GPL-2.0`) — не копіювати** у P0; лише довідка. Якщо колись адаптуємо — позначити як GPL-2.0-похідне й зафіксувати рішення власника (L2 у `docs/licensing.md`).
- **JacRed-каталог (`AGPL-3.0`)** — лише довідник назв/пріоритетів (D12), код/модель індексу не беремо.

## 5. Агрегований `THIRD_PARTY_NOTICES`

Розташування: `server/vendor/THIRD_PARTY_NOTICES.md` (створює T15; T28 доповнює). Мінімум:

1. rqbit — Apache-2.0, copyright + modified + commit (посилання на `server/vendor/rqbit/LICENSE`);
2. TorrStream Lite — MIT, copyright + порт;
3. dman-search — MIT OR Apache-2.0, copyright + modified + commit (посилання на ліцензії);
4. перелік інших прямих залежностей із `Cargo.lock` (генерується `cargo metadata`/`cargo deny` у W2+; у P0 — не блокер).

## 6. Чеклист W2 (T15/T28) — зведений

- [ ] `server/vendor/rqbit/{LICENSE,NOTICE}` + modified-шапки + `UPSTREAM.md`.
- [ ] `server/src/torrent/NOTICE` (TSL MIT) + шапки портованих файлів.
- [ ] `server/vendor/dman-search/{LICENSE-MIT,LICENSE-APACHE,NOTICE,UPSTREAM.md}` + modified-шапки.
- [ ] `server/vendor/THIRD_PARTY_NOTICES.md`.
- [ ] `docs/licensing.md` — таблиця компонентів і шляхи оновлені.
- [ ] CI (T05/T15) перевіряє наявність файлів ліцензій (проста перевірка `test -f`; `cargo-deny` — окрема задача, не блокер P0).
- [ ] Жодного GPL/AGPL-коду в вендорених теках без рішення власника (D11).
- [ ] Публікація образу — лише після аудиту `docs/licensing.md` §5.
