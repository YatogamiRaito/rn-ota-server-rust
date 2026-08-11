-- Move the tenant boundary into the schema.
--
-- Until now `bundles.id` was the primary key on its own, so a bundle id was global
-- across every app this server hosts. The tenant boundary existed only in application
-- code: every query carried `AND app_name = ?`, and `create_bundles` did a row-locked
-- ownership check before its upsert. That check was added because without it one app's
-- token could POST another app's bundle id and overwrite that bundle's contents while
-- the row kept its original owner -- arbitrary content delivery to another tenant's
-- devices. Application-layer guards are the right emergency fix, but the boundary
-- belongs in the schema, where it cannot be forgotten by the next query someone writes.
--
-- After this migration:
--   * bundles       PRIMARY KEY (app_name, id)
--   * bundle_patches gains `app_name`, PRIMARY KEY (app_name, id)
--   * both foreign keys become composite, so a patch can only ever reference a bundle
--     belonging to the SAME app. That was previously enforced only in api.rs.
--
-- Consequences worth knowing:
--   * `bundles.id` is no longer globally unique. Two apps may each hold a bundle with
--     the same id; they are different rows and cannot see each other. Any query that
--     looks a bundle up by id alone is now a bug -- it must carry `app_name`.
--   * `bundle_patches.id` is derived as "{bundle_id}:{base_bundle_id}". That string can
--     now repeat across apps, which is exactly why `app_name` joins its primary key.
--
-- Verified against a live MySQL 8.0.46 on 2026-08-11, on an empty database and on one
-- already holding bundles and patches across two apps, including the orphan case below.

-- The foreign keys reference `bundles(id)`, so they must go before its primary key can
-- change. They are restored at the bottom in composite form.
ALTER TABLE bundle_patches
  DROP FOREIGN KEY bundle_patches_bundle_id_fk,
  DROP FOREIGN KEY bundle_patches_base_bundle_id_fk;

-- Nullable first: existing rows have no value yet and the backfill needs somewhere to
-- write. It becomes NOT NULL once every row is populated.
ALTER TABLE bundle_patches
  ADD COLUMN app_name VARCHAR(255) NULL AFTER id;

UPDATE bundle_patches p
  JOIN bundles b ON b.id = p.bundle_id
   SET p.app_name = b.app_name;

-- A patch whose bundle no longer exists cannot be assigned an owner. The foreign keys
-- should have made this impossible, but a database that ran the broken first revision
-- of 20260722010000 spent time with both of them dropped (see that file's header), so
-- orphans may exist. They are unusable -- they reference a bundle that is gone, so no
-- device can ever be served them -- and they would block the NOT NULL below.
DELETE FROM bundle_patches WHERE app_name IS NULL;

ALTER TABLE bundle_patches
  MODIFY COLUMN app_name VARCHAR(255) NOT NULL;

ALTER TABLE bundles
  DROP PRIMARY KEY,
  ADD PRIMARY KEY (app_name, id);

ALTER TABLE bundle_patches
  DROP PRIMARY KEY,
  ADD PRIMARY KEY (app_name, id);

-- `bundles_app_name_idx` is now redundant: the new primary key has app_name as its
-- leftmost column, so it serves every lookup that index served.
DROP INDEX bundles_app_name_idx ON bundles;

-- The composite foreign keys. `(app_name, bundle_id)` and `(app_name, base_bundle_id)`
-- both reference `bundles(app_name, id)`, which is what makes a cross-app patch
-- reference impossible rather than merely rejected by application code.
ALTER TABLE bundle_patches
  ADD INDEX bundle_patches_app_bundle_idx (app_name, bundle_id),
  ADD INDEX bundle_patches_app_base_bundle_idx (app_name, base_bundle_id);

ALTER TABLE bundle_patches
  ADD CONSTRAINT bundle_patches_bundle_id_fk
      FOREIGN KEY (app_name, bundle_id) REFERENCES bundles(app_name, id) ON DELETE CASCADE,
  ADD CONSTRAINT bundle_patches_base_bundle_id_fk
      FOREIGN KEY (app_name, base_bundle_id) REFERENCES bundles(app_name, id) ON DELETE CASCADE;

-- Single-column indexes superseded by the composite ones above.
DROP INDEX bundle_patches_bundle_id_idx ON bundle_patches;
DROP INDEX bundle_patches_base_bundle_id_idx ON bundle_patches;
