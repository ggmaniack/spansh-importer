//! End-to-end tests against a real PostgreSQL. Each test uses its own freshly created database.
//! Server: $TEST_PG (default postgres://postgres@localhost:5432), e.g. the instance from db/pg.ps1.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Output};

use postgres::{Client, NoTls};
use serde_json::Value;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/spanshtools-100.json.gz");

fn server() -> String {
    std::env::var("TEST_PG").unwrap_or_else(|_| "postgres://postgres@localhost:5432".into())
}

fn fresh_db(name: &str) -> String {
    let mut c = Client::connect(&format!("{}/postgres", server()), NoTls).expect("connect to test server");
    c.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).unwrap();
    c.batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
    let url = format!("{}/{name}", server());
    run_ok(&["init"], &url);
    url
}

fn run(args: &[&str], db: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spansh-importer")).args(args).env("DATABASE_URL", db).output().unwrap()
}

fn run_ok(args: &[&str], db: &str) -> String {
    let o = run(args, db);
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(o.status.success(), "{args:?} failed:\n{err}");
    err
}

fn ingest(db: &str, source: &str, extra: &[&str]) -> Output {
    let mut args = vec!["ingest", "--source", source];
    args.extend_from_slice(extra);
    run(&args, db)
}

fn ingest_ok(db: &str, source: &str, extra: &[&str]) -> String {
    let o = ingest(db, source, extra);
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(o.status.success(), "ingest failed:\n{err}");
    err
}

fn fixture_systems() -> Vec<Value> {
    let gz = flate2::read::MultiGzDecoder::new(std::fs::File::open(FIXTURE).unwrap());
    BufReader::new(gz)
        .lines()
        .map(Result::unwrap)
        .map(|l| l.trim().trim_end_matches(',').to_owned())
        .filter(|l| l.starts_with('{'))
        .map(|l| serde_json::from_str(&l).unwrap())
        .collect()
}

fn db(url: &str) -> Client {
    let mut c = Client::connect(url, NoTls).unwrap();
    c.batch_execute("SET search_path = spansh; SET TIME ZONE 'UTC'").unwrap();
    c
}

const DATA_TABLES: &[&str] = &[
    "system",
    "system_faction",
    "faction_state",
    "power_conflict",
    "thargoid_war",
    "body",
    "body_parent",
    "body_composition",
    "body_signal",
    "ring",
    "ring_signal",
    "station",
    "station_economy",
    "market_commodity",
];

fn counts(c: &mut Client) -> HashMap<&'static str, i64> {
    DATA_TABLES.iter().map(|t| (*t, c.query_one(&format!("SELECT count(*) FROM {t}"), &[]).unwrap().get(0))).collect()
}

fn arr(v: &Value, k: &str) -> Vec<Value> {
    v.get(k).and_then(Value::as_array).cloned().unwrap_or_default()
}

fn obj_len(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_object).map_or(0, |m| m.len() as i64)
}

/// Row counts the fixture should produce, derived from the JSON independently of the importer.
fn expected_counts(systems: &[Value]) -> HashMap<&'static str, i64> {
    let mut e: HashMap<&'static str, i64> = DATA_TABLES.iter().map(|t| (*t, 0)).collect();
    let mut add = |t: &'static str, n: i64| *e.get_mut(t).unwrap() += n;
    for s in systems {
        add("system", 1);
        let factions = arr(s, "factions");
        let states = |f: &Value| ["activeStates", "pendingStates", "recoveringStates"].iter().map(|k| arr(f, k).len() as i64).sum::<i64>();
        add("system_faction", factions.len() as i64);
        add("faction_state", factions.iter().map(states).sum());
        if let Some(cf) = s.get("controllingFaction").filter(|v| v.is_object()) {
            if !factions.iter().any(|f| f["name"] == cf["name"]) {
                add("system_faction", 1);
                add("faction_state", states(cf));
            }
        }
        add("power_conflict", arr(s, "powerConflictProgress").len() as i64);
        add("thargoid_war", s.get("thargoidWar").is_some_and(Value::is_object) as i64);
        let mut stations = arr(s, "stations");
        for b in arr(s, "bodies") {
            add("body", 1);
            add("body_parent", arr(&b, "parents").len() as i64);
            add("body_composition", obj_len(&b, "atmosphereComposition") + obj_len(&b, "solidComposition") + obj_len(&b, "materials"));
            add("body_signal", b.get("signals").map_or(0, |sg| obj_len(sg, "signals")));
            for r in arr(&b, "rings").into_iter().chain(arr(&b, "belts")) {
                add("ring", 1);
                add("ring_signal", r.get("signals").map_or(0, |sg| obj_len(sg, "signals")));
            }
            stations.extend(arr(&b, "stations"));
        }
        for st in stations {
            add("station", 1);
            add("station_economy", obj_len(&st, "economies"));
            add("market_commodity", st.get("market").map_or(0, |m| arr(m, "commodities").len() as i64));
        }
    }
    e
}

