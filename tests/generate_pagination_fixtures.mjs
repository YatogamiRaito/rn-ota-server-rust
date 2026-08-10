// Records the real cursor-pagination and query-parameter behaviour of the upstream
// hot-updater packages so the Rust CLI API (`src/routes/api.rs`) can be tested against it.
//
// Nothing here is a reimplementation. Every expected value comes out of real upstream code,
// reached in one of three ways:
//
//   1. Called directly, where the function is exported:
//        calculatePagination        -- @hot-updater/plugin-core
//
//   2. Observed through the PUBLIC createDatabasePlugin, for module-private functions. An
//      in-memory factory records every inner getBundles() call, so the query shape, the row
//      order and the pagination object are all upstream's own output:
//        buildCursorPageQuery, createPaginatedResult
//
//   3. Observed by driving @hot-updater/server's real createHandler() with a stub api and
//      reading either the forwarded options or the 400 body, for the private query-parameter
//      helpers in handler.mjs:
//        parsePositiveIntegerSearchParam  -> limitParams (bounded 1..=100)
//        the inline `page` rule            -> pageParams (NO upper bound, different message)
//        the inline `platform` check       -> platformParams
//        parseStringArraySearchParam       -> stringArrayParams (getAll; commas never split)
//        parseBooleanSearchParam           -> booleanParams (exact "true"/"false" only)
//        parseNullableStringSearchParam    -> nullableStringParams ("null" -> SQL NULL)
//        the `...channel && {channel}` fold -> truthyStringParams (empty value drops filter)
//
// The last two groups are deliberately kept apart: `targetAppVersion`/`fingerprintHash` are
// guarded with `!== undefined` (so `?x=` is a real filter on the empty string) while
// `channel` is guarded with JS truthiness (so `?channel=` drops the filter entirely). Two
// different emptiness rules in the same handler.
//
// Re-run whenever the hot-updater packages are upgraded:
//   node tests/generate_pagination_fixtures.mjs > tests/fixtures/pagination_fixtures.json
//
// Output is deterministic: fixed case order, no timestamps, no randomness.

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

const { calculatePagination, createDatabasePlugin } = await importUpstream(
  '@hot-updater/plugin-core',
  '@hot-updater/plugin-core/dist/index.mjs',
);
const { createHandler } = await importUpstream(
  '@hot-updater/server',
  '@hot-updater/server/dist/index.mjs',
);

// =====================================================================================
// Group 1 -- calculatePagination(total, { limit, offset })
// =====================================================================================

const paginationCases = [];
const addPagination = (description, total, limit, offset) => {
  paginationCases.push({
    description,
    total,
    limit,
    offset,
    expected: calculatePagination(total, { limit, offset }),
  });
};

// The total === 0 short circuit returns currentPage 1 / totalPages 0 whatever the offset is.
addPagination('total 0 short circuit', 0, 50, 0);
addPagination('total 0 with limit 1', 0, 1, 0);
addPagination('total 0 with an offset past the end', 0, 50, 100);
addPagination('total 0 with a huge offset', 0, 10, 999999);

// Ordinary aligned pages over 7 rows with limit 2.
addPagination('first page of 7 with limit 2', 7, 2, 0);
addPagination('second page of 7 with limit 2', 7, 2, 2);
addPagination('third page of 7 with limit 2', 7, 2, 4);
addPagination('last page of 7 with limit 2', 7, 2, 6);

// Offsets that are NOT a multiple of limit -- this is what a cursor page produces, and it is
// where upstream's own currentPage becomes incoherent (floor division).
addPagination('non-aligned offset 1 with limit 2', 7, 2, 1);
addPagination('non-aligned offset 3 with limit 2', 7, 2, 3);
addPagination('non-aligned offset 5 with limit 2', 7, 2, 5);
addPagination('non-aligned offset 1 with limit 5', 7, 5, 1);
addPagination('non-aligned offset 4 with limit 5', 7, 5, 4);

// Offsets past the end.
addPagination('offset exactly equal to total', 7, 2, 7);
addPagination('offset past the end', 7, 2, 10);
addPagination('offset far past the end', 7, 2, 100);

