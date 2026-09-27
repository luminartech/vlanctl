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
/// **macOS's implementation only.** Its sole caller is
/// [`crate::plan::Platform::list_devices`] on `MacOs`; `apply`/`down`/`status`
/// go through the trait, so a Linux host reads `ip -json link show` instead.
/// Do not call it directly — `-l` is a BSD flag and net-tools rejects it.
pub(crate) fn live_interfaces<R: CommandRunner + ?Sized>(runner: &mut R) -> Result<Vec<String>> {
    let out = runner.run(&Cmd::new("ifconfig", &["-l"]))?;
    Ok(out.split_whitespace().map(|s| s.to_string()).collect())
}

/// IPv4 addresses currently configured on `device`, parsed from the `inet`
/// lines of `ifconfig <device>` (address plus netmask). Makes untagged-interface
/// apply idempotent.
///
/// **macOS's implementation only**, reached through
/// [`crate::plan::Platform::addresses_on`] on `MacOs`; `apply` calls the trait.
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
    device_override: Option<&str>,
    state_path: &Path,
    dry_run: bool,
) -> Result<Vec<String>> {
    // A backend that cannot take this profile says so before anything is
    // touched — including the active profile below, which a refusal must
    // leave standing.
    platform.validate_profile(profile)?;

    // Tear down whatever is currently active. The local `state` is rewritten
    // wholesale below, so there is no need to reload it after `down`.
    let mut state = State::load(state_path)?;
    if state.active_profile.is_some() {
        down(runner, platform, state_path, dry_run)?;
    }

    // The CLI override wins over the profile's own field: a committed
    // profile cannot know the host's parent device name.
    let device = resolve_device(
        platform,
        runner,
        device_override.or(profile.device.as_deref()),
    )?;
    // The names this run will create, derived from the *resolved* device
    // through the platform's own naming, so the guard below cannot disagree
    // with what bring-up actually creates. Entries that name an interface
    // of their own, that is: on macOS and Linux an untagged entry configures
    // the parent device itself, which is never guarded, and `iface_name`
    // returns the parent for it. On Windows every entry is a virtual adapter
    // vlanctl creates, untagged included, and each is guarded.
    let interfaces: Vec<String> = profile
        .interfaces
        .iter()
        .map(|i| platform.iface_name(i, &device))
        .filter(|name| *name != device)
        .collect();

    // Refuse to touch an interface that already exists and is not ours. The
    // physical device is never guarded — an untagged apply onto it is
    // additive. Through the platform, so a Linux host reads `ip -json link
    // show` rather than the BSD-only `ifconfig -l` (seam gap 2).
    let existing = platform.list_devices(runner)?;
    for iface in &interfaces {
        if existing.contains(iface) {
            bail!(
                "interface {iface} already exists and was not created by vlanctl; \
                 refusing to touch it (destroy it manually if it is stale)"
            );
        }
    }

    // A name is not the only thing that can collide. On Linux the kernel
    // keys VLANs on (parent, vlan id), so an existing `vlan10` on this
    // parent makes `eth0.10` impossible to create even though the names
    // differ — `ip link add` would fail with "8021q: VLAN device already
    // exists" after the apply had already started. Refuse here instead,
    // where the message can say which device holds the id. A platform with
    // no such constraint returns an empty list and this is a no-op.
    let taken = platform.existing_vlans(runner, &device)?;
    for interface in profile.interfaces.iter().filter(|i| i.vlan.is_some()) {
        let Some(id) = interface.vlan else { continue };
        if let Some((_, holder)) = taken.iter().find(|(taken_id, _)| *taken_id == id) {
            bail!(
                "vlan id {id} is already in use on {device} by interface {holder}, \
                 which vlanctl did not create; refusing to touch it (the kernel \
                 allows one VLAN per id per parent, whatever it is named)"
            );
        }
    }

    let mut created: Vec<String> = Vec::new();
    for interface in &profile.interfaces {
        let iface = platform.iface_name(interface, &device);
        // Untagged: idempotent. If the address is already on the interface
        // the entry names, a previous apply set it up (and its routes) —
        // skip. On macOS and Linux that interface is the parent device
        // itself; on Windows it is the untagged virtual adapter, which does
        // not exist before the first apply and so reports no addresses.
        if interface.vlan.is_none() {
            let already_configured = platform
                .addresses_on(runner, &iface)?
                .iter()
                .any(|net| net.addr() == interface.address.addr());
            if already_configured {
                continue;
            }
        }
        // Record the interface as soon as its creation command succeeds, so
        // rollback can destroy it if a later step fails. What counts as a
        // creation is the platform's call (`records_created_interface`), and
        // that is what decides whether an untagged entry is recorded: an
        // address alias on the parent (macOS `ifconfig ... alias`, Linux
        // `ip addr add`) creates nothing, so the parent is never recorded,
        // never guarded above, and never handed to `teardown_commands` —
        // which on macOS would render `ifconfig <device> destroy` and
        // destroy the real NIC. A Windows untagged entry is a virtual
        // adapter the backend creates and can remove, so there it is
        // recorded and torn down like a tagged one.
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

    state.active_profile = Some(profile.name.clone());
    state.interfaces = created.clone();
    // Which backend made these, so `down` can run through the same one
    // rather than whatever the host would pick for a fresh profile — on
    // Windows those can differ, and the other backend's teardown would not
    // undo this one's work.
    state.backend = Some(platform.name().to_string());
    if !dry_run {
        // Saved before the check below, deliberately: if verification fails
        // the host is partly configured, and the caller needs a state file
        // for `down` to have something to clean up.
        state.save(state_path)?;
        // The bring-up commands exited 0; now ask the host, the way `down`
        // does. A command that succeeds and an interface that exists are
        // different claims.
        let present =
            surviving_interfaces(runner, platform, &created).map_err(|e| BringUpUnverified {
                profile: profile.name.clone(),
                reason: format!("{e:#}"),
            })?;
        let missing: Vec<String> = created
            .iter()
            .filter(|c| !present.contains(c))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(BringUpIncomplete {
                profile: profile.name.clone(),
                missing,
            }
            .into());
        }
    }
    Ok(created)
}

