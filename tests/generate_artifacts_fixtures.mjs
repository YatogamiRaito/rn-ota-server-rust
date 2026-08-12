// Records the real manifest/asset-diff behaviour of the upstream hot-updater packages so the
// Rust update-check response builder (`src/routes/check.rs`) can be tested against it.
//
// The decision layer is manifest-agnostic: upstream's `makeResponse` returns only
// `{ id, message, shouldForceUpdate, status, storageUri, fileHash }`, so everything this file
// records -- `fileUrl`, `manifestUrl`, `manifestFileHash`, `changedAssets` -- is resolved
// OUTSIDE the decision and is not touched by `tests/fixtures/decision_fixtures.json`.
//
// Nothing here is a reimplementation. Every expected value comes out of real upstream code,
// reached in one of two ways:
//
//   1. Called directly, where the function is exported:
//        resolveManifestAssetStorageUri  -- @hot-updater/plugin-core
//        getBundlePatch                  -- @hot-updater/core
//      Both are used ONLY as cross-checks against the full-flow observation (see below); they
//      never stand in for it.
//
//   2. Observed through the PUBLIC `createHotUpdater({ database, storages }).getAppUpdateInfo`,
//      for the module-private functions that are the real subject:
//        updateArtifacts.ts  resolveManifestArtifacts
//        updateArtifacts.ts  resolveHbcPatchDescriptor
//        updateArtifacts.ts  resolveUniqueHbcAssetPath
//        updateArtifacts.ts  resolveChangedAssets   (incl. the brotli `.br` rule)
//        pluginCore.ts       makeResponse
//      The database is an in-memory plugin over the case's own bundle rows; the storage is a
//      real `createRuntimeStoragePlugin` whose runtime profile is a recording map. So the
//      response object, the set of storage URIs upstream asked to presign and the set it asked
//      to read are all upstream's own output.
//
// # Why the recorded URLs are storage URIs
//
// A presigned URL is not reproducible across implementations (different signature, different
// expiry). The fake storage plugin therefore maps each storage URI to a stable download URL and
// LOGS the pair; the recorded response then has every URL replaced by the storage URI it came
// from, looked up in that log. The substitution is an inversion of an observed mapping, never a
// recomputation -- a URL that is not in the log makes the generator throw.
//
// # Why the fake storage plugin fails the way it does
//
// It mirrors the ONE failure the Rust storage layer can produce locally: `resolve_key` in
// `src/storage.rs` rejects a storage URI whose bucket is not the app's configured bucket. So
// here, any URI outside the `bucket` bucket throws from both `getDownloadUrl` and `readText`,
// and a URI with no object behind it reads as `null`. Both are reproducible on the Rust side
// (point the column at another bucket; do not upload the object), which is what makes the
// failure and degradation cases replayable rather than merely recorded.
//
// Re-run whenever the hot-updater packages are upgraded:
//   node tests/generate_artifacts_fixtures.mjs > tests/fixtures/artifacts_fixtures.json
//
// Output is deterministic: fixed case order, no timestamps, no randomness, every observed URI
// list sorted.

// Node's ESM resolver ignores NODE_PATH, so a bare import only works when a node_modules
// containing the package sits on the path from THIS file to the filesystem root. Fall back to
// the tools/fixture-gen install that exists precisely to provide the packages.
const importUpstream = async (specifier, fallbackPath) => {
  try {
    return await import(specifier);
  } catch (err) {
    if (err?.code !== 'ERR_MODULE_NOT_FOUND') throw err;
    return import(new URL(`../tools/fixture-gen/node_modules/${fallbackPath}`, import.meta.url));
  }
};

const { createHotUpdater } = await importUpstream(
  '@hot-updater/server',
  '@hot-updater/server/dist/index.mjs',
);
const { createRuntimeStoragePlugin, resolveManifestAssetStorageUri } = await importUpstream(
  '@hot-updater/plugin-core',
  '@hot-updater/plugin-core/dist/index.mjs',
);
const { getBundlePatch } = await importUpstream(
  '@hot-updater/core',
  '@hot-updater/core/dist/index.mjs',
);

const NIL_UUID = '00000000-0000-0000-0000-000000000000';
const BUCKET = 'bucket';
const FOREIGN_BUCKET = 'other-bucket';
const DOWNLOAD_PREFIX = 'https://download.invalid/';

/** Deterministic, lexicographically ordered, 36-character bundle ids -- the shape the harness
 *  in `tests/common/mod.rs` also uses, so a fixture id can be seeded verbatim. */
const bid = (n) => `00000000-0000-0000-0000-${String(n).padStart(12, '0')}`;

const BASE = bid(1); // the device's current bundle in most cases
const TARGET = bid(2); // the bundle the device is being moved to
// A third bundle, for the "patch names a different base" cases. It sits on its own channel so
// it is never a candidate -- it exists only to be referenced.
const OTHER = bid(3);
const OTHER_CHANNEL = 'archive';

// Ids that actually contain hex LETTERS, for the case-sensitivity of baseBundleId matching.
// `bid()` produces digits only, where toUpperCase() is a no-op and the case rule is invisible.
// Byte order: '...0000AA' < '...0000aa' < '...0000bb', so the decision still picks HEX_TARGET.
const HEX_BASE = '00000000-0000-0000-0000-0000000000aa';
const HEX_TARGET = '00000000-0000-0000-0000-0000000000bb';

// =====================================================================================
// The harness
// =====================================================================================

/** Bucket component of an `s3://bucket/key` URI, or null when it is not an s3 URI at all. */
const bucketOf = (storageUri) => {
  const match = /^s3:\/\/([^/]*)(?:\/|$)/.exec(storageUri ?? '');
  return match ? match[1] : null;
};

const defaultBundle = (id, overrides = {}) => ({
  id,
  platform: 'ios',
  channel: 'production',
  enabled: true,
  shouldForceUpdate: false,
  fileHash: `filehash-${id.slice(-4)}`,
  message: null,
  storageUri: `s3://${BUCKET}/${id}/bundle.zip`,
  targetAppVersion: '1.0.0',
  fingerprintHash: null,
  rolloutCohortCount: 1000,
  targetCohorts: null,
  manifestStorageUri: `s3://${BUCKET}/${id}/manifest.json`,
  manifestFileHash: `manifesthash-${id.slice(-4)}`,
  assetBaseStorageUri: `s3://${BUCKET}/${id}/assets`,
  patches: [],
  ...overrides,
});

