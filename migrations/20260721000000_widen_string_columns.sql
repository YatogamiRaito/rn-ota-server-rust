-- Columns such as file_hash carry an RSA-4096 signature on signing-enabled builds
-- ("sig:" + base64, ~688 characters) -- VARCHAR(255) truncated that silently, and
-- signature verification of the downloaded bundle then failed on the device.
-- The real @hot-updater/server schema emits these "string" typed columns as TEXT
-- in MySQL (db/schema/sql.ts:getSqlType) -- we match that here.

ALTER TABLE bundles
  DROP INDEX bundles_target_app_version_idx,
  DROP INDEX bundles_fingerprint_hash_idx,
  DROP INDEX bundles_channel_idx,
  DROP INDEX bundles_platform_idx;

ALTER TABLE bundles
  MODIFY COLUMN platform TEXT NOT NULL,
  MODIFY COLUMN file_hash TEXT NOT NULL,
  MODIFY COLUMN git_commit_hash TEXT NULL,
  MODIFY COLUMN message TEXT NULL,
  MODIFY COLUMN channel TEXT NOT NULL DEFAULT ('production'),
  MODIFY COLUMN storage_uri TEXT NOT NULL,
  MODIFY COLUMN target_app_version TEXT NULL,
  MODIFY COLUMN fingerprint_hash TEXT NULL,
  MODIFY COLUMN manifest_storage_uri TEXT NULL,
  MODIFY COLUMN manifest_file_hash TEXT NULL,
  MODIFY COLUMN asset_base_storage_uri TEXT NULL;

ALTER TABLE bundles
  ADD INDEX bundles_target_app_version_idx (target_app_version(255)),
  ADD INDEX bundles_fingerprint_hash_idx (fingerprint_hash(255)),
  ADD INDEX bundles_channel_idx (channel(255)),
  ADD INDEX bundles_platform_idx (platform(255));

ALTER TABLE bundle_patches
  MODIFY COLUMN base_file_hash TEXT NOT NULL,
  MODIFY COLUMN patch_file_hash TEXT NOT NULL,
  MODIFY COLUMN patch_storage_uri TEXT NOT NULL;
