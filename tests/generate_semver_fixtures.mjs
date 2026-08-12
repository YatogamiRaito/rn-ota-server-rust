// Calls @hot-updater/plugin-core's semverSatisfies wrapper and the semver implementation
// it is built on, recording the real input/output pairs. The Rust side's
// `coerce_version`/`satisfies` (src/semver.rs) is tested against these fixtures
// (tests/semver_parity_tests.rs).
//
// NOTE on which implementation this records. Up to 0.35.8 upstream used the npm `semver`
// package: `semverSatisfies(target, current) = semver.satisfies(semver.coerce(current).version,
// target)`. From 0.35.9 it uses `verkit` instead — a zero-dependency reimplementation — and the
// call became `satisfies(coerce(current), target)`. The coerce cases below therefore call
// `verkit.coerce`, because recording a package upstream no longer uses would pin the wrong
// ground truth. `verkit.coerce` returns a plain string (or null) where `semver.coerce` returned
// an object, so there is no `.version` to read.
//
// The swap was verified to be behaviour-preserving across all 139 cases in this file before the
// pin moved; if a future upstream change makes them diverge, this file is where it shows up.
//
// Re-run whenever the upstream pin moves:
//   node tests/generate_semver_fixtures.mjs > tests/fixtures/semver_fixtures.json

// Node's ESM resolver ignores NODE_PATH, so a bare `import '@hot-updater/plugin-core'` only
// works when a node_modules containing it sits on the path from THIS file to the filesystem
// root (ESM resolution walks up from the importing file, not from the cwd). Fall back to the
// tools/fixture-gen install that exists precisely to provide the packages.
const importUpstream = async (specifier, fallbackPath) => {
  try {
    return await import(specifier);
  } catch (err) {
    if (err?.code !== 'ERR_MODULE_NOT_FOUND') throw err;
    return import(new URL(`../tools/fixture-gen/node_modules/${fallbackPath}`, import.meta.url));
  }
};

const { semverSatisfies } = await importUpstream(
  '@hot-updater/plugin-core',
  '@hot-updater/plugin-core/dist/index.mjs',
);
const { coerce } = await importUpstream('verkit', 'verkit/dist/index.js');

const satisfiesCases = [];
const addSatisfies = (description, currentVersion, targetRange, group = 'common') => {
  let expected;
  try {
    expected = semverSatisfies(targetRange, currentVersion);
  } catch {
    expected = false;
  }
  satisfiesCases.push({ description, currentVersion, targetRange, expected, group });
};

// --- Exact matches / basic comparisons ---
addSatisfies('exact match (bare version = exact)', '1.2.3', '1.2.3');
addSatisfies('bare version does not match', '1.5.0', '1.2.3');
addSatisfies('bare version - different major', '2.0.0', '1.2.3');
addSatisfies('v-prefixed target version', '1.5.0', 'v1.5.0');
addSatisfies('empty range -> matches everything', '1.5.0', '');
addSatisfies('whitespace-only range -> matches everything', '1.5.0', '    ');
addSatisfies('* -> matches everything', '1.5.0', '*');
addSatisfies('x -> matches everything', '1.5.0', 'x');
addSatisfies('X -> matches everything', '1.5.0', 'X');
addSatisfies('invalid range -> false', '1.5.0', 'not-a-version');
addSatisfies('invalid range (4 segments) -> false', '1.2.3', '1.2.3.4');
addSatisfies('invalid range (x.y.z.w) -> false', '1.2.3', '1.2.x.y');

// --- >= / <= / > / < (full version) ---
addSatisfies('>= full version equal', '1.2.3', '>=1.2.3');
addSatisfies('>= full version greater', '1.5.0', '>=1.2.3');
addSatisfies('>= full version smaller', '1.0.0', '>=1.2.3');
addSatisfies('<= full version equal', '1.2.3', '<=1.2.3');
addSatisfies('<= full version smaller', '1.0.0', '<=1.2.3');
addSatisfies('<= full version greater', '1.5.0', '<=1.2.3');
addSatisfies('> full version greater', '1.5.0', '>1.2.3');
addSatisfies('> full version equal -> false', '1.2.3', '>1.2.3');
addSatisfies('< full version smaller', '1.0.0', '<1.2.3');
addSatisfies('< full version equal -> false', '1.2.3', '<1.2.3');

