# Upstream Parity Log — rn-ota-server-rust ↔ hot-updater

> Last verified: **2026-08-12** — upstream version **0.35.12**
> This file records which npm packages this server is the Rust counterpart of, what was ported
> from which source, and how to verify parity when upgrading upstream.

---

## 1. Which package is this server a Rust port of?

**It is not a port of a single package.** `@hot-updater/server` is the dominant source for the
skeleton, but the decision logic and semver behavior come from separate packages. Verified mapping:

| Rust file             | Upstream source (0.35.12)                                      | What was ported                                                                                |
| --------------------- | -------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `src/routes/mod.rs`   | `@hot-updater/server` → `dist/handler.mjs` (`createHandler`)   | Route table: `app-version/*`, `fingerprint/*`, `api/bundles*` — path shapes match exactly       |
| `src/routes/check.rs` | `@hot-updater/js` → `getUpdateInfo`                            | Update decision: UPDATE / ROLLBACK / UP_TO_DATE, rollout, minBundleId, manifest/patch resolution |
| `src/routes/api.rs`   | `@hot-updater/server` → `dist/handler.mjs` (`handleGetBundles`, query-param layer) | Query-parameter contract: `limit`/`page`/`offset`, `platform`, the `getAll` array params, booleans, nullable strings — including the exact 400 messages |
| `src/routes/api.rs`   | `@hot-updater/plugin-core` → `dist/createDatabasePlugin.mjs` (`getBundlesWithLegacyCursorFallback`) | Cursor pagination: `buildCursorPageQuery`, `createPaginatedResult`, `calculatePagination`        |
| `src/routes/api.rs`   | `@hot-updater/server` → `dist/handler.mjs` CLI handlers        | `insertBundle`, `updateBundleById`, `deleteBundleById`                                          |
| `src/semver.rs`       | `@hot-updater/plugin-core` → `semverSatisfies` + npm `semver`  | Exact reproduction of `semver.coerce()` + `semver.satisfies()` behavior                          |
| `src/cohort.rs`       | `@hot-updater/js` (cohort/rollout helpers)                     | JS `hash << 5 - hash` string hash, 1000-slot bucketing, `positive_mod`, slug validation          |
| `src/models.rs`       | `@hot-updater/core` (type definitions)                         | `Bundle` / `BundlePatch` field shape                                                             |
| `src/storage.rs`      | `@hot-updater/aws` → `s3Storage` (presign path)                | `s3://` URI parsing + S3/R2 presigned URL generation                                             |
| `migrations/*.sql`    | `@hot-updater/server` → `dist/schema/v0_31_0.mjs`              | `bundles`, `bundle_patches`, `hot_updater_settings` tables                                       |

**Evidence:** `tests/generate_decision_fixtures.mjs` calls the real `@hot-updater/js`
`getUpdateInfo`, and `tests/generate_semver_fixtures.mjs` calls the real
`@hot-updater/plugin-core` `semverSatisfies`, recording input/output pairs;
`tests/decision_tests.rs` and `tests/semver_parity_tests.rs` verify the Rust side against those
fixtures. In other words, the parity claim is a tested claim.

### Intentional deviations from upstream

1. **Multi-app `{app}` prefix.** Upstream is single-app (`/api/...`). Here every route starts with
   `/{app}/hot-updater/...`, where the set of valid app names comes from the `APPS` env var
   (`src/config.rs`). The extra `app_name` column on `bundles` and the `bundles_app_name_idx`
   index exist for this.
2. **Per-app auth token + per-app R2 bucket/credentials.** `src/routes/api.rs` `authorize()`
   expects `Bearer <AUTH_TOKEN_<APP>>`. Upstream has no such built-in authorization (it is left
   to the host framework).
3. **No DB adapter layer.** Upstream ships kysely/drizzle/prisma/mongodb adapters; this server
   uses `sqlx` + MySQL directly (`src/db.rs`). Migrations are hand-written; `hot-updater db migrate`
   is not used.
