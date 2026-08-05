// Calls the real @hot-updater/js package and records the real input/output pairs of the
// update-check decision algorithm (appVersionStrategy/fingerprintStrategy). The Rust
// side's `decide_update` is tested against these fixtures (tests/decision_tests.rs).
//
// Re-run whenever the hot-updater packages are upgraded:
//   node tests/generate_decision_fixtures.mjs > tests/fixtures/decision_fixtures.json
import { getUpdateInfo } from '@hot-updater/js';

const NIL = '00000000-0000-0000-0000-000000000000';

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

const cases = [];

const addCase = (description, strategy, bundles, args) => {
  cases.push({
    description,
    strategy,
    bundles,
    args,
  });
};

// --- appVersion strategy ---
addCase(
  'fresh install receives the newest eligible bundle',
  'appVersion',
  [bundle('00000000-0000-0000-0000-000000000001'), bundle('00000000-0000-0000-0000-000000000002')],
  { bundleId: NIL, minBundleId: NIL, cohort: undefined },
);

addCase(
  'with rollout 0 a fresh install gets no update',
  'appVersion',
  [bundle('00000000-0000-0000-0000-000000000001', { rolloutCohortCount: 0 })],
  { bundleId: NIL, minBundleId: NIL, cohort: undefined },
);

addCase(
  'current bundle still eligible and no newer candidate -> no update',
  'appVersion',
  [bundle('00000000-0000-0000-0000-000000000001')],
  { bundleId: '00000000-0000-0000-0000-000000000001', minBundleId: NIL, cohort: undefined },
);

addCase(
  'current bundle eligible but a newer candidate exists -> UPDATE',
  'appVersion',
  [
    bundle('00000000-0000-0000-0000-000000000001'),
    bundle('00000000-0000-0000-0000-000000000002'),
  ],
  { bundleId: '00000000-0000-0000-0000-000000000001', minBundleId: NIL, cohort: undefined },
);

addCase(
  'current bundle fell out of the rollout but a newer eligible candidate exists -> UPDATE',
  'appVersion',
  [
    bundle('00000000-0000-0000-0000-000000000001', { rolloutCohortCount: 0 }),
    bundle('00000000-0000-0000-0000-000000000002'),
  ],
  { bundleId: '00000000-0000-0000-0000-000000000001', minBundleId: NIL, cohort: undefined },
);

addCase(
  'current bundle fell out of the rollout, no newer candidate but an older eligible one exists -> ROLLBACK',
  'appVersion',
  [
    bundle('00000000-0000-0000-0000-000000000001'),
    bundle('00000000-0000-0000-0000-000000000002', { rolloutCohortCount: 0 }),
  ],
  { bundleId: '00000000-0000-0000-0000-000000000002', minBundleId: NIL, cohort: undefined },
);

addCase(
  'current bundle fell out of the rollout, no eligible candidate at all, below minBundleId -> native rollback (INIT)',
  'appVersion',
  [bundle('00000000-0000-0000-0000-000000000002', { rolloutCohortCount: 0 })],
  { bundleId: '00000000-0000-0000-0000-000000000002', minBundleId: NIL, cohort: undefined },
);

addCase(
  'current bundle is below minBundleId and there is no candidate -> no update',
  'appVersion',
  [bundle('00000000-0000-0000-0000-000000000002', { rolloutCohortCount: 0 })],
  {
    bundleId: '00000000-0000-0000-0000-000000000002',
    minBundleId: '00000000-0000-0000-0000-000000000002',
    cohort: undefined,
  },
);

addCase(
  'semver does not match -> candidate is filtered out, no update',
  'appVersion',
  [bundle('00000000-0000-0000-0000-000000000001', { targetAppVersion: '>=2.0.0' })],
  { bundleId: NIL, minBundleId: NIL, cohort: undefined, appVersion: '1.5.0' },
);

addCase(
  'explicit cohort target matches -> UPDATE',
  'appVersion',
  [
    bundle('00000000-0000-0000-0000-000000000001'),
    bundle('00000000-0000-0000-0000-000000000002', { rolloutCohortCount: 0, targetCohorts: ['beta-testers'] }),
  ],
  { bundleId: '00000000-0000-0000-0000-000000000001', minBundleId: NIL, cohort: 'beta-testers' },
);

addCase(
  'numeric cohort with a partial rollout (500/1000)',
  'appVersion',
  [
    bundle('00000000-0000-0000-0000-000000000001'),
    bundle('00000000-0000-0000-0000-000000000002', { rolloutCohortCount: 500 }),
  ],
  { bundleId: '00000000-0000-0000-0000-000000000001', minBundleId: NIL, cohort: '42' },
);

// --- fingerprint strategy (same state machine, different matching field) ---
addCase(
  'fresh install with a matching fingerprint -> UPDATE',
  'fingerprint',
  [bundle('00000000-0000-0000-0000-000000000001', { targetAppVersion: null, fingerprintHash: 'fp-abc' })],
  { bundleId: NIL, minBundleId: NIL, cohort: undefined, fingerprintHash: 'fp-abc' },
);

addCase(
  'fingerprint does not match -> no update',
  'fingerprint',
  [bundle('00000000-0000-0000-0000-000000000001', { targetAppVersion: null, fingerprintHash: 'fp-abc' })],
  { bundleId: NIL, minBundleId: NIL, cohort: undefined, fingerprintHash: 'fp-xyz' },
);

addCase(
  'fingerprint: current bundle fell out of the rollout, an older eligible candidate exists -> ROLLBACK',
  'fingerprint',
  [
    bundle('00000000-0000-0000-0000-000000000001', { targetAppVersion: null, fingerprintHash: 'fp-abc' }),
    bundle('00000000-0000-0000-0000-000000000002', {
      targetAppVersion: null,
      fingerprintHash: 'fp-abc',
      rolloutCohortCount: 0,
    }),
  ],
  { bundleId: '00000000-0000-0000-0000-000000000002', minBundleId: NIL, cohort: undefined, fingerprintHash: 'fp-abc' },
);

const results = [];
for (const c of cases) {
  const args =
    c.strategy === 'appVersion'
      ? {
          _updateStrategy: 'appVersion',
          platform: 'android',
          appVersion: c.args.appVersion ?? '1.0.0',
          channel: 'production',
          minBundleId: c.args.minBundleId,
          bundleId: c.args.bundleId,
          cohort: c.args.cohort,
        }
      : {
          _updateStrategy: 'fingerprint',
          platform: 'android',
          fingerprintHash: c.args.fingerprintHash,
          channel: 'production',
          minBundleId: c.args.minBundleId,
          bundleId: c.args.bundleId,
          cohort: c.args.cohort,
        };

  // eslint-disable-next-line no-await-in-loop
  const info = await getUpdateInfo(c.bundles, args);

  results.push({
    description: c.description,
    strategy: c.strategy,
    bundles: c.bundles,
    args: c.args,
    expected: info ? { status: info.status, id: info.id, shouldForceUpdate: info.shouldForceUpdate } : null,
  });
}

process.stdout.write(JSON.stringify(results, null, 2));
process.stdout.write('\n');
