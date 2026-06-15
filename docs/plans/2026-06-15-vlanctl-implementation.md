# vlanctl Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Rust CLI (`vlanctl`) that applies and tears down named VLAN profiles on macOS from version-controlled TOML files.

**Architecture:** Declarative TOML profiles describe one or more VLANs (id, CIDR address, routes). A `CommandRunner` trait abstracts `ifconfig`/`route` execution so the apply/down/rollback logic is fully unit-testable and powers `--dry-run`. Apply is imperative with state tracking: validate → tear down any active profile → create interfaces → assign addresses → add routes, recording created interfaces to a JSON state file. Failures mid-apply roll back that run's changes.

**Tech Stack:** Rust (edition 2024, rustc 1.96), clap 4.6 (derive), serde 1.0, toml 1.1, anyhow 1.0, ipnet 2.12.

---

## File Structure

- `Cargo.toml` — dependencies and binary metadata.
- `src/main.rs` — entry point: parse CLI, dispatch, top-level error reporting, root check.
- `src/cli.rs` — clap command/argument definitions.
- `src/config.rs` — `Profile`/`Vlan`/`Route` types, TOML loading, validation.
- `src/net.rs` — `CommandRunner` trait, `SystemRunner` (real), `RecordingRunner` (mock/dry-run).
- `src/plan.rs` — build ordered command sequences for apply/down from a `Profile`.
- `src/state.rs` — `State` type, load/save to the JSON state file.
- `src/device.rs` — auto-detect the active Ethernet device.
- `src/commands.rs` — orchestration of each subcommand (list/show/validate/apply/down/status).
- `profiles/example.toml` — a checked-in sample profile.
- Tests live inline as `#[cfg(test)]` modules in each source file plus `tests/` fixtures where noted.

---

## Task 1: Dependencies and module skeleton

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/main.rs`
- Create: `src/cli.rs`, `src/config.rs`, `src/net.rs`, `src/plan.rs`, `src/state.rs`, `src/device.rs`, `src/commands.rs`

- [ ] **Step 1: Add dependencies**

Run from `vlanctl/`:

```bash
cargo add clap@4.6 --features derive
cargo add serde@1.0 --features derive
cargo add toml@1.1
cargo add anyhow@1.0
cargo add ipnet@2.12 --features serde
```

- [ ] **Step 2: Create empty module files**

Create each file with a single placeholder line so the crate compiles:

`src/cli.rs`, `src/config.rs`, `src/net.rs`, `src/plan.rs`, `src/state.rs`, `src/device.rs`, `src/commands.rs` — each containing:

```rust
// module implemented in later tasks
```

- [ ] **Step 3: Declare modules in main.rs**

Replace `src/main.rs` with:

```rust
mod cli;
mod commands;
mod config;
mod device;
mod net;
mod plan;
mod state;

fn main() {
    println!("vlanctl");
}
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo build`
Expected: compiles successfully (warnings about unused modules are fine).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore: add dependencies and module skeleton"
```

---

## Task 2: Config types and TOML parsing

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing test**

Add to `src/config.rs`:

```rust
use anyhow::{bail, Context, Result};
use ipnet::IpNet;
use serde::Deserialize;
use std::net::IpAddr;
use std::path::Path;

#[derive(Debug, Deserialize, PartialEq)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(rename = "vlan")]
    pub vlans: Vec<Vlan>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Vlan {
    pub id: u16,
    pub address: IpNet,
    #[serde(default)]
    pub mtu: Option<u32>,
    #[serde(default, rename = "route")]
    pub routes: Vec<Route>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Route {
    pub destination: String,
    pub gateway: IpAddr,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_vlan_profile() {
        let toml = r#"
name = "iris_bench"
description = "bench"

[[vlan]]
id = 100
address = "192.168.10.2/24"

  [[vlan.route]]
  destination = "192.168.20.0/24"
  gateway = "192.168.10.1"

[[vlan]]
id = 200
address = "10.0.0.5/24"
"#;
        let p: Profile = toml::from_str(toml).unwrap();
        assert_eq!(p.name, "iris_bench");
        assert_eq!(p.vlans.len(), 2);
        assert_eq!(p.vlans[0].id, 100);
        assert_eq!(p.vlans[0].routes.len(), 1);
        assert_eq!(p.vlans[1].routes.len(), 0);
    }
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test config::tests::parses_multi_vlan_profile`
Expected: PASS (types are the implementation here).

