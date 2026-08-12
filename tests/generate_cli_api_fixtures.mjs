// Records the real REQUEST and RESPONSE BODY contract of the upstream hot-updater CLI API so
// the Rust side (`src/routes/api.rs`) can be tested against it.
//
// `tests/generate_pagination_fixtures.mjs` already records the QUERY-STRING layer of
// `GET /api/bundles`. This file extends the same technique past the query string to the
// bodies: the exact field set and casing of a bundle in a response, which keys are OMITTED
// versus null, the accepted shape of the POST payload (including its patches array), the
// PATCH payload and which fields are actually patchable, the success body of each verb, and
// the error bodies.
//
// Nothing here is a reimplementation. The whole upstream stack is assembled and driven:
//
//   createHandler                @hot-updater/server dist/handler.mjs
//     -> createPluginDatabaseCore  @hot-updater/server dist/db/pluginCore.mjs
//          (assertBundlePersistenceConstraints, the insert/update/delete orchestration)
//       -> createDatabasePlugin    @hot-updater/plugin-core dist/createDatabasePlugin.mjs
//            (BundleUnitOfWork, mergeBundleUpdate -- the PATCH merge semantics)
//         -> an in-memory store whose ONLY logic is the real upstream row mappers:
//              rowToBundle / bundleToRow / bundleToPatchRows   dist/db/bundleRows.mjs
//              bundleMatchesQueryWhere / sortBundles           dist/queryBundles.mjs
//
// `bundleRows.mjs` is module-private (not in the package `exports` map) and is imported by
// explicit path, the same fallback the pagination generator already uses. It is the right
// ground truth: ALL FOUR shipped SQL adapters (kysely, drizzle, prisma, mongodb) map their
// rows with it, so it *is* upstream's database-row <-> Bundle contract, which is exactly what
// `src/routes/api.rs` `map_to_client_bundle` / `CLIBundle` mirror.
//
// Each case records the raw response text, the parsed body AND the top-level key list.
// JSON.stringify silently drops an `undefined` value, so an omitted key and a null one are
// indistinguishable in the parsed body alone -- and upstream really does omit some. The
// `bodyKeys` / `bundleKeys` / `patchKeys` arrays are what keep that distinction recorded.
//
// Every case runs against a FRESH store, so the cases are order-independent.
//
// Re-run whenever the hot-updater packages are upgraded:
//   node tests/generate_cli_api_fixtures.mjs > tests/fixtures/cli_api_fixtures.json
//
// Output is deterministic: fixed case order, no timestamps, no randomness, no id generation.

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

const { createHandler } = await importUpstream(
  '@hot-updater/server',
  '@hot-updater/server/dist/index.mjs',
);
const { createDatabasePlugin } = await importUpstream(
  '@hot-updater/plugin-core',
  '@hot-updater/plugin-core/dist/index.mjs',
);

// Module-private upstream internals, reached by explicit path. These have no bare specifier
// to try first -- they are not in either package's `exports` map.
const fixtureGenModules = new URL('../tools/fixture-gen/node_modules/', import.meta.url);
const { rowToBundle, bundleToRow, bundleToPatchRows } = await import(
  new URL('@hot-updater/server/dist/db/bundleRows.mjs', fixtureGenModules)
);
const { createPluginDatabaseCore } = await import(
  new URL('@hot-updater/server/dist/db/pluginCore.mjs', fixtureGenModules)
);
const { bundleMatchesQueryWhere, sortBundles } = await import(
  new URL('@hot-updater/plugin-core/dist/queryBundles.mjs', fixtureGenModules)
);

// =====================================================================================
// The harness: the real upstream stack over an in-memory row store.
// =====================================================================================

const clone = (value) => (value === undefined ? undefined : JSON.parse(JSON.stringify(value)));

/**
 * An in-memory store of snake_case DATABASE ROWS -- the same shape `bundles` and
 * `bundle_patches` have in `migrations/*.sql`. Reads go out through the real `rowToBundle`
 * and writes come back in through the real `bundleToRow` / `bundleToPatchRows`, so the store
 * itself contributes no mapping logic of its own.
 */
