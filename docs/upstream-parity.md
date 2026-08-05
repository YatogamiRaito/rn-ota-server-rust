# Upstream Parity Log — rn-ota-server-rust ↔ hot-updater

> Last verified: **2026-07-29** — upstream version **0.35.8**
> This file records which npm packages this server is the Rust counterpart of, what was ported
> from which source, and how to verify parity when upgrading upstream.

---

## 1. Which package is this server a Rust port of?

**It is not a port of a single package.** `@hot-updater/server` is the dominant source for the
skeleton, but the decision logic and semver behavior come from separate packages. Verified mapping:

| Rust file             | Upstream source (0.35.8)                                       | What was ported                                                                                |
| --------------------- | -------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `src/routes/mod.rs`   | `@hot-updater/server` → `dist/handler.mjs` (`createHandler`)   | Route table: `app-version/*`, `fingerprint/*`, `api/bundles*` — path shapes match exactly       |
| `src/routes/check.rs` | `@hot-updater/js` → `getUpdateInfo`                            | Update decision: UPDATE / ROLLBACK / UP_TO_DATE, rollout, minBundleId, manifest/patch resolution |
| `src/routes/api.rs`   | `@hot-updater/server` → `dist/handler.mjs` CLI handlers        | `getBundles` filters, cursor pagination, `insertBundle`, `updateBundleById`, `deleteBundleById`  |
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

---

## 2. 0.35.1 → 0.35.8 change analysis

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

## 3. Known gaps and deviations (not new issues introduced by 0.35.8)

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

---

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

- **2026-07-29** — Upstream 0.35.1 → 0.35.8. The server-side packages
  (`server`/`js`/`plugin-core`/`core`) turned out to be byte-for-byte identical; no Rust change was
  needed. The only changes were in the RN SDK (Brotli proguard keep, `Dispatchers.IO` for
  bsdiff/manifest, iOS bridge reload ordering, store emit batching) and the CLI (`bundle delete`
  with multiple ids). This file was created.