- [ ] **Step 3: Add the loader and validation function**

Append to `src/config.rs` (above the `#[cfg(test)]` module):

```rust
impl Profile {
    /// Load and validate a profile from a TOML file.
    pub fn load(path: &Path) -> Result<Profile> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading profile {}", path.display()))?;
        let profile: Profile = toml::from_str(&text)
            .with_context(|| format!("parsing profile {}", path.display()))?;
        profile.validate()?;
        Ok(profile)
    }

    /// Semantic validation beyond what the type system enforces.
    pub fn validate(&self) -> Result<()> {
        if self.vlans.is_empty() {
            bail!("profile '{}' has no [[vlan]] entries", self.name);
        }
        let mut seen = std::collections::HashSet::new();
        for vlan in &self.vlans {
            if !(1..=4094).contains(&vlan.id) {
                bail!("vlan id {} out of range (1..=4094)", vlan.id);
            }
            if !seen.insert(vlan.id) {
                bail!("duplicate vlan id {} in profile '{}'", vlan.id, self.name);
            }
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Write failing validation tests**

Add inside the `tests` module:

```rust
    fn profile_with(vlans: &str) -> Result<Profile> {
        let toml = format!("name = \"t\"\n{vlans}");
        let p: Profile = toml::from_str(&toml)?;
        p.validate()?;
        Ok(p)
    }

    #[test]
    fn rejects_duplicate_vlan_ids() {
        let err = profile_with(
            "[[vlan]]\nid = 100\naddress = \"1.1.1.1/24\"\n\
             [[vlan]]\nid = 100\naddress = \"2.2.2.2/24\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate vlan id 100"));
    }

    #[test]
    fn rejects_out_of_range_id() {
        let err = profile_with("[[vlan]]\nid = 5000\naddress = \"1.1.1.1/24\"\n")
            .unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn rejects_empty_vlan_list() {
        let err = profile_with("").unwrap_err();
        assert!(err.to_string().contains("no [[vlan]] entries"));
    }
```

Note: VLAN id `5000` exceeds `u16`? No — `u16` max is 65535, so `5000` parses and is caught by `validate`. Good.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test config::`
Expected: all config tests PASS.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: profile types, TOML loading, and validation"
```

---

## Task 3: CommandRunner abstraction

**Files:**
- Modify: `src/net.rs`

- [ ] **Step 1: Write the failing test**

Replace `src/net.rs` with:

```rust
use anyhow::{bail, Result};
use std::process::Command;

/// One external command to execute (program + args), e.g. `ifconfig vlan0 create`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
}

impl Cmd {
    pub fn new(program: &str, args: &[&str]) -> Cmd {
        Cmd {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Render as a shell-like string for display (show / dry-run).
    pub fn display(&self) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.args.iter().cloned());
        parts.join(" ")
    }
}

/// Abstraction over running system commands so logic is testable.
pub trait CommandRunner {
    /// Run a command, returning its stdout on success.
    fn run(&mut self, cmd: &Cmd) -> Result<String>;
}

/// Records commands instead of running them. Powers --dry-run and tests.
/// `fail_at` makes `run` return an error the Nth time (0-based) to test rollback.
#[derive(Default)]
pub struct RecordingRunner {
    pub commands: Vec<Cmd>,
    pub fail_at: Option<usize>,
    pub stdout: std::collections::HashMap<String, String>,
}

impl CommandRunner for RecordingRunner {
    fn run(&mut self, cmd: &Cmd) -> Result<String> {
        let index = self.commands.len();
        self.commands.push(cmd.clone());
        if Some(index) == self.fail_at {
            bail!("simulated failure running `{}`", cmd.display());
        }
        Ok(self.stdout.get(&cmd.display()).cloned().unwrap_or_default())
    }
}

