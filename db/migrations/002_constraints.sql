-- Run after the bulk load by `finalize`. Each statement commits on its own, and `finalize` skips a
-- statement whose named constraint already exists (or, for VALIDATE, is already validated), so an
-- interrupted run resumes at the first unfinished step. Indexes use IF NOT EXISTS for the same reason.
-- Names are PostgreSQL's defaults, so databases finalized by earlier versions are recognised.
SET maintenance_work_mem = '2GB';

-- Primary keys: building the unique index is the duplicate check.
ALTER TABLE system           ADD CONSTRAINT system_pkey           PRIMARY KEY (id64);
ALTER TABLE system_faction   ADD CONSTRAINT system_faction_pkey   PRIMARY KEY (system_id64, name);
ALTER TABLE faction_state    ADD CONSTRAINT faction_state_pkey    PRIMARY KEY (system_id64, faction_name, bucket, state);
ALTER TABLE power_conflict   ADD CONSTRAINT power_conflict_pkey   PRIMARY KEY (system_id64, power);
ALTER TABLE thargoid_war     ADD CONSTRAINT thargoid_war_pkey     PRIMARY KEY (system_id64);
ALTER TABLE body             ADD CONSTRAINT body_pkey             PRIMARY KEY (id64);
ALTER TABLE body_signal      ADD CONSTRAINT body_signal_pkey      PRIMARY KEY (body_id64, signal);
ALTER TABLE ring             ADD CONSTRAINT ring_pkey             PRIMARY KEY (body_id64, is_belt, ordinal);
ALTER TABLE ring_signal      ADD CONSTRAINT ring_signal_pkey      PRIMARY KEY (body_id64, is_belt, ordinal, signal);
ALTER TABLE station          ADD CONSTRAINT station_pkey          PRIMARY KEY (id);
ALTER TABLE station_economy  ADD CONSTRAINT station_economy_pkey  PRIMARY KEY (station_id, economy);
ALTER TABLE market_commodity ADD CONSTRAINT market_commodity_pkey PRIMARY KEY (station_id, commodity_id);

-- Foreign keys are added NOT VALID (instant: enforced for new writes only), then checked against
-- the existing rows by VALIDATE, one constraint per transaction.
ALTER TABLE system_faction   ADD CONSTRAINT system_faction_system_id64_fkey  FOREIGN KEY (system_id64) REFERENCES system (id64) NOT VALID;
ALTER TABLE faction_state    ADD CONSTRAINT faction_state_system_id64_faction_name_fkey  FOREIGN KEY (system_id64, faction_name) REFERENCES system_faction (system_id64, name) NOT VALID;
ALTER TABLE power_conflict   ADD CONSTRAINT power_conflict_system_id64_fkey  FOREIGN KEY (system_id64) REFERENCES system (id64) NOT VALID;
ALTER TABLE thargoid_war     ADD CONSTRAINT thargoid_war_system_id64_fkey  FOREIGN KEY (system_id64) REFERENCES system (id64) NOT VALID;
ALTER TABLE body             ADD CONSTRAINT body_system_id64_fkey  FOREIGN KEY (system_id64) REFERENCES system (id64) NOT VALID;
ALTER TABLE body_signal      ADD CONSTRAINT body_signal_body_id64_fkey  FOREIGN KEY (body_id64) REFERENCES body (id64) NOT VALID;
ALTER TABLE ring             ADD CONSTRAINT ring_body_id64_fkey  FOREIGN KEY (body_id64) REFERENCES body (id64) NOT VALID;
ALTER TABLE ring_signal      ADD CONSTRAINT ring_signal_body_id64_is_belt_ordinal_fkey  FOREIGN KEY (body_id64, is_belt, ordinal) REFERENCES ring (body_id64, is_belt, ordinal) NOT VALID;
ALTER TABLE station          ADD CONSTRAINT station_system_id64_fkey  FOREIGN KEY (system_id64) REFERENCES system (id64) NOT VALID;
ALTER TABLE station          ADD CONSTRAINT station_body_id64_fkey  FOREIGN KEY (body_id64) REFERENCES body (id64) NOT VALID;
ALTER TABLE station_economy  ADD CONSTRAINT station_economy_station_id_fkey  FOREIGN KEY (station_id) REFERENCES station (id) NOT VALID;
ALTER TABLE market_commodity ADD CONSTRAINT market_commodity_station_id_fkey FOREIGN KEY (station_id) REFERENCES station (id) NOT VALID;
ALTER TABLE market_commodity ADD CONSTRAINT market_commodity_commodity_id_fkey FOREIGN KEY (commodity_id) REFERENCES commodity (id) NOT VALID;

ALTER TABLE system_faction   VALIDATE CONSTRAINT system_faction_system_id64_fkey;
ALTER TABLE faction_state    VALIDATE CONSTRAINT faction_state_system_id64_faction_name_fkey;
ALTER TABLE power_conflict   VALIDATE CONSTRAINT power_conflict_system_id64_fkey;
ALTER TABLE thargoid_war     VALIDATE CONSTRAINT thargoid_war_system_id64_fkey;
ALTER TABLE body             VALIDATE CONSTRAINT body_system_id64_fkey;
ALTER TABLE body_signal      VALIDATE CONSTRAINT body_signal_body_id64_fkey;
ALTER TABLE ring             VALIDATE CONSTRAINT ring_body_id64_fkey;
ALTER TABLE ring_signal      VALIDATE CONSTRAINT ring_signal_body_id64_is_belt_ordinal_fkey;
ALTER TABLE station          VALIDATE CONSTRAINT station_system_id64_fkey;
ALTER TABLE station          VALIDATE CONSTRAINT station_body_id64_fkey;
ALTER TABLE station_economy  VALIDATE CONSTRAINT station_economy_station_id_fkey;
ALTER TABLE market_commodity VALIDATE CONSTRAINT market_commodity_station_id_fkey;
ALTER TABLE market_commodity VALIDATE CONSTRAINT market_commodity_commodity_id_fkey;

-- Lookup references are not declared as foreign keys: ~30 columns reference one lookup table, and
-- validating each FK costs a full scan of its table. The importer is the only writer; the
-- integration tests check that every lookup id used exists with the expected kind.

-- Secondary indexes for common access paths.
CREATE INDEX IF NOT EXISTS system_name_idx                ON system (name);
CREATE INDEX IF NOT EXISTS body_system_id64_idx           ON body (system_id64);
CREATE INDEX IF NOT EXISTS station_system_id64_idx        ON station (system_id64);
CREATE INDEX IF NOT EXISTS market_commodity_commodity_id_idx ON market_commodity (commodity_id);
CREATE INDEX IF NOT EXISTS station_module_variant_ids_idx ON station USING gin (module_variant_ids);
CREATE INDEX IF NOT EXISTS module_variant_module_id_idx   ON module_variant (module_id);
CREATE INDEX IF NOT EXISTS station_ship_ids_idx           ON station USING gin (ship_ids);

-- ANALYZE's sample sees only ~75k distinct systems among 610M bodies (rows are clustered by system),
-- which made GROUP BY system_id64 pick a plan that ran for hours. About 3 bodies per system.
ALTER TABLE body ALTER COLUMN system_id64 SET (n_distinct = -0.33);
ANALYZE;