const makeStack = (bundleRows = [], patchRows = []) => {
  const store = {
    bundles: clone(bundleRows) ?? [],
    patches: clone(patchRows) ?? [],
  };

  // Every mutation the handler drove, captured at the plugin boundary. `appendBundle`
  // receives the POST body verbatim; `updateBundle` receives the PATCH payload after
  // `requireBundlePatchPayload` has stripped `id`.
  const calls = [];

  const patchesOf = (bundleId) => store.patches.filter((p) => p.bundle_id === bundleId);
  const toBundle = (row) => rowToBundle(row, patchesOf(row.id));

  const methods = {
    name: 'fixture-store',
    async getBundleById(bundleId) {
      const row = store.bundles.find((b) => b.id === bundleId);
      return row ? toBundle(row) : null;
    },
    async getBundles(options) {
      const all = store.bundles.map(toBundle).filter((b) => bundleMatchesQueryWhere(b, options.where));
      const sorted = sortBundles(all, options.orderBy);
      const offset = options.offset ?? 0;
      return {
        data: sorted.slice(offset, offset + options.limit),
        pagination: { total: sorted.length },
      };
    },
    async getChannels() {
      return [...new Set(store.bundles.map((b) => b.channel))].sort();
    },
    async commitBundle({ changedSets }) {
      for (const { operation, data } of changedSets) {
        store.bundles = store.bundles.filter((b) => b.id !== data.id);
        store.patches = store.patches.filter((p) => p.bundle_id !== data.id);
        if (operation === 'delete') continue;
        store.bundles.push(bundleToRow(data));
        store.patches.push(...bundleToPatchRows(data));
      }
    },
  };

  // The self-hosted server passes a database *factory*, so each mutation runs on a fresh
  // plugin instance with its own unit of work (createHotUpdaterCore.mjs). Mirror that.
  const database = createDatabasePlugin({ name: 'fixture-store', factory: () => methods })({}, {});

  const instrument = (instance) => ({
    ...instance,
    async appendBundle(bundle, context) {
      calls.push({
        op: 'appendBundle',
        // Key presence, recorded separately: JSON.stringify drops undefined values, and an
        // absent field is not the same request as an explicitly null one.
        keys: bundle && typeof bundle === 'object' ? Object.keys(bundle) : null,
        value: clone(bundle) ?? null,
      });
      return instance.appendBundle(bundle, context);
    },
    async updateBundle(bundleId, patch, context) {
      calls.push({
        op: 'updateBundle',
        bundleId,
        keys: patch && typeof patch === 'object' ? Object.keys(patch) : null,
        value: clone(patch) ?? null,
      });
      return instance.updateBundle(bundleId, patch, context);
    },
    async deleteBundle(bundle, context) {
      calls.push({ op: 'deleteBundle', bundleId: bundle?.id ?? null });
      return instance.deleteBundle(bundle, context);
    },
  });

  const core = createPluginDatabaseCore(() => instrument(database()), async () => null, {
    createMutationPlugin: () => instrument(database()),
  });

  // basePath "" keeps the recorded paths readable: `/api/bundles`, not `/api/api/bundles`.
  const handler = createHandler(core.api, { basePath: '', routes: { bundles: true } });

  const request = async (method, path, body) => {
    const init = { method };
    if (body !== undefined) {
      init.body = typeof body === 'string' ? body : JSON.stringify(body);
      init.headers = { 'Content-Type': 'application/json' };
    }
    const response = await handler(new Request(`http://fixture.invalid${path}`, init));
    const text = await response.text();
    let json = null;
    try {
      json = JSON.parse(text);
    } catch {
      json = undefined;
    }
    return {
      status: response.status,
      contentType: response.headers.get('content-type'),
      body: text,
      json,
      bodyKeys: json && typeof json === 'object' && !Array.isArray(json) ? Object.keys(json) : null,
    };
  };

  // `bundleToRow` emits a key with an `undefined` VALUE for every field the payload omitted
  // (there are no defaults in it), and JSON.stringify would erase that. Record each row's
  // key list next to its JSON form so "the column was never given a value" stays visible.
  const describeRows = (rows) =>
    rows
      .slice()
      .sort((a, b) => a.id.localeCompare(b.id))
      .map((r) => ({
        // Keys whose value is undefined -- the column the adapter is given nothing for.
        undefinedKeys: Object.keys(r).filter((k) => r[k] === undefined),
        value: clone(r),
      }));

  const rows = () => ({
    bundles: describeRows(store.bundles),
    patches: describeRows(store.patches),
  });

  return { request, rows, calls };
};

// =====================================================================================
// The dataset. Ids are lowercase-hex uuidv7 shapes, the alphabet the upstream CLI
// generates -- byte order and ICU order agree there, so `docs/upstream-parity.md` §3.3
// cannot contaminate these fixtures.
// =====================================================================================

const ID1 = '01900000-0000-7000-8000-000000000001';
const ID2 = '01900000-0000-7000-8000-000000000002';
const ID3 = '01900000-0000-7000-8000-000000000003';
const MISSING_ID = '01900000-0000-7000-8000-0000000000ff';

