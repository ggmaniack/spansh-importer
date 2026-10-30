# Field mapping: Spansh JSON -> PostgreSQL

Schema: `db/migrations/001_schema.sql` (load phase) and `002_constraints.sql` (keys, FKs, indexes after
the load). All tables live in schema `spansh`. Paths below are relative to one system object.

## General rules

| Rule | Detail |
|---|---|
| Missing vs `null` | Both become SQL `NULL` (e.g. `allegiance` is absent on unpopulated systems, explicit `null` on 928k others). The difference is not kept. |
| Arrays | Absent key -> `NULL`; `[]` -> empty array / no child rows (the parent row still exists). |
| id64 | `u64` reinterpreted as `BIGINT` (`as i64`). Values >= 2^63 would appear negative; read back with `id64::numeric + 2^64` when negative. None observed. |
| Floats | `DOUBLE PRECISION`; serde_json parses to the nearest f64, which round-trips the source text. Integers in float fields (e.g. `powerStateReinforcement`) are stored as doubles, exact below 2^53. |
| Timestamps | `TIMESTAMPTZ`, microsecond precision. Accepted shapes: `YYYY-MM-DD[T ]HH:MM:SS[.f{1,6}](Z\|+00\|+00:00)`. Anything else -> `extra` + `import_issue`. |
| Enum-like strings | `SMALLINT` -> `lookup(id, kind, name)`; exact, case-sensitive string. New values are added on first sight, never rejected. |
| Catalogue entities | `commodity`, `ship` keyed by Frontier id. An id seen again with different attributes -> `import_issue` (`catalogue_conflict`) and the full item kept in the station's `extra`. Modules legitimately keep old names/ratings per station, so each distinct attribute set is a `module_variant` row (importer-assigned id); no conflicts. |
| Unknown fields | Any key not listed below is copied, with its value, into the nearest entity's `extra` JSONB (`system`, `body` or `station`), keyed by its path relative to that entity. Nothing is dropped. |
| Type mismatches / overflow | A value that does not fit its column (string in a number field, integer > `INTEGER` max, ...) -> column `NULL`, original value in `extra`, `import_issue` row. |
| Map keys | Known map keys get columns; unknown keys go to `extra` (e.g. a new `timestamps` key). |

## system (`$`)

| JSON | Column | Notes |
|---|---|---|
| `id64` | `system.id64` | PK |
| `name` | `name` | |
| `coords.x/y/z` | `x`, `y`, `z` | |
| `allegiance`, `government`, `primaryEconomy`, `secondaryEconomy`, `security` | same names | lookup kinds `allegiance`, `government`, `economy`, `economy`, `security` |
| `population` | `population` | BIGINT, max seen 3.16e10 |
| `bodyCount` | `body_count` | SMALLINT, max seen 173 |
| `date` | `updated_at` | |
| `controllingPower`, `powerState` | `controlling_power`, `power_state` | lookup `power`, `power_state` |
| `powers[]` | `powers SMALLINT[]` | lookup `power` |
| `powerStateControlProgress/Reinforcement/Undermining` | `power_state_*` | DOUBLE |
| `timestamps.{factions,powerState,powers,controllingPower,thargoidWar}` | `ts_*` | other keys -> `extra` |
| `powerConflictProgress[]` | `power_conflict(power, progress)` | |
| `thargoidWar.*` | `thargoid_war` (1 row) | states: lookup `thargoid_state` |
| `factions[]` | `system_faction` | `in_factions = true` |
| `factions[].activeStates/pendingStates/recoveringStates[]` | `faction_state(bucket, state, trend)` | bucket: lookup `faction_state_bucket` |
| `controllingFaction.name` | `system.controlling_faction`, and `system_faction.is_controlling` on the matching row | If no `factions[]` entry has that name: a `system_faction` row with `in_factions = false` (plus its states). If an entry exists but its fields differ: the `controllingFaction` object goes to `extra.controllingFaction`. |
| `bodies[]` | `body` | |
| `stations[]` | `station` with `body_id64 = NULL` | |

## body (`bodies[]`)

