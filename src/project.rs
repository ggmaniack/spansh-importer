//! Projection of one system's JSON into binary COPY rows, one buffer per table.
//! Every source key is either mapped to a column/row or copied into the owning entity's
//! `extra` JSONB (unknown keys, type mismatches, conflicting catalogue data) - never dropped.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use postgres::Client;
use serde_json::{Map, Value};

use crate::pgcopy::CopyBuf;

/// Destination tables and the COPY column order the projection writes them in.
pub const TABLES: &[(&str, &str)] = &[
    ("system", "id64,name,x,y,z,allegiance,government,primary_economy,secondary_economy,security,population,body_count,updated_at,controlling_power,power_state,powers,power_state_control_progress,power_state_reinforcement,power_state_undermining,ts_factions,ts_power_state,ts_powers,ts_controlling_power,ts_thargoid_war,controlling_faction,extra"),
    ("system_faction", "system_id64,name,allegiance,government,state,influence,is_controlling,in_factions"),
    ("faction_state", "system_id64,faction_name,bucket,state,trend"),
    ("power_conflict", "system_id64,power,progress"),
    ("thargoid_war", "system_id64,current_state,success_state,failure_state,progress,days_remaining,ports_remaining,success_reached"),
    ("body", "id64,system_id64,body_id,name,type,sub_type,updated_at,distance_to_arrival,orbital_period,semi_major_axis,orbital_eccentricity,orbital_inclination,arg_of_periapsis,mean_anomaly,ascending_node,rotational_period,tidally_locked,axial_tilt,surface_temperature,main_star,age,spectral_class,luminosity,absolute_magnitude,solar_masses,solar_radius,is_landable,gravity,earth_masses,radius,surface_pressure,volcanism,atmosphere,terraforming_state,reserve_level,ts_mean_anomaly,ts_distance_to_arrival,ts_ascending_node,signals_updated_at,genuses,parent_kinds,parent_body_ids,\
solid_ice,solid_rock,solid_metal,\
atm_ammonia,atm_argon,atm_carbon_dioxide,atm_helium,atm_hydrogen,atm_iron,atm_methane,atm_neon,atm_nitrogen,atm_oxygen,atm_silicates,atm_sulphur_dioxide,atm_water,\
mat_antimony,mat_arsenic,mat_cadmium,mat_carbon,mat_chromium,mat_germanium,mat_iron,mat_manganese,mat_mercury,mat_molybdenum,mat_nickel,mat_niobium,mat_phosphorus,\
mat_polonium,mat_ruthenium,mat_selenium,mat_sulphur,mat_technetium,mat_tellurium,mat_tin,mat_tungsten,mat_vanadium,mat_yttrium,mat_zinc,mat_zirconium,extra"),
    ("body_signal", "body_id64,signal,count"),
    ("ring", "body_id64,is_belt,ordinal,name,type,mass,inner_radius,outer_radius,id64,signals_updated_at"),
    ("ring_signal", "body_id64,is_belt,ordinal,signal,count"),
    ("station", "id,system_id64,body_id64,name,updated_at,real_name,carrier_name,controlling_faction,controlling_faction_state,distance_to_arrival,primary_economy,secondary_economy,allegiance,government,type,state,carrier_docking_access,latitude,longitude,pads_large,pads_medium,pads_small,services,market_updated_at,prohibited_commodities,shipyard_updated_at,ship_ids,outfitting_updated_at,module_variant_ids,extra"),
    ("station_economy", "station_id,economy,share"),
    ("market_commodity", "station_id,commodity_id,buy_price,sell_price,demand,supply"),
    ("import_issue", "run_id,ordinal,system_id64,path,kind,detail"),
];

#[derive(Clone, Copy)]
pub enum T {
    System,
    SystemFaction,
    FactionState,
    PowerConflict,
    ThargoidWar,
    Body,
    BodySignal,
    Ring,
    RingSignal,
    Station,
    StationEconomy,
    MarketCommodity,
    Issue,
}

pub struct Batch {
    pub tables: Vec<CopyBuf>,
    pub systems: u64,
    pub json_bytes: u64,
    pub first_ordinal: u64,
    pub last_ordinal: u64,
}

impl Batch {
    pub fn new() -> Self {
        Batch {
            tables: TABLES.iter().map(|_| CopyBuf::default()).collect(),
            systems: 0,
            json_bytes: 0,
            first_ordinal: 0,
            last_ordinal: 0,
        }
    }

    pub fn t(&mut self, t: T) -> &mut CopyBuf {
        &mut self.tables[t as usize]
    }

    pub fn bytes(&self) -> usize {
        self.tables.iter().map(|t| t.buf.len()).sum()
    }

    pub fn clear(&mut self) {
        self.tables.iter_mut().for_each(CopyBuf::clear);
        self.systems = 0;
        self.json_bytes = 0;
        self.first_ordinal = 0;
        self.last_ordinal = 0;
    }
}

/// Parses `YYYY-MM-DD[T ]HH:MM:SS[.f{1,6}](Z|±HH|±HH:MM)` into Unix microseconds.
pub fn parse_ts(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let d = b.get(r)?;
        d.iter().all(u8::is_ascii_digit).then(|| d.iter().fold(0i64, |a, c| a * 10 + (c - b'0') as i64))
    };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d, h, mi, sec) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut micros = 0;
    if b[i] == b'.' {
        let end = (i + 1..b.len()).find(|&j| !b[j].is_ascii_digit()).unwrap_or(b.len());
        let digits = end - i - 1;
        if digits == 0 || digits > 6 {
            return None;
        }
        micros = num(i + 1..end)? * 10i64.pow(6 - digits as u32);
        i = end;
    }
    let offset_s = match &b[i..] {
        b"Z" => 0,
        [sign @ (b'+' | b'-'), rest @ ..] => {
            let off = match rest.len() {
                2 => num(i + 1..i + 3)? * 3600,
                5 if rest[2] == b':' => num(i + 1..i + 3)? * 3600 + num(i + 4..i + 6)? * 60,
                _ => return None,
            };
            if *sign == b'-' { -off } else { off }
        }
        _ => return None,
    };
    // Howard Hinnant's days_from_civil.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60 + sec - offset_s) * 1_000_000 + micros)
}

