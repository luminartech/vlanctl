use crate::config::Profile;
use crate::device::resolve_device;
use crate::net::{Cmd, CommandRunner};
use crate::plan::{Platform, bringup_commands_for};
use crate::state::State;
use anyhow::{Context, Result, bail};
use ipnet::IpNet;
use std::path::Path;

/// List live interface names from `ifconfig -l`.
///
/// `pub(crate)`: also the parser behind [`crate::plan::Platform::list_devices`].
pub(crate) fn live_interfaces<R: CommandRunner + ?Sized>(runner: &mut R) -> Result<Vec<String>> {
    let out = runner.run(&Cmd::new("ifconfig", &["-l"]))?;
    Ok(out.split_whitespace().map(|s| s.to_string()).collect())
}

/// IPv4 addresses currently configured on `device`, parsed from the `inet`
/// lines of `ifconfig <device>` (address plus netmask). Used to make
/// untagged-interface apply idempotent.
///
/// `pub(crate)`: also the parser behind [`crate::plan::Platform::addresses_on`].
pub(crate) fn device_inet_addresses<R: CommandRunner + ?Sized>(
    runner: &mut R,
    device: &str,
) -> Result<Vec<IpNet>> {
    let out = runner.run(&Cmd::new("ifconfig", &[device]))?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let mut toks = line.split_whitespace();
            if toks.next() != Some("inet") {
                return None;
            }
            let addr = toks.next()?;
            let prefix = match toks.next() {
                Some("netmask") => toks.next().and_then(prefix_from_hex_netmask).unwrap_or(32),
                _ => 32,
            };
            format!("{addr}/{prefix}").parse::<IpNet>().ok()
        })
        .collect())
}

/// Parse an `ifconfig` hex netmask (e.g. `0xffffff00`) into a prefix length.
/// Returns `None` if `hex` is malformed, OR if the mask's one-bits are not a
/// contiguous run from the most-significant bit (e.g. `0xff00ff00`) — such a
/// mask has no single prefix length, and counting bits alone (as a naive
/// `count_ones()` would) silently invents one. The caller's `unwrap_or(32)`
/// governs what happens for a `None` here, same as for a dotted-quad or
/// absent netmask.
fn prefix_from_hex_netmask(hex: &str) -> Option<u8> {
    let hex = hex.strip_prefix("0x")?;
    let mask = u32::from_str_radix(hex, 16).ok()?;
    let prefix = mask.count_ones() as u8;
    let contiguous = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    (mask == contiguous).then_some(prefix)
}