| JSON | Column | Notes |
|---|---|---|
| `id64` | `body.id64` | PK; always `system id64 \| bodyId << 55` |
| `bodyId` | `body_id` | SMALLINT, max seen 219 |
| `name`, `updateTime` | `name`, `updated_at` | |
| `type`, `subType` | `type`, `sub_type` | lookup `body_type`, `body_sub_type` |
| `distanceToArrival`, `orbitalPeriod`, `semiMajorAxis`, `orbitalEccentricity`, `orbitalInclination`, `argOfPeriapsis`, `meanAnomaly`, `ascendingNode`, `rotationalPeriod`, `axialTilt`, `surfaceTemperature` | snake_case columns | no range checks: real data exceeds the schema's bounds |
| `rotationalPeriodTidallyLocked` | `tidally_locked` | |
| `mainStar`, `age`, `spectralClass`, `luminosity`, `absoluteMagnitude`, `solarMasses`, `solarRadius` | snake_case | stars; `spectral_class`, `luminosity` are lookups (many values outside the schema enums) |
| `isLandable`, `gravity`, `earthMasses`, `radius`, `surfacePressure` | snake_case | planets |
| `volcanismType`, `atmosphereType`, `terraformingState`, `reserveLevel` | `volcanism`, `atmosphere`, `terraforming_state`, `reserve_level` | lookups |
| `timestamps.{meanAnomaly,distanceToArrival,ascendingNode}` | `ts_mean_anomaly`, `ts_distance_to_arrival`, `ts_ascending_node` | other keys -> `extra`; `ascendingNode` first appeared in the full galaxy dump |
| `parents[]` (`{kind: bodyId}`) | `parent_kinds SMALLINT[]`, `parent_body_ids SMALLINT[]` | parallel arrays, element 1 = immediate parent; kind lookup `parent_kind` |
| `atmosphereComposition{}` | `atm_<gas>` columns (13) | percent; a component without a column goes to `extra` |
| `solidComposition{}` | `solid_ice`, `solid_rock`, `solid_metal` | percent |
| `materials{}` | `mat_<element>` columns (25) | percent |
| `signals.signals{}` | `body_signal(signal, count)` | lookup `signal` |
| `signals.updateTime` | `signals_updated_at` | |
| `signals.genuses[]` | `genuses SMALLINT[]` | lookup `genus` |
| `rings[]`, `belts[]` | `ring` with `is_belt` false/true, `ordinal` = array index | `type`: lookup `ring_type` |
| `rings[].signals.signals{}` | `ring_signal` | lookup `signal` |
| `rings[].signals.updateTime` | `ring.signals_updated_at` | |
| `stations[]` | `station` with `body_id64` = this body | |

## station (`stations[]`, `bodies[].stations[]`)

| JSON | Column | Notes |
|---|---|---|
| `id` | `station.id` | market id, PK (unique across all dumps surveyed, incl. 56k carriers) |
| `name`, `updateTime`, `realName`, `carrierName` | `name`, `updated_at`, `real_name`, `carrier_name` | |
| `controllingFaction` (string), `controllingFactionState` | `controlling_faction`, `controlling_faction_state` | text, lookup `faction_state` |
| `distanceToArrival`, `latitude`, `longitude` | same | |
| `primaryEconomy`, `secondaryEconomy`, `allegiance`, `government` | same | lookups |
| `economies{}` | `station_economy(economy, share)` | |
| `type`, `state`, `carrierDockingAccess` | same | lookup `station_type`, `station_state`, `carrier_docking_access` |
| `landingPads.{large,medium,small}` | `pads_*` | |
| `services[]` | `services SMALLINT[]` | lookup `service` |
| `market` | `market_updated_at` (NULL = no market object) | |
| `market.commodities[]` | `market_commodity(commodity_id, buy_price, sell_price, demand, supply)` | `name`, `symbol`, `category` -> `commodity` |
| `market.prohibitedCommodities[]` | `prohibited_commodities TEXT[]` | |
| `shipyard.updateTime`, `shipyard.ships[]` | `shipyard_updated_at`, `ship_ids INTEGER[]` | `name`, `symbol` -> `ship` |
| `outfitting.updateTime`, `outfitting.modules[]` | `outfitting_updated_at`, `module_variant_ids INTEGER[]` | `moduleId`, `name`, `symbol`, `class`, `rating`, `category`, `ship` -> `module_variant` |

## Design notes

- Why arrays for services, ships, modules, genuses and powers: they are pure id lists with no per-item
  values. As rows they would be the biggest tables (71.5M outfitting rows in galaxy_populated alone,
  ~40 bytes each); as `INTEGER[]` they cost ~4 bytes per item and stay queryable (`module_variant_ids @> '{42}'`,
  with GIN indexes).
- Why rows for markets and signals: each item carries its own values.
- Why columns for compositions and arrays for parents: they were the two biggest row tables per body
  (610M bodies galaxy-wide, ~1.9 parents and ~7.6 composition entries each; 24-byte tuple header per
  row). The component sets are closed game data - 41 across the full galaxy survey - so fixed nullable
  columns cost ~8 bytes per present value and nothing for absent ones (NULL bitmap).
- One `lookup` table instead of ~30 enum tables or PostgreSQL enums: the value sets drift with game
  updates (new spectral classes, `DamagedHuman`, `PlanetAnomaly`...), and a new value must never fail the import.
- Faction names stay `TEXT` per system: ~650k rows, small; a faction dictionary can come later if needed.
