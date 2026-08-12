# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Upstream parity target moved from hot-updater **0.35.8 to 0.35.12**. The only server-side change
  in that range is npm `semver` being replaced with `verkit`, a zero-dependency reimplementation,
  in `plugin-core`'s `semverSatisfies` and in the server's SDK-version header check. The decision
  algorithm is byte-identical between the two versions.

  No behaviour changed here, and that is measured rather than assumed: `semver@7.8.5` and
  `verkit@0.3.2` agree on all 139 recorded semver cases, and regenerating all three fixture files
  against 0.35.12 produced byte-identical output. `tests/generate_semver_fixtures.mjs` now records
  `verkit.coerce`, since recording a package upstream no longer uses would pin the wrong ground
  truth. See `docs/upstream-parity.md` §2.

## [1.2.0] - 2026-08-12

### Security

- **The tenant boundary now lives in the schema.** `bundles` moves to a `(app_name, id)`
  primary key, `bundle_patches` gains an `app_name` column and the same composite key, and both
  its foreign keys become `(app_name, …) → bundles(app_name, id)`. 1.1.0 closed the cross-tenant
  holes in application code; this makes them unreachable by construction, so a future query that
  forgets `AND app_name = ?` cannot reopen one. A patch referencing another app's bundle is now
  rejected by the database itself rather than by a lookup in `create_bundles`.

### Changed

- **`POST /{app}/hot-updater/api/bundles` no longer answers `409` for an id another app uses.**
  Under the composite key that id is not this app's business: the two are separate rows, the
  upsert cannot reach the other tenant's, and refusing the write would leak the fact that
  somebody else's bundle carries that id. The row-locked ownership check in front of the upsert
  is removed with it.
- **`bundles.id` is no longer globally unique** — it is unique per app. Any query that looks a
  bundle up by id alone is now a bug. Four such queries were scoped as part of this change: the
  patch lookups in `get_bundle`, `list_bundles` and the update-check path, and the patch delete in
  `create_bundles`. Each was previously correct on the grounds that ids were global, which is
  precisely the reasoning this migration invalidates.

### Added

- `ota_update_check_degraded_total{app,reason}` — a counter for every path where an update-check
  is served in a reduced form or refused outright: `presign_failed`, `manifest_unavailable`,
  `patch_unavailable`, `current_bundle_unavailable`. These were logged but not measurable, and the
  degraded ones were invisible: the device still gets a working update and simply re-downloads
  everything, so a storage backend quietly failing showed up on the bill rather than on a graph.

### Fixed

- **`ota_update_checks_total` counted decisions that were never served.** The counter was
  incremented beside the decision, before the response was built — and building it can now fail,
  since a presign failure answers 500 rather than handing the device a null `fileUrl`. During a
  storage incident the graph would have shown updates shipping normally while every device got
  nothing. It is recorded after the response is known to be servable.

### Operators

The migration deletes `bundle_patches` rows whose bundle no longer exists. The foreign keys
should have made those impossible, but a database that ran the broken first revision of
`id_ascii_bin_collation` spent time with both keys dropped, so orphans may have accumulated. They
reference a bundle that is gone and can never be served to a device. Inspect them first if you
want a record:

```sql
SELECT p.* FROM bundle_patches p
 WHERE NOT EXISTS (SELECT 1 FROM bundles b WHERE b.id = p.bundle_id)
    OR NOT EXISTS (SELECT 1 FROM bundles b WHERE b.id = p.base_bundle_id);
```

## [1.1.0] - 2026-08-11

> **If you are running 1.0.0, read the Fixed section before upgrading.** 1.0.0 could not start
> against a fresh MySQL at all, and a deployment that attempted its migrations may be running with
> two foreign keys silently missing. Remediation steps are below.

### Security

- **Cross-tenant bundle takeover via `POST /{app}/hot-updater/api/bundles` (critical).** The
  `bundles` primary key is `id` alone, and the upsert's `ON DUPLICATE KEY UPDATE` branch did not
  check `app_name`. App A's token could therefore POST app B's bundle id and overwrite that
  bundle's `storage_uri`, `file_hash`, `enabled`, `channel` and `platform`. The row kept
  `app_name = B`, so B's devices continued to receive the bundle — now pointing at content chosen
  by A. That is arbitrary code delivery to another tenant's production devices, from a single
  request, using an id that is public in every device's update-check response. The upsert is now
  preceded by a row-locked ownership check (`SELECT app_name … FOR UPDATE`) and returns `409`.
- **Cross-tenant patch attachment.** `bundle_patches.base_bundle_id` was validated only as a
  foreign key, not by owner, so an app could attach patch rows to another app's bundle. Now `400`.
