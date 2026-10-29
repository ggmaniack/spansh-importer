//! Structural survey of a dump: for every JSON path, which types occur, how often,
//! value ranges and (for low-cardinality fields) the distinct values. Feeds schema design.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::BufRead;
use std::time::Instant;

use anyhow::Result;
use serde_json::Value;

/// Distinct values tracked per path before it is declared high-cardinality.
const MAX_DISTINCT: usize = 2000;
const MAX_ERRORS: usize = 20;

/// Objects whose keys are data (name -> value maps), not fields.
const MAP_FIELDS: &[&str] = &[
    "timestamps",
    "atmosphereComposition",
    "solidComposition",
    "materials",
    "economies",
];

#[derive(Default)]
struct PathStats {
    present: u64,
    nulls: u64,
    bools: u64,
    uints: u64,
    negints: u64,
    floats: u64,
    strings: u64,
    arrays: u64,
    objects: u64,
    num_min: f64,
    num_max: f64,
    uint_max: u64,
    arr_min: usize,
    arr_max: usize,
    str_max: usize,
    values: HashMap<String, u64>,
    high_card: bool,
}

impl PathStats {
    fn note_value(&mut self, v: &str) {
        if self.high_card {
            return;
        }
        if let Some(c) = self.values.get_mut(v) {
            *c += 1;
        } else if self.values.len() < MAX_DISTINCT {
            self.values.insert(v.to_owned(), 1);
        } else {
            self.high_card = true;
            self.values.clear();
        }
    }

    fn note_num(&mut self, x: f64) {
        let first = self.uints + self.negints + self.floats == 1;
        if first || x < self.num_min {
            self.num_min = x;
        }
        if first || x > self.num_max {
            self.num_max = x;
        }
    }

    fn merge(&mut self, o: PathStats) {
        let had_nums = self.uints + self.negints + self.floats > 0;
        let o_nums = o.uints + o.negints + o.floats > 0;
        if o_nums {
            self.num_min = if had_nums { self.num_min.min(o.num_min) } else { o.num_min };
            self.num_max = if had_nums { self.num_max.max(o.num_max) } else { o.num_max };
        }
        if o.arrays > 0 {
            self.arr_min = if self.arrays > 0 { self.arr_min.min(o.arr_min) } else { o.arr_min };
        }
        self.present += o.present;
        self.nulls += o.nulls;
        self.bools += o.bools;
        self.uints += o.uints;
        self.negints += o.negints;
        self.floats += o.floats;
        self.strings += o.strings;
        self.arrays += o.arrays;
        self.objects += o.objects;
        self.uint_max = self.uint_max.max(o.uint_max);
        self.arr_max = self.arr_max.max(o.arr_max);
        self.str_max = self.str_max.max(o.str_max);
        if o.high_card {
            self.high_card = true;
            self.values.clear();
        }
        for (k, c) in o.values {
            if self.high_card {
                break;
            }
            if let Some(mine) = self.values.get_mut(&k) {
                *mine += c;
            } else if self.values.len() < MAX_DISTINCT {
                self.values.insert(k, c);
            } else {
                self.high_card = true;
                self.values.clear();
            }
        }
    }
}

#[derive(Default)]
pub struct Survey {
    paths: HashMap<String, PathStats>,
    systems: u64,
    bytes: u64,
    biggest: Vec<(usize, String)>,
    errors: Vec<String>,
    error_count: u64,
    // Plain list, sorted once at the end: 8 bytes per system instead of a hash map entry.
    system_ids: Vec<u64>,
    station_ids: HashMap<u64, u32>,
    // Body ids are not stored: id64 = system | bodyId << 55 (checked below), so unique systems
    // plus unique bodyIds within each system imply unique body ids. Keeps memory bounded.
    bodies: u64,
    body_key_dupes: u64,
    body_id64_mismatches: u64,
}

