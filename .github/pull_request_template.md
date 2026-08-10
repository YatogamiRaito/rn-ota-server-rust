## What and why

<!-- The diff shows what changed; explain why it needed to change. -->

## Upstream parity

<!-- Delete the lines that do not apply. -->

- [ ] This change does not affect the update decision, semver engine, route shapes or response bodies.
- [ ] This change matches upstream hot-updater behaviour, and a fixture-backed test covers it.
- [ ] This is an intentional deviation, documented in `docs/upstream-parity.md`.

## Checklist

- [ ] `cargo fmt --all`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test`
- [ ] `CHANGELOG.md` updated under `## [Unreleased]` (skip for pure refactors)