/// Body composition objects and their components, in the body table's column order. The column
/// name is the prefix plus the lowercased component with spaces as underscores.
const COMPOSITIONS: [(&str, &[&str]); 3] = [
    ("solidComposition", &["Ice", "Rock", "Metal"]),
    (
        "atmosphereComposition",
        &["Ammonia", "Argon", "Carbon dioxide", "Helium", "Hydrogen", "Iron", "Methane", "Neon", "Nitrogen", "Oxygen", "Silicates", "Sulphur dioxide", "Water"],
    ),
    (
        "materials",
        &[
            "Antimony", "Arsenic", "Cadmium", "Carbon", "Chromium", "Germanium", "Iron", "Manganese", "Mercury", "Molybdenum", "Nickel", "Niobium", "Phosphorus",
            "Polonium", "Ruthenium", "Selenium", "Sulphur", "Technetium", "Tellurium", "Tin", "Tungsten", "Vanadium", "Yttrium", "Zinc", "Zirconium",
        ],
    ),
];
const COMPOSITION_COLUMNS: usize = 41;

/// (moduleId, class, category, rating, symbol, name, ship)
type ModuleVariant = (i32, i16, i16, String, String, String, Option<String>);

/// Enum-like strings and Frontier catalogue entities, mirrored in memory and inserted (committed)
/// on first sight through a dedicated connection, so batches only ever reference committed rows.
pub struct Dict {
    conn: Client,
    lookups: HashMap<String, HashMap<String, i16>>,
    next_id: i16,
    commodities: HashMap<i32, (String, String, i16)>,
    module_variants: HashMap<ModuleVariant, i32>,
    ships: HashMap<i32, (String, String)>,
}

impl Dict {
    pub fn load(mut conn: Client) -> Result<Self> {
        let mut lookups: HashMap<String, HashMap<String, i16>> = HashMap::new();
        let mut next_id = 1;
        for r in conn.query("SELECT id, kind, name FROM lookup", &[])? {
            let id: i16 = r.get(0);
            next_id = next_id.max(id + 1);
            lookups.entry(r.get(1)).or_default().insert(r.get(2), id);
        }
        let commodities = conn
            .query("SELECT id, symbol, name, category FROM commodity", &[])?
            .iter()
            .map(|r| (r.get(0), (r.get(1), r.get(2), r.get(3))))
            .collect();
        let module_variants = conn
            .query("SELECT id, module_id, class, category, rating, symbol, name, ship FROM module_variant", &[])?
            .iter()
            .map(|r| ((r.get(1), r.get(2), r.get(3), r.get(4), r.get(5), r.get(6), r.get(7)), r.get(0)))
            .collect();
        let ships = conn
            .query("SELECT id, symbol, name FROM ship", &[])?
            .iter()
            .map(|r| (r.get(0), (r.get(1), r.get(2))))
            .collect();
        Ok(Dict { conn, lookups, next_id, commodities, module_variants, ships })
    }

    pub fn lookup(&mut self, kind: &str, name: &str) -> Result<i16> {
        if let Some(id) = self.lookups.get(kind).and_then(|m| m.get(name)) {
            return Ok(*id);
        }
        let id = self.next_id;
        anyhow::ensure!(id < i16::MAX, "lookup id space exhausted");
        self.conn.execute("INSERT INTO lookup (id, kind, name) VALUES ($1, $2, $3)", &[&id, &kind, &name])?;
        self.next_id += 1;
        self.lookups.entry(kind.to_owned()).or_default().insert(name.to_owned(), id);
        Ok(id)
    }

    /// Returns false when the id is already known with different attributes.
    fn commodity(&mut self, id: i32, v: (String, String, i16)) -> Result<bool> {
        if let Some(known) = self.commodities.get(&id) {
            return Ok(*known == v);
        }
        self.conn.execute(
            "INSERT INTO commodity (id, symbol, name, category) VALUES ($1, $2, $3, $4)",
            &[&id, &v.0, &v.1, &v.2],
        )?;
        self.commodities.insert(id, v);
        Ok(true)
    }

    fn module_variant(&mut self, v: ModuleVariant) -> Result<i32> {
        if let Some(id) = self.module_variants.get(&v) {
            return Ok(*id);
        }
        let id = self.module_variants.len() as i32 + 1;
        self.conn.execute(
            "INSERT INTO module_variant (id, module_id, class, category, rating, symbol, name, ship) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[&id, &v.0, &v.1, &v.2, &v.3, &v.4, &v.5, &v.6],
        )?;
        self.module_variants.insert(v, id);
        Ok(id)
    }

    fn ship(&mut self, id: i32, v: (String, String)) -> Result<bool> {
        if let Some(known) = self.ships.get(&id) {
            return Ok(*known == v);
        }
        self.conn.execute("INSERT INTO ship (id, symbol, name) VALUES ($1, $2, $3)", &[&id, &v.0, &v.1])?;
        self.ships.insert(id, v);
        Ok(true)
    }
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
}

