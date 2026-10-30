//! Single-worker import: read line -> parse -> project into a byte-sized batch -> binary COPY.
//! Each batch commits together with its `import_batch` row, which is the resume checkpoint.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use postgres::{Client, NoTls};

use crate::pgcopy::{HEADER, TRAILER};
use crate::project::{Batch, Dict, Issues, Projector, TABLES};
use crate::report::{self, Level, Unit};
use crate::source::{self, Fingerprint};

pub struct Options {
    pub source: String,
    pub database: String,
    pub batch_mib: usize,
    pub max_systems: Option<u64>,
    pub issue_cap: u64,
}

#[derive(Default)]
pub struct Summary {
    pub run_id: i32,
    pub systems: u64,
    pub skipped: u64,
    pub invalid: u64,
    pub json_bytes: u64,
    pub batches: u64,
    pub rows: Vec<u64>,
    pub copy_bytes: u64,
    pub issues: u64,
    pub complete: bool,
    pub t_read: Duration,
    pub t_parse: Duration,
    pub t_project: Duration,
    pub t_copy: Duration,
    pub elapsed: Duration,
}

pub fn connect(url: &str) -> Result<Client> {
    Client::connect(url, NoTls).with_context(|| format!("connecting to {url}"))
}

/// Drops and recreates the `spansh` schema (load phase: no keys on the big tables).
pub fn init(database: &str) -> Result<()> {
    let mut c = connect(database)?;
    c.batch_execute("DROP SCHEMA IF EXISTS spansh CASCADE")?;
    c.batch_execute(include_str!("../db/migrations/001_schema.sql"))?;
    Ok(())
}

/// Adds primary keys, foreign keys and indexes after the load. Each statement commits on its own
/// and finished ones are skipped, so an interrupted run resumes; a second connection reports index
/// build progress.
pub fn finalize(database: &str) -> Result<()> {
    let steps = statements(include_str!("../db/migrations/002_constraints.sql"));
    let done = Arc::new(AtomicU64::new(0));
    report::track(done.clone(), Some(steps.len() as u64), Unit::Steps);
    let mut watcher = connect(database)?;
    let finished = Arc::new(AtomicBool::new(false));
    let stop = finished.clone();
    let poller = std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let row = watcher.query_opt(
                "SELECT relid::regclass::text, phase, round(100.0 * blocks_done / nullif(blocks_total, 0), 1)::float8
                 FROM pg_stat_progress_create_index LIMIT 1",
                &[],
            );
            report::note(match row {
                Ok(Some(r)) => format!("{}: {} {}%", r.get::<_, String>(0), r.get::<_, String>(1), r.get::<_, Option<f64>>(2).unwrap_or(0.0)),
                _ => String::new(),
            });
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    let result = (|| -> Result<()> {
        let mut c = connect(database)?;
        let mut skipped = 0;
        for (i, sql) in steps.iter().enumerate() {
            let label = sql.split_whitespace().collect::<Vec<_>>().join(" ");
            report::detail(format!("step {}/{}: {label}", i + 1, steps.len()));
            if step_done(&mut c, sql)? {
                skipped += 1;
            } else {
                let t = Instant::now();
                c.batch_execute(sql).with_context(|| format!("finalize step {}: {label}", i + 1))?;
                report::log(Level::Info, "step", &format!("{label} ({})", report::hms(t.elapsed())));
            }
            done.fetch_add(1, Ordering::Relaxed);
        }
        if skipped > 0 {
            report::log(Level::Info, "resumed", &format!("{skipped} steps were already done and skipped"));
        }
        Ok(())
    })();
    finished.store(true, Ordering::Relaxed);
    let _ = poller.join();
    result
}

/// True when a step's named constraint already exists (`ADD CONSTRAINT`) or is already validated
/// (`VALIDATE CONSTRAINT`). Other steps are idempotent (`SET`, `CREATE INDEX IF NOT EXISTS`, `ANALYZE`).
fn step_done(c: &mut Client, sql: &str) -> Result<bool> {
    let words: Vec<&str> = sql.split_whitespace().collect();
    let name_after = |a: &str, b: &str| words.windows(3).find(|w| w[0].eq_ignore_ascii_case(a) && w[1].eq_ignore_ascii_case(b)).map(|w| w[2]);
    let (name, need_valid) = match (name_after("VALIDATE", "CONSTRAINT"), name_after("ADD", "CONSTRAINT")) {
        (Some(n), _) => (n, true),
        (None, Some(n)) => (n, false),
        (None, None) => return Ok(false),
    };
    let row = c.query_opt("SELECT convalidated FROM pg_constraint WHERE conname = $1 AND connamespace = 'spansh'::regnamespace", &[&name])?;
    Ok(row.is_some_and(|r| !need_valid || r.get::<_, bool>(0)))
}

