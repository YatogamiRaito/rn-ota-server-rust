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
-- DEPENDS ON A STRICT sql_mode -- verified on MySQL 8.0.46
-- ---------------------------------------------------------------------------
-- These MODIFY COLUMNs convert the columns from utf8mb4 to ascii. Any existing ID
-- holding a non-ASCII byte therefore cannot be represented after the change.
--   * With MySQL 8's DEFAULT sql_mode (STRICT_TRANS_TABLES), the ALTER aborts with
--     ERROR 1366 "Incorrect string value" and no data is touched -- the safe outcome.
--   * With a non-strict sql_mode (e.g. sql_mode=''), MySQL SILENTLY replaces each
--     non-ASCII character with '?': 'ünïcode-...' becomes '??n??code-...'. That mangles
--     primary keys and can collapse two distinct IDs into one. Measured, both ways.
-- Do not deploy this migration with a relaxed sql_mode. `bundles.id` is a UUID and
-- api.rs::validate_id rejects non-ASCII, so this only bites databases that predate that
-- validation -- but on those it bites silently. Find them first with:
--   SELECT id FROM bundles WHERE id <> CONVERT(id USING ascii);
-- If the ALTER does abort on 1366, the database is left in the damaged state described
-- below (FKs dropped); the repair notes apply unchanged.
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
-- keys itself -- its FK drops are conditional precisely so that this works (verified on
-- a reproduction of the damaged state). Steps 1 and 2 are read-only and safe to run
-- against a healthy database.

-- FKs are dropped first to allow the collation change
-- (MySQL does not allow MODIFY COLUMN to change collation while related columns differ).
--
-- The drops are conditional. MySQL 8.0 has no `DROP FOREIGN KEY IF EXISTS`, and a plain
-- `ALTER TABLE ... DROP FOREIGN KEY` on a key that is already gone fails with ERROR 1091
-- -- which is exactly the state the broken first revision of this migration left behind
-- (see the repair notes above). Without this guard, deleting the `success = 0` row would
-- not be enough: the migration would fail again on its very first statement and the
-- operator would have to re-create both foreign keys by hand before it could proceed.
-- Prepared statements are used because IF() cannot gate DDL directly.
SET @fk_bundle_id := (
  SELECT COUNT(*) FROM information_schema.TABLE_CONSTRAINTS
   WHERE CONSTRAINT_SCHEMA = DATABASE() AND TABLE_NAME = 'bundle_patches'
     AND CONSTRAINT_NAME = 'bundle_patches_bundle_id_fk' AND CONSTRAINT_TYPE = 'FOREIGN KEY');
SET @sql := IF(@fk_bundle_id > 0,
  'ALTER TABLE bundle_patches DROP FOREIGN KEY bundle_patches_bundle_id_fk', 'DO 0');
PREPARE drop_fk_bundle_id FROM @sql;
EXECUTE drop_fk_bundle_id;
DEALLOCATE PREPARE drop_fk_bundle_id;

SET @fk_base_bundle_id := (
  SELECT COUNT(*) FROM information_schema.TABLE_CONSTRAINTS
   WHERE CONSTRAINT_SCHEMA = DATABASE() AND TABLE_NAME = 'bundle_patches'
     AND CONSTRAINT_NAME = 'bundle_patches_base_bundle_id_fk' AND CONSTRAINT_TYPE = 'FOREIGN KEY');
SET @sql := IF(@fk_base_bundle_id > 0,
  'ALTER TABLE bundle_patches DROP FOREIGN KEY bundle_patches_base_bundle_id_fk', 'DO 0');
PREPARE drop_fk_base_bundle_id FROM @sql;
EXECUTE drop_fk_base_bundle_id;
DEALLOCATE PREPARE drop_fk_base_bundle_id;

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
