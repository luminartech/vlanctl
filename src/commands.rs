use crate::config::Profile;
use crate::device::resolve_device;
use crate::net::{Cmd, CommandRunner};
use crate::plan::{bringup_commands, iface_name, interface_names, teardown_commands};
use crate::state::State;
use anyhow::{Context, Result, bail};
use std::path::Path;

/// List live interface names from `ifconfig -l`.
fn live_interfaces<R: CommandRunner>(runner: &mut R) -> Result<Vec<String>> {
    let out = runner.run(&Cmd::new("ifconfig", &["-l"]))?;
    Ok(out.split_whitespace().map(|s| s.to_string()).collect())
}

/// IPv4 addresses currently configured on `device` (parsed from `ifconfig`),
/// used to make untagged-interface apply idempotent.
fn device_inet_addresses<R: CommandRunner>(runner: &mut R, device: &str) -> Result<Vec<String>> {
    let out = runner.run(&Cmd::new("ifconfig", &[device]))?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let mut toks = line.split_whitespace();
            match toks.next() {
                Some("inet") => toks.next().map(|a| a.to_string()),
                _ => None,
            }
        })
        .collect())
}

/// Bring up `profile`. Tears down any active profile first, then creates each
/// VLAN. On any failure, rolls back interfaces created during this run.
/// Returns the list of created interface names. When `dry_run` is true, the
/// state file is read (for an accurate preview) but never written.
pub fn apply<R: CommandRunner>(
    runner: &mut R,
    profile: &Profile,
    state_path: &Path,
    dry_run: bool,
) -> Result<Vec<String>> {
    // Tear down whatever is currently active. The local `state` is rewritten
    // wholesale below, so there is no need to reload it after `down`.
    let mut state = State::load(state_path)?;
    if state.active_profile.is_some() {
        down(runner, state_path, dry_run)?;
    }

    let device = resolve_device(runner, profile.device.as_deref())?;
    let interfaces = interface_names(profile); // tagged names, for the guard + state

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
            // routes) — skip. Untagged config is never recorded or torn down.
            None => {
                let addr = interface.address.addr().to_string();
                if device_inet_addresses(runner, &device)?.contains(&addr) {
                    continue;
                }
                for cmd in bringup_commands(interface, &device) {
                    if let Err(e) = runner.run(&cmd) {
                        rollback(runner, &created);
                        return Err(e).with_context(|| {
                            format!("applying profile '{}'; rolled back", profile.name)
                        });
                    }
                }
            }
            // Tagged: create the vlan sub-interface, recording it as soon as the
            // `create` succeeds so rollback can destroy it if a later step fails.
            Some(_) => {
                let iface = iface_name(interface, &device);
                for cmd in bringup_commands(interface, &device) {
                    let is_create = cmd.args.get(1).map(|a| a == "create").unwrap_or(false);
                    match runner.run(&cmd) {
                        Ok(_) => {
                            if is_create {
                                created.push(iface.clone());
                            }
                        }
                        Err(e) => {
                            rollback(runner, &created);
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
fn rollback<R: CommandRunner>(runner: &mut R, created: &[String]) {
    for iface in created.iter().rev() {
        for cmd in teardown_commands(iface) {
            let _ = runner.run(&cmd);
        }
    }
}

/// Tear down the active profile recorded in state and clear it. When `dry_run`
/// is true, the teardown commands are still produced but state is not cleared.
pub fn down<R: CommandRunner>(runner: &mut R, state_path: &Path, dry_run: bool) -> Result<()> {
    let state = State::load(state_path)?;
    // A reboot drops the VLAN interfaces but leaves the state file intact, so
    // recorded interfaces may already be gone. Only tear down ones still live;
    // destroying a missing interface would error and is a no-op anyway.
    let live = live_interfaces(runner)?;
    for iface in state.interfaces.iter().rev() {
        if !live.contains(iface) {
            continue;
        }
        for cmd in teardown_commands(iface) {
            runner.run(&cmd)?;
        }
    }
    if !dry_run {
        State::default().save(state_path)?;
    }
    Ok(())
}

/// Render the full bring-up plan for a profile as displayable command lines.
pub fn show_plan(profile: &Profile, device: &str) -> Vec<String> {
    profile
        .interfaces
        .iter()
        .flat_map(|interface| bringup_commands(interface, device))
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

    #[test]
    fn apply_creates_all_vlans_and_records_state() {
        let state_path = std::env::temp_dir().join("vlanctl-apply-ok.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = runner_with_device();
        let created = apply(&mut r, &profile(), &state_path, false).unwrap();
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
        let err = apply(&mut r, &profile(), &state_path, false).unwrap_err();
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
        down(&mut r, &state_path, false).unwrap();
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
        down(&mut r, &state_path, false).unwrap();
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(rendered.contains(&"ifconfig vlan10 destroy".to_string()));
        assert!(!rendered.contains(&"ifconfig vlan11 destroy".to_string()));
        assert_eq!(State::load(&state_path).unwrap(), State::default());
        std::fs::remove_file(&state_path).unwrap();
    }

    #[test]
    fn show_plan_lists_commands() {
        let lines = show_plan(&profile(), "en0");
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
        let err = apply(&mut r, &profile(), &state_path, false).unwrap_err();
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
        let created = apply(&mut r, &untagged_profile(), &state_path, false).unwrap();
        assert_eq!(created, vec!["vlan12"]); // only the tagged interface is recorded
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(rendered.contains(&"ifconfig en0 inet 192.168.1.100 netmask 255.255.255.0 alias".to_string()));
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
        apply(&mut r, &untagged_profile(), &state_path, false).unwrap();
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(!rendered.iter().any(|c| c.contains("alias")));
        assert!(!rendered.iter().any(|c| c.contains("route add -host 192.168.10.151")));
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
        let created = apply(&mut r, &mac_profile(), &state_path, false).unwrap();
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
