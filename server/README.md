# lampa-server (P0.0 / W0)

Мінімальний скелет майбутнього єдиного Rust-бінарника Lampa. У цій хвилі є лише
composition root, `GET /healthz` і коректне завершення за SIGTERM/SIGINT.
БД, auth, статика, медіа й торенти — наступні хвилі (W1+).

- Toolchain: **Rust 1.100.0**, зафіксований у `rust-toolchain.toml`
  (`rustfmt`, `clippy`); `Cargo.lock` комітиться, збірка/CI — `--locked`.
- Пакет: `lampa-server` (edition 2021), залежності: axum 0.8.9, tokio 1.54.1,
  serde/serde_json, tracing/tracing-subscriber.
- `target/` не комітиться (`server/target/` у кореневому `.gitignore`).

## Конфігурація (лише env, без секретів)

| Змінна | Default | Призначення |
|---|---|---|
| `HOST` | `0.0.0.0` | інтерфейс, на який біндиться HTTP |
| `PORT` | `3000` | TCP-порт |
| `RUST_LOG` | `info` | фільтр `tracing-subscriber`; невалідне значення → warn + fallback `info` |

Порожні або whitespace-значення трактуються як «не задано» (беруться дефолти).
Інших змінних скелет не читає; секрети не читаються й не логуються. У логах —
лише адреса bind, назва сигналу, факт старту/зупинки.

## Локальний запуск

Потрібен Rust 1.100.0 (rustup підхопить `rust-toolchain.toml`):

```bash
cd server
cargo run                 # 0.0.0.0:3000
HOST=127.0.0.1 PORT=9120 RUST_LOG=debug cargo run
curl -i http://127.0.0.1:3000/healthz
```

Очікувано:

```http
HTTP/1.1 200 OK
content-type: application/json
content-length: 15

{"status":"ok"}
```

## Docker-recipe (на хості немає cargo/rustc)

Один раз зібрати builder-образ (rust 1.100.0 + rustfmt + clippy):

```bash
printf 'FROM rust:1-slim\nRUN rustup component add rustfmt clippy\n' \
  | docker build -t lampa-rust-builder:1.100.0 -
```

На цьому хості `$HOME/.docker` доступний лише read-only для агентського
sandbox, тому фактично використана команда — legacy builder із локальним
`DOCKER_CONFIG` (buildx пише activity-файли в `$HOME`):

```bash
mkdir -p /opt/lampa/.docker
printf 'FROM rust:1-slim\nRUN rustup component add rustfmt clippy\n' \
  | DOCKER_CONFIG=/opt/lampa/.docker DOCKER_BUILDKIT=0 \
    docker build -t lampa-rust-builder:1.100.0 -
```

Повний чек-чейн (fmt + clippy + test + release build) — саме ця команда
використовувалася у W0:

```bash
docker run --rm --user "$(id -u):$(id -g)" \
  -v /opt/lampa/source:/work \
  -v /opt/lampa/poc/cargo-cache:/cargo \
  -v /opt/lampa/.cargo-target:/target \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target \
  -w /work/server lampa-rust-builder:1.100.0 \
  bash -c 'cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && cargo build --release --locked'
```

> Важливо: `CARGO_TARGET_DIR` мусить бути шляхом **усередині** контейнера.
> Хостовий `/opt/lampa/.cargo-target` змонтований як `/target`; якщо задати
> `CARGO_TARGET_DIR=/opt/lampa/.cargo-target` без mount, cargo впаде з
> `failed to create directory /opt/lampa`.

## Smoke на 127.0.0.1:9120

Бінарник `/target/release/lampa-server` запускається як PID 1 контейнера
(`--network host`, щоб `127.0.0.1:9120` був доступний з хоста):

```bash
docker run -d --name lampa-rust-smoke --network host \
  --user "$(id -u):$(id -g)" \
  -e HOST=127.0.0.1 -e PORT=9120 -e RUST_LOG=info \
  -v /opt/lampa/.cargo-target:/target:ro \
  lampa-rust-builder:1.100.0 /target/release/lampa-server

until curl -fsS -o /dev/null http://127.0.0.1:9120/healthz; do sleep 0.1; done
curl -sS -i http://127.0.0.1:9120/healthz     # 200 application/json {"status":"ok"}
```

### Перевірка SIGTERM

```bash
docker kill --signal=TERM lampa-rust-smoke
docker wait lampa-rust-smoke                   # exit code 0
docker logs lampa-rust-smoke                   # ... "shutdown complete"
docker rm lampa-rust-smoke
```

SIGINT — те саме з `--signal=INT`.

Семантика завершення (реалізовано в `src/main.rs`):

1. сигнал SIGTERM/SIGINT → лог `shutdown signal received; draining in-flight requests`;
2. listener закривається — нові з'єднання відхиляються (curl rc 7 connection refused);
3. in-flight запити дотягуються, сумарно не довше **10 с**
   (`SHUTDOWN_TIMEOUT`, unit-тест тримає бюджет ≤10 с);
4. якщо за 10 с не вдалося — `WARN graceful shutdown timed out`, процес все одно
   завершується з кодом 0;
5. успішний дренаж → `INFO http server stopped` + `INFO shutdown complete`, exit 0.

Виміряно у W0: чистий SIGTERM/SIGINT — exit 0 за ~96/93 мс; утримуване
незавершене з'єднання — рівно 10 с дренажу, новий запит у цей час відхилено,
потім exit 0 + `shutdown complete`.

## Структура

| Файл | Роль |
|---|---|
| `src/main.rs` | composition root: config, tracing, bind, serve, graceful shutdown |
| `src/config.rs` | `HOST`/`PORT`/`RUST_LOG` з env, дефолти, `ConfigError` (без panic) |
| `src/state.rs` | `AppState` (мінімальний, `Arc<Config>`); без глобальних singleton |
| `src/http/mod.rs` | збірка роутера + router unit-тести (`tower::ServiceExt::oneshot`) |
| `src/http/health.rs` | `GET /healthz` → `200 application/json {"status":"ok"}` |

`/healthz` — це liveness (без БД та engine); readiness `/readyz` з'явиться
разом із DB-шаром у наступних хвилях.

## Тести

```bash
cargo test    # 11 unit-тестів: healthz через oneshot, 404/405, конфіг/дефолти,
              # помилковий PORT, бюджет shutdown ≤10 с
```
