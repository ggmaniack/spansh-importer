-- Route-planning flags per system, derived from its stars. Skipped by `finalize --no-flags`.
-- One row per system with at least one known star:
--   main_star_sub_type   lookup id ('body_sub_type') of the arrival star
--   main_star_scoopable  arrival star is fuel-scoopable (spectral class K, G, B, F, O, A or M)
--   scoopable_nearby     another scoopable star within 100 ls of arrival
--   neutron              arrival star, or a star within 100 ls, is a neutron star (FSD boost)
-- The 100 ls cut-off follows Spansh's own route data. Built once; drop it to rebuild.
CREATE MATERIALIZED VIEW IF NOT EXISTS system_flags AS
WITH star AS (
    SELECT b.system_id64,
           b.sub_type,
           coalesce(b.main_star, false) AS main,
           b.distance_to_arrival AS dist,
           t.name ~ '^[OBAFGKM] \(' AS scoopable,
           t.name = 'Neutron Star' AS neutron
    FROM body b
    JOIN lookup t ON t.id = b.sub_type
    WHERE b.type = (SELECT id FROM lookup WHERE kind = 'body_type' AND name = 'Star')
)
SELECT system_id64 AS id64,
       min(sub_type) FILTER (WHERE main) AS main_star_sub_type,
       coalesce(bool_or(scoopable) FILTER (WHERE main), false) AS main_star_scoopable,
       coalesce(bool_or(scoopable) FILTER (WHERE NOT main AND dist < 100), false) AS scoopable_nearby,
       coalesce(bool_or(neutron) FILTER (WHERE main OR dist < 100), false) AS neutron
FROM star
GROUP BY system_id64;
CREATE UNIQUE INDEX IF NOT EXISTS system_flags_id64_idx ON system_flags (id64);
ANALYZE system_flags;
