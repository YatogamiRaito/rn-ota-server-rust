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
| `src/routes/check.rs` `decide_update` | `@hot-updater/js` → `getUpdateInfo`            | Update decision: UPDATE / ROLLBACK / UP_TO_DATE, rollout, minBundleId                            |
| `src/routes/check.rs` `plan_manifest_artifacts` | `@hot-updater/server` → `dist/db/updateArtifacts.mjs` | Manifest diff, content-addressed asset paths, brotli `.br` rule, bsdiff patch descriptor |
| `src/routes/check.rs` `make_response` | `@hot-updater/server` → `dist/db/pluginCore.mjs` | Response shape: forced rollback, which keys are omitted rather than nulled                      |
| `src/routes/api.rs`   | `@hot-updater/server` → `dist/handler.mjs` (`handleGetBundles`, query-param layer) | Query-parameter contract: `limit`/`page`/`offset`, `platform`, the `getAll` array params, booleans, nullable strings — including the exact 400 messages |
| `src/routes/api.rs`   | `@hot-updater/plugin-core` → `dist/createDatabasePlugin.mjs` (`getBundlesWithLegacyCursorFallback`) | Cursor pagination: `buildCursorPageQuery`, `createPaginatedResult`, `calculatePagination`        |
| `src/routes/api.rs`   | `@hot-updater/server` → `dist/handler.mjs` CLI handlers        | `insertBundle`, `updateBundleById`, `deleteBundleById`                                          |
| `src/semver.rs`       | `@hot-updater/plugin-core` → `semverSatisfies` + npm `semver`  | Exact reproduction of `semver.coerce()` + `semver.satisfies()` behavior                          |
| `src/cohort.rs`       | `@hot-updater/js` (cohort/rollout helpers)                     | JS `hash << 5 - hash` string hash, 1000-slot bucketing, `positive_mod`, slug validation          |
| `src/models.rs`       | `@hot-updater/core` (type definitions)                         | `Bundle` / `BundlePatch` field shape                                                             |
| `src/storage.rs`      | `@hot-updater/aws` → `s3Storage` (presign path)                | `s3://` URI parsing + S3/R2 presigned URL generation                                             |
| `migrations/*.sql`    | `@hot-updater/server` → `dist/schema/v0_31_0.mjs`              | `bundles`, `bundle_patches`, `hot_updater_settings` tables                                       |

**Evidence:** `tests/generate_artifacts_fixtures.mjs` drives the real
`createHotUpdater(...).getAppUpdateInfo`; `tests/generate_decision_fixtures.mjs` calls the real `@hot-updater/js`
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

**A correction, kept visible on purpose.** An earlier revision of this section claimed that
`manifestUrl`, `manifestFileHash` and `changedAssets` degrading on a storage failure "is also
upstream's behaviour", citing `fetchBundleManifest`'s `if (!fileUrl) return null`. That was wrong,
and wrong in an instructive way: it conflated a manifest that is **absent** with one that could
not be **read**. The same conflation was live in this server's code and is defect 8 of §3.7.

What upstream actually does, measured rather than inferred:

- `updateArtifacts.ts` performs four storage operations after the primary bundle is presigned, and
  **catches exactly one** — the per-asset `resolveFileUrl`, and only when a bsdiff patch covers
  that asset. Everything else, `resolveHbcPatchDescriptor`'s own presign included (it has no `try`
  at all), propagates into `getAppUpdateInfo`, which awaits it without a catch, and becomes a 5xx.
- The `if (!fileUrl) return null` arm is **not** a storage-failure path. With a storage plugin
  registered, `resolveFileUrl` throws rather than returning a falsy value (`createStorageAccess`
  raises `"Storage plugin returned empty fileUrl"`), so that arm is reachable only when
  `storageUri` is itself null.
- What upstream *does* treat as "no artifacts, ship the update anyway" is a manifest that is
  **absent or malformed** — `readText` maps a missing object to `null` (`if (!response.ok) return
  null`), `fetchBundleManifest` catches its own `JSON.parse`, and `isBundleManifest` rejects a
  well-formed-but-wrong document. All three answer 200 with no artifacts.

This server now matches all of it; see §3.7. Reading `fetchBundleManifest` and concluding "storage
failures degrade" is an easy step to take — it took recording the real thing to see that `readText`
returns null for one case and throws for the other.

Guarded by `tests/storage_integration_tests.rs::an_update_response_never_carries_a_null_file_url`
and `::update_check_fails_loudly_when_a_bundle_points_at_another_apps_bucket`.

### 3.5 CLI API request/response bodies — recorded, and what it changed