#[test]
fn imports_fixture_with_expected_rows_and_keys() {
    let url = fresh_db("spansh_test_full");
    let out = ingest_ok(&url, FIXTURE, &[]);
    assert!(out.contains("complete"), "{out}");
    run_ok(&["finalize"], &url); // primary and foreign keys hold
    let mut c = db(&url);
    assert_eq!(counts(&mut c), expected_counts(&fixture_systems()));
    let status: String = c.query_one("SELECT status FROM import_run", &[]).unwrap().get(0);
    assert_eq!(status, "done");

    // Every lookup id used must exist with the kind its column expects.
    for (table, col, kind) in [
        ("system", "allegiance", "allegiance"),
        ("system", "security", "security"),
        ("body", "sub_type", "body_sub_type"),
        ("body", "spectral_class", "spectral_class"),
        ("body_composition", "component", NULL_KIND),
        ("station", "type", "station_type"),
        ("body_parent", "kind", "parent_kind"),
    ] {
        let kind_check = if kind == NULL_KIND { "l.kind IN ('atmosphere_gas','solid','material')".to_owned() } else { format!("l.kind = '{kind}'") };
        let bad: i64 = c
            .query_one(&format!("SELECT count(*) FROM {table} t LEFT JOIN lookup l ON l.id = t.{col} AND {kind_check} WHERE t.{col} IS NOT NULL AND l.id IS NULL"), &[])
            .unwrap()
            .get(0);
        assert_eq!(bad, 0, "{table}.{col} has ids outside lookup kind {kind}");
    }
}
const NULL_KIND: &str = "<composition>";

/// Normalises timestamps from either side to `YYYY-MM-DDTHH:MM:SS[.frac]+00:00` without trailing zeros.
fn norm_ts(s: &str) -> String {
    let mut s = s.replacen(' ', "T", 1).replace('Z', "+00:00");
    if s.ends_with("+00") {
        s.push_str(":00");
    }
    let (main, off) = s.split_at(s.len() - 6);
    let main = if main.contains('.') { main.trim_end_matches('0').trim_end_matches('.') } else { main };
    format!("{main}{off}")
}

