-- Init migration for the OTA server schema

CREATE TABLE IF NOT EXISTS bundles (
    id CHAR(36) NOT NULL PRIMARY KEY,
    app_name VARCHAR(255) NOT NULL,
    platform VARCHAR(255) NOT NULL,
    should_force_update BOOLEAN NOT NULL DEFAULT FALSE,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    file_hash VARCHAR(255) NOT NULL,
    git_commit_hash VARCHAR(255) NULL,
    message TEXT NULL,
    channel VARCHAR(255) NOT NULL DEFAULT 'production',
    storage_uri VARCHAR(1024) NOT NULL,
    target_app_version VARCHAR(255) NULL,
    fingerprint_hash VARCHAR(255) NULL,
    metadata JSON NULL,
    rollout_cohort_count INT NOT NULL DEFAULT 1000,
    target_cohorts JSON NULL,
    manifest_storage_uri VARCHAR(1024) NULL,
    manifest_file_hash VARCHAR(255) NULL,
    asset_base_storage_uri VARCHAR(1024) NULL,
    ctime TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    INDEX bundles_app_name_idx (app_name),
    INDEX bundles_target_app_version_idx (target_app_version),
    INDEX bundles_fingerprint_hash_idx (fingerprint_hash),
    INDEX bundles_channel_idx (channel),
    INDEX bundles_platform_idx (platform),
    INDEX bundles_rollout_idx (rollout_cohort_count)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS bundle_patches (
    id VARCHAR(255) NOT NULL PRIMARY KEY,
    bundle_id CHAR(36) NOT NULL,
    base_bundle_id CHAR(36) NOT NULL,
    base_file_hash VARCHAR(255) NOT NULL,
    patch_file_hash VARCHAR(255) NOT NULL,
    patch_storage_uri VARCHAR(1024) NOT NULL,
    order_index INT NOT NULL DEFAULT 0,
    ctime TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT bundle_patches_bundle_id_fk FOREIGN KEY (bundle_id) REFERENCES bundles(id) ON DELETE CASCADE,
    CONSTRAINT bundle_patches_base_bundle_id_fk FOREIGN KEY (base_bundle_id) REFERENCES bundles(id) ON DELETE CASCADE,
    INDEX bundle_patches_bundle_id_idx (bundle_id),
    INDEX bundle_patches_base_bundle_id_idx (base_bundle_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS hot_updater_settings (
    `key` VARCHAR(255) NOT NULL PRIMARY KEY,
    `value` TEXT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

INSERT IGNORE INTO hot_updater_settings (`key`, `value`) VALUES ('version', '0.31.0');
