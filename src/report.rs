//! Progress display and log file shared by all commands.
//!
//! The terminal gets a status block redrawn twice a second (or, when stderr is not a terminal, the
//! progress line once a minute). The log file gets one line per event, always the same four
//! fields in logfmt: `ts=<RFC 3339 UTC> level=<INFO|WARN|ERROR> event=<word> msg="<text>"`.
//! Progress is logged once a minute; each event name is logged at most `EVENT_CAP` times, after
//! which repeats are only counted and reported with the next progress line.

use std::collections::HashMap;
use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};

const EVENT_CAP: u64 = 20;
const LOG_EVERY: Duration = Duration::from_secs(60);
const REDRAW_EVERY: Duration = Duration::from_millis(500);
const WIDTH: usize = 110;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Unit {
    Bytes,
    Steps,
}

struct State {
    command: String,
    path: PathBuf,
    file: File,
    tty: bool,
    start: Instant,
    last_log: Instant,
    drawn: usize,
    done: Option<(Arc<AtomicU64>, Option<u64>, Unit)>,
    detail: String,
    note: String,
    counts: HashMap<&'static str, u64>,
    suppressed: HashMap<&'static str, u64>,
    warnings: u64,
    errors: u64,
    finished: bool,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// Opens `<dir>/<command>-<YYYYMMDD-HHMMSS>.log` and starts the display thread.
pub fn start(command: &str, dir: &Path, about: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let stamp = utc(SystemTime::now());
    let name = format!("{command}-{}{}{}-{}{}{}.log", &stamp[0..4], &stamp[5..7], &stamp[8..10], &stamp[11..13], &stamp[14..16], &stamp[17..19]);
    // A second run started in the same second gets a numbered name instead of sharing the file.
    let (mut path, mut n) = (dir.join(&name), 1);
    let file = loop {
        match File::options().write(true).create_new(true).open(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => {
                n += 1;
                path = dir.join(name.replace(".log", &format!("-{n}.log")));
            }
            r => break r.with_context(|| format!("opening {}", path.display()))?,
        }
    };
    let tty = std::io::stderr().is_terminal() && enable_ansi();
    let now = Instant::now();
    let state = State {
        command: command.to_owned(),
        path,
        file,
        tty,
        start: now,
        last_log: now,
        drawn: 0,
        done: None,
        detail: String::new(),
        note: String::new(),
        counts: HashMap::new(),
        suppressed: HashMap::new(),
        warnings: 0,
        errors: 0,
        finished: false,
    };
    if STATE.set(Mutex::new(state)).is_err() {
        anyhow::bail!("report::start called twice");
    }
    log(Level::Info, "start", about);
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(REDRAW_EVERY);
            let Some(m) = STATE.get() else { return };
            let mut s = m.lock().unwrap();
            if s.finished {
                return;
            }
            s.tick();
        }
    });
    Ok(())
}

/// Progress is `done / total` of `unit`; `done` is read by the display thread.
pub fn track(done: Arc<AtomicU64>, total: Option<u64>, unit: Unit) {
    with(|s| s.done = Some((done, total, unit)));
}

/// The status line under the progress bar.
pub fn detail(text: impl Into<String>) {
    let text = text.into();
    with(|s| s.detail = text);
}

/// A second, independently updated part of the status line (e.g. a sub-step's progress).
pub fn note(text: impl Into<String>) {
    let text = text.into();
    with(|s| s.note = text);
}

/// Writes one event to the log file (rate-limited per event name). Warnings and errors are also
/// shown in the terminal above the status block.
pub fn log(level: Level, event: &'static str, msg: &str) {
    let printed = STATE.get().is_none();
    with(|s| s.event(level, event, msg));
    if printed && level != Level::Info {
        eprintln!("{}: {msg}", level.as_str());
    }
}

/// Final progress line and log entry; leaves the terminal below the status block.
pub fn finish(ok: bool) {
    with(|s| {
        s.write_progress();
        let (event, level) = if ok { ("done", Level::Info) } else { ("failed", Level::Error) };
        let msg = format!("{} after {} ({} warnings, {} errors), log {}", if ok { "finished" } else { "failed" }, hms(s.start.elapsed()), s.warnings, s.errors, s.path.display());
        s.event(level, event, &msg);
        s.draw();
        s.finished = true;
        if !s.tty {
            eprintln!("{msg}");
        }
    });
}

fn with(f: impl FnOnce(&mut State)) {
    if let Some(m) = STATE.get() {
        f(&mut m.lock().unwrap());
    }
}

impl State {
    fn tick(&mut self) {
        if self.last_log.elapsed() >= LOG_EVERY {
            self.last_log = Instant::now();
            self.write_progress();
        }
        self.draw();
    }

    fn write_progress(&mut self) {
        let status = self.status_text();
        let line = if status.is_empty() { self.progress_line() } else { format!("{} | {status}", self.progress_line()) };
        self.write(Level::Info, "progress", &line);
        if !self.tty {
            eprintln!("{line}");
        }
        let suppressed: Vec<_> = self.suppressed.drain().filter(|(_, n)| *n > 0).collect();
        for (event, n) in suppressed {
            self.write(Level::Info, "suppressed", &format!("{n} more '{event}' events since the last progress line"));
        }
    }