4. **`GET /version` reports this server's own version** (`CARGO_PKG_VERSION`), not the upstream
   hot-updater version. See §3.1.
5. **Bundle id ordering is byte-wise, upstream's is ICU `localeCompare`.** See §3.3 — this is the
   one behavioural deviation in the decision path, and it is pinned by an `#[ignore]`d fixture
   case rather than hidden.

> **A warning about the phrase "cursor pagination" and `@hot-updater/server`.** That package does
> **not** implement cursor pagination — nothing in it declares `supportsCursorPagination`, so
> `createDatabasePlugin.getBundles` falls through to `getBundlesWithLegacyCursorFallback` in
> `@hot-updater/plugin-core`. That fallback is the real code path for the self-hosted server. An
> earlier investigation looked in the wrong package and concluded there was nothing to match; six
> deviations were hiding there. Read `plugin-core`, not `server`.

---

## 2. 0.35.8 → 0.35.12 change analysis

**One change on the server side, and it is a dependency swap: npm `semver` was replaced with
[`verkit`](https://github.com/sxzz/verkit), a zero-dependency reimplementation.** It lands in
exactly the two places this project reproduces:

| File | 0.35.8 | 0.35.12 |
| --- | --- | --- |
| `@hot-updater/plugin-core` `semverSatisfies` | `semver.satisfies(semver.coerce(v).version, range)` | `satisfies(coerce(v), range)` from `verkit` |
| `@hot-updater/server` `handler.ts` | `semver.valid` + `semver.gte` | `normalize` + `isGreaterOrEqual` from `verkit` |

**The decision algorithm itself did not change.** The `getUpdateInfo` region of
`@hot-updater/js` `dist/index.mjs` is identical between the two versions, and so are the counts of
every construct the port depends on (`isEligibleUpdateCandidate`, `rollbackCandidate`,
`updateCandidate`, `localeCompare`, `NIL_UUID`, `shouldForceUpdate`). `@hot-updater/core` differs
only in the version string in its `package.json`.

**The swap is behaviour-preserving, measured rather than assumed.** `semver@7.8.5` and
`verkit@0.3.2` were run side by side over all 139 recorded cases in
`tests/fixtures/semver_fixtures.json` — 111 `satisfies`, 28 `coerce` — with zero differences. The
SDK-header path was checked separately: `verkit.normalize` behaves like `semver.valid`, not like
`coerce` (`"0.35"` still yields `null`), so the deviation recorded in §3.2 is unchanged rather
than closed.

Regenerating all three fixture files against 0.35.12 produced **byte-identical output** to the
files recorded against 0.35.8. That is the strongest available statement: nothing this server
reproduces behaves differently.

One consequence for the generator: `tests/generate_semver_fixtures.mjs` now records
`verkit.coerce` rather than `semver.coerce`, because continuing to call a package upstream no
longer uses would pin the wrong ground truth. `verkit.coerce` returns a plain string where
`semver.coerce` returned an object.

```bash
# How this was established
npm pack @hot-updater/server@0.35.8 && npm pack @hot-updater/server@0.35.12   # + js, plugin-core, core
diff -rq server-0.35.8 server-0.35.12
cd tools/fixture-gen && npm ci && cd ../..
node tests/generate_semver_fixtures.mjs | cmp - tests/fixtures/semver_fixtures.json
```

---

## 2a. 0.35.1 → 0.35.8 change analysis (historical)

This project pinned 0.35.1 (the lockfile partly resolved 0.35.3). It was upgraded to 0.35.8.

### Packages that affect the server side: **nothing changed**

The 0.35.1 and 0.35.8 tarballs of `@hot-updater/server`, `@hot-updater/js`,
`@hot-updater/plugin-core` and `@hot-updater/core` are **byte-for-byte identical apart from the
version strings in `package.json` and `dist/package.*`.** Verification:

```bash
npm pack @hot-updater/server@0.35.1 && npm pack @hot-updater/server@0.35.8   # + js, plugin-core, core
diff -rq server-0.35.1 server-0.35.8
# → only package.json, dist/package.mjs, dist/package.cjs (version string)
```

Conclusion: **the decision algorithm, route shape, DB schema (still `v0_31_0`) and response format
did not change. No code change was REQUIRED on the Rust side for 0.35.8 compatibility.**

The fixtures were regenerated against 0.35.8 and compared:

- `semver_fixtures.json` → **no difference**
- `decision_fixtures.json` → only a prettier formatting difference (single-line array vs
  multi-line); semantically identical. The files were deliberately left unchanged.

`cargo test` and `cargo clippy --all-targets -- -D warnings` pass clean.

### Packages that actually changed: client/CLI only

| Package                     | Change                                                                                                                                                | Affects the server?    |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------- |
| `@hot-updater/react-native` | `src/store.ts`: `emitChange` is now batched via `requestAnimationFrame` (fewer redundant re-renders)                                                    | No                     |
| `@hot-updater/react-native` | `android/proguard-rules.pro`: **Brotli decoder keep rule** — R8 stripped the `DictionaryData` static init and broke `.tar.br` extraction in release builds | No                     |
| `@hot-updater/react-native` | `BundleFileStorageService.kt`: `BsdiffPatch.apply` and `updateBundleFromManifest` now run inside `withContext(Dispatchers.IO)` — main-thread blocking fixed | No                     |
| `@hot-updater/react-native` | `ios/HotUpdater.mm`: new `HotUpdaterTriggerReloadCommand` — fixes reloading before bridge invalidation on the old architecture                          | No                     |
| `hot-updater` (CLI)         | `bundle delete` now takes multiple ids (`<bundle-ids...>`); issues a separate `DELETE /api/bundles/:id` per id                                          | No — already supported |
| `@hot-updater/bsdiff`       | `assets/hdiff.wasm` recompiled                                                                                                                          | No                     |
| `@hot-updater/console`      | build output refreshed                                                                                                                                  | No (not used)          |
| `@hot-updater/cli-tools`    | Bundler variable-naming noise only                                                                                                                      | No                     |

**The three fixes with native impact (proguard + Kotlin threading + iOS reload) only take effect
with a new native build — they do not arrive via an OTA bundle.** Benefiting from them requires
rebuilding the mobile apps and shipping them to the stores.

**No app-side action is needed for the Brotli proguard rule** (verified 2026-07-29):
`@hot-updater/react-native/android/build.gradle` declares
`consumerProguardFiles 'proguard-rules.pro'`, so the rule flows into R8 from the library
automatically. The keep target (`com.hotupdater.vendor.brotli.**`) matches the real package name
inside the package's `libs/hot-updater-brotli-dec-1.2.0.jar` (`com.hotupdater.vendor.brotli.dec.*`).
With `minifyEnabled`/`shrinkResources` enabled in release, a rebuild is all that is needed.

---

## 3. Known gaps and deviations

### 3.1 `GET /version` reports this server's version

Upstream `createHandler` always adds `GET {basePath}/version` and returns the hot-updater version,
e.g. `{ "version": "0.35.8" }`. The `hot-updater doctor` command (`doctorInfrastructure.ts`) calls
this endpoint to report whether the server infrastructure is up to date.

This server implements `/version`, but returns **its own** crate version (`CARGO_PKG_VERSION`)
rather than an upstream hot-updater version — this project is versioned independently. The deploy
flow (`standaloneRepository`) does not call `/version`, so publishing is unaffected; only
`hot-updater doctor` may report an unexpected version string.

### 3.2 `valid` vs `coerce` difference in the SDK version header

Upstream (`handler.mjs`, `supportsExplicitNoUpdateResponse`) evaluates the
`Hot-Updater-SDK-Version` header with **`semver.valid()`** — a partial string such as `"0.35"` is
invalid and yields a `null` body. The Rust side (`src/routes/check.rs`) uses **`coerce_version()`**
at the same place; `"0.35"` is accepted as `0.35.0` and `{"status":"UP_TO_DATE"}` is returned.

Since the real SDK always sends a full semver (`HOT_UPDATER_SDK_VERSION = "0.35.8"`), this makes no
difference in practice. It is recorded here as a known deviation.

### 3.3 Bundle id ordering: byte-wise here, ICU `localeCompare` upstream

Upstream orders and compares bundle ids with `String.prototype.localeCompare`, which is ICU
collation: it sorts by base letter first, so `…0000a` sorts **before** `…0000A`. This server
compares ids byte-wise in Rust (`…0000A` first, since `A` is `0x41` and `a` is `0x61`).

**Why this is not being fixed.** Reproducing ICU collation identically in both Rust and MySQL is
not realistically achievable, and the alternative — a `_bin` collation on the id columns — is
closed to us: MySQL sets the wire-protocol BINARY flag on *any* `_bin` collation, and sqlx then
refuses to decode such a column into `String`, which fails every read path in the server. The id
columns therefore use `CHARACTER SET ascii COLLATE ascii_general_ci` (see
`migrations/20260722010000_id_ascii_bin_collation.sql`).

**Exposure.** Nil for ids the upstream CLI generates, which are lowercase-hex UUIDs — for the
alphabet `[0-9a-f-]`, byte order and ICU order agree. It bites only where mixed-case hex ids
exist, and under `ascii_general_ci` the database cannot even hold both `…0000a` and `…0000A`:
they collide on the primary key. A second-order consequence is that SQL comparisons are now
case-*insensitive* while the Rust ones are case-sensitive; the places where that mattered are
normalised in `src/routes/api.rs`, each marked with a comment saying to revert the normalisation
if the columns ever move to `_bin`.

**How it is pinned.** Fixture case `D07` records upstream's real answer and is replayed by
`tests/decision_tests.rs::test_known_limitation_id_localecompare_ordering`, which is `#[ignore]`d
with this reason. The mechanism is deliberately narrow: the test asserts that the known limitation
matches **exactly one** fixture case, so a second divergence cannot hide behind the same
exemption. Run it with `cargo test -- --ignored` to see the deviation demonstrated.

---

### 3.4 `fileUrl: null` is reserved for the reset-to-built-in shape

`fileUrl` is nullable in the published type — `@hot-updater/core` 0.35.8, `dist/index.d.mts:179`:
`interface AppUpdateAvailableInfo { ...; fileUrl: string | null; ... }` — but in the protocol it
does not mean "no URL available". It means **reset to the bundle built into the binary**, and the
device acts on it: `android/.../BundleFileStorageService.kt` (`if (fileUrl.isNullOrEmpty())`) and
`ios/.../BundleFileStorageService.swift` (`guard let validFileUrl = fileUrl else`) clear the bundle
URL, reset the metadata, **delete every downloaded bundle** and report success. `checkForUpdate.ts`
passes the null straight through without a null check; only `isResetToBuiltInResponse` inspects it,
and that additionally requires `status === "ROLLBACK" && id === NIL_UUID`.

**Upstream never pairs it with `UPDATE`.** `@hot-updater/server` `src/storageAccess.ts`
`resolveFileUrl` returns null only when `storageUri` itself is null and **throws** on every real
failure (`"Storage plugin returned empty fileUrl"`, unknown protocol, non-HTTP(S) url), and
`src/db/pluginCore.ts:359` awaits it with no `try`/`catch` — so a presign failure surfaces as a
5xx, which `pluginCore.spec.ts:910` pins (`rejects.toThrow("storage read failed")`). The only
upstream producers of a null `fileUrl` are `INIT_BUNDLE_ROLLBACK_UPDATE_INFO` and
`withJwtSignedUrl`'s `if (data.id === NIL_UUID || !storageUri)`.

**This server matches that.** `make_response` in `src/routes/check.rs` fails the request when the
primary bundle cannot be presigned, and the handlers turn that into `500 Storage error`; a failed
check is retried harmlessly, whereas a null-`fileUrl` `UPDATE` would silently wipe the device's OTA
state (and, with `shouldForceUpdate`, reload into the same answer indefinitely — the client's loop
guard compares bundle ids, and the id genuinely is newer than the built-in one). The only response
this server emits with `file_url: None` is `Decision::InitRollback`, which carries `NIL_UUID` and
`ROLLBACK` — upstream's `INIT_BUNDLE_ROLLBACK_UPDATE_INFO` shape exactly.

`manifestUrl`, `manifestFileHash` and `changedAssets` are unaffected and still degrade to null on a
storage failure. That is also upstream's behaviour (`updateArtifacts.ts` `fetchBundleManifest`
returns null `if (!fileUrl)`, and `resolveChangedAssets` drops the whole set rather than emitting a
partial one): they are download optimisations, and losing them costs bytes, not correctness.