/// [`apply`]'s bring-up commands all succeeded, but some interfaces it
/// created were not on the host when it looked afterward.
///
/// Returned inside the `anyhow::Error`, so a caller that needs the names —
/// to show the operator exactly what is missing — can
/// `err.downcast_ref::<BringUpIncomplete>()` instead of parsing the message.
/// The state file has already been saved, so [`down`] can clean up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BringUpIncomplete {
    pub profile: String,
    pub missing: Vec<String>,
}

impl std::fmt::Display for BringUpIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let one = self.missing.len() == 1;
        write!(
            f,
            "applied profile '{}', but {} not on this host afterward: {}. \
             The commands reported success, so something removed or \
             refused {} after creation",
            self.profile,
            if one {
                "one interface is"
            } else {
                "some interfaces are"
            },
            self.missing.join(", "),
            if one { "it" } else { "them" },
        )
    }
}

impl std::error::Error for BringUpIncomplete {}

/// [`down`]'s teardown commands all succeeded, but some recorded interfaces
/// were still on the host afterward — typically because a connection
/// manager re-creates them as fast as they are removed.
///
/// Returned inside the `anyhow::Error`; `err.downcast_ref::<TeardownIncomplete>()`
/// recovers the names. The state file is deliberately left in place, so a
/// retry still knows what it is responsible for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeardownIncomplete {
    /// The profile the state file recorded, if it named one.
    pub profile: Option<String>,
    pub survivors: Vec<String>,
}

impl std::fmt::Display for TeardownIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let one = self.survivors.len() == 1;
        write!(
            f,
            "tore down '{}', but {} still present on this host: {}. \
             Something outside vlanctl is holding {} up — a connection \
             manager with a profile for {} will re-create {} as fast as it \
             is removed",
            self.profile.as_deref().unwrap_or("(unnamed)"),
            if one {
                "one interface is"
            } else {
                "some interfaces are"
            },
            self.survivors.join(", "),
            if one { "it" } else { "them" },
            if one { "that name" } else { "those names" },
            if one { "it" } else { "them" },
        )
    }
}

impl std::error::Error for TeardownIncomplete {}

/// [`apply`]'s bring-up commands all succeeded, but the host could not be
/// listed afterward, so whether the interfaces exist is unknown.
///
/// Distinct from a failed apply, which a caller must treat differently:
/// the state file has been saved, so [`down`] can take down whatever was
/// created. Recover it with `err.downcast_ref::<BringUpUnverified>()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BringUpUnverified {
    pub profile: String,
    /// Why the listing failed.
    pub reason: String,
}