/// Compares mapped scalar fields of every system, body and station with what PostgreSQL returns.
#[test]
fn round_trips_scalar_fields() {
    let url = fresh_db("spansh_test_roundtrip");
    ingest_ok(&url, FIXTURE, &[]);
    let mut c = db(&url);
    let names: HashMap<i16, String> = c.query("SELECT id, name FROM lookup", &[]).unwrap().iter().map(|r| (r.get(0), r.get(1))).collect();
    let rows = |c: &mut Client, sql: &str| -> HashMap<i64, Value> {
        c.query(sql, &[]).unwrap().iter().map(|r| (r.get::<_, i64>(0), r.get::<_, Value>(1))).collect()
    };
    let systems = rows(&mut c, "SELECT id64, to_jsonb(t) FROM system t");
    let bodies = rows(&mut c, "SELECT id64, to_jsonb(t) FROM body t");
    let stations = rows(&mut c, "SELECT id, to_jsonb(t) FROM station t");

    // (json key, column, how to compare)
    let sys_map: &[(&str, &str, &str)] = &[
        ("name", "name", "eq"),
        ("population", "population", "num"),
        ("bodyCount", "body_count", "num"),
        ("allegiance", "allegiance", "lookup"),
        ("government", "government", "lookup"),
        ("primaryEconomy", "primary_economy", "lookup"),
        ("security", "security", "lookup"),
        ("controllingPower", "controlling_power", "lookup"),
        ("powerState", "power_state", "lookup"),
        ("date", "updated_at", "ts"),
    ];
    let body_map: &[(&str, &str, &str)] = &[
        ("name", "name", "eq"),
        ("bodyId", "body_id", "num"),
        ("type", "type", "lookup"),
        ("subType", "sub_type", "lookup"),
        ("updateTime", "updated_at", "ts"),
        ("distanceToArrival", "distance_to_arrival", "num"),
        ("orbitalPeriod", "orbital_period", "num"),
        ("semiMajorAxis", "semi_major_axis", "num"),
        ("orbitalEccentricity", "orbital_eccentricity", "num"),
        ("orbitalInclination", "orbital_inclination", "num"),
        ("argOfPeriapsis", "arg_of_periapsis", "num"),
        ("meanAnomaly", "mean_anomaly", "num"),
        ("ascendingNode", "ascending_node", "num"),
        ("rotationalPeriod", "rotational_period", "num"),
        ("rotationalPeriodTidallyLocked", "tidally_locked", "eq"),
        ("axialTilt", "axial_tilt", "num"),
        ("surfaceTemperature", "surface_temperature", "num"),
        ("mainStar", "main_star", "eq"),
        ("age", "age", "num"),
        ("spectralClass", "spectral_class", "lookup"),
        ("luminosity", "luminosity", "lookup"),
        ("absoluteMagnitude", "absolute_magnitude", "num"),
        ("solarMasses", "solar_masses", "num"),
        ("solarRadius", "solar_radius", "num"),
        ("isLandable", "is_landable", "eq"),
        ("gravity", "gravity", "num"),
        ("earthMasses", "earth_masses", "num"),
        ("radius", "radius", "num"),
        ("surfacePressure", "surface_pressure", "num"),
        ("volcanismType", "volcanism", "lookup"),
        ("atmosphereType", "atmosphere", "lookup"),
        ("terraformingState", "terraforming_state", "lookup"),
        ("reserveLevel", "reserve_level", "lookup"),
    ];
    let station_map: &[(&str, &str, &str)] = &[
        ("name", "name", "eq"),
        ("updateTime", "updated_at", "ts"),
        ("controllingFaction", "controlling_faction", "eq"),
        ("controllingFactionState", "controlling_faction_state", "lookup"),
        ("distanceToArrival", "distance_to_arrival", "num"),
        ("primaryEconomy", "primary_economy", "lookup"),
        ("allegiance", "allegiance", "lookup"),
        ("government", "government", "lookup"),
        ("type", "type", "lookup"),
        ("latitude", "latitude", "num"),
        ("longitude", "longitude", "num"),
        ("carrierName", "carrier_name", "eq"),
    ];
    let check = |what: &str, src: &Value, row: &Value, map: &[(&str, &str, &str)]| {
        for (key, col, how) in map {
            let s = src.get(*key).cloned().unwrap_or(Value::Null);
            let d = row[*col].clone();
            let ok = match (*how, &s, &d) {
                (_, Value::Null, Value::Null) => true,
                ("num", s, d) => s.as_f64() == d.as_f64() && s.as_f64().is_some(),
                ("lookup", Value::String(s), d) => d.as_i64().and_then(|i| names.get(&(i as i16))) == Some(s),
                ("ts", Value::String(s), Value::String(d)) => norm_ts(s) == norm_ts(d),
                ("eq", s, d) => s == d,
                _ => false,
            };
            assert!(ok, "{what}: {key} source {s} != db {d}");
        }
    };

    let mut n = (0, 0, 0);
    for s in fixture_systems() {
        let id = s["id64"].as_u64().unwrap() as i64;
        let row = &systems[&id];
        check(&format!("system {id}"), &s, row, sys_map);
        for (axis, col) in [("x", "x"), ("y", "y"), ("z", "z")] {
            assert_eq!(s["coords"][axis].as_f64(), row[col].as_f64());
        }
        let mut stations_src = arr(&s, "stations");
        for b in arr(&s, "bodies") {
            let bid = b["id64"].as_u64().unwrap() as i64;
            check(&format!("body {bid}"), &b, &bodies[&bid], body_map);
            assert_eq!(bodies[&bid]["system_id64"].as_i64(), Some(id));
            stations_src.extend(arr(&b, "stations"));
            n.1 += 1;
        }
        for st in stations_src {
            let sid = st["id"].as_u64().unwrap() as i64;
            check(&format!("station {sid}"), &st, &stations[&sid], station_map);
            n.2 += 1;
        }
        n.0 += 1;
    }
    assert_eq!((n.0, n.1, n.2), (systems.len(), bodies.len(), stations.len()));

    // Market prices for one commodity at one station, exact.
    let sol = fixture_systems().into_iter().find(|s| s["name"] == "Sol").unwrap();
    let st = arr(&sol, "stations").into_iter().find(|s| s.get("market").is_some_and(|m| !arr(m, "commodities").is_empty())).unwrap();
    let item = &arr(&st["market"], "commodities")[0];
    let r = c
        .query_one(
            "SELECT buy_price, sell_price, demand, supply FROM market_commodity WHERE station_id = $1 AND commodity_id = $2",
            &[&(st["id"].as_u64().unwrap() as i64), &(item["commodityId"].as_i64().unwrap() as i32)],
        )
        .unwrap();
    for (i, k) in ["buyPrice", "sellPrice", "demand", "supply"].iter().enumerate() {
        assert_eq!(r.get::<_, i32>(i) as i64, item[*k].as_i64().unwrap(), "{k}");
    }
}

