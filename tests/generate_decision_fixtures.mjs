// Calls the real @hot-updater/js package and records the real input/output pairs of the
// update-check decision algorithm (appVersionStrategy/fingerprintStrategy). The Rust
// side's `decide_update` is tested against these fixtures (tests/decision_tests.rs).
//
// Re-run whenever the hot-updater packages are upgraded:
//   node tests/generate_decision_fixtures.mjs > tests/fixtures/decision_fixtures.json
//
// The case matrix below is derived from the upstream source (getUpdateInfo.ts, compiled into
// node_modules/@hot-updater/js/dist/index.mjs), branch by branch:
//
//   1. candidate filter: platform / channel / targetAppVersion|fingerprintHash /
//      enabled / minBundleId
//   2. empty candidate list -> null (NIL bundleId or bundleId <= minBundleId) else INIT ROLLBACK
//   3. currentBundle  = candidate whose id === bundleId
//      rollbackCandidate = highest-id candidate < bundleId  (NO cohort eligibility check!)
//      updateCandidate   = highest-id candidate > bundleId  WITH cohort eligibility check
//   4. NIL bundleId        -> UPDATE(updateCandidate) or null
//      current eligible    -> UPDATE(updateCandidate) or null
//      otherwise           -> UPDATE || ROLLBACK || (bundleId <= minBundleId ? null : INIT)
//
// Output is deterministic: fixed case order, no timestamps, no randomness.

// Node's ESM resolver ignores NODE_PATH, so a bare `import '@hot-updater/js'` only works when
// a node_modules containing it sits on the path from this file to the filesystem root. Fall
// back to the tools/fixture-gen install that exists precisely to provide the package.
const importUpstream = async () => {
  try {
    return await import('@hot-updater/js');
  } catch (err) {
    if (err?.code !== 'ERR_MODULE_NOT_FOUND') throw err;
    return import(
      new URL('../tools/fixture-gen/node_modules/@hot-updater/js/dist/index.mjs', import.meta.url)
    );
  }
};

const { getUpdateInfo } = await importUpstream();

const NIL = '00000000-0000-0000-0000-000000000000';

// Bundle ids are ordinary lowercase UUIDs differing only in the last hex digits, so that
// their ordering is obvious from the case description.
const uuid = (suffix) => `00000000-0000-0000-0000-${String(suffix).padStart(12, '0')}`;

const B1 = uuid(1);
const B2 = uuid(2);
const B3 = uuid(3);
const B4 = uuid(4);
const B9 = uuid(9);
const BA = uuid('a');
const BA_UPPER = uuid('A');

const bundle = (id, overrides = {}) => ({
  id,
  platform: 'android',
  shouldForceUpdate: false,
  enabled: true,
  fileHash: `hash-${id}`,
  storageUri: `s3://bucket/${id}/bundle.zip`,
  gitCommitHash: null,
  message: null,
  channel: 'production',
  targetAppVersion: '*',
  fingerprintHash: null,
  rolloutCohortCount: 1000,
  targetCohorts: null,
  manifestStorageUri: null,
  manifestFileHash: null,
  assetBaseStorageUri: null,
  patches: [],
  ...overrides,
});

// A fingerprint-strategy bundle: no targetAppVersion, matching fingerprintHash.
const fpBundle = (id, overrides = {}) =>
  bundle(id, { targetAppVersion: null, fingerprintHash: 'fp-abc', ...overrides });

const cases = [];

const addCase = (description, strategy, bundles, args) => {
  cases.push({ description, strategy, bundles, args });
};

const addAppVersion = (description, bundles, args) =>
  addCase(description, 'appVersion', bundles, args);

const addFingerprint = (description, bundles, args) =>
  addCase(description, 'fingerprint', bundles, args);

// =====================================================================================
// A. appVersion strategy -- the core state machine
// =====================================================================================

addAppVersion('A01 fresh install with no bundles at all -> no update', [], {
  bundleId: NIL,
  minBundleId: NIL,
});

addAppVersion('A02 fresh install receives the only eligible bundle', [bundle(B1)], {
  bundleId: NIL,
  minBundleId: NIL,
});

addAppVersion(
  'A03 fresh install receives the newest of three eligible bundles',
  [bundle(B1), bundle(B2), bundle(B3)],
  { bundleId: NIL, minBundleId: NIL },
);

addAppVersion(
  'A04 fresh install where every bundle is outside the rollout -> no update',
  [bundle(B1, { rolloutCohortCount: 0 }), bundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: NIL, minBundleId: NIL },
);

addAppVersion(
  'A05 current bundle is the newest and eligible -> no update',
  [bundle(B1), bundle(B2)],
  { bundleId: B2, minBundleId: NIL },
);

addAppVersion(
  'A06 current bundle eligible with a newer candidate -> UPDATE to the newest',
  [bundle(B1), bundle(B2), bundle(B3)],
  { bundleId: B1, minBundleId: NIL },
);