// --- >= / <= / > / < (partial / X-range expansion) ---
addSatisfies('>1.4 partial - false at the lower bound', '1.4.5', '>1.4');
addSatisfies('>1.4 partial - true above it', '1.5.0', '>1.4');
addSatisfies('<1.6 partial - true below it', '1.5.9', '<1.6');
addSatisfies('<1.6 partial - equal is false', '1.6.0', '<1.6');
addSatisfies('>=1.4 partial - equal is true', '1.4.0', '>=1.4');
addSatisfies('>=1.4 partial - false below it', '1.3.9', '>=1.4');
addSatisfies('>=1.4 partial - true above it', '1.4.5', '>=1.4');
addSatisfies('<=1.2 partial - inside is true', '1.2.9', '<=1.2');
addSatisfies('<=1.2 partial - outside is false', '1.3.0', '<=1.2');
addSatisfies('>1 partial (major only)', '2.0.0', '>1');
addSatisfies('>1 partial (major only) - false', '1.5.0', '>1');
addSatisfies('<2 partial (major only)', '1.9.9', '<2');
addSatisfies('<2 partial (major only) - false', '2.0.0', '<2');

// --- Bare partial (X-range, no operator) ---
addSatisfies('1.2 bare partial - inside', '1.2.9', '1.2');
addSatisfies('1.2 bare partial - outside', '1.5.0', '1.2');
addSatisfies('1.2.x bare partial - inside', '1.2.9', '1.2.x');
addSatisfies('1.x bare partial - inside', '1.9.9', '1.x');
addSatisfies('1.x bare partial - outside', '2.0.0', '1.x');
addSatisfies('1 bare partial (major) - inside', '1.9.9', '1');
addSatisfies('1 bare partial (major) - outside', '2.0.0', '1');
addSatisfies('1.2.* bare partial', '1.2.5', '1.2.*');
addSatisfies('1.X bare partial', '1.9.0', '1.X');

// --- Caret (^) ---
addSatisfies('^1.2.3 - inside', '1.5.0', '^1.2.3');
addSatisfies('^1.2.3 - outside on a major bump', '2.0.0', '^1.2.3');
addSatisfies('^1.2.3 - outside below it', '1.2.2', '^1.2.3');
addSatisfies('^0.2.3 - inside (0.x minor pin)', '0.2.9', '^0.2.3');
addSatisfies('^0.2.3 - outside on a minor bump', '0.3.0', '^0.2.3');
addSatisfies('^0.0.3 - inside (0.0.x patch pin)', '0.0.3', '^0.0.3');
addSatisfies('^0.0.3 - outside on a patch bump', '0.0.4', '^0.0.3');
addSatisfies('^1.2.x - inside', '1.9.9', '^1.2.x');
addSatisfies('^1.2.x - outside', '2.0.0', '^1.2.x');
addSatisfies('^0.0.x - inside', '0.0.5', '^0.0.x');
addSatisfies('^0.0.x - outside', '0.1.0', '^0.0.x');
addSatisfies('^1.x - inside', '1.9.9', '^1.x');
addSatisfies('^0.x - inside', '0.5.0', '^0.x');
addSatisfies('^0.x - outside', '1.0.0', '^0.x');
addSatisfies('^1 - major only', '1.9.9', '^1');
addSatisfies('^1 - major only, outside', '2.0.0', '^1');

// --- Tilde (~) ---
addSatisfies('~1.2.3 - inside', '1.2.9', '~1.2.3');
addSatisfies('~1.2.3 - outside', '1.3.0', '~1.2.3');
addSatisfies('~1.2 - inside', '1.2.9', '~1.2');
addSatisfies('~1.2 - outside', '1.3.0', '~1.2');
addSatisfies('~1 - inside', '1.9.9', '~1');
addSatisfies('~1 - outside', '2.0.0', '~1');
addSatisfies('~0.2.3 - inside', '0.2.9', '~0.2.3');
addSatisfies('~0.2 - inside', '0.2.9', '~0.2');
addSatisfies('~0 - inside', '0.9.9', '~0');
addSatisfies('~0 - outside', '1.0.0', '~0');