impl std::fmt::Display for BringUpUnverified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "applied profile '{}', but could not list this host's interfaces \
             afterward to confirm it ({})",
            self.profile, self.reason
        )
    }
}

impl std::error::Error for BringUpUnverified {}

/// [`down`]'s teardown commands all succeeded, but the host could not be
/// listed afterward, so whether anything survived is unknown.
///
/// The state file is left in place, as for [`TeardownIncomplete`]. Recover
/// it with `err.downcast_ref::<TeardownUnverified>()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeardownUnverified {
    /// The profile the state file recorded, if it named one.
    pub profile: Option<String>,
    /// Why the listing failed.
    pub reason: String,
}

impl std::fmt::Display for TeardownUnverified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tore down '{}', but could not list this host's interfaces \
             afterward to confirm it ({})",
            self.profile.as_deref().unwrap_or("(unnamed)"),
            self.reason
        )
    }
}

impl std::error::Error for TeardownUnverified {}

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
    let live = platform.list_devices(runner)?;
    for iface in state.interfaces.iter().rev() {
        if !live.contains(iface) {
            continue;
        }
        for cmd in platform.teardown_commands(iface) {
            runner.run(&cmd)?;
        }
    }
    if dry_run {
        return Ok(());
    }
    // The commands exited 0; now ask the host. Something outside vlanctl can
    // hold an interface up — a NetworkManager connection profile for the same
    // name re-creates it as fast as the teardown removes it — and a caller
    // that trusts the exit code tells its operator the host is clean when it
    // is not.
    let survivors = surviving_interfaces(runner, platform, &state.interfaces).map_err(|e| {
        TeardownUnverified {
            profile: state.active_profile.clone(),
            reason: format!("{e:#}"),
        }
    })?;
    if !survivors.is_empty() {
        // The record is deliberately NOT cleared. A retry needs to know what
        // it is still responsible for, and the caller has just been told the
        // teardown did not finish.
        return Err(TeardownIncomplete {
            profile: state.active_profile.clone(),
            survivors,
        }
        .into());
    }
    State::default().save(state_path)?;
    Ok(())
}

/// Which of `interfaces` are still present on the host.
///
/// Its own function because both [`down`] and [`apply`] need the same
/// question asked of the host rather than of the exit codes — one to confirm
/// interfaces went away, the other to confirm they arrived.
fn surviving_interfaces<R: CommandRunner>(
    runner: &mut R,
    platform: &dyn Platform,
    interfaces: &[String],
) -> Result<Vec<String>> {
    let live = platform.list_devices(runner)?;
    Ok(interfaces
        .iter()
        .filter(|name| live.contains(name))
        .cloned()
        .collect())
}