addAppVersion(
  'A07 current eligible, newest candidate outside the rollout -> UPDATE to the middle one',
  [bundle(B1), bundle(B2), bundle(B3, { rolloutCohortCount: 0 })],
  { bundleId: B1, minBundleId: NIL },
);

addAppVersion(
  'A08 current bundle outside the rollout, newer eligible candidate -> UPDATE wins over ROLLBACK',
  [bundle(B1), bundle(B2, { rolloutCohortCount: 0 }), bundle(B3)],
  { bundleId: B2, minBundleId: NIL },
);

addAppVersion(
  'A09 current bundle outside the rollout, only an older eligible bundle -> ROLLBACK',
  [bundle(B1), bundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: NIL },
);

// Upstream picks the rollback candidate WITHOUT any cohort/rollout eligibility check --
// only `id < bundleId` matters. This is the sharpest discriminator in the whole matrix.
addAppVersion(
  'A10 current AND older bundle both outside the rollout -> ROLLBACK anyway (rollback ignores cohort)',
  [bundle(B1, { rolloutCohortCount: 0 }), bundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: NIL },
);

addAppVersion(
  'A11 two older bundles, both outside the rollout -> ROLLBACK to the higher id',
  [
    bundle(B1, { rolloutCohortCount: 0 }),
    bundle(B2, { rolloutCohortCount: 0 }),
    bundle(B3, { rolloutCohortCount: 0 }),
  ],
  { bundleId: B3, minBundleId: NIL },
);

addAppVersion(
  'A12 older bundle is outside the rollout but the newer older-one is eligible -> ROLLBACK to the highest older',
  [bundle(B1), bundle(B2, { rolloutCohortCount: 0 }), bundle(B3, { rolloutCohortCount: 0 })],
  { bundleId: B3, minBundleId: NIL },
);

addAppVersion(
  'A13 current outside the rollout, no older bundle, bundleId above minBundleId -> INIT rollback',
  [bundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: NIL },
);

addAppVersion(
  'A14 current outside the rollout, no older bundle, bundleId equals minBundleId -> no update',
  [bundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: B2 },
);

addAppVersion(
  'A15 update candidate carries shouldForceUpdate -> forced UPDATE',
  [bundle(B1), bundle(B2, { shouldForceUpdate: true })],
  { bundleId: B1, minBundleId: NIL },
);

addAppVersion(
  'A16 rollback target has shouldForceUpdate=false -> ROLLBACK is forced regardless',
  [bundle(B1, { shouldForceUpdate: false }), bundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: NIL },
);

addAppVersion(
  'A17 rollback target has shouldForceUpdate=true -> ROLLBACK still forced',
  [bundle(B1, { shouldForceUpdate: true }), bundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: NIL },
);

addAppVersion(
  'A18 update candidate shouldForceUpdate=false is reported verbatim',
  [bundle(B1), bundle(B2, { shouldForceUpdate: false })],
  { bundleId: B1, minBundleId: NIL },
);

addAppVersion(
  'A19 the client is on a bundle that is not in the candidate set at all -> INIT rollback',
  [bundle(B1)],
  { bundleId: B4, minBundleId: NIL },
);

addAppVersion(
  'A20 the client is on an unknown bundle but an older eligible one exists -> ROLLBACK',
  [bundle(B1), bundle(B2)],
  { bundleId: B4, minBundleId: NIL },
);

addAppVersion(
  'A21 the client is on an unknown bundle and a newer one exists -> UPDATE',
  [bundle(B1), bundle(B9)],
  { bundleId: B4, minBundleId: NIL },
);

// =====================================================================================
// B. candidate filtering: enabled / platform / channel
// =====================================================================================

addAppVersion(
  'B01 every bundle disabled, fresh install -> no update',
  [bundle(B1, { enabled: false }), bundle(B2, { enabled: false })],
  { bundleId: NIL, minBundleId: NIL },
);

addAppVersion(
  'B02 every bundle disabled, client above minBundleId -> INIT rollback',
  [bundle(B1, { enabled: false }), bundle(B2, { enabled: false })],
  { bundleId: B2, minBundleId: NIL },
);

addAppVersion(
  'B03 newest bundle disabled -> UPDATE to the newest enabled one',
  [bundle(B1), bundle(B2), bundle(B3, { enabled: false })],
  { bundleId: B1, minBundleId: NIL },
);

addAppVersion(
  'B04 the current bundle was disabled, an older enabled one exists -> ROLLBACK',
  [bundle(B1), bundle(B2, { enabled: false })],
  { bundleId: B2, minBundleId: NIL },
);

addAppVersion(
  'B05 disabled bundle is not a rollback target either',
  [bundle(B1, { enabled: false }), bundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: NIL },
);

addAppVersion(
  'B06 every bundle is for the other platform -> no update on a fresh install',
  [bundle(B1, { platform: 'ios' }), bundle(B2, { platform: 'ios' })],
  { bundleId: NIL, minBundleId: NIL, platform: 'android' },
);

