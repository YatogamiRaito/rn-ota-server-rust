# Contributing

Thanks for taking the time. This project is small on purpose; the bar is that every change keeps
wire compatibility with [hot-updater](https://github.com/gronxb/hot-updater).

## Ground rule: parity comes first

This repository is **only** the server half of hot-updater. The React Native SDK and the
`hot-updater` CLI are upstream's and are used unchanged. A change that makes the stock SDK or CLI
stop working is a bug here, no matter how much nicer the new behaviour looks.

If your change alters the update decision, the semver range engine, a route shape or a response
body, it must be justified against upstream. Add or update a fixture-backed test, and if the
deviation is intentional, document it in [docs/upstream-parity.md](docs/upstream-parity.md).

## Setup

You need a recent stable Rust toolchain (1.94.1+, set by the AWS SDK). MySQL and S3 are **not** needed for the test
suite — the parity tests are pure and synchronous.

```bash
git clone https://github.com/YatogamiRaito/rn-ota-server-rust.git
cd rn-ota-server-rust
cp .env.example .env
cargo test
```

To run the server itself you do need MySQL and an S3-compatible bucket; `docker compose up` brings
up MySQL for you.

## Before opening a PR

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
OTA_REQUIRE_DOCKER_TESTS=1 cargo test --all-features
```

CI runs exactly these and will not merge on a failure.

### The two test tiers

`cargo test` on its own runs everything that needs no services — the parity fixtures, the pure
logic, the unit tests. Two groups of integration tests need a backend and start one with
testcontainers: the MySQL-backed API and update-check tests, and the storage tests, which run
against MinIO (S3-compatible, one of the supported backends). Both **skip themselves with a notice
if Docker is unavailable** so the plain command still works anywhere.

That skip is a trap if you rely on it: a broken Docker daemon would turn missing coverage into a
green run. Set `OTA_REQUIRE_DOCKER_TESTS=1` — as CI does — to make a missing backend a hard
failure. The flag covers both backends.

| Variable                            | Effect                                                                    |
| ----------------------------------- | ------------------------------------------------------------------------- |
| `OTA_REQUIRE_DOCKER_TESTS=1`        | A missing backend **panics** instead of skipping. CI sets this.            |
| `OTA_TEST_MYSQL_URL=<url>`          | Use a running MySQL instead of a container, e.g. `mysql://root@127.0.0.1:3306` (no `/dbname`; needs `CREATE DATABASE` rights). |
| `OTA_TEST_MYSQL_TAG=<tag>`          | MySQL image tag (default `8.0`).                                          |
| `OTA_TEST_S3_ENDPOINT=<url>`        | Use a running S3-compatible server instead of a MinIO container, e.g. `http://127.0.0.1:9000` (needs `CreateBucket` rights). |
| `OTA_TEST_S3_ACCESS_KEY_ID=<id>`    | Credentials for that server (default `minioadmin`).                       |
| `OTA_TEST_S3_SECRET_ACCESS_KEY=<k>` | Credentials for that server (default `minioadmin`).                       |
| `OTA_TEST_S3_TAG=<tag>`             | MinIO image tag.                                                          |

One test needs the MinIO **container** specifically rather than any S3 endpoint: the per-app
credential isolation test provisions a MinIO user scoped to a single bucket by running `mc` inside
the container, so it skips with a notice against `OTA_TEST_S3_ENDPOINT`. It still runs in CI.

The storage tests deliberately include two that take ~20 s each: they hold a connection open
against a server that never answers, or one that trickles a response forever, to prove the storage
timeouts actually fire. Neither needs Docker, so they run in the plain `cargo test` tier too.

Three tests are `#[ignore]`d. Two are documented divergences — run them with
`cargo test -- --ignored` to see each demonstrated. The third, `r2_manual_verification`, is a
release-time smoke test against a real Cloudflare R2 bucket: it needs credentials (see the comment
at the top of that file) and covers the two things MinIO cannot reproduce, R2 accepting a
signature scoped to region `auto` and path-style addressing over TLS against a real account host.

## Commit and PR style

- One logical change per PR; keep unrelated formatting out of the diff.
- Explain *why* in the PR description — the *what* is readable from the diff.
- Reference the upstream file or behaviour you matched when the change is parity-related.

## Regenerating fixtures

The decision and semver fixtures come from the real npm packages — see
[tools/fixture-gen/README.md](tools/fixture-gen/README.md). Regenerate only when bumping the
upstream version you target, and review the diff carefully: a semantic change in a fixture means
the Rust side has to follow, not that the test should be relaxed.

## Reporting bugs

Open an issue with the request URL (with secrets removed), the response you got, the response you
expected from upstream, and the hot-updater version your SDK/CLI is on. Security issues go through
[SECURITY.md](SECURITY.md) instead, not the public tracker.