`tests/fixtures/cli_api_fixtures.json` (generated by `tests/generate_cli_api_fixtures.mjs`,
replayed by `tests/cli_api_parity_tests.rs`) records the CLI API's **body** contract from the
real 0.35.12 stack — `createHandler` over `createPluginDatabaseCore` over
`createDatabasePlugin` over upstream's own `rowToBundle`/`bundleToRow`/`bundleToPatchRows`.
132 cases. Ten defects it found, all now fixed:

1. **`metadata` was emitted as `{}` where upstream omits the key.** `rowToBundle` sets
   `metadata: parseBundleMetadata(record.metadata)`, which is `undefined` for a NULL,
   unparseable or non-object column, and `JSON.stringify` drops it. `metadata` is the **only**
   key a bundle body ever omits. `src/routes/api.rs` `parse_bundle_metadata` +
   `ClientBundle::metadata: Option<_>` with `skip_serializing_if`.
2. **`target_cohorts` holding the array as a JSON *string* parsed as `None`.** Upstream's
   `parseTargetCohorts` parses that second layer. The old
   `serde_json::from_value::<Vec<String>>` did not, so the bundle looked untargeted.
3. **A single non-string element discarded the whole `target_cohorts` list.** Upstream
   filters the offending entries and keeps the rest.
   2 and 3 also affected **`src/routes/check.rs`**, which held a second copy of the same
   stricter rule — on the device path, where it silently drops an explicit cohort list and
   falls back to the rollout percentage. Both now call one `api::parse_target_cohorts`.
4. **A malformed `patches[]` entry rejected the whole publish.** Upstream's
   `readBundlePatchArray` filters entries that are not four strings, tolerates a non-array
   `patches`, and de-duplicates repeated `baseBundleId`s keeping the first. `get_bundle_patches`
   now mirrors it, and the old 400 `Duplicate patch baseBundleId "…"` is gone.
5. **PATCH rejected bodies upstream accepts, and accepted one it rejects.**
   `requireBundlePatchPayload` collapses an **array body to its first element**, 400s
   `Invalid bundle payload` for a non-object, and treats a present `id` — **`null` included** —
   that differs from the route id as `Bundle id mismatch`. A `Json<UpdateBundlePayload>`
   extractor answered axum's generic 422 for the first two and let `{"id": null}` through.
   `require_bundle_patch_payload` now mirrors it.
6. **Every error body was `text/plain`; upstream's are JSON.** `createHandler` answers
   `{"error": …}` (400/404) or `{"error": "Internal server error", "message": …}` (500), always
   with `Content-Type: application/json`. A client calling `res.json()` on a failure — which
   the CLI does — got a parse error instead of the message. `error_response()` in
   `src/routes/api.rs` is now the single exit for every error in that module.

7. **An explicit `null` in a PATCH was ignored, so no nullable column could ever be cleared.**
   `mergeBundleUpdate` skips only `undefined`; a present `null` is assigned. A plain
   `Option<T>` folds JSON `null` to `None`, which `build_update_query` read as "leave
   unchanged" — so `PATCH {"message": null}` answered `200 {"success":true}` and left the row
   untouched. `UpdateBundlePayload` now uses `Option<Option<T>>` throughout (see
   `double_option`). The three classes, each recorded:
   - the **eight nullable columns** (`git_commit_hash`, `message`, `target_app_version`,
     `fingerprint_hash`, `target_cohorts`, and the three manifest/asset ones) → SET NULL;
   - `metadata` → reset to `{}` and `rollout_cohort_count` → reset to `1000`, because
     `bundleToRow` writes `?? {}` and `?? DEFAULT_ROLLOUT_COHORT_COUNT` and both columns are
     NOT NULL with a default;
   - the **six NOT NULL columns** (`platform`, `shouldForceUpdate`, `enabled`, `fileHash`,
     `channel`, `storageUri`) → 400 naming the field. Upstream hands the adapter the null and
     lets the *database* refuse it; the fixture's in-memory store enforces no constraints, so
     upstream's HTTP answer there is unobservable. Both ends fail the request; only the status
     and message differ, the same call as §3.6.
8. **`patches` was not patchable at all.** It is one of upstream's two
   `REPLACE_ON_UPDATE_KEYS`, so a PATCH carrying it replaces the whole set and `[]` clears it.
   `UpdateBundlePayload` had no such field, so the key was dropped and the patch rows were
   left untouched. It now writes rows, sharing a transaction with the column UPDATE.
9. **`metadata` was replaced where upstream deep merges it.** `mergeBundleUpdate` is an
   es-toolkit `mergeWith` and `metadata` is *not* in `REPLACE_ON_UPDATE_KEYS`, so objects
   recurse, **arrays merge index by index** (`[1,2,3]` patched with `[9]` → `[9,2,3]`), and a
   metadata key can never be removed. `merge_bundle_metadata` reproduces it; its doc comment
   carries the recorded case for every rule, because this is precisely the behaviour a later
   reader will try to "fix" into a replace.