addAppVersion(
  'B07 newest bundle belongs to the other platform -> UPDATE to the matching one',
  [bundle(B1), bundle(B2), bundle(B3, { platform: 'ios' })],
  { bundleId: B1, minBundleId: NIL, platform: 'android' },
);

addAppVersion(
  'B08 ios client only sees ios bundles',
  [bundle(B1, { platform: 'android' }), bundle(B2, { platform: 'ios' }), bundle(B3, { platform: 'android' })],
  { bundleId: NIL, minBundleId: NIL, platform: 'ios' },
);

addAppVersion(
  'B09 platform mismatch removes the current bundle -> INIT rollback',
  [bundle(B2, { platform: 'ios' })],
  { bundleId: B2, minBundleId: NIL, platform: 'android' },
);

addAppVersion(
  'B10 every bundle is on another channel -> no update on a fresh install',
  [bundle(B1, { channel: 'staging' }), bundle(B2, { channel: 'staging' })],
  { bundleId: NIL, minBundleId: NIL, channel: 'production' },
);

addAppVersion(
  'B11 newest bundle is on another channel -> UPDATE to the newest same-channel bundle',
  [bundle(B1), bundle(B2), bundle(B3, { channel: 'staging' })],
  { bundleId: B1, minBundleId: NIL, channel: 'production' },
);

addAppVersion(
  'B12 client on the staging channel gets the staging bundle',
  [bundle(B1, { channel: 'production' }), bundle(B2, { channel: 'staging' }), bundle(B3, { channel: 'production' })],
  { bundleId: NIL, minBundleId: NIL, channel: 'staging' },
);

addAppVersion(
  'B13 channel is case sensitive -> Production does not match production',
  [bundle(B1, { channel: 'Production' })],
  { bundleId: NIL, minBundleId: NIL, channel: 'production' },
);

addAppVersion(
  'B14 channel mismatch hides the only rollback target -> INIT rollback',
  [bundle(B1, { channel: 'staging' }), bundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: NIL, channel: 'production' },
);

// =====================================================================================
// C. minBundleId boundaries
// =====================================================================================

addAppVersion(
  'C01 bundle whose id is exactly minBundleId is still a candidate (>= not >)',
  [bundle(B2)],
  { bundleId: NIL, minBundleId: B2 },
);

addAppVersion(
  'C02 bundles below minBundleId are filtered out -> UPDATE only from the allowed range',
  [bundle(B1), bundle(B2), bundle(B3)],
  { bundleId: NIL, minBundleId: B2 },
);

addAppVersion(
  'C03 minBundleId hides the only rollback target -> INIT rollback',
  [bundle(B1), bundle(B3, { rolloutCohortCount: 0 })],
  { bundleId: B3, minBundleId: B3 },
);

addAppVersion(
  'C04 minBundleId hides the older bundles but bundleId equals minBundleId -> no update',
  [bundle(B1), bundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: B2 },
);

addAppVersion(
  'C05 bundleId one step above minBundleId with no candidates -> INIT rollback',
  [],
  { bundleId: B2, minBundleId: B1 },
);

addAppVersion(
  'C06 bundleId below minBundleId with no candidates -> no update',
  [],
  { bundleId: B1, minBundleId: B2 },
);

addAppVersion(
  'C07 bundleId exactly equal to minBundleId with no candidates -> no update',
  [],
  { bundleId: B2, minBundleId: B2 },
);

addAppVersion(
  'C08 fresh install (NIL bundleId) with no candidates and a real minBundleId -> no update',
  [],
  { bundleId: NIL, minBundleId: B2 },
);

addAppVersion(
  'C09 minBundleId is the NIL uuid so nothing is filtered out',
  [bundle(B1), bundle(B2)],
  { bundleId: B1, minBundleId: NIL },
);

addAppVersion(
  'C10 minBundleId equals the newest bundle -> rollback range is empty, INIT rollback',
  [bundle(B1), bundle(B2), bundle(B3, { rolloutCohortCount: 0 })],
  { bundleId: B3, minBundleId: B3 },
);

addAppVersion(
  'C11 minBundleId between two bundles -> ROLLBACK only reaches the allowed one',
  [bundle(B1), bundle(B2), bundle(B3, { rolloutCohortCount: 0 })],
  { bundleId: B3, minBundleId: B2 },
);

addAppVersion(
  'C12 empty-string minBundleId disables the range filter entirely',
  [bundle(B1), bundle(B2)],
  { bundleId: B1, minBundleId: '' },
);

addAppVersion(
  'C13 empty-string minBundleId with no candidates -> INIT rollback',
  [],
  { bundleId: B2, minBundleId: '' },
);

// =====================================================================================
// D. ordering and ties -- only the id ordering decides
// =====================================================================================

addAppVersion(
  'D01 bundles supplied in ascending order still yield the newest',
  [bundle(B1), bundle(B2), bundle(B3), bundle(B4)],
  { bundleId: NIL, minBundleId: NIL },
);