fn snippet(v: &Value) -> String {
    truncate(v.to_string(), 500)
}

/// Bounded log of problems that did not stop the import. Rows go out with the system's batch.
pub struct Issues {
    pub run_id: i32,
    pub logged: u64,
    pub total: u64,
    pub cap: u64,
    seen_unknown: HashSet<String>,
    pending: Vec<(String, &'static str, String)>,
}

impl Issues {
    pub fn new(run_id: i32, cap: u64) -> Self {
        Issues { run_id, logged: 0, total: 0, cap, seen_unknown: HashSet::new(), pending: Vec::new() }
    }

    pub fn add(&mut self, path: String, kind: &'static str, detail: String) {
        self.total += 1;
        crate::report::log(crate::report::Level::Warn, "issue", &format!("{kind} at {path}: {}", truncate(detail.clone(), 300)));
        if self.logged < self.cap {
            self.logged += 1;
            self.pending.push((path, kind, truncate(detail, 2000)));
        }
    }

    /// Unknown keys are logged once per path (array indices ignored) per run; the value itself
    /// always goes to `extra`.
    fn unknown(&mut self, path: String, v: &Value) {
        let norm: String = path.chars().filter(|c| !c.is_ascii_digit()).collect();
        if self.seen_unknown.insert(norm) {
            self.add(path, "unknown_field", snippet(v));
        }
    }

    pub fn flush(&mut self, out: &mut CopyBuf, ordinal: u64, system_id64: Option<i64>) {
        for (path, kind, detail) in self.pending.drain(..) {
            out.row(6)
                .i32(Some(self.run_id))
                .i64(Some(ordinal as i64))
                .i64(system_id64)
                .text(Some(&path))
                .text(Some(kind))
                .text(Some(&detail));
        }
    }
}

/// A JSON object being consumed field by field. Whatever is not taken (or does not fit its
/// column) ends up in the entity's `extra` map under `prefix + key`.
struct Fields {
    map: Map<String, Value>,
    prefix: String,
    entity: &'static str,
    /// (key, value, log as type_mismatch)
    misfits: Vec<(String, Value, bool)>,
}

impl Fields {
    fn new(map: Map<String, Value>, prefix: String, entity: &'static str) -> Self {
        Fields { map, prefix, entity, misfits: Vec::new() }
    }

    /// Preserves a value in `extra` whose problem was already logged by the caller.
    fn keep(&mut self, k: impl Into<String>, v: Value) {
        self.misfits.push((k.into(), v, false));
    }

    fn take(&mut self, k: &str) -> Option<Value> {
        match self.map.remove(k) {
            None | Some(Value::Null) => None,
            v => v,
        }
    }

    fn misfit(&mut self, k: impl Into<String>, v: Value) {
        self.misfits.push((k.into(), v, true));
    }

    fn f64(&mut self, k: &str) -> Option<f64> {
        match self.take(k)? {
            Value::Number(n) => n.as_f64(),
            v => {
                self.misfit(k, v);
                None
            }
        }
    }

    fn int<I: TryFrom<i64>>(&mut self, k: &str) -> Option<I> {
        let v = self.take(k)?;
        if let Some(x) = v.as_i64().and_then(|i| I::try_from(i).ok()) {
            return Some(x);
        }
        self.misfit(k, v);
        None
    }

    /// u64 ids, stored as i64 by bit reinterpretation.
    fn id64(&mut self, k: &str) -> Option<i64> {
        let v = self.take(k)?;
        if let Some(x) = v.as_u64() {
            return Some(x as i64);
        }
        self.misfit(k, v);
        None
    }

    fn string(&mut self, k: &str) -> Option<String> {
        match self.take(k)? {
            Value::String(s) => Some(s),
            v => {
                self.misfit(k, v);
                None
            }
        }
    }

    fn bool(&mut self, k: &str) -> Option<bool> {
        match self.take(k)? {
            Value::Bool(b) => Some(b),
            v => {
                self.misfit(k, v);
                None
            }
        }
    }

    fn ts(&mut self, k: &str) -> Option<i64> {
        let v = self.take(k)?;
        if let Some(t) = v.as_str().and_then(parse_ts) {
            return Some(t);
        }
        self.misfit(k, v);
        None
    }

    fn obj(&mut self, k: &str) -> Option<Fields> {
        match self.take(k)? {
            Value::Object(m) => Some(Fields::new(m, format!("{}{k}.", self.prefix), self.entity)),
            v => {
                self.misfit(k, v);
                None
            }
        }
    }

    fn arr(&mut self, k: &str) -> Option<Vec<Value>> {
        match self.take(k)? {
            Value::Array(a) => Some(a),
            v => {
                self.misfit(k, v);
                None
            }
        }
    }

    /// Items of array `k` that pass `checks` become `Fields`; the rest are kept whole as misfits.
    fn items(&mut self, k: &str, checks: &[Check]) -> Option<Vec<Fields>> {
        let items = self.arr(k)?;
        let mut out = Vec::with_capacity(items.len());
        for (i, it) in items.into_iter().enumerate() {
            match it {
                Value::Object(m) if has_map(&m, checks) => out.push(Fields::new(m, format!("{}{k}[{i}].", self.prefix), self.entity)),
                v => self.misfit(format!("{k}[{i}]"), v),
            }
        }
        Some(out)
    }