/// Splits a migration into statements (no function bodies or quoted `;` in these files).
fn statements(sql: &str) -> Vec<String> {
    let stripped: String = sql.lines().map(|l| l.split("--").next().unwrap_or("")).collect::<Vec<_>>().join("\n");
    stripped.split(';').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

/// Finds the run to continue, or starts a new one. Refuses to mix two versions of a source.
fn start_run(c: &mut Client, fp: &Fingerprint) -> Result<(i32, Vec<(u64, u64)>)> {
    let prev = c.query_opt(
        "SELECT id, status,
                content_length IS NOT DISTINCT FROM $2 AND etag IS NOT DISTINCT FROM $3
                AND last_modified IS NOT DISTINCT FROM $4 AND file_mtime IS NOT DISTINCT FROM $5
                AND gzip_crc32 IS NOT DISTINCT FROM $6 AND gzip_isize IS NOT DISTINCT FROM $7
         FROM spansh.import_run WHERE source = $1 ORDER BY id DESC LIMIT 1",
        &[&fp.source, &fp.content_length, &fp.etag, &fp.last_modified, &fp.file_mtime, &fp.gzip_crc32, &fp.gzip_isize],
    )?;
    if let Some(r) = prev {
        let (id, status, same): (i32, String, bool) = (r.get(0), r.get(1), r.get(2));
        if !same {
            bail!("source changed since run {id} (fingerprint differs); run `init` to start over: {fp:?}");
        }
        if status == "done" {
            bail!("run {id} already imported this source completely");
        }
        let ranges = c
            .query("SELECT first_ordinal, last_ordinal FROM spansh.import_batch WHERE run_id = $1 ORDER BY first_ordinal", &[&id])?
            .iter()
            .map(|r| (r.get::<_, i64>(0) as u64, r.get::<_, i64>(1) as u64))
            .collect();
        c.execute("UPDATE spansh.import_run SET status = 'running' WHERE id = $1", &[&id])?;
        return Ok((id, ranges));
    }
    if c.query_one("SELECT EXISTS (SELECT 1 FROM spansh.system)", &[])?.get::<_, bool>(0) {
        bail!("tables already hold data from another source; run `init` first");
    }
    let id: i32 = c
        .query_one(
            "INSERT INTO spansh.import_run (source, content_length, etag, last_modified, file_mtime, gzip_crc32, gzip_isize)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
            &[&fp.source, &fp.content_length, &fp.etag, &fp.last_modified, &fp.file_mtime, &fp.gzip_crc32, &fp.gzip_isize],
        )?
        .get(0);
    Ok((id, Vec::new()))
}

fn commit_batch(c: &mut Client, run_id: i32, b: &Batch) -> Result<()> {
    let mut tx = c.transaction()?;
    for ((name, cols), t) in TABLES.iter().zip(&b.tables) {
        if t.rows == 0 {
            continue;
        }
        let mut w = tx.copy_in(&format!("COPY spansh.{name} ({cols}) FROM STDIN (FORMAT binary)"))?;
        w.write_all(HEADER)?;
        w.write_all(&t.buf)?;
        w.write_all(TRAILER)?;
        w.finish().with_context(|| format!("COPY into {name}"))?;
    }
    tx.execute(
        "INSERT INTO spansh.import_batch (run_id, first_ordinal, last_ordinal, systems, json_bytes) VALUES ($1, $2, $3, $4, $5)",
        &[&run_id, &(b.first_ordinal as i64), &(b.last_ordinal as i64), &(b.systems as i32), &(b.json_bytes as i64)],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn run(o: &Options) -> Result<Summary> {
    let start = Instant::now();
    let fp = source::fingerprint(&o.source)?;
    let mut db = connect(&o.database)?;
    let (run_id, committed) = start_run(&mut db, &fp)?;
    let mut p = Projector { dict: Dict::load(connect(&o.database)?)?, issues: Issues::new(run_id, o.issue_cap) };
    let result = ingest(o, &mut db, &mut p, run_id, &committed, start);
    let status = match &result {
        Ok(s) if s.complete => "done",
        Ok(_) => "stopped",
        Err(_) => "failed",
    };
    // Best effort: the batches themselves are the durable record.
    let _ = db.execute(
        "UPDATE spansh.import_run SET status = $2, finished_at = now() WHERE id = $1",
        &[&run_id, &status],
    );
    result
}

fn ingest(o: &Options, db: &mut Client, p: &mut Projector, run_id: i32, committed: &[(u64, u64)], start: Instant) -> Result<Summary> {
    let limit = o.batch_mib << 20;
    let mut s = Summary { run_id, rows: vec![0; TABLES.len()], ..Default::default() };
    let input = source::open(&o.source)?;
    report::track(input.read.clone(), input.size, Unit::Bytes);
    let mut input = input.reader;
    let mut batch = Batch::new();
    let mut line = Vec::with_capacity(1 << 20);
    let mut ordinal = 0u64;
    let mut ranges = committed.iter().peekable();
    s.complete = true;
    loop {
        line.clear();
        let t = Instant::now();
        let n = input.read_until(b'\n', &mut line)?;
        s.t_read += t.elapsed();
        if n == 0 {
            break;
        }
        let Some(json) = source::system_bytes(&line) else { continue };
        if o.max_systems.is_some_and(|m| ordinal >= m) {
            s.complete = false;
            break;
        }
        ordinal += 1;
        while ranges.peek().is_some_and(|r| r.1 < ordinal) {
            ranges.next();
        }
        if ranges.peek().is_some_and(|r| r.0 <= ordinal) {
            s.skipped += 1;
            continue;
        }

        let t = Instant::now();
        let parsed = serde_json::from_slice(json);
        s.t_parse += t.elapsed();
        if batch.systems == 0 {
            batch.first_ordinal = ordinal;
        }
        batch.last_ordinal = ordinal;
        batch.systems += 1;
        batch.json_bytes += json.len() as u64;
        s.json_bytes += json.len() as u64;
        let t = Instant::now();
        let ok = match parsed {
            Ok(v) => p.system(ordinal, v, &mut batch)?,
            Err(e) => {
                let head = String::from_utf8_lossy(&json[..json.len().min(300)]);
                p.issues.add("system".into(), "parse_error", format!("{e}: {head}"));
                p.issues.flush(batch.t(crate::project::T::Issue), ordinal, None);
                false
            }
        };
        s.t_project += t.elapsed();
        if ok {
            s.systems += 1;
        } else {
            s.invalid += 1;
        }

        if batch.bytes() >= limit {
            flush(db, run_id, &mut batch, &mut s)?;
            let el = start.elapsed().as_secs_f64();
            report::detail(format!(
                "batch {}, {} systems (+{} skipped), {:.0} MB JSON ({:.0} MB/s), {:.0} MB COPY, {} issues",
                s.batches,
                s.systems,
                s.skipped,
                s.json_bytes as f64 / 1e6,
                s.json_bytes as f64 / 1e6 / el,
                s.copy_bytes as f64 / 1e6,
                p.issues.total
            ));
        }
    }
    flush(db, run_id, &mut batch, &mut s)?;
    s.issues = p.issues.total;
    s.elapsed = start.elapsed();
    Ok(s)
}

fn flush(db: &mut Client, run_id: i32, batch: &mut Batch, s: &mut Summary) -> Result<()> {
    if batch.systems == 0 {
        return Ok(());
    }
    let t = Instant::now();
    commit_batch(db, run_id, batch)?;
    s.t_copy += t.elapsed();
    s.batches += 1;
    s.copy_bytes += batch.bytes() as u64;
    for (acc, t) in s.rows.iter_mut().zip(&batch.tables) {
        *acc += t.rows;
    }
    batch.clear();
    Ok(())
}

impl Summary {
    /// Prints the summary to stderr and the log.
    pub fn print(&self) {
        let secs = self.elapsed.as_secs_f64();
        let rows: u64 = self.rows.iter().sum();
        let mut lines = vec![
            format!("run {} {}", self.run_id, if self.complete { "complete" } else { "stopped early (resumable)" }),
            format!(
                "systems {} (+{} skipped as already committed, {} invalid), {} batches, {} issues",
                self.systems, self.skipped, self.invalid, self.batches, self.issues
            ),
            format!(
                "{:.1}s total: {:.0} MB JSON ({:.0} MB/s), {:.0} systems/s, {} rows ({:.0} rows/s), {:.0} MB COPY",
                secs,
                self.json_bytes as f64 / 1e6,
                self.json_bytes as f64 / 1e6 / secs,
                self.systems as f64 / secs,
                rows,
                rows as f64 / secs,
                self.copy_bytes as f64 / 1e6
            ),
            format!(
                "time in: read+gunzip {:.1}s, parse {:.1}s, project {:.1}s, COPY+commit {:.1}s",
                self.t_read.as_secs_f64(),
                self.t_parse.as_secs_f64(),
                self.t_project.as_secs_f64(),
                self.t_copy.as_secs_f64()
            ),
        ];
        lines.extend(TABLES.iter().zip(&self.rows).filter(|(_, n)| **n > 0).map(|((name, _), n)| format!("  {name:<18} {n:>12}")));
        for l in &lines {
            report::log(Level::Info, "summary", l.trim());
            eprintln!("{l}");
        }
    }
}