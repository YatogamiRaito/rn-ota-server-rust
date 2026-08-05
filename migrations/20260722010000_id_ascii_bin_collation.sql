-- bundles.id / bundle_patches.id / bundle_patches.bundle_id / bundle_patches.base_bundle_id
-- columns inherit the table-level default `utf8mb4_unicode_ci` collation.
-- That collation is case-insensitive and linguistic; but these columns carry UUIDs
-- (pure ASCII hex + dashes) and src/routes/check.rs::decide_update compares the same
-- IDs with raw Rust &str comparison (e.g. `b.id.as_str() > client_bundle_id`) --
-- a byte-wise, case-sensitive comparison. Two strings SQL calls "equal/ordered" can
-- behave differently on the Rust side (generated UUIDs are currently always lowercase,
-- so this causes no problem in production yet, but it is a latent risk).
--
-- Fix: switch these ID/FK columns to the `ascii_bin` collation -- UUIDs are pure ASCII,
-- so no linguistic rule is needed; `_bin` tells MySQL to "compare bytes as-is", which
-- is exactly the same semantics as Rust's str comparison.
--
-- ⚠️ NOTE: These ALTERs were NOT TESTED against a real MySQL server in this environment
-- (no live DB access). Watch the deploy carefully -- in particular verify that the
-- FOREIGN KEY DROP/ADD ordering and the collation change on existing data (e.g. two IDs
-- differing only in case now counting as distinct) cause no problems.

-- FKs are dropped first to allow the collation change
-- (MySQL does not allow MODIFY COLUMN to change collation while related columns differ).
ALTER TABLE bundle_patches
  DROP FOREIGN KEY bundle_patches_bundle_id_fk,
  DROP FOREIGN KEY bundle_patches_base_bundle_id_fk;

ALTER TABLE bundles
  MODIFY COLUMN id CHAR(36) NOT NULL CHARACTER SET ascii COLLATE ascii_bin;

ALTER TABLE bundle_patches
  MODIFY COLUMN id VARCHAR(255) NOT NULL CHARACTER SET ascii COLLATE ascii_bin,
  MODIFY COLUMN bundle_id CHAR(36) NOT NULL CHARACTER SET ascii COLLATE ascii_bin,
  MODIFY COLUMN base_bundle_id CHAR(36) NOT NULL CHARACTER SET ascii COLLATE ascii_bin;

-- FKs are re-added with the same names and ON DELETE CASCADE
ALTER TABLE bundle_patches
  ADD CONSTRAINT bundle_patches_bundle_id_fk FOREIGN KEY (bundle_id) REFERENCES bundles(id) ON DELETE CASCADE,
  ADD CONSTRAINT bundle_patches_base_bundle_id_fk FOREIGN KEY (base_bundle_id) REFERENCES bundles(id) ON DELETE CASCADE;
