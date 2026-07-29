mod scope;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Bench tooling for the DSO (Siglent SDS2102X Plus).
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Save a screenshot of the scope's display as a PNG.
    ScopeShot {
        /// Output file path.
        #[arg(short, long, default_value = "scope.png")]
        output: PathBuf,
    },
    /// Export one channel's on-screen waveform as CSV (sample,volts).
    ScopeDump {
        /// Channel to read, e.g. C1, C2.
        #[arg(short, long, default_value = "C1")]
        channel: String,
        /// Output file path.
        #[arg(short, long, default_value = "waveform.csv")]
        output: PathBuf,
    },
    /// Send a raw SCPI query and print the reply (debugging aid).
    ScopeQuery {
        command: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::ScopeShot { output } => {
            let mut dso = scope::Scope::open()?;
            let png = dso.screenshot_png()?;
            std::fs::write(&output, png)
                .with_context(|| format!("writing {}", output.display()))?;
            println!("saved {}", output.display());
        }
        Command::ScopeDump { channel, output } => {
            let mut dso = scope::Scope::open()?;
            let samples = dso.dump_waveform(&channel)?;
            let mut csv = String::from("sample,volts\n");
            for (i, v) in samples {
                csv.push_str(&format!("{i},{v}\n"));
            }
            std::fs::write(&output, csv)
                .with_context(|| format!("writing {}", output.display()))?;
            println!("saved {}", output.display());
        }
        Command::ScopeQuery { command } => {
            let mut dso = scope::Scope::open()?;
            println!("{}", dso.query(&command)?);
        }
    }
    Ok(())
}
