mod cli;
mod commands;
mod config;
mod device;
mod net;
mod plan;
mod state;

use anyhow::{bail, Result};
use clap::Parser;
use cli::{Cli, Command};
use config::Profile;
use net::{RecordingRunner, SystemRunner};
use state::State;
use std::path::PathBuf;

fn profile_path(dir: &std::path::Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.toml"))
}

/// macOS: root has uid 0. Bail if not elevated.
fn require_root() -> Result<()> {
    // SAFETY: getuid is always safe to call and has no preconditions.
    let uid = unsafe { libc::getuid() };
    if uid != 0 {
        bail!("this command modifies network interfaces and must be run with sudo");
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let state_path = State::default_path();

    match cli.command {
        Command::List => {
            for name in commands::list_profiles(&cli.profiles_dir)? {
                println!("{name}");
            }
        }
        Command::Show { profile } => {
            let p = Profile::load(&profile_path(&cli.profiles_dir, &profile))?;
            // Use a recording runner to resolve the device without side effects.
            let mut probe = SystemRunner;
            let device = device::resolve_device(&mut probe, p.device.as_deref())?;
            for line in commands::show_plan(&p, &device) {
                println!("{line}");
            }
        }
        Command::Validate { profile } => {
            Profile::load(&profile_path(&cli.profiles_dir, &profile))?;
            println!("{profile}: ok");
        }
        Command::Apply { profile, dry_run } => {
            let p = Profile::load(&profile_path(&cli.profiles_dir, &profile))?;
            if dry_run {
                let mut runner = RecordingRunner::default();
                // Seed ifconfig -l so device auto-detect and interface
                // allocation work offline. Real apply queries the live system.
                runner
                    .stdout
                    .insert("ifconfig -l".to_string(), "lo0 en0".to_string());
                commands::apply(&mut runner, &p, &state_path, true)?;
                for cmd in &runner.commands {
                    println!("{}", cmd.display());
                }
            } else {
                require_root()?;
                let mut runner = SystemRunner;
                let created = commands::apply(&mut runner, &p, &state_path, false)?;
                println!("applied '{}': {}", p.name, created.join(", "));
            }
        }
        Command::Down { dry_run } => {
            if dry_run {
                let mut runner = RecordingRunner::default();
                commands::down(&mut runner, &state_path, true)?;
                for cmd in &runner.commands {
                    println!("{}", cmd.display());
                }
            } else {
                require_root()?;
                let mut runner = SystemRunner;
                commands::down(&mut runner, &state_path, false)?;
                println!("torn down");
            }
        }
        Command::Status => {
            let mut runner = SystemRunner;
            print!("{}", commands::status(&mut runner, &state_path)?);
        }
    }
    Ok(())
}
