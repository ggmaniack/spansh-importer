-- Spansh galaxy schema, bulk-load phase.
-- Big tables are created without primary keys, foreign keys or indexes; 002_constraints.sql adds
-- them after the load (it also proves uniqueness: ADD PRIMARY KEY fails on duplicates).
-- Column order within each table puts 8-byte types first to minimise alignment padding.
-- id64 values are u64 in the source, stored as BIGINT by bit reinterpretation (u64 as i64);
-- every id64 observed so far is < 2^63, so stored values equal the source values.

CREATE SCHEMA IF NOT EXISTS spansh;
SET search_path = spansh;

-- Enum-like strings (allegiance, economy, body sub type, service, material, ...). One table, one
-- global SMALLINT id space; the importer adds values as it meets them (committed before use).
CREATE TABLE lookup (
    id   SMALLINT PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    UNIQUE (kind, name)
);

-- Frontier catalogue entities, keyed by their numeric ids. Attributes are checked for consistency
-- on every occurrence; conflicts are logged to import_issue and kept in the station's extra.
CREATE TABLE commodity (
    id       INTEGER PRIMARY KEY,      -- commodityId
    category SMALLINT NOT NULL,        -- lookup 'commodity_category'
    symbol   TEXT NOT NULL,
    name     TEXT NOT NULL
);

CREATE TABLE module (
    id       INTEGER PRIMARY KEY,      -- moduleId
    class    SMALLINT NOT NULL,
    category SMALLINT NOT NULL,        -- lookup 'module_category'
    rating   TEXT NOT NULL,
    symbol   TEXT NOT NULL,
    name     TEXT NOT NULL,
    ship     TEXT                      -- ship restriction, by ship name
);

CREATE TABLE ship (
    id     INTEGER PRIMARY KEY,        -- shipId
    symbol TEXT NOT NULL,
    name   TEXT NOT NULL
);

-- Import bookkeeping -------------------------------------------------------------------------

CREATE TABLE import_run (
    id             SERIAL PRIMARY KEY,
    started_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at    TIMESTAMPTZ,
    status         TEXT NOT NULL DEFAULT 'running',  -- running | done | failed | aborted
    source         TEXT NOT NULL,                    -- URL or canonical path
    -- source fingerprint: any difference on resume means a different file
    content_length BIGINT,
    etag           TEXT,
    last_modified  TEXT,
    file_mtime     TIMESTAMPTZ,
    gzip_crc32     BIGINT,
    gzip_isize     BIGINT,
    options        JSONB
);

-- One row per committed batch, written in the same transaction as the batch's data.
-- A restart skips every system whose ordinal lies in a committed range of the same run.
CREATE TABLE import_batch (
    run_id        INTEGER NOT NULL REFERENCES import_run (id),
    first_ordinal BIGINT NOT NULL,
    last_ordinal  BIGINT NOT NULL,
    systems       INTEGER NOT NULL,
    json_bytes    BIGINT NOT NULL,
    committed_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, first_ordinal)
);

-- Problems that did not stop the import (bounded per run by the importer).
CREATE TABLE import_issue (
    run_id      INTEGER NOT NULL REFERENCES import_run (id),
    ordinal     BIGINT,
    system_id64 BIGINT,
    path        TEXT,              -- JSON path, e.g. bodies[3].stations[0].market
    kind        TEXT NOT NULL,     -- e.g. catalogue_conflict, int_overflow, parse_error
    detail      TEXT NOT NULL      -- truncated
);

-- Systems ------------------------------------------------------------------------------------

CREATE TABLE system (
    id64                         BIGINT NOT NULL,
    x                            DOUBLE PRECISION NOT NULL,
    y                            DOUBLE PRECISION NOT NULL,
    z                            DOUBLE PRECISION NOT NULL,
    population                   BIGINT,
    power_state_control_progress DOUBLE PRECISION,
    power_state_reinforcement    DOUBLE PRECISION,
    power_state_undermining      DOUBLE PRECISION,
    updated_at                   TIMESTAMPTZ NOT NULL,   -- "date"
    ts_factions                  TIMESTAMPTZ,            -- timestamps.factions
    ts_power_state               TIMESTAMPTZ,
    ts_powers                    TIMESTAMPTZ,
    ts_controlling_power         TIMESTAMPTZ,
    ts_thargoid_war              TIMESTAMPTZ,
    body_count                   SMALLINT,
    allegiance                   SMALLINT,               -- lookup 'allegiance'
    government                   SMALLINT,               -- lookup 'government'
    primary_economy              SMALLINT,               -- lookup 'economy'
    secondary_economy            SMALLINT,
    security                     SMALLINT,               -- lookup 'security'
    controlling_power            SMALLINT,               -- lookup 'power'
    power_state                  SMALLINT,               -- lookup 'power_state'
    powers                       SMALLINT[],             -- lookup 'power'; NULL = key absent
    name                         TEXT NOT NULL,
    controlling_faction          TEXT,                   -- controllingFaction.name
    extra                        JSONB                   -- source fields with no column (see field-mapping.md)
);