// limit 1 and limit 100 boundaries.
addPagination('limit 1 first row', 7, 1, 0);
addPagination('limit 1 last row', 7, 1, 6);
addPagination('limit 1 one past the last row', 7, 1, 7);
addPagination('limit 100 with fewer rows than the limit', 7, 100, 0);
addPagination('limit 100 first page of 250', 250, 100, 0);
addPagination('limit 100 second page of 250', 250, 100, 100);
addPagination('limit 100 last page of 250', 250, 100, 200);

// hasNextPage boundary: offset + limit < total.
addPagination('hasNextPage boundary -- offset+limit equals total', 10, 5, 5);
addPagination('hasNextPage boundary -- offset+limit one below total', 11, 5, 5);
addPagination('hasNextPage boundary -- offset+limit one above total', 10, 5, 6);
addPagination('hasNextPage true just below the boundary', 10, 5, 4);

// hasPreviousPage boundary: offset > 0.
addPagination('hasPreviousPage boundary -- offset 0', 10, 5, 0);
addPagination('hasPreviousPage boundary -- offset 1', 10, 5, 1);

// Exact multiples and small totals.
addPagination('total is an exact multiple of limit -- first page', 100, 50, 0);
addPagination('total is an exact multiple of limit -- last page', 100, 50, 50);
addPagination('total 1 with limit 1', 1, 1, 0);
addPagination('total 1 with limit 50', 1, 50, 0);
addPagination('total smaller than limit', 3, 50, 0);
addPagination('large total, last page', 1000000, 50, 999950);

// =====================================================================================
// Group 2 -- buildCursorPageQuery + createPaginatedResult, via createDatabasePlugin
// =====================================================================================

const DATASET_7 = ['b1', 'b2', 'b3', 'b4', 'b5', 'b6', 'b7'];
const DATASET_1 = ['b1'];

// An in-memory getBundles that honours the id gt/lt filter, the order direction, limit and
// offset exactly as a SQL-backed plugin would, and records every call it receives.
const makeRecordingPlugin = (ids) => {
  const rows = ids.map((id) => ({ id }));
  const calls = [];
  const factory = () => ({
    name: 'fixture-recorder',
    async getBundles(options) {
      let matching = rows.slice();
      const idFilter = options.where?.id;
      if (idFilter?.gt !== undefined) matching = matching.filter((r) => r.id > idFilter.gt);
      if (idFilter?.lt !== undefined) matching = matching.filter((r) => r.id < idFilter.lt);

      const direction = options.orderBy?.direction ?? 'desc';
      matching.sort((a, b) =>
        direction === 'desc' ? b.id.localeCompare(a.id) : a.id.localeCompare(b.id),
      );

      const total = matching.length;
      const offset = options.offset ?? 0;
      const data = matching.slice(offset, offset + options.limit);

      calls.push({
        where: JSON.parse(JSON.stringify(options.where ?? {})),
        orderByDirection: direction,
        limit: options.limit,
        offset: options.offset ?? null,
        returnedTotal: total,
        returnedIds: data.map((r) => r.id),
      });

      return { data, pagination: { total } };
    },
    async getBundleById() {
      return null;
    },
    async getChannels() {
      return [];
    },
    async commitBundle() {},
  });

  return { plugin: createDatabasePlugin({ name: 'fixture-recorder', factory })({}, {})(), calls };
};

const cursorCases = [];

