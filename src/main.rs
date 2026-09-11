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
use net::SystemRunner;
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
fn print_planned_commands(commands: &[net::Cmd]) {
    for cmd in commands {
        if is_read_only_probe(cmd) {
            continue;
        }
        println!("{}", cmd.display());
    }
}

/// Runner for a **preview**: executes read-only probes against the live
/// system, records everything else without running it.
///
/// A dry run has to read real host state to be worth anything — which devices
/// exist, which VLAN ids are already taken — or it previews against a fiction.
/// It obviously must not *change* anything, so the split is
/// [`is_read_only_probe`], and that function is written as a strict allowlist
/// for exactly this reason. `show` has always probed the live system on the
/// same reasoning; this extends it to `apply --dry-run` and `down --dry-run`.
///
/// Returning empty stdout for a recorded mutation is safe: the backends only
/// parse output from the probes above, never from a command that changes
/// something.
#[cfg(feature = "cli")]
struct PreviewRunner {
    probe: SystemRunner,
    recorded: Vec<net::Cmd>,
}

#[cfg(feature = "cli")]
impl net::CommandRunner for PreviewRunner {
    fn run(&mut self, cmd: &net::Cmd) -> Result<String> {
        self.recorded.push(cmd.clone());
        if is_read_only_probe(cmd) {
            self.probe.run(cmd)
        } else {
            Ok(String::new())
        }
    }
}

/// A command that only inspects system state, as opposed to one that
/// creates/configures/destroys interfaces or routes.
///
/// **This is a safety boundary, not a convenience.** A dry run executes
/// exactly the commands this returns `true` for, so a false positive turns a
/// preview into an apply. It is therefore a strict ALLOWLIST of the probe
/// forms the backends actually emit — never a denylist of known mutations,
/// which would classify anything unrecognised as safe.
#[cfg(feature = "cli")]
fn is_read_only_probe(cmd: &net::Cmd) -> bool {
    let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
    match cmd.program.as_str() {
        // macOS. `networksetup -listallhardwareports` lists adapters;
        // `ifconfig -l` / `ifconfig <iface>` query. Mutations take more args
        // (`ifconfig vlan10 create ...`).
        "networksetup" => matches!(args.as_slice(), ["-listallhardwareports"]),
        "ifconfig" => args.len() <= 1,
        // Linux. `ip` is the mutation tool as well as the query tool, so only
        // these exact query forms pass: every `add`/`del`/`set` shape falls
        // through to `false`.
        "ip" => matches!(
            args.as_slice(),
            ["-json", "link", "show"]
                | ["-d", "-json", "link", "show"]
                | ["-json", "addr", "show", "dev", _]
        ),
        // Sysfs reads. `cat` and `ls` are general tools, so they are confined
        // to the one tree the Linux backend reads from.
        "cat" | "ls" => {
            matches!(args.as_slice(), [path] if path.starts_with("/sys/class/net/"))
        }
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
                &*plan::preview_platform(),
                &mut probe,
                device.as_deref().or(p.device.as_deref()),
            )?;
            // A preview renders through the fixed reference platform, not
            // `host_platform()`: like `apply --dry-run`/`down --dry-run`
            // below, it touches no real system and must keep working on any
            // host, so it must not fail just because this build has no real
            // backend for the host OS.
            for line in commands::show_plan(&*plan::preview_platform(), &p, &device) {
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
                // reads real host state through `PreviewRunner` (read-only
                // probes only) and records every mutation without running it.
                // It previously seeded a fake `ifconfig -l` and read nothing,
                // so the collision guard could never fire in a preview and the
                // output was rendered against a fiction.
                let mut runner = PreviewRunner {
                    probe: SystemRunner,
                    recorded: Vec::new(),
                };
                commands::apply(
                    &mut runner,
                    &*plan::preview_platform(),
                    &p,
                    device.as_deref(),
                    &state_path,
                    true,
                )?;
                print_planned_commands(&runner.recorded);
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
                let mut runner = PreviewRunner {
                    probe: SystemRunner,
                    recorded: Vec::new(),
                };
                commands::down(&mut runner, &*plan::preview_platform(), &state_path, true)?;
                print_planned_commands(&runner.recorded);
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
                commands::status(&mut runner, &*plan::preview_platform(), &state_path)?
            );
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "cli"))]
mod tests {
    use super::*;
    use net::Cmd;