Guarded by `tests/storage_integration_tests.rs::an_update_response_never_carries_a_null_file_url`
and `::update_check_fails_loudly_when_a_bundle_points_at_another_apps_bucket`.

## 4. Procedure for an upstream upgrade

```bash
# 1. Bump the version ranges in your React Native app(s)
yarn install

# 2. Did anything actually change in the server-relevant packages?
cd /tmp && for p in server js plugin-core core; do
  npm pack @hot-updater/$p@<OLD> && npm pack @hot-updater/$p@<NEW>
done
# extract the tarballs and diff -rq — investigate any difference beyond package.json/dist/package.*

# 3. Regenerate the fixtures against the real packages and compare
#    (see tools/fixture-gen/README.md — the generators need a cwd that can resolve
#     @hot-updater/js + plugin-core)
node tests/generate_decision_fixtures.mjs > tests/fixtures/decision_fixtures.json
node tests/generate_semver_fixtures.mjs   > tests/fixtures/semver_fixtures.json

# 4. Verify
cargo test && cargo clippy --all-targets --all-features -- -D warnings

# 5. Native side (only if @hot-updater/react-native changed)
cd ios && pod install
# On Android consumerProguardFiles flows automatically; just rebuild
```

### Checklist — which upstream change breaks this server?

- `dist/handler.mjs` route table or query parameters → `src/routes/mod.rs` + `api.rs`
- `@hot-updater/js` `getUpdateInfo` → `src/routes/check.rs` (caught by the fixture test)
- `plugin-core/semverSatisfies` → `src/semver.rs` (caught by the fixture test)
- New `dist/schema/v0_XX_X.mjs` version → new `migrations/*.sql` + `hot_updater_settings.version`
- New field on `@hot-updater/core` `Bundle` → `src/models.rs` + migration + `api.rs` map functions
- `standaloneRepository` route/body format → `src/routes/api.rs` `CLIBundle` / `UpdateBundlePayload`

---

## Changelog

- **2026-08-11** — Storage layer hardening. Recorded §3.4: a presign failure for the primary
  bundle now fails the update-check (5xx) instead of answering `UPDATE` with `fileUrl: null`,
  matching upstream's `resolveFileUrl`-throws behaviour. Verified against the vendored 0.35.8
  sources in `tools/fixture-gen/node_modules/@hot-updater/` and the `v0.35.8` RN client.

- **2026-08-12** — Upstream 0.35.8 → 0.35.12. The only server-side change is npm `semver` being
  replaced with `verkit`; the decision algorithm is byte-identical. Verified behaviour-preserving
  by running both implementations over all 139 semver fixture cases (zero differences) and by
  regenerating all three fixture files against 0.35.12 (byte-identical output). See §2.
- **2026-07-29** — Upstream 0.35.1 → 0.35.8. The server-side packages
  (`server`/`js`/`plugin-core`/`core`) turned out to be byte-for-byte identical; no Rust change was
  needed. The only changes were in the RN SDK (Brotli proguard keep, `Dispatchers.IO` for
  bsdiff/manifest, iOS bridge reload ordering, store emit batching) and the CLI (`bundle delete`
  with multiple ids). This file was created.