/** A manifest document, ready to be stringified into the object store. */
const manifest = (bundleId, assets) => ({ bundleId, assets });

const cases = [];

/**
 * Drives the real public `getAppUpdateInfo` once and records everything observable.
 *
 * `objects` maps a storage URI to the exact text stored at it; a URI absent from the map reads
 * as null (missing object). A URI outside `BUCKET` throws from both storage operations, which
 * is what `resolve_key` in `src/storage.rs` does.
 */
const addCase = async (name, description, dimensions, { bundles, objects = {}, request }) => {
  // storageUri -> download url, recorded as the plugin hands each one out. The recorded
  // response is rewritten through the INVERSE of this map, so a URL can only be turned back
  // into a storage URI if upstream really asked for that URI.
  const urlToStorageUri = new Map();
  const presignedStorageUris = new Set();
  const readStorageUris = new Set();

  const storage = createRuntimeStoragePlugin({
    name: 'fixture-storage',
    supportedProtocol: 's3',
    factory: () => ({
      async getDownloadUrl(storageUri) {
        presignedStorageUris.add(storageUri);
        if (bucketOf(storageUri) !== BUCKET) {
          throw new Error(
            `Bucket name mismatch: expected '${BUCKET}', but found '${bucketOf(storageUri)}'`,
          );
        }
        const fileUrl = `${DOWNLOAD_PREFIX}${encodeURIComponent(storageUri)}`;
        urlToStorageUri.set(fileUrl, storageUri);
        return { fileUrl };
      },
      async readText(storageUri) {
        readStorageUris.add(storageUri);
        if (bucketOf(storageUri) !== BUCKET) {
          throw new Error(
            `Bucket name mismatch: expected '${BUCKET}', but found '${bucketOf(storageUri)}'`,
          );
        }
        return Object.hasOwn(objects, storageUri) ? objects[storageUri] : null;
      },
    }),
  })({});

  const database = {
    name: 'fixture-db',
    async getBundleById(id) {
      return bundles.find((bundle) => bundle.id === id) ?? null;
    },
    async getBundles() {
      // The decision layer is already pinned by decision_fixtures.json; this returns the whole
      // (small) candidate set in one page so the scan terminates immediately and the case is
      // about the artifacts, not about paging.
      return {
        data: bundles.slice(),
        pagination: {
          total: bundles.length,
          hasNextPage: false,
          hasPreviousPage: false,
          currentPage: 1,
          totalPages: bundles.length === 0 ? 0 : 1,
        },
      };
    },
    async getChannels() {
      return ['production'];
    },
  };

  const api = createHotUpdater({ database, storages: [storage] });

  let response = null;
  let thrownMessage = null;
  try {
    response = await api.getAppUpdateInfo(
      {
        platform: request.platform ?? 'ios',
        appVersion: request.appVersion ?? '1.0.0',
        bundleId: request.bundleId,
        minBundleId: request.minBundleId ?? NIL_UUID,
        channel: request.channel ?? 'production',
        _updateStrategy: 'appVersion',
      },
      {},
    );
  } catch (error) {
    thrownMessage = error instanceof Error ? error.message : String(error);
  }

  // ---- rewrite every URL back to the storage URI it was minted from --------------------
  const toStorageUri = (url) => {
    if (url === null || url === undefined) return url;
    const storageUri = urlToStorageUri.get(url);
    if (storageUri === undefined) {
      throw new Error(
        `case '${name}': response URL ${JSON.stringify(url)} was never handed out by the ` +
          `storage plugin, so it cannot be inverted into a storage URI`,
      );
    }
    return storageUri;
  };

  let expected = null;
  if (response !== null && response !== undefined) {
    expected = { ...response };
    // `fileUrl` is always present on an update-available response; keep the distinction
    // between "the key is absent" and "the key is null", both of which upstream can produce.
    if (Object.hasOwn(expected, 'fileUrl')) expected.fileUrl = toStorageUri(expected.fileUrl);
    if (Object.hasOwn(expected, 'manifestUrl')) {
      expected.manifestUrl = toStorageUri(expected.manifestUrl);
    }
    if (Object.hasOwn(expected, 'changedAssets') && expected.changedAssets) {
      expected.changedAssets = Object.fromEntries(
        Object.entries(expected.changedAssets).map(([assetPath, asset]) => {
          const rewritten = { ...asset };
          if (rewritten.file) rewritten.file = { ...rewritten.file, url: toStorageUri(rewritten.file.url) };
          if (rewritten.patch) {
            rewritten.patch = { ...rewritten.patch, patchUrl: toStorageUri(rewritten.patch.patchUrl) };
          }
          return [assetPath, rewritten];
        }),
      );
    }
  }

  // ---- cross-checks: the two exported primitives must agree with the full-flow result ----
  //
  // `resolveManifestAssetStorageUri` and `getBundlePatch` are public, and the private code
  // under test is built out of them. Calling them here is not a second source of truth -- it
  // is a consistency check that makes the generator FAIL rather than quietly record a case
  // whose asset URI or patch selection cannot be explained by the documented primitives.
  const targetBundle = bundles.find((bundle) => bundle.id === expected?.id) ?? null;
  const currentBundle = bundles.find((bundle) => bundle.id === request.bundleId) ?? null;
  if (expected?.changedAssets && targetBundle?.assetBaseStorageUri) {
    const targetManifestText = objects[targetBundle.manifestStorageUri];
    const targetAssets = targetManifestText ? JSON.parse(targetManifestText).assets : {};
    for (const [assetPath, asset] of Object.entries(expected.changedAssets)) {
      if (!asset.file) continue;
      const usesBrotli = /(^|\/)index\.[^/]+\.bundle$/.test(assetPath);
      const primitive = resolveManifestAssetStorageUri({
        assetBaseStorageUri: targetBundle.assetBaseStorageUri,
        assetPath: usesBrotli ? `${assetPath}.br` : assetPath,
        fileHash: targetAssets[assetPath].fileHash,
      });
      if (primitive !== asset.file.url) {
        throw new Error(
          `case '${name}': asset ${JSON.stringify(assetPath)} resolved to ${asset.file.url} ` +
            `through the full flow but resolveManifestAssetStorageUri says ${primitive}`,
        );
      }
      // `compression: "br"` and the `.br` suffix are set together upstream; a case where one
      // appears without the other would mean the rule has been split.
      const hasCompression = asset.file.compression === 'br';
      if (hasCompression !== usesBrotli) {
        throw new Error(
          `case '${name}': asset ${JSON.stringify(assetPath)} has compression=${asset.file.compression} ` +
            `but the brotli path rule says ${usesBrotli}`,
        );
      }
    }
  }
  if (expected?.changedAssets && targetBundle && currentBundle) {
    const primitivePatch = getBundlePatch(targetBundle, currentBundle.id);
    const emitted = Object.values(expected.changedAssets).find((asset) => asset.patch)?.patch ?? null;
    if (emitted && primitivePatch === null) {
      throw new Error(
        `case '${name}': a bsdiff patch was emitted but getBundlePatch selected none`,
      );
    }
    if (emitted && emitted.baseBundleId !== primitivePatch.baseBundleId) {
      throw new Error(
        `case '${name}': emitted patch baseBundleId ${emitted.baseBundleId} disagrees with ` +
          `getBundlePatch's ${primitivePatch.baseBundleId}`,
      );
    }
  }

  cases.push({
    name,
    description,
    dimensions,
    request: {
      platform: request.platform ?? 'ios',
      appVersion: request.appVersion ?? '1.0.0',
      channel: request.channel ?? 'production',
      minBundleId: request.minBundleId ?? NIL_UUID,
      bundleId: request.bundleId,
    },
    bundles,
    objects,
    // `null` here means upstream answered UP_TO_DATE (no update); `throws` means it produced a
    // 5xx instead of an answer. The two are very different and must not be conflated.
    throws: thrownMessage,
    expected,
    // Every storage URI upstream touched, sorted so the fixture is order-independent.
    presignedStorageUris: [...presignedStorageUris].sort(),
    readStorageUris: [...readStorageUris].sort(),
  });
};

