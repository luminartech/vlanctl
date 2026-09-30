use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Profile applied when `apply` is invoked without a profile name.
const DEFAULT_APPLY_PROFILE: &str = "lum";

#[derive(Parser)]
#[command(
    name = "vlanctl",
    about = "Apply named VLAN profiles (macOS and Linux)"
)]
pub struct Cli {
    /// Directory containing profile .toml files.
    #[arg(long, default_value = "profiles", global = true)]
    pub profiles_dir: PathBuf,

    /// Also write the outcome as JSON to this file: `{"command", "ok",
    /// "message", "created"}`. For a program that runs vlanctl in a process
    /// whose output it cannot read — one launched elevated on Windows, say —
    /// this is how it learns what happened. Written on success and failure
    /// alike; a missing file afterwards means vlanctl never got as far as
    /// running the command.
    #[arg(long, global = true, value_name = "FILE")]
    pub report: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

impl Command {
    /// The subcommand's name as typed, for the report.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Command::List => "list",
            Command::Show { .. } => "show",
            Command::Validate { .. } => "validate",
            Command::Apply { .. } => "apply",
            Command::Down { .. } => "down",
            Command::Status => "status",
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// List available profiles.
    List,
    /// Print the commands a profile would run.
    Show {
        profile: String,
        /// Parent device to attach VLANs to, overriding the profile's
        /// `device` field. The parent is host-local — macOS numbers adapters
        /// `enN` per machine and Linux uses `eth0`/`enp*`/`enx*` — so it
        /// belongs on the command line, not in a committed profile.
        #[arg(long)]
        device: Option<String>,
    },
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
        /// Parent device to attach VLANs to, overriding the profile's
        /// `device` field. See `show --device`.
        #[arg(long)]
        device: Option<String>,
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
            Command::Apply {
                profile, dry_run, ..
            } => {
                assert_eq!(profile, "lum");
                assert!(!dry_run);
            }
            _ => panic!("expected Apply"),
        }
    }

    #[test]
    fn apply_accepts_a_device_override_and_defaults_to_none() {
        // The parent device is host-local: macOS `enN` numbering is per
        // machine and Linux uses `eth0`/`enp*` entirely, so it cannot live
        // in a committed profile. `--device` makes it a runtime argument.
        let cli = Cli::try_parse_from(["vlanctl", "apply", "lum", "--device", "eth0"]).unwrap();
        match cli.command {
            Command::Apply { device, .. } => assert_eq!(device.as_deref(), Some("eth0")),
            _ => panic!("expected Apply"),
        }
        let cli = Cli::try_parse_from(["vlanctl", "apply"]).unwrap();
        match cli.command {
            Command::Apply { device, .. } => assert_eq!(device, None),
            _ => panic!("expected Apply"),
        }
    }

    #[test]
    fn show_accepts_a_device_override() {
        let cli = Cli::try_parse_from(["vlanctl", "show", "lab", "--device", "enp0s31f6"]).unwrap();
        match cli.command {
            Command::Show { device, .. } => assert_eq!(device.as_deref(), Some("enp0s31f6")),
            _ => panic!("expected Show"),
        }
    }

    #[test]
    fn apply_with_profile_overrides_default() {
        let cli = Cli::try_parse_from(["vlanctl", "apply", "lum_legacy", "--dry-run"]).unwrap();
        match cli.command {
            Command::Apply {
                profile, dry_run, ..
            } => {
                assert_eq!(profile, "lum_legacy");
                assert!(dry_run);
            }
            _ => panic!("expected Apply"),
        }
    }
}
