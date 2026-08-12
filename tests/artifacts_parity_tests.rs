//! Replays `tests/fixtures/artifacts_fixtures.json` — recorded from the real hot-updater
//! 0.35.12 packages by `tests/generate_artifacts_fixtures.mjs` — against this server.
//!
//! # What this covers that the other parity suites do not
//!
//! `decision_fixtures.json` pins which *bundle* a device is given. It cannot pin anything in
//! this file, because upstream's decision layer is manifest-agnostic: `makeResponse` in
//! `pluginCore.ts` returns only `{ id, message, shouldForceUpdate, status, storageUri,
//! fileHash }`. `fileUrl`, `manifestUrl`, `manifestFileHash` and `changedAssets` are resolved
//! *after* the decision, in `updateArtifacts.ts`, and had no recorded ground truth at all.
//!
//! # Three stages
//!
//! 1. `every_recorded_case_satisfies_the_documented_upstream_rules` checks each recorded case
//!    against the rules stated in `updateArtifacts.ts` / `pluginCore.ts`, re-derived from the
//!    case's own inputs. It runs before anything is compared with Rust, so "upstream moved" and
//!    "we regressed" cannot present identically.
//! 2. `the_artifact_plan_matches_upstream` replays the **pure** `plan_manifest_artifacts`,
//!    which was extracted out of `resolve_manifest_artifacts` for exactly this purpose — the
//!    same extraction `decide_update` and `calculate_pagination` already had. It needs no
//!    Docker, so the rules are covered by a plain `cargo test` on any machine.
//! 3. The remaining tests drive the real HTTP route against real MySQL and real MinIO, which is
//!    the only way to cover the I/O-shaped behaviour: presigning, degradation, and the exact
//!    response shape. Those skip without Docker.
//!
//! # How a recorded URL is compared with a presigned one
//!
//! A presigned URL is not reproducible across implementations. The generator therefore records
//! **storage URIs** (`s3://bucket/...`), inverted from an observed URI→URL log. This file does
//! the same inversion in the other direction: it parses the presigned URL this server returns,
//! strips the path-style `/{bucket}/` prefix, percent-decodes the rest and reassembles
//! `s3://bucket/<key>`. Nothing about the signature, expiry or endpoint is compared — only
//! *which object* each URL addresses.
//!
//! # Case isolation
//!
//! All cases share one database and one bucket. Each case gets its own `app_name` (the case
//! name), which isolates both `bundles` and `bundle_patches` — ids are unique per
//! `(app_name, id)`. Object keys are isolated by prefixing every storage URI with the case
//! name, which is undone before comparison.

mod common;

use common::{TestApp, TestBucket};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const FIXTURES: &str = include_str!("fixtures/artifacts_fixtures.json");

/// The placeholder bucket the generator recorded against.
const FIXTURE_BUCKET: &str = "bucket";
/// A bucket the test app is deliberately *not* configured for, so `resolve_key` in
/// `src/storage.rs` rejects it — the Rust counterpart of the generator's storage plugin
/// throwing on any URI outside `FIXTURE_BUCKET`.
const FOREIGN_BUCKET: &str = "other-bucket";