    fn lookup(&mut self, k: &str, kind: &str, dict: &mut Dict) -> Result<Option<i16>> {
        match self.take(k) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(dict.lookup(kind, &s)?)),
            Some(v) => {
                self.misfit(k, v);
                Ok(None)
            }
        }
    }

    fn lookup_array(&mut self, k: &str, kind: &str, dict: &mut Dict) -> Result<Option<Vec<i16>>> {
        let Some(items) = self.arr(k) else { return Ok(None) };
        let mut ids = Vec::with_capacity(items.len());
        for (i, it) in items.into_iter().enumerate() {
            match it {
                Value::String(s) => ids.push(dict.lookup(kind, &s)?),
                v => self.misfit(format!("{k}[{i}]"), v),
            }
        }
        Ok(Some(ids))
    }

    /// `{name: value}` maps whose names go through `lookup(kind)`; values that `conv` rejects
    /// are kept as misfits.
    fn lookup_map<V>(&mut self, k: &str, kind: &str, dict: &mut Dict, conv: impl Fn(&Value) -> Option<V>) -> Result<Vec<(i16, V)>> {
        let Some(m) = self.obj(k) else { return Ok(Vec::new()) };
        let mut out = Vec::with_capacity(m.map.len());
        for (name, v) in m.map {
            match conv(&v) {
                Some(x) => out.push((dict.lookup(kind, &name)?, x)),
                None => self.misfit(format!("{k}.{name}"), v),
            }
        }
        Ok(out)
    }

    fn finish(self, extra: &mut Map<String, Value>, issues: &mut Issues) {
        for (k, v) in self.map {
            let path = format!("{}{k}", self.prefix);
            issues.unknown(format!("{}.{path}", self.entity), &v);
            extra.insert(path, v);
        }
        for (k, v, log) in self.misfits {
            let path = format!("{}{k}", self.prefix);
            if log {
                issues.add(format!("{}.{path}", self.entity), "type_mismatch", snippet(&v));
            }
            extra.insert(path, v);
        }
    }
}