const BASE_ROW = {
  id: ID2,
  platform: 'ios',
  should_force_update: 1,
  enabled: 1,
  file_hash: 'file-hash-2',
  git_commit_hash: 'abc1234',
  message: 'second bundle',
  channel: 'production',
  storage_uri: 's3://bucket/app/2/bundle.zip',
  target_app_version: '1.0.0',
  fingerprint_hash: null,
  metadata: { buildTool: 'metro' },
  manifest_storage_uri: 's3://bucket/app/2/manifest.json',
  manifest_file_hash: 'sha256:manifest-2',
  asset_base_storage_uri: 's3://bucket/app/2/assets',
  rollout_cohort_count: 1000,
  target_cohorts: null,
};

const row = (overrides) => ({ ...BASE_ROW, ...overrides });

const patchRow = (bundleId, baseBundleId, orderIndex, overrides = {}) => ({
  id: `${bundleId}:${baseBundleId}`,
  bundle_id: bundleId,
  base_bundle_id: baseBundleId,
  base_file_hash: `base-hash-${orderIndex}`,
  patch_file_hash: `patch-hash-${orderIndex}`,
  patch_storage_uri: `s3://bucket/app/${bundleId}/patch-${orderIndex}.bin`,
  order_index: orderIndex,
  ...overrides,
});

// =====================================================================================
// Group 1 -- GET /api/bundles/:id : the bundle RESPONSE body
//
// handleGetBundle JSON.stringifies whatever the api returns, so the response body IS
// `rowToBundle`'s output. This group pins the field set, the camelCase spelling, the
// omitted-vs-null decisions and the patch-array ordering.
// =====================================================================================

const bundleResponseCases = [];

const addBundleResponse = async (description, bundleRow, patchRows = []) => {
  const stack = makeStack([bundleRow], patchRows);
  const response = await stack.request('GET', `/api/bundles/${bundleRow.id}`);
  bundleResponseCases.push({
    description,
    row: clone(bundleRow),
    patchRows: clone(patchRows),
    status: response.status,
    contentType: response.contentType,
    // The exact key list upstream emitted, in emission order. Compare key SETS in the
    // replay -- JSON object key order carries no meaning over the wire -- but record the
    // order so a field that silently disappears is visible in the diff.
    bundleKeys: response.bodyKeys,
    bundle: response.json,
  });
};

await addBundleResponse('a fully populated row', row({}));
await addBundleResponse(
  'every nullable column null (fingerprintHash carries the constraint instead)',
  row({
    git_commit_hash: null,
    message: null,
    target_app_version: null,
    fingerprint_hash: 'fp-abc',
    metadata: null,
    manifest_storage_uri: null,
    manifest_file_hash: null,
    asset_base_storage_uri: null,
    rollout_cohort_count: null,
    target_cohorts: null,
  }),
);
await addBundleResponse('metadata stored as a JSON object', row({ metadata: { a: 1, b: { c: 2 } } }));
await addBundleResponse('metadata stored as a JSON string', row({ metadata: '{"a":1}' }));
await addBundleResponse('metadata stored as an empty object', row({ metadata: {} }));
await addBundleResponse('metadata is unparseable text', row({ metadata: 'not json at all' }));
await addBundleResponse('metadata is a JSON array', row({ metadata: '[1,2,3]' }));
await addBundleResponse('metadata is the JSON literal null', row({ metadata: 'null' }));
await addBundleResponse('metadata is the empty string', row({ metadata: '' }));
await addBundleResponse('should_force_update / enabled as MySQL tinyint 0', row({ should_force_update: 0, enabled: 0 }));
await addBundleResponse('should_force_update / enabled as real booleans', row({ should_force_update: true, enabled: false }));
await addBundleResponse('rollout_cohort_count 0 is kept, not defaulted', row({ rollout_cohort_count: 0 }));
await addBundleResponse('rollout_cohort_count null falls back to the default', row({ rollout_cohort_count: null }));
await addBundleResponse('rollout_cohort_count part way through a rollout', row({ rollout_cohort_count: 250 }));
await addBundleResponse('target_cohorts stored as a JSON string array', row({ target_cohorts: '["alpha","beta"]' }));
await addBundleResponse('target_cohorts stored as a real array', row({ target_cohorts: ['alpha', 'beta'] }));
await addBundleResponse('target_cohorts with non-string entries filtered out', row({ target_cohorts: '["alpha",1,null,"beta"]' }));
await addBundleResponse('target_cohorts unparseable', row({ target_cohorts: 'not json' }));
await addBundleResponse('target_cohorts is a JSON object, not an array', row({ target_cohorts: '{"a":1}' }));
await addBundleResponse('target_cohorts is an empty JSON array', row({ target_cohorts: '[]' }));
await addBundleResponse('target_cohorts is the empty string', row({ target_cohorts: '' }));
await addBundleResponse('one patch -- the deprecated patch* mirror fields are filled from it', row({}), [
  patchRow(ID2, ID1, 0),
]);
await addBundleResponse('two patches -- order_index decides the primary', row({}), [
  patchRow(ID2, ID3, 1),
  patchRow(ID2, ID1, 0),
]);
await addBundleResponse('two patches with an equal order_index -- baseBundleId breaks the tie', row({}), [
  patchRow(ID2, ID3, 0),
  patchRow(ID2, ID1, 0),
]);
await addBundleResponse('a patch row with a missing order_index sorts as 0', row({}), [
  patchRow(ID2, ID3, 1),
  { ...patchRow(ID2, ID1, 0), order_index: undefined },
]);