// --- Compound AND (multiple space-separated comparators) ---
addSatisfies('>=1.0.0 <2.0.0 - inside', '1.5.0', '>=1.0.0 <2.0.0');
addSatisfies('>=1.0.0 <2.0.0 - outside', '2.0.0', '>=1.0.0 <2.0.0');
addSatisfies('>1.4 <1.6 - inside', '1.5.0', '>1.4 <1.6');
addSatisfies('>1.4 <1.6 - outside (lower)', '1.4.0', '>1.4 <1.6');
addSatisfies('range with extra whitespace', '1.5.0', '  >=1.0.0   <2.0.0  ');

// --- Hyphen range ---
addSatisfies('1.0.0 - 2.0.0 full - inside', '1.5.0', '1.0.0 - 2.0.0');
addSatisfies('1.0.0 - 2.0.0 full - upper bound inclusive', '2.0.0', '1.0.0 - 2.0.0');
addSatisfies('1.0.0 - 2.0.0 full - outside', '2.0.1', '1.0.0 - 2.0.0');
addSatisfies('1.0 - 2.0 partial on both ends', '1.5.0', '1.0 - 2.0');
addSatisfies('1.2.3 - 1.6 full lower, partial upper - inside', '1.5.0', '1.2.3 - 1.6');
addSatisfies('1.2.3 - 1.6 full lower, partial upper - outside', '1.7.0', '1.2.3 - 1.6');
addSatisfies('1.2 - 1.6.3 partial lower, full upper - inside', '1.5.0', '1.2 - 1.6.3');
addSatisfies('1.2 - 1.6.3 partial lower, full upper - outside', '1.1.9', '1.2 - 1.6.3');
addSatisfies('1.2.3 - 2.3 partial upper, README example - upper bound', '2.3.9', '1.2.3 - 2.3');
addSatisfies('1.2.3 - 2.3 partial upper, README example - outside', '2.4.0', '1.2.3 - 2.3');

// --- OR (||) ---
addSatisfies('1.0.0||2.0.0 - no match', '1.5.0', '1.0.0||2.0.0');
addSatisfies('1.0.0||2.0.0 - matches', '2.0.0', '1.0.0||2.0.0');
addSatisfies('1.x||3.x - first group matches', '1.5.0', '1.x||3.x');
addSatisfies('1.x||3.x - second group matches', '3.5.0', '1.x||3.x');
addSatisfies('1.x||3.x - neither group matches', '2.5.0', '1.x||3.x');

// --- Loose inputs that require coercion (currentVersion) ---
addSatisfies('currentVersion v-prefix coerce', 'v1.5.0', '^1.0.0');
addSatisfies('currentVersion major.minor only coerce', '1.5', '^1.0.0');
addSatisfies('currentVersion major only coerce', '1', '^1.0.0');
addSatisfies('currentVersion build-metadata coerce', '1.5.0+build.123', '^1.0.0');
addSatisfies('currentVersion prerelease coerce (tag is dropped)', '1.5.0-beta.1', '^1.0.0');
addSatisfies('currentVersion noisy prefix coerce', 'app-version-1.5.0-final', '^1.0.0');
addSatisfies('currentVersion cannot be parsed -> false', 'not-a-version', '^1.0.0');
addSatisfies('currentVersion empty -> false', '', '^1.0.0');

// --- Prerelease range (prerelease tag in the target range) ---
addSatisfies('bare prerelease range - numerically equal but tagged -> false', '1.2.3', '1.2.3-beta.1');
addSatisfies('>= prerelease range - numerically equal -> true (release > prerelease)', '1.2.3', '>=1.2.3-beta.1');
addSatisfies('>= prerelease range - numerically below -> false', '1.2.2', '>=1.2.3-beta.1');
addSatisfies('<= prerelease range - numerically equal -> false (release > prerelease)', '1.2.3', '<=1.2.3-beta.1');
addSatisfies('prerelease-to-prerelease exact match', '1.5.0-beta.1', '1.5.0-beta.1');