    fn event(&mut self, level: Level, event: &'static str, msg: &str) {
        match level {
            Level::Warn => self.warnings += 1,
            Level::Error => self.errors += 1,
            Level::Info => {}
        }
        let n = self.counts.entry(event).or_default();
        *n += 1;
        // Info events come from bounded code paths; only warnings and errors can repeat without limit.
        if level == Level::Info {
            self.write(level, event, msg);
            return;
        }
        if *n > EVENT_CAP {
            *self.suppressed.entry(event).or_default() += 1;
            return;
        }
        if *n == EVENT_CAP {
            self.write(level, event, &format!("{msg} (further '{event}' events are counted, not logged)"));
        } else {
            self.write(level, event, msg);
        }
        if level != Level::Info && !self.finished {
            self.clear();
            eprintln!("{}: {}", level.as_str(), msg);
            self.draw();
        }
    }

    fn write(&mut self, level: Level, event: &str, msg: &str) {
        let _ = writeln!(self.file, "ts={} level={} event={} msg={}", utc(SystemTime::now()), level.as_str(), event, quote(msg));
    }

    fn status_text(&self) -> String {
        match (self.detail.is_empty(), self.note.is_empty()) {
            (_, true) => self.detail.clone(),
            (true, false) => self.note.clone(),
            (false, false) => format!("{} - {}", self.detail, self.note),
        }
    }

    fn progress_line(&self) -> String {
        let elapsed = self.start.elapsed();
        let Some((done, total, unit)) = &self.done else { return format!("{} elapsed", hms(elapsed)) };
        let done = done.load(Ordering::Relaxed);
        let amount = |n: u64| match unit {
            Unit::Bytes if *total >= Some(1_000_000_000) => format!("{:.1} GB", n as f64 / 1e9),
            Unit::Bytes => format!("{:.1} MB", n as f64 / 1e6),
            Unit::Steps => n.to_string(),
        };
        match total {
            Some(total) if *total > 0 => {
                let frac = (done as f64 / *total as f64).min(1.0);
                let bar = (frac * 20.0) as usize;
                let eta = if done > 0 && frac < 1.0 && *unit == Unit::Bytes {
                    format!(", ETA {}", hms(elapsed.mul_f64((1.0 - frac) / frac)))
                } else {
                    String::new()
                };
                format!("[{}{}] {:5.1}%  {} / {}  {} elapsed{eta}", "#".repeat(bar), ".".repeat(20 - bar), frac * 100.0, amount(done), amount(*total), hms(elapsed))
            }
            _ => format!("{} done, {} elapsed", amount(done), hms(elapsed)),
        }
    }

    fn draw(&mut self) {
        if !self.tty || self.finished {
            return;
        }
        self.clear();
        let lines = [
            format!("{}  {}", self.command, self.progress_line()),
            self.status_text(),
            format!("warnings {}  errors {}  log {}", self.warnings, self.errors, self.path.display()),
        ];
        let mut err = std::io::stderr().lock();
        for l in &lines {
            let _ = writeln!(err, "{}", truncate(l));
        }
        self.drawn = lines.len();
    }

    /// Moves the cursor back over the status block and erases it.
    fn clear(&mut self) {
        if self.tty && self.drawn > 0 {
            eprint!("\x1b[{}F\x1b[J", self.drawn);
            self.drawn = 0;
        }
    }
}

fn truncate(s: &str) -> &str {
    match s.char_indices().nth(WIDTH) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn quote(s: &str) -> String {
    let mut q = String::with_capacity(s.len() + 2);
    q.push('"');
    for c in s.chars() {
        match c {
            '"' => q.push_str("\\\""),
            '\\' => q.push_str("\\\\"),
            '\n' => q.push_str("\\n"),
            '\r' => q.push_str("\\r"),
            '\t' => q.push_str("\\t"),
            c => q.push(c),
        }
    }
    q.push('"');
    q
}

pub fn hms(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`
fn utc(t: SystemTime) -> String {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let (days, rem) = ((d.as_secs() / 86_400) as i64, d.as_secs() % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem / 60 % 60, rem % 60, d.subsec_millis())
}

/// Windows consoles need VT processing switched on for the cursor escapes; elsewhere it is a no-op.
#[cfg(windows)]
fn enable_ansi() -> bool {
    unsafe extern "system" {
        fn GetStdHandle(n: u32) -> *mut std::ffi::c_void;
        fn GetConsoleMode(h: *mut std::ffi::c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: *mut std::ffi::c_void, mode: u32) -> i32;
    }
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 4;
    unsafe {
        let h = GetStdHandle(STD_ERROR_HANDLE);
        let mut mode = 0;
        GetConsoleMode(h, &mut mode) != 0 && SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn enable_ansi() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_log_fields() {
        assert_eq!(utc(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc(SystemTime::UNIX_EPOCH + Duration::from_millis(1_782_774_216_123)), "2026-06-29T23:03:36.123Z");
        assert_eq!(utc(SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00.000Z");
        assert_eq!(quote("a \"b\"\n\\"), r#""a \"b\"\n\\""#);
        assert_eq!(hms(Duration::from_secs(3725)), "1h02m");
        assert_eq!(hms(Duration::from_secs(125)), "2m05s");
    }

    #[test]
    fn caps_repeated_events() {
        let mut s = State {
            command: "test".into(),
            path: PathBuf::from("x"),
            file: tempfile(),
            tty: false,
            start: Instant::now(),
            last_log: Instant::now(),
            drawn: 0,
            done: None,
            detail: String::new(),
            note: String::new(),
            counts: HashMap::new(),
            suppressed: HashMap::new(),
            warnings: 0,
            errors: 0,
            finished: true,
        };
        for _ in 0..100 {
            s.event(Level::Warn, "reconnect", "x");
        }
        assert_eq!(s.warnings, 100);
        assert_eq!(s.suppressed["reconnect"], 100 - EVENT_CAP);
    }

    fn tempfile() -> File {
        let p = std::env::temp_dir().join(format!("report-test-{}.log", std::process::id()));
        File::create(p).unwrap()
    }
}