const addCursor = (description, dataset, { limit, after, before, page }) => {
  const { plugin, calls } = makeRecordingPlugin(dataset);
  return plugin
    .getBundles({
      where: {},
      limit,
      ...(page !== undefined ? { page } : {}),
      ...(after !== undefined || before !== undefined ? { cursor: { after, before } } : {}),
    })
    .then((result) => {
      const pagination = result.pagination;

      // createPaginatedResult omits nextCursor/previousCursor entirely rather than setting
      // them to null. JSON.stringify would drop an undefined value silently and make the
      // fixture ambiguous, so the key set is recorded explicitly alongside the object.
      const paginationKeys = Object.keys(pagination);

      // Two different upstream branches, with two different inner-call shapes.
      //
      // `page` branch: offset pagination, ONE call (or two when the requested page is past
      //   the end and upstream re-queries at the clamped offset). No cursor query at all --
      //   the cursor is ignored entirely.
      // cursor fallback branch: call 0 is the unfiltered total probe, call 1 is the cursor
      //   page query, call 2 (only when the page is non-empty) is the count-before probe
      //   whose total IS the startIndex handed to createPaginatedResult.
      const usesPageBranch = page !== undefined;
      const pageCall = usesPageBranch ? null : calls[1] ?? null;
      const countBeforeCall = usesPageBranch ? null : calls[2] ?? null;

      // startIndex is the offset createPaginatedResult was given -- observed, never assumed.
      let startIndex;
      if (usesPageBranch) startIndex = calls[calls.length - 1].offset;
      else if (countBeforeCall) startIndex = countBeforeCall.returnedTotal;
      else if (result.data.length === 0 && (after !== undefined || before !== undefined))
        startIndex = after !== undefined ? pagination.total : 0;
      else startIndex = 0;

      // reverseData is derived from the direction flip rather than by diffing the row order:
      // upstream flips the sort and sets reverseData together, and a page of 0 or 1 rows
      // makes the reversal itself invisible. Cross-check the two whenever the page is long
      // enough for them both to be meaningful, so this stays an observation and not a guess.
      const requestedDirection = 'desc';
      const reverseData = pageCall ? pageCall.orderByDirection !== requestedDirection : false;
      if (pageCall && result.data.length > 1) {
        const observedByOrder =
          JSON.stringify(pageCall.returnedIds) !== JSON.stringify(result.data.map((b) => b.id));
        if (observedByOrder !== reverseData) {
          throw new Error(
            `reverseData derivation disagrees for case '${description}': ` +
              `direction-flip says ${reverseData}, row order says ${observedByOrder}`,
          );
        }
      }

      cursorCases.push({
        description,
        dataset,
        limit,
        after: after ?? null,
        before: before ?? null,
        page: page ?? null,
        usesPageBranch,
        // The plan buildCursorPageQuery produced, read back off the recorded inner call.
        cursorPlan: pageCall
          ? {
              idFilter: pageCall.where.id
                ? pageCall.where.id.gt !== undefined
                  ? { op: 'gt', value: pageCall.where.id.gt }
                  : { op: 'lt', value: pageCall.where.id.lt }
                : null,
              ascending: pageCall.orderByDirection === 'asc',
              reverseData,
            }
          : null,
        innerCalls: calls,
        innerPageIds: pageCall ? pageCall.returnedIds : null,
        startIndex,
        data: result.data.map((b) => b.id),
        pagination,
        paginationKeys,
      });
    });
};

// -- no cursor --------------------------------------------------------------------------
await addCursor('no cursor, first page of 7', DATASET_7, { limit: 2 });
await addCursor('no cursor, limit larger than the dataset', DATASET_7, { limit: 50 });
await addCursor('no cursor, limit 1', DATASET_7, { limit: 1 });
await addCursor('no cursor, single-row dataset', DATASET_1, { limit: 2 });

// -- after ------------------------------------------------------------------------------
await addCursor('after the first row', DATASET_7, { limit: 2, after: 'b7' });
await addCursor('after a middle row', DATASET_7, { limit: 2, after: 'b6' });
await addCursor('after a middle row, deeper in', DATASET_7, { limit: 2, after: 'b5' });
// b1 alone: startIndex 6, 6 + 1 === total so NO nextCursor -- this is the case where
// upstream's `startIndex + data.length < total` differs from `offset + limit < total`.
await addCursor('after leaving a SHORT final page', DATASET_7, { limit: 2, after: 'b2' });
await addCursor('after leaving an exactly-full final page', DATASET_7, { limit: 3, after: 'b4' });
await addCursor('after with a full page and more to come', DATASET_7, { limit: 3, after: 'b5' });
await addCursor('after the last row -> empty page, cursor echoed back', DATASET_7, {
  limit: 2,
  after: 'b1',
});
await addCursor('after on the single-row dataset -> empty', DATASET_1, { limit: 2, after: 'b1' });

// -- before -----------------------------------------------------------------------------
await addCursor('before a middle row', DATASET_7, { limit: 2, before: 'b4' });
await addCursor('before a middle row, nearer the top', DATASET_7, { limit: 2, before: 'b6' });
await addCursor('before the last row', DATASET_7, { limit: 2, before: 'b1' });
await addCursor('before the last row with limit 3', DATASET_7, { limit: 3, before: 'b1' });
await addCursor('before with a partial page available', DATASET_7, { limit: 3, before: 'b5' });
await addCursor('before limit 1', DATASET_7, { limit: 1, before: 'b4' });
await addCursor('before the first row -> empty page, cursor echoed back', DATASET_7, {
  limit: 2,
  before: 'b7',
});
await addCursor('before on the single-row dataset -> empty', DATASET_1, { limit: 2, before: 'b1' });

// -- both cursors supplied --------------------------------------------------------------
// buildCursorPageQuery tests `cursor.after` first, so `after` wins outright.
await addCursor('after and before together -- after wins', DATASET_7, {
  limit: 2,
  after: 'b6',
  before: 'b2',
});

// -- garbage cursors --------------------------------------------------------------------
// Cursors are raw bundle ids fed into an id gt/lt filter; nothing decodes or validates them.
await addCursor('garbage after cursor sorting above every row', DATASET_7, {
  limit: 2,
  after: 'ZZZZ',
});
await addCursor('garbage after cursor sorting below every row', DATASET_7, {
  limit: 2,
  after: 'c',
});
await addCursor('garbage before cursor sorting below every row', DATASET_7, {
  limit: 2,
  before: '!!!!',
});
await addCursor('garbage before cursor sorting above every row', DATASET_7, {
  limit: 2,
  before: 'c',
});
await addCursor('empty-string after cursor', DATASET_7, { limit: 2, after: '' });
await addCursor('empty-string before cursor', DATASET_7, { limit: 2, before: '' });

// -- page takes precedence over any cursor ----------------------------------------------
await addCursor('page 2 supplied together with a before cursor -- cursor ignored', DATASET_7, {
  limit: 2,
  page: 2,
  before: 'b4',
});
await addCursor('page 2 supplied together with an after cursor -- cursor ignored', DATASET_7, {
  limit: 2,
  page: 2,
  after: 'b4',
});
await addCursor('page past the end is clamped to the last page', DATASET_7, { limit: 2, page: 99 });

// =====================================================================================
// Group 3 -- parsePositiveIntegerSearchParam, via the real HTTP handler
// =====================================================================================

const LIMIT_DEFAULT = 50;
const LIMIT_MAX = 100;

const limitParamCases = [];

const stubApi = {
  lastOptions: null,
  async getBundles(options) {
    stubApi.lastOptions = options;
    return {
      data: [],
      pagination: {
        total: 0,
        hasNextPage: false,
        hasPreviousPage: false,
        currentPage: 1,
        totalPages: 0,
      },
    };
  },
};
const bundlesHandler = createHandler(stubApi, { routes: { bundles: true } });

// Drives the real GET /api/bundles handler and reports either the options it forwarded to the
// api (200) or the 400 body it produced.
const probeBundles = async (queryString) => {
  stubApi.lastOptions = null;
  const response = await bundlesHandler(
    new Request(`http://fixture.invalid/api/api/bundles${queryString}`),
  );
  const body = await response.text();
  return {
    status: response.status,
    options: response.status === 200 ? stubApi.lastOptions : null,
    error: response.status === 200 ? null : JSON.parse(body).error,
  };
};

const addLimitParam = async (description, raw) => {
  const query = raw === null ? '' : `?limit=${encodeURIComponent(raw)}`;
  const { status, options, error } = await probeBundles(query);

  limitParamCases.push({
    description,
    key: 'limit',
    raw,
    defaultValue: LIMIT_DEFAULT,
    maxValue: LIMIT_MAX,
    status,
    expected: status === 200 ? { ok: true, value: options.limit } : { ok: false, message: error },
  });
};

await addLimitParam('absent -> the default', null);
await addLimitParam('an ordinary value', '10');
await addLimitParam('the minimum accepted value', '1');
await addLimitParam('the maximum accepted value', '100');
await addLimitParam('zero is rejected', '0');
await addLimitParam('negative is rejected', '-5');
await addLimitParam('one above the maximum is rejected', '101');
await addLimitParam('far above the maximum is rejected', '1000');
await addLimitParam('a fractional value is rejected', '1.5');
await addLimitParam('a non-numeric value is rejected', 'abc');
await addLimitParam('the empty string is rejected (Number("") is 0)', '');
await addLimitParam('whitespace only is rejected (Number("  ") is 0)', '  ');
await addLimitParam('a leading plus is ACCEPTED (Number("+5") is 5)', '+5');
await addLimitParam('exponent notation is ACCEPTED (Number("1e2") is 100)', '1e2');
await addLimitParam('hexadecimal is ACCEPTED (Number("0x10") is 16)', '0x10');
await addLimitParam('a trailing .0 is ACCEPTED (Number("50.0") is 50)', '50.0');
await addLimitParam('surrounding whitespace is ACCEPTED (Number(" 7 ") is 7)', ' 7 ');
await addLimitParam('leading zeroes are ACCEPTED (Number("007") is 7)', '007');
await addLimitParam('a number beyond the safe-integer range is rejected', '99999999999999999999');
await addLimitParam('Infinity is rejected (not an integer)', 'Infinity');
await addLimitParam('NaN is rejected', 'NaN');
await addLimitParam('negative zero is rejected', '-0');
await addLimitParam('a numeric separator is rejected (Number("1_000") is NaN)', '1_000');
await addLimitParam('a trailing unit is rejected', '10px');

// =====================================================================================
// Group 4 -- the `page` query parameter
//
// Parsed by a DIFFERENT rule from `limit` (handler.mjs:155,157):
//   const page = pageParam === null ? void 0
//     : Number.isInteger(Number(pageParam)) && Number(pageParam) > 0 ? Number(pageParam) : null;
//   if (page === null) throw new HandlerBadRequestError("The 'page' query parameter must be a positive integer.");
// No upper bound, and a different message from the limit parser.
// =====================================================================================

const pageParamCases = [];

const addPageParam = async (description, raw) => {
  const query = raw === null ? '' : `?page=${encodeURIComponent(raw)}`;
  const { status, options, error } = await probeBundles(query);

  const value = status === 200 ? options.page ?? null : null;
  // JS numbers beyond 2^53-1 cannot round-trip through an i64, and upstream saturates them
  // anyway. Such cases pin only the ACCEPT/REJECT decision, never the exact number -- the
  // observable response is unaffected because the offset is clamped to the last page.
  const beyondI64 = value !== null && !Number.isSafeInteger(value);

  pageParamCases.push({
    description,
    raw,
    status,
    beyondI64,
    expected:
      status === 200
        ? { ok: true, value: beyondI64 ? null : value }
        : { ok: false, message: error },
  });
};

await addPageParam('absent -> no page (cursor pagination applies)', null);
await addPageParam('page 1', '1');
await addPageParam('page 2', '2');
await addPageParam('page 50', '50');
await addPageParam('page 101 is ACCEPTED -- page has no upper bound, unlike limit', '101');
await addPageParam('page 1000000 is accepted', '1000000');
await addPageParam('the largest exactly representable page (2^53-1)', '9007199254740991');
await addPageParam('zero is rejected', '0');
await addPageParam('negative is rejected', '-1');
await addPageParam('a fractional value is rejected', '1.5');
await addPageParam('a non-numeric value is rejected', 'abc');
await addPageParam('the empty string is rejected', '');
await addPageParam('whitespace only is rejected', '  ');
await addPageParam('Infinity is rejected (not an integer)', 'Infinity');
await addPageParam('NaN is rejected', 'NaN');
await addPageParam('negative zero is rejected', '-0');
await addPageParam('a leading plus is ACCEPTED', '+5');
await addPageParam('exponent notation is ACCEPTED', '1e2');
await addPageParam('hexadecimal is ACCEPTED', '0x10');
await addPageParam('leading zeroes are ACCEPTED', '007');
await addPageParam('surrounding whitespace is ACCEPTED', ' 7 ');
await addPageParam('a value beyond i64 is ACCEPTED (accept/reject only)', '99999999999999999999');
await addPageParam('an enormous exponent is ACCEPTED (accept/reject only)', '1e30');

// =====================================================================================
// Group 5 -- the `platform` query parameter on GET /bundles
//
// handler.mjs:158 -- anything outside ios/android is a 400, NOT a silent empty result.
// =====================================================================================

const platformParamCases = [];

const addPlatformParam = async (description, raw) => {
  const query = raw === null ? '' : `?platform=${encodeURIComponent(raw)}`;
  const { status, options, error } = await probeBundles(query);

  platformParamCases.push({
    description,
    raw,
    status,
    expected:
      status === 200
        ? { ok: true, value: options.where.platform ?? null }
        : { ok: false, message: error },
  });
};