// The 404 body -- a JSON object, not bare text.
{
  const stack = makeStack([row({})]);
  const response = await stack.request('GET', `/api/bundles/${MISSING_ID}`);
  bundleResponseCases.push({
    description: 'an unknown bundle id',
    row: null,
    patchRows: [],
    status: response.status,
    contentType: response.contentType,
    bundleKeys: response.bodyKeys,
    bundle: response.json,
  });
}

// =====================================================================================
// Group 2 -- GET /api/bundles and GET /api/bundles/channels : the envelope bodies
// =====================================================================================

const envelopeCases = [];

const addEnvelope = async (description, method, path, bundleRows, patchRows = []) => {
  const stack = makeStack(bundleRows, patchRows);
  const response = await stack.request(method, path);
  envelopeCases.push({
    description,
    method,
    path,
    status: response.status,
    contentType: response.contentType,
    bodyKeys: response.bodyKeys,
    // For the list envelope: the key list of the `data` element and of `pagination`.
    dataElementKeys: Array.isArray(response.json?.data) && response.json.data[0]
      ? Object.keys(response.json.data[0])
      : null,
    paginationKeys: response.json?.pagination ? Object.keys(response.json.pagination) : null,
    body: response.json,
  });
};

await addEnvelope('list with no bundles at all', 'GET', '/api/bundles', []);
await addEnvelope('list with one bundle', 'GET', '/api/bundles', [row({})]);
await addEnvelope('list with three bundles, default limit', 'GET', '/api/bundles', [
  row({ id: ID1, file_hash: 'file-hash-1' }),
  row({}),
  row({ id: ID3, file_hash: 'file-hash-3' }),
]);
await addEnvelope('list with a limit that splits the set -- nextCursor appears', 'GET', '/api/bundles?limit=2', [
  row({ id: ID1, file_hash: 'file-hash-1' }),
  row({}),
  row({ id: ID3, file_hash: 'file-hash-3' }),
]);
await addEnvelope('list element carries the SAME bundle shape as the single-bundle route', 'GET', '/api/bundles', [row({})], [
  patchRow(ID2, ID1, 0),
]);
await addEnvelope('channels with no bundles', 'GET', '/api/bundles/channels', []);
await addEnvelope('channels with one channel', 'GET', '/api/bundles/channels', [row({})]);
await addEnvelope('channels with several channels', 'GET', '/api/bundles/channels', [
  row({ id: ID1, channel: 'staging' }),
  row({}),
  row({ id: ID3, channel: 'development' }),
]);

// =====================================================================================
// Group 3 -- POST /api/bundles : the CLIBundle request body
//
// handleCreateBundles wraps a non-array body in an array and hands each element STRAIGHT to
// api.insertBundle -- there is no schema validation at the HTTP layer at all. What the
// payload must look like is therefore decided further down, by
// assertBundlePersistenceConstraints (pluginCore) and by what bundleToRow /
// bundleToPatchRows / getBundlePatches do with it.
//
// Each case records the status and body, the object appendBundle received (with its key
// list) and the rows that ended up persisted -- so the accepted field set, the defaults and
// the silently-dropped fields are all observed rather than inferred.
// =====================================================================================

const createBundleCases = [];

// A body that satisfies the persistence constraints, used as the base for the variants.
const CLI_BUNDLE = {
  id: ID2,
  platform: 'ios',
  shouldForceUpdate: false,
  enabled: true,
  fileHash: 'file-hash-2',
  gitCommitHash: 'abc1234',
  message: 'published from the CLI',
  channel: 'production',
  storageUri: 's3://bucket/app/2/bundle.zip',
  targetAppVersion: '1.0.0',
  fingerprintHash: null,
  metadata: { buildTool: 'metro' },
  manifestStorageUri: 's3://bucket/app/2/manifest.json',
  manifestFileHash: 'sha256:manifest-2',
  assetBaseStorageUri: 's3://bucket/app/2/assets',
  patches: [],
  rolloutCohortCount: 1000,
  targetCohorts: null,
};

