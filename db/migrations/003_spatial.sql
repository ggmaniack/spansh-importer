-- 3D spatial index on system coordinates, using the `cube` extension bundled with PostgreSQL.
-- Skipped by `finalize --no-spatial`. Queries must use the same expression to hit the index:
--   ORDER BY cube(array[x, y, z]) <-> cube(array[0, 0, 0]) LIMIT 10                 (nearest)
--   WHERE cube(array[x, y, z]) <@ cube_enlarge(cube(array[0, 0, 0]), 20, 3)          (20 ly box)
-- The extension goes into `public`, so it serves every import schema in the database.
CREATE EXTENSION IF NOT EXISTS cube WITH SCHEMA public;
CREATE INDEX IF NOT EXISTS system_coords_idx ON system USING gist (cube(array[x, y, z]));
