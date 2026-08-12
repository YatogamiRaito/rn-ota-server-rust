# React Native OTA Server (Rust)

[![CI](https://github.com/YatogamiRaito/rn-ota-server-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/YatogamiRaito/rn-ota-server-rust/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/rn-ota-server-rust.svg)](https://crates.io/crates/rn-ota-server-rust)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**A drop-in Rust replacement for the [hot-updater](https://github.com/gronxb/hot-updater)
self-hosted server — and nothing else.**

## Read this first: what this project is, and what it is not

This is **not a standalone OTA solution.** It is one piece of hot-updater — the server — rewritten
in Rust. Everything else still comes from upstream and is required:

| Piece                        | Where it comes from                     |
| ---------------------------- | --------------------------------------- |
| React Native SDK in your app | **hot-updater** (upstream, unchanged)   |
| `hot-updater` CLI            | **hot-updater** (upstream, unchanged)   |
| Bundle format, protocol      | **hot-updater** (upstream)              |
| Update server                | **this project** (Rust)                 |

**You must install and use hot-updater in your React Native app.** Add
[`hot-updater`](https://github.com/gronxb/hot-updater) exactly as its documentation describes,
build your bundles with its CLI, and then simply point its update source at this server instead
of a Node one. There is no SDK to swap, no patch to apply, no fork to install — the route shapes
and response bodies are wire-compatible, so the stock SDK and CLI cannot tell the difference.

If you are not already a hot-updater user, start with
[upstream's documentation](https://gronxb.github.io/hot-updater/) first. Come back here when you
want to run the server side yourself.

Verified against upstream **hot-updater 0.35.8**. See [docs/upstream-parity.md](docs/upstream-parity.md)
for the exact source-to-file mapping and the list of intentional deviations.

---

## Why swap out the server

Upstream's self-hosted server is a Node handler you embed in your own framework and wire to a DB
adapter. This project gives you instead:

- **A single static binary** — Axum + sqlx + MySQL, no Node runtime at request time
- **Multi-app out of the box** — one server serves any number of apps, each with its own auth token
  and its own S3/R2 bucket
- **Parity that is tested, not claimed** — the update decision logic and the semver range engine
  are verified against fixtures generated from the real `@hot-updater/js` and `semver` npm packages
- Full feature coverage: **appVersion** and **fingerprint** strategies, staged rollouts, cohorts,
  bundle rollback, manifest/changed-asset responses and bsdiff patch delivery

If you run one app and are happy with Node, use upstream. If you run several apps, want a single
small binary, or want the update-check path to be cheap, this is for you.

---

## Requirements

- **hot-updater** in your React Native app (SDK + CLI) — see above
- **MySQL 8.0+**
- **S3-compatible storage** — Cloudflare R2, AWS S3 or MinIO
- Rust 1.94.1+ *only if you build from source* (the AWS SDK sets that floor, not this code)

---

## Install

Pick whichever fits. All three give you the same `rn-ota-server-rust` binary.

### 1. Prebuilt binary (no toolchain needed)

Grab the archive for your platform from the
[latest release](https://github.com/YatogamiRaito/rn-ota-server-rust/releases/latest) — Linux
(x86_64, aarch64), macOS (Intel, Apple Silicon) and Windows (x86_64) are published on every tag,
each with a `.sha256` checksum file.

```bash
VERSION=v1.0.0
TARGET=x86_64-unknown-linux-gnu
curl -LO "https://github.com/YatogamiRaito/rn-ota-server-rust/releases/download/${VERSION}/rn-ota-server-rust-${VERSION}-${TARGET}.tar.gz"
curl -LO "https://github.com/YatogamiRaito/rn-ota-server-rust/releases/download/${VERSION}/rn-ota-server-rust-${VERSION}-${TARGET}.tar.gz.sha256"
shasum -a 256 -c "rn-ota-server-rust-${VERSION}-${TARGET}.tar.gz.sha256"

tar xzf "rn-ota-server-rust-${VERSION}-${TARGET}.tar.gz"
cd "rn-ota-server-rust-${VERSION}-${TARGET}"
cp .env.example .env      # fill it in, see Configuration
./rn-ota-server-rust
```

### 2. cargo install

```bash
cargo install rn-ota-server-rust
rn-ota-server-rust                 # reads .env from the working directory
```

### 3. Docker

```bash
git clone https://github.com/YatogamiRaito/rn-ota-server-rust.git
cd rn-ota-server-rust
cp .env.example .env               # fill in APPS and the per-app credentials
docker compose up --build          # starts MySQL + the server
```

`docker-compose.yml` overrides `DATABASE_URL` to point at its own MySQL service, so you only need
to fill in the app credentials in `.env`.

### 4. From source

```bash
git clone https://github.com/YatogamiRaito/rn-ota-server-rust.git
cd rn-ota-server-rust
cp .env.example .env
cargo run --release
```

On startup the server connects to MySQL and runs the migrations in `migrations/` automatically, so
there is no separate schema step.

---

## Configuration

Everything is configured through environment variables (a `.env` file in the working directory is
loaded if present).

| Variable       | Required | Default                                             | Meaning                                    |
| -------------- | -------- | --------------------------------------------------- | ------------------------------------------ |
| `APPS`         | yes      | —                                                   | Comma-separated app names                  |
| `DATABASE_URL` | no       | `mysql://root:password@127.0.0.1:3306/ota_server`   | MySQL connection string                    |
| `HOST`         | no       | `127.0.0.1`                                         | Bind address                               |
| `PORT`         | no       | `3010`                                              | Bind port                                  |
| `R2_ENDPOINT`  | no       | —                                                   | Shared S3-compatible endpoint (fallback)   |
| `R2_REGION`    | no       | `auto`                                              | Shared signing region (fallback)           |
| `R2_FORCE_PATH_STYLE` | no | follows the endpoint                                | Shared addressing style (fallback)         |

### Observability

All optional; the defaults are what the server ran with before these existed.

| Variable                | Default | Meaning                                                                            |
| ----------------------- | ------- | ---------------------------------------------------------------------------------- |
| `RUST_LOG`              | `info`  | Log filter, standard `tracing` syntax. The default quiets sqlx/hyper/the AWS SDK.   |
| `HTTP_LOG_LEVEL`        | `info`  | Level of the per-request access log. `off` disables request spans entirely.         |
| `METRICS_ENABLED`       | `true`  | Serve `GET /metrics` in Prometheus text format.                                     |
| `CORS_ALLOWED_ORIGINS`  | —       | Comma-separated origins, or `*`. Unset sends no CORS headers at all.                |

The counters worth alerting on are `ota_update_check_degraded_total{app,reason}` — every path
where the server keeps serving after something failed, plus the one where it refuses to. `reason`
is one of `presign_failed` (the bundle could not be signed, so the request answers 500 rather than
telling the device to update with nothing to download), `manifest_unavailable` and
`patch_unavailable` (the update still ships, but the device re-downloads instead of applying a
diff), and `current_bundle_unavailable`. The degraded cases are otherwise invisible: the device
gets a working update and you only notice the cost on the storage bill.

`/metrics` is unauthenticated, like `/health` — block it at your reverse proxy or set
`METRICS_ENABLED=false`. Metric labels are deliberately low-cardinality: routes collapse to a
fixed set of classes, `app` is restricted to the names in `APPS` (anything else becomes
`unknown`), and no metric is ever labelled by bundle id, app version, fingerprint hash or cohort.

### Database connection pool

| Variable                  | Default |
| ------------------------- | ------- |
| `DB_MAX_CONNECTIONS`      | `10`    |
| `DB_MIN_CONNECTIONS`      | `2`     |
| `DB_ACQUIRE_TIMEOUT_SECS` | `3`     |
| `DB_IDLE_TIMEOUT_SECS`    | `60`    |

### Storage timeouts

Every call to S3/R2 is bounded by these; an endpoint that accepts the connection and then goes
quiet would otherwise stall the device update-check waiting on it. Values are seconds and must be
at least 1 — there is no "unlimited" setting. Each limit must fit inside the one containing it
(`CONNECT` ≤ `ATTEMPT`, `READ` ≤ `ATTEMPT`, `ATTEMPT` ≤ `OPERATION`); startup fails otherwise.

| Variable                         | Default | Meaning                                                       |
| -------------------------------- | ------- | ------------------------------------------------------------- |
| `STORAGE_CONNECT_TIMEOUT_SECS`   | `3`     | Establishing the TCP/TLS connection                            |
| `STORAGE_READ_TIMEOUT_SECS`      | `5`     | Waiting for the first byte of the response                     |
| `STORAGE_ATTEMPT_TIMEOUT_SECS`   | `10`    | One attempt; retries are counted separately                    |
| `STORAGE_OPERATION_TIMEOUT_SECS` | `20`    | The whole call, retries and body download included             |

If a slow but healthy store is being cut off, raise `STORAGE_OPERATION_TIMEOUT_SECS` first.

### Presigned URL lifetime

| Variable                       | Default | Meaning                                              |
| ------------------------------ | ------- | ---------------------------------------------------- |
| `STORAGE_PRESIGN_EXPIRY_SECS`  | `3600`  | How long a download URL handed to a device stays valid |

Accepted range is 60 s to 604800 s (7 days), checked at startup. The upper bound is the SigV4
maximum that both S3 and R2 enforce; the lower one keeps a URL alive long enough to survive a
client retry or a slow network before the download starts. Shorter is tighter if a leaked URL
worries you — the device fetches the bundle immediately after the update-check.

### Rate limiting

Off by default, and applied to the **unauthenticated update-check routes only** — the CLI API is
never throttled. Most deployments sit behind a reverse proxy that already does this; turn it on
only if yours does not.

| Variable                             | Default | Meaning                                                     |
| ------------------------------------ | ------- | ----------------------------------------------------------- |
| `RATE_LIMIT_ENABLED`                 | `false` | Master switch                                               |
| `RATE_LIMIT_UPDATE_CHECK_PER_SECOND` | `10`    | Sustained rate per client                                   |
| `RATE_LIMIT_UPDATE_CHECK_BURST`      | `20`    | Burst allowance                                             |
| `RATE_LIMIT_TRUST_PROXY_HEADERS`     | `false` | Take the client IP from `X-Forwarded-For` / `X-Real-IP`      |

Only enable `RATE_LIMIT_TRUST_PROXY_HEADERS` when a trusted proxy sets **and overwrites** those
headers. Otherwise any client can choose its own rate-limit bucket by forging one.

### Per-app variables

Each name in `APPS` gets its own set of variables. The suffix is the app name upper-cased with
every non-alphanumeric character replaced by `_`:

```
main-app   ->  MAIN_APP
beta.app   ->  BETA_APP
```

| Variable                        | Required | Meaning                                                  |
| ------------------------------- | -------- | -------------------------------------------------------- |
| `AUTH_TOKEN_<SUFFIX>`           | yes      | Bearer token the CLI must present for this app's API      |
| `R2_ACCESS_KEY_ID_<SUFFIX>`     | yes      | S3/R2 access key                                          |
| `R2_SECRET_ACCESS_KEY_<SUFFIX>` | yes      | S3/R2 secret key                                          |
| `R2_BUCKET_NAME_<SUFFIX>`       | yes      | Bucket holding this app's bundles                         |
| `R2_ENDPOINT_<SUFFIX>`          | no       | Per-app endpoint; falls back to `R2_ENDPOINT`             |
| `R2_REGION_<SUFFIX>`            | no       | Per-app signing region; falls back to `R2_REGION`, then `auto` |
| `R2_FORCE_PATH_STYLE_<SUFFIX>`  | no       | Per-app addressing style; falls back to `R2_FORCE_PATH_STYLE` |

App names appear in URL paths, so whitespace and `/` are rejected at startup. kebab-case is
recommended. A missing variable fails startup with an explicit message naming the variable.

#### Choosing a region and an addressing style

`auto` is Cloudflare R2's convention and is the default, so **R2 and MinIO deployments need
neither variable**. AWS S3 does: the SigV4 credential scope has to name the bucket's real region,
and with no endpoint configured the SDK builds the hostname from it too — `auto` there produces
`<bucket>.s3.auto.amazonaws.com`, which does not resolve.

`R2_FORCE_PATH_STYLE` defaults to whether an endpoint is set, which is the right answer for each
backend: R2 and MinIO are reached through an endpoint and want the bucket in the path (MinIO
cannot serve virtual-hosted style without wildcard DNS), while AWS S3 has no endpoint override and
prefers virtual-hosted style, path style being its legacy alternative. Set it only to override.

```bash
# AWS S3
APPS=main-app
R2_REGION_MAIN_APP=eu-central-1
# no R2_ENDPOINT: the SDK builds <bucket>.s3.eu-central-1.amazonaws.com

# MinIO
R2_ENDPOINT=http://minio.internal:9000
# region and addressing style: leave both unset
```

Example:

```bash
APPS=main-app,beta-app
AUTH_TOKEN_MAIN_APP=…
R2_ACCESS_KEY_ID_MAIN_APP=…
R2_SECRET_ACCESS_KEY_MAIN_APP=…
R2_BUCKET_NAME_MAIN_APP=…
AUTH_TOKEN_BETA_APP=…
# … same four for BETA_APP
R2_ENDPOINT=https://<account>.r2.cloudflarestorage.com
```

---

## Pointing hot-updater at this server

This is the only change on the React Native side. In your app's hot-updater config, set the update
source to the **app-prefixed** base URL:

```
https://ota.example.com/main-app/hot-updater
```

and give the CLI `AUTH_TOKEN_MAIN_APP` as its token. The `/{app}` prefix is what lets one server
serve several apps; everything after it matches upstream exactly.

Build and deploy bundles with the upstream CLI as you normally would:

```bash
npx hot-updater deploy
```

---

## Endpoints

Every route is prefixed with the app name.

### Update check (called by the device, no auth)

```
GET /{app}/hot-updater/app-version/{platform}/{appVersion}/{channel}/{minBundleId}/{bundleId}
GET /{app}/hot-updater/app-version/{platform}/{appVersion}/{channel}/{minBundleId}/{bundleId}/{cohort}
GET /{app}/hot-updater/fingerprint/{platform}/{fingerprintHash}/{channel}/{minBundleId}/{bundleId}
GET /{app}/hot-updater/fingerprint/{platform}/{fingerprintHash}/{channel}/{minBundleId}/{bundleId}/{cohort}
```

Returns the update decision (`UPDATE` / `ROLLBACK` / `UP_TO_DATE`) with presigned download URLs,
and — when the bundle has a manifest — the changed-asset set and any applicable bsdiff patch.

### CLI API (called by the `hot-updater` CLI, `Authorization: Bearer <token>`)

```
GET    /{app}/hot-updater/api/bundles/channels
GET    /{app}/hot-updater/api/bundles
POST   /{app}/hot-updater/api/bundles
GET    /{app}/hot-updater/api/bundles/{id}
PATCH  /{app}/hot-updater/api/bundles/{id}
DELETE /{app}/hot-updater/api/bundles/{id}
```

### Meta

```
GET /version      # this server's own version
GET /health       # liveness probe, does not touch the database
GET /metrics      # Prometheus text format, when METRICS_ENABLED
```

---

## Deployment

Put the server behind a TLS-terminating reverse proxy (nginx, Caddy, a load balancer) — CLI bearer
tokens travel in the `Authorization` header. See [SECURITY.md](SECURITY.md) for the full list of
deployment considerations.

The process shuts down gracefully on SIGINT/SIGTERM, so `docker stop`, systemd and PM2 restarts
finish in-flight requests instead of dropping them.

### systemd

```ini
[Unit]
Description=React Native OTA Server
After=network.target

[Service]
Type=simple
User=ota
WorkingDirectory=/opt/rn-ota-server
ExecStart=/opt/rn-ota-server/rn-ota-server-rust
EnvironmentFile=/opt/rn-ota-server/.env
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

### PM2

```bash
cargo build --release
pm2 start ecosystem.config.cjs --env production
```

### Migrations

Migrations run automatically at startup, so the database user needs DDL rights.

All migrations are verified against a live MySQL 8, both on an empty database and on one already
holding data. Two things to check **before** upgrading an existing database, because both
fail in ways that stop the server from starting:

```sql
-- Non-ASCII ids. Under MySQL's default strict sql_mode the migration aborts and touches
-- nothing; under sql_mode='' it silently rewrites them, mangling primary keys.
SELECT id FROM bundles WHERE id <> CONVERT(id USING ascii);

-- Rows that violate the CHECK constraints. If any exist, no constraint is created at all.
SELECT id FROM bundles
 WHERE (target_app_version IS NULL AND fingerprint_hash IS NULL)
    OR rollout_cohort_count < 0 OR rollout_cohort_count > 1000;
```

Both should return zero rows.

The `tenant_scoped_primary_keys` migration also removes `bundle_patches` rows whose bundle no
longer exists. Those are unusable either way -- they point at a deleted bundle -- but list them
first if you want a record; the query is in `CHANGELOG.md` under that release's Operators note. If your database ever failed to start on the
`id_ascii_bin_collation` migration, see the recovery procedure in that file's header — it may be
running with two foreign keys missing.

> Note: `20260722000000_bundle_check_constraints.sql` still opens with a warning that it was never
> tested against a live MySQL. That warning is now out of date, but the file is deliberately left
> byte-for-byte as published: sqlx records a checksum of each applied migration, and it is the one
> file that did apply successfully in the field, so editing even a comment would make sqlx refuse
> to start until every operator hand-patched the stored hash. Trust this section over that header.

---

## Development

```bash
cargo run                                                    # dev server
cargo test                                                   # unit + parity tests (no DB needed)
cargo clippy --all-targets --all-features -- -D warnings     # lint
cargo fmt --all                                              # format
```

The parity tests are pure and synchronous, and the MySQL- and MinIO-backed integration tests skip
themselves when Docker is unavailable, so `cargo test` runs anywhere. That skip is a trap in CI —
see [CONTRIBUTING.md](CONTRIBUTING.md) for `OTA_REQUIRE_DOCKER_TESTS=1` and the rest of the test
tiers. One test (`r2_manual_verification`) is `#[ignore]`d by default and hits a real bucket; see
the comment at the top of that file to run it.

### Regenerating the fixtures

The decision and semver fixtures are generated from the real npm packages. See
[tools/fixture-gen/README.md](tools/fixture-gen/README.md). Regenerate only when upgrading
upstream, and review the diff — a semantic change there means the Rust side needs to follow.

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

---

## Relationship to upstream

This is an independent reimplementation of the server, not a fork — no upstream code is vendored.
It aims for behavioral parity with hot-updater's server on the endpoints it implements, with a
small set of documented, deliberate deviations (multi-app prefix, built-in per-app auth, direct
MySQL access, independent versioning). All of them are listed in
[docs/upstream-parity.md](docs/upstream-parity.md).

The protocol, the client SDK, the CLI and the bundle format are all the work of the
[hot-updater](https://github.com/gronxb/hot-updater) project by
[@gronxb](https://github.com/gronxb) — this server would not exist or be useful without it. This
project is not affiliated with or endorsed by upstream.

---

## License

MIT — see [LICENSE](LICENSE). hot-updater itself is licensed separately by its authors.
