mod db;
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

#[derive(Parser)]
#[command(about = "Spansh galaxy dump importer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Directory for log files (<command>-<UTC timestamp>.log).
    #[arg(long, global = true, default_value = "logs")]
    log_dir: PathBuf,
    /// Database schema holding the imported tables.
    #[arg(long, global = true, env = "SPANSH_SCHEMA", default_value = "spansh", value_parser = schema_name)]
    schema: String,
}

/// Lower-case SQL identifier, so it can be used in statements without quoting.
fn schema_name(s: &str) -> Result<String, String> {
    let ok = s.len() <= 63
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok { Ok(s.to_owned()) } else { Err("use lower-case letters, digits and _ (not starting with a digit)".into()) }
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
    /// Drop and recreate the import schema, empty, ready for loading.
    Init {
        /// Connection string (URL or key=value); PG* variables and the password file fill the rest.
        #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
        database: Option<String>,
    },
    /// Stream a dump into the database; resumes an interrupted run of the same source.
    Ingest {
        /// Dump file path or http(s) URL (.json or .json.gz).
        #[arg(long)]
        source: String,
        /// Connection string (URL or key=value); PG* variables and the password file fill the rest.
        #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
        database: Option<String>,
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
    /// Add primary keys, foreign keys and indexes after loading; resumes an interrupted run.
    Finalize {
        /// Connection string (URL or key=value); PG* variables and the password file fill the rest.
        #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
        database: Option<String>,
        /// Skip the 3D spatial index on system coordinates (cube extension).
        #[arg(long)]
        no_spatial: bool,
        /// Skip the system_flags route-planning view.
        #[arg(long)]
        no_flags: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let (command, about) = match &cli.cmd {
        Cmd::Survey { source, workers, .. } => ("survey", format!("survey {source} with {workers} workers")),
        Cmd::Init { .. } => ("init", format!("recreate schema {}", cli.schema)),
        Cmd::Ingest { source, batch_mib, .. } => ("ingest", format!("ingest {source} into schema {}, batches of {batch_mib} MiB", cli.schema)),
        Cmd::Finalize { .. } => ("finalize", format!("add keys and indexes in schema {}", cli.schema)),
    };
    report::start(command, &cli.log_dir, &about)?;
    match run(cli.cmd, &cli.schema) {
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
fn run(cmd: Cmd, schema: &str) -> Result<Box<dyn FnOnce()>> {
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
            ingest::init(&database.unwrap_or_default(), schema)?;
            Box::new(|| ())
        }
        Cmd::Ingest { source, database, batch_mib, max_systems, issue_cap } => {
            let s = ingest::run(&ingest::Options { source, database: database.unwrap_or_default(), schema: schema.to_owned(), batch_mib, max_systems, issue_cap })?;
            Box::new(move || s.print())
        }
        Cmd::Finalize { database, no_spatial, no_flags } => {
            ingest::finalize(&database.unwrap_or_default(), schema, !no_spatial, !no_flags)?;
            Box::new(|| ())
        }
    })
}