/// Bring up `profile`. Tears down any active profile first, then creates each
/// VLAN. On any failure, rolls back interfaces created during this run.
/// Returns the list of created interface names. When `dry_run` is true, the
/// state file is read (for an accurate preview) but never written.
pub fn apply<R: CommandRunner>(
    runner: &mut R,
    platform: &dyn Platform,
    profile: &Profile,
    state_path: &Path,
    dry_run: bool,
) -> Result<Vec<String>> {
    // Tear down whatever is currently active. The local `state` is rewritten
    // wholesale below, so there is no need to reload it after `down`.
    let mut state = State::load(state_path)?;
    if state.active_profile.is_some() {
        down(runner, platform, state_path, dry_run)?;
    }

    let device = resolve_device(runner, profile.device.as_deref())?;
    // The names this run will create, derived from the *resolved* device
    // through the platform's own naming, so the guard below cannot disagree
    // with what bring-up actually creates. Tagged entries only: an untagged
    // entry configures the parent device, which is never guarded.
    let interfaces: Vec<String> = profile
        .interfaces
        .iter()
        .filter(|i| i.vlan.is_some())
        .map(|i| platform.iface_name(i, &device))
        .collect();

    // Refuse to touch a vlan<id> sub-interface that already exists and is not
    // ours. The physical device is never guarded — untagged apply is additive.
    let existing = live_interfaces(runner)?;
    for iface in &interfaces {
        if existing.contains(iface) {
            bail!(
                "interface {iface} already exists and was not created by vlanctl; \
                 refusing to touch it (destroy it manually if it is stale)"
            );
        }
    }

    let mut created: Vec<String> = Vec::new();
    for interface in &profile.interfaces {
        match interface.vlan {
            // Untagged: configure the parent device, idempotently. If the alias
            // address is already present, a previous apply set it up (and its
            // routes) — skip. Never recorded, regardless of the platform:
            // `platform.reverts_parent_config()` names *whether* reverting
            // parent config is appropriate, but an untagged entry is an
            // address alias, and undoing one needs `-alias <addr>` — syntax
            // the generic `platform.teardown_commands(iface)` used for
            // tagged sub-interfaces has no way to express (on macOS it would
            // render as `ifconfig <device> destroy`, which destroys the real
            // NIC). Recording it here would also bypass the tagged-only
            // collision guard above and mislabel the parent device as a
            // managed interface in `status`. A platform that actually wants
            // to revert parent config (a Windows vSwitch binding, say) needs
            // its own recording/teardown path built deliberately, not this
            // one reused by accident.
            None => {
                let already_configured = device_inet_addresses(runner, &device)?
                    .iter()
                    .any(|net| net.addr() == interface.address.addr());
                if already_configured {
                    continue;
                }
                for cmd in bringup_commands_for(platform, interface, &device) {
                    if let Err(e) = runner.run(&cmd) {
                        rollback(runner, platform, &created);
                        return Err(e).with_context(|| {
                            format!("applying profile '{}'; rolled back", profile.name)
                        });
                    }
                }
            }
            // Tagged: create the vlan sub-interface, recording it as soon as the
            // `create` succeeds so rollback can destroy it if a later step fails.
            Some(_) => {
                let iface = platform.iface_name(interface, &device);
                for cmd in bringup_commands_for(platform, interface, &device) {
                    let is_create = platform.records_created_interface(&cmd);
                    match runner.run(&cmd) {
                        Ok(_) => {
                            if is_create {
                                created.push(iface.clone());
                            }
                        }
                        Err(e) => {
                            rollback(runner, platform, &created);
                            return Err(e).with_context(|| {
                                format!("applying profile '{}'; rolled back", profile.name)
                            });
                        }
                    }
                }
            }
        }
    }

    state.active_profile = Some(profile.name.clone());
    state.interfaces = created.clone();
    if !dry_run {
        state.save(state_path)?;
    }
    Ok(created)
}

/// Destroy created interfaces in reverse order, ignoring errors (best effort).
fn rollback<R: CommandRunner>(runner: &mut R, platform: &dyn Platform, created: &[String]) {
    for iface in created.iter().rev() {
        for cmd in platform.teardown_commands(iface) {
            let _ = runner.run(&cmd);
        }
    }
}

/// Tear down the active profile recorded in state and clear it. When `dry_run`
/// is true, the teardown commands are still produced but state is not cleared.
pub fn down<R: CommandRunner>(
    runner: &mut R,
    platform: &dyn Platform,
    state_path: &Path,
    dry_run: bool,
) -> Result<()> {
    let state = State::load(state_path)?;
    // A reboot drops the VLAN interfaces but leaves the state file intact, so
    // recorded interfaces may already be gone. Only tear down ones still live;
    // destroying a missing interface would error and is a no-op anyway.
    let live = live_interfaces(runner)?;
    for iface in state.interfaces.iter().rev() {
        if !live.contains(iface) {
            continue;
        }
        for cmd in platform.teardown_commands(iface) {
            runner.run(&cmd)?;
        }
    }
    if !dry_run {
        State::default().save(state_path)?;
    }
    Ok(())
}

/// Render the full bring-up plan for a profile as displayable command lines,
/// through the same `platform` that `apply` would use — so the preview never
/// disagrees with what actually runs.
pub fn show_plan(platform: &dyn Platform, profile: &Profile, device: &str) -> Vec<String> {
    profile
        .interfaces
        .iter()
        .flat_map(|interface| bringup_commands_for(platform, interface, device))
        .map(|cmd| cmd.display())
        .collect()
}