await addPlatformParam('absent -> no platform filter', null);
await addPlatformParam('ios', 'ios');
await addPlatformParam('android', 'android');
await addPlatformParam('uppercase is rejected', 'IOS');
await addPlatformParam('mixed case is rejected', 'Android');
await addPlatformParam('an unknown platform is rejected', 'web');
await addPlatformParam('the empty string is rejected', '');
await addPlatformParam('surrounding whitespace is rejected', ' ios ');
await addPlatformParam('a comma-separated pair is rejected', 'ios,android');
await addPlatformParam('a near miss is rejected', 'iOS');

// =====================================================================================
// Group 6 -- repeated array query parameters (`idIn`, `targetAppVersionIn`)
//
// handler.mjs:60-63 -- parseStringArraySearchParam uses url.searchParams.getAll(key), so
// these are REPEATED parameters (?idIn=a&idIn=b), never one comma-separated string.
// `?idIn=a,b` is a single id that happens to contain a comma.
// =====================================================================================

const stringArrayParamCases = [];

const addStringArrayParam = async (description, key, query) => {
  const { status, options, error } = await probeBundles(query);

  let value = null;
  if (status === 200) {
    value = key === 'idIn' ? options.where.id?.in ?? null : options.where.targetAppVersionIn ?? null;
  }

  stringArrayParamCases.push({
    description,
    key,
    query,
    status,
    expected: status === 200 ? { ok: true, value } : { ok: false, message: error },
  });
};

await addStringArrayParam('idIn absent -> no filter', 'idIn', '');
await addStringArrayParam('idIn once', 'idIn', '?idIn=a');
await addStringArrayParam('idIn twice -> two ids', 'idIn', '?idIn=a&idIn=b');
await addStringArrayParam('idIn three times -> three ids', 'idIn', '?idIn=a&idIn=b&idIn=c');
await addStringArrayParam(
  'idIn with a comma -> ONE id containing a literal comma, not two ids',
  'idIn',
  '?idIn=a,b',
);
await addStringArrayParam(
  'idIn with a percent-encoded comma -> identical to the raw comma',
  'idIn',
  '?idIn=a%2Cb',
);
await addStringArrayParam('idIn empty value -> a one-element array holding ""', 'idIn', '?idIn=');
await addStringArrayParam('idIn with one real and one empty value', 'idIn', '?idIn=a&idIn=');
await addStringArrayParam('idIn repeated with the same value -> duplicates kept', 'idIn', '?idIn=a&idIn=a');
await addStringArrayParam('idIn order is preserved as supplied', 'idIn', '?idIn=c&idIn=a&idIn=b');
await addStringArrayParam('idIn alongside idEq -> both survive', 'idIn', '?idEq=z&idIn=a&idIn=b');
await addStringArrayParam(
  'idIn with a uuid-shaped comma-separated pair -> still one value',
  'idIn',
  '?idIn=00000000-0000-0000-0000-000000000001,00000000-0000-0000-0000-000000000002',
);
await addStringArrayParam('targetAppVersionIn absent -> no filter', 'targetAppVersionIn', '');
await addStringArrayParam('targetAppVersionIn once', 'targetAppVersionIn', '?targetAppVersionIn=1.0.0');
await addStringArrayParam(
  'targetAppVersionIn twice -> two ranges',
  'targetAppVersionIn',
  '?targetAppVersionIn=1.0.0&targetAppVersionIn=2.0.0',
);
await addStringArrayParam(
  'targetAppVersionIn with a comma -> ONE range containing a comma',
  'targetAppVersionIn',
  '?targetAppVersionIn=1.0.0,2.0.0',
);
await addStringArrayParam(
  'targetAppVersionIn with a range that legitimately contains spaces',
  'targetAppVersionIn',
  '?targetAppVersionIn=%3E%3D1.0.0%20%3C2.0.0',
);
await addStringArrayParam('targetAppVersionIn empty value', 'targetAppVersionIn', '?targetAppVersionIn=');

// =====================================================================================
// Group 7 -- boolean query parameters (`enabled`, `targetAppVersionNotNull`)
//
// handler.mjs:47-54 -- parseBooleanSearchParam accepts ONLY the exact strings "true" and
// "false"; anything else is a 400 naming the offending key. No 1/0, no yes/no, no case
// folding, no trimming.
// =====================================================================================