const cliBundle = (overrides) => ({ ...CLI_BUNDLE, ...overrides });

const cliPatch = (baseBundleId, n) => ({
  baseBundleId,
  baseFileHash: `base-hash-${n}`,
  patchFileHash: `patch-hash-${n}`,
  patchStorageUri: `s3://bucket/app/patch-${n}.bin`,
});

const addCreate = async (description, requestBody, { existingRows = [] } = {}) => {
  const stack = makeStack(existingRows);
  const response = await stack.request('POST', '/api/bundles', requestBody);
  const appended = stack.calls.filter((c) => c.op === 'appendBundle');
  createBundleCases.push({
    description,
    requestBody: typeof requestBody === 'string' ? requestBody : clone(requestBody) ?? null,
    requestBodyIsRawText: typeof requestBody === 'string',
    status: response.status,
    contentType: response.contentType,
    bodyKeys: response.bodyKeys,
    body: response.json,
    // What upstream forwarded verbatim, one entry per bundle in the request.
    appended: appended.map((c) => ({ keys: c.keys, value: c.value })),
    persisted: stack.rows(),
  });
};

await addCreate('a complete CLI bundle', cliBundle({}));
await addCreate('an array body with two bundles', [
  cliBundle({ id: ID1, fileHash: 'file-hash-1' }),
  cliBundle({ id: ID3, fileHash: 'file-hash-3' }),
]);
await addCreate('an empty array body -- 201 with nothing written', []);
await addCreate(
  'only the fields the CLI cannot omit -- everything else is left out',
  { id: ID2, platform: 'ios', fileHash: 'file-hash-2', storageUri: 's3://b/2.zip', targetAppVersion: '1.0.0' },
);
// POSTing an id that already exists is not a conflict at the handler level: upstream answers
// 201 {"success":true} and hands the bundle down unchanged. Whether the row is then replaced
// or rejected is the adapter's business, so only the STATUS and BODY of this case are upstream
// ground truth -- the `persisted` rows reflect this harness's replace-by-id store.
await addCreate('POSTing an id that already exists', cliBundle({ message: 'republished' }), {
  existingRows: [row({ message: 'original' })],
});
await addCreate('a fingerprint bundle (targetAppVersion null)', cliBundle({ targetAppVersion: null, fingerprintHash: 'fp-abc' }));
await addCreate('neither targetAppVersion nor fingerprintHash', cliBundle({ targetAppVersion: null, fingerprintHash: null }));
await addCreate('targetAppVersion present but whitespace only', cliBundle({ targetAppVersion: '   ', fingerprintHash: null }));
await addCreate('fingerprintHash present but whitespace only', cliBundle({ targetAppVersion: null, fingerprintHash: '\t\n ' }));
await addCreate('targetAppVersion is the empty string', cliBundle({ targetAppVersion: '', fingerprintHash: null }));
await addCreate('both fields omitted entirely rather than nulled', (() => {
  const { targetAppVersion, fingerprintHash, ...rest } = CLI_BUNDLE;
  return rest;
})());
await addCreate('rolloutCohortCount 0 -- a fully paused rollout', cliBundle({ rolloutCohortCount: 0 }));
await addCreate('rolloutCohortCount at the maximum', cliBundle({ rolloutCohortCount: 1000 }));
await addCreate('rolloutCohortCount one above the maximum', cliBundle({ rolloutCohortCount: 1001 }));
await addCreate('rolloutCohortCount negative', cliBundle({ rolloutCohortCount: -1 }));
await addCreate('rolloutCohortCount fractional', cliBundle({ rolloutCohortCount: 1.5 }));
await addCreate('rolloutCohortCount null -- accepted, defaulted on the way to the row', cliBundle({ rolloutCohortCount: null }));
await addCreate('rolloutCohortCount omitted entirely', (() => {
  const { rolloutCohortCount, ...rest } = CLI_BUNDLE;
  return rest;
})());
await addCreate('rolloutCohortCount as a numeric string', cliBundle({ rolloutCohortCount: '500' }));
await addCreate('targetCohorts with valid slugs', cliBundle({ targetCohorts: ['alpha', 'beta-2'] }));
await addCreate('targetCohorts with a numeric cohort string', cliBundle({ targetCohorts: ['1', '1000'] }));
await addCreate('targetCohorts with an uppercase slug', cliBundle({ targetCohorts: ['Alpha'] }));
await addCreate('targetCohorts with a space in the slug', cliBundle({ targetCohorts: ['alpha beta'] }));
await addCreate('targetCohorts with an out-of-range numeric cohort', cliBundle({ targetCohorts: ['1001'] }));
await addCreate('targetCohorts empty array', cliBundle({ targetCohorts: [] }));
await addCreate('one patch', cliBundle({ patches: [cliPatch(ID1, 1)] }), { existingRows: [row({ id: ID1 })] });
await addCreate('two patches -- order is preserved as order_index', cliBundle({ patches: [cliPatch(ID1, 1), cliPatch(ID3, 3)] }), {
  existingRows: [row({ id: ID1 }), row({ id: ID3 })],
});
await addCreate('two patches sharing a baseBundleId', cliBundle({ patches: [cliPatch(ID1, 1), cliPatch(ID1, 9)] }), {
  existingRows: [row({ id: ID1 })],
});
await addCreate('a patch missing patchStorageUri', cliBundle({
  patches: [{ baseBundleId: ID1, baseFileHash: 'h', patchFileHash: 'p' }],
}), { existingRows: [row({ id: ID1 })] });
await addCreate('a patch whose baseFileHash is null', cliBundle({
  patches: [{ ...cliPatch(ID1, 1), baseFileHash: null }],
}), { existingRows: [row({ id: ID1 })] });
// NOT recorded: "a patch whose baseBundleId names no bundle". Referential integrity lives in
// the adapter's SQL schema, and the in-memory store has none -- whatever this harness answered
// would be the harness's behaviour, not upstream's. `src/routes/api.rs` answers 400 there; that
// is a deliberate decision of this server's and has no ground truth at this boundary.
await addCreate('patches is null', cliBundle({ patches: null }));
await addCreate('patches omitted entirely', (() => {
  const { patches, ...rest } = CLI_BUNDLE;
  return rest;
})());
await addCreate('patches is not an array', cliBundle({ patches: { baseBundleId: ID1 } }));
await addCreate('metadata omitted', (() => {
  const { metadata, ...rest } = CLI_BUNDLE;
  return rest;
})());
await addCreate('metadata null', cliBundle({ metadata: null }));
await addCreate('metadata is a non-object', cliBundle({ metadata: 'plain text' }));
await addCreate('an unknown extra field', cliBundle({ someFutureField: 'ignored?' }));
await addCreate('the deprecated flat patch fields instead of the patches array', cliBundle({
  patches: undefined,
  patchBaseBundleId: ID1,
  patchBaseFileHash: 'base-hash-1',
  patchFileHash: 'patch-hash-1',
  patchStorageUri: 's3://bucket/app/patch-1.bin',
}), { existingRows: [row({ id: ID1 })] });
await addCreate('a body that is a bare string', '"just a string"');
await addCreate('a body that is the JSON literal null', 'null');
await addCreate('a body that is a number', '42');
await addCreate('a body that is not JSON at all', 'this is not json');

