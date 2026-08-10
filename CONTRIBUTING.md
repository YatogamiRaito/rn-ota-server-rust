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
git clone https://github.com/ebubekirkaraca/rn-ota-server-rust.git
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
logic, the unit tests. The MySQL-backed integration tests start a real MySQL 8 container, and
**skip themselves with a notice if Docker is unavailable** so the plain command still works
anywhere.

That skip is a trap if you rely on it: a broken Docker daemon would turn missing coverage into a
green run. Set `OTA_REQUIRE_DOCKER_TESTS=1` — as CI does — to make a missing backend a hard
failure. `OTA_TEST_MYSQL_URL=<url>` points the suite at a server you are already running, and
`OTA_TEST_MYSQL_TAG` picks a different image tag.

Three tests are `#[ignore]`d, and all three are documented divergences rather than dead tests —
run them with `cargo test -- --ignored` to see each one demonstrated.

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