const booleanParamCases = [];

const addBooleanParam = async (description, key, raw) => {
  const query = raw === null ? '' : `?${key}=${encodeURIComponent(raw)}`;
  const { status, options, error } = await probeBundles(query);

  let value = null;
  if (status === 200) {
    value = (key === 'enabled' ? options.where.enabled : options.where.targetAppVersionNotNull) ?? null;
  }

  booleanParamCases.push({
    description,
    key,
    raw,
    status,
    expected: status === 200 ? { value } : { message: error },
  });
};

await addBooleanParam('enabled absent -> no filter', 'enabled', null);
await addBooleanParam('enabled true', 'enabled', 'true');
await addBooleanParam('enabled false', 'enabled', 'false');
await addBooleanParam('enabled uppercase TRUE is rejected', 'enabled', 'TRUE');
await addBooleanParam('enabled capitalised True is rejected', 'enabled', 'True');
await addBooleanParam('enabled 1 is rejected', 'enabled', '1');
await addBooleanParam('enabled 0 is rejected', 'enabled', '0');
await addBooleanParam('enabled yes is rejected', 'enabled', 'yes');
await addBooleanParam('enabled no is rejected', 'enabled', 'no');
await addBooleanParam('enabled on is rejected', 'enabled', 'on');
await addBooleanParam('enabled the empty string is rejected', 'enabled', '');
await addBooleanParam('enabled whitespace-padded true is rejected (no trimming)', 'enabled', ' true ');
await addBooleanParam('enabled null is rejected', 'enabled', 'null');
await addBooleanParam('enabled garbage is rejected', 'enabled', 'maybe');
await addBooleanParam('targetAppVersionNotNull absent -> no filter', 'targetAppVersionNotNull', null);
await addBooleanParam('targetAppVersionNotNull true', 'targetAppVersionNotNull', 'true');
await addBooleanParam('targetAppVersionNotNull false', 'targetAppVersionNotNull', 'false');
await addBooleanParam(
  'targetAppVersionNotNull rejection names ITS OWN key, not enabled',
  'targetAppVersionNotNull',
  'yes',
);
await addBooleanParam(
  'targetAppVersionNotNull uppercase is rejected',
  'targetAppVersionNotNull',
  'FALSE',
);
// These are the shapes that would wrongly pass if someone assumed symmetry with
// parsePositiveIntegerSearchParam, which runs everything through JS Number() and therefore
// DOES tolerate surrounding whitespace and alternative numeric spellings. The boolean parser
// does no coercion and no trimming whatsoever: it is a bare === against two literals.
await addBooleanParam('enabled trailing space is rejected (no trimming)', 'enabled', 'true ');
await addBooleanParam('enabled leading space is rejected (no trimming)', 'enabled', ' true');
await addBooleanParam('enabled tab-padded is rejected (no trimming)', 'enabled', '\ttrue');
await addBooleanParam('enabled mixed case is rejected (no case folding)', 'enabled', 'TrUe');
await addBooleanParam('enabled false with trailing space is rejected', 'enabled', 'false ');
await addBooleanParam('enabled numeric 1.0 is rejected (no Number coercion)', 'enabled', '1.0');
await addBooleanParam('enabled +1 is rejected (no Number coercion)', 'enabled', '+1');

// =====================================================================================
// Group 8 -- nullable string parameters (`targetAppVersion`, `fingerprintHash`)
//
// handler.mjs:55-59 -- parseNullableStringSearchParam returns undefined when the parameter
// is absent, SQL NULL for the EXACT lowercase string "null", and the raw value otherwise.
// Both fields are then folded in with `!== undefined` (handler.mjs:173,178), so an empty
// string is a REAL filter here. Contrast group 9.
// =====================================================================================

const nullableStringParamCases = [];

const addNullableStringParam = async (description, key, raw) => {
  const query = raw === null ? '' : `?${key}=${encodeURIComponent(raw)}`;
  const { status, options, error } = await probeBundles(query);

  let present = false;
  let value = null;
  if (status === 200) {
    present = Object.prototype.hasOwnProperty.call(options.where, key);
    value = present ? options.where[key] : null;
  }

  nullableStringParamCases.push({
    description,
    key,
    raw,
    status,
    // `present` distinguishes "no filter at all" from "a filter whose value is SQL NULL";
    // a bare `value: null` cannot express that difference on its own.
    expected: status === 200 ? { present, value } : { message: error },
  });
};

