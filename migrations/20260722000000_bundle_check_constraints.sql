-- ⚠️ These ALTER TABLE ... ADD CONSTRAINT ... CHECK statements were NOT TESTED
-- against a real/live MySQL database (no live MySQL access in this environment).
-- If any existing `bundles` row violates these constraints (e.g. a row where both
-- target_app_version and fingerprint_hash are NULL, or where rollout_cohort_count
-- is outside the 0-1000 range), this ALTER TABLE FAILS and the migration (and
-- therefore server startup) stops. Verify/clean up existing data against these
-- constraints before applying.
--
-- Reference: node_modules/@hot-updater/server/src/schema/v0_31_0.ts
-- (bundlesV031.checks) and node_modules/@hot-updater/server/src/db/schemaEnhancements.ts
-- (assertBundlePersistenceConstraints). MySQL 8.0.16+ actually enforces CHECK
-- constraints (earlier versions parse but ignore them).

ALTER TABLE bundles
  ADD CONSTRAINT check_version_or_fingerprint
    CHECK (target_app_version IS NOT NULL OR fingerprint_hash IS NOT NULL),
  ADD CONSTRAINT bundles_rollout_cohort_count_check
    CHECK (rollout_cohort_count >= 0 AND rollout_cohort_count <= 1000);