addAppVersion(
  'D02 bundles supplied in shuffled order still yield the newest',
  [bundle(B3), bundle(B1), bundle(B4), bundle(B2)],
  { bundleId: NIL, minBundleId: NIL },
);

addAppVersion(
  'D03 rollback target is the highest id below the client, not the lowest',
  [bundle(B1), bundle(B2), bundle(B3), bundle(B4, { rolloutCohortCount: 0 })],
  { bundleId: B4, minBundleId: NIL },
);

addAppVersion(
  'D04 update target is the highest eligible id above the client, not the nearest',
  [bundle(B1), bundle(B2), bundle(B3), bundle(B4)],
  { bundleId: B1, minBundleId: NIL },
);

addAppVersion(
  'D05 id ordering crosses the 9 -> a hex boundary',
  [bundle(B9), bundle(BA)],
  { bundleId: NIL, minBundleId: NIL },
);

addAppVersion(
  'D06 id ordering crosses the 9 -> a hex boundary while rolling back',
  [bundle(B9), bundle(BA, { rolloutCohortCount: 0 })],
  { bundleId: BA, minBundleId: NIL },
);

// Upstream compares ids with String.prototype.localeCompare, which is NOT byte order for
// mixed-case ids (ICU sorts lowercase before uppercase; ASCII sorts the other way round).
addAppVersion(
  'D07 ids differing only in hex letter case -- upstream orders them with localeCompare',
  [bundle(BA), bundle(BA_UPPER)],
  { bundleId: NIL, minBundleId: NIL },
);

// =====================================================================================
// E. targetAppVersion semver ranges
// =====================================================================================

const semverCase = (label, targetAppVersion, appVersion) =>
  addAppVersion(`E ${label}`, [bundle(B1, { targetAppVersion })], {
    bundleId: NIL,
    minBundleId: NIL,
    appVersion,
  });

semverCase('01 star range matches any version', '*', '1.2.3');
semverCase('02 >= matches an equal version', '>=1.0.0', '1.0.0');
semverCase('03 >= rejects a lower version', '>=2.0.0', '1.5.0');
semverCase('04 >= accepts a higher version', '>=1.0.0', '3.4.5');
semverCase('05 tilde accepts a patch bump', '~1.2.0', '1.2.9');
semverCase('06 tilde rejects a minor bump', '~1.2.0', '1.3.0');
semverCase('07 caret accepts a minor bump', '^1.2.0', '1.9.9');
semverCase('08 caret rejects a major bump', '^1.2.0', '2.0.0');
semverCase('09 caret on 0.x is minor-locked', '^0.2.0', '0.3.0');
semverCase('10 x-range on the minor position', '1.x', '1.4.2');
semverCase('11 x-range on the patch position', '1.2.x', '1.2.0');
semverCase('12 x-range on the patch position rejects another minor', '1.2.x', '1.3.0');
semverCase('13 range that matches nothing realistic', '>=99.0.0', '1.0.0');
semverCase('14 exact pin matches', '1.2.3', '1.2.3');
semverCase('15 exact pin rejects a patch bump', '1.2.3', '1.2.4');
semverCase('16 hyphen range includes the middle', '1.0.0 - 2.0.0', '1.5.0');
semverCase('17 hyphen range is inclusive at the top', '1.0.0 - 2.0.0', '2.0.0');
semverCase('18 hyphen range excludes above the top', '1.0.0 - 2.0.0', '2.0.1');
semverCase('19 or-compound range matches the second alternative', '1.0.0 || >=2.0.0', '2.5.0');
semverCase('20 or-compound range matches neither alternative', '1.0.0 || >=3.0.0', '2.5.0');
semverCase('21 and-compound range', '>=1.0.0 <2.0.0', '1.9.0');
semverCase('22 and-compound range at the exclusive bound', '>=1.0.0 <2.0.0', '2.0.0');
// coerce() strips prereleases and build metadata from the CLIENT version before matching.
semverCase('23 prerelease client version is coerced before matching', '1.0.0', '1.0.0-beta.1');
semverCase('24 prerelease client version against a prerelease range', '>=1.0.0-alpha', '1.0.0-beta.2');
semverCase('25 prerelease-only pin never matches a coerced version', '1.0.0-beta.1', '1.0.0');
semverCase('26 build metadata is stripped from the client version', '~1.2.0', '1.2.3-rc.1+build.5');
semverCase('27 v-prefixed client version', '>=1.2.0', 'v1.2.3');
semverCase('28 two-segment client version coerces to x.y.0', '~1.2.0', '1.2');
semverCase('29 one-segment client version coerces to x.0.0', '^1.0.0', '1');
semverCase('30 four-segment client version keeps only the first three', '~1.2.3', '1.2.3.4');
semverCase('31 client version with leading zeroes', '>=1.0.0', '01.2.3');
semverCase('32 range with a leading v', 'v1.2.3', '1.2.3');
semverCase('33 whitespace-padded range', '  >=1.0.0  ', '1.5.0');