/// (key, predicate its value must satisfy)
type Check = (&'static str, fn(&Value) -> bool);

fn has_map(m: &Map<String, Value>, checks: &[Check]) -> bool {
    checks.iter().all(|(k, ok)| m.get(*k).is_some_and(ok))
}
fn has(v: &Value, checks: &[Check]) -> bool {
    v.as_object().is_some_and(|m| has_map(m, checks))
}
fn is_u64(v: &Value) -> bool {
    v.is_u64()
}
fn is_i16(v: &Value) -> bool {
    v.as_i64().is_some_and(|i| i16::try_from(i).is_ok())
}
fn is_i32(v: &Value) -> bool {
    v.as_i64().is_some_and(|i| i32::try_from(i).is_ok())
}
fn is_str(v: &Value) -> bool {
    v.is_string()
}
fn is_num(v: &Value) -> bool {
    v.is_number()
}
fn is_ts(v: &Value) -> bool {
    v.as_str().and_then(parse_ts).is_some()
}
fn as_i32(v: &Value) -> Option<i32> {
    v.as_i64().and_then(|i| i32::try_from(i).ok())
}

#[derive(PartialEq)]
struct Faction {
    name: String,
    allegiance: Option<i16>,
    government: Option<i16>,
    state: Option<i16>,
    influence: Option<f64>,
    states: Vec<(i16, i16, Option<f64>)>,
}

pub struct Projector {
    pub dict: Dict,
    pub issues: Issues,
}

impl Projector {
    /// Projects one system. Returns false when the system itself is unusable (logged as an issue).
    pub fn system(&mut self, ordinal: u64, v: Value, b: &mut Batch) -> Result<bool> {
        let sys_id = v.get("id64").and_then(Value::as_u64).map(|x| x as i64);
        let ok = has(&v, &[("id64", is_u64), ("name", is_str), ("date", is_ts)])
            && has(&v["coords"], &[("x", is_num), ("y", is_num), ("z", is_num)]);
        if !ok {
            self.issues.add("system".into(), "invalid_system", snippet(&v));
            self.issues.flush(b.t(T::Issue), ordinal, sys_id);
            return Ok(false);
        }
        let Value::Object(map) = v else { unreachable!("checked above") };
        let id = sys_id.expect("checked");
        let mut f = Fields::new(map, String::new(), "system");
        f.take("id64");
        let mut extra = Map::new();
        let r = self.system_inner(id, &mut f, &mut extra, b);
        f.finish(&mut extra, &mut self.issues);
        self.issues.flush(b.t(T::Issue), ordinal, Some(id));
        b.t(T::System).jsonb((!extra.is_empty()).then_some(&extra));
        r?;
        Ok(true)
    }

    /// Writes every column of the system row except the trailing `extra`.
    fn system_inner(&mut self, id: i64, f: &mut Fields, extra: &mut Map<String, Value>, b: &mut Batch) -> Result<()> {
        // Children first: they only add to `extra`, and the system row must be contiguous in its buffer.
        for (i, item) in f.arr("bodies").unwrap_or_default().into_iter().enumerate() {
            self.body(id, i, item, extra, b)?;
        }
        for (i, item) in f.arr("stations").unwrap_or_default().into_iter().enumerate() {
            self.station(id, None, &format!("stations[{i}]"), item, extra, b)?;
        }
        let controlling_faction = self.factions(id, f, extra, b)?;

        let d = &mut self.dict;
        for mut p in f.items("powerConflictProgress", &[("power", is_str), ("progress", is_num)]).unwrap_or_default() {
            let power = p.lookup("power", "power", d)?;
            b.t(T::PowerConflict).row(3).i64(Some(id)).i16(power).f64(p.f64("progress"));
            p.finish(extra, &mut self.issues);
        }
        if let Some(mut w) = f.obj("thargoidWar") {
            b.t(T::ThargoidWar)
                .row(8)
                .i64(Some(id))
                .i16(w.lookup("currentState", "thargoid_state", d)?)
                .i16(w.lookup("successState", "thargoid_state", d)?)
                .i16(w.lookup("failureState", "thargoid_state", d)?)
                .f64(w.f64("progress"))
                .f64(w.f64("daysRemaining"))
                .f64(w.f64("portsRemaining"))
                .bool(w.bool("successReached"));
            w.finish(extra, &mut self.issues);
        }
        let mut coords = f.obj("coords").expect("checked");
        let (x, y, z) = (coords.f64("x"), coords.f64("y"), coords.f64("z"));
        coords.finish(extra, &mut self.issues);
        let mut ts = [None; 5];
        if let Some(mut t) = f.obj("timestamps") {
            for (slot, key) in ts.iter_mut().zip(["factions", "powerState", "powers", "controllingPower", "thargoidWar"]) {
                *slot = t.ts(key);
            }
            t.finish(extra, &mut self.issues);
        }
        let name = f.string("name");
        let row = b.t(T::System).row(26);
        row.i64(Some(id))
            .text(name.as_deref())
            .f64(x)
            .f64(y)
            .f64(z)
            .i16(f.lookup("allegiance", "allegiance", d)?)
            .i16(f.lookup("government", "government", d)?)
            .i16(f.lookup("primaryEconomy", "economy", d)?)
            .i16(f.lookup("secondaryEconomy", "economy", d)?)
            .i16(f.lookup("security", "security", d)?)
            .i64(f.int("population"))
            .i16(f.int("bodyCount"))
            .ts(f.ts("date"))
            .i16(f.lookup("controllingPower", "power", d)?)
            .i16(f.lookup("powerState", "power_state", d)?)
            .i16_array(f.lookup_array("powers", "power", d)?.as_deref())
            .f64(f.f64("powerStateControlProgress"))
            .f64(f.f64("powerStateReinforcement"))
            .f64(f.f64("powerStateUndermining"))
            .ts(ts[0])
            .ts(ts[1])
            .ts(ts[2])
            .ts(ts[3])
            .ts(ts[4])
            .text(controlling_faction.as_deref());
        Ok(())
    }

    /// factions[] rows; `controllingFaction` flags its entry, or is added when absent from the list.
    /// Returns the controlling faction's name.
    fn factions(&mut self, id: i64, f: &mut Fields, extra: &mut Map<String, Value>, b: &mut Batch) -> Result<Option<String>> {
        let mut factions: Vec<Faction> = Vec::new();
        for (i, item) in f.arr("factions").unwrap_or_default().into_iter().enumerate() {
            let name = item.get("name").and_then(Value::as_str).map(str::to_owned);
            match item {
                Value::Object(m) if name.as_ref().is_some_and(|n| !factions.iter().any(|x| &x.name == n)) => {
                    let mut ff = Fields::new(m, format!("factions[{i}]."), "system");
                    factions.push(self.faction(&mut ff, extra)?);
                    ff.finish(extra, &mut self.issues);
                }
                other => {
                    let kind = if name.is_some() { "duplicate_faction" } else { "invalid_faction" };
                    self.issues.add(format!("system.factions[{i}]"), kind, snippet(&other));
                    extra.insert(format!("factions[{i}]"), other);
                }
            }
        }
        let listed = factions.len();
        let mut controlling = None;
        let mut controlling_name = None;
        match f.take("controllingFaction") {
            None => {}
            Some(raw) if has(&raw, &[("name", is_str)]) => {
                let Value::Object(m) = raw.clone() else { unreachable!("checked") };
                let mut cf = Fields::new(m, "controllingFaction.".into(), "system");
                let c = self.faction(&mut cf, extra)?;
                controlling_name = Some(c.name.clone());
                match factions.iter().position(|x| x.name == c.name) {
                    Some(i) => {
                        controlling = Some(i);
                        let x = &factions[i];
                        // controllingFaction carries no influence; everything else must agree.
                        let same = x.allegiance == c.allegiance && x.government == c.government && x.state == c.state && x.states == c.states && c.influence.is_none();
                        if same {
                            cf.finish(extra, &mut self.issues);
                        } else {
                            extra.insert("controllingFaction".into(), raw);
                        }
                    }
                    None => {
                        factions.push(c);
                        controlling = Some(listed);
                        cf.finish(extra, &mut self.issues);
                    }
                }
            }
            Some(raw) => f.misfit("controllingFaction", raw),
        }
        for (i, fac) in factions.iter().enumerate() {
            b.t(T::SystemFaction)
                .row(8)
                .i64(Some(id))
                .text(Some(&fac.name))
                .i16(fac.allegiance)
                .i16(fac.government)
                .i16(fac.state)
                .f64(fac.influence)
                .bool(Some(controlling == Some(i)))
                .bool(Some(i < listed));
            for (bucket, state, trend) in &fac.states {
                b.t(T::FactionState).row(5).i64(Some(id)).text(Some(&fac.name)).i16(Some(*bucket)).i16(Some(*state)).f64(*trend);
            }
        }
        Ok(controlling_name)
    }

    /// `f` must have a string `name` (checked by the caller).
    fn faction(&mut self, f: &mut Fields, extra: &mut Map<String, Value>) -> Result<Faction> {
        let d = &mut self.dict;
        let mut fac = Faction {
            name: f.string("name").expect("checked"),
            allegiance: f.lookup("allegiance", "allegiance", d)?,
            government: f.lookup("government", "government", d)?,
            state: f.lookup("state", "faction_state", d)?,
            influence: f.f64("influence"),
            states: Vec::new(),
        };
        for (key, bucket) in [("activeStates", "active"), ("pendingStates", "pending"), ("recoveringStates", "recovering")] {
            let bucket_id = d.lookup("faction_state_bucket", bucket)?;
            for mut st in f.items(key, &[("state", is_str)]).unwrap_or_default() {
                let state = st.lookup("state", "faction_state", d)?.expect("checked");
                fac.states.push((bucket_id, state, st.f64("trend")));
                st.finish(extra, &mut self.issues);
            }
        }
        Ok(fac)
    }

    fn body(&mut self, sys: i64, i: usize, v: Value, sys_extra: &mut Map<String, Value>, b: &mut Batch) -> Result<()> {
        if !has(&v, &[("id64", is_u64), ("bodyId", is_i16), ("name", is_str), ("updateTime", is_ts)]) {
            self.issues.add(format!("system.bodies[{i}]"), "invalid_body", snippet(&v));
            sys_extra.insert(format!("bodies[{i}]"), v);
            return Ok(());
        }
        let Value::Object(map) = v else { unreachable!("checked above") };
        let mut f = Fields::new(map, String::new(), "body");
        let mut extra = Map::new();
        let id = f.id64("id64").expect("checked");

        for (si, item) in f.arr("stations").unwrap_or_default().into_iter().enumerate() {
            self.station(sys, Some(id), &format!("stations[{si}]"), item, &mut extra, b)?;
        }
        for (key, is_belt) in [("rings", false), ("belts", true)] {
            for (ordinal, item) in f.arr(key).unwrap_or_default().into_iter().enumerate() {
                self.ring(id, is_belt, key, ordinal, item, &mut extra, b)?;
            }
        }
        let d = &mut self.dict;
        let mut parents: Option<(Vec<i16>, Vec<i16>)> = None;
        if let Some(items) = f.arr("parents") {
            let (kinds, ids) = parents.insert((Vec::with_capacity(items.len()), Vec::with_capacity(items.len())));
            for (ordinal, p) in items.into_iter().enumerate() {
                match p {
                    Value::Object(m) if m.len() == 1 && m.values().all(is_i16) => {
                        let (kind, pid) = m.into_iter().next().expect("one entry");
                        kinds.push(d.lookup("parent_kind", &kind)?);
                        ids.push(pid.as_i64().expect("checked") as i16);
                    }
                    other => f.misfit(format!("parents[{ordinal}]"), other),
                }
            }
        }
        let mut composition = [None; COMPOSITION_COLUMNS];
        let mut slots = composition.iter_mut();
        for (key, components) in COMPOSITIONS {
            let mut c = f.obj(key);
            for component in components {
                *slots.next().expect("COMPOSITION_COLUMNS") = c.as_mut().and_then(|c| c.f64(component));
            }
            // Components without a column land in extra as unknown fields.
            if let Some(c) = c {
                c.finish(&mut extra, &mut self.issues);
            }
        }
        let (mut signals_at, mut genuses) = (None, None);
        if let Some(mut sg) = f.obj("signals") {
            signals_at = sg.ts("updateTime");
            genuses = sg.lookup_array("genuses", "genus", d)?;
            for (signal, count) in sg.lookup_map("signals", "signal", d, as_i32)? {
                b.t(T::BodySignal).row(3).i64(Some(id)).i16(Some(signal)).i32(Some(count));
            }
            sg.finish(&mut extra, &mut self.issues);
        }
        let (mut ts_mean_anomaly, mut ts_distance, mut ts_ascending_node) = (None, None, None);
        if let Some(mut ts) = f.obj("timestamps") {
            ts_mean_anomaly = ts.ts("meanAnomaly");
            ts_distance = ts.ts("distanceToArrival");
            ts_ascending_node = ts.ts("ascendingNode");
            ts.finish(&mut extra, &mut self.issues);
        }
        let body_id: Option<i16> = f.int("bodyId");
        let name = f.string("name");
        let row = b.t(T::Body).row(43 + COMPOSITION_COLUMNS as i16);
        row.i64(Some(id))
            .i64(Some(sys))
            .i16(body_id)
            .text(name.as_deref())
            .i16(f.lookup("type", "body_type", d)?)
            .i16(f.lookup("subType", "body_sub_type", d)?)
            .ts(f.ts("updateTime"));
        for k in [
            "distanceToArrival",
            "orbitalPeriod",
            "semiMajorAxis",
            "orbitalEccentricity",
            "orbitalInclination",
            "argOfPeriapsis",
            "meanAnomaly",
            "ascendingNode",
            "rotationalPeriod",
        ] {
            row.f64(f.f64(k));
        }
        row.bool(f.bool("rotationalPeriodTidallyLocked"))
            .f64(f.f64("axialTilt"))
            .f64(f.f64("surfaceTemperature"))
            .bool(f.bool("mainStar"))
            .i32(f.int("age"))
            .i16(f.lookup("spectralClass", "spectral_class", d)?)
            .i16(f.lookup("luminosity", "luminosity", d)?)
            .f64(f.f64("absoluteMagnitude"))
            .f64(f.f64("solarMasses"))
            .f64(f.f64("solarRadius"))
            .bool(f.bool("isLandable"))
            .f64(f.f64("gravity"))
            .f64(f.f64("earthMasses"))
            .f64(f.f64("radius"))
            .f64(f.f64("surfacePressure"))
            .i16(f.lookup("volcanismType", "volcanism", d)?)
            .i16(f.lookup("atmosphereType", "atmosphere", d)?)
            .i16(f.lookup("terraformingState", "terraforming_state", d)?)
            .i16(f.lookup("reserveLevel", "reserve_level", d)?)
            .ts(ts_mean_anomaly)
            .ts(ts_distance)
            .ts(ts_ascending_node)
            .ts(signals_at)
            .i16_array(genuses.as_deref())
            .i16_array(parents.as_ref().map(|p| &p.0[..]))
            .i16_array(parents.as_ref().map(|p| &p.1[..]));
        for pct in composition {
            row.f64(pct);
        }
        f.finish(&mut extra, &mut self.issues);
        b.t(T::Body).jsonb((!extra.is_empty()).then_some(&extra));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn ring(&mut self, body: i64, is_belt: bool, key: &str, ordinal: usize, v: Value, extra: &mut Map<String, Value>, b: &mut Batch) -> Result<()> {
        if !has(&v, &[("name", is_str), ("type", is_str), ("mass", is_num), ("innerRadius", is_num), ("outerRadius", is_num)]) {
            self.issues.add(format!("body.{key}[{ordinal}]"), "invalid_ring", snippet(&v));
            extra.insert(format!("{key}[{ordinal}]"), v);
            return Ok(());
        }
        let Value::Object(map) = v else { unreachable!("checked above") };
        let mut f = Fields::new(map, format!("{key}[{ordinal}]."), "body");
        let d = &mut self.dict;
        let ord = ordinal as i16;
        let mut signals_at = None;
        if let Some(mut sg) = f.obj("signals") {
            signals_at = sg.ts("updateTime");
            for (signal, count) in sg.lookup_map("signals", "signal", d, as_i32)? {
                b.t(T::RingSignal).row(5).i64(Some(body)).bool(Some(is_belt)).i16(Some(ord)).i16(Some(signal)).i32(Some(count));
            }
            sg.finish(extra, &mut self.issues);
        }
        let name = f.string("name");
        b.t(T::Ring)
            .row(10)
            .i64(Some(body))
            .bool(Some(is_belt))
            .i16(Some(ord))
            .text(name.as_deref())
            .i16(f.lookup("type", "ring_type", d)?)
            .f64(f.f64("mass"))
            .f64(f.f64("innerRadius"))
            .f64(f.f64("outerRadius"))
            .i64(f.id64("id64"))
            .ts(signals_at);
        f.finish(extra, &mut self.issues);
        Ok(())
    }

    fn station(&mut self, sys: i64, body: Option<i64>, path: &str, v: Value, parent_extra: &mut Map<String, Value>, b: &mut Batch) -> Result<()> {
        if !has(&v, &[("id", is_u64), ("name", is_str), ("updateTime", is_ts)]) {
            self.issues.add(format!("station:{path}"), "invalid_station", snippet(&v));
            parent_extra.insert(path.to_owned(), v);
            return Ok(());
        }
        let Value::Object(map) = v else { unreachable!("checked above") };
        let mut f = Fields::new(map, String::new(), "station");
        let mut extra = Map::new();
        let id = f.id64("id").expect("checked");
        let d = &mut self.dict;

        for (economy, share) in f.lookup_map("economies", "economy", d, Value::as_f64)? {
            b.t(T::StationEconomy).row(3).i64(Some(id)).i16(Some(economy)).f64(Some(share));
        }
        let mut pads = (None, None, None);
        if let Some(mut lp) = f.obj("landingPads") {
            pads = (lp.int("large"), lp.int("medium"), lp.int("small"));
            lp.finish(&mut extra, &mut self.issues);
        }

        let (mut market_at, mut prohibited) = (None, None);
        if let Some(mut m) = f.obj("market") {
            market_at = m.ts("updateTime");
            if let Some(items) = m.arr("prohibitedCommodities") {
                let mut names = Vec::with_capacity(items.len());
                for (i, x) in items.into_iter().enumerate() {
                    match x {
                        Value::String(s) => names.push(s),
                        other => m.misfit(format!("prohibitedCommodities[{i}]"), other),
                    }
                }
                prohibited = Some(names);
            }
            let checks: &[Check] = &[
                ("commodityId", is_i32),
                ("symbol", is_str),
                ("name", is_str),
                ("category", is_str),
                ("buyPrice", is_i32),
                ("sellPrice", is_i32),
                ("demand", is_i32),
                ("supply", is_i32),
            ];
            for mut c in m.items("commodities", checks).unwrap_or_default() {
                let cid: i32 = c.int("commodityId").expect("checked");
                let (sym, name, cat_name) = (c.string("symbol").expect("checked"), c.string("name").expect("checked"), c.string("category").expect("checked"));
                let attrs = (sym, name, d.lookup("commodity_category", &cat_name)?);
                if !d.commodity(cid, attrs.clone())? {
                    self.issues.add(format!("station.{}", c.prefix), "catalogue_conflict", format!("commodity {cid}: {} / {}", attrs.0, attrs.1));
                    c.keep("symbol", Value::String(attrs.0));
                    c.keep("name", Value::String(attrs.1));
                    c.keep("category", Value::String(cat_name));
                }
                b.t(T::MarketCommodity)
                    .row(6)
                    .i64(Some(id))
                    .i32(Some(cid))
                    .i32(c.int("buyPrice"))
                    .i32(c.int("sellPrice"))
                    .i32(c.int("demand"))
                    .i32(c.int("supply"));
                c.finish(&mut extra, &mut self.issues);
            }
            m.finish(&mut extra, &mut self.issues);
        }

        let (mut shipyard_at, mut ship_ids) = (None, None);
        if let Some(mut sy) = f.obj("shipyard") {
            shipyard_at = sy.ts("updateTime");
            if let Some(items) = sy.items("ships", &[("shipId", is_i32), ("symbol", is_str), ("name", is_str)]) {
                let mut ids = Vec::with_capacity(items.len());
                for mut s in items {
                    let sid: i32 = s.int("shipId").expect("checked");
                    let attrs = (s.string("symbol").expect("checked"), s.string("name").expect("checked"));
                    if !d.ship(sid, attrs.clone())? {
                        self.issues.add(format!("station.{}", s.prefix), "catalogue_conflict", format!("ship {sid}: {} / {}", attrs.0, attrs.1));
                        s.keep("symbol", Value::String(attrs.0));
                        s.keep("name", Value::String(attrs.1));
                    }
                    ids.push(sid);
                    s.finish(&mut extra, &mut self.issues);
                }
                ship_ids = Some(ids);
            }
            sy.finish(&mut extra, &mut self.issues);
        }

        let (mut outfitting_at, mut variant_ids) = (None, None);
        if let Some(mut of) = f.obj("outfitting") {
            outfitting_at = of.ts("updateTime");
            let checks: &[Check] =
                &[("moduleId", is_i32), ("class", is_i16), ("category", is_str), ("rating", is_str), ("symbol", is_str), ("name", is_str)];
            if let Some(items) = of.items("modules", checks) {
                let mut ids = Vec::with_capacity(items.len());
                for mut m in items {
                    let cat_name = m.string("category").expect("checked");
                    let v: ModuleVariant = (
                        m.int("moduleId").expect("checked"),
                        m.int("class").expect("checked"),
                        d.lookup("module_category", &cat_name)?,
                        m.string("rating").expect("checked"),
                        m.string("symbol").expect("checked"),
                        m.string("name").expect("checked"),
                        m.string("ship"),
                    );
                    ids.push(d.module_variant(v)?);
                    m.finish(&mut extra, &mut self.issues);
                }
                variant_ids = Some(ids);
            }
            of.finish(&mut extra, &mut self.issues);
        }

        let name = f.string("name");
        let row = b.t(T::Station).row(30);
        row.i64(Some(id))
            .i64(Some(sys))
            .i64(body)
            .text(name.as_deref())
            .ts(f.ts("updateTime"))
            .text(f.string("realName").as_deref())
            .text(f.string("carrierName").as_deref())
            .text(f.string("controllingFaction").as_deref())
            .i16(f.lookup("controllingFactionState", "faction_state", d)?)
            .f64(f.f64("distanceToArrival"))
            .i16(f.lookup("primaryEconomy", "economy", d)?)
            .i16(f.lookup("secondaryEconomy", "economy", d)?)
            .i16(f.lookup("allegiance", "allegiance", d)?)
            .i16(f.lookup("government", "government", d)?)
            .i16(f.lookup("type", "station_type", d)?)
            .i16(f.lookup("state", "station_state", d)?)
            .i16(f.lookup("carrierDockingAccess", "carrier_docking_access", d)?)
            .f64(f.f64("latitude"))
            .f64(f.f64("longitude"))
            .i16(pads.0)
            .i16(pads.1)
            .i16(pads.2)
            .i16_array(f.lookup_array("services", "service", d)?.as_deref())
            .ts(market_at)
            .text_array(prohibited.as_deref())
            .ts(shipyard_at)
            .i32_array(ship_ids.as_deref())
            .ts(outfitting_at)
            .i32_array(variant_ids.as_deref());
        f.finish(&mut extra, &mut self.issues);
        b.t(T::Station).jsonb((!extra.is_empty()).then_some(&extra));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_ts, COMPOSITIONS, COMPOSITION_COLUMNS, TABLES};

    #[test]
    fn composition_columns_match_the_body_copy_list() {
        let expected: Vec<String> = COMPOSITIONS
            .iter()
            .flat_map(|(key, comps)| {
                let prefix = match *key {
                    "solidComposition" => "solid_",
                    "atmosphereComposition" => "atm_",
                    _ => "mat_",
                };
                comps.iter().map(move |c| format!("{prefix}{}", c.to_lowercase().replace(' ', "_")))
            })
            .collect();
        assert_eq!(expected.len(), COMPOSITION_COLUMNS);
        let body: Vec<&str> = TABLES.iter().find(|t| t.0 == "body").unwrap().1.split(',').collect();
        let at = body.iter().position(|c| *c == "solid_ice").unwrap();
        assert_eq!(body[at..at + COMPOSITION_COLUMNS], expected[..]);
        assert_eq!(body.len(), 43 + COMPOSITION_COLUMNS);
    }

    #[test]
    fn parses_all_observed_timestamp_shapes() {
        let base = 1_782_774_216_000_000; // 2026-06-29T23:03:36Z
        assert_eq!(parse_ts("2026-06-29T23:03:36Z"), Some(base));
        assert_eq!(parse_ts("2026-06-29 23:03:36+00"), Some(base));
        assert_eq!(parse_ts("2026-06-29T23:03:36+00:00"), Some(base));
        assert_eq!(parse_ts("2026-06-29T23:03:36.5Z"), Some(base + 500_000));
        assert_eq!(parse_ts("2026-06-29T23:03:36.462365Z"), Some(base + 462_365));
        assert_eq!(parse_ts("2026-06-30T01:03:36+02:00"), Some(base));
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("2000-02-29T00:00:00Z"), Some(951_782_400_000_000));
        for bad in ["", "2026-06-29", "2026-06-29T23:03:36", "2026-13-01T00:00:00Z", "2026-06-29T23:03:36.1234567Z", "2026-06-29T23:03:36+0"] {
            assert_eq!(parse_ts(bad), None, "{bad}");
        }
    }
}
