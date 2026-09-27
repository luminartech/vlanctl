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
        // Windows. A PowerShell command line cannot be told apart on argv
        // alone — the script is one argument — so the backend owns the
        // allowlist for its own probes, statement by statement.
        "powershell.exe" => plan::WindowsHyperV::is_read_only_probe(cmd),
        _ => false,
    }
}

/// Unix: root has uid 0. Bail if not elevated.
#[cfg(all(feature = "cli", unix))]
fn require_root() -> Result<()> {
    // SAFETY: getuid is always safe to call and has no preconditions.
    let uid = unsafe { libc::getuid() };
    if uid != 0 {
        bail!("this command modifies network interfaces and must be run with sudo");
    }
    Ok(())
}

/// Windows: the process token must be elevated. Membership of the
/// Administrators group is not enough — under UAC an administrator's normal
/// shell runs with a filtered token, and Hyper-V cmdlets and `netsh` both
/// fail from it.
#[cfg(all(feature = "cli", windows))]
fn require_root() -> Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no
    // closing, and `token` is a valid out-pointer that `OpenProcessToken`
    // fills on success.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        bail!(
            "cannot read this process's token to check for elevation: {}",
            std::io::Error::last_os_error()
        );
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned: u32 = 0;
    // SAFETY: `token` is the open handle from above; `elevation` is a
    // correctly-sized, writable buffer for the `TokenElevation` class, and
    // its size is passed alongside it.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    let query_error = (ok == 0).then(std::io::Error::last_os_error);
    // SAFETY: `token` came from `OpenProcessToken` and is closed exactly once.
    unsafe { CloseHandle(token) };
    if let Some(e) = query_error {
        bail!("cannot read this process's elevation state: {e}");
    }
    if elevation.TokenIsElevated == 0 {
        bail!(
            "this command modifies network interfaces and must be run from an \
             elevated (Run as administrator) shell"
        );
    }
    Ok(())
}

/// Any other platform has no backend, so `host_platform()` refuses before
/// anything here could run; this only has to compile.
#[cfg(all(feature = "cli", not(any(unix, windows))))]
fn require_root() -> Result<()> {
    bail!("vlanctl cannot check for elevation on this platform")
}

/// The backend `down` and `status` run through: the one the state file
/// says applied the active profile, so an apply through one Windows backend
/// is never torn down through the other. A state file with no backend
/// recorded predates the field and can only have come from the host's
/// default backend, which is the fallback — the preview flavour for a dry
/// run or `status`, which must keep working on a host with no backend.
#[cfg(feature = "cli")]
fn platform_for_state(
    state_path: &std::path::Path,
    preview: bool,
) -> Result<Box<dyn plan::Platform>> {
    let state = State::load(state_path)?;
    match state.backend.as_deref().and_then(plan::platform_named) {
        Some(platform) => Ok(platform),
        None if preview => Ok(plan::preview_platform()),
        None => plan::host_platform(),
    }
}

#[cfg(not(feature = "cli"))]
fn main() {
    eprintln!("vlanctl was built without the `cli` feature");
    std::process::exit(2);
}

/// What `--report` writes. One shape for every command: `created` is
/// empty for anything but a successful `apply`.
#[cfg(feature = "cli")]
#[derive(serde::Serialize)]
struct Report<'a> {
    command: &'a str,
    ok: bool,
    message: String,
    created: &'a [String],
}

/// Write the report `--report` asked for. A failure to write it is its own
/// error only when the command itself succeeded: a reader that finds no
/// file already treats that as "vlanctl did not get as far as running",
/// and a write failure must not hide the command's own error.
#[cfg(feature = "cli")]
fn write_report(path: &std::path::Path, command: &str, result: &Result<Vec<String>>) {
    let (ok, message, created): (bool, String, &[String]) = match result {
        Ok(created) => (true, String::new(), created),
        Err(e) => (false, format!("{e:#}"), &[]),
    };
    let report = Report {
        command,
        ok,
        message,
        created,
    };
    let write = serde_json::to_string_pretty(&report)
        .map_err(anyhow::Error::from)
        .and_then(|text| std::fs::write(path, text).map_err(anyhow::Error::from));
    if let Err(e) = write {
        eprintln!(
            "vlanctl: could not write --report {}: {e:#}",
            path.display()
        );
    }
}

#[cfg(feature = "cli")]
fn main() -> Result<()> {
    let cli = Cli::parse();
    let report_path = cli.report.clone();
    let command = cli.command.name();
    let result = run(cli);
    if let Some(path) = &report_path {
        write_report(path, command, &result);
    }
    result.map(|_| ())
}