impl Survey {
    pub fn add_line(&mut self, line_no: u64, bytes: &[u8]) {
        self.bytes += bytes.len() as u64;
        let v: Value = match serde_json::from_slice(bytes) {
            Ok(v) => v,
            Err(e) => {
                self.error_count += 1;
                if self.errors.len() < MAX_ERRORS {
                    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]);
                    self.errors.push(format!("line {line_no}: {e}: {head}"));
                }
                return;
            }
        };
        self.systems += 1;
        let name = v["name"].as_str().unwrap_or("?");
        self.biggest.push((bytes.len(), name.to_owned()));
        if self.biggest.len() > 40 {
            self.biggest.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            self.biggest.truncate(10);
        }
        self.check_ids(&v);
        let mut path = String::from("$");
        self.walk(&mut path, &v, false);
    }

    fn check_ids(&mut self, sys: &Value) {
        if let Some(id) = sys["id64"].as_u64() {
            self.system_ids.push(id);
        }
        let mut body_ids = std::collections::HashSet::new();
        let mut stations: Vec<&Value> = sys["stations"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
        let sys_id = sys["id64"].as_u64();
        for b in sys["bodies"].as_array().into_iter().flatten() {
            if let Some(id) = b["id64"].as_u64() {
                self.bodies += 1;
                // Hypothesis: body id64 = system id64 | bodyId << 55.
                if sys_id.zip(b["bodyId"].as_u64()).is_some_and(|(s, bid)| id != s | bid << 55) {
                    self.body_id64_mismatches += 1;
                }
            }
            if let Some(bid) = b["bodyId"].as_u64() {
                if !body_ids.insert(bid) {
                    self.body_key_dupes += 1;
                }
            }
            stations.extend(b["stations"].as_array().into_iter().flatten());
        }
        for s in stations {
            if let Some(id) = s["id"].as_u64() {
                *self.station_ids.entry(id).or_default() += 1;
            }
        }
    }

    fn walk(&mut self, path: &mut String, v: &Value, is_map: bool) {
        let st = match self.paths.get_mut(path.as_str()) {
            Some(s) => s,
            None => self.paths.entry(path.clone()).or_default(),
        };
        st.present += 1;
        match v {
            Value::Null => st.nulls += 1,
            Value::Bool(b) => {
                st.bools += 1;
                st.note_value(if *b { "true" } else { "false" });
            }
            Value::Number(n) => {
                if let Some(u) = n.as_u64() {
                    st.uints += 1;
                    st.uint_max = st.uint_max.max(u);
                    st.note_value(&u.to_string());
                } else if let Some(i) = n.as_i64() {
                    st.negints += 1;
                    st.note_value(&i.to_string());
                } else {
                    st.floats += 1;
                    st.high_card = true;
                    st.values.clear();
                }
                st.note_num(n.as_f64().unwrap_or(f64::NAN));
            }
            Value::String(s) => {
                st.strings += 1;
                st.str_max = st.str_max.max(s.len());
                st.note_value(s);
            }
            Value::Array(a) => {
                st.arr_min = if st.arrays == 0 { a.len() } else { st.arr_min.min(a.len()) };
                st.arr_max = st.arr_max.max(a.len());
                st.arrays += 1;
                // `parents` items are single-key {kind: bodyId} maps.
                let items_are_maps = path.ends_with(".parents");
                let len = path.len();
                path.push_str("[]");
                for item in a {
                    self.walk(path, item, items_are_maps);
                }
                path.truncate(len);
            }
            Value::Object(o) => {
                st.objects += 1;
                if is_map {
                    for k in o.keys() {
                        st.note_value(k);
                    }
                }
                let len = path.len();
                for (k, child) in o {
                    if is_map {
                        path.push_str(".{}");
                        self.walk(path, child, false);
                    } else {
                        path.push('.');
                        path.push_str(k);
                        let child_is_map =
                            MAP_FIELDS.contains(&k.as_str()) || (k == "signals" && path.ends_with(".signals.signals"));
                        self.walk(path, child, child_is_map);
                    }
                    path.truncate(len);
                }
            }
        }
    }

    pub fn merge(&mut self, o: Survey) {
        for (k, s) in o.paths {
            match self.paths.get_mut(&k) {
                Some(mine) => mine.merge(s),
                None => {
                    self.paths.insert(k, s);
                }
            }
        }
        self.systems += o.systems;
        self.bytes += o.bytes;
        self.biggest.extend(o.biggest);
        self.error_count += o.error_count;
        self.errors.extend(o.errors);
        self.errors.truncate(MAX_ERRORS);
        self.system_ids.extend(o.system_ids);
        for (id, c) in o.station_ids {
            *self.station_ids.entry(id).or_default() += c;
        }
        self.bodies += o.bodies;
        self.body_key_dupes += o.body_key_dupes;
        self.body_id64_mismatches += o.body_id64_mismatches;
    }

    pub fn report(mut self, source: &str, elapsed_s: f64) -> String {
        let mut r = String::new();
        let mb = self.bytes as f64 / 1e6;
        let _ = writeln!(r, "# Survey of {source}\n");
        let _ = writeln!(
            r,
            "systems: {}  decompressed: {:.0} MB  time: {:.1}s  ({:.0} MB/s, {:.0} systems/s)",
            self.systems,
            mb,
            elapsed_s,
            mb / elapsed_s,
            self.systems as f64 / elapsed_s
        );
        let _ = writeln!(r, "parse errors: {}", self.error_count);
        for e in &self.errors {
            let _ = writeln!(r, "  {e}");
        }
        self.system_ids.sort_unstable();
        let system_dupes = self.system_ids.windows(2).filter(|w| w[0] == w[1]).count();
        let station_dupes = self.station_ids.values().filter(|&&c| c > 1).count();
        let _ = writeln!(
            r,
            "duplicate ids: system id64 {} of {}, station id {} of {}, (system, bodyId) {} of {} bodies",
            system_dupes,
            self.system_ids.len(),
            station_dupes,
            self.station_ids.len(),
            self.body_key_dupes,
            self.bodies
        );
        let _ = writeln!(r, "body id64 != system id64 | bodyId << 55: {}", self.body_id64_mismatches);
        let mut dup_stations: Vec<_> = self.station_ids.iter().filter(|(_, c)| **c > 1).collect();
        dup_stations.sort_unstable_by(|a, b| b.1.cmp(a.1));
        for (id, c) in dup_stations.iter().take(10) {
            let _ = writeln!(r, "  station id {id} appears {c}x");
        }
        self.biggest.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        let _ = writeln!(r, "largest systems:");
        for (n, name) in self.biggest.iter().take(10) {
            let _ = writeln!(r, "  {n:>10} bytes  {name}");
        }

        let mut keys: Vec<&String> = self.paths.keys().collect();
        keys.sort();
        let _ = writeln!(r, "\n## Paths\n");
        for k in keys {
            let s = &self.paths[k];
            // Presence relative to the containing object (fields only, not array items or map values).
            let parent_objects = k
                .rfind('.')
                .filter(|_| !k.ends_with("[]") && !k.ends_with("{}"))
                .and_then(|i| self.paths.get(&k[..i]))
                .map(|p| p.objects);
            let _ = write!(r, "{k}  n={}", s.present);
            if let Some(p) = parent_objects.filter(|&p| p > 0) {
                let _ = write!(r, " ({:.2}% of parents)", 100.0 * s.present as f64 / p as f64);
            }
            for (label, c) in [
                ("null", s.nulls),
                ("bool", s.bools),
                ("uint", s.uints),
                ("negint", s.negints),
                ("float", s.floats),
                ("str", s.strings),
                ("arr", s.arrays),
                ("obj", s.objects),
            ] {
                if c > 0 {
                    let _ = write!(r, " {label}={c}");
                }
            }
            if s.uints + s.negints + s.floats > 0 {
                let _ = write!(r, " range=[{}, {}]", s.num_min, s.num_max);
                if s.uints > 0 {
                    let _ = write!(r, " uint_max={}", s.uint_max);
                }
            }
            if s.arrays > 0 {
                let _ = write!(r, " len=[{}, {}]", s.arr_min, s.arr_max);
            }
            if s.strings > 0 {
                let _ = write!(r, " str_max={}", s.str_max);
            }
            if s.high_card {
                let _ = write!(r, " distinct>{MAX_DISTINCT}");
            } else if !s.values.is_empty() {
                let _ = write!(r, " distinct={}", s.values.len());
            }
            let _ = writeln!(r);
            if !s.high_card && !s.values.is_empty() && s.uints + s.negints == 0 {
                let mut vals: Vec<_> = s.values.iter().collect();
                vals.sort_unstable_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                for (v, c) in vals {
                    let _ = writeln!(r, "    {c:>12}  {v:?}");
                }
            }
        }
        r
    }
}