// =====================================================================================
// Shared manifest documents
// =====================================================================================

const BASE_MANIFEST = manifest(BASE, {
  'index.abc.bundle': { fileHash: 'aa00000000000000000000000000000000000000000000000000000000000001' },
  'assets/logo.png': { fileHash: 'bb00000000000000000000000000000000000000000000000000000000000002' },
  'assets/font.ttf': { fileHash: 'cc00000000000000000000000000000000000000000000000000000000000003' },
});

const TARGET_MANIFEST = manifest(TARGET, {
  // changed
  'index.abc.bundle': { fileHash: 'dd00000000000000000000000000000000000000000000000000000000000004' },
  // unchanged -- must be ABSENT from changedAssets
  'assets/logo.png': { fileHash: 'bb00000000000000000000000000000000000000000000000000000000000002' },
  // changed
  'assets/font.ttf': { fileHash: 'ee00000000000000000000000000000000000000000000000000000000000005' },
  // added
  'assets/new.jpg': { fileHash: 'ff00000000000000000000000000000000000000000000000000000000000006' },
});

const BASE_MANIFEST_URI = `s3://${BUCKET}/${BASE}/manifest.json`;
const TARGET_MANIFEST_URI = `s3://${BUCKET}/${TARGET}/manifest.json`;

const PATCH = {
  baseBundleId: BASE,
  baseFileHash: `filehash-${BASE.slice(-4)}`,
  patchFileHash: 'patchhash-0001',
  patchStorageUri: `s3://${BUCKET}/${TARGET}/patch.bsdiff`,
};

/** The standard two-bundle setup: `BASE` is what the device has, `TARGET` is the update. */
const pair = (targetOverrides = {}, baseOverrides = {}) => [
  defaultBundle(TARGET, targetOverrides),
  defaultBundle(BASE, baseOverrides),
];

const bothManifests = (targetAssets = TARGET_MANIFEST.assets, baseAssets = BASE_MANIFEST.assets) => ({
  [TARGET_MANIFEST_URI]: JSON.stringify(manifest(TARGET, targetAssets)),
  [BASE_MANIFEST_URI]: JSON.stringify(manifest(BASE, baseAssets)),
});

const fromBase = { bundleId: BASE };
const fromNil = { bundleId: NIL_UUID };

// =====================================================================================
// Group 1 -- the baseline diff: which assets count as "changed"
// =====================================================================================

await addCase(
  'A01',
  'ordinary update: changed + added assets present, the unchanged one omitted entirely',
  { group: 'diff', status: 'UPDATE' },
  { bundles: pair(), objects: bothManifests(), request: fromBase },
);

await addCase(
  'A02',
  'no current manifest (device on NIL) -> every target asset counts as changed',
  { group: 'diff', status: 'UPDATE', currentBundle: 'NIL' },
  { bundles: pair(), objects: bothManifests(), request: fromNil },
);

await addCase(
  'A03',
  'every asset unchanged -> changedAssets is an EMPTY object, not null and not absent',
  { group: 'diff', status: 'UPDATE' },
  {
    bundles: pair(),
    objects: bothManifests(BASE_MANIFEST.assets, BASE_MANIFEST.assets),
    request: fromBase,
  },
);

await addCase(
  'A04',
  'an asset removed in the target simply does not appear; removal is never signalled',
  { group: 'diff', status: 'UPDATE' },
  {
    bundles: pair(),
    objects: bothManifests(
      { 'assets/logo.png': BASE_MANIFEST.assets['assets/logo.png'] },
      BASE_MANIFEST.assets,
    ),
    request: fromBase,
  },
);

await addCase(
  'A05',
  'same path, same hash, different position in the manifest -> still unchanged',
  { group: 'diff', status: 'UPDATE' },
  {
    bundles: pair(),
    objects: bothManifests(
      {
        'assets/font.ttf': BASE_MANIFEST.assets['assets/font.ttf'],
        'assets/logo.png': BASE_MANIFEST.assets['assets/logo.png'],
      },
      BASE_MANIFEST.assets,
    ),
    request: fromBase,
  },
);

await addCase(
  'A06',
  'hash comparison is case-sensitive: the same hex in upper case counts as CHANGED',
  { group: 'diff', status: 'UPDATE' },
  {
    bundles: pair(),
    objects: bothManifests(
      { 'assets/logo.png': { fileHash: 'BB00000000000000000000000000000000000000000000000000000000000002' } },
      { 'assets/logo.png': { fileHash: 'bb00000000000000000000000000000000000000000000000000000000000002' } },
    ),
    request: fromBase,
  },
);