/// Run the parsed command. Returns the interfaces a successful `apply`
/// created, and nothing for every other command, which is what the report
/// carries.
#[cfg(feature = "cli")]
fn run(cli: Cli) -> Result<Vec<String>> {
    let state_path = State::default_path();
    let mut created_interfaces = Vec::new();

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
            // Chosen for this profile: on Windows its shape decides between
            // the two backends there.
            let platform = plan::preview_platform_for(&p);
            let device = device::resolve_device(
                &*platform,
                &mut probe,
                device.as_deref().or(p.device.as_deref()),
            )?;
            // A preview renders through the reference platform, not
            // `host_platform_for()`: like `apply --dry-run`/`down --dry-run`
            // below, it touches no real system and must keep working on any
            // host, so it must not fail just because this build has no real
            // backend for the host OS.
            for line in commands::show_plan(&*platform, &p, &device) {
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
                    &*plan::preview_platform_for(&p),
                    &p,
                    device.as_deref(),
                    &state_path,
                    true,
                )?;
                print_planned_commands(&runner.recorded);
            } else {
                require_root()?;
                let platform = plan::host_platform_for(&p)?;
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
                created_interfaces = created;
            }
        }
        Command::Down { dry_run } => {
            if dry_run {
                // See the matching comment in `Command::Apply`: dry runs must
                // keep working on any host, so a state file naming no
                // backend falls back to `plan::preview_platform()`, never
                // the fallible `host_platform()`.
                let mut runner = PreviewRunner {
                    probe: SystemRunner,
                    recorded: Vec::new(),
                };
                let platform = platform_for_state(&state_path, true)?;
                commands::down(&mut runner, &*platform, &state_path, true)?;
                print_planned_commands(&runner.recorded);
            } else {
                require_root()?;
                // Through the backend that applied, not the one this host
                // would pick for a fresh profile.
                let platform = platform_for_state(&state_path, false)?;
                let mut runner = SystemRunner;
                commands::down(&mut runner, &*platform, &state_path, false)?;
                println!("torn down");
            }
        }
        Command::Status => {
            let mut runner = SystemRunner;
            let platform = platform_for_state(&state_path, true)?;
            print!(
                "{}",
                commands::status(&mut runner, &*platform, &state_path)?
            );
        }
    }
    Ok(created_interfaces)
}

#[cfg(all(test, feature = "cli"))]
mod tests {
    use super::*;
    use net::Cmd;
    use plan::Platform;

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
                    "00:00:5e:00:53:01",
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
            Cmd::new("arp", &["-s", "192.168.10.151", "00:00:5e:00:53:01"]),
            // Windows. `netsh` never passes (the backend issues no probe
            // through it), and a PowerShell command line passes only when
            // every statement is one of the backend's probe shapes.
            Cmd::new(
                "netsh",
                &[
                    "interface",
                    "ipv4",
                    "set",
                    "address",
                    "vEthernet (vlan11)",
                    "static",
                    "192.168.11.87",
                    "255.255.255.0",
                ],
            ),
            Cmd::new(
                "netsh",
                &[
                    "interface",
                    "ipv4",
                    "add",
                    "route",
                    "239.255.0.255/32",
                    "vEthernet (vlan11)",
                ],
            ),
            Cmd::new(
                "powershell.exe",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    "$ErrorActionPreference = 'Stop'; Remove-VMSwitch -Name 'vlanctl' -Force",
                ],
            ),
            // A probe shape with a mutation appended.
            Cmd::new(
                "powershell.exe",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    "$ErrorActionPreference = 'Stop'; Get-NetAdapter | ForEach-Object { $_.Name }; \
                     Remove-VMSwitch -Name 'vlanctl' -Force",
                ],
            ),
        ];
        // And every mutation the Windows backend itself renders.
        let tagged = config::Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: Some(1500),
            routes: vec![config::Route {
                destination: "192.168.11.151/32".to_string(),
                gateway: None,
                mac: Some("00:00:5e:00:53:01".to_string()),
            }],
        };
        let mut mutations = mutations.to_vec();
        mutations.extend(plan::bringup_commands_for(
            &plan::WindowsHyperV,
            &tagged,
            "Ethernet 2",
        ));
        mutations.extend(plan::WindowsHyperV.teardown_commands("vEthernet (vlan11)"));
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
        // The Windows probes are rendered by the backend rather than spelled
        // out here, so this test follows the backend's own shapes. Each is
        // issued against a recording runner and the command it recorded is
        // what the allowlist must recognise.
        let mut windows_probes = Vec::new();
        {
            let mut runner = net::RecordingRunner::default();
            let _ = plan::WindowsHyperV.list_devices(&mut runner);
            let _ = plan::WindowsHyperV.addresses_on(&mut runner, "vEthernet (vlan11)");
            let _ = plan::WindowsHyperV.is_wireless(&mut runner, "Ethernet 2");
            let _ = plan::WindowsHyperV.link_is_active(&mut runner, "Ethernet 2");
            windows_probes.extend(runner.commands);
        }
        assert_eq!(
            windows_probes.len(),
            4,
            "every Windows probe should issue one command"
        );
        let probes: Vec<Cmd> = probes.into_iter().chain(windows_probes).collect();
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