/// Reads the dump line by line and fans lines out to `workers` threads, each with its own `Survey`.
pub fn run(input: Box<dyn BufRead + Send>, workers: usize, source: &str) -> Result<String> {
    let start = Instant::now();
    let (tx, rx) = crossbeam_channel::bounded::<(u64, Vec<u8>)>(workers * 64);
    let handles: Vec<_> = (0..workers)
        .map(|_| {
            let rx = rx.clone();
            std::thread::spawn(move || {
                let mut s = Survey::default();
                for (n, line) in rx {
                    if let Some(b) = crate::source::system_bytes(&line) {
                        s.add_line(n, b);
                    }
                }
                s
            })
        })
        .collect();
    drop(rx);

    let mut input = input;
    let mut line_no = 0u64;
    let mut total = 0u64;
    let mut last_report = Instant::now();
    loop {
        let mut buf = Vec::new();
        if input.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        line_no += 1;
        total += buf.len() as u64;
        if last_report.elapsed().as_secs() >= 10 {
            last_report = Instant::now();
            let s = start.elapsed().as_secs_f64();
            eprintln!("{line_no} lines, {:.0} MB, {:.0} MB/s", total as f64 / 1e6, total as f64 / 1e6 / s);
        }
        tx.send((line_no, buf))?;
    }
    drop(tx);

    let mut all = Survey::default();
    for h in handles {
        all.merge(h.join().expect("survey worker panicked"));
    }
    Ok(all.report(source, start.elapsed().as_secs_f64()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surveys_types_maps_and_duplicates() {
        let mut s = Survey::default();
        s.add_line(1, br#"{"id64":18446744073709551615,"name":"A","bodies":[{"id64":5,"bodyId":1,"parents":[{"Star":0}],"materials":{"Iron":1.5}},{"id64":6,"bodyId":1}],"stations":[{"id":9}]}"#);
        s.add_line(2, br#"{"id64":2,"name":"B","bodies":[],"stations":[{"id":9}],"population":null}"#);
        s.add_line(3, b"{broken");
        assert_eq!(s.systems, 2);
        assert_eq!(s.error_count, 1);
        assert_eq!(s.body_key_dupes, 1);
        assert_eq!(s.body_id64_mismatches, 2);
        assert_eq!(s.station_ids[&9], 2);
        assert_eq!(s.paths["$.id64"].uint_max, u64::MAX);
        assert_eq!(s.paths["$.bodies[].parents[]"].values["Star"], 1);
        assert_eq!(s.paths["$.bodies[].parents[].{}"].uints, 1);
        assert_eq!(s.paths["$.bodies[].materials"].values["Iron"], 1);
        assert_eq!(s.paths["$.bodies[].materials.{}"].floats, 1);
        assert_eq!(s.paths["$.population"].nulls, 1);
        assert_eq!(s.paths["$.bodies"].arr_min, 0);

        let mut other = Survey::default();
        other.add_line(4, br#"{"id64":2,"name":"C","bodies":[{"id64":7,"bodyId":3}]}"#);
        s.merge(other);
        assert_eq!(s.system_ids.iter().filter(|&&i| i == 2).count(), 2);
        assert_eq!(s.paths["$.name"].values.len(), 3);
        assert_eq!(s.paths["$.bodies"].arr_max, 2);
    }
}