// =====================================================================================
// Group 4 -- PATCH /api/bundles/:id : the UpdateBundlePayload request body
//
// requireBundlePatchPayload (handler.mjs:76-82) is the whole HTTP-layer contract: an array
// body collapses to its FIRST element, a non-object is a 400, an `id` that disagrees with
// the route is a 400, and a matching `id` is STRIPPED. Everything that survives is merged
// into the stored bundle by mergeBundleUpdate (createDatabasePlugin.mjs:15-19), which is an
// es-toolkit `mergeWith` -- a DEEP merge, with `patches` and `targetCohorts` replaced whole.
// =====================================================================================

const updateBundleCases = [];

const addUpdate = async (description, requestBody, { targetId = ID2, existingRows, patchRows = [] } = {}) => {
  const rows = existingRows ?? [row({}), row({ id: ID1, file_hash: 'file-hash-1' })];
  const stack = makeStack(rows, patchRows);
  const response = await stack.request('PATCH', `/api/bundles/${targetId}`, requestBody);
  const updates = stack.calls.filter((c) => c.op === 'updateBundle');
  updateBundleCases.push({
    description,
    targetId,
    requestBody: typeof requestBody === 'string' ? requestBody : clone(requestBody) ?? null,
    requestBodyIsRawText: typeof requestBody === 'string',
    beforeRows: clone(rows),
    status: response.status,
    contentType: response.contentType,
    bodyKeys: response.bodyKeys,
    body: response.json,
    // The payload that survived requireBundlePatchPayload, with its exact key list. This is
    // the definitive answer to "which fields are patchable".
    patchPayload: updates[0] ? { keys: updates[0].keys, value: updates[0].value } : null,
    persisted: stack.rows(),
  });
};

