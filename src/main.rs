mod ingest;
mod pgcopy;
mod project;
mod source;
mod survey;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

const DEFAULT_DB: &str = "postgres://postgres@localhost:5432/elite";

#[derive(Parser)]
#[command(about = "Spansh galaxy dump importer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
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
    match Cli::parse().cmd {
        Cmd::Survey { source, out, workers } => {
            let report = survey::run(source::open(&source)?, workers, &source)?;
            std::fs::write(&out, &report)?;
            eprintln!("{}", report.lines().take(20).collect::<Vec<_>>().join("\n"));
            eprintln!("report written to {}", out.display());
        }
        Cmd::Init { database } => ingest::init(&database)?,
        Cmd::Ingest { source, database, batch_mib, max_systems, issue_cap } => {
            let s = ingest::run(&ingest::Options { source, database, batch_mib, max_systems, issue_cap })?;
            s.print();
        }
        Cmd::Finalize { database } => ingest::finalize(&database)?,
    }
    Ok(())
}