/// Runs commands for real via std::process::Command.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&mut self, cmd: &Cmd) -> Result<String> {
        let output = Command::new(&cmd.program).args(&cmd.args).output()?;
        if !output.status.success() {
            bail!(
                "command `{}` failed: {}",
                cmd.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_runner_captures_commands() {
        let mut r = RecordingRunner::default();
        r.run(&Cmd::new("ifconfig", &["vlan0", "create"])).unwrap();
        assert_eq!(r.commands.len(), 1);
        assert_eq!(r.commands[0].display(), "ifconfig vlan0 create");
    }

    #[test]
    fn recording_runner_fails_at_index() {
        let mut r = RecordingRunner {
            fail_at: Some(1),
            ..Default::default()
        };
        assert!(r.run(&Cmd::new("a", &[])).is_ok());
        assert!(r.run(&Cmd::new("b", &[])).is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test net::`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: CommandRunner trait with recording and system runners"
```

---

## Task 4: State file

**Files:**
- Modify: `src/state.rs`

- [ ] **Step 1: Write the failing test**

Replace `src/state.rs` with:

```rust
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const STATE_PATH: &str = "/usr/local/var/vlanctl/state.json";

/// Persistent record of what vlanctl has brought up.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    /// Name of the currently active profile, if any.
    pub active_profile: Option<String>,
    /// Interface names (e.g. "vlan0") this tool created for the active profile.
    pub interfaces: Vec<String>,
}

impl State {
    pub fn default_path() -> PathBuf {
        PathBuf::from(STATE_PATH)
    }

    /// Load state from `path`; a missing file yields the default (empty) state.
    pub fn load(path: &Path) -> Result<State> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Save state to `path`, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_default_state() {
        let path = std::env::temp_dir().join("vlanctl-nonexistent-xyz.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(State::load(&path).unwrap(), State::default());
    }

    #[test]
    fn round_trips_through_disk() {
        let path = std::env::temp_dir().join("vlanctl-test-state.json");
        let state = State {
            active_profile: Some("iris_bench".to_string()),
            interfaces: vec!["vlan0".to_string(), "vlan1".to_string()],
        };
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), state);
        std::fs::remove_file(&path).unwrap();
    }
}
```

- [ ] **Step 2: Add serde_json dependency**

Run: `cargo add serde_json@1.0`

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test state::`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat: JSON state file load/save"
```

---

## Task 5: Command planning (apply/down sequences)

**Files:**
- Modify: `src/plan.rs`

- [ ] **Step 1: Write the failing test**

Replace `src/plan.rs` with:

```rust
use crate::config::{Profile, Vlan};
use crate::net::Cmd;

/// One VLAN's bring-up: the interface name chosen plus its ordered commands.
pub struct VlanBringup {
    pub interface: String,
    pub commands: Vec<Cmd>,
}

/// Build the ordered commands to bring up a single VLAN on `device`,
/// using the interface name `iface` (e.g. "vlan0").
pub fn bringup_commands(iface: &str, device: &str, vlan: &Vlan) -> Vec<Cmd> {
    let id = vlan.id.to_string();
    let addr = vlan.address.addr().to_string();
    let netmask = ipv4_netmask(vlan.address.prefix_len());

    let mut cmds = vec![
        Cmd::new("ifconfig", &[iface, "create", "vlan", &id, "vlandev", device]),
        Cmd::new("ifconfig", &[iface, "inet", &addr, "netmask", &netmask]),
    ];
    if let Some(mtu) = vlan.mtu {
        cmds.push(Cmd::new("ifconfig", &[iface, "mtu", &mtu.to_string()]));
    }
    for route in &vlan.routes {
        cmds.push(Cmd::new(
            "route",
            &["add", &route.destination, &route.gateway.to_string()],
        ));
    }
    cmds
}

/// Commands to tear down a single interface. Destroying the vlan interface
/// also drops its addresses and routes.
pub fn teardown_commands(iface: &str) -> Vec<Cmd> {
    vec![Cmd::new("ifconfig", &[iface, "destroy"])]
}

/// Render a dotted-quad netmask from an IPv4 prefix length.
fn ipv4_netmask(prefix: u8) -> String {
    let bits: u32 = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix as u32)
    };
    format!(
        "{}.{}.{}.{}",
        (bits >> 24) & 0xff,
        (bits >> 16) & 0xff,
        (bits >> 8) & 0xff,
        bits & 0xff
    )
}

/// Allocate interface names for a profile's VLANs, starting at the first free
/// vlan unit number not already present in `existing` (live interface names).
pub fn allocate_interfaces(profile: &Profile, existing: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    let mut unit = 0u32;
    for _ in &profile.vlans {
        loop {
            let candidate = format!("vlan{unit}");
            if !existing.contains(&candidate) && !names.contains(&candidate) {
                names.push(candidate);
                unit += 1;
                break;
            }
            unit += 1;
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Route, Vlan};
    use std::net::IpAddr;

    fn vlan(id: u16, cidr: &str) -> Vlan {
        Vlan {
            id,
            address: cidr.parse().unwrap(),
            mtu: None,
            routes: vec![],
        }
    }

    #[test]
    fn netmask_from_prefix() {
        assert_eq!(ipv4_netmask(24), "255.255.255.0");
        assert_eq!(ipv4_netmask(16), "255.255.0.0");
        assert_eq!(ipv4_netmask(8), "255.0.0.0");
    }

    #[test]
    fn bringup_creates_assigns_and_routes() {
        let mut v = vlan(100, "192.168.10.2/24");
        v.mtu = Some(1500);
        v.routes.push(Route {
            destination: "192.168.20.0/24".to_string(),
            gateway: "192.168.10.1".parse::<IpAddr>().unwrap(),
        });
        let cmds = bringup_commands("vlan0", "en10", &v);
        let rendered: Vec<String> = cmds.iter().map(|c| c.display()).collect();
        assert_eq!(
            rendered,
            vec![
                "ifconfig vlan0 create vlan 100 vlandev en10",
                "ifconfig vlan0 inet 192.168.10.2 netmask 255.255.255.0",
                "ifconfig vlan0 mtu 1500",
                "route add 192.168.20.0/24 192.168.10.1",
            ]
        );
    }

    #[test]
    fn teardown_destroys_interface() {
        assert_eq!(teardown_commands("vlan3")[0].display(), "ifconfig vlan3 destroy");
    }

    #[test]
    fn allocate_skips_existing_units() {
        let mut p: Profile = toml::from_str(
            "name=\"t\"\n[[vlan]]\nid=1\naddress=\"1.1.1.1/24\"\n[[vlan]]\nid=2\naddress=\"2.2.2.2/24\"\n",
        )
        .unwrap();
        p.validate().unwrap();
        let names = allocate_interfaces(&p, &["vlan0".to_string()]);
        assert_eq!(names, vec!["vlan1", "vlan2"]);
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test plan::`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: command planning for vlan bring-up and teardown"
```

---

## Task 6: Device auto-detection

**Files:**
- Modify: `src/device.rs`

- [ ] **Step 1: Write the failing test**

Replace `src/device.rs` with:

```rust
use crate::net::{Cmd, CommandRunner};
use anyhow::{bail, Result};

/// Resolve the physical Ethernet device to attach VLANs to.
/// If `override_device` is set, use it; otherwise pick the first active
/// Ethernet interface reported by `ifconfig`.
pub fn resolve_device<R: CommandRunner>(
    runner: &mut R,
    override_device: Option<&str>,
) -> Result<String> {
    if let Some(dev) = override_device {
        return Ok(dev.to_string());
    }
    let output = runner.run(&Cmd::new("ifconfig", &["-l"]))?;
    // `ifconfig -l` prints a space-separated list of interface names.
    let candidate = output
        .split_whitespace()
        .find(|name| name.starts_with("en"));
    match candidate {
        Some(name) => Ok(name.to_string()),
        None => bail!("could not auto-detect an Ethernet (enX) device; set `device` in the profile"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::RecordingRunner;

    #[test]
    fn override_wins() {
        let mut r = RecordingRunner::default();
        let dev = resolve_device(&mut r, Some("en42")).unwrap();
        assert_eq!(dev, "en42");
        assert!(r.commands.is_empty(), "override should not query ifconfig");
    }

    #[test]
    fn picks_first_en_interface() {
        let mut r = RecordingRunner::default();
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 en10 bridge0".to_string());
        let dev = resolve_device(&mut r, None).unwrap();
        assert_eq!(dev, "en0");
    }

    #[test]
    fn errors_when_no_en_interface() {
        let mut r = RecordingRunner::default();
        r.stdout.insert("ifconfig -l".to_string(), "lo0 bridge0".to_string());
        assert!(resolve_device(&mut r, None).is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test device::`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: Ethernet device auto-detection"
```

---

## Task 7: Apply and down orchestration with rollback

**Files:**
- Modify: `src/commands.rs`

- [ ] **Step 1: Write the failing test**

Replace `src/commands.rs` with:

```rust
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
    // Tear down whatever is currently active.
    let mut state = State::load(state_path)?;
    if state.active_profile.is_some() {
        down(runner, state_path, dry_run)?;
        state = State::load(state_path)?;
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
        // Commands: [0]=ifconfig -l, [1]=create vlan0, [2]=inet vlan0,
        // [3]=create vlan1. Fail at index 3 (second create).
        r.fail_at = Some(3);
        let err = apply(&mut r, &profile(), &state_path, false).unwrap_err();
        assert!(err.to_string().contains("rolled back"));

        // vlan0 was created then destroyed during rollback.
        let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
        assert!(rendered.contains(&"ifconfig vlan0 destroy".to_string()));
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
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test commands::`
Expected: PASS (3 tests).

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: apply/down orchestration with rollback and state tracking"
```

---

## Task 8: Read-only operations (list/show/validate/status)

**Files:**
- Modify: `src/commands.rs`

- [ ] **Step 1: Write the failing test**

Add these functions to `src/commands.rs` (above the `#[cfg(test)]` module):

```rust
/// Render the full bring-up plan for a profile as displayable command lines.
pub fn show_plan(profile: &Profile, device: &str) -> Vec<String> {
    let interfaces = allocate_interfaces(profile, &[]);
    let mut lines = Vec::new();
    for (iface, vlan) in interfaces.iter().zip(&profile.vlans) {
        for cmd in bringup_commands(iface, device, vlan) {
            lines.push(cmd.display());
        }
    }
    lines
}

/// List profile names (file stems) found in `dir`.
pub fn list_profiles(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    if !dir.exists() {
        return Ok(names);
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.to_string());
            }
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
                let present = if live.contains(iface) { "up" } else { "MISSING" };
                report.push_str(&format!("  {iface}: {present}\n"));
            }
        }
    }
    Ok(report)
}
```

Add to the `tests` module:

```rust
    #[test]
    fn show_plan_lists_commands() {
        let lines = show_plan(&profile(), "en0");
        assert_eq!(lines[0], "ifconfig vlan0 create vlan 100 vlandev en0");
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
        r.stdout.insert("ifconfig -l".to_string(), "lo0 en0 vlan0".to_string());
        let report = status(&mut r, &state_path).unwrap();
        assert!(report.contains("vlan0: up"));
        assert!(report.contains("vlan9: MISSING"));
        std::fs::remove_file(&state_path).unwrap();
    }
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test commands::`
Expected: PASS (now 6 tests).

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: list, show, and status read-only operations"
```

---

## Task 9: CLI definitions

**Files:**
- Modify: `src/cli.rs`

- [ ] **Step 1: Define the CLI**

Replace `src/cli.rs` with:

```rust
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "vlanctl", about = "Apply named VLAN profiles on macOS")]
pub struct Cli {
    /// Directory containing profile .toml files.
    #[arg(long, default_value = "profiles", global = true)]
    pub profiles_dir: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List available profiles.
    List,
    /// Print the commands a profile would run.
    Show { profile: String },
    /// Validate a profile without applying it.
    Validate { profile: String },
    /// Bring up a profile (requires root).
    Apply {
        profile: String,
        /// Print commands without executing them.
        #[arg(long)]
        dry_run: bool,
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
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build`
Expected: compiles (unused-code warnings acceptable until wired in Task 10).

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: clap CLI definitions"
```

---

## Task 10: Wire up main with root check and dispatch

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Replace main.rs**

```rust
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
```

Note: `--dry-run` apply uses `RecordingRunner`, which returns empty stdout for
`ifconfig -l` by default. The dry-run arm above seeds `ifconfig -l` with
`"lo0 en0"` so `resolve_device` and interface allocation work offline without
root; the real (non-dry-run) path queries the live system instead.

- [ ] **Step 2: Add libc dependency**

Run: `cargo add libc@0.2`

- [ ] **Step 3: Build**

```bash
cargo build
```

Expected: compiles cleanly.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat: wire up main dispatch with root check and dry-run"
```

---

## Task 11: Example profile and end-to-end dry-run check

**Files:**
- Create: `profiles/example.toml`

- [ ] **Step 1: Create the example profile**

```toml
# profiles/example.toml
name = "example"
description = "Sample two-VLAN sensor profile"

[[vlan]]
id = 100
address = "192.168.10.2/24"

  [[vlan.route]]
  destination = "192.168.20.0/24"
  gateway = "192.168.10.1"

[[vlan]]
id = 200
address = "10.0.0.5/24"
```

- [ ] **Step 2: Validate it**

Run: `cargo run -- validate example`
Expected output: `example: ok`

- [ ] **Step 3: Dry-run apply**

Run: `cargo run -- apply example --dry-run`
Expected output (interface units may differ if vlan0 exists live, but offline seed gives vlan0/vlan1):

```
ifconfig vlan0 create vlan 100 vlandev en0
ifconfig vlan0 inet 192.168.10.2 netmask 255.255.255.0
route add 192.168.20.0/24 192.168.10.1
ifconfig vlan1 create vlan 200 vlandev en0
ifconfig vlan1 inet 10.0.0.5 netmask 255.255.255.0
```

- [ ] **Step 4: List profiles**

Run: `cargo run -- list`
Expected output includes: `example`

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "docs: add example profile and verify dry-run end to end"
```

---

## Task 12: README and final full-suite run

**Files:**
- Create: `README.md`

- [ ] **Step 1: Write the README**

```markdown
# vlanctl

Apply and tear down named VLAN profiles on macOS for working with
differently-configured lidar sensors.

## Usage

```
vlanctl list                       # list profiles in ./profiles
vlanctl show <profile>             # print the commands a profile would run
vlanctl validate <profile>         # check a profile without applying
sudo vlanctl apply <profile>       # bring it up
vlanctl apply <profile> --dry-run  # print commands without running them
sudo vlanctl down                  # tear down the active profile
vlanctl status                     # what is currently up
```

Profiles live in `profiles/*.toml`. See `profiles/example.toml`.

Mutating commands (`apply`, `down`) modify network interfaces and must run
under `sudo`. State is tracked at `/usr/local/var/vlanctl/state.json`.
```

- [ ] **Step 2: Run the full test suite**

Run: `cargo test`
Expected: all tests PASS, no failures.

- [ ] **Step 3: Check formatting and lints**

Run: `cargo fmt && cargo clippy -- -D warnings`
Expected: no errors. Fix any clippy findings, then re-run.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "docs: add README"
```

---

## Manual Verification (requires hardware + sudo)

These steps are run by the user against a real adapter and sensor; not part of
automated tests:

1. Connect the USB/Thunderbolt Ethernet adapter to the sensor.
2. `sudo vlanctl apply <your-profile>` and confirm `ifconfig` shows the `vlanN`
   interfaces with the expected addresses.
3. Confirm sensor traffic reaches the host (e.g. the point-cloud parser sees data).
4. `vlanctl status` shows the active profile with interfaces `up`.
5. `sudo vlanctl down` and confirm the interfaces are gone.
```
