-- Run after the bulk load. Primary keys double as the duplicate check; foreign keys as the
-- referential check. Each statement builds its index in one pass over a fully loaded table.
SET search_path = spansh;
SET maintenance_work_mem = '2GB';

ALTER TABLE system           ADD PRIMARY KEY (id64);
ALTER TABLE system_faction   ADD PRIMARY KEY (system_id64, name);
ALTER TABLE faction_state    ADD PRIMARY KEY (system_id64, faction_name, bucket, state);
ALTER TABLE power_conflict   ADD PRIMARY KEY (system_id64, power);
ALTER TABLE thargoid_war     ADD PRIMARY KEY (system_id64);
ALTER TABLE body             ADD PRIMARY KEY (id64);
ALTER TABLE body_parent      ADD PRIMARY KEY (body_id64, ordinal);
ALTER TABLE body_composition ADD PRIMARY KEY (body_id64, component);
ALTER TABLE body_signal      ADD PRIMARY KEY (body_id64, signal);
ALTER TABLE ring             ADD PRIMARY KEY (body_id64, is_belt, ordinal);
ALTER TABLE ring_signal      ADD PRIMARY KEY (body_id64, is_belt, ordinal, signal);
ALTER TABLE station          ADD PRIMARY KEY (id);
ALTER TABLE station_economy  ADD PRIMARY KEY (station_id, economy);
ALTER TABLE market_commodity ADD PRIMARY KEY (station_id, commodity_id);

ALTER TABLE system_faction   ADD FOREIGN KEY (system_id64) REFERENCES system (id64);
ALTER TABLE faction_state    ADD FOREIGN KEY (system_id64, faction_name) REFERENCES system_faction (system_id64, name);
ALTER TABLE power_conflict   ADD FOREIGN KEY (system_id64) REFERENCES system (id64);
ALTER TABLE thargoid_war     ADD FOREIGN KEY (system_id64) REFERENCES system (id64);
ALTER TABLE body             ADD FOREIGN KEY (system_id64) REFERENCES system (id64);
ALTER TABLE body_parent      ADD FOREIGN KEY (body_id64) REFERENCES body (id64);
ALTER TABLE body_composition ADD FOREIGN KEY (body_id64) REFERENCES body (id64);
ALTER TABLE body_signal      ADD FOREIGN KEY (body_id64) REFERENCES body (id64);
ALTER TABLE ring             ADD FOREIGN KEY (body_id64) REFERENCES body (id64);
ALTER TABLE ring_signal      ADD FOREIGN KEY (body_id64, is_belt, ordinal) REFERENCES ring (body_id64, is_belt, ordinal);
ALTER TABLE station          ADD FOREIGN KEY (system_id64) REFERENCES system (id64);
ALTER TABLE station          ADD FOREIGN KEY (body_id64) REFERENCES body (id64);
ALTER TABLE station_economy  ADD FOREIGN KEY (station_id) REFERENCES station (id);
ALTER TABLE market_commodity ADD FOREIGN KEY (station_id) REFERENCES station (id);
ALTER TABLE market_commodity ADD FOREIGN KEY (commodity_id) REFERENCES commodity (id);

-- Lookup references are not declared as foreign keys: ~30 columns reference one lookup table, and
-- validating each FK costs a full scan of its table. The importer is the only writer; the
-- integration tests check that every lookup id used exists with the expected kind.

-- Secondary indexes for common access paths.
CREATE INDEX ON system (name);
CREATE INDEX ON body (system_id64);
CREATE INDEX ON station (system_id64);
CREATE INDEX ON market_commodity (commodity_id);
CREATE INDEX ON station USING gin (module_ids);
CREATE INDEX ON station USING gin (ship_ids);

ANALYZE;