await addUpdate('enabling a bundle', { enabled: false });
await addUpdate('an empty patch object', {});
await addUpdate('a matching id is accepted and stripped from the payload', { id: ID2, enabled: false });
await addUpdate('a mismatched id', { id: ID1, enabled: false });
await addUpdate('id null is neither a mismatch nor stripped-to-nothing', { id: null, enabled: false });
await addUpdate('an array body collapses to its first element', [{ enabled: false }, { enabled: true }]);
await addUpdate('an empty array body', []);
await addUpdate('a bare string body', '"nope"');
await addUpdate('a numeric body', '7');
await addUpdate('a null body', 'null');
await addUpdate('a boolean body', 'true');
await addUpdate('setting message to null explicitly', { message: null });
await addUpdate('setting gitCommitHash to null explicitly', { gitCommitHash: null });
await addUpdate('setting targetAppVersion to null while fingerprintHash stays null', { targetAppVersion: null });
await addUpdate('setting targetAppVersion to null when fingerprintHash is set', { targetAppVersion: null }, {
  existingRows: [row({ fingerprint_hash: 'fp-abc' })],
});
await addUpdate('setting a field to undefined (the key is absent after JSON round-trip)', { message: undefined, enabled: false });
await addUpdate('patching every scalar column at once', {
  platform: 'android',
  shouldForceUpdate: true,
  enabled: false,
  fileHash: 'file-hash-new',
  gitCommitHash: 'def5678',
  message: 'patched',
  channel: 'staging',
  storageUri: 's3://bucket/app/2/new.zip',
  targetAppVersion: '2.0.0',
  fingerprintHash: 'fp-new',
  manifestStorageUri: 's3://bucket/app/2/new-manifest.json',
  manifestFileHash: 'sha256:manifest-new',
  assetBaseStorageUri: 's3://bucket/app/2/new-assets',
  rolloutCohortCount: 500,
});
await addUpdate('metadata is DEEP merged, not replaced', { metadata: { a: 1 } }, {
  existingRows: [row({ metadata: { b: 2, nested: { x: 1 } } })],
});
await addUpdate('a nested metadata object is merged key by key', { metadata: { nested: { y: 2 } } }, {
  existingRows: [row({ metadata: { nested: { x: 1 } } })],
});
await addUpdate('metadata set to an empty object leaves the stored keys in place', { metadata: {} }, {
  existingRows: [row({ metadata: { b: 2 } })],
});
await addUpdate('metadata overwrites a scalar with a scalar', { metadata: { a: 2 } }, {
  existingRows: [row({ metadata: { a: 1 } })],
});
await addUpdate('metadata overwrites an object with a scalar', { metadata: { a: 'flat' } }, {
  existingRows: [row({ metadata: { a: { deep: 1 } } })],
});
await addUpdate('metadata overwrites a scalar with an object', { metadata: { a: { deep: 1 } } }, {
  existingRows: [row({ metadata: { a: 'flat' } })],
});
// es-toolkit `merge` walks arrays INDEX BY INDEX rather than replacing them, so a shorter
// patch array leaves the tail of the stored one behind. `metadata` is the only place this can
// bite -- `patches` and `targetCohorts` are in REPLACE_ON_UPDATE_KEYS.
await addUpdate('metadata array is merged index by index, not replaced', { metadata: { a: [9] } }, {
  existingRows: [row({ metadata: { a: [1, 2, 3] } })],
});
await addUpdate('metadata array grows', { metadata: { a: [1, 2, 3] } }, {
  existingRows: [row({ metadata: { a: [9] } })],
});
await addUpdate('metadata value set to null explicitly', { metadata: { a: null } }, {
  existingRows: [row({ metadata: { a: 1 } })],
});
await addUpdate('metadata set on a bundle whose column is NULL', { metadata: { a: 1 } }, {
  existingRows: [row({ metadata: null })],
});
await addUpdate('metadata set on a bundle whose column is unparseable', { metadata: { a: 1 } }, {
  existingRows: [row({ metadata: 'not json' })],
});
await addUpdate('metadata itself set to null', { metadata: null }, {
  existingRows: [row({ metadata: { b: 2 } })],
});
await addUpdate('targetCohorts set to null', { targetCohorts: null }, {
  existingRows: [row({ target_cohorts: '["alpha"]' })],
});