10. `bundle_patches` was read with `ORDER BY order_index ASC` alone, and upstream tie-breaks
    equal indices on `base_bundle_id`. `patches[0]` fills the deprecated
    `patchBaseBundleId`/`patch*` mirror fields, so an unstable order there is an unstable
    response body. Both patch queries now order by `order_index ASC, base_bundle_id ASC`.

> **One PATCH body, two merge semantics — do not unify them.** `metadata` deep merges;
> `targetCohorts` and `patches` are replaced whole. Same request, same handler, opposite
> behaviour, because the latter two are upstream's `REPLACE_ON_UPDATE_KEYS`. A case that
> patches `metadata` **and** `targetCohorts` in the *same* body is recorded for exactly this
> reason, and `one_patch_body_has_two_merge_semantics` asserts both halves — collapsing the
> rules breaks one of them, and a single-key test would not notice. This is the sibling of the
> truthy-vs-nullable query-parameter pair described in `tools/fixture-gen/README.md`.

**Not a deviation, though it looks like one.** `patchBaseBundleId`, `patchBaseFileHash`,
`patchFileHash` and `patchStorageUri` are accepted by a PATCH upstream and merged into its
in-memory bundle, but `bundleToRow` has no column for any of them and `bundleToPatchRows`
reads only `patches` — so they never reach storage and are regenerated from `patches[0]` on
the way out. `UpdateBundlePayload` ignoring them is the same outcome.
`tests/cli_api_parity_tests.rs` proves the column set from the recorded rows rather than
trusting that claim.

### 3.6 Payload-validation failures answer 400 here, 500 upstream

`assertBundlePersistenceConstraints` throws a plain `Error`, so it reaches `createHandler`'s
catch-all rather than its `HandlerBadRequestError` branch:

```
POST /api/bundles  {"targetAppVersion": null, "fingerprintHash": null, …}
upstream: 500 {"error":"Internal server error","message":"Bundle must define either targetAppVersion or fingerprintHash."}
here    : 400 {"error":"Bundle must define either targetAppVersion or fingerprintHash."}
```

Same for `rolloutCohortCount must be an integer between 0 and 1000.`, the
`Invalid target cohort "…"` message, and `targetBundleId not found` for a PATCH against a
missing bundle (404 `Bundle not found` here). **The message text matches exactly** — only the
status and the envelope key differ.

**Kept deliberately, and the evidence rather than the principle.** The worry was that a client
reading `.message` would see the reason from upstream and nothing from us. Three findings
settle it:

1. **No upstream client calls these endpoints at all.** Across `hot-updater@0.35.12`,
   `@hot-updater/console@0.35.12` and every server-side package, the only references to
   `api/bundles` are `@hot-updater/server`'s own handler and its own specs. The CLI bundle
   makes two `fetch` calls in total and neither is to a bundles route — it publishes through
   its configured *database plugin*, and the console does the same through server functions.
   `createHandler` even defaults `routes.bundles` to **`false`**. These endpoints exist for
   third-party consumers, which is what this server's users are.
2. **Upstream's own specs read `.error`, never `.message`.** `handler.spec.ts` asserts
   `response.json()` resolves to `{ error: "<the real message>" }` for every 400 it covers.
   Our 400s carry the real message in `.error`, so a client written against those specs reads
   it correctly from us.
3. **The 500 shape is untested and incidental.** No spec exercises a constraint violation
   through the HTTP handler at all — `pluginCore.spec.ts:1038` asserts
   `rejects.toThrow("Bundle must define either targetAppVersion or fingerprintHash.")` at the
   *api* level. The 500 is what happens to fall out of a plain `Error` reaching the catch-all,
   not a designed contract.

So no real client's field-reading behaviour is broken by the 400, and matching the 500 would
mean deliberately reporting a caller error as a server error while *losing* the message from
the `.error` field that upstream's own tests read. Recorded in
`tests/fixtures/cli_api_fixtures.json` so it stays a decision rather than an accident.

### 3.7 Artifact resolution — recorded, and the deviation it removed

`tests/fixtures/artifacts_fixtures.json` (generated by `tests/generate_artifacts_fixtures.mjs`,
replayed by `tests/artifacts_parity_tests.rs`) records the artifact layer from the real 0.35.12
stack through the public `createHotUpdater({database, storages}).getAppUpdateInfo`:
`resolveManifestArtifacts`, `resolveHbcPatchDescriptor`, `resolveUniqueHbcAssetPath`,
`resolveChangedAssets` and `makeResponse`. 101 cases.