CREATE TABLE system_faction (
    system_id64    BIGINT NOT NULL,
    influence      DOUBLE PRECISION,
    allegiance     SMALLINT,          -- lookup 'allegiance'
    government     SMALLINT,          -- lookup 'government'
    state          SMALLINT,          -- lookup 'faction_state'
    is_controlling BOOLEAN NOT NULL,
    in_factions    BOOLEAN NOT NULL,  -- false: only present as controllingFaction
    name           TEXT NOT NULL
);

CREATE TABLE faction_state (
    system_id64  BIGINT NOT NULL,
    trend        DOUBLE PRECISION,
    bucket       SMALLINT NOT NULL,   -- lookup 'faction_state_bucket': active | pending | recovering
    state        SMALLINT NOT NULL,   -- lookup 'faction_state'
    faction_name TEXT NOT NULL
);

CREATE TABLE power_conflict (
    system_id64 BIGINT NOT NULL,
    progress    DOUBLE PRECISION NOT NULL,
    power       SMALLINT NOT NULL     -- lookup 'power'
);

CREATE TABLE thargoid_war (
    system_id64     BIGINT NOT NULL,
    progress        DOUBLE PRECISION,
    days_remaining  DOUBLE PRECISION,
    ports_remaining DOUBLE PRECISION,
    current_state   SMALLINT,         -- lookup 'thargoid_state'
    success_state   SMALLINT,
    failure_state   SMALLINT,
    success_reached BOOLEAN
);

-- Bodies -------------------------------------------------------------------------------------

CREATE TABLE body (
    id64                   BIGINT NOT NULL,          -- = system_id64 | body_id << 55 (verified on 16.6M bodies)
    system_id64            BIGINT NOT NULL,
    distance_to_arrival    DOUBLE PRECISION,
    orbital_period         DOUBLE PRECISION,
    semi_major_axis        DOUBLE PRECISION,
    orbital_eccentricity   DOUBLE PRECISION,
    orbital_inclination    DOUBLE PRECISION,
    arg_of_periapsis       DOUBLE PRECISION,
    mean_anomaly           DOUBLE PRECISION,
    ascending_node         DOUBLE PRECISION,
    rotational_period      DOUBLE PRECISION,
    axial_tilt             DOUBLE PRECISION,
    surface_temperature    DOUBLE PRECISION,
    absolute_magnitude     DOUBLE PRECISION,         -- stars
    solar_masses           DOUBLE PRECISION,
    solar_radius           DOUBLE PRECISION,
    gravity                DOUBLE PRECISION,         -- planets
    earth_masses           DOUBLE PRECISION,
    radius                 DOUBLE PRECISION,
    surface_pressure       DOUBLE PRECISION,
    updated_at             TIMESTAMPTZ NOT NULL,     -- updateTime
    ts_mean_anomaly        TIMESTAMPTZ,              -- timestamps.meanAnomaly
    ts_distance_to_arrival TIMESTAMPTZ,
    signals_updated_at     TIMESTAMPTZ,              -- signals.updateTime
    age                    INTEGER,
    body_id                SMALLINT NOT NULL,
    type                   SMALLINT,                 -- lookup 'body_type'
    sub_type               SMALLINT,                 -- lookup 'body_sub_type'
    spectral_class         SMALLINT,                 -- lookup 'spectral_class'
    luminosity             SMALLINT,                 -- lookup 'luminosity'
    volcanism              SMALLINT,                 -- lookup 'volcanism'
    atmosphere             SMALLINT,                 -- lookup 'atmosphere'
    terraforming_state     SMALLINT,                 -- lookup 'terraforming_state'
    reserve_level          SMALLINT,                 -- lookup 'reserve_level'
    main_star              BOOLEAN,
    is_landable            BOOLEAN,
    tidally_locked         BOOLEAN,                  -- rotationalPeriodTidallyLocked
    genuses                SMALLINT[],               -- lookup 'genus'; NULL = key absent
    name                   TEXT NOT NULL,
    extra                  JSONB
);

