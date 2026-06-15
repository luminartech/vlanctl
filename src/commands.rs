use crate::config::Profile;
use crate::device::resolve_device;
use crate::net::{Cmd, CommandRunner};
use crate::plan::{allocate_interfaces, bringup_commands, teardown_commands};
use crate::state::State;
use anyhow::{Context, Result};
use std::path::Path;

/// List live interface names from `ifconfig -l`.
fn live_interfaces<R: CommandRunner>(runner: &mut R) -> Result<Vec<String>> {
    let out = runner.run(&Cmd::new("ifconfig", &["-l"]))?;
    Ok(out.split_whitespace().map(|s| s.to_string()).collect())
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
    let existing = live_interfaces(runner)?;
    let interfaces = allocate_interfaces(profile, &existing);

    let mut created: Vec<String> = Vec::new();
    for (iface, vlan) in interfaces.iter().zip(&profile.vlans) {
        for cmd in bringup_commands(iface, &device, vlan) {
            // Record the interface as created as soon as the `create` succeeds,
            // so rollback can destroy it even if a later command fails.
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
    for iface in state.interfaces.iter().rev() {
        for cmd in teardown_commands(iface) {
            runner.run(&cmd)?;
        }
    }
    if !dry_run {
        State::default().save(state_path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::RecordingRunner;

    fn profile() -> Profile {
        let p: Profile = toml::from_str(
            "name=\"t\"\n\
             [[vlan]]\nid=100\naddress=\"192.168.10.2/24\"\n\
             [[vlan]]\nid=200\naddress=\"10.0.0.5/24\"\n",
        )
        .unwrap();
        p.validate().unwrap();
        p
    }

    fn runner_with_device() -> RecordingRunner {
        let mut r = RecordingRunner::default();
        r.stdout.insert("ifconfig -l".to_string(), "lo0 en0".to_string());
        r
    }

    #[test]
    fn apply_creates_all_vlans_and_records_state() {
        let state_path = std::env::temp_dir().join("vlanctl-apply-ok.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = runner_with_device();
        let created = apply(&mut r, &profile(), &state_path, false).unwrap();
        assert_eq!(created, vec!["vlan0", "vlan1"]);

        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(rendered.contains(&"ifconfig vlan0 create vlan 100 vlandev en0".to_string()));
        assert!(rendered.contains(&"ifconfig vlan1 create vlan 200 vlandev en0".to_string()));

        let state = State::load(&state_path).unwrap();
        assert_eq!(state.active_profile.as_deref(), Some("t"));
        assert_eq!(state.interfaces, vec!["vlan0", "vlan1"]);
        std::fs::remove_file(&state_path).unwrap();
    }

    #[test]
    fn apply_rolls_back_on_failure() {
        let state_path = std::env::temp_dir().join("vlanctl-apply-fail.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = runner_with_device();
        // Two `ifconfig -l` calls run first (resolve_device, then
        // live_interfaces), so the command indices are:
        //   [0]=ifconfig -l, [1]=ifconfig -l,
        //   [2]=create vlan0, [3]=inet vlan0,
        //   [4]=create vlan1 (fails here).
        // vlan0 is fully configured; vlan1's create fails, so rollback must
        // destroy vlan0 only.
        r.fail_at = Some(4);
        let err = apply(&mut r, &profile(), &state_path, false).unwrap_err();
        assert!(err.to_string().contains("rolled back"));

        // vlan0 was created then destroyed during rollback; vlan1 never was.
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(rendered.contains(&"ifconfig vlan0 destroy".to_string()));
        assert!(!rendered.contains(&"ifconfig vlan1 destroy".to_string()));
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
        down(&mut r, &state_path, false).unwrap();
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert_eq!(
            rendered,
            vec!["ifconfig vlan1 destroy", "ifconfig vlan0 destroy"]
        );
        assert_eq!(State::load(&state_path).unwrap(), State::default());
        std::fs::remove_file(&state_path).unwrap();
    }
}