// --- Realistic usage scenarios (likely target_app_version values in practice) ---
addSatisfies('realistic: production >=2.0.0', '2.3.1', '>=2.0.0');
addSatisfies('realistic: production >=2.0.0, older version outside', '1.9.0', '>=2.0.0');
addSatisfies('realistic: specific major ^3.0.0', '3.4.5', '^3.0.0');
addSatisfies('realistic: specific minor ~2.1.0', '2.1.9', '~2.1.0');
addSatisfies('realistic: hotfix with a range >=2.1.0 <2.2.0', '2.1.5', '>=2.1.0 <2.2.0');
addSatisfies('realistic: outside the hotfix range >=2.1.0 <2.2.0', '2.2.0', '>=2.1.0 <2.2.0');

// --- Known limitations (exotic/extreme edge cases) ---
// This group's fixtures are marked #[ignore] on the Rust side; the real node-semver
// behavior is recorded but full parity is NOT REQUIRED (see the comment in src/semver.rs).
addSatisfies('extremely long numeric component (16+ digits) overflow', '1.0.0', '>=99999999999999999.1.1', 'known-limitation');
addSatisfies('coerce: extremely long number (17 digits), unexpected truncation', 'app-11111111111111111.1.1-final', '*', 'known-limitation');

const coerceCases = [];
const addCoerce = (description, input, group = 'common') => {
  // `verkit.coerce` returns a string, not an object -- see the note at the top of this file.
  const result = coerce(input);
  coerceCases.push({ description, input, expected: result ?? null, group });
};

addCoerce('full version', '1.2.3');
addCoerce('v-prefix', 'v1.2.3');
addCoerce('major.minor', '1.2');
addCoerce('major only', '1');
addCoerce('text prefix + version', 'abc-1.2.3.4');
addCoerce('extra segments are truncated', '1.2.3.4.5');
addCoerce('leading/trailing whitespace', '  1.2.3  ');
addCoerce('prerelease tag is dropped', '1.2.3-beta.1');
addCoerce('build metadata is dropped', '1.2.3+build');
addCoerce('empty string -> null', '');
addCoerce('whitespace only -> null', '   ');
addCoerce('no digits -> null', 'abc');
addCoerce('x prefix + version', 'x1.2.3');
addCoerce('major.minor.patch with a single-digit zero that is not a leading zero', '0.1.2');
// --- Segments with a leading zero are rejected ---
addCoerce('leading zero: date-like version -> null', '2021.01.02', 'known-limitation');
addCoerce('leading zero: minor -> null', '2021.1.02', 'known-limitation');
addCoerce('leading zero: minor2 -> null', '1.02.3', 'known-limitation');
addCoerce('leading zero: major -> null', '01.2.3', 'known-limitation');
addCoerce('leading zero: patch -> null', '1.2.03', 'known-limitation');
addCoerce('leading zero: major.minor -> null', '2021.01', 'known-limitation');
addCoerce('leading zero: double zero -> null', '00.1.2', 'known-limitation');
addCoerce('leading zero: standalone -> null', '010', 'known-limitation');
addCoerce('leading zero: after a prefix -> null', 'v01.2.3', 'known-limitation');
addCoerce('leading zero: after a text prefix -> null', 'abc01.2.3', 'known-limitation');
// --- Extremely large numeric components (overflow / safe-integer limit) ---
addCoerce('16 digits at the limit -> valid', '1111111111111111.1.1', 'known-limitation');
addCoerce('17 digits -> node truncates unexpectedly', '11111111111111111.1.1', 'known-limitation');
addCoerce('sixteen nines -> safe-integer overflow, null', '9999999999999999.1.1', 'known-limitation');
addCoerce('seventeen nines -> node truncates unexpectedly', '99999999999999999.1.1', 'known-limitation');

const output = { satisfiesCases, coerceCases };
process.stdout.write(JSON.stringify(output, null, 2));
process.stdout.write('\n');
