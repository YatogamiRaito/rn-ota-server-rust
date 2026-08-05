# React Native OTA Server (Rust)

A self-hosted over-the-air update server for React Native apps, written in Rust.

It is a wire-compatible reimplementation of the [hot-updater](https://github.com/gronxb/hot-updater)
self-hosted server: the same update-check endpoints, the same decision algorithm, the same CLI API
surface — so the stock `hot-updater` React Native SDK and CLI talk to it unchanged.

- **Axum + sqlx + MySQL**, single binary, no Node runtime at request time
- **Multi-app** out of the box — one server serves any number of apps, each with its own auth token
  and its own S3/R2 bucket
- **Parity is tested, not claimed** — the update decision logic and the semver range engine are
  verified against fixtures generated from the real `@hot-updater/js` and `semver` npm packages
- Supports **appVersion** and **fingerprint** update strategies, staged rollouts, cohorts,
  bundle rollback, manifest/changed-asset responses and bsdiff patch delivery

Verified against upstream **hot-updater 0.35.8**. See [docs/upstream-parity.md](docs/upstream-parity.md)
for the exact source-to-file mapping and the list of intentional deviations.

---

## Why

The upstream self-hosted server is a Node handler you embed in your own framework, wired to a DB
adapter. This project instead gives you a standalone binary you can run behind nginx or in a
container, with MySQL and S3-compatible storage (Cloudflare R2, AWS S3, MinIO) configured through
environment variables, and with per-app credential isolation built in.

If you run one app and are happy with Node, use upstream. If you run several apps, want a single
small binary, or want the update-check path to be cheap, this is for you.

---

## Quick start

```bash
git clone https://github.com/<your-account>/rn-ota-server-rust.git
cd rn-ota-server-rust
cp .env.example .env      # fill in DATABASE_URL, APPS and the per-app credentials
cargo run --release
```

On startup the server connects to MySQL and runs the migrations in `migrations/` automatically.

### Docker

```bash
docker compose up --build      # starts MySQL + the server, reads .env
```

---

## Configuration

Everything is configured through environment variables (a `.env` file is loaded if present).

| Variable       | Required | Default                                             | Meaning                                    |
| -------------- | -------- | --------------------------------------------------- | ------------------------------------------ |
| `APPS`         | yes      | —                                                   | Comma-separated app names                  |
| `DATABASE_URL` | no       | `mysql://root:password@127.0.0.1:3306/ota_server`   | MySQL connection string                    |
| `HOST`         | no       | `127.0.0.1`                                         | Bind address                               |
| `PORT`         | no       | `3010`                                              | Bind port                                  |
| `R2_ENDPOINT`  | no       | —                                                   | Shared S3-compatible endpoint (fallback)   |

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

App names appear in URL paths, so whitespace and `/` are rejected at startup. kebab-case is
recommended. A missing variable fails startup with an explicit message naming the variable.

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

## Endpoints

Every route is prefixed with the app name, which is how one server serves several apps.

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
```

---

## Pointing the React Native SDK at this server

In your app's hot-updater config, set the update source to the app-prefixed base URL:

```
https://ota.example.com/main-app/hot-updater
```

and use `AUTH_TOKEN_MAIN_APP` as the CLI token. No SDK patch is required — the route shapes and
response bodies match upstream.

---

## Development

```bash
cargo run                                                    # dev server
cargo test                                                   # unit + parity tests (no DB needed)
cargo clippy --all-targets --all-features -- -D warnings     # lint
cargo fmt --all                                              # format
```

The parity tests are pure and synchronous — they need neither MySQL nor S3, so `cargo test` runs
anywhere. One test (`r2_manual_verification`) is `#[ignore]`d by default and hits a real bucket;
see the comment at the top of that file to run it.

### Regenerating the fixtures

The decision and semver fixtures are generated from the real npm packages. See
[tools/fixture-gen/README.md](tools/fixture-gen/README.md). Regenerate only when upgrading
upstream, and review the diff — a semantic change there means the Rust side needs to follow.

---

## Deployment

`ecosystem.config.cjs` is included for PM2:

```bash
cargo build --release
pm2 start ecosystem.config.cjs --env production
```

Or use the provided `Dockerfile` / `docker-compose.yml`.

Migrations run automatically at startup. Note that two of them (`bundle_check_constraints`,
`id_ascii_bin_collation`) were authored without access to a live MySQL instance; read their header
comments before applying them to a database that already holds data.

---

## Relationship to upstream

This is an independent reimplementation, not a fork — no upstream code is vendored. It aims for
behavioral parity with hot-updater's server on the endpoints it implements, with a small set of
documented, deliberate deviations (multi-app prefix, built-in per-app auth, direct MySQL access,
independent versioning). All of them are listed in
[docs/upstream-parity.md](docs/upstream-parity.md).

Credit for the protocol, the client SDK and the CLI goes to the
[hot-updater](https://github.com/gronxb/hot-updater) project.

---

## License

MIT — see [LICENSE](LICENSE).