addAppVersion(
  'E34 targetAppVersion null excludes the bundle entirely',
  [bundle(B1, { targetAppVersion: null })],
  { bundleId: NIL, minBundleId: NIL, appVersion: '1.0.0' },
);

addAppVersion(
  'E35 empty-string targetAppVersion is falsy upstream and excludes the bundle',
  [bundle(B1, { targetAppVersion: '' })],
  { bundleId: NIL, minBundleId: NIL, appVersion: '1.0.0' },
);

addAppVersion(
  'E36 semver removes the newest bundle -> UPDATE to the older matching one',
  [bundle(B1, { targetAppVersion: '1.x' }), bundle(B2, { targetAppVersion: '>=2.0.0' })],
  { bundleId: NIL, minBundleId: NIL, appVersion: '1.5.0' },
);

addAppVersion(
  'E37 semver removes the current bundle -> it is neither current nor rollback target',
  [bundle(B1, { targetAppVersion: '1.x' }), bundle(B2, { targetAppVersion: '>=2.0.0' })],
  { bundleId: B2, minBundleId: NIL, appVersion: '1.5.0' },
);

addAppVersion(
  'E38 semver removes every bundle above minBundleId -> INIT rollback',
  [bundle(B1, { targetAppVersion: '>=2.0.0' }), bundle(B2, { targetAppVersion: '>=2.0.0' })],
  { bundleId: B2, minBundleId: NIL, appVersion: '1.5.0' },
);

addAppVersion(
  'E39 mixed ranges where only the middle bundle matches',
  [
    bundle(B1, { targetAppVersion: '>=2.0.0' }),
    bundle(B2, { targetAppVersion: '^1.0.0' }),
    bundle(B3, { targetAppVersion: '~2.1.0' }),
  ],
  { bundleId: NIL, minBundleId: NIL, appVersion: '1.4.0' },
);

// =====================================================================================
// F. rollout (rolloutCohortCount) and cohorts (targetCohorts)
// =====================================================================================

// Rollout without a client cohort: only a 100% rollout is eligible.
const rolloutNoCohort = (label, rolloutCohortCount) =>
  addAppVersion(`F ${label}`, [bundle(B1, { rolloutCohortCount })], {
    bundleId: NIL,
    minBundleId: NIL,
  });

rolloutNoCohort('01 rollout 1000 without a cohort is eligible', 1000);
rolloutNoCohort('02 rollout 999 without a cohort is NOT eligible', 999);
rolloutNoCohort('03 rollout 500 without a cohort is NOT eligible', 500);
rolloutNoCohort('04 rollout 1 without a cohort is NOT eligible', 1);
rolloutNoCohort('05 rollout 0 without a cohort is NOT eligible', 0);
rolloutNoCohort('06 negative rollout clamps to 0', -5);
rolloutNoCohort('07 rollout above 1000 clamps to 1000', 5000);

// Rollout with a numeric client cohort: the bundle-id-seeded shuffle decides.
const rolloutWithCohort = (label, rolloutCohortCount, cohort) =>
  addAppVersion(`F ${label}`, [bundle(B1, { rolloutCohortCount })], {
    bundleId: NIL,
    minBundleId: NIL,
    cohort,
  });

rolloutWithCohort('08 rollout 1000 with a numeric cohort is always eligible', 1000, '42');
rolloutWithCohort('09 rollout 0 with a numeric cohort is never eligible', 0, '42');
rolloutWithCohort('10 rollout 1 with cohort 1', 1, '1');
rolloutWithCohort('11 rollout 1 with cohort 42', 1, '42');
rolloutWithCohort('12 rollout 1 with cohort 1000', 1, '1000');
rolloutWithCohort('13 rollout 500 with cohort 42', 500, '42');
rolloutWithCohort('14 rollout 500 with cohort 777', 500, '777');
rolloutWithCohort('15 rollout 500 with cohort 1', 500, '1');
rolloutWithCohort('16 rollout 500 with cohort 1000', 500, '1000');
rolloutWithCohort('17 rollout 999 with cohort 42', 999, '42');
rolloutWithCohort('18 rollout 999 with cohort 777', 999, '777');
rolloutWithCohort('19 cohort 0 is not a valid numeric cohort', 500, '0');
rolloutWithCohort('20 cohort 1001 is out of the numeric range', 500, '1001');
rolloutWithCohort('21 whitespace-padded numeric cohort is normalized', 1000, ' 42 ');
rolloutWithCohort('22 zero-padded numeric cohort is normalized', 500, '042');
rolloutWithCohort('23 non-numeric cohort with a partial rollout is never eligible', 500, 'beta-testers');
rolloutWithCohort('24 non-numeric cohort with a full rollout is eligible', 1000, 'beta-testers');
rolloutWithCohort('25 non-numeric cohort with rollout 999 is NOT eligible', 999, 'beta-testers');