- **Cross-tenant read in the update-check manifest path.** `resolve_manifest_artifacts` looked up
  the client's current bundle by id without an `app_name` scope. Now scoped; a bundle owned by
  another app resolves as absent, which also stops the response acting as an existence oracle.
- **Bearer tokens are now compared in constant time** (`subtle`). Note that the naive
  `slice.ct_eq()` would have been wrong here — it returns early on a length mismatch and so leaks
  the length of the secret; the comparison instead runs for the length of the *presented* value
  and folds the length check into the result.
- `AUTH_TOKEN_*` is unchanged, but see `SECURITY.md` for what that token can do and why the server
  belongs behind TLS.

### Added

- `GET /health` liveness endpoint, used by the Docker healthcheck.
- `GET /metrics` in Prometheus text format (`METRICS_ENABLED`, default on). Labels are
  deliberately bounded: route classes are a fixed set, `app` is restricted to the names in `APPS`,
  and no metric is labelled by bundle id, app version, fingerprint hash or cohort.
- Per-request access logging with request-id propagation (`RUST_LOG`, `HTTP_LOG_LEVEL`). The
  `tower-http` dependency had been declared since 1.0.0 but was never wired to anything, so the
  server had no request-level logging at all.
- Configurable database pool (`DB_MAX_CONNECTIONS`, `DB_MIN_CONNECTIONS`,
  `DB_ACQUIRE_TIMEOUT_SECS`, `DB_IDLE_TIMEOUT_SECS`) and CORS (`CORS_ALLOWED_ORIGINS`, off by
  default).
- Opt-in rate limiting on the unauthenticated update-check routes only, off by default
  (`RATE_LIMIT_*`). The CLI API is never throttled.
- An explicit request body limit on the CLI API routes.
- **MySQL-backed integration tests** covering auth, cross-app isolation, all six CLI endpoints,
  cursor pagination and the update-check endpoints against a real MySQL 8 container.
- **Pagination and query-parameter parity fixtures** (188 cases) recorded from the real
  `@hot-updater/server` and `@hot-updater/plugin-core`, replayed against ten exported pure
  functions. Every `GET /bundles` query-parameter rule is now covered.
- `--help` / `--version` flags, so a downloaded binary is self-describing.
- Graceful shutdown on SIGINT/SIGTERM — in-flight requests finish before the process exits.
- Prebuilt binaries for Linux (x86_64, aarch64), macOS (Intel, Apple Silicon) and Windows
  (x86_64), published on every `v*` tag with SHA-256 checksums.
- Published to crates.io — installable with `cargo install rn-ota-server-rust`.
- `CONTRIBUTING.md`, `SECURITY.md`, `CHANGELOG.md`, a code of conduct, and issue/PR templates.

### Changed

- Release profile now builds with thin LTO, one codegen unit and stripped symbols.
- Startup failures on bind now exit with a clear log line instead of panicking.
- The Docker image runs as an unprivileged user.
- Declared MSRV is 1.94.1, enforced in CI. The floor comes from the AWS SDK, not from this code.
- README now states up front that this replaces only hot-updater's server; the upstream React
  Native SDK and CLI are still required and are used unchanged.
- Decision parity fixtures expanded from **14 to 169 cases**; pagination parity is now covered at
  all (188 cases) where it previously was not.
- `?idIn=a&idIn=b` — repeated query parameters are now honoured, matching upstream's `getAll`.
  **This is a wire-format change**, but it fixes rather than breaks: previously `serde_urlencoded`
  silently kept only the last occurrence, so a client sending the upstream format was already
  getting wrong results. Consequently `?idIn=a,b` is now **one** id containing a comma, as
  upstream treats it — comma-splitting was our invention and has been removed rather than kept as
  a fallback, since accepting both forms would mangle any id legitimately containing a comma. The
  same applies to `targetAppVersionIn`, whose values contain spaces.

### Fixed

- **The server could not start against a fresh MySQL at all.** The
  `id_ascii_bin_collation` migration was invalid SQL — MySQL 8 rejects
  `CHAR(36) NOT NULL CHARACTER SET … COLLATE …` with error 1064, as the character set and
  collation must precede `NOT NULL`. Since migrations run at startup, every new deployment of
  1.0.0 failed to boot.
- **The same migration's intent was unachievable.** Once the syntax was fixed, `ascii_bin` made
  every read path return 500: MySQL sets the wire-protocol BINARY flag on *any* `_bin` collation,
  and sqlx then refuses to decode such columns into `String`. The id columns now use
  `ascii_general_ci`. See `docs/upstream-parity.md` §3.3 for what this costs.