await addCase(
  'A07',
  'an empty target manifest asset set -> changedAssets is an empty object',
  { group: 'diff', status: 'UPDATE' },
  { bundles: pair(), objects: bothManifests({}, BASE_MANIFEST.assets), request: fromBase },
);

await addCase(
  'A08',
  'an asset the current manifest holds under a different path counts as changed',
  { group: 'diff', status: 'UPDATE' },
  {
    bundles: pair(),
    objects: bothManifests(
      { 'assets/moved/logo.png': BASE_MANIFEST.assets['assets/logo.png'] },
      BASE_MANIFEST.assets,
    ),
    request: fromBase,
  },
);

// =====================================================================================
// Group 2 -- the content-addressed `sha256/xx/<hash><ext>` path, and the legacy layout
// =====================================================================================

/** One-asset case: the whole point is the storage URI the single changed asset resolves to. */
const addPathCase = (name, description, dimensions, assetPath, fileHash, assetBaseStorageUri) =>
  addCase(name, description, { group: 'assetPath', ...dimensions }, {
    bundles: pair({ assetBaseStorageUri }),
    objects: {
      [TARGET_MANIFEST_URI]: JSON.stringify(manifest(TARGET, { [assetPath]: { fileHash } })),
    },
    request: fromNil,
  });

const CA_BASE = `s3://${BUCKET}/${TARGET}/assets`;
const LEGACY_BASE = `s3://${BUCKET}/${TARGET}/files`;
const HASH = 'ab12340000000000000000000000000000000000000000000000000000000099';

await addPathCase('B01', 'content-addressed: hash is split into sha256/<first two>/<hash><ext>',
  { layout: 'content-addressed' }, 'assets/logo.png', HASH, CA_BASE);
await addPathCase('B02', 'content-addressed: the ORIGINAL path is dropped, only the extension survives',
  { layout: 'content-addressed' }, 'deeply/nested/dir/logo.png', HASH, CA_BASE);
await addPathCase('B03', 'content-addressed: no extension -> no suffix at all',
  { layout: 'content-addressed' }, 'assets/LICENSE', HASH, CA_BASE);
await addPathCase('B04', 'content-addressed: several dots -> only the LAST segment is the extension',
  { layout: 'content-addressed' }, 'assets/logo.min.tar.gz', HASH, CA_BASE);
await addPathCase('B05', 'content-addressed: a dotfile -- "assets/.gitkeep" -- takes ".gitkeep" as its extension',
  { layout: 'content-addressed' }, 'assets/.gitkeep', HASH, CA_BASE);
await addPathCase('B06', 'content-addressed: a trailing dot yields an empty extension after the dot',
  { layout: 'content-addressed' }, 'assets/weird.', HASH, CA_BASE);
await addPathCase('B07', 'content-addressed: a path that already ends in .br keeps .br as the extension',
  { layout: 'content-addressed' }, 'assets/prepacked.tar.br', HASH, CA_BASE);
await addPathCase('B08', 'content-addressed: the base URI\'s trailing slash is stripped, not doubled',
  { layout: 'content-addressed' }, 'assets/logo.png', HASH, `s3://${BUCKET}/${TARGET}/assets/`);
await addPathCase('B09', 'content-addressed: several trailing slashes are all stripped',
  { layout: 'content-addressed' }, 'assets/logo.png', HASH, `s3://${BUCKET}/${TARGET}/assets///`);
await addPathCase('B10', 'content-addressed: a bucket-root "/assets" base is content-addressed too',
  { layout: 'content-addressed' }, 'assets/logo.png', HASH, `s3://${BUCKET}/assets`);
await addPathCase('B11', 'content-addressed: an UPPERCASE hash keeps its case in both the shard and the name',
  { layout: 'content-addressed' }, 'assets/logo.png', 'AB1234000000000000000000000000000000000000000000000000000000FF', CA_BASE);
await addPathCase('B12', 'content-addressed: a hash SHORTER than two characters -- slice(0,2) does not pad or throw',
  { layout: 'content-addressed', edge: 'short-hash' }, 'assets/logo.png', 'z', CA_BASE);
await addPathCase('B13', 'content-addressed: an EMPTY hash -- the shard segment collapses and is dropped',
  { layout: 'content-addressed', edge: 'empty-hash' }, 'assets/logo.png', '', CA_BASE);
await addPathCase('B14', 'content-addressed: a hash of exactly two characters',
  { layout: 'content-addressed', edge: 'short-hash' }, 'assets/logo.png', 'ab', CA_BASE);
await addPathCase('B15', 'content-addressed: a NON-ASCII hash -- slice(0,2) counts UTF-16 units, not bytes',
  { layout: 'content-addressed', edge: 'non-ascii-hash' }, 'assets/logo.png', 'ödev1234', CA_BASE);

await addPathCase('B20', 'legacy layout: a base NOT ending in /assets keeps the manifest-relative path verbatim',
  { layout: 'legacy-files' }, 'assets/logo.png', HASH, LEGACY_BASE);
await addPathCase('B21', 'legacy layout: a nested path is preserved segment by segment',
  { layout: 'legacy-files' }, 'deeply/nested/dir/logo.png', HASH, LEGACY_BASE);
await addPathCase('B22', 'legacy layout: a space in the filename is percent-encoded per segment',
  { layout: 'legacy-files', edge: 'encoding' }, 'assets/my logo.png', HASH, LEGACY_BASE);
await addPathCase('B23', 'legacy layout: "+" is encoded as %2B by encodeURIComponent',
  { layout: 'legacy-files', edge: 'encoding' }, 'assets/a+b.png', HASH, LEGACY_BASE);
await addPathCase('B24', 'legacy layout: "&" and "=" are encoded',
  { layout: 'legacy-files', edge: 'encoding' }, 'assets/a&b=c.png', HASH, LEGACY_BASE);
await addPathCase('B25', 'legacy layout: "#" and "?" are encoded rather than truncating the URI',
  { layout: 'legacy-files', edge: 'encoding' }, 'assets/a#b?c.png', HASH, LEGACY_BASE);
await addPathCase('B26', 'legacy layout: non-ASCII is percent-encoded as UTF-8',
  { layout: 'legacy-files', edge: 'encoding' }, 'assets/görsel.png', HASH, LEGACY_BASE);
