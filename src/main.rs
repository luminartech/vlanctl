//! CLI entry point. Requires the `cli` feature (on by default); a library
//! consumer builds with `--no-default-features` and links the lib alone.

#[cfg(feature = "cli")]
mod cli;

#[cfg(feature = "cli")]
use anyhow::{Result, bail};
#[cfg(feature = "cli")]
use clap::Parser;
#[cfg(feature = "cli")]
use cli::{Cli, Command};
#[cfg(feature = "cli")]
use config::Profile;
#[cfg(feature = "cli")]
use net::{RecordingRunner, SystemRunner};
#[cfg(feature = "cli")]
use state::State;
#[cfg(feature = "cli")]
use std::path::PathBuf;
#[cfg(feature = "cli")]
use vlanctl::{commands, config, device, net, plan, state};

#[cfg(feature = "cli")]
fn profile_path(dir: &std::path::Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.toml"))
}

/// Print the commands a recording runner captured, omitting read-only probes
/// (device detection) so a dry run previews only the changes it would make.
#[cfg(feature = "cli")]
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
#[cfg(feature = "cli")]
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
#[cfg(feature = "cli")]
fn require_root() -> Result<()> {
    // SAFETY: getuid is always safe to call and has no preconditions.
    let uid = unsafe { libc::getuid() };
    if uid != 0 {
        bail!("this command modifies network interfaces and must be run with sudo");
    }
    Ok(())
}

#[cfg(not(feature = "cli"))]
fn main() {
    eprintln!("vlanctl was built without the `cli` feature");
    std::process::exit(2);
}

#[cfg(feature = "cli")]
fn main() -> Result<()> {
    let cli = Cli::parse();
    let state_path = State::default_path();

    match cli.command {
        Command::List => {
            for name in commands::list_profiles(&cli.profiles_dir)? {
                println!("{name}");
            }
        }
        Command::Show { profile, device } => {
            let p = Profile::load(&profile_path(&cli.profiles_dir, &profile))?;
            // Resolve the device against the live system; `ifconfig -l` is a
            // read-only query, so this has no side effects.
            let mut probe = SystemRunner;
            // Resolved through the SAME reference platform the preview renders
            // with, so the device it picks and the commands it prints agree.
            let device = device::resolve_device(
                plan::preview_platform(),
                &mut probe,
                device.as_deref().or(p.device.as_deref()),
            )?;
            // A preview renders through the fixed reference platform, not
            // `host_platform()`: like `apply --dry-run`/`down --dry-run`
            // below, it touches no real system and must keep working on any
            // host, so it must not fail just because this build has no real
            // backend for the host OS.
            for line in commands::show_plan(plan::preview_platform(), &p, &device) {
                println!("{line}");
            }
        }
        Command::Validate { profile } => {
            Profile::load(&profile_path(&cli.profiles_dir, &profile))?;
            println!("{profile}: ok");
        }
        Command::Apply {
            profile,
            dry_run,
            device,
        } => {
            let p = Profile::load(&profile_path(&cli.profiles_dir, &profile))?;
            if dry_run {
                // A dry run touches no real system and seeds its own
                // `ifconfig -l` below, so — like every other preview this
                // crate shipped before the `Platform` seam existed — it must
                // keep working on any host. It renders through
                // `plan::preview_platform()`, not the fallible
                // `host_platform()`, which is reserved for the real-apply
                // arm below (see that function's doc for why).
                let mut runner = RecordingRunner::default();
                // Seed ifconfig -l so device auto-detect and interface
                // allocation work offline. Real apply queries the live system.
                runner
                    .stdout
                    .insert("ifconfig -l".to_string(), "lo0 en0".to_string());
                commands::apply(
                    &mut runner,
                    plan::preview_platform(),
                    &p,
                    device.as_deref(),
                    &state_path,
                    true,
                )?;
                print_planned_commands(&runner);
            } else {
                require_root()?;
                let platform = plan::host_platform()?;
                let mut runner = SystemRunner;
                let created = commands::apply(
                    &mut runner,
                    &*platform,
                    &p,
                    device.as_deref(),
                    &state_path,
                    false,
                )?;
                println!("applied '{}': {}", p.name, created.join(", "));
            }
        }
        Command::Down { dry_run } => {
            if dry_run {
                // See the matching comment in `Command::Apply`: dry runs must
                // keep working on any host, so they render through
                // `plan::preview_platform()`, never the fallible
                // `host_platform()`.
                let mut runner = RecordingRunner::default();
                commands::down(&mut runner, plan::preview_platform(), &state_path, true)?;
                print_planned_commands(&runner);
            } else {
                require_root()?;
                let platform = plan::host_platform()?;
                let mut runner = SystemRunner;
                commands::down(&mut runner, &*platform, &state_path, false)?;
                println!("torn down");
            }
        }
        Command::Status => {
            let mut runner = SystemRunner;
            print!(
                "{}",
                commands::status(&mut runner, plan::preview_platform(), &state_path)?
            );
        }
    }
    Ok(())
}