await addNullableStringParam('targetAppVersion absent -> no filter at all', 'targetAppVersion', null);
await addNullableStringParam(
  'targetAppVersion=null -> SQL NULL, not the literal string',
  'targetAppVersion',
  'null',
);
await addNullableStringParam(
  'targetAppVersion=NULL (uppercase) -> the LITERAL string, matching is case sensitive',
  'targetAppVersion',
  'NULL',
);
await addNullableStringParam('targetAppVersion=Null -> the literal string', 'targetAppVersion', 'Null');
await addNullableStringParam(
  'targetAppVersion= (empty) -> a REAL filter on the empty string, not "absent"',
  'targetAppVersion',
  '',
);
await addNullableStringParam('targetAppVersion with an ordinary value', 'targetAppVersion', '1.0.0');
await addNullableStringParam('targetAppVersion with a range value', 'targetAppVersion', '>=1.0.0 <2.0.0');
await addNullableStringParam(
  'targetAppVersion=" null " -> whitespace-padded, so the literal string (no trimming)',
  'targetAppVersion',
  ' null ',
);
await addNullableStringParam('targetAppVersion=nul -> the literal string', 'targetAppVersion', 'nul');
await addNullableStringParam('targetAppVersion=nulll -> the literal string', 'targetAppVersion', 'nulll');
await addNullableStringParam('fingerprintHash absent -> no filter at all', 'fingerprintHash', null);
await addNullableStringParam(
  'fingerprintHash=null -> SQL NULL, the SAME rule as targetAppVersion',
  'fingerprintHash',
  'null',
);
await addNullableStringParam(
  'fingerprintHash=NULL (uppercase) -> the literal string',
  'fingerprintHash',
  'NULL',
);
await addNullableStringParam(
  'fingerprintHash= (empty) -> a REAL filter on the empty string',
  'fingerprintHash',
  '',
);
await addNullableStringParam('fingerprintHash with an ordinary value', 'fingerprintHash', 'fp-abc');

// =====================================================================================
// Group 9 -- JS-truthy string parameters (`channel`)
//
// handler.mjs:170 folds channel in with `...channel && { channel }`, NOT `!== undefined`.
// So `?channel=` drops the filter entirely, while `?channel=0` and `?channel=%20` keep it
// (non-empty strings are truthy in JS). And `?channel=null` is the LITERAL string "null" --
// channel does not go through parseNullableStringSearchParam at all.
//
// Two different emptiness rules in the same handler; these cases keep them apart.
// =====================================================================================

const truthyStringParamCases = [];

const addTruthyStringParam = async (description, key, raw) => {
  const query = raw === null ? '' : `?${key}=${encodeURIComponent(raw)}`;
  const { status, options, error } = await probeBundles(query);

  let present = false;
  let value = null;
  if (status === 200) {
    present = Object.prototype.hasOwnProperty.call(options.where, key);
    value = present ? options.where[key] : null;
  }

  truthyStringParamCases.push({
    description,
    key,
    raw,
    status,
    expected: status === 200 ? { present, value } : { message: error },
  });
};

await addTruthyStringParam('channel absent -> no filter', 'channel', null);
await addTruthyStringParam(
  'channel= (empty) -> filter DROPPED, unlike targetAppVersion which keeps it',
  'channel',
  '',
);
await addTruthyStringParam('channel with an ordinary value', 'channel', 'production');
await addTruthyStringParam(
  'channel=null -> the LITERAL string, NOT SQL NULL -- channel is not a nullable-string param',
  'channel',
  'null',
);
await addTruthyStringParam('channel=0 -> kept ("0" is a truthy STRING in JS)', 'channel', '0');
await addTruthyStringParam('channel=" " (space) -> kept, a non-empty string is truthy', 'channel', ' ');
await addTruthyStringParam('channel=false -> kept, it is just a string', 'channel', 'false');

// =====================================================================================

const output = {
  paginationCases,
  cursorCases,
  limitParamCases,
  pageParamCases,
  platformParamCases,
  stringArrayParamCases,
  booleanParamCases,
  nullableStringParamCases,
  truthyStringParamCases,
};
process.stdout.write(JSON.stringify(output, null, 2));
process.stdout.write('\n');