addAppVersion(
  'F26 targetCohorts match overrides a 0% rollout',
  [bundle(B1, { rolloutCohortCount: 0, targetCohorts: ['beta-testers'] })],
  { bundleId: NIL, minBundleId: NIL, cohort: 'beta-testers' },
);

addAppVersion(
  'F27 targetCohorts without a matching cohort leaves the 0% rollout in force',
  [bundle(B1, { rolloutCohortCount: 0, targetCohorts: ['beta-testers'] })],
  { bundleId: NIL, minBundleId: NIL, cohort: 'other-group' },
);

addAppVersion(
  'F28 targetCohorts present but the client sends no cohort at all',
  [bundle(B1, { rolloutCohortCount: 0, targetCohorts: ['beta-testers'] })],
  { bundleId: NIL, minBundleId: NIL },
);

addAppVersion(
  'F29 empty targetCohorts array behaves like none',
  [bundle(B1, { rolloutCohortCount: 0, targetCohorts: [] })],
  { bundleId: NIL, minBundleId: NIL, cohort: 'beta-testers' },
);

addAppVersion(
  'F30 targetCohorts entries are normalized (case and whitespace)',
  [bundle(B1, { rolloutCohortCount: 0, targetCohorts: ['  BETA-Testers '] })],
  { bundleId: NIL, minBundleId: NIL, cohort: 'beta-testers' },
);

addAppVersion(
  'F31 numeric targetCohorts entries are normalized (007 matches 7)',
  [bundle(B1, { rolloutCohortCount: 0, targetCohorts: ['007'] })],
  { bundleId: NIL, minBundleId: NIL, cohort: '7' },
);

addAppVersion(
  'F32 targetCohorts with several entries, one of which matches',
  [bundle(B1, { rolloutCohortCount: 0, targetCohorts: ['alpha', 'beta-testers', 'gamma'] })],
  { bundleId: NIL, minBundleId: NIL, cohort: 'gamma' },
);

addAppVersion(
  'F33 explicit cohort targeting promotes the newest bundle past a 0% rollout',
  [bundle(B1), bundle(B2, { rolloutCohortCount: 0, targetCohorts: ['beta-testers'] })],
  { bundleId: B1, minBundleId: NIL, cohort: 'beta-testers' },
);

addAppVersion(
  'F34 the same setup without the cohort stays on the current bundle',
  [bundle(B1), bundle(B2, { rolloutCohortCount: 0, targetCohorts: ['beta-testers'] })],
  { bundleId: B1, minBundleId: NIL },
);

addAppVersion(
  'F35 the CURRENT bundle stays eligible through targetCohorts -> no rollback',
  [bundle(B1), bundle(B2, { rolloutCohortCount: 0, targetCohorts: ['beta-testers'] })],
  { bundleId: B2, minBundleId: NIL, cohort: 'beta-testers' },
);

addAppVersion(
  'F36 the current bundle loses targetCohorts eligibility -> ROLLBACK',
  [bundle(B1), bundle(B2, { rolloutCohortCount: 0, targetCohorts: ['beta-testers'] })],
  { bundleId: B2, minBundleId: NIL, cohort: 'other-group' },
);

addAppVersion(
  'F37 partial rollout on the update candidate with a numeric cohort',
  [bundle(B1), bundle(B2, { rolloutCohortCount: 500 })],
  { bundleId: B1, minBundleId: NIL, cohort: '42' },
);

addAppVersion(
  'F38 partial rollout on the update candidate with a different numeric cohort',
  [bundle(B1), bundle(B2, { rolloutCohortCount: 500 })],
  { bundleId: B1, minBundleId: NIL, cohort: '777' },
);

addAppVersion(
  'F39 the newest bundle is behind a partial rollout, the middle one is not',
  [bundle(B1), bundle(B2), bundle(B3, { rolloutCohortCount: 1 })],
  { bundleId: B1, minBundleId: NIL, cohort: '42' },
);

addAppVersion(
  'F40 rollout eligibility is seeded per bundle id -- same cohort, two different bundles',
  [bundle(B1, { rolloutCohortCount: 500 }), bundle(B2, { rolloutCohortCount: 500 })],
  { bundleId: NIL, minBundleId: NIL, cohort: '123' },
);

addAppVersion(
  'F41 current bundle outside a partial rollout while the older one is inside -> ROLLBACK',
  [bundle(B1), bundle(B2, { rolloutCohortCount: 1 })],
  { bundleId: B2, minBundleId: NIL, cohort: '500' },
);

addAppVersion(
  'F42 cohort supplied but the current bundle is fully rolled out -> stays put',
  [bundle(B1), bundle(B2)],
  { bundleId: B2, minBundleId: NIL, cohort: '42' },
);

// =====================================================================================
// G. fingerprint strategy
// =====================================================================================

addFingerprint('G01 fresh install with no bundles -> no update', [], {
  bundleId: NIL,
  minBundleId: NIL,
  fingerprintHash: 'fp-abc',
});