-- parents[]: ordinal 0 is the immediate parent, then up the chain.
CREATE TABLE body_parent (
    body_id64      BIGINT NOT NULL,
    ordinal        SMALLINT NOT NULL,
    kind           SMALLINT NOT NULL,   -- lookup 'parent_kind': Null (barycentre) | Star | Planet | Ring
    parent_body_id SMALLINT NOT NULL    -- bodyId within the same system
);

-- atmosphereComposition, solidComposition and materials; the component's lookup kind says which.
CREATE TABLE body_composition (
    body_id64 BIGINT NOT NULL,
    percent   DOUBLE PRECISION NOT NULL,
    component SMALLINT NOT NULL         -- lookup 'atmosphere_gas' | 'solid' | 'material'
);

CREATE TABLE body_signal (
    body_id64 BIGINT NOT NULL,
    count     INTEGER NOT NULL,
    signal    SMALLINT NOT NULL         -- lookup 'signal'
);

-- rings[] and belts[]
CREATE TABLE ring (
    body_id64          BIGINT NOT NULL,
    id64               BIGINT,          -- present on ~half of rings only
    mass               DOUBLE PRECISION NOT NULL,
    inner_radius       DOUBLE PRECISION NOT NULL,
    outer_radius       DOUBLE PRECISION NOT NULL,
    signals_updated_at TIMESTAMPTZ,
    ordinal            SMALLINT NOT NULL,   -- position in rings[] / belts[]
    type               SMALLINT NOT NULL,   -- lookup 'ring_type'
    is_belt            BOOLEAN NOT NULL,
    name               TEXT NOT NULL
);

CREATE TABLE ring_signal (
    body_id64 BIGINT NOT NULL,
    count     INTEGER NOT NULL,
    ordinal   SMALLINT NOT NULL,
    signal    SMALLINT NOT NULL,        -- lookup 'signal'
    is_belt   BOOLEAN NOT NULL
);

-- Stations -----------------------------------------------------------------------------------

CREATE TABLE station (
    id                        BIGINT NOT NULL,       -- market id
    system_id64               BIGINT NOT NULL,
    body_id64                 BIGINT,                -- set for stations listed under bodies[].stations
    distance_to_arrival       DOUBLE PRECISION,
    latitude                  DOUBLE PRECISION,
    longitude                 DOUBLE PRECISION,
    updated_at                TIMESTAMPTZ NOT NULL,
    market_updated_at         TIMESTAMPTZ,           -- NULL = no market object
    shipyard_updated_at       TIMESTAMPTZ,
    outfitting_updated_at     TIMESTAMPTZ,
    controlling_faction_state SMALLINT,              -- lookup 'faction_state'
    primary_economy           SMALLINT,              -- lookup 'economy'
    secondary_economy         SMALLINT,
    allegiance                SMALLINT,
    government                SMALLINT,
    type                      SMALLINT,              -- lookup 'station_type'
    state                     SMALLINT,              -- lookup 'station_state'
    carrier_docking_access    SMALLINT,              -- lookup 'carrier_docking_access'
    pads_large                SMALLINT,
    pads_medium               SMALLINT,
    pads_small                SMALLINT,
    services                  SMALLINT[],            -- lookup 'service'; NULL = key absent
    ship_ids                  INTEGER[],             -- shipyard.ships[].shipId -> ship
    module_ids                INTEGER[],             -- outfitting.modules[].moduleId -> module
    prohibited_commodities    TEXT[],                -- market.prohibitedCommodities
    name                      TEXT NOT NULL,
    real_name                 TEXT,
    carrier_name              TEXT,
    controlling_faction       TEXT,
    extra                     JSONB
);

CREATE TABLE station_economy (
    station_id BIGINT NOT NULL,
    share      DOUBLE PRECISION NOT NULL,
    economy    SMALLINT NOT NULL        -- lookup 'economy'
);

CREATE TABLE market_commodity (
    station_id   BIGINT NOT NULL,
    commodity_id INTEGER NOT NULL,
    buy_price    INTEGER NOT NULL,
    sell_price   INTEGER NOT NULL,
    demand       INTEGER NOT NULL,
    supply       INTEGER NOT NULL
);
