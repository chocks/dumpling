use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "dumpling",
    about = "Copy production MySQL databases into local environments while anonymizing secrets.",
    long_about = "Dumpling makes safe copies of production databases for local development.\n\nCopy production. Lose the secrets. Keep the bugs.",
    version
)]
pub struct Cli {
    /// Path to configuration file (default: .dumpling.yml)
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    /// Reduce output (still prints errors)
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Increase logging verbosity (-v, -vv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Create a commented example .dumpling.yml
    Init {
        /// Overwrite an existing config file
        #[arg(long)]
        force: bool,

        /// Output path (default: .dumpling.yml)
        #[arg(long)]
        path: Option<PathBuf>,
    },

    /// Parse config, verify connections and rules without copying data
    Validate,

    /// Inspect schema and report potentially sensitive columns
    Scan,

    /// Copy and anonymize data according to configuration
    Sync {
        /// Show the plan without modifying the destination
        #[arg(long)]
        dry_run: bool,

        /// Restrict to these tables (comma-separated)
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,

        /// Copy a configured table group (and FK dependencies)
        #[arg(long)]
        group: Option<String>,

        /// Select a named environment profile
        #[arg(long)]
        env: Option<String>,

        /// Permit explicitly configured destructive destination actions.
        /// Never overrides production-destination protections.
        #[arg(long)]
        force: bool,
    },

    /// Print version information
    Version,
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