**There is no deviation left on this surface.** This server previously degraded — 200 with fewer
artifacts — where upstream throws, on five recorded cases. It now propagates, matching upstream,
and `KNOWN_DEGRADE_INSTEAD_OF_5XX` in the replay is empty with an assertion that the deviating set
equals it exactly, so a new divergence cannot appear unnoticed.

**Upstream's one catch is preserved and pinned.** `resolveChangedAssets` swallows a per-asset
presign failure only when a bsdiff patch covers that asset — that asset is emitted with `patch`
and no `file`, because the device can still reconstruct it. Everything else propagates.
`the_per_asset_presign_catch_is_exactly_as_wide_as_upstreams` asserts the catch is neither
widened nor narrowed, and was verified to fail in both directions.

Eight defects were found here and all are fixed; each carries a `bug_*` regression lock named
after the symptom. The one worth knowing about is the last, because it was found *by* removing
the deviation above rather than by the recording alone: `readText` maps a **missing** object to
`null` and only **throws** on a real failure, so "the manifest was never uploaded" and "storage is
down" are different answers upstream. This server conflated them. Propagating naively would have
turned every bundle published before the manifest columns existed into a permanent 500 — a total
outage for those bundles, introduced in the name of compatibility. `read_s3_file_optional` keeps
the two apart, and `bug_a_missing_manifest_object_was_conflated_with_an_unreadable_one` locks it.
### 3.8 `enabled` and `shouldForceUpdate` have defaults here and none upstream

Upstream's `v0_31_0` schema declares `channel` with `.defaultTo("production")` and
`rollout_cohort_count` with `.defaultTo(1000)` — both of which `migrations/20260714000000_init.sql`
matches — but `enabled` and `should_force_update` are plain `bool(...)` columns with **no default
at all**. `bundleToRow` passes an omitted field through as `undefined`, so upstream's adapter
leaves the column out of the insert and the database rejects the row.

This server is more permissive: the columns carry `DEFAULT TRUE` / `DEFAULT FALSE`, and
`create_bundles` additionally applies `unwrap_or(true)` / `unwrap_or(false)`. A `POST` that omits
either field succeeds here and fails upstream.

**Kept deliberately.** The stock `hot-updater` CLI always sends both, so this is unreachable
through it; the exposure is third-party clients, which — since no upstream client calls these
endpoints at all — is the whole audience. Matching upstream would mean rejecting requests that
work today, to reproduce a rejection that comes from a missing column default rather than from any
deliberate contract. The permissive direction cannot corrupt data: both defaults are the value a
caller omitting the field almost certainly intends.

Recorded rather than fixed so it stays a decision. If the reasoning ever stops holding, the change
is to drop the two `unwrap_or` calls and let the column defaults stand alone — which still differs
from upstream, since upstream has no column default either.

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
- `@hot-updater/server` `dist/db/updateArtifacts.mjs` (manifest diff, `sha256/xx/<hash><ext>`
  paths, the brotli `.br` rule, the bsdiff descriptor) → `src/routes/check.rs`
  `plan_manifest_artifacts` (caught by `tests/artifacts_parity_tests.rs`)
- `plugin-core/semverSatisfies` → `src/semver.rs` (caught by the fixture test)
- New `dist/schema/v0_XX_X.mjs` version → new `migrations/*.sql` + `hot_updater_settings.version`
- New field on `@hot-updater/core` `Bundle` → `src/models.rs` + migration + `api.rs` map functions
- `standaloneRepository` route/body format → `src/routes/api.rs` `CLIBundle` / `UpdateBundlePayload`

---

## Changelog

- **2026-08-12** — Manifest/asset-diff parity. 101 cases recorded from the real 0.35.12
  `updateArtifacts.ts` / `pluginCore.ts` through the public `getAppUpdateInfo`
  (`tests/fixtures/artifacts_fixtures.json`). Seven defects found and fixed in
  `src/routes/check.rs`: a panic on a manifest `fileHash` shorter than two bytes, backslash
  normalisation making a non-brotli asset look brotli, `encodeURIComponent`/empty-segment
  differences in the storage key, case-insensitive bsdiff base matching, empty strings treated as
  present where upstream treats them as absent, manifest validation looser than `isBundleManifest`,
  and optional keys serialised as `null` where upstream omits them. `plan_manifest_artifacts` was
  extracted as a pure function so the rules replay without Docker. §3.4's claim about degradation
  was corrected — it conflated an absent manifest with an unreadable one, and so did the code; the
  artifact layer now propagates storage failures exactly as upstream does, leaving no deviation on
  that surface. Recorded as §3.7.

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