#[test]
fn resumes_without_duplicates_and_matches_single_batch_import() {
    let url = fresh_db("spansh_test_resume");
    let out = ingest_ok(&url, FIXTURE, &["--max-systems", "40", "--batch-mib", "0"]);
    assert!(out.contains("stopped early"), "{out}");
    let mut c = db(&url);
    assert_eq!(c.query_one("SELECT count(*) FROM system", &[]).unwrap().get::<_, i64>(0), 40);
    assert_eq!(c.query_one("SELECT status FROM import_run", &[]).unwrap().get::<_, String>(0), "stopped");

    let out = ingest_ok(&url, FIXTURE, &["--batch-mib", "0"]);
    assert!(out.contains("+40 skipped"), "{out}");
    run_ok(&["finalize"], &url); // primary keys prove nothing was imported twice
    assert_eq!(counts(&mut c), expected_counts(&fixture_systems()));

    // A finished run is not repeated.
    let again = ingest(&url, FIXTURE, &[]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already imported"));
}

#[test]
fn failed_batch_rolls_back_and_resume_completes() {
    let url = fresh_db("spansh_test_rollback");
    let systems = fixture_systems();
    let victim = systems[60]["id64"].as_u64().unwrap() as i64;
    let mut c = db(&url);
    c.batch_execute(&format!("ALTER TABLE system ADD CONSTRAINT poison CHECK (id64 <> {victim})")).unwrap();

    let out = ingest(&url, FIXTURE, &["--batch-mib", "0"]);
    assert!(!out.status.success(), "the poisoned system must fail its batch");
    // Batch 61 rolled back entirely: no system, body or batch row from it.
    assert_eq!(c.query_one("SELECT count(*) FROM system", &[]).unwrap().get::<_, i64>(0), 60);
    let bodies_of_victim: i64 = c.query_one("SELECT count(*) FROM body WHERE system_id64 = $1", &[&victim]).unwrap().get(0);
    assert_eq!(bodies_of_victim, 0);
    let max_committed: i64 = c.query_one("SELECT max(last_ordinal) FROM import_batch", &[]).unwrap().get(0);
    assert_eq!(max_committed, 60);
    assert_eq!(c.query_one("SELECT status FROM import_run", &[]).unwrap().get::<_, String>(0), "failed");

    c.batch_execute("ALTER TABLE system DROP CONSTRAINT poison").unwrap();
    ingest_ok(&url, FIXTURE, &["--batch-mib", "0"]);
    run_ok(&["finalize"], &url);
    assert_eq!(counts(&mut c), expected_counts(&systems));
}

#[test]
fn refuses_to_resume_a_changed_source() {
    let url = fresh_db("spansh_test_changed");
    let dir = std::env::temp_dir().join("spansh_test_changed");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dump.json");
    let lines: Vec<String> = BufReader::new(flate2::read::MultiGzDecoder::new(std::fs::File::open(FIXTURE).unwrap())).lines().map(Result::unwrap).collect();
    std::fs::write(&path, lines.join("\n")).unwrap();
    let p = path.to_str().unwrap();
    ingest_ok(&url, p, &["--max-systems", "10"]);

    std::fs::write(&path, lines[..50].join("\n") + "\n]").unwrap();
    let out = ingest(&url, p, &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("source changed"), "{}", String::from_utf8_lossy(&out.stderr));
}