- **Four update-decision parity bugs**, all found by the fixture expansion:
  - The rollback candidate was being filtered by cohort eligibility. Upstream selects it purely by
    id — only the *update* candidate is eligibility-tested. The effect was severe: a device that
    fell outside a rollout was sent a full native rollback instead of stepping back one bundle.
  - An empty-string `targetAppVersion` was treated as `*` and matched every client version;
    upstream drops such a bundle before any semver evaluation.
  - An empty-string `fingerprintHash` on a bundle could match a request.
  - Id ordering diverges from upstream's ICU `localeCompare`; recorded as a documented deviation
    rather than fixed, and pinned by an ignored fixture case.
- **Seven CLI API pagination parity bugs.** `?before=` returned the first page instead of the
  preceding one; `total` was narrowed by the cursor; `currentPage`/`hasPreviousPage` were computed
  from `page` rather than the row's absolute index; absent cursor keys were sent as `null` instead
  of being omitted; `nextCursor` used the wrong condition on a short final page; `page` combined
  with a cursor applied both; and an empty cursor (`?after=`) produced a bogus `id < ''` predicate
  — for `?before=` an ascending scan of the whole table.
- **Query-parameter contract** now matches upstream exactly, including the 400 message bodies:
  `limit`/`page`/`offset`, `platform`, the boolean params, and `?fingerprintHash=null` meaning SQL
  `IS NULL`. Previously `?limit=-1` was a 500, a large `?page` panicked in debug builds, and
  `?offset=` was silently ignored.
- Database errors are no longer swallowed: a failed count query used to return `total: 0` beside a
  populated `data` array, and a failed patch query used to return a bundle with no patches — which
  on the device path silently degrades an update into a full download. Both now surface.
- Payload validation on `POST`/`PATCH` (id shape, text length against the `TEXT` limit, array
  bounds) closes a set of 500s and silent truncations.

### Operators: read before upgrading

**1. Recovering from a failed 1.0.0 migration.** If a deployment ever attempted the
`id_ascii_bin_collation` migration, it stopped partway: the first `ALTER TABLE` committed before
the invalid statement failed. Such a database has a `success = 0` row in `_sqlx_migrations` **and
has permanently lost both foreign keys on `bundle_patches`**, so orphaned patch rows may have
accumulated undetected since.

The migration's FK drops are now conditional, so clearing the failed row is enough for it to
repair itself — this was verified against a reproduction of the damaged state. The full
copy-pasteable procedure (detect, back up, remove orphans, clear the row, verify) is in the header
of `migrations/20260722010000_id_ascii_bin_collation.sql`. Its detection and inspection steps are
read-only and safe to run against a healthy database.

**2. Do not run migrations with a relaxed `sql_mode`.** This release converts the id columns from
`utf8mb4` to `ascii`. Under MySQL 8's default `STRICT_TRANS_TABLES` an id containing a non-ASCII
byte aborts the `ALTER` with error 1366 and nothing is touched — the safe outcome. Under
`sql_mode=''` MySQL **silently** replaces each non-ASCII character with `?`, which mangles primary
keys and can collapse two distinct ids into one. Check first:

```sql
SELECT id FROM bundles WHERE id <> CONVERT(id USING ascii);
```

If that returns rows, resolve them before upgrading.

**3. Check the `bundle_check_constraints` migration's preconditions.** If any existing row violates
the constraints it adds, the `ALTER` fails and **no constraint is created**, so the server will not
start. Verified against MySQL 8.0.46; find offending rows with:

```sql
SELECT id FROM bundles
 WHERE (target_app_version IS NULL AND fingerprint_hash IS NULL)
    OR rollout_cohort_count < 0 OR rollout_cohort_count > 1000;
```

Both previously-unverified migrations have now been exercised against a live MySQL 8, on a fresh
database and on one already holding data. Their bytes are unchanged from 1.0.0 where they had
already applied successfully, so no `_sqlx_migrations` checksum is invalidated by this release.

## [1.0.0] - 2026-08-05

### Added

- Initial release: wire-compatible reimplementation of the hot-updater self-hosted server.
- appVersion and fingerprint update strategies, staged rollouts, cohorts, bundle rollback,
  manifest/changed-asset responses and bsdiff patch delivery.
- Multi-app support with per-app auth tokens and per-app S3/R2 buckets.
- Update-decision and semver-range parity tests generated from the real `@hot-updater/js` and
  `semver` npm packages, verified against hot-updater 0.35.8.

[Unreleased]: https://github.com/YatogamiRaito/rn-ota-server-rust/compare/v1.2.0...HEAD
[1.2.0]: https://github.com/YatogamiRaito/rn-ota-server-rust/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/YatogamiRaito/rn-ota-server-rust/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/YatogamiRaito/rn-ota-server-rust/releases/tag/v1.0.0