#[derive(Deserialize)]
struct Fixtures {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    name: String,
    description: String,
    dimensions: Value,
    request: FixtureRequest,
    bundles: Vec<FixtureBundle>,
    objects: BTreeMap<String, String>,
    /// Upstream produced an error instead of an answer (a 5xx), rather than a response.
    throws: Option<String>,
    /// `None` means upstream answered "no update" (the route body is JSON `null`).
    expected: Option<Value>,
    presigned_storage_uris: Vec<String>,
    read_storage_uris: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureRequest {
    platform: String,
    app_version: String,
    channel: String,
    min_bundle_id: String,
    bundle_id: String,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct FixtureBundle {
    id: String,
    platform: String,
    channel: String,
    enabled: bool,
    should_force_update: bool,
    file_hash: String,
    message: Option<String>,
    storage_uri: String,
    target_app_version: Option<String>,
    fingerprint_hash: Option<String>,
    rollout_cohort_count: i32,
    target_cohorts: Option<Vec<String>>,
    manifest_storage_uri: Option<String>,
    manifest_file_hash: Option<String>,
    asset_base_storage_uri: Option<String>,
    patches: Vec<FixturePatch>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct FixturePatch {
    base_bundle_id: String,
    base_file_hash: String,
    patch_file_hash: String,
    patch_storage_uri: String,
}

fn fixtures() -> Fixtures {
    serde_json::from_str(FIXTURES)
        .expect("tests/fixtures/artifacts_fixtures.json is not valid JSON")
}

// ---------------------------------------------------------------------------
// Storage URI ↔ real bucket translation
// ---------------------------------------------------------------------------

/// Map a recorded `s3://bucket/<rest>` URI onto the real test bucket, under a per-case prefix.
///
/// The prefix is what lets 93 cases share one bucket: several of them record the *same* URI
/// (`s3://bucket/<TARGET>/manifest.json`) with different contents. It is applied uniformly, so
/// the relative structure the layout rules depend on — whether the asset base path ends in
/// `/assets`, how many trailing slashes it has — is untouched.
///
/// URIs outside `FIXTURE_BUCKET` are returned verbatim: those are the deliberate
/// bucket-mismatch cases and must stay pointing at a bucket this app cannot read.
fn to_real_uri(uri: &str, case: &str, bucket: &str) -> String {
    if let Some(rest) = uri.strip_prefix(&format!("s3://{FIXTURE_BUCKET}/")) {
        format!("s3://{bucket}/{case}/{rest}")
    } else if uri == format!("s3://{FIXTURE_BUCKET}") {
        format!("s3://{bucket}/{case}")
    } else {
        uri.to_string()
    }
}

/// The exact inverse of [`to_real_uri`], so a URI this server produced can be compared with the
/// recorded one.
fn to_fixture_uri(uri: &str, case: &str, bucket: &str) -> String {
    if let Some(rest) = uri.strip_prefix(&format!("s3://{bucket}/{case}/")) {
        format!("s3://{FIXTURE_BUCKET}/{rest}")
    } else if uri == format!("s3://{bucket}/{case}") {
        format!("s3://{FIXTURE_BUCKET}")
    } else {
        uri.to_string()
    }
}

/// Decode `%XX` escapes, byte-wise, then interpret the result as UTF-8.
///
/// Written out rather than pulled from a crate because the decoding has to be *exactly* one
/// level: an object key may legitimately contain a literal `%20` (upstream's
/// `encodeURIComponent` puts one there), which the AWS SDK then encodes again as `%2520`. One
/// decode of the wire path gives the key back; two would silently turn it into a space and make
/// the encoding cases compare equal when they are not.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Turn a presigned URL this server returned back into the `s3://` URI it addresses.
///
/// The test apps are configured with an endpoint, so `src/storage.rs` builds path-style URLs:
/// `http://host:port/<bucket>/<key>?X-Amz-...`.
fn presigned_to_storage_uri(url: &str, bucket: &str) -> String {
    let parsed = url::Url::parse(url)
        .unwrap_or_else(|err| panic!("response URL {url:?} is not a URL at all: {err}"));
    let path = parsed.path();
    let prefix = format!("/{bucket}/");
    let encoded_key = path.strip_prefix(&prefix).unwrap_or_else(|| {
        panic!("presigned URL {url:?} is not path-style against bucket {bucket:?}")
    });
    format!("s3://{bucket}/{}", percent_decode(encoded_key))
}

/// Rewrite every URL position in a response body into its fixture-space storage URI.
fn normalize_response(body: &mut Value, case: &str, bucket: &str) {
    let rewrite = |value: &mut Value| {
        if let Some(url) = value.as_str() {
            let uri = presigned_to_storage_uri(url, bucket);
            *value = Value::String(to_fixture_uri(&uri, case, bucket));
        }
    };

    let Some(object) = body.as_object_mut() else {
        return;
    };
    if let Some(v) = object.get_mut("fileUrl") {
        rewrite(v);
    }
    if let Some(v) = object.get_mut("manifestUrl") {
        rewrite(v);
    }
    if let Some(Value::Object(assets)) = object.get_mut("changedAssets") {
        for asset in assets.values_mut() {
            let Some(asset) = asset.as_object_mut() else {
                continue;
            };
            if let Some(Value::Object(file)) = asset.get_mut("file") {
                if let Some(url) = file.get_mut("url") {
                    rewrite(url);
                }
            }
            if let Some(Value::Object(patch)) = asset.get_mut("patch") {
                if let Some(url) = patch.get_mut("patchUrl") {
                    rewrite(url);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 1 — the case must satisfy the documented upstream rules
//
// This runs BEFORE anything is compared with Rust, so "upstream moved" and "we regressed"
// cannot present identically. Every rule here is one stated in updateArtifacts.ts /
// pluginCore.ts, re-derived from the case's own inputs.
// ---------------------------------------------------------------------------

/// `BR_COMPRESSED_ASSET_PATH_RE = /(^|\/)index\.[^/]+\.bundle$/`, written out.
///
/// Note what it does NOT do: it never touches backslashes, and it is case-sensitive.
fn upstream_is_brotli_asset(asset_path: &str) -> bool {
    let Some(name) = asset_path.rsplit('/').next() else {
        return false;
    };
    // `(^|/)` means the match must begin at the start of a slash-delimited component.
    let Some(after_index) = name.strip_prefix("index.") else {
        return false;
    };
    let Some(middle) = after_index.strip_suffix(".bundle") else {
        return false;
    };
    // `[^/]+` needs at least one character, and cannot contain a separator.
    !middle.is_empty() && !middle.contains('/')
}

/// `getContentAddressedAssetStoragePath`, written out: the extension is `.br` when the path
/// already ends in `.br`, else the text after the final `.`, else nothing.
fn upstream_content_addressed_extension(asset_path: &str) -> String {
    if asset_path.ends_with(".br") {
        return ".br".to_string();
    }
    match asset_path.rsplit_once('.') {
        Some((_, ext)) => format!(".{ext}"),
        None => String::new(),
    }
}

/// JavaScript's `encodeURIComponent`: everything except the unreserved set
/// `A-Z a-z 0-9 - _ . ! ~ * ' ( )` is percent-encoded, byte-wise over UTF-8.
///
/// `createStorageUriWithRelativePath` applies this to each path segment, which is why the
/// storage key for an asset called `a+b.png` contains `%2B` and not `+`.
fn encode_uri_component(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte);
        if unreserved {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `manifest.assets` as a path -> asset map, accepting the array spelling upstream also takes.
fn manifest_assets(manifest: &Value) -> std::collections::BTreeMap<String, Value> {
    match &manifest["assets"] {
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(index, asset)| (index.to_string(), asset.clone()))
            .collect(),
        _ => std::collections::BTreeMap::new(),
    }
}

fn check_upstream_rules(case: &Case) {
    let name = &case.name;
    if case.throws.is_some() {
        assert!(
            case.expected.is_none(),
            "{name}: a case cannot both throw and answer",
        );
        return;
    }
    // `expected: null` with no `throws` is upstream's "no update" — a legitimate outcome with
    // nothing further to check.
    let Some(expected) = case.expected.as_ref() else {
        return;
    };

    let status = expected["status"].as_str().unwrap_or_else(|| {
        panic!("{name}: recorded response has no status: {expected}");
    });
    assert!(
        matches!(status, "UPDATE" | "ROLLBACK"),
        "{name}: getAppUpdateInfo only ever answers UPDATE or ROLLBACK, got {status:?}",
    );

    // pluginCore.ts makeResponse: `shouldForceUpdate: status === "ROLLBACK" ? true : ...`
    if status == "ROLLBACK" {
        assert_eq!(
            expected["shouldForceUpdate"],
            json!(true),
            "{name}: a ROLLBACK is always forced upstream",
        );
    }

    // §3.4 of docs/upstream-parity.md: a null fileUrl is the reset-to-built-in shape and
    // upstream pairs it only with NIL_UUID + ROLLBACK.
    if expected["fileUrl"].is_null() {
        assert_eq!(
            expected["id"], json!(common::NIL_UUID),
            "{name}: a null fileUrl outside the NIL rollback would tell the device to wipe its bundles",
        );
        assert_eq!(expected["status"], json!("ROLLBACK"), "{name}: ditto");
    }

    // resolveManifestArtifacts returns `Pick<..., "changedAssets" | "manifestFileHash" |
    // "manifestUrl"> | null` — the three arrive together or not at all.
    let has_manifest_url = expected.get("manifestUrl").is_some();
    let has_manifest_hash = expected.get("manifestFileHash").is_some();
    let has_changed_assets = expected.get("changedAssets").is_some();
    assert_eq!(
        (has_manifest_url, has_manifest_hash),
        (has_changed_assets, has_changed_assets),
        "{name}: the three manifest artifacts must be all-or-nothing, got \
         manifestUrl={has_manifest_url} manifestFileHash={has_manifest_hash} \
         changedAssets={has_changed_assets}",
    );

    let Some(Value::Object(changed)) = expected.get("changedAssets") else {
        return;
    };

    let target = case
        .bundles
        .iter()
        .find(|b| Some(b.id.as_str()) == expected["id"].as_str())
        .unwrap_or_else(|| panic!("{name}: the answered bundle is not in the case's bundle set"));
    let asset_base = target
        .asset_base_storage_uri
        .as_deref()
        .unwrap_or_else(|| panic!("{name}: artifacts were returned without an asset base"));
    let manifest_uri = target.manifest_storage_uri.as_deref().unwrap();
    let target_manifest: Value = serde_json::from_str(&case.objects[manifest_uri])
        .unwrap_or_else(|err| panic!("{name}: the target manifest is not JSON: {err}"));
    // `isBundleManifest` accepts an ARRAY for `assets` (it only excludes arrays at the top
    // level and per asset), in which case the array index is the asset path — see E19/E20.
    let target_assets = manifest_assets(&target_manifest);

    let current_manifest: Option<Value> = case
        .bundles
        .iter()
        .find(|b| b.id == case.request.bundle_id)
        .and_then(|b| b.manifest_storage_uri.as_deref())
        .and_then(|uri| case.objects.get(uri))
        .and_then(|text| serde_json::from_str(text).ok());

    let mut patched_assets = 0;
    for (asset_path, asset) in changed {
        // resolveChangedAssets skips an asset whose hash is unchanged.
        let file_hash = asset["fileHash"].as_str().unwrap();
        assert_eq!(
            Some(file_hash),
            target_assets[asset_path]["fileHash"].as_str(),
            "{name}: changedAssets[{asset_path:?}] reports a hash the target manifest does not have",
        );
        if let Some(current) = current_manifest.as_ref() {
            let current_assets = manifest_assets(current);
            let previous = current_assets
                .get(asset_path)
                .and_then(|asset| asset["fileHash"].as_str());
            assert_ne!(
                previous,
                Some(file_hash),
                "{name}: {asset_path:?} is in changedAssets but its hash is unchanged",
            );
        }

        let is_brotli = upstream_is_brotli_asset(asset_path);
        if let Some(file) = asset.get("file") {
            assert_eq!(
                file.get("compression").and_then(Value::as_str) == Some("br"),
                is_brotli,
                "{name}: {asset_path:?} compression disagrees with the brotli path rule",
            );

            // The content-addressed layout applies exactly when the base path ends in /assets.
            let base_path = url::Url::parse(asset_base).unwrap().path().to_string();
            let base_path = base_path.trim_end_matches('/');
            if base_path.ends_with("/assets") || base_path == "/assets" {
                let stored_path = if is_brotli {
                    format!("{asset_path}.br")
                } else {
                    asset_path.clone()
                };
                let shard: String = file_hash.chars().take(2).collect();
                // `createStorageUriWithRelativePath` drops EMPTY segments (`.filter(Boolean)`),
                // so a hash shorter than two characters collapses the shard rather than
                // producing `sha256//<hash>`.
                let expected_tail = [
                    "sha256".to_string(),
                    shard,
                    format!(
                        "{file_hash}{}",
                        upstream_content_addressed_extension(&stored_path)
                    ),
                ]
                .into_iter()
                .filter(|segment| !segment.is_empty())
                .map(|segment| encode_uri_component(&segment))
                .collect::<Vec<_>>()
                .join("/");
                let url = file["url"].as_str().unwrap();
                assert!(
                    url.ends_with(&expected_tail),
                    "{name}: {asset_path:?} resolved to {url}, which does not end in the \
                     content-addressed path {expected_tail}",
                );
            }
        }

        if let Some(patch) = asset.get("patch") {
            patched_assets += 1;
            assert_eq!(
                patch["algorithm"],
                json!("bsdiff"),
                "{name}: the only patch algorithm upstream emits is bsdiff",
            );
            assert!(
                asset_path.ends_with(".bundle"),
                "{name}: a patch landed on {asset_path:?}, which is not the HBC asset",
            );
            // resolveHbcPatchDescriptor requires all three fields to be non-empty.
            for field in ["baseBundleId", "baseFileHash", "patchFileHash", "patchUrl"] {
                assert!(
                    patch[field].as_str().is_some_and(|v| !v.is_empty()),
                    "{name}: patch.{field} is empty, which upstream treats as absent",
                );
            }
        }
    }
    assert!(
        patched_assets <= 1,
        "{name}: resolveHbcPatchDescriptor returns at most one descriptor, got {patched_assets}",
    );

    // Every URI that made it into the response must be one the generator watched upstream ask
    // for, and the manifest it diffed against must be one it watched upstream read. This is
    // what keeps the recorded URI logs from being decoration.
    let mut response_uris = vec![expected["manifestUrl"].as_str().unwrap().to_string()];
    if let Some(url) = expected["fileUrl"].as_str() {
        response_uris.push(url.to_string());
    }
    for asset in changed.values() {
        if let Some(url) = asset.pointer("/file/url").and_then(Value::as_str) {
            response_uris.push(url.to_string());
        }
        if let Some(url) = asset.pointer("/patch/patchUrl").and_then(Value::as_str) {
            response_uris.push(url.to_string());
        }
    }
    for uri in response_uris {
        assert!(
            case.presigned_storage_uris.contains(&uri),
            "{name}: {uri} appears in the response but was never presigned upstream",
        );
    }
    assert!(
        case.read_storage_uris.contains(&manifest_uri.to_string()),
        "{name}: artifacts were returned without upstream reading {manifest_uri}",
    );
}

#[test]
fn every_recorded_case_satisfies_the_documented_upstream_rules() {
    let fixtures = fixtures();
    assert!(
        fixtures.cases.len() >= 90,
        "the artifacts fixture set shrank; cases must not be dropped to get green",
    );
    let mut names: Vec<&str> = fixtures.cases.iter().map(|c| c.name.as_str()).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(
        before,
        names.len(),
        "duplicate case names in the fixture file"
    );

    for case in &fixtures.cases {
        assert!(
            !case.description.is_empty() && case.dimensions.is_object(),
            "{}: every case must carry a description and its dimensions",
            case.name
        );
        check_upstream_rules(case);
    }
}

// ---------------------------------------------------------------------------
// Stage 2a — replay the PURE plan, with no Docker
//
// `plan_manifest_artifacts` was extracted out of `resolve_manifest_artifacts` exactly so this
// could exist: it holds every rule (which asset changed, which storage URI it resolves to, the
// brotli suffix, the bsdiff patch selection, the falsy-column checks) and none of the I/O. This
// runs on any machine, which is what makes the surface genuinely covered by `cargo test`.
// ---------------------------------------------------------------------------

use rn_ota_server_rust::models::BundlePatch;
use rn_ota_server_rust::routes::check::{plan_manifest_artifacts, Manifest};

/// Cases the pure plan cannot express, because what distinguishes them happens during I/O
/// rather than during planning. The list is asserted to be exactly right, so it cannot quietly
/// absorb a real deviation.
///
/// * `F01`/`F02`/`F04`/`F05` — a bucket the app cannot read. The plan has no notion of buckets;
///   it happily produces the URI whose *presign* is what fails.
/// * `F03` — the asset presign fails and a patch covers the asset, so upstream emits the asset
///   with no `file` key. The plan always produces a storage URI; dropping it is the caller's
///   step.
/// * `D14` — likewise for the patch URL.
const NOT_EXPRESSIBLE_IN_THE_PURE_PLAN: [&str; 6] = ["D14", "F01", "F02", "F03", "F04", "F05"];

fn parse_manifest(case: &Case, storage_uri: Option<&str>) -> Option<Manifest> {
    let text = case
        .objects
        .get(storage_uri.filter(|uri| !uri.is_empty())?)?;
    serde_json::from_str::<Manifest>(text).ok()
}

fn to_bundle_patches(bundle: &FixtureBundle) -> Vec<BundlePatch> {
    bundle
        .patches
        .iter()
        .enumerate()
        .map(|(index, patch)| BundlePatch {
            id: format!("{}:{}:{index}", bundle.id, patch.base_bundle_id),
            bundle_id: bundle.id.clone(),
            base_bundle_id: patch.base_bundle_id.clone(),
            base_file_hash: patch.base_file_hash.clone(),
            patch_file_hash: patch.patch_file_hash.clone(),
            patch_storage_uri: patch.patch_storage_uri.clone(),
            order_index: index as i32,
            ctime: Default::default(),
        })
        .collect()
}

/// What upstream said, reduced to the shape `plan_manifest_artifacts` produces.
type PlanShape = (
    String,
    Vec<(String, String, Option<String>, Option<String>)>,
    Option<(String, String, String, String, String)>,
);

fn expected_plan_shape(expected: &Value) -> Option<PlanShape> {
    let changed = expected.get("changedAssets")?.as_object()?;
    let mut assets: Vec<_> = changed
        .iter()
        .map(|(path, asset)| {
            (
                path.clone(),
                asset["fileHash"].as_str().unwrap().to_string(),
                asset
                    .pointer("/file/url")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                asset
                    .pointer("/file/compression")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            )
        })
        .collect();
    assets.sort();
    let patch = changed.iter().find_map(|(path, asset)| {
        let patch = asset.get("patch")?;
        Some((
            path.clone(),
            patch["baseBundleId"].as_str().unwrap().to_string(),
            patch["baseFileHash"].as_str().unwrap().to_string(),
            patch["patchFileHash"].as_str().unwrap().to_string(),
            patch["patchUrl"].as_str().unwrap().to_string(),
        ))
    });
    Some((
        expected["manifestFileHash"].as_str().unwrap().to_string(),
        assets,
        patch,
    ))
}

#[test]
fn the_artifact_plan_matches_upstream() {
    let fixtures = fixtures();
    let mut deviations = Vec::new();
    let mut compared = 0;
    let mut skipped = Vec::new();

    for case in &fixtures.cases {
        if NOT_EXPRESSIBLE_IN_THE_PURE_PLAN.contains(&case.name.as_str()) {
            skipped.push(case.name.as_str());
            continue;
        }
        if case.throws.is_some() {
            continue;
        }
        let Some(expected) = case.expected.as_ref() else {
            continue;
        };
        let Some(target) = case
            .bundles
            .iter()
            .find(|b| Some(b.id.as_str()) == expected["id"].as_str())
        else {
            continue; // the NIL rollback carries no bundle
        };

        let target_manifest = parse_manifest(case, target.manifest_storage_uri.as_deref());
        let upstream_has_artifacts = expected.get("changedAssets").is_some();

        // The manifest-validity rule (`isBundleManifest`) is observable on its own: upstream
        // returning no artifacts because it rejected the document must mean our parse rejects
        // it too, and vice versa.
        //
        // Only meaningful when the object exists AND the three manifest columns are all
        // truthy — otherwise upstream never looked at the document at all and "no artifacts"
        // says nothing about whether it would have accepted it (cases E01-E04, E21, E22).
        let object_exists = target
            .manifest_storage_uri
            .as_deref()
            .filter(|uri| !uri.is_empty())
            .is_some_and(|uri| case.objects.contains_key(uri));
        let columns_truthy = target
            .manifest_file_hash
            .as_deref()
            .is_some_and(|value| !value.is_empty())
            && target
                .asset_base_storage_uri
                .as_deref()
                .is_some_and(|value| !value.is_empty());
        if object_exists && columns_truthy && upstream_has_artifacts != target_manifest.is_some() {
            deviations.push(format!(
                "  {} ({}): upstream {} the target manifest, we {} it",
                case.name,
                case.description,
                if upstream_has_artifacts {
                    "accepted"
                } else {
                    "rejected"
                },
                if target_manifest.is_some() {
                    "accepted"
                } else {
                    "rejected"
                },
            ));
            continue;
        }

        let Some(target_manifest) = target_manifest else {
            continue;
        };
        let current = case.bundles.iter().find(|b| b.id == case.request.bundle_id);
        let current_manifest =
            current.and_then(|b| parse_manifest(case, b.manifest_storage_uri.as_deref()));

        let plan = plan_manifest_artifacts(
            target.manifest_file_hash.as_deref(),
            target.asset_base_storage_uri.as_deref(),
            &target_manifest,
            current_manifest.as_ref(),
            current.map(|b| b.id.as_str()),
            &to_bundle_patches(target),
        );

        compared += 1;

        // A descriptor whose asset is NOT in the changed set is real but invisible in the
        // response: upstream computes it, presigns it (case D13's recorded presign log shows
        // `patch.bsdiff`) and then never attaches it, because `resolveChangedAssets` only
        // consults it for assets it is already emitting. So the response comparison uses the
        // ATTACHED descriptor, and the unattached one is checked against the presign log —
        // both observed, neither inferred.
        if let Some(patch) = plan.as_ref().and_then(|plan| plan.patch.as_ref()) {
            assert!(
                case.presigned_storage_uris
                    .contains(&patch.patch_storage_uri),
                "{}: we selected the bsdiff patch at {} but upstream never presigned it",
                case.name,
                patch.patch_storage_uri,
            );
        }

        let observed: Option<PlanShape> = plan.map(|plan| {
            let mut assets: Vec<_> = plan
                .assets
                .iter()
                .map(|asset| {
                    (
                        asset.asset_path.clone(),
                        asset.file_hash.clone(),
                        Some(asset.storage_uri.clone()),
                        asset.compression.clone(),
                    )
                })
                .collect();
            assets.sort();
            let attached = plan
                .patch
                .filter(|patch| {
                    plan.assets
                        .iter()
                        .any(|asset| asset.asset_path == patch.asset_path)
                })
                .map(|patch| {
                    (
                        patch.asset_path,
                        patch.base_bundle_id,
                        patch.base_file_hash,
                        patch.patch_file_hash,
                        patch.patch_storage_uri,
                    )
                });
            (plan.manifest_file_hash.clone(), assets, attached)
        });
        let expected_shape = expected_plan_shape(expected);

        if observed != expected_shape {
            deviations.push(format!(
                "  {} ({})\n      upstream: {:?}\n      ours:     {:?}",
                case.name, case.description, expected_shape, observed
            ));
        }
    }

    assert_eq!(
        skipped, NOT_EXPRESSIBLE_IN_THE_PURE_PLAN,
        "the pure-plan exemption list must match the cases it names exactly, so a new \
         deviation cannot hide behind it",
    );
    assert!(
        deviations.is_empty(),
        "{} of {compared} cases deviate from the recorded upstream plan:\n{}",
        deviations.len(),
        deviations.join("\n"),
    );
    // The cases that never reach the plan are the ones with no answer (G04), no bundle (G05),
    // an upstream throw, or a manifest upstream itself rejected — those are checked by the
    // accept/reject comparison above instead. Everything else must have been planned.
    assert!(
        compared >= 75,
        "only {compared} cases reached the pure plan; the test is not exercising it",
    );
}

// ---------------------------------------------------------------------------
// Stage 2b — replay the whole route against MySQL + MinIO
// ---------------------------------------------------------------------------

async fn seed_case(app: &TestApp, bucket: &TestBucket, case: &Case) {
    let name = &case.name;
    for bundle in &case.bundles {
        sqlx::query(
            r#"INSERT INTO bundles (
                   id, app_name, platform, should_force_update, enabled, file_hash,
                   git_commit_hash, message, channel, storage_uri, target_app_version,
                   fingerprint_hash, metadata, rollout_cohort_count, target_cohorts,
                   manifest_storage_uri, manifest_file_hash, asset_base_storage_uri
               ) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&bundle.id)
        .bind(name)
        .bind(&bundle.platform)
        .bind(i8::from(bundle.should_force_update))
        .bind(i8::from(bundle.enabled))
        .bind(&bundle.file_hash)
        .bind(&bundle.message)
        .bind(&bundle.channel)
        .bind(to_real_uri(&bundle.storage_uri, name, &bucket.name))
        .bind(&bundle.target_app_version)
        .bind(&bundle.fingerprint_hash)
        .bind(json!({}))
        .bind(bundle.rollout_cohort_count)
        .bind(
            bundle
                .target_cohorts
                .as_ref()
                .map(|c| serde_json::to_value(c).unwrap()),
        )
        .bind(
            bundle
                .manifest_storage_uri
                .as_deref()
                .map(|u| to_real_uri(u, name, &bucket.name)),
        )
        .bind(&bundle.manifest_file_hash)
        .bind(
            bundle
                .asset_base_storage_uri
                .as_deref()
                .map(|u| to_real_uri(u, name, &bucket.name)),
        )
        .execute(&app.pool)
        .await
        .unwrap_or_else(|err| panic!("{name}: failed to seed bundle {}: {err}", bundle.id));
    }

    for bundle in &case.bundles {
        for (index, patch) in bundle.patches.iter().enumerate() {
            // `api.rs` derives the id as "{bundle_id}:{base_bundle_id}". The index is appended
            // here only so that case D11 — two records naming the SAME base, which is what
            // upstream's `getBundlePatches` dedupe rule is about — can be seeded at all. The
            // column is a free-form VARCHAR; nothing enforces the convention.
            let id = format!("{}:{}:{index}", bundle.id, patch.base_bundle_id);
            sqlx::query(
                r#"INSERT INTO bundle_patches (
                       id, app_name, bundle_id, base_bundle_id, base_file_hash,
                       patch_file_hash, patch_storage_uri, order_index
                   ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(&id)
            .bind(name)
            .bind(&bundle.id)
            .bind(&patch.base_bundle_id)
            .bind(&patch.base_file_hash)
            .bind(&patch.patch_file_hash)
            .bind(to_real_uri(&patch.patch_storage_uri, name, &bucket.name))
            .bind(index as i32)
            .execute(&app.pool)
            .await
            .unwrap_or_else(|err| panic!("{name}: failed to seed patch {id}: {err}"));
        }
    }

    for (uri, body) in &case.objects {
        let real = to_real_uri(uri, name, &bucket.name);
        let key = real
            .strip_prefix(&format!("s3://{}/", bucket.name))
            .unwrap_or_else(|| panic!("{name}: object {uri} is not in the test bucket"));
        bucket.put(key, body.as_bytes()).await;
    }
}

/// Marker status for "the request handler panicked", which is not an HTTP outcome at all.
const PANICKED: u16 = 0;

/// One `GET .../app-version/...` per case, with the response translated back into fixture space.
///
/// No `Hot-Updater-SDK-Version` header is sent, so "no update" comes back as JSON `null` —
/// the same shape the fixture records for it.
///
/// The request runs on its own task so that a **panic inside the handler** is caught and
/// reported as a deviation rather than taking the whole replay down. That is not defensive
/// padding: case B12 (a manifest asset whose `fileHash` is one character long) really does
/// panic `src/routes/check.rs`, and without this the other 92 cases would never be compared.
async fn run_case(app: &TestApp, bucket: &TestBucket, case: &Case) -> (u16, Value) {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let uri = format!(
        "/{}/hot-updater/app-version/{}/{}/{}/{}/{}",
        case.name,
        case.request.platform,
        case.request.app_version,
        case.request.channel,
        case.request.min_bundle_id,
        case.request.bundle_id,
    );
    let router = app.router.clone();
    let request = Request::builder()
        .method("GET")
        .uri(&uri)
        .body(Body::empty())
        .unwrap();

    let joined = tokio::spawn(async move {
        let response = router.oneshot(request).await.expect("router call failed");
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("failed to read response body")
            .to_bytes()
            .to_vec();
        (status, body)
    })
    .await;

    let (status, body) = match joined {
        Ok(result) => result,
        Err(err) if err.is_panic() => {
            let payload = err.into_panic();
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            return (PANICKED, Value::String(message));
        }
        Err(err) => panic!("{}: the request task failed: {err}", case.name),
    };

    if status != 200 {
        return (
            status,
            Value::String(String::from_utf8_lossy(&body).into_owned()),
        );
    }
    let mut parsed: Value = serde_json::from_slice(&body).unwrap_or_else(|err| {
        panic!(
            "{}: response body was not JSON ({err}): {}",
            case.name,
            String::from_utf8_lossy(&body)
        )
    });
    normalize_response(&mut parsed, &case.name, &bucket.name);
    (status, parsed)
}

struct Replay {
    case_name: String,
    description: String,
    status: u16,
    observed: Value,
    expected_body: Value,
    /// The recorded upstream outcome as a compact label, for failure messages.
    upstream: String,
}

async fn replay_all() -> Option<Vec<Replay>> {
    let fixtures = fixtures();
    let bucket = TestBucket::create().await?;
    let apps: Vec<(&str, _)> = fixtures
        .cases
        .iter()
        .map(|case| (case.name.as_str(), bucket.storage_config()))
        .collect();
    let app = TestApp::spawn_with_storage(&apps).await?;

    let mut out = Vec::with_capacity(fixtures.cases.len());
    for case in &fixtures.cases {
        seed_case(&app, &bucket, case).await;
        let (status, observed) = run_case(&app, &bucket, case).await;

        let expected_body = match (&case.throws, &case.expected) {
            // Upstream threw: `getAppUpdateInfo` rejects and the host framework answers 5xx.
            (Some(_), _) => Value::Null,
            (None, Some(expected)) => expected.clone(),
            (None, None) => Value::Null,
        };
        let upstream = match (&case.throws, &case.expected) {
            (Some(message), _) => format!("THROWS ({message})"),
            (None, Some(_)) => "answers".to_string(),
            (None, None) => "no update (null body)".to_string(),
        };

        out.push(Replay {
            case_name: case.name.clone(),
            description: case.description.clone(),
            status,
            observed,
            expected_body,
            upstream,
        });
    }
    Some(out)
}

/// Cases where this server answers differently from upstream on *whether the check fails*.
///
/// It is empty, and that is the point. It held `["D14", "F01", "F02", "F04", "F05"]` while this
/// server degraded on a storage failure where upstream propagates; those five now propagate too.
/// The assertion below compares the deviating set against this list for EQUALITY, so an empty
/// list is a live guard that it stays empty — a new divergence fails, and nothing can be added
/// here without someone deciding to.
const KNOWN_DEGRADE_INSTEAD_OF_5XX: [&str; 0] = [];

/// The status code contract: upstream throwing means a 5xx, upstream answering means a body.
#[tokio::test]
async fn artifact_failures_that_upstream_turns_into_a_5xx_are_answered_the_same_way() {
    let Some(replays) = replay_all().await else {
        return;
    };
    let fixtures = fixtures();

    let mut deviations = Vec::new();
    let mut deviating_cases = Vec::new();
    for (case, replay) in fixtures.cases.iter().zip(&replays) {
        // A panic is not an HTTP outcome: under hyper it aborts the connection task, so the
        // device sees a transport error rather than a status it can reason about. It never
        // matches upstream, whatever upstream did.
        let matches = match (case.throws.is_some(), replay.status) {
            (_, PANICKED) => false,
            (true, status) => status >= 500,
            (false, status) => status == 200,
        };
        if !matches {
            let ours = if replay.status == PANICKED {
                format!(
                    "PANICKED ({})",
                    replay.observed.as_str().unwrap_or_default()
                )
            } else {
                format!(
                    "HTTP {} {}",
                    replay.status,
                    serde_json::to_string(&replay.observed).unwrap()
                )
            };
            deviating_cases.push(replay.case_name.as_str());
            if !KNOWN_DEGRADE_INSTEAD_OF_5XX.contains(&replay.case_name.as_str()) {
                deviations.push(format!(
                    "  {} ({})\n      upstream: {}\n      ours:     {ours}",
                    replay.case_name, replay.description, replay.upstream,
                ));
            }
        }
    }

    // Neither wider nor narrower than the documented set: a case that stops deviating must be
    // struck off the list rather than left to soak up a future regression.
    assert_eq!(
        deviating_cases, KNOWN_DEGRADE_INSTEAD_OF_5XX,
        "the set of cases that disagree with upstream on whether the check fails must stay \
         exactly as documented (currently: empty)",
    );
    assert!(
        deviations.is_empty(),
        "{} of {} cases disagree with upstream on whether the check fails:\n{}",
        deviations.len(),
        replays.len(),
        deviations.join("\n"),
    );
}

/// The values: which assets are changed, which storage URI each resolves to, whether a patch is
/// emitted and what it says.
#[tokio::test]
async fn artifact_response_values_match_upstream() {
    let Some(replays) = replay_all().await else {
        return;
    };
    let fixtures = fixtures();

    let mut deviations = Vec::new();
    for (case, replay) in fixtures.cases.iter().zip(&replays) {
        // Cases where upstream produced no answer at all are the other test's subject.
        if case.throws.is_some() || replay.status != 200 {
            continue;
        }
        let expected = replay.expected_body.clone();
        let observed = replay.observed.clone();
        if expected != observed {
            deviations.push(format!(
                "  {} ({})\n      upstream: {}\n      ours:     {}",
                replay.case_name,
                replay.description,
                serde_json::to_string(&expected).unwrap(),
                serde_json::to_string(&observed).unwrap(),
            ));
        }
    }

    assert!(
        deviations.is_empty(),
        "{} of {} cases deviate from the recorded upstream response:\n{}",
        deviations.len(),
        replays.len(),
        deviations.join("\n"),
    );
}

/// The shape: upstream *omits* `manifestUrl` / `manifestFileHash` / `changedAssets` when there
/// are no artifacts, and omits an asset's `file` when it could not be presigned. This server
/// serialises them as explicit nulls instead.
///
/// Kept as its own test because the difference is systemic and would otherwise mask every
/// value-level comparison above.
#[tokio::test]
async fn optional_artifact_keys_are_omitted_exactly_as_upstream_omits_them() {
    let Some(replays) = replay_all().await else {
        return;
    };
    let fixtures = fixtures();

    let key_set = |value: &Value| -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        if let Some(Value::Object(assets)) = value.get("changedAssets") {
            for (path, asset) in assets {
                if let Some(asset) = asset.as_object() {
                    for key in asset.keys() {
                        keys.push(format!("changedAssets[{path}].{key}"));
                    }
                }
            }
        }
        keys.sort();
        keys
    };

    let mut deviations = Vec::new();
    for (case, replay) in fixtures.cases.iter().zip(&replays) {
        if case.throws.is_some() || replay.status != 200 || replay.expected_body.is_null() {
            continue;
        }
        let expected = key_set(&replay.expected_body);
        let observed = key_set(&replay.observed);
        if expected != observed {
            deviations.push(format!(
                "  {}: upstream {:?} vs ours {:?}",
                replay.case_name, expected, observed
            ));
        }
    }

    assert!(
        deviations.is_empty(),
        "{} of {} cases carry keys upstream omits (or omit keys upstream carries):\n{}",
        deviations.len(),
        replays.len(),
        deviations.join("\n"),
    );
}

/// A guard on the harness itself: if the bucket translation or the URL inversion silently
/// stopped working, every comparison above would still "pass" on cases with no artifacts. This
/// asserts that the replay really did produce presigned URLs pointing into the test bucket.
#[tokio::test]
async fn the_replay_really_resolves_storage_uris() {
    let Some(replays) = replay_all().await else {
        return;
    };

    let with_assets = replays
        .iter()
        .filter(|r| {
            r.observed
                .get("changedAssets")
                .and_then(Value::as_object)
                .is_some_and(|assets| {
                    assets
                        .values()
                        .any(|a| a.get("file").is_some_and(|f| !f.is_null()))
                })
        })
        .count();
    assert!(
        with_assets >= 40,
        "only {with_assets} replayed cases produced a presigned asset URL; the harness is not \
         exercising the asset path"
    );

    let with_patch = replays
        .iter()
        .filter(|r| {
            r.observed
                .get("changedAssets")
                .and_then(Value::as_object)
                .is_some_and(|assets| {
                    assets
                        .values()
                        .any(|a| a.get("patch").is_some_and(|p| !p.is_null()))
                })
        })
        .count();
    assert!(
        with_patch >= 3,
        "only {with_patch} replayed cases produced a bsdiff patch descriptor"
    );
}

/// The foreign-bucket cases must really be foreign: if the test bucket were ever named
/// `other-bucket`, every failure case would silently turn into a success case.
#[test]
fn the_foreign_bucket_is_never_the_configured_one() {
    let fixtures = fixtures();
    let foreign_cases = fixtures
        .cases
        .iter()
        .filter(|c| {
            c.bundles.iter().any(|b| {
                [
                    Some(b.storage_uri.as_str()),
                    b.manifest_storage_uri.as_deref(),
                    b.asset_base_storage_uri.as_deref(),
                ]
                .iter()
                .flatten()
                .any(|u| u.starts_with(&format!("s3://{FOREIGN_BUCKET}/")))
                    || b.patches.iter().any(|p| {
                        p.patch_storage_uri
                            .starts_with(&format!("s3://{FOREIGN_BUCKET}/"))
                    })
            })
        })
        .count();
    assert!(
        foreign_cases >= 5,
        "expected the fixture set to exercise the bucket-mismatch failure path, found \
         {foreign_cases} cases"
    );
}

// ---------------------------------------------------------------------------
// Regression locks for the specific defects this fixture set uncovered.
//
// The replays above would catch all of these, but only as one line in a list of 101. These name
// them, so a reintroduction says what broke. Each takes its expectation from the fixture rather
// than hard-coding it: ground truth stays in one place.
// ---------------------------------------------------------------------------

/// Look up the single asset a one-asset fixture case resolved to, straight out of the recording.
fn recorded_single_asset(case_name: &str) -> (String, String, String, Option<String>) {
    let fixtures = fixtures();
    let case = fixtures
        .cases
        .iter()
        .find(|c| c.name == case_name)
        .unwrap_or_else(|| panic!("fixture case {case_name} is gone"));
    let expected = case.expected.as_ref().expect("case has no recorded answer");
    let changed = expected["changedAssets"].as_object().unwrap();
    assert_eq!(changed.len(), 1, "{case_name} is not a one-asset case");
    let (path, asset) = changed.iter().next().unwrap();
    (
        path.clone(),
        asset["fileHash"].as_str().unwrap().to_string(),
        asset["file"]["url"].as_str().unwrap().to_string(),
        asset
            .pointer("/file/compression")
            .and_then(Value::as_str)
            .map(str::to_string),
    )
}

/// Replay one one-asset case through the pure plan.
fn plan_single_asset(case_name: &str) -> (String, Option<String>) {
    let fixtures = fixtures();
    let case = fixtures
        .cases
        .iter()
        .find(|c| c.name == case_name)
        .unwrap_or_else(|| panic!("fixture case {case_name} is gone"));
    let expected = case.expected.as_ref().unwrap();
    let target = case
        .bundles
        .iter()
        .find(|b| Some(b.id.as_str()) == expected["id"].as_str())
        .unwrap();
    let manifest =
        parse_manifest(case, target.manifest_storage_uri.as_deref()).expect("manifest rejected");
    let plan = plan_manifest_artifacts(
        target.manifest_file_hash.as_deref(),
        target.asset_base_storage_uri.as_deref(),
        &manifest,
        None,
        None,
        &[],
    )
    .expect("no plan");
    assert_eq!(plan.assets.len(), 1, "{case_name} is not a one-asset case");
    let asset = &plan.assets[0];
    (asset.storage_uri.clone(), asset.compression.clone())
}

/// A manifest asset whose `fileHash` was shorter than two bytes used to panic
/// `src/routes/check.rs` on `&file_hash[0..2]`, taking the whole update-check request down —
/// under hyper that aborts the connection rather than returning a status. JavaScript's
/// `slice(0, 2)` truncates instead, so upstream answers normally.
#[test]
fn bug_a_manifest_asset_hash_shorter_than_two_characters_panicked_the_request() {
    for case_name in ["B12", "B13", "B14", "B15"] {
        let (_, _, expected_url, _) = recorded_single_asset(case_name);
        let (observed_url, _) = plan_single_asset(case_name);
        assert_eq!(
            observed_url, expected_url,
            "{case_name}: short/non-ASCII hash resolved to the wrong content-addressed path",
        );
    }
}

/// `uses_brotli_asset` used to normalise backslashes to slashes before matching, so
/// `build\index.abc.bundle` was reported as brotli-compressed. Upstream's regex has no such
/// rule, and the difference is not cosmetic: the device would have been pointed at a `.br`
/// object the CLI never uploaded.
#[test]
fn bug_a_backslash_before_index_was_treated_as_a_brotli_bundle() {
    let (path, _, expected_url, expected_compression) = recorded_single_asset("C09");
    assert!(path.contains('\\'), "C09 no longer carries a backslash");
    assert_eq!(
        expected_compression, None,
        "upstream says C09 is not brotli"
    );

    let (observed_url, observed_compression) = plan_single_asset("C09");
    assert_eq!(observed_compression, None);
    assert_eq!(observed_url, expected_url);
}

/// Storage keys are built with `encodeURIComponent` per path segment and empty segments are
/// dropped. Assigning the raw path to `url::Url::set_path` left `+`, `&` and `=` unescaped and
/// kept empty segments, addressing a key one or two characters away from the uploaded one.
#[test]
fn bug_asset_paths_were_not_encoded_the_way_encodeuricomponent_encodes_them() {
    for case_name in [
        "B22", "B23", "B24", "B25", "B26", "B27", "B28", "B29", "B30",
    ] {
        let (_, _, expected_url, _) = recorded_single_asset(case_name);
        let (observed_url, _) = plan_single_asset(case_name);
        assert_eq!(observed_url, expected_url, "{case_name}");
    }
}

/// Upstream guards `manifestFileHash`, `assetBaseStorageUri`, `patchStorageUri`,
/// `patchFileHash` and `baseFileHash` with a JS falsy test, so an empty string counts as
/// absent. Reading them as `Option` alone let `Some("")` through, which emitted a bsdiff
/// descriptor with an empty hash and a `manifestFileHash: ""` alongside a full asset set.
#[test]
fn bug_an_empty_string_column_was_treated_as_present_rather_than_absent() {
    let fixtures = fixtures();
    for case_name in ["D08", "D09", "D10", "E04", "E21", "E22"] {
        let case = fixtures
            .cases
            .iter()
            .find(|c| c.name == case_name)
            .unwrap_or_else(|| panic!("fixture case {case_name} is gone"));
        let expected = case.expected.as_ref().unwrap();
        let target = case
            .bundles
            .iter()
            .find(|b| Some(b.id.as_str()) == expected["id"].as_str())
            .unwrap();
        let manifest = parse_manifest(case, target.manifest_storage_uri.as_deref());
        let plan = manifest.as_ref().and_then(|manifest| {
            plan_manifest_artifacts(
                target.manifest_file_hash.as_deref(),
                target.asset_base_storage_uri.as_deref(),
                manifest,
                parse_manifest(
                    case,
                    case.bundles
                        .iter()
                        .find(|b| b.id == case.request.bundle_id)
                        .and_then(|b| b.manifest_storage_uri.as_deref()),
                )
                .as_ref(),
                Some(case.request.bundle_id.as_str()),
                &to_bundle_patches(target),
            )
        });

        // E04/E21/E22 drop the artifacts entirely; D08/D09/D10 keep them but drop the patch.
        match expected.get("changedAssets") {
            None => assert!(
                plan.is_none(),
                "{case_name}: artifacts should have been dropped"
            ),
            Some(_) => {
                let plan = plan.unwrap_or_else(|| panic!("{case_name}: artifacts were dropped"));
                assert!(
                    plan.patch.is_none(),
                    "{case_name}: an empty patch field must drop the whole descriptor",
                );
            }
        }
    }
}

/// `getBundlePatch` compares `baseBundleId` with `===`. This used to use
/// `eq_ignore_ascii_case`, which selected a patch upstream does not. D06 and D06b are the same
/// scenario differing only in the case of the recorded base id.
#[test]
fn bug_bundle_patch_base_ids_were_matched_case_insensitively() {
    let fixtures = fixtures();
    let patch_of = |case_name: &str| -> Option<String> {
        let case = fixtures.cases.iter().find(|c| c.name == case_name).unwrap();
        let expected = case.expected.as_ref().unwrap();
        let target = case
            .bundles
            .iter()
            .find(|b| Some(b.id.as_str()) == expected["id"].as_str())
            .unwrap();
        let manifest = parse_manifest(case, target.manifest_storage_uri.as_deref()).unwrap();
        plan_manifest_artifacts(
            target.manifest_file_hash.as_deref(),
            target.asset_base_storage_uri.as_deref(),
            &manifest,
            parse_manifest(
                case,
                case.bundles
                    .iter()
                    .find(|b| b.id == case.request.bundle_id)
                    .and_then(|b| b.manifest_storage_uri.as_deref()),
            )
            .as_ref(),
            Some(case.request.bundle_id.as_str()),
            &to_bundle_patches(target),
        )
        .and_then(|plan| plan.patch)
        .map(|patch| patch.base_bundle_id)
    };

    assert_eq!(
        patch_of("D06"),
        None,
        "an upper-case base id must not match"
    );
    assert!(
        patch_of("D06b").is_some(),
        "the same shape with matching case must still select the patch",
    );
}

/// `isBundleManifest` is stricter than "the fields we read parse": a non-string `signature`
/// (including `null`) invalidates the whole document, and `assets` may legitimately be an
/// array. Both change whether the device gets a diff at all.
#[test]
fn bug_manifest_validation_was_looser_than_isbundlemanifest() {
    let fixtures = fixtures();
    for (case_name, should_parse) in [
        ("E12", false),  // signature: 99
        ("E12b", false), // signature: null
        ("E12c", false), // an asset that is an array
        ("E13", true),   // signature: "sig-abc"
        ("E19", true),   // assets: []
        ("E20", true),   // assets: [{ fileHash }]
    ] {
        let case = fixtures.cases.iter().find(|c| c.name == case_name).unwrap();
        let target = case.bundles.iter().max_by_key(|b| b.id.clone()).unwrap();
        let parsed = parse_manifest(case, target.manifest_storage_uri.as_deref());
        assert_eq!(
            parsed.is_some(),
            should_parse,
            "{case_name}: manifest acceptance disagrees with isBundleManifest",
        );
    }
}

/// Upstream catches **one** storage failure in the whole artifact path, and its width is exact:
///
/// ```js
/// try { fileUrl = await resolveFileUrl(storageUri, context); }
/// catch (error) { if (!patch) throw error; }
/// ```
///
/// A changed asset covered by a bsdiff patch survives losing its download URL and is emitted
/// patch-only; every other storage failure fails the check. This is easy to lose to a later
/// "simplification" in either direction — a blanket `.ok()` widens it, deleting the branch
/// narrows it — so it gets a lock of its own rather than relying on being one row in the
/// 101-case replay.
///
/// `F03` is the caught case, `F01` and `F02` the ones that must still propagate: all three have
/// an unreachable asset bucket, and the ONLY difference is whether a patch covers every changed
/// asset.
#[tokio::test]
async fn the_per_asset_presign_catch_is_exactly_as_wide_as_upstreams() {
    let Some(replays) = replay_all().await else {
        return;
    };
    let fixtures = fixtures();
    let replay_of = |name: &str| {
        replays
            .iter()
            .find(|r| r.case_name == name)
            .unwrap_or_else(|| panic!("fixture case {name} is gone"))
    };

    // Caught: one changed asset, unreachable, but a patch covers it.
    let caught = replay_of("F03");
    assert_eq!(
        caught.status, 200,
        "F03: an asset covered by a bsdiff patch must survive a presign failure, not fail the \
         check — the catch has been narrowed away",
    );
    let assets = caught.observed["changedAssets"].as_object().unwrap();
    assert_eq!(assets.len(), 1);
    let asset = assets.values().next().unwrap();
    assert!(
        asset.get("file").is_none(),
        "F03: the asset must be emitted patch-only, with no `file` key: {asset}",
    );
    assert!(
        asset.get("patch").is_some(),
        "F03: the patch is what licenses the catch; it must be present: {asset}",
    );

    // Not caught: same unreachable bucket, but assets the patch does not cover.
    for name in ["F01", "F02"] {
        assert!(
            replay_of(name).status >= 500,
            "{name}: an asset with no patch must rethrow — the catch has been widened to assets \
             upstream does not swallow (got HTTP {})",
            replay_of(name).status,
        );
    }

    // And the fixture must still describe that shape, so this cannot pass against a case that
    // was quietly rewritten into something easier.
    for name in ["F01", "F02"] {
        let case = fixtures.cases.iter().find(|c| c.name == name).unwrap();
        assert!(
            case.throws.is_some(),
            "{name} no longer records an upstream throw",
        );
    }
    let f03 = fixtures.cases.iter().find(|c| c.name == "F03").unwrap();
    assert!(
        f03.throws.is_none() && f03.expected.is_some(),
        "F03 no longer records an upstream answer",
    );
}

/// "The object is not there" and "the read failed" are different answers upstream, and they
/// lead to different HTTP status codes.
///
/// `@hot-updater/server` `src/storageAccess.ts` maps a non-OK response to `null`
/// (`if (!response.ok) return null`), and `fetchBundleManifest` turns that into "no artifacts,
/// ship the update anyway". A genuine failure throws and fails the check. `read_s3_file`
/// returns `Err` for both, so adopting upstream's propagate-on-failure behaviour initially made
/// a bundle whose manifest was never uploaded answer 500 — `read_s3_file_optional` restores the
/// distinction.
///
/// `E05` (target manifest absent) and `E15` (diff base absent) are the two shapes; `F04`/`F05`
/// are the same two objects made *unreadable* instead of absent, and must still fail.
#[tokio::test]
async fn bug_a_missing_manifest_object_was_conflated_with_an_unreadable_one() {
    let Some(replays) = replay_all().await else {
        return;
    };
    let replay_of = |name: &str| {
        replays
            .iter()
            .find(|r| r.case_name == name)
            .unwrap_or_else(|| panic!("fixture case {name} is gone"))
    };

    // Absent -> the update still ships.
    let target_absent = replay_of("E05");
    assert_eq!(
        target_absent.status, 200,
        "E05: a target manifest that was never uploaded must not fail the update-check",
    );
    assert!(
        target_absent.observed.get("changedAssets").is_none(),
        "E05: absent manifest means no artifacts at all: {}",
        target_absent.observed,
    );

    let base_absent = replay_of("E15");
    assert_eq!(
        base_absent.status, 200,
        "E15: an absent diff base must not fail the update-check",
    );
    assert_eq!(
        base_absent.observed["changedAssets"]
            .as_object()
            .map(|assets| assets.len()),
        Some(4),
        "E15: with no diff base every asset counts as changed",
    );

    // Unreadable -> the check fails. Same two objects, different failure.
    for name in ["F04", "F05"] {
        assert!(
            replay_of(name).status >= 500,
            "{name}: an unreadable manifest must fail the check, not be treated as absent \
             (got HTTP {})",
            replay_of(name).status,
        );
    }
}
