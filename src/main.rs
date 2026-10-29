mod source;
mod survey;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

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
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Survey { source, out, workers } => {
            let report = survey::run(source::open(&source)?, workers, &source)?;
            std::fs::write(&out, &report)?;
            eprintln!("{}", report.lines().take(20).collect::<Vec<_>>().join("\n"));
            eprintln!("report written to {}", out.display());
        }
    }
    Ok(())
}
