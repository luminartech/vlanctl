use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "vlanctl", about = "Apply named VLAN profiles on macOS")]
pub struct Cli {
    /// Directory containing profile .toml files.
    #[arg(long, default_value = "profiles", global = true)]
    pub profiles_dir: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List available profiles.
    List,
    /// Print the commands a profile would run.
    Show { profile: String },
    /// Validate a profile without applying it.
    Validate { profile: String },
    /// Bring up a profile (requires root).
    Apply {
        profile: String,
        /// Print commands without executing them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Tear down the active profile (requires root).
    Down {
        /// Print commands without executing them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show what is currently up.
    Status,
}