await addPathCase('B27', 'legacy layout: an empty path segment (//) is DROPPED, not preserved',
  { layout: 'legacy-files', edge: 'empty-segment' }, 'assets//logo.png', HASH, LEGACY_BASE);
await addPathCase('B28', 'legacy layout: a leading "./" is normalised away by the URL path parser',
  { layout: 'legacy-files', edge: 'dot-segment' }, './assets/logo.png', HASH, LEGACY_BASE);
await addPathCase('B29', 'legacy layout: a backslash is treated as a SEPARATOR and split on',
  { layout: 'legacy-files', edge: 'backslash' }, 'assets\\logo.png', HASH, LEGACY_BASE);
await addPathCase('B30', 'legacy layout: a leading slash does not produce a double slash',
  { layout: 'legacy-files', edge: 'empty-segment' }, '/assets/logo.png', HASH, LEGACY_BASE);
await addPathCase('B31', 'legacy layout: a base with NO path at all',
  { layout: 'legacy-files' }, 'assets/logo.png', HASH, `s3://${BUCKET}`);
await addPathCase('B32', 'legacy layout: "/assets" as a mid-path segment is NOT content-addressed',
  { layout: 'legacy-files' }, 'logo.png', HASH, `s3://${BUCKET}/assets/v2`);
await addPathCase('B33', 'content-addressed: the encoding rules apply to the extension too',
  { layout: 'content-addressed', edge: 'encoding' }, 'assets/logo.p g', HASH, CA_BASE);

// =====================================================================================
// Group 3 -- the brotli `.br` rule
//
// BR_COMPRESSED_ASSET_PATH_RE = /(^|\/)index\.[^/]+\.bundle$/ -- the ONLY assets stored
// pre-compressed. `.br` is appended to the path BEFORE the storage URI is resolved, which is
// why the content-addressed extension comes out as `.br` and not `.bundle`.
// =====================================================================================

await addPathCase('C01', 'brotli: index.<hash>.bundle at the root matches -> .br suffix and compression',
  { group2: 'brotli', brotli: true }, 'index.abc123.bundle', HASH, CA_BASE);
await addPathCase('C02', 'brotli: index.<hash>.bundle in a subdirectory matches (the (^|/) alternative)',
  { group2: 'brotli', brotli: true }, 'build/index.abc123.bundle', HASH, CA_BASE);
await addPathCase('C03', 'brotli: a bare "index.bundle" does NOT match -- [^/]+ needs at least one character',
  { group2: 'brotli', brotli: false }, 'index.bundle', HASH, CA_BASE);
await addPathCase('C04', 'brotli: "index..bundle" does NOT match either',
  { group2: 'brotli', brotli: false }, 'index..bundle', HASH, CA_BASE);
await addPathCase('C05', 'brotli: several dots between index. and .bundle DO match ([^/]+ spans dots)',
  { group2: 'brotli', brotli: true }, 'index.a.b.c.bundle', HASH, CA_BASE);
await addPathCase('C06', 'brotli: a filename merely ENDING in index.x.bundle does not match (no separator)',
  { group2: 'brotli', brotli: false }, 'myindex.abc.bundle', HASH, CA_BASE);
await addPathCase('C07', 'brotli: the match is anchored at the end -- a trailing .map does not match',
  { group2: 'brotli', brotli: false }, 'index.abc.bundle.map', HASH, CA_BASE);
await addPathCase('C08', 'brotli: an ordinary .bundle asset is NOT brotli-compressed',
  { group2: 'brotli', brotli: false }, 'main.jsbundle.bundle', HASH, CA_BASE);
await addPathCase('C09', 'brotli: a BACKSLASH before index. does not match -- the regex only knows "/"',
  { group2: 'brotli', brotli: false, edge: 'backslash' }, 'build\\index.abc.bundle', HASH, CA_BASE);
await addPathCase('C10', 'brotli under the LEGACY layout: .br is appended to the stored path itself',
  { group2: 'brotli', brotli: true, layout: 'legacy-files' }, 'index.abc123.bundle', HASH, LEGACY_BASE);
await addPathCase('C11', 'brotli: uppercase "Index." does not match -- the regex is case-sensitive',
  { group2: 'brotli', brotli: false }, 'Index.abc.bundle', HASH, CA_BASE);

// =====================================================================================
// Group 4 -- resolveUniqueHbcAssetPath + resolveHbcPatchDescriptor (the bsdiff patch)
// =====================================================================================

const withPatch = (patch = PATCH, targetOverrides = {}) =>
  pair({ patches: [patch], ...targetOverrides });

await addCase(
  'D01',
  'a patch from the device\'s current bundle lands on the single .bundle asset',
  { group: 'patch', patch: 'emitted' },
  { bundles: withPatch(), objects: bothManifests(), request: fromBase },
);

await addCase(
  'D02',
  'TWO .bundle assets -> resolveUniqueHbcAssetPath is ambiguous, NO patch is emitted at all',
  { group: 'patch', patch: 'ambiguous' },
  {
    bundles: withPatch(),
    objects: bothManifests(
      {
        'index.abc.bundle': { fileHash: 'dd00000000000000000000000000000000000000000000000000000000000004' },
        'other.bundle': { fileHash: 'ee00000000000000000000000000000000000000000000000000000000000005' },
      },
      BASE_MANIFEST.assets,
    ),
    request: fromBase,
  },
);

await addCase(
  'D03',
  'NO .bundle asset at all -> no patch, the rest of the diff is unaffected',
  { group: 'patch', patch: 'no-candidate' },
  {
    bundles: withPatch(),
    objects: bothManifests(
      { 'assets/logo.png': { fileHash: 'dd00000000000000000000000000000000000000000000000000000000000004' } },
      BASE_MANIFEST.assets,
    ),
    request: fromBase,
  },
);

await addCase(
  'D04',
  'the unique .bundle asset need not be brotli-shaped: "other.bundle" alone still takes the patch',
  { group: 'patch', patch: 'emitted' },
  {
    bundles: withPatch(),
    objects: bothManifests(
      { 'other.bundle': { fileHash: 'dd00000000000000000000000000000000000000000000000000000000000004' } },
      BASE_MANIFEST.assets,
    ),
    request: fromBase,
  },
);

