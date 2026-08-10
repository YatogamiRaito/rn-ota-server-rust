-- Give the ID / FK columns their own charset and collation instead of inheriting the
-- table-level `utf8mb4_unicode_ci` default:
--   bundles.id, bundle_patches.id, bundle_patches.bundle_id, bundle_patches.base_bundle_id
--
-- These columns hold UUIDs (pure ASCII hex + dashes), so `ascii` is a better fit than
-- utf8mb4: one byte per character instead of four, which meaningfully shrinks the index
-- entries on the primary keys these columns form.
--
-- ---------------------------------------------------------------------------
-- WHY NOT `ascii_bin` -- do not "fix" this back, it does not work
-- ---------------------------------------------------------------------------
-- The obvious choice here is `ascii_bin`, because src/routes/check.rs compares the same
-- IDs with raw Rust &str comparison (`b.id.as_str() > client_bundle_id`) -- byte-wise and
-- case-sensitive -- and `_bin` is the only family of MySQL collations with those exact
-- semantics. An earlier revision of this file did precisely that. It cannot work:
--
--   MySQL sets the wire-protocol BINARY column flag (bit 128) on *any* `_bin` collation,
--   not only on the `binary` charset. sqlx maps a flagged column to SQL type BINARY and
--   then refuses to decode it into `String`:
--
--     ColumnDecode { index: "id", source: "mismatched types; Rust type `String`
--       (as SQL type `VARCHAR`) is not compatible with SQL type `BINARY`" }
--
--   See sqlx-mysql-0.8.6 src/types/str.rs (`!ty.flags.contains(ColumnFlags::BINARY)`).
--
-- Because `models::Bundle.id` and the three `BundlePatch` ID fields are `String`, every
-- `query_as::<Bundle>` and `query_as::<BundlePatch>` in the server fails at decode time
-- under a `_bin` collation. That means HTTP 500 on *every* read path: both update-check
-- queries, GET/PATCH on bundles, and the bundle list. Measured: 33 of 58 integration
-- tests failed on this single cause. `utf8mb4_bin` behaves identically -- the escape is
-- not "a different binary collation", there isn't one.
--
-- So `ascii_general_ci` it is. What that costs, and why it is acceptable:
--   * Comparison in SQL is now case-INSENSITIVE while Rust's stays case-sensitive.
--     For the alphabet these IDs actually use -- lowercase hex `[0-9a-f-]`, which is what
--     the upstream CLI generates -- `ORDER BY id` is identical to byte order, so nothing
--     diverges in practice.
--   * Two IDs differing only in hex-letter case now collide on the PRIMARY KEY, so a
--     database cannot hold both `...0000a` and `...0000A` at all. That is arguably safer:
--     the cross-tenant ownership check in create_bundles runs in SQL, so it shares this
--     collation and cannot be narrower than the key it is protecting.
--   * A few Rust-side comparisons were normalised to match (see the `ascii_general_ci`
--     comments in src/routes/api.rs). Those normalisations would be WRONG under `_bin`;
--     revert them together with this migration if it is ever changed.
--
-- The remaining divergence from upstream hot-updater (which orders IDs with ICU
-- localeCompare) is recorded in docs/upstream-parity.md section 3.3 and pinned by an
-- ignored fixture case, rather than left implicit.
--
-- ---------------------------------------------------------------------------
-- IF YOUR DATABASE ALREADY TRIED THE BROKEN VERSION OF THIS MIGRATION -- READ THIS
-- ---------------------------------------------------------------------------
-- The first published revision of this file was invalid SQL. It wrote
--   MODIFY COLUMN id CHAR(36) NOT NULL CHARACTER SET ascii COLLATE ascii_bin
-- and MySQL rejects that with ERROR 1064: charset/collation are part of the data *type*
-- and must precede NOT NULL. So this migration always failed -- but not before its first
-- statement, the FK drop below, had already committed (DDL is not transactional in MySQL).
--
-- A database that ran it is therefore left with BOTH foreign keys on bundle_patches
-- missing, and a `success = 0` row in _sqlx_migrations that blocks all further migrations.
-- Orphaned bundle_patches rows may have accumulated silently since. Detect and repair:
--
--   -- 1. Is this database affected?
--   SELECT * FROM _sqlx_migrations WHERE version = 20260722010000;
--   SELECT CONSTRAINT_NAME FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS
--    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'bundle_patches'
--      AND CONSTRAINT_TYPE = 'FOREIGN KEY';
--   -- Affected if the first returns a row with success = 0, or the second returns
--   -- fewer than two rows. If neither is true, stop -- nothing to repair.
--
--   -- 2. Find rows that the missing FKs would have prevented.
--   SELECT p.* FROM bundle_patches p
--    WHERE NOT EXISTS (SELECT 1 FROM bundles b WHERE b.id = p.bundle_id)
--       OR NOT EXISTS (SELECT 1 FROM bundles b WHERE b.id = p.base_bundle_id);
--   -- Review them. They reference bundles that no longer exist, so they are unusable;
--   -- once you are satisfied, delete them -- the FKs cannot be restored while they exist.
--
--   -- 3. Clear the failed attempt so sqlx will re-run this migration.
--   DELETE FROM _sqlx_migrations WHERE version = 20260722010000 AND success = 0;
--
-- Then start the server: this migration re-runs from the top and restores both foreign
-- keys itself. Steps 1 and 2 are read-only and safe to run against a healthy database.

-- FKs are dropped first to allow the collation change
-- (MySQL does not allow MODIFY COLUMN to change collation while related columns differ).
ALTER TABLE bundle_patches
  DROP FOREIGN KEY bundle_patches_bundle_id_fk,
  DROP FOREIGN KEY bundle_patches_base_bundle_id_fk;

ALTER TABLE bundles
  MODIFY COLUMN id CHAR(36) CHARACTER SET ascii COLLATE ascii_general_ci NOT NULL;

ALTER TABLE bundle_patches
  MODIFY COLUMN id VARCHAR(255) CHARACTER SET ascii COLLATE ascii_general_ci NOT NULL,
  MODIFY COLUMN bundle_id CHAR(36) CHARACTER SET ascii COLLATE ascii_general_ci NOT NULL,
  MODIFY COLUMN base_bundle_id CHAR(36) CHARACTER SET ascii COLLATE ascii_general_ci NOT NULL;

-- FKs are re-added with the same names and ON DELETE CASCADE
ALTER TABLE bundle_patches
  ADD CONSTRAINT bundle_patches_bundle_id_fk FOREIGN KEY (bundle_id) REFERENCES bundles(id) ON DELETE CASCADE,
  ADD CONSTRAINT bundle_patches_base_bundle_id_fk FOREIGN KEY (base_bundle_id) REFERENCES bundles(id) ON DELETE CASCADE;
