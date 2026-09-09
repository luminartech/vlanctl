mod cli;

use anyhow::{Result, bail};
use clap::Parser;
use cli::{Cli, Command};
use config::Profile;
use net::{RecordingRunner, SystemRunner};
use state::State;
use std::path::PathBuf;
use vlanctl::{commands, config, device, net, state};

fn profile_path(dir: &std::path::Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.toml"))
}

/// Print the commands a recording runner captured, omitting read-only probes
/// (device detection) so a dry run previews only the changes it would make.
fn print_planned_commands(runner: &RecordingRunner) {
    for cmd in &runner.commands {
        if is_read_only_probe(cmd) {
            continue;
        }
        println!("{}", cmd.display());
    }
}

/// A command that only inspects system state (used during device resolution),
/// as opposed to one that creates/configures/destroys interfaces or routes.
fn is_read_only_probe(cmd: &net::Cmd) -> bool {
    match cmd.program.as_str() {
        // `networksetup -listallhardwareports` lists adapters.
        "networksetup" => true,
        // `ifconfig -l` or `ifconfig <iface>` query; mutations take more args
        // (e.g. `ifconfig vlan10 create ...`).
        "ifconfig" => cmd.args.len() <= 1,
        _ => false,
    }
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
            // Resolve the device against the live system; `ifconfig -l` is a
            // read-only query, so this has no side effects.
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
                print_planned_commands(&runner);
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
                print_planned_commands(&runner);
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