await addCase(
  'D05',
  'the patch names a DIFFERENT base bundle -> getBundlePatch finds nothing, no patch emitted',
  { group: 'patch', patch: 'base-mismatch' },
  {
    bundles: [
      defaultBundle(TARGET, { patches: [{ ...PATCH, baseBundleId: OTHER }] }),
      defaultBundle(BASE),
      defaultBundle(OTHER, { channel: OTHER_CHANNEL }),
    ],
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'D06',
  'baseBundleId matching is CASE-SENSITIVE: an upper-case hex id does not match a lower-case one',
  { group: 'patch', patch: 'base-case' },
  {
    bundles: [
      defaultBundle(HEX_TARGET, {
        patches: [{ ...PATCH, baseBundleId: HEX_BASE.toUpperCase(), baseFileHash: `filehash-${HEX_BASE.slice(-4)}` }],
      }),
      defaultBundle(HEX_BASE),
    ],
    objects: {
      [`s3://${BUCKET}/${HEX_TARGET}/manifest.json`]: JSON.stringify(manifest(HEX_TARGET, TARGET_MANIFEST.assets)),
      [`s3://${BUCKET}/${HEX_BASE}/manifest.json`]: JSON.stringify(manifest(HEX_BASE, BASE_MANIFEST.assets)),
    },
    request: { bundleId: HEX_BASE },
  },
);

await addCase(
  'D06b',
  'the same shape with matching case -> the patch IS selected, proving D06 turns on case alone',
  { group: 'patch', patch: 'base-case' },
  {
    bundles: [
      defaultBundle(HEX_TARGET, {
        patches: [{ ...PATCH, baseBundleId: HEX_BASE, baseFileHash: `filehash-${HEX_BASE.slice(-4)}` }],
      }),
      defaultBundle(HEX_BASE),
    ],
    objects: {
      [`s3://${BUCKET}/${HEX_TARGET}/manifest.json`]: JSON.stringify(manifest(HEX_TARGET, TARGET_MANIFEST.assets)),
      [`s3://${BUCKET}/${HEX_BASE}/manifest.json`]: JSON.stringify(manifest(HEX_BASE, BASE_MANIFEST.assets)),
    },
    request: { bundleId: HEX_BASE },
  },
);

await addCase(
  'D07',
  'device on NIL -> there is no current bundle, so no patch even though one exists',
  { group: 'patch', patch: 'no-current' },
  { bundles: withPatch(), objects: bothManifests(), request: fromNil },
);

await addCase(
  'D08',
  'an EMPTY patchStorageUri is falsy upstream -> the whole descriptor is dropped',
  { group: 'patch', patch: 'empty-field' },
  {
    bundles: withPatch({ ...PATCH, patchStorageUri: '' }),
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'D09',
  'an EMPTY patchFileHash is falsy upstream -> the whole descriptor is dropped',
  { group: 'patch', patch: 'empty-field' },
  {
    bundles: withPatch({ ...PATCH, patchFileHash: '' }),
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'D10',
  'an EMPTY baseFileHash is falsy upstream -> the whole descriptor is dropped',
  { group: 'patch', patch: 'empty-field' },
  {
    bundles: withPatch({ ...PATCH, baseFileHash: '' }),
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'D11',
  'two patch records for the SAME base -> the first wins (getBundlePatches dedupes)',
  { group: 'patch', patch: 'duplicate-base' },
  {
    bundles: pair({
      patches: [PATCH, { ...PATCH, patchFileHash: 'patchhash-SECOND', patchStorageUri: `s3://${BUCKET}/${TARGET}/patch-second.bsdiff` }],
    }),
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'D12',
  'patch records in order [other, matching] -> the matching one is still selected',
  { group: 'patch', patch: 'ordering' },
  {
    bundles: [
      defaultBundle(TARGET, { patches: [{ ...PATCH, baseBundleId: OTHER }, PATCH] }),
      defaultBundle(BASE),
      defaultBundle(OTHER, { channel: OTHER_CHANNEL }),
    ],
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'D13',
  'the .bundle asset is UNCHANGED -> it is not in changedAssets, so the patch has nowhere to go',
  { group: 'patch', patch: 'unchanged-target' },
  {
    bundles: withPatch(),
    objects: bothManifests(BASE_MANIFEST.assets, BASE_MANIFEST.assets),
    request: fromBase,
  },
);

await addCase(
  'D14',
  'patchStorageUri in ANOTHER bucket -> resolveHbcPatchDescriptor does NOT catch, so the whole check 5xxs',
  { group: 'patch', patch: 'presign-fails', failure: 'patch-presign', expect: 'throws' },
  {
    bundles: withPatch({ ...PATCH, patchStorageUri: `s3://${FOREIGN_BUCKET}/${TARGET}/patch.bsdiff` }),
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'D15',
  'a patch on a ROLLBACK response: artifacts are resolved for rollbacks too',
  { group: 'patch', status: 'ROLLBACK' },
  {
    bundles: [
      defaultBundle(BASE, { patches: [{ ...PATCH, baseBundleId: TARGET, baseFileHash: `filehash-${TARGET.slice(-4)}` }] }),
      defaultBundle(TARGET, { enabled: false }),
    ],
    objects: bothManifests(),
    request: { bundleId: TARGET },
  },
);

// =====================================================================================
// Group 5 -- when the artifact block is dropped entirely
// =====================================================================================

await addCase(
  'E01',
  'manifestStorageUri NULL -> no artifacts at all, but the update still ships',
  { group: 'degrade', missing: 'manifestStorageUri' },
  { bundles: pair({ manifestStorageUri: null }), objects: bothManifests(), request: fromBase },
);

await addCase(
  'E02',
  'manifestFileHash NULL -> no artifacts (all three columns are required together)',
  { group: 'degrade', missing: 'manifestFileHash' },
  { bundles: pair({ manifestFileHash: null }), objects: bothManifests(), request: fromBase },
);

await addCase(
  'E03',
  'assetBaseStorageUri NULL -> no artifacts',
  { group: 'degrade', missing: 'assetBaseStorageUri' },
  { bundles: pair({ assetBaseStorageUri: null }), objects: bothManifests(), request: fromBase },
);

await addCase(
  'E04',
  'an EMPTY manifestFileHash is falsy upstream and drops the artifacts exactly like NULL',
  { group: 'degrade', missing: 'manifestFileHash-empty' },
  { bundles: pair({ manifestFileHash: '' }), objects: bothManifests(), request: fromBase },
);

await addCase(
  'E05',
  'the target manifest object is MISSING -> no artifacts',
  { group: 'degrade', failure: 'target-manifest-missing' },
  {
    bundles: pair(),
    objects: { [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST) },
    request: fromBase,
  },
);

await addCase(
  'E06',
  'the target manifest is not JSON -> no artifacts',
  { group: 'degrade', failure: 'target-manifest-invalid' },
  {
    bundles: pair(),
    objects: { [TARGET_MANIFEST_URI]: 'not json at all', [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST) },
    request: fromBase,
  },
);

await addCase(
  'E07',
  'the target manifest is a JSON ARRAY -> isBundleManifest rejects it',
  { group: 'degrade', failure: 'target-manifest-shape' },
  {
    bundles: pair(),
    objects: { [TARGET_MANIFEST_URI]: '[]', [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST) },
    request: fromBase,
  },
);

await addCase(
  'E08',
  'the target manifest has no bundleId -> rejected',
  { group: 'degrade', failure: 'target-manifest-shape' },
  {
    bundles: pair(),
    objects: {
      [TARGET_MANIFEST_URI]: JSON.stringify({ assets: TARGET_MANIFEST.assets }),
      [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST),
    },
    request: fromBase,
  },
);

await addCase(
  'E09',
  'the target manifest bundleId is a NUMBER -> rejected (it must be a string)',
  { group: 'degrade', failure: 'target-manifest-shape' },
  {
    bundles: pair(),
    objects: {
      [TARGET_MANIFEST_URI]: JSON.stringify({ bundleId: 7, assets: TARGET_MANIFEST.assets }),
      [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST),
    },
    request: fromBase,
  },
);

await addCase(
  'E10',
  'the target manifest has no assets key -> rejected',
  { group: 'degrade', failure: 'target-manifest-shape' },
  {
    bundles: pair(),
    objects: {
      [TARGET_MANIFEST_URI]: JSON.stringify({ bundleId: TARGET }),
      [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST),
    },
    request: fromBase,
  },
);

await addCase(
  'E11',
  'an asset whose fileHash is a NUMBER invalidates the WHOLE manifest, not just that asset',
  { group: 'degrade', failure: 'target-manifest-shape' },
  {
    bundles: pair(),
    objects: {
      [TARGET_MANIFEST_URI]: JSON.stringify({
        bundleId: TARGET,
        assets: { 'assets/logo.png': { fileHash: 12345 } },
      }),
      [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST),
    },
    request: fromBase,
  },
);

await addCase(
  'E12',
  'an asset whose SIGNATURE is a number also invalidates the whole manifest',
  { group: 'degrade', failure: 'target-manifest-shape', edge: 'signature' },
  {
    bundles: pair(),
    objects: {
      [TARGET_MANIFEST_URI]: JSON.stringify({
        bundleId: TARGET,
        assets: { 'assets/logo.png': { fileHash: HASH, signature: 99 } },
      }),
      [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST),
    },
    request: fromBase,
  },
);

await addCase(
  'E12b',
  'a NULL signature is rejected too -- the check is `=== undefined || typeof === "string"`, and null is neither',
  { group: 'degrade', failure: 'target-manifest-shape', edge: 'signature' },
  {
    bundles: pair(),
    objects: {
      [TARGET_MANIFEST_URI]: JSON.stringify({
        bundleId: TARGET,
        assets: { 'assets/logo.png': { fileHash: HASH, signature: null } },
      }),
      [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST),
    },
    request: fromBase,
  },
);

await addCase(
  'E12c',
  'an asset that is an ARRAY is rejected (isBundleManifest excludes arrays per asset)',
  { group: 'degrade', failure: 'target-manifest-shape' },
  {
    bundles: pair(),
    objects: {
      [TARGET_MANIFEST_URI]: JSON.stringify({ bundleId: TARGET, assets: { 'assets/logo.png': [] } }),
      [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST),
    },
    request: fromBase,
  },
);

await addCase(
  'E19',
  'assets as an empty ARRAY is ACCEPTED -- Array.isArray is only checked on the manifest and on each asset',
  { group: 'degrade', edge: 'assets-array' },
  {
    bundles: pair(),
    objects: {
      [TARGET_MANIFEST_URI]: JSON.stringify({ bundleId: TARGET, assets: [] }),
      [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST),
    },
    request: fromBase,
  },
);

await addCase(
  'E20',
  'assets as a POPULATED array is accepted too, and the array index becomes the asset path',
  { group: 'degrade', edge: 'assets-array' },
  {
    bundles: pair(),
    objects: {
      [TARGET_MANIFEST_URI]: JSON.stringify({ bundleId: TARGET, assets: [{ fileHash: HASH }] }),
      [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST),
    },
    request: fromBase,
  },
);

await addCase(
  'E21',
  'an EMPTY manifestStorageUri is falsy and drops the artifacts, exactly like NULL',
  { group: 'degrade', missing: 'manifestStorageUri-empty' },
  { bundles: pair({ manifestStorageUri: '' }), objects: bothManifests(), request: fromBase },
);

await addCase(
  'E22',
  'an EMPTY assetBaseStorageUri is falsy and drops the artifacts, exactly like NULL',
  { group: 'degrade', missing: 'assetBaseStorageUri-empty' },
  { bundles: pair({ assetBaseStorageUri: '' }), objects: bothManifests(), request: fromBase },
);

await addCase(
  'E23',
  'the CURRENT bundle having an empty manifestStorageUri means no diff base, not a read of ""',
  { group: 'degrade', failure: 'current-manifest-empty-column' },
  { bundles: pair({}, { manifestStorageUri: '' }), objects: bothManifests(), request: fromBase },
);

await addCase(
  'E13',
  'a STRING signature is accepted, and is not carried into the response',
  { group: 'diff', edge: 'signature' },
  {
    bundles: pair(),
    objects: {
      [TARGET_MANIFEST_URI]: JSON.stringify({
        bundleId: TARGET,
        assets: { 'assets/logo.png': { fileHash: HASH, signature: 'sig-abc' } },
      }),
      [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST),
    },
    request: fromBase,
  },
);

await addCase(
  'E14',
  'the manifest bundleId need not match the bundle it belongs to -- it is never compared',
  { group: 'diff', edge: 'bundleId-mismatch' },
  {
    bundles: pair(),
    objects: {
      [TARGET_MANIFEST_URI]: JSON.stringify(manifest('completely-unrelated', { 'assets/logo.png': { fileHash: HASH } })),
      [BASE_MANIFEST_URI]: JSON.stringify(BASE_MANIFEST),
    },
    request: fromBase,
  },
);

await addCase(
  'E15',
  'the CURRENT manifest is missing -> the update still ships, with every asset marked changed',
  { group: 'degrade', failure: 'current-manifest-missing' },
  {
    bundles: pair(),
    objects: { [TARGET_MANIFEST_URI]: JSON.stringify(TARGET_MANIFEST) },
    request: fromBase,
  },
);

await addCase(
  'E16',
  'the CURRENT manifest is invalid JSON -> same as missing, artifacts still returned',
  { group: 'degrade', failure: 'current-manifest-invalid' },
  {
    bundles: pair(),
    objects: { [TARGET_MANIFEST_URI]: JSON.stringify(TARGET_MANIFEST), [BASE_MANIFEST_URI]: '{{{' },
    request: fromBase,
  },
);

await addCase(
  'E17',
  'the current bundle has NO manifestStorageUri -> no diff base, every asset changed',
  { group: 'degrade', failure: 'current-manifest-null-column' },
  {
    bundles: pair({}, { manifestStorageUri: null }),
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'E18',
  'the device reports an unknown id ABOVE every bundle -> ROLLBACK, and with no diff base every asset is changed',
  { group: 'degrade', failure: 'current-bundle-unknown', status: 'ROLLBACK' },
  {
    bundles: [defaultBundle(TARGET), defaultBundle(BASE)],
    objects: bothManifests(),
    request: { bundleId: bid(9), minBundleId: NIL_UUID },
  },
);

// =====================================================================================
// Group 6 -- storage failures, and where they degrade vs where they become a 5xx
// =====================================================================================

await addCase(
  'F01',
  'the ASSET base is in another bucket -> presign throws and, with no patch, the error PROPAGATES',
  { group: 'failure', failure: 'asset-presign', expect: 'throws' },
  {
    bundles: pair({ assetBaseStorageUri: `s3://${FOREIGN_BUCKET}/${TARGET}/assets` }),
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'F02',
  'the asset base is in another bucket BUT a patch covers the one .bundle asset: the other assets still throw',
  { group: 'failure', failure: 'asset-presign', expect: 'throws' },
  {
    bundles: withPatch(PATCH, { assetBaseStorageUri: `s3://${FOREIGN_BUCKET}/${TARGET}/assets` }),
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'F03',
  'a patch covers the ONLY changed asset and its file presign fails -> the asset ships patch-only, no `file` key',
  { group: 'failure', failure: 'asset-presign', expect: 'patch-only' },
  {
    bundles: withPatch(PATCH, { assetBaseStorageUri: `s3://${FOREIGN_BUCKET}/${TARGET}/assets` }),
    objects: bothManifests(
      { 'index.abc.bundle': { fileHash: 'dd00000000000000000000000000000000000000000000000000000000000004' } },
      BASE_MANIFEST.assets,
    ),
    request: fromBase,
  },
);

await addCase(
  'F04',
  'the TARGET MANIFEST is in another bucket -> readText throws and the whole check becomes a 5xx',
  { group: 'failure', failure: 'target-manifest-bucket', expect: 'throws' },
  {
    bundles: pair({ manifestStorageUri: `s3://${FOREIGN_BUCKET}/${TARGET}/manifest.json` }),
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'F05',
  'the CURRENT manifest is in another bucket -> that read throws too, so the check is a 5xx',
  { group: 'failure', failure: 'current-manifest-bucket', expect: 'throws' },
  {
    bundles: pair({}, { manifestStorageUri: `s3://${FOREIGN_BUCKET}/${BASE}/manifest.json` }),
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'F06',
  'the BUNDLE itself is in another bucket -> resolveFileUrl throws; fileUrl is never null on an UPDATE',
  { group: 'failure', failure: 'bundle-presign', expect: 'throws' },
  {
    bundles: pair({ storageUri: `s3://${FOREIGN_BUCKET}/${TARGET}/bundle.zip` }),
    objects: bothManifests(),
    request: fromBase,
  },
);

// =====================================================================================
// Group 7 -- makeResponse: the fields that do not come from the manifest
// =====================================================================================

await addCase(
  'G01',
  'shouldForceUpdate is carried through unchanged on an UPDATE',
  { group: 'makeResponse', status: 'UPDATE' },
  { bundles: pair({ shouldForceUpdate: true }), objects: bothManifests(), request: fromBase },
);

await addCase(
  'G02',
  'a ROLLBACK forces the update regardless of the bundle\'s own shouldForceUpdate=false',
  { group: 'makeResponse', status: 'ROLLBACK' },
  {
    bundles: [defaultBundle(BASE), defaultBundle(TARGET, { enabled: false })],
    objects: bothManifests(),
    request: { bundleId: TARGET },
  },
);

await addCase(
  'G03',
  'message and fileHash are carried straight through; storageUri is NOT in the response',
  { group: 'makeResponse', status: 'UPDATE' },
  {
    bundles: pair({ message: 'ship it', fileHash: 'the-file-hash' }),
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'G04',
  'no candidate above the device\'s bundle and nothing to roll back to -> UP_TO_DATE (null)',
  { group: 'makeResponse', status: 'UP_TO_DATE' },
  {
    bundles: [defaultBundle(BASE)],
    objects: bothManifests(),
    request: fromBase,
  },
);

await addCase(
  'G05',
  'the reset-to-built-in rollback: NIL id, null fileUrl, and NO manifest artifacts are resolved',
  { group: 'makeResponse', status: 'INIT_ROLLBACK' },
  {
    bundles: [],
    objects: {},
    request: { bundleId: BASE },
  },
);

await addCase(
  'G06',
  'a null message stays null rather than being dropped from the response',
  { group: 'makeResponse', status: 'UPDATE' },
  { bundles: pair({ message: null }), objects: bothManifests(), request: fromBase },
);

// =====================================================================================

const output = { cases };
process.stdout.write(JSON.stringify(output, null, 2));
process.stdout.write('\n');