/// List profile names (file stems) found in `dir`.
pub fn list_profiles(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    if !dir.exists() {
        return Ok(names);
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            names.push(stem.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// A human-readable status report: active profile and whether each recorded
/// interface is still live.
pub fn status<R: CommandRunner>(runner: &mut R, state_path: &Path) -> Result<String> {
    let state = State::load(state_path)?;
    let live = live_interfaces(runner)?;
    let mut report = String::new();
    match &state.active_profile {
        None => report.push_str("No active profile.\n"),
        Some(name) => {
            report.push_str(&format!("Active profile: {name}\n"));
            for iface in &state.interfaces {
                let present = if live.contains(iface) {
                    "up"
                } else {
                    "MISSING"
                };
                report.push_str(&format!("  {iface}: {present}\n"));
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::RecordingRunner;
    use crate::plan::{Linux, MacOs};

    fn profile() -> Profile {
        // Pin `device` so these tests exercise apply/down orchestration without
        // depending on device auto-detection (covered in device.rs tests).
        let p: Profile = toml::from_str(
            "name=\"t\"\ndevice=\"en0\"\n\
             [[interface]]\nvlan=100\naddress=\"192.168.10.2/24\"\n\
             [[interface]]\nvlan=200\naddress=\"10.0.0.5/24\"\n",
        )
        .unwrap();
        p.validate().unwrap();
        p
    }

    fn untagged_profile() -> Profile {
        let p: Profile = toml::from_str(
            "name=\"halo\"\ndevice=\"en0\"\n\
             [[interface]]\naddress=\"192.168.1.100/24\"\n\
             [[interface.route]]\ndestination=\"192.168.10.151/32\"\n\
             [[interface]]\nvlan=12\naddress=\"192.168.10.1/24\"\n",
        )
        .unwrap();
        p.validate().unwrap();
        p
    }

    fn runner_with_device() -> RecordingRunner {
        let mut r = RecordingRunner::default();
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0".to_string());
        r
    }

    /// The same two VLANs as [`profile`], on a Linux-named parent. Under
    /// `Linux` these become `eth0.100` and `eth0.200`, so anything in `apply`
    /// that still assumes `vlan<id>` or `ifconfig` shapes shows up here.
    fn linux_profile() -> Profile {
        let p: Profile = toml::from_str(
            "name=\"t\"\ndevice=\"eth0\"\n\
             [[interface]]\nvlan=100\naddress=\"192.168.10.2/24\"\n\
             [[interface]]\nvlan=200\naddress=\"10.0.0.5/24\"\n",
        )
        .unwrap();
        p.validate().unwrap();
        p
    }

    /// `apply` still lists live interfaces through the `ifconfig -l` parser
    /// regardless of platform, so a Linux run needs that probe stubbed too.
    fn linux_runner(live: &str) -> RecordingRunner {
        let mut r = RecordingRunner::default();
        r.stdout.insert("ifconfig -l".to_string(), live.to_string());
        r
    }

    #[test]
    fn apply_with_linux_records_what_ip_link_add_created() {
        // Guards the recording call site in `apply`: `ip link add link eth0
        // name eth0.100 ...` has `link` where ifconfig has `create`, so an
        // ifconfig-shaped test there records nothing — rollback undoes
        // nothing, the state file stays empty, and `down` becomes a no-op.
        let state_path = std::env::temp_dir().join("vlanctl-linux-apply-ok.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = linux_runner("lo eth0");
        let created = apply(&mut r, &Linux, &linux_profile(), &state_path, false).unwrap();
        assert_eq!(created, vec!["eth0.100", "eth0.200"]);

        let state = State::load(&state_path).unwrap();
        assert_eq!(state.active_profile.as_deref(), Some("t"));
        assert_eq!(state.interfaces, vec!["eth0.100", "eth0.200"]);
        std::fs::remove_file(&state_path).unwrap();
    }

    #[test]
    fn apply_with_linux_rolls_back_with_ip_link_del() {
        let state_path = std::env::temp_dir().join("vlanctl-linux-apply-fail.json");
        let _ = std::fs::remove_file(&state_path);
        // Locate eth0.200's creation instead of hardcoding its index, so a
        // change to the Linux bring-up sequence cannot silently move the
        // simulated failure onto some other command.
        let mut probe = linux_runner("lo eth0");
        apply(&mut probe, &Linux, &linux_profile(), &state_path, true).unwrap();
        let fail_at = probe
            .commands
            .iter()
            .position(|c| {
                c.display()
                    .starts_with("ip link add link eth0 name eth0.200 ")
            })
            .expect("the plan creates eth0.200");

        let mut r = linux_runner("lo eth0");
        r.fail_at = Some(fail_at);
        let err = apply(&mut r, &Linux, &linux_profile(), &state_path, false).unwrap_err();
        assert!(err.to_string().contains("rolled back"), "{err}");

        // eth0.100 was fully created and must be deleted; eth0.200 never was.
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(
            rendered.contains(&"ip link del eth0.100".to_string()),
            "rollback must delete the sub-interface it created: {rendered:?}"
        );
        assert!(!rendered.contains(&"ip link del eth0.200".to_string()));
        assert_eq!(State::load(&state_path).unwrap(), State::default());
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn apply_with_linux_refuses_an_existing_sub_interface() {
        // Guards the collision-guard call site in `apply`: a guard that looks
        // for `vlan100` never matches a live `eth0.100`, so it never fires
        // and `apply` reconfigures an interface it did not create.
        let state_path = std::env::temp_dir().join("vlanctl-linux-apply-collision.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = linux_runner("lo eth0 eth0.100");
        let err = apply(&mut r, &Linux, &linux_profile(), &state_path, false).unwrap_err();
        assert!(
            err.to_string().contains("eth0.100 already exists"),
            "expected the guard to refuse eth0.100: {err}"
        );
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(
            !rendered.iter().any(|c| c.contains("link add")),
            "{rendered:?}"
        );
        assert_eq!(State::load(&state_path).unwrap(), State::default());
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn apply_with_linux_guards_the_auto_detected_device_not_the_profile_field() {
        // Pins that the collision guard is built from the *resolved* device.
        // With `device` left unset, `profile.device` is `None`, and a guard
        // that reads it directly derives `".100"` — a name that is never
        // live — so it silently never fires. Every other `apply` test pins
        // `device`, which is why that regression passed them all.
        let p: Profile = toml::from_str(
            "name=\"t\"\n\
             [[interface]]\nvlan=100\naddress=\"192.168.10.2/24\"\n",
        )
        .unwrap();
        p.validate().unwrap();
        assert!(p.device.is_none(), "this test exercises auto-detect");

        let state_path = std::env::temp_dir().join("vlanctl-linux-apply-autodetect-guard.json");
        let _ = std::fs::remove_file(&state_path);

        // `resolve_device` still runs the macOS probes on every platform:
        // hardware ports (en0 is a wired port, not Wi-Fi), the interface
        // listing, and a link-status query per wired candidate. `en0.100`
        // also matches the `en*` candidate filter, but its unstubbed status
        // reads as inactive, so en0 is the single active device and wins.
        let mut r = RecordingRunner::default();
        r.stdout.insert(
            "networksetup -listallhardwareports".to_string(),
            "Hardware Port: USB 10/100/1000 LAN\nDevice: en0\n".to_string(),
        );
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 en0.100".to_string());
        r.stdout
            .insert("ifconfig en0".to_string(), "\tstatus: active\n".to_string());

        let err = apply(&mut r, &Linux, &p, &state_path, false).unwrap_err();
        assert!(
            err.to_string().contains("en0.100 already exists"),
            "expected the guard to refuse the resolved device's sub-interface: {err}"
        );
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(
            rendered.contains(&"networksetup -listallhardwareports".to_string())
                && rendered.contains(&"ifconfig en0".to_string()),
            "auto-detect must actually have run: {rendered:?}"
        );
        assert!(
            !rendered.iter().any(|c| c.contains("link add")),
            "nothing may be created past a refused guard: {rendered:?}"
        );
        assert_eq!(State::load(&state_path).unwrap(), State::default());
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn hex_netmask_prefix_for_contiguous_masks() {
        assert_eq!(prefix_from_hex_netmask("0xffffff00"), Some(24));
        assert_eq!(prefix_from_hex_netmask("0xffff0000"), Some(16));
        assert_eq!(prefix_from_hex_netmask("0x00000000"), Some(0));
        assert_eq!(prefix_from_hex_netmask("0xffffffff"), Some(32));
    }

    #[test]
    fn hex_netmask_prefix_is_none_for_a_non_contiguous_mask() {
        // A non-contiguous mask has no single prefix length; counting set
        // bits alone would invent a wrong one (/16 here).
        assert_eq!(prefix_from_hex_netmask("0xff00ff00"), None);
    }

    #[test]
    fn hex_netmask_prefix_is_none_for_malformed_input() {
        assert_eq!(prefix_from_hex_netmask("255.255.255.0"), None);
        assert_eq!(prefix_from_hex_netmask(""), None);
    }

    #[test]
    fn apply_uses_the_platform_for_bringup() {
        let state_path = std::env::temp_dir().join("vlanctl-apply-uses-platform.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = runner_with_device();
        apply(&mut r, &MacOs, &profile(), &state_path, true).expect("dry-run apply succeeds");
        assert!(
            r.commands
                .iter()
                .any(|c| c.display().starts_with("ifconfig vlan")),
            "recorded: {:?}",
            r.commands
        );
    }

    #[test]
    fn apply_creates_all_vlans_and_records_state() {
        let state_path = std::env::temp_dir().join("vlanctl-apply-ok.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = runner_with_device();
        let created = apply(&mut r, &MacOs, &profile(), &state_path, false).unwrap();
        // Interface name = vlan<id>, so ids 100/200 -> vlan100/vlan200.
        assert_eq!(created, vec!["vlan100", "vlan200"]);

        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(rendered.contains(&"ifconfig vlan100 create".to_string()));
        assert!(rendered.contains(&"ifconfig vlan100 vlan 100 vlandev en0".to_string()));
        assert!(rendered.contains(&"ifconfig vlan200 create".to_string()));
        assert!(rendered.contains(&"ifconfig vlan200 vlan 200 vlandev en0".to_string()));

        let state = State::load(&state_path).unwrap();
        assert_eq!(state.active_profile.as_deref(), Some("t"));
        assert_eq!(state.interfaces, vec!["vlan100", "vlan200"]);
        std::fs::remove_file(&state_path).unwrap();
    }

    #[test]
    fn apply_rolls_back_on_failure() {
        let state_path = std::env::temp_dir().join("vlanctl-apply-fail.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = runner_with_device();
        // With `device` pinned, resolve_device issues no commands, so:
        //   [0]=ifconfig -l (live_interfaces),
        //   [1]=create vlan100, [2]=vlan vlan100, [3]=inet vlan100,
        //   [4]=create vlan200 (fails here).
        // vlan100 is fully configured; vlan200's create fails, so rollback must
        // destroy vlan100 only.
        r.fail_at = Some(4);
        let err = apply(&mut r, &MacOs, &profile(), &state_path, false).unwrap_err();
        assert!(err.to_string().contains("rolled back"));

        // vlan100 was created then destroyed during rollback; vlan200 never was.
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(rendered.contains(&"ifconfig vlan100 destroy".to_string()));
        assert!(!rendered.contains(&"ifconfig vlan200 destroy".to_string()));
        // No state file should be written on failure.
        assert_eq!(State::load(&state_path).unwrap(), State::default());
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn down_destroys_recorded_interfaces() {
        let state_path = std::env::temp_dir().join("vlanctl-down.json");
        let state = State {
            active_profile: Some("t".to_string()),
            interfaces: vec!["vlan0".to_string(), "vlan1".to_string()],
        };
        state.save(&state_path).unwrap();
        let mut r = RecordingRunner::default();
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 vlan0 vlan1".to_string());
        down(&mut r, &MacOs, &state_path, false).unwrap();
        // Drop the `ifconfig -l` probe; assert on the teardown commands only.
        let rendered: Vec<String> = r
            .commands
            .iter()
            .map(|c| c.display())
            .filter(|c| c != "ifconfig -l")
            .collect();
        assert_eq!(
            rendered,
            vec!["ifconfig vlan1 destroy", "ifconfig vlan0 destroy"]
        );
        assert_eq!(State::load(&state_path).unwrap(), State::default());
        std::fs::remove_file(&state_path).unwrap();
    }

    #[test]
    fn down_skips_interfaces_no_longer_live() {
        // Regression: after a reboot the kernel drops the VLAN interfaces, but
        // the state file still records them. `down` must not fail trying to
        // destroy interfaces that are already gone.
        let state_path = std::env::temp_dir().join("vlanctl-down-reboot.json");
        State {
            active_profile: Some("t".to_string()),
            interfaces: vec!["vlan10".to_string(), "vlan11".to_string()],
        }
        .save(&state_path)
        .unwrap();
        let mut r = RecordingRunner::default();
        // vlan10 survived (somehow), vlan11 is gone. Only vlan10 should be
        // destroyed; the missing vlan11 is silently skipped.
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 vlan10".to_string());
        down(&mut r, &MacOs, &state_path, false).unwrap();
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(rendered.contains(&"ifconfig vlan10 destroy".to_string()));
        assert!(!rendered.contains(&"ifconfig vlan11 destroy".to_string()));
        assert_eq!(State::load(&state_path).unwrap(), State::default());
        std::fs::remove_file(&state_path).unwrap();
    }

    #[test]
    fn show_plan_lists_commands() {
        let lines = show_plan(&MacOs, &profile(), "en0");
        assert_eq!(lines[0], "ifconfig vlan100 create");
        assert_eq!(lines[1], "ifconfig vlan100 vlan 100 vlandev en0");
    }

    #[test]
    fn apply_errors_if_interface_already_exists() {
        let state_path = std::env::temp_dir().join("vlanctl-apply-collision.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = RecordingRunner::default();
        // vlan100 (the interface for VLAN id 100) is already live.
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 vlan100".to_string());
        let err = apply(&mut r, &MacOs, &profile(), &state_path, false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        // Nothing was created and no state was written.
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(!rendered.iter().any(|c| c.contains("create")));
        assert_eq!(State::load(&state_path).unwrap(), State::default());
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn apply_untagged_configures_parent_and_records_only_vlan() {
        let state_path = std::env::temp_dir().join("vlanctl-apply-untagged.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = runner_with_device(); // "ifconfig -l" -> "lo0 en0"; "ifconfig en0" -> empty
        let created = apply(&mut r, &MacOs, &untagged_profile(), &state_path, false).unwrap();
        assert_eq!(created, vec!["vlan12"]); // only the tagged interface is recorded
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(
            rendered.contains(
                &"ifconfig en0 inet 192.168.1.100 netmask 255.255.255.0 alias".to_string()
            )
        );
        assert!(rendered.contains(&"route add -host 192.168.10.151 -interface en0".to_string()));
        assert!(rendered.contains(&"ifconfig vlan12 create".to_string()));
    }

    #[test]
    fn apply_untagged_skips_when_alias_already_present() {
        let state_path = std::env::temp_dir().join("vlanctl-apply-untagged-idem.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = runner_with_device();
        r.stdout.insert(
            "ifconfig en0".to_string(),
            "\tinet 192.168.1.100 netmask 0xffffff00 broadcast 192.168.1.255".to_string(),
        );
        apply(&mut r, &MacOs, &untagged_profile(), &state_path, false).unwrap();
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(!rendered.iter().any(|c| c.contains("alias")));
        assert!(
            !rendered
                .iter()
                .any(|c| c.contains("route add -host 192.168.10.151"))
        );
        assert!(rendered.contains(&"ifconfig vlan12 create".to_string())); // tagged still applied
    }

    #[test]
    fn list_profiles_finds_toml_stems() {
        let dir = std::env::temp_dir().join("vlanctl-profiles-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.toml"), "name=\"a\"").unwrap();
        std::fs::write(dir.join("b.toml"), "name=\"b\"").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let names = list_profiles(&dir).unwrap();
        assert_eq!(names, vec!["a", "b"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn mac_profile() -> Profile {
        let p: Profile = toml::from_str(
            "name=\"halo\"\ndevice=\"en0\"\n\
             [[interface]]\naddress=\"192.168.1.100/24\"\n\
             [[interface.route]]\ndestination=\"192.168.10.151/32\"\nmac=\"3a:42:f7:79:32:2e\"\n",
        )
        .unwrap();
        p.validate().unwrap();
        p
    }

    #[test]
    fn apply_mac_route_emits_static_arp_and_records_nothing() {
        let state_path = std::env::temp_dir().join("vlanctl-apply-mac.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = runner_with_device();
        let created = apply(&mut r, &MacOs, &mac_profile(), &state_path, false).unwrap();
        assert!(created.is_empty()); // untagged-only profile records nothing
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        // The route is added, then a single static ARP entry — no `arp -d`,
        // which on macOS would delete the freshly-added host route.
        assert!(rendered.contains(&"route add -host 192.168.10.151 -interface en0".to_string()));
        assert!(rendered.contains(&"arp -s 192.168.10.151 3a:42:f7:79:32:2e".to_string()));
        assert!(!rendered.iter().any(|c| c.starts_with("arp -d")));
    }

    #[test]
    fn status_flags_missing_interface() {
        let state_path = std::env::temp_dir().join("vlanctl-status.json");
        State {
            active_profile: Some("t".to_string()),
            interfaces: vec!["vlan0".to_string(), "vlan9".to_string()],
        }
        .save(&state_path)
        .unwrap();
        let mut r = RecordingRunner::default();
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 vlan0".to_string());
        let report = status(&mut r, &state_path).unwrap();
        assert!(report.contains("vlan0: up"));
        assert!(report.contains("vlan9: MISSING"));
        std::fs::remove_file(&state_path).unwrap();
    }
}
