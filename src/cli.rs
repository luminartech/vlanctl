use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Profile applied when `apply` is invoked without a profile name.
const DEFAULT_APPLY_PROFILE: &str = "lum";

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
    /// Bring up a profile (requires root). Defaults to the `lum` profile.
    Apply {
        /// Profile to apply (defaults to "lum").
        #[arg(default_value = DEFAULT_APPLY_PROFILE)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_without_profile_defaults_to_lum() {
        let cli = Cli::try_parse_from(["vlanctl", "apply"]).unwrap();
        match cli.command {
            Command::Apply { profile, dry_run } => {
                assert_eq!(profile, "lum");
                assert!(!dry_run);
            }
            _ => panic!("expected Apply"),
        }
    }

    #[test]
    fn apply_with_profile_overrides_default() {
        let cli = Cli::try_parse_from(["vlanctl", "apply", "lum_legacy", "--dry-run"]).unwrap();
        match cli.command {
            Command::Apply { profile, dry_run } => {
                assert_eq!(profile, "lum_legacy");
                assert!(dry_run);
            }
            _ => panic!("expected Apply"),
        }
    }
}
