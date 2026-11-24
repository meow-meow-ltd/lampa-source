# DELIVERY — як зміни доходять до продакшену

Станом на 2026-11-24. Цей документ описує **реальний** шлях коду в прод (він не збігається з тим, що написано в `README.md` про `docker-compose.yml`).

## TL;DR

```
git push origin main
  │
  ├─► GitHub Actions · build-and-push.yml        (runner: GitHub-hosted, ubuntu-latest)
  │     docker buildx build .  →  ghcr.io/meow-meow-ltd/lampa-source:latest
  │                            →  ghcr.io/meow-meow-ltd/lampa-source:<git-sha>
  │
  ├─► GitHub Actions · deploy.yml                (runner: self-hosted "home-ubuntu" — цей сервер)
  │     cd /opt/lampa
  │     docker compose -f compose.yaml pull lampa
  │     docker compose -f compose.yaml up -d lampa
  │     docker image prune -f
  │
  └─► cloudflared (systemd, token-based tunnel, керується з Cloudflare Dashboard)
        https://lampa.niverovskii.com  →  http://localhost:9118  →  контейнер lampa:3000
```

**На сервері нічого не збирається.** Сервер лише тягне готовий образ з GHCR і перезапускає контейнер.
Локальна папка `/opt/lampa/source` у деплої **не бере участі** (і взагалі розійшлася з GitHub — див. «Відомі обмеження»).

## Компоненти

| Компонент | Розташування | Примітки |
|---|---|---|
| Репозиторій | `github.com/meow-meow-ltd/lampa-source` | публічний, форк `yumata/lampa-source`; гілки `main`/`develop` |
| Білд | `.github/workflows/build-and-push.yml` | тригер: push у `main` (+ ручний `workflow_dispatch`); кеш GHA |
| Реєстр образів | `ghcr.io/meow-meow-ltd/lampa-source` | пакет **приватний**; теги `:latest` і `:<git-sha>` |
| Деплой | `.github/workflows/deploy.yml` | тригер: `workflow_run` після успішного білда |
| Раннер деплою | `/opt/actions-runner` | org-рівень, імʼя `home-ubuntu`, юзер `runner`, лейбли `self-hosted,Linux,X64,home,deploy` |
| Служба раннера | `actions.runner.meow-meow-ltd.home-ubuntu.service` | + drop-in `Restart=on-failure` (додано 2026-11-24) |
| Compose | `/opt/lampa/compose.yaml` | `image: ghcr.io/meow-meow-ltd/lampa-source:latest`, `9118:3000`, `restart: unless-stopped` |
| Змінні оточення | `/opt/lampa/.env` | `HOST`, `PORT`, `DB_PATH`, `TOTP_SECRET` (не в git) |
| Дані | `/opt/lampa/data/database.sqlite` | bind-mount `/opt/lampa/data:/data` |
| Публікація | `/etc/systemd/system/cloudflared.service` | токен-керований тунель, ingress — у Cloudflare Dashboard |

## Ручний запуск

```bash
# перебілд + авто-деплой (workflow_dispatch у build-and-push, після мерджу PR #…)
gh workflow run build-and-push --repo meow-meow-ltd/lampa-source --ref main
gh run list --repo meow-meow-ltd/lampa-source --limit 5

# деплой руками на сервері (як юзер runner — у нього є креденшели до GHCR)
cd /opt/lampa && docker compose pull lampa && docker compose up -d lampa
```

Перевірка, що сервіс живий:

```bash
curl -fsS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9118/
curl -fsS -o /dev/null -w '%{http_code}\n' https://lampa.niverovskii.com/
docker inspect lampa --format 'created={{.Created}} startedAt={{.State.StartedAt}}'
```

## Відкат

Образів у реєстрі два на кожен білд: рухомий `:latest` і незмінний `:<git-sha>`.
Відкат = прибити compose до конкретного SHA і перезапустити:

```bash
cd /opt/lampa
sudo -u runner docker compose pull lampa        # потрібні креденшели до приватного пакета
# у compose.yaml тимчасово: image: ghcr.io/meow-meow-ltd/lampa-source:<SHA>
docker compose up -d lampa
```

Увага: `docker image prune -f` у кроці деплою видаляє попередній образ, тому «просто перезапустити старий контейнер» **не вийде** — тільки через `:<sha>` з реєстру.

## Секрети й доступи

- **GHCR**: пакет приватний. `deploy.yml` НЕ робить `docker login`; `docker compose pull` працює завдяки збереженому логіну юзера `runner` (`/home/runner/.docker/config.json`, PAT). Якщо PAT протермінується — деплой впаде з `denied: unauthorized`. Рішення: або зробити пакет публічним, або додати в `deploy.yml` явний `docker login` з `GITHUB_TOKEN` (`permissions: packages: read`).
- **`.env`** на сервері містить `TOTP_SECRET` — не комітити.
- **Токен тунелю** лежить відкритим текстом у `/etc/systemd/system/cloudflared.service` (бажано перенести в `EnvironmentFile` з `chmod 600`).

## Моніторинг і діагностика

```bash
gh run list --repo meow-meow-ltd/lampa-source --limit 10
gh run view <run-id> --log
systemctl status actions.runner.meow-meow-ltd.home-ubuntu.service
journalctl -u actions.runner.meow-meow-ltd.home-ubuntu.service -n 50 --no-pager
tail -f /opt/actions-runner/_diag/Runner_*.log
docker logs --tail 100 lampa
journalctl -u cloudflared --since '1 hour ago' --no-pager
```

## Типові збої

| Симптом | Причина | Лікування |
|---|---|---|
| Білд зелений, деплой висить у `queued` | немає живого раннера з лейблами `home,deploy`; GitHub видаляє реєстрації раннерів після ~30 днів неактивності | `sudo bash /opt/lampa/scripts/fix-runner.sh` (перереєстрація + `Restart=on-failure` + перевірка GHCR) |
| `docker compose pull` → `denied: unauthorized` | протермінований PAT юзера `runner` або пакет став приватним/переїхав | оновити `docker login ghcr.io` під `runner` або зробити пакет публічним |
| Сайт не відкривається, тунель flapping | UDP/QUIC-таймаути в домашній мережі, застарілий `cloudflared` | `systemctl status cloudflared`, оновити бінарник (зараз `--no-autoupdate`, версія 2026.2.0) |
| Деплой «зелений», але сайт старий | раніше не було healthcheck: `up -d` не перевіряв відповідь | після PR з healthcheck крок падає, якщо `:9118` не відповідає |

## Відомі обмеження (свідомо не виправлені)

- У CI **немає тестів/лінту** — `vitest` є, але не запускається.
- `:latest` рухомий, немає git-тегів і GitHub Releases; версія в `package.json` — `0.0.1`.
- Немає сповіщень про падіння білда/деплою.
- `develop` не збирається і не деплоїться (тригери тільки на `main`).
- У проді працює `npm run start` = BrowserSync (dev-сервер) над зібраним `build/web`, а не окремий статичний сервер.
- `main` захищений (1 рев'ю), але адміністратор може мерджити в обхід.