// An explicit null against each remaining column, so "which columns can a PATCH clear?" is
// answered by the recording rather than by reading the schema. Upstream's v0_31_0 schema makes
// exactly eight columns nullable (git_commit_hash, message, target_app_version,
// fingerprint_hash, target_cohorts and the three manifest/asset ones); `channel`, `metadata`
// and `rollout_cohort_count` have DEFAULTS but are NOT nullable, and the rest are required. The
// cases below show what bundleToRow actually hands the adapter for each.
await addUpdate('manifestStorageUri set to null (a nullable column)', { manifestStorageUri: null });
await addUpdate('fingerprintHash set to null when targetAppVersion is set', { fingerprintHash: null });
await addUpdate('rolloutCohortCount set to null (defaulted, not nulled)', { rolloutCohortCount: null }, {
  existingRows: [row({ rollout_cohort_count: 250 })],
});
await addUpdate('channel set to null (a NOT NULL column with a default)', { channel: null });
await addUpdate('enabled set to null (a required column)', { enabled: null });
await addUpdate('fileHash set to null (a required column)', { fileHash: null });
await addUpdate('platform set to null (a required column)', { platform: null });
await addUpdate('storageUri set to null (a required column)', { storageUri: null });
await addUpdate('shouldForceUpdate set to null (a required column)', { shouldForceUpdate: null });

// THE TWO-MERGE-SEMANTICS INVARIANT, in ONE request. `metadata` is deep merged and
// `targetCohorts` is replaced -- same body, same handler, opposite behaviour, because
// targetCohorts is in REPLACE_ON_UPDATE_KEYS and metadata is not. A single-key case cannot
// catch someone unifying the two; this one can.
await addUpdate('metadata MERGES and targetCohorts REPLACES in the same request', {
  metadata: { added: true },
  targetCohorts: ['gamma'],
}, {
  existingRows: [row({ metadata: { kept: 1 }, target_cohorts: '["alpha","beta"]' })],
});
await addUpdate('metadata MERGES and patches REPLACE in the same request', {
  metadata: { added: true },
  patches: [cliPatch(ID1, 9)],
}, {
  existingRows: [row({ metadata: { kept: 1 } }), row({ id: ID1 })],
  patchRows: [patchRow(ID2, ID3, 0)],
});
await addUpdate('targetCohorts is REPLACED, not merged', { targetCohorts: ['gamma'] }, {
  existingRows: [row({ target_cohorts: '["alpha","beta"]' })],
});
await addUpdate('targetCohorts replaced with an empty array', { targetCohorts: [] }, {
  existingRows: [row({ target_cohorts: '["alpha","beta"]' })],
});
await addUpdate('patches is REPLACED, not merged', { patches: [cliPatch(ID1, 9)] }, {
  existingRows: [row({}), row({ id: ID1 })],
  patchRows: [patchRow(ID2, ID3, 0)],
});
await addUpdate('patches replaced with an empty array clears them', { patches: [] }, {
  existingRows: [row({}), row({ id: ID1 })],
  patchRows: [patchRow(ID2, ID1, 0)],
});
await addUpdate('the deprecated flat patch fields are patchable too', {
  patchBaseBundleId: ID1,
  patchBaseFileHash: 'base-hash-9',
  patchFileHash: 'patch-hash-9',
  patchStorageUri: 's3://bucket/app/patch-9.bin',
}, { existingRows: [row({}), row({ id: ID1 })] });
await addUpdate('rolloutCohortCount above the maximum', { rolloutCohortCount: 1001 });
await addUpdate('rolloutCohortCount negative', { rolloutCohortCount: -1 });
await addUpdate('rolloutCohortCount 0', { rolloutCohortCount: 0 });
await addUpdate('an invalid target cohort', { targetCohorts: ['NOT A SLUG'] });
await addUpdate('an unknown extra field', { someFutureField: 'kept?' });
await addUpdate('patching a bundle that does not exist', { enabled: false }, { targetId: MISSING_ID });

// =====================================================================================
// Group 5 -- DELETE /api/bundles/:id
// =====================================================================================

const deleteBundleCases = [];

const addDelete = async (description, targetId, existingRows, patchRows = []) => {
  const stack = makeStack(existingRows, patchRows);
  const response = await stack.request('DELETE', `/api/bundles/${targetId}`);
  deleteBundleCases.push({
    description,
    targetId,
    beforeRows: clone(existingRows),
    status: response.status,
    contentType: response.contentType,
    bodyKeys: response.bodyKeys,
    body: response.json,
    deleteCalls: stack.calls.filter((c) => c.op === 'deleteBundle').length,
    persisted: stack.rows(),
  });
};

await addDelete('deleting an existing bundle', ID2, [row({}), row({ id: ID1 })]);
await addDelete('deleting a bundle that has patches', ID2, [row({}), row({ id: ID1 })], [patchRow(ID2, ID1, 0)]);
await addDelete('deleting a bundle that does not exist -- still 200', MISSING_ID, [row({})]);
await addDelete('deleting from an empty store', MISSING_ID, []);

// =====================================================================================

const output = {
  bundleResponseCases,
  envelopeCases,
  createBundleCases,
  updateBundleCases,
  deleteBundleCases,
};
process.stdout.write(JSON.stringify(output, null, 2));
process.stdout.write('\n');