    /// THE safety property. `is_read_only_probe` decides what a dry run
    /// executes for real, so a false positive here turns a preview into an
    /// apply. Every mutating command this crate can emit, on either backend,
    /// must classify as NOT read-only.
    #[test]
    fn mutating_commands_are_never_classified_read_only() {
        let mutations = [
            // Linux
            Cmd::new(
                "ip",
                &[
                    "link", "add", "link", "eth0", "name", "eth0.11", "type", "vlan", "id", "11",
                ],
            ),
            Cmd::new("ip", &["link", "set", "eth0", "up"]),
            Cmd::new("ip", &["link", "del", "eth0.11"]),
            Cmd::new("ip", &["addr", "add", "192.168.11.87/24", "dev", "eth0.11"]),
            Cmd::new("ip", &["addr", "del", "192.168.11.87/24", "dev", "eth0"]),
            Cmd::new(
                "ip",
                &["route", "add", "192.168.11.151/32", "dev", "eth0.11"],
            ),
            Cmd::new(
                "ip",
                &[
                    "neigh",
                    "replace",
                    "192.168.11.151",
                    "lladdr",
                    "3a:42:f7:79:32:2e",
                    "dev",
                    "eth0.11",
                ],
            ),
            // macOS
            Cmd::new("ifconfig", &["vlan11", "create"]),
            Cmd::new("ifconfig", &["vlan11", "vlan", "11", "vlandev", "en7"]),
            Cmd::new("ifconfig", &["vlan11", "destroy"]),
            Cmd::new(
                "route",
                &["add", "-host", "192.168.10.151", "-interface", "en7"],
            ),
            Cmd::new("arp", &["-s", "192.168.10.151", "3a:42:f7:79:32:2e"]),
        ];
        for cmd in &mutations {
            assert!(
                !is_read_only_probe(cmd),
                "MUTATION classified as a read-only probe, so a dry run would \
                 EXECUTE it: {}",
                cmd.display()
            );
        }
    }

    /// The complement: the probes a preview genuinely needs must pass, or the
    /// dry run reads nothing and cannot reflect the host.
    #[test]
    fn the_backends_own_probes_are_classified_read_only() {
        let probes = [
            Cmd::new("ip", &["-json", "link", "show"]),
            Cmd::new("ip", &["-d", "-json", "link", "show"]),
            Cmd::new("ip", &["-json", "addr", "show", "dev", "eth0"]),
            Cmd::new("cat", &["/sys/class/net/eth0/carrier"]),
            Cmd::new("cat", &["/sys/class/net/eth0/operstate"]),
            Cmd::new("ls", &["/sys/class/net/eth0"]),
            Cmd::new("ifconfig", &["-l"]),
            Cmd::new("ifconfig", &["en0"]),
            Cmd::new("networksetup", &["-listallhardwareports"]),
        ];
        for cmd in &probes {
            assert!(
                is_read_only_probe(cmd),
                "probe not recognised, so a dry run would stub it out: {}",
                cmd.display()
            );
        }
    }

    /// `cat`/`ls` are allowed only under `/sys/class/net`. They are general
    /// tools, so an unrestricted allowance would let any path through.
    #[test]
    fn cat_and_ls_are_confined_to_sys_class_net() {
        assert!(!is_read_only_probe(&Cmd::new("cat", &["/etc/shadow"])));
        assert!(!is_read_only_probe(&Cmd::new("ls", &["/"])));
        assert!(!is_read_only_probe(&Cmd::new("cat", &[])));
    }
}