/// Render the full bring-up plan for a profile as displayable command lines,
/// through whatever `platform` the caller passes.
///
/// It renders faithfully for the platform it is GIVEN — but the caller is the
/// one that decides, and on a Linux host the two callers disagree today:
/// previews come from `preview_platform()` (fixed at `MacOs`) while `apply`
/// resolves `host_platform()` (`Linux`). So `apply --dry-run` prints
/// `ifconfig vlan10 create …` where the real apply would run
/// `ip link add … type vlan`. This doc previously claimed the preview "never
/// disagrees with what actually runs", which stopped being true the moment a
/// second backend existed.
///
/// The repoint is deferred because the two functions return different shapes
/// (`&'static dyn Platform` vs `Result<Box<dyn Platform>>`). Stated here
/// rather than by citing the platform-seam doc, which is no longer tracked in
/// this repository and so will not exist in a fresh clone.
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
pub fn status<R: CommandRunner>(
    runner: &mut R,
    platform: &dyn Platform,
    state_path: &Path,
) -> Result<String> {
    let state = State::load(state_path)?;
    let live = platform.list_devices(runner)?;
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
    use crate::plan::{Linux, MacOs, WindowsDriverVlan, WindowsHyperV};

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
        // The pre-apply host, consumed by the collision guard.
        r.stdout_queue.insert(
            "ifconfig -l".to_string(),
            ["lo0 en0"].into_iter().map(String::from).collect(),
        );
        // Every later answer is the host *after* bring-up, carrying the
        // interfaces `profile()` creates. `apply` verifies its own work now,
        // so a fixture that never gains them describes a failed apply.
        r.stdout.insert(
            "ifconfig -l".to_string(),
            "lo0 en0 vlan100 vlan200".to_string(),
        );
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

    /// `apply` lists live interfaces through `Platform::list_devices`, so a
    /// Linux run needs `ip -json link show` stubbed — not the BSD
    /// `ifconfig -l` this helper used to seed.
    fn linux_runner(live: &str) -> RecordingRunner {
        let mut r = RecordingRunner::default();
        let json = format!(
            "[{}]",
            live.split_whitespace()
                .map(|n| format!(r#"{{"ifname":"{n}"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        // The pre-apply host, consumed by the collision guard...
        r.stdout_queue.insert(
            "ip -json link show".to_string(),
            [json.clone()].into_iter().collect(),
        );
        // ...and every later answer is the host after bring-up, carrying what
        // `linux_profile()` creates. `apply` verifies its own work now, so a
        // fixture that never gains them describes a failed apply.
        let after = format!(
            "[{}]",
            live.split_whitespace()
                .chain(["eth0.100", "eth0.200"])
                .map(|n| format!(r#"{{"ifname":"{n}"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        r.stdout.insert("ip -json link show".to_string(), after);
        // The vlan-id guard's probe. A real host always emits a JSON array,
        // so an empty default here would be an unrealistic fixture — and the
        // parse is deliberately strict, treating unreadable output as a
        // failure to ask. Tests that need an id collision override this.
        r.stdout
            .insert("ip -d -json link show".to_string(), "[]".to_string());
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
        let created = apply(&mut r, &Linux, &linux_profile(), None, &state_path, false).unwrap();
        assert_eq!(created, vec!["eth0.100", "eth0.200"]);

        let state = State::load(&state_path).unwrap();
        assert_eq!(state.active_profile.as_deref(), Some("t"));
        assert_eq!(state.interfaces, vec!["eth0.100", "eth0.200"]);
        std::fs::remove_file(&state_path).unwrap();
    }

    /// The bring-up commands exiting 0 is not the same as the interfaces
    /// existing. `apply` asks the host afterward, the way `down` does.
    #[test]
    fn apply_reports_interfaces_that_never_appeared() {
        let state_path = std::env::temp_dir().join("vlanctl-apply-missing.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = linux_runner("lo eth0");
        // Override the post-apply answer: the commands succeeded but the
        // host never gained the interfaces.
        r.stdout.insert(
            "ip -json link show".to_string(),
            r#"[{"ifname":"lo"},{"ifname":"eth0"}]"#.to_string(),
        );

        let err = apply(&mut r, &Linux, &linux_profile(), None, &state_path, false)
            .expect_err("interfaces that never appeared are not a successful apply");
        let text = err.to_string();
        assert!(
            text.contains("eth0.100"),
            "the missing interface must be named: {text}"
        );
        // And recoverable as data, so a caller can act on the names without
        // parsing the message.
        let typed = err
            .downcast_ref::<BringUpIncomplete>()
            .expect("a failed verification is a BringUpIncomplete");
        assert_eq!(typed.profile, "t");
        assert_eq!(typed.missing, vec!["eth0.100", "eth0.200"]);

        // State is still written: the caller needs `down` to be able to clean
        // up whatever *did* get created.
        assert_eq!(
            State::load(&state_path).unwrap().active_profile.as_deref(),
            Some("t")
        );
        let _ = std::fs::remove_file(&state_path);
    }

    /// A post-apply listing that fails is not a failed apply: the commands
    /// ran and the state was saved. It has to be distinguishable from one, or
    /// a caller tells its operator nothing was touched.
    #[test]
    fn apply_whose_host_cannot_be_listed_afterward_is_unverified() {
        let state_path = std::env::temp_dir().join("vlanctl-apply-unverified.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = linux_runner("lo eth0");
        r.stdout
            .insert("ip -json link show".to_string(), "not json".to_string());

        let err = apply(&mut r, &Linux, &linux_profile(), None, &state_path, false)
            .expect_err("an unconfirmed apply is not a success");
        let typed = err
            .downcast_ref::<BringUpUnverified>()
            .unwrap_or_else(|| panic!("expected BringUpUnverified; got {err:#}"));
        assert_eq!(typed.profile, "t");
        assert_eq!(
            State::load(&state_path).unwrap().active_profile.as_deref(),
            Some("t"),
            "the state is saved so `down` can clean up"
        );
        let _ = std::fs::remove_file(&state_path);
    }

    /// The teardown counterpart: commands ran, the host could not be asked.
    #[test]
    fn down_whose_host_cannot_be_listed_afterward_is_unverified() {
        let state_path = std::env::temp_dir().join("vlanctl-down-unverified.json");
        State {
            active_profile: Some("t".to_string()),
            interfaces: vec!["eth0.100".to_string()],
        }
        .save(&state_path)
        .unwrap();
        let mut r = RecordingRunner::default();
        // Live before teardown; unreadable after it.
        r.stdout_queue.insert(
            "ip -json link show".to_string(),
            [r#"[{"ifname":"eth0"},{"ifname":"eth0.100"}]"#.to_string()]
                .into_iter()
                .collect(),
        );
        r.stdout
            .insert("ip -json link show".to_string(), "not json".to_string());

        let err = down(&mut r, &Linux, &state_path, false)
            .expect_err("an unconfirmed teardown is not a clean one");
        assert_eq!(
            err.downcast_ref::<TeardownUnverified>()
                .map(|t| t.profile.as_deref()),
            Some(Some("t")),
            "expected TeardownUnverified; got {err:#}"
        );
        assert!(
            r.commands
                .iter()
                .any(|c| c.display().starts_with("ip link del")),
            "the teardown itself must have run"
        );
        assert_eq!(
            State::load(&state_path).unwrap().interfaces,
            vec!["eth0.100".to_string()],
            "state survives an unconfirmed teardown"
        );
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
        apply(
            &mut probe,
            &Linux,
            &linux_profile(),
            None,
            &state_path,
            true,
        )
        .unwrap();
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
        let err = apply(&mut r, &Linux, &linux_profile(), None, &state_path, false).unwrap_err();
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

    /// [`untagged_profile`] on a Windows-named parent: an untagged entry and
    /// VLAN 12, both of which become Hyper-V virtual adapters there.
    fn windows_profile() -> Profile {
        let p: Profile = toml::from_str(
            "name=\"halo\"\ndevice=\"Ethernet 2\"\n\
             [[interface]]\naddress=\"192.168.1.100/24\"\n\
             [[interface.route]]\ndestination=\"192.168.10.151/32\"\n\
             [[interface]]\nvlan=12\naddress=\"192.168.10.1/24\"\n",
        )
        .unwrap();
        p.validate().unwrap();
        p
    }

    /// A runner whose Windows `list_devices` probe answers `live`, one
    /// adapter name per line. The probe's command line is taken from the
    /// backend rather than spelled out, so a change to how it is rendered
    /// cannot silently turn this stub into an unmapped command.
    fn windows_runner(live: &[&str]) -> RecordingRunner {
        let mut probe = RecordingRunner::default();
        WindowsHyperV.list_devices(&mut probe).unwrap();
        let mut r = RecordingRunner::default();
        r.stdout
            .insert(probe.commands[0].display(), live.join("\r\n"));
        r
    }

    #[test]
    fn apply_with_windows_records_the_untagged_adapter_and_down_removes_it() {
        // On macOS and Linux an untagged entry is an address on the parent
        // and is never recorded. On Windows it is a virtual adapter the
        // backend creates, so it must be recorded — or `down` would leave
        // it, and with it the switch binding that took the parent over.
        let state_path = std::env::temp_dir().join("vlanctl-windows-apply-ok.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = windows_runner(&["Ethernet 2", "Wi-Fi"]);
        let created = apply(
            &mut r,
            &WindowsHyperV,
            &windows_profile(),
            None,
            &state_path,
            false,
        )
        .unwrap();
        assert_eq!(created, vec!["vEthernet (untagged)", "vEthernet (vlan12)"]);
        let state = State::load(&state_path).unwrap();
        assert_eq!(state.interfaces, created);

        // Both adapters are live now; `down` must remove both, in reverse.
        let mut r = windows_runner(&["Ethernet 2", "vEthernet (untagged)", "vEthernet (vlan12)"]);
        down(&mut r, &WindowsHyperV, &state_path, false).unwrap();
        let teardowns: Vec<&str> = r
            .commands
            .iter()
            .filter(|c| WindowsHyperV.teardown_commands("x")[0].program == c.program)
            .filter_map(|c| c.args.last())
            .filter(|s| s.contains("Remove-VMNetworkAdapter"))
            .map(String::as_str)
            .collect();
        assert_eq!(teardowns.len(), 2, "{teardowns:?}");
        assert!(teardowns[0].contains("-Name 'vlan12'"), "{}", teardowns[0]);
        assert!(
            teardowns[1].contains("-Name 'untagged'"),
            "{}",
            teardowns[1]
        );
        assert_eq!(State::load(&state_path).unwrap(), State::default());
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn apply_with_windows_guards_the_untagged_adapter_too() {
        // The collision guard covers every interface the platform names,
        // not only tagged ones: a leftover `vEthernet (untagged)` from
        // something else is refused, not silently doubled.
        let state_path = std::env::temp_dir().join("vlanctl-windows-apply-guard.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = windows_runner(&["Ethernet 2", "vEthernet (untagged)"]);
        let err = apply(
            &mut r,
            &WindowsHyperV,
            &windows_profile(),
            None,
            &state_path,
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("vEthernet (untagged) already exists"),
            "{err}"
        );
        assert!(
            !r.commands
                .iter()
                .any(|c| WindowsHyperV.records_created_interface(c)),
            "nothing may be created after a refusal"
        );
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn apply_records_the_backend_that_applied() {
        let state_path = std::env::temp_dir().join("vlanctl-backend-recorded.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = linux_runner("lo eth0");
        apply(&mut r, &Linux, &linux_profile(), None, &state_path, false).unwrap();
        assert_eq!(
            State::load(&state_path).unwrap().backend.as_deref(),
            Some("linux")
        );
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn apply_with_the_driver_vlan_backend_records_the_parent_and_down_puts_it_back() {
        // The parent adapter itself is the interface here: the keyword
        // changes what it is, and `down` must reset the keyword and the
        // address, so the parent is recorded although nothing new appeared.
        let p: Profile = toml::from_str(
            "name=\"live\"\ndevice=\"Ethernet 2\"\n\
             [[interface]]\nvlan=11\naddress=\"192.168.11.87/24\"\n",
        )
        .unwrap();
        p.validate().unwrap();
        let state_path = std::env::temp_dir().join("vlanctl-driver-vlan-apply.json");
        let _ = std::fs::remove_file(&state_path);
        let mut r = windows_runner(&["Ethernet 2", "Wi-Fi"]);
        let created = apply(&mut r, &WindowsDriverVlan, &p, None, &state_path, false).unwrap();
        assert_eq!(created, vec!["Ethernet 2"]);
        let state = State::load(&state_path).unwrap();
        assert_eq!(state.backend.as_deref(), Some("windows-driver-vlan"));

        let mut r = windows_runner(&["Ethernet 2", "Wi-Fi"]);
        down(&mut r, &WindowsDriverVlan, &state_path, false).unwrap();
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(
            rendered.contains(&"netsh interface ipv4 set address Ethernet 2 dhcp".to_string()),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|c| c.contains("-RegistryKeyword 'VlanID' -RegistryValue '0'")),
            "{rendered:?}"
        );
        assert_eq!(State::load(&state_path).unwrap(), State::default());
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn a_backend_refusal_leaves_the_active_profile_standing() {
        // Two entries on the driver VLAN backend: refused by
        // `validate_profile`, and the refusal must come before the teardown
        // of what is active, or "refused" would also mean "torn down".
        let state_path = std::env::temp_dir().join("vlanctl-refusal-keeps-active.json");
        let active = State {
            active_profile: Some("live".to_string()),
            interfaces: vec!["Ethernet 2".to_string()],
            backend: Some("windows-driver-vlan".to_string()),
        };
        active.save(&state_path).unwrap();
        let mut r = windows_runner(&["Ethernet 2"]);
        let err = apply(
            &mut r,
            &WindowsDriverVlan,
            &windows_profile(),
            None,
            &state_path,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("one VLAN per adapter"), "{err}");
        assert!(r.commands.is_empty(), "nothing may run: {:?}", r.commands);
        assert_eq!(State::load(&state_path).unwrap(), active);
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
        let err = apply(&mut r, &Linux, &linux_profile(), None, &state_path, false).unwrap_err();
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
    fn apply_refuses_a_vlan_id_already_taken_under_another_name() {
        // The kernel's uniqueness constraint is (parent, vlan id), NOT the
        // name: an existing `vlan10` on eth0 makes `eth0.10` impossible to
        // create, so `ip link add` fails with "VLAN device already exists"
        // part-way through an apply. The name guard cannot see it —
        // "vlan10" != "eth0.10" — so it must be refused on the id.
        let state_path = std::env::temp_dir().join("vlanctl-vlan-id-collision.json");
        let _ = std::fs::remove_file(&state_path);

        // `linux_profile()` wants vlan 100; a foreign `vlan100` already
        // holds that id on eth0 under a name the guard cannot match.
        let mut r = linux_runner("lo eth0 vlan100");
        r.stdout.insert(
            "ip -d -json link show".to_string(),
            r#"[{"ifname":"vlan100","link":"eth0",
                 "linkinfo":{"info_kind":"vlan","info_data":{"id":100}}}]"#
                .to_string(),
        );
        let err = apply(
            &mut r,
            &Linux,
            &linux_profile(),
            Some("eth0"),
            &state_path,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("vlan id 100") && msg.contains("vlan100") && msg.contains("eth0"),
            "the refusal must name the id, the device holding it, and the parent: {msg}"
        );
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(
            !rendered.iter().any(|c| c.contains("link add")),
            "it must refuse BEFORE creating anything: {rendered:?}"
        );
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn the_cli_device_override_beats_the_profile_field() {
        // A committed profile cannot know the host's device name, so the
        // runtime override must win. Profile pins "en7" (a macOS name from
        // one bench); the override names this host's real parent.
        let mut p = linux_profile();
        p.device = Some("en7".to_string());
        let state_path = std::env::temp_dir().join("vlanctl-device-override.json");
        let _ = std::fs::remove_file(&state_path);

        let mut r = linux_runner("lo eth0");
        let created = apply(&mut r, &Linux, &p, Some("eth0"), &state_path, true).unwrap();
        assert!(
            created.iter().all(|c| !c.contains("en7")),
            "the profile's en7 must not appear: {created:?}"
        );
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(
            rendered
                .iter()
                .any(|c| c.contains("dev eth0") || c.contains("link eth0")),
            "expected the override device to be used: {rendered:?}"
        );
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

        // `resolve_device` now runs the PLATFORM's probes, so a Linux run
        // reads `ip -json link show` + `/sys`, not `ifconfig`/`networksetup`.
        // eth0.100 is listed but is not a candidate parent (it contains a
        // `.`), so eth0 is resolved — and the guard must then refuse the
        // eth0.100 that is already live.
        let mut r = RecordingRunner::default();
        r.stdout.insert(
            "ip -json link show".to_string(),
            r#"[{"ifname":"lo"},{"ifname":"eth0"},{"ifname":"eth0.100"}]"#.to_string(),
        );
        r.stdout.insert(
            "ls /sys/class/net/eth0".to_string(),
            "carrier operstate".to_string(),
        );
        r.stdout.insert(
            "cat /sys/class/net/eth0/carrier".to_string(),
            "1\n".to_string(),
        );

        let err = apply(&mut r, &Linux, &p, None, &state_path, false).unwrap_err();
        assert!(
            err.to_string().contains("eth0.100 already exists"),
            "expected the guard to refuse the resolved device's sub-interface: {err}"
        );
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(
            rendered.contains(&"ip -json link show".to_string())
                && rendered.contains(&"cat /sys/class/net/eth0/carrier".to_string()),
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
        apply(&mut r, &MacOs, &profile(), None, &state_path, true).expect("dry-run apply succeeds");
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
        let created = apply(&mut r, &MacOs, &profile(), None, &state_path, false).unwrap();
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
        let err = apply(&mut r, &MacOs, &profile(), None, &state_path, false).unwrap_err();
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
            backend: None,
        };
        state.save(&state_path).unwrap();
        let mut r = RecordingRunner::default();
        // Live before teardown, gone after it — `down` now asks twice, and a
        // constant answer would make a successful teardown indistinguishable
        // from one that did nothing.
        r.stdout_queue.insert(
            "ifconfig -l".to_string(),
            ["lo0 en0 vlan0 vlan1", "lo0 en0"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
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
            backend: None,
        }
        .save(&state_path)
        .unwrap();
        let mut r = RecordingRunner::default();
        // vlan10 survived (somehow), vlan11 is gone. Only vlan10 should be
        // destroyed; the missing vlan11 is silently skipped. The second
        // answer is the post-teardown host, with vlan10 now gone too.
        r.stdout_queue.insert(
            "ifconfig -l".to_string(),
            ["lo0 en0 vlan10", "lo0 en0"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        down(&mut r, &MacOs, &state_path, false).unwrap();
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(rendered.contains(&"ifconfig vlan10 destroy".to_string()));
        assert!(!rendered.contains(&"ifconfig vlan11 destroy".to_string()));
        assert_eq!(State::load(&state_path).unwrap(), State::default());
        std::fs::remove_file(&state_path).unwrap();
    }

    /// `down` exiting 0 is not the same as the host being clean. Something
    /// outside vlanctl can hold an interface up — a NetworkManager
    /// connection profile for the same name re-creates it as fast as
    /// `ip link del` removes it, which was measured on a bench where the
    /// ifindex moved while the interface never disappeared.
    ///
    /// Reporting success there is the worst answer available: the caller
    /// tells its operator the host is clean and everyone stops looking.
    #[test]
    fn down_reports_interfaces_that_survive_teardown() {
        let state_path = std::env::temp_dir().join("vlanctl-down-survivor.json");
        State {
            active_profile: Some("t".to_string()),
            interfaces: vec!["vlan10".to_string()],
        }
        .save(&state_path)
        .unwrap();
        let mut r = RecordingRunner::default();
        // Live before teardown, and still live after it.
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 vlan10".to_string());

        let err = down(&mut r, &MacOs, &state_path, false)
            .expect_err("a surviving interface is not a clean teardown");
        let text = err.to_string();
        assert!(
            text.contains("vlan10"),
            "the survivor must be named: {text}"
        );
        assert_eq!(
            err.downcast_ref::<TeardownIncomplete>(),
            Some(&TeardownIncomplete {
                profile: Some("t".to_string()),
                survivors: vec!["vlan10".to_string()],
            }),
            "a failed teardown is a TeardownIncomplete carrying the survivors"
        );

        // The record is kept, not cleared: a retry needs to know what it is
        // still responsible for, and the caller was just told the host is
        // not clean.
        assert_eq!(
            State::load(&state_path).unwrap().interfaces,
            vec!["vlan10".to_string()],
            "state must survive a teardown that did not finish"
        );
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
        let err = apply(&mut r, &MacOs, &profile(), None, &state_path, false).unwrap_err();
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
        let mut r = runner_with_device();
        // This profile creates vlan12, not the vlan100/200 the shared
        // helper scripts; `apply` verifies against the host afterward.
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 vlan12".to_string()); // "ifconfig -l" -> "lo0 en0"; "ifconfig en0" -> empty
        let created = apply(
            &mut r,
            &MacOs,
            &untagged_profile(),
            None,
            &state_path,
            false,
        )
        .unwrap();
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
        // This profile creates vlan12, not the vlan100/200 the shared
        // helper scripts; `apply` verifies against the host afterward.
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 vlan12".to_string());
        r.stdout.insert(
            "ifconfig en0".to_string(),
            "\tinet 192.168.1.100 netmask 0xffffff00 broadcast 192.168.1.255".to_string(),
        );
        apply(
            &mut r,
            &MacOs,
            &untagged_profile(),
            None,
            &state_path,
            false,
        )
        .unwrap();
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
             [[interface.route]]\ndestination=\"192.168.10.151/32\"\nmac=\"00:00:5e:00:53:01\"\n",
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
        let created = apply(&mut r, &MacOs, &mac_profile(), None, &state_path, false).unwrap();
        assert!(created.is_empty()); // untagged-only profile records nothing
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        // The route is added, then a single static ARP entry — no `arp -d`,
        // which on macOS would delete the freshly-added host route.
        assert!(rendered.contains(&"route add -host 192.168.10.151 -interface en0".to_string()));
        assert!(rendered.contains(&"arp -s 192.168.10.151 00:00:5e:00:53:01".to_string()));
        assert!(!rendered.iter().any(|c| c.starts_with("arp -d")));
    }

    #[test]
    fn status_flags_missing_interface() {
        let state_path = std::env::temp_dir().join("vlanctl-status.json");
        State {
            active_profile: Some("t".to_string()),
            interfaces: vec!["vlan0".to_string(), "vlan9".to_string()],
            backend: None,
        }
        .save(&state_path)
        .unwrap();
        let mut r = RecordingRunner::default();
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 vlan0".to_string());
        let report = status(&mut r, &MacOs, &state_path).unwrap();
        assert!(report.contains("vlan0: up"));
        assert!(report.contains("vlan9: MISSING"));
        std::fs::remove_file(&state_path).unwrap();
    }
}
