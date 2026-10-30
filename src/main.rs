mod ingest;
mod pgcopy;
mod project;
mod report;
mod source;
mod survey;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::report::Level;

const DEFAULT_DB: &str = "postgres://postgres@localhost:5432/elite";

#[derive(Parser)]
#[command(about = "Spansh galaxy dump importer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Directory for log files (<command>-<UTC timestamp>.log).
    #[arg(long, global = true, default_value = "logs")]
    log_dir: PathBuf,
}

#[derive(Subcommand)]
enum Cmd {
    /// Report every JSON path's types, ranges and distinct values.
    Survey {
        /// Dump file path or http(s) URL (.json or .json.gz).
        source: String,
        /// Report file to write.
        #[arg(long, default_value = "survey.txt")]
        out: PathBuf,
        #[arg(long, default_value_t = 8)]
        workers: usize,
    },
    /// Drop and recreate the `spansh` schema, empty, ready for loading.
    Init {
        #[arg(long, env = "DATABASE_URL", default_value = DEFAULT_DB)]
        database: String,
    },
    /// Stream a dump into the database; resumes an interrupted run of the same source.
    Ingest {
        /// Dump file path or http(s) URL (.json or .json.gz).
        #[arg(long)]
        source: String,
        #[arg(long, env = "DATABASE_URL", default_value = DEFAULT_DB)]
        database: String,
        /// Commit after this many MiB of encoded COPY data.
        #[arg(long, default_value_t = 64)]
        batch_mib: usize,
        /// Stop after this many systems (counted from the start of the dump).
        #[arg(long)]
        max_systems: Option<u64>,
        /// Maximum issue rows stored per run (all are counted).
        #[arg(long, default_value_t = 100_000)]
        issue_cap: u64,
    },
    /// Add primary keys, foreign keys and indexes after loading.
    Finalize {
        #[arg(long, env = "DATABASE_URL", default_value = DEFAULT_DB)]
        database: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let (command, about) = match &cli.cmd {
        Cmd::Survey { source, workers, .. } => ("survey", format!("survey {source} with {workers} workers")),
        Cmd::Init { .. } => ("init", "recreate schema spansh".to_owned()),
        Cmd::Ingest { source, batch_mib, .. } => ("ingest", format!("ingest {source}, batches of {batch_mib} MiB")),
        Cmd::Finalize { .. } => ("finalize", "add keys and indexes".to_owned()),
    };
    report::start(command, &cli.log_dir, &about)?;
    match run(cli.cmd) {
        Ok(after) => {
            report::finish(true);
            after();
            Ok(())
        }
        Err(e) => {
            // Shown in the terminal and the log by report::log.
            report::log(Level::Error, "error", &format!("{e:#}"));
            report::finish(false);
            std::process::exit(1);
        }
    }
}

/// Runs the command; the returned closure prints its results once the status display is closed.
fn run(cmd: Cmd) -> Result<Box<dyn FnOnce()>> {
    Ok(match cmd {
        Cmd::Survey { source, out, workers } => {
            let report = survey::run(source::open(&source)?, workers, &source)?;
            std::fs::write(&out, &report)?;
            Box::new(move || {
                eprintln!("{}", report.lines().take(20).collect::<Vec<_>>().join("\n"));
                eprintln!("report written to {}", out.display());
            })
        }
        Cmd::Init { database } => {
            ingest::init(&database)?;
            Box::new(|| ())
        }
        Cmd::Ingest { source, database, batch_mib, max_systems, issue_cap } => {
            let s = ingest::run(&ingest::Options { source, database, batch_mib, max_systems, issue_cap })?;
            Box::new(move || s.print())
        }
        Cmd::Finalize { database } => {
            ingest::finalize(&database)?;
            Box::new(|| ())
        }
    })
}