addFingerprint('G02 fresh install with a matching fingerprint -> UPDATE', [fpBundle(B1)], {
  bundleId: NIL,
  minBundleId: NIL,
  fingerprintHash: 'fp-abc',
});

addFingerprint('G03 fingerprint does not match -> no update', [fpBundle(B1)], {
  bundleId: NIL,
  minBundleId: NIL,
  fingerprintHash: 'fp-xyz',
});

addFingerprint(
  'G04 null fingerprintHash on the bundle excludes it',
  [fpBundle(B1, { fingerprintHash: null })],
  { bundleId: NIL, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G05 empty-string fingerprintHash on the bundle is falsy upstream and excludes it',
  [fpBundle(B1, { fingerprintHash: '' })],
  { bundleId: NIL, minBundleId: NIL, fingerprintHash: '' },
);

addFingerprint(
  'G06 fingerprint matching is exact, not a prefix',
  [fpBundle(B1, { fingerprintHash: 'fp-abcdef' })],
  { bundleId: NIL, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G07 fingerprint matching is case sensitive',
  [fpBundle(B1, { fingerprintHash: 'FP-ABC' })],
  { bundleId: NIL, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G08 the newest bundle with a matching fingerprint wins',
  [fpBundle(B1), fpBundle(B2), fpBundle(B3)],
  { bundleId: NIL, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G09 the newest bundle has a different fingerprint -> UPDATE to the older matching one',
  [fpBundle(B1), fpBundle(B2, { fingerprintHash: 'fp-other' })],
  { bundleId: NIL, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G10 targetAppVersion is ignored entirely by the fingerprint strategy',
  [fpBundle(B1, { targetAppVersion: '>=99.0.0' })],
  { bundleId: NIL, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G11 current bundle eligible with a newer match -> UPDATE',
  [fpBundle(B1), fpBundle(B2)],
  { bundleId: B1, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G12 current bundle is the newest match -> no update',
  [fpBundle(B1), fpBundle(B2)],
  { bundleId: B2, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G13 current bundle outside the rollout, older match eligible -> ROLLBACK',
  [fpBundle(B1), fpBundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

// Same rollback-ignores-cohort discriminator as A10, on the fingerprint path.
addFingerprint(
  'G14 current AND older bundle both outside the rollout -> ROLLBACK anyway',
  [fpBundle(B1, { rolloutCohortCount: 0 }), fpBundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G15 current bundle outside the rollout, no other match -> INIT rollback',
  [fpBundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G16 current bundle outside the rollout, no other match, at minBundleId -> no update',
  [fpBundle(B2, { rolloutCohortCount: 0 })],
  { bundleId: B2, minBundleId: B2, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G17 disabled bundles are excluded from the fingerprint candidate set',
  [fpBundle(B1), fpBundle(B2, { enabled: false })],
  { bundleId: NIL, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G18 platform mismatch excludes the bundle',
  [fpBundle(B1, { platform: 'ios' }), fpBundle(B2, { platform: 'ios' })],
  { bundleId: NIL, minBundleId: NIL, fingerprintHash: 'fp-abc', platform: 'android' },
);

addFingerprint(
  'G19 the newest match is on another platform -> UPDATE to the matching one',
  [fpBundle(B1), fpBundle(B2, { platform: 'ios' })],
  { bundleId: NIL, minBundleId: NIL, fingerprintHash: 'fp-abc', platform: 'android' },
);

addFingerprint(
  'G20 channel mismatch excludes the bundle',
  [fpBundle(B1, { channel: 'staging' })],
  { bundleId: NIL, minBundleId: NIL, fingerprintHash: 'fp-abc', channel: 'production' },
);

addFingerprint(
  'G21 the newest match is on another channel -> UPDATE to the same-channel one',
  [fpBundle(B1), fpBundle(B2, { channel: 'staging' })],
  { bundleId: NIL, minBundleId: NIL, fingerprintHash: 'fp-abc', channel: 'production' },
);

addFingerprint(
  'G22 minBundleId filters the fingerprint candidates too',
  [fpBundle(B1), fpBundle(B2), fpBundle(B3)],
  { bundleId: NIL, minBundleId: B2, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G23 minBundleId hides the only rollback target -> INIT rollback',
  [fpBundle(B1), fpBundle(B3, { rolloutCohortCount: 0 })],
  { bundleId: B3, minBundleId: B3, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G24 fingerprint strategy with targetCohorts targeting',
  [fpBundle(B1), fpBundle(B2, { rolloutCohortCount: 0, targetCohorts: ['beta-testers'] })],
  { bundleId: B1, minBundleId: NIL, fingerprintHash: 'fp-abc', cohort: 'beta-testers' },
);

addFingerprint(
  'G25 fingerprint strategy with a partial rollout and a numeric cohort',
  [fpBundle(B1), fpBundle(B2, { rolloutCohortCount: 500 })],
  { bundleId: B1, minBundleId: NIL, fingerprintHash: 'fp-abc', cohort: '42' },
);

addFingerprint(
  'G26 fingerprint strategy honours shouldForceUpdate on the update candidate',
  [fpBundle(B1), fpBundle(B2, { shouldForceUpdate: true })],
  { bundleId: B1, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

addFingerprint(
  'G27 client on an unknown bundle with an older match -> ROLLBACK',
  [fpBundle(B1)],
  { bundleId: B4, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

// =====================================================================================
// H. manifest / patch artifacts must NOT influence the decision
//
// Upstream's makeResponse returns only { id, message, shouldForceUpdate, status,
// storageUri, fileHash } -- manifestStorageUri / assetBaseStorageUri / patches never reach
// the decision at all. These cases pin that down so a future artifact-resolution change on
// the Rust side cannot silently start affecting UPDATE/ROLLBACK selection.
// =====================================================================================

const manifestFields = (id) => ({
  manifestStorageUri: `s3://bucket/${id}/manifest.json`,
  manifestFileHash: `manifest-hash-${id}`,
  assetBaseStorageUri: `s3://bucket/${id}/assets`,
});

const patchTo = (baseId) => ({
  baseBundleId: baseId,
  baseFileHash: `hash-${baseId}`,
  patchFileHash: `patch-hash-${baseId}`,
  patchStorageUri: `s3://bucket/patches/${baseId}.patch`,
});

addAppVersion(
  'H01 full manifest artifacts on the update candidate do not change the decision',
  [bundle(B1, manifestFields(B1)), bundle(B2, manifestFields(B2))],
  { bundleId: B1, minBundleId: NIL },
);

addAppVersion(
  'H02 a patch chain from the current bundle does not change the decision',
  [bundle(B1, manifestFields(B1)), bundle(B2, { ...manifestFields(B2), patches: [patchTo(B1)] })],
  { bundleId: B1, minBundleId: NIL },
);

addAppVersion(
  'H03 a patch whose base bundle is not the current one does not change the decision',
  [
    bundle(B1, manifestFields(B1)),
    bundle(B2, manifestFields(B2)),
    bundle(B3, { ...manifestFields(B3), patches: [patchTo(B1)] }),
  ],
  { bundleId: B2, minBundleId: NIL },
);

addAppVersion(
  'H04 missing manifest fields on the update candidate do not change the decision',
  [bundle(B1, manifestFields(B1)), bundle(B2, { manifestStorageUri: null, manifestFileHash: null })],
  { bundleId: B1, minBundleId: NIL },
);

addAppVersion(
  'H05 patch artifacts on a rollback target do not change the decision',
  [
    bundle(B1, { ...manifestFields(B1), patches: [patchTo(B2)] }),
    bundle(B2, { ...manifestFields(B2), rolloutCohortCount: 0 }),
  ],
  { bundleId: B2, minBundleId: NIL },
);

addFingerprint(
  'H06 manifest and patch artifacts are equally irrelevant on the fingerprint path',
  [fpBundle(B1, manifestFields(B1)), fpBundle(B2, { ...manifestFields(B2), patches: [patchTo(B1)] })],
  { bundleId: B1, minBundleId: NIL, fingerprintHash: 'fp-abc' },
);

// =====================================================================================
// Record the real upstream answers.
// =====================================================================================

const results = [];
for (const c of cases) {
  const platform = c.args.platform ?? 'android';
  const channel = c.args.channel ?? 'production';

  const args =
    c.strategy === 'appVersion'
      ? {
          _updateStrategy: 'appVersion',
          platform,
          appVersion: c.args.appVersion ?? '1.0.0',
          channel,
          minBundleId: c.args.minBundleId,
          bundleId: c.args.bundleId,
          cohort: c.args.cohort,
        }
      : {
          _updateStrategy: 'fingerprint',
          platform,
          fingerprintHash: c.args.fingerprintHash,
          channel,
          minBundleId: c.args.minBundleId,
          bundleId: c.args.bundleId,
          cohort: c.args.cohort,
        };

  // eslint-disable-next-line no-await-in-loop
  const info = await getUpdateInfo(c.bundles, args);

  // The recorded `args` are normalized (platform/channel always explicit) so that the Rust
  // replay in tests/decision_tests.rs can reproduce the SQL-level candidate filter exactly.
  const recordedArgs = {
    bundleId: c.args.bundleId,
    minBundleId: c.args.minBundleId,
    platform,
    channel,
    cohort: c.args.cohort,
  };
  if (c.strategy === 'appVersion') {
    recordedArgs.appVersion = c.args.appVersion ?? '1.0.0';
  } else {
    recordedArgs.fingerprintHash = c.args.fingerprintHash;
  }

  results.push({
    description: c.description,
    strategy: c.strategy,
    bundles: c.bundles,
    args: recordedArgs,
    expected: info ? { status: info.status, id: info.id, shouldForceUpdate: info.shouldForceUpdate } : null,
  });
}

process.stdout.write(JSON.stringify(results, null, 2));
process.stdout.write('\n');
