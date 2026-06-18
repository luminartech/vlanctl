# Untagged Interface Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a vlanctl profile express an untagged parent-interface path (address + routes) alongside tagged VLANs, via a generalized `[[interface]]` schema.

**Architecture:** Rename the profile's `[[vlan]]` entries to `[[interface]]` with an optional `vlan` tag. `vlan = Some(id)` creates a `vlan<id>` sub-interface (today's behaviour, recorded in state for teardown). `vlan = None` configures the parent device directly with an address alias + routes; it is idempotent on apply (the whole untagged interface is skipped when its alias address is already present) and is never recorded or torn down.

**Tech Stack:** Rust, `serde`/`toml`, `ipnet`, `anyhow`. Tests use the existing `RecordingRunner` (no hardware).

## Global Constraints

- IPv4 only — reject non-IPv4 addresses at validation (existing behaviour).
- At most one untagged interface (`vlan = None`) per profile.
- VLAN id range `1..=4094`, unique within a profile — applies only to tagged interfaces.
- `down` and `state.rs` are unchanged: only `vlan<id>` interfaces are recorded and torn down. Untagged parent config persists.
- Preserve the existing in-subnet skip for gatewayless routes (commit `345578a`): a gatewayless route whose destination falls inside the interface's own connected subnet emits no command.
- **The schema rename spans `config.rs`, `plan.rs`, and `commands.rs`; they compile together, so Task 1 edits all three before building.**
- Idempotency mechanism (refinement of the spec's "route-replace" wording): apply skips an untagged interface entirely when its alias address is already configured on the device. Same effect for the re-apply case, simpler and DRY (reuses `bringup_commands`).
- Spec: `docs/superpowers/specs/2026-06-18-untagged-interface-design.md`.

---

### Task 1: Implement the `[[interface]]` schema, command generation, and idempotent apply

This task edits `config.rs`, `plan.rs`, and `commands.rs` together (they share the renamed type and must compile as a unit), then builds and tests once.

**Files:**
- Modify: `src/config.rs`, `src/plan.rs`, `src/commands.rs`

**Interfaces:**
- Produces:
  - `config.rs`: `struct Interface { vlan: Option<u16>, address: IpNet, mtu: Option<u32>, routes: Vec<Route> }`; `Profile { name, description: Option<String>, device: Option<String>, interfaces: Vec<Interface> }`; `Profile::validate(&self) -> Result<()>`. `Route` unchanged.
  - `plan.rs`: `pub fn iface_name(&Interface, &str) -> String`; `pub fn bringup_commands(&Interface, &str) -> Vec<Cmd>`; `pub fn interface_names(&Profile) -> Vec<String>` (tagged names only); `pub fn teardown_commands(&str) -> Vec<Cmd>` unchanged.
  - `commands.rs`: `fn device_inet_addresses<R: CommandRunner>(&mut R, &str) -> Result<Vec<String>>`; updated `apply`, `show_plan`.

- [ ] **Step 1: Rewrite the `config.rs` structs.**

In `src/config.rs`, replace the `Profile` and `Vlan` definitions with:

```rust
#[derive(Debug, Deserialize, PartialEq)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default, rename = "interface")]
    pub interfaces: Vec<Interface>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Interface {
    /// 802.1Q VLAN tag. `None` means untagged: configure the parent device directly.
    #[serde(default)]
    pub vlan: Option<u16>,
    pub address: IpNet,
    #[serde(default)]
    pub mtu: Option<u32>,
    #[serde(default, rename = "route")]
    pub routes: Vec<Route>,
}
```

- [ ] **Step 2: Rewrite `Profile::validate`.**

Replace the body of `validate` with:

```rust
pub fn validate(&self) -> Result<()> {
    if self.interfaces.is_empty() {
        bail!("profile '{}' has no [[interface]] entries", self.name);
    }
    let mut seen = std::collections::HashSet::new();
    let mut untagged = 0;
    for iface in &self.interfaces {
        match iface.vlan {
            None => untagged += 1,
            Some(id) => {
                if !(1..=4094).contains(&id) {
                    bail!("vlan id {} out of range (1..=4094)", id);
                }
                if !seen.insert(id) {
                    bail!("duplicate vlan id {} in profile '{}'", id, self.name);
                }
            }
        }
        // Only IPv4 is supported: netmask computation assumes a /0../32 prefix.
        if !iface.address.addr().is_ipv4() {
            bail!(
                "interface address {} is not IPv4 (IPv6 is unsupported)",
                iface.address
            );
        }
        for route in &iface.routes {
            if route.destination != "default" && route.destination.parse::<IpNet>().is_err() {
                bail!(
                    "route destination '{}' is not a CIDR or \"default\"",
                    route.destination
                );
            }
        }
    }
    if untagged > 1 {
        bail!(
            "profile '{}' has {} untagged interfaces (at most one allowed)",
            self.name,
            untagged
        );
    }
    Ok(())
}
```

- [ ] **Step 3: Update `config.rs` tests and add untagged coverage.**

In the `tests` module: in every TOML literal change `[[vlan]]`→`[[interface]]`, `[[vlan.route]]`→`[[interface.route]]`, and `id = N`→`vlan = N`; change `p.vlans`→`p.interfaces`. In `parses_multi_vlan_profile`, set assertions to:

```rust
assert_eq!(p.interfaces.len(), 2);
assert_eq!(p.interfaces[0].vlan, Some(100));
assert_eq!(p.interfaces[0].routes.len(), 1);
assert_eq!(p.interfaces[1].routes.len(), 0);
```

In `rejects_empty_vlan_list`, change the expected substring to `"no [[interface]] entries"`. In `route_without_gateway_is_interface_scoped`, change `p.vlans`→`p.interfaces`. Then add:

```rust
#[test]
fn parses_untagged_interface() {
    let p = profile_with(
        "[[interface]]\naddress = \"192.168.1.100/24\"\n\
         [[interface.route]]\ndestination = \"192.168.10.151/32\"\n\
         [[interface]]\nvlan = 12\naddress = \"192.168.10.1/24\"\n",
    )
    .unwrap();
    assert_eq!(p.interfaces[0].vlan, None);
    assert_eq!(p.interfaces[0].routes.len(), 1);
    assert_eq!(p.interfaces[1].vlan, Some(12));
}

#[test]
fn rejects_two_untagged_interfaces() {
    let err = profile_with(
        "[[interface]]\naddress = \"192.168.1.100/24\"\n\
         [[interface]]\naddress = \"192.168.2.100/24\"\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("at most one"));
}
```

- [ ] **Step 4: Rewrite `plan.rs` command generation.**

Change the top import to `use crate::config::{Interface, Profile, Route};`. Replace `bringup_commands` and `interface_names` with the following (keep `destination_in_subnet`, `interface_route_command`, `ipv4_netmask`, `teardown_commands` unchanged):

```rust
/// Interface name this entry configures: `vlan<id>` for a tagged interface,
/// or the parent `device` itself for an untagged one.
pub fn iface_name(interface: &Interface, device: &str) -> String {
    match interface.vlan {
        Some(id) => format!("vlan{id}"),
        None => device.to_string(),
    }
}

/// Build the ordered commands to bring up one interface on `device`.
pub fn bringup_commands(interface: &Interface, device: &str) -> Vec<Cmd> {
    let name = iface_name(interface, device);
    let addr = interface.address.addr().to_string();
    let netmask = ipv4_netmask(interface.address.prefix_len());

    let mut cmds = Vec::new();
    match interface.vlan {
        // Tagged: create the vlan pseudo-device, then bind tag+parent in a
        // SEPARATE call (a combined `create ... vlandev` leaves it unbound),
        // then assign the address.
        Some(id) => {
            let id = id.to_string();
            cmds.push(Cmd::new("ifconfig", &[&name, "create"]));
            cmds.push(Cmd::new("ifconfig", &[&name, "vlan", &id, "vlandev", device]));
            cmds.push(Cmd::new("ifconfig", &[&name, "inet", &addr, "netmask", &netmask]));
        }
        // Untagged: add the address as an alias on the parent device. `down`
        // never removes it; apply skips this interface when the address is
        // already configured.
        None => {
            cmds.push(Cmd::new(
                "ifconfig",
                &[&name, "inet", &addr, "netmask", &netmask, "alias"],
            ));
        }
    }
    if let Some(mtu) = interface.mtu {
        cmds.push(Cmd::new("ifconfig", &[&name, "mtu", &mtu.to_string()]));
    }
    for route in &interface.routes {
        match &route.gateway {
            Some(gateway) => {
                let gateway = gateway.to_string();
                cmds.push(Cmd::new("route", &["add", &route.destination, &gateway]));
            }
            // A gatewayless destination inside this interface's own connected
            // subnet is reached by the connected route; an explicit
            // `-host ... -interface` route would install a self-MAC ARP entry
            // that breaks resolution, so it is skipped.
            None if destination_in_subnet(&route.destination, &interface.address) => {}
            None => cmds.push(interface_route_command(&route.destination, &name)),
        }
    }
    cmds
}

/// Names of the VLAN sub-interfaces this profile creates (`vlan<id>`), in order.
/// Untagged interfaces configure the parent device and are not named here, so
/// they are never recorded in state or torn down.
pub fn interface_names(profile: &Profile) -> Vec<String> {
    profile
        .interfaces
        .iter()
        .filter_map(|i| i.vlan.map(|id| format!("vlan{id}")))
        .collect()
}
```

- [ ] **Step 5: Update `plan.rs` tests.**

In the `tests` module, replace the `vlan` constructor helper and the affected tests:

```rust
fn tagged(id: u16, cidr: &str) -> Interface {
    Interface { vlan: Some(id), address: cidr.parse().unwrap(), mtu: None, routes: vec![] }
}

#[test]
fn tagged_creates_assigns_and_routes() {
    let mut v = tagged(100, "192.168.10.2/24");
    v.mtu = Some(1500);
    v.routes.push(Route {
        destination: "192.168.20.0/24".to_string(),
        gateway: Some("192.168.10.1".parse::<IpAddr>().unwrap()),
    });
    let rendered: Vec<String> = bringup_commands(&v, "en10").iter().map(|c| c.display()).collect();
    assert_eq!(
        rendered,
        vec![
            "ifconfig vlan100 create",
            "ifconfig vlan100 vlan 100 vlandev en10",
            "ifconfig vlan100 inet 192.168.10.2 netmask 255.255.255.0",
            "ifconfig vlan100 mtu 1500",
            "route add 192.168.20.0/24 192.168.10.1",
        ]
    );
}

#[test]
fn tagged_routes_without_gateway() {
    let mut v = tagged(10, "192.168.10.90/24");
    v.routes.push(Route { destination: "192.168.10.150/32".to_string(), gateway: None });
    v.routes.push(Route { destination: "10.9.9.9/32".to_string(), gateway: None });
    v.routes.push(Route { destination: "239.255.0.0/24".to_string(), gateway: None });
    let rendered: Vec<String> = bringup_commands(&v, "en10").iter().map(|c| c.display()).collect();
    assert!(!rendered.iter().any(|c| c.contains("192.168.10.150"))); // in-subnet -> skipped
    assert!(rendered.contains(&"route add -host 10.9.9.9 -interface vlan10".to_string()));
    assert!(rendered.contains(&"route add -net 239.255.0.0/24 -interface vlan10".to_string()));
}

#[test]
fn untagged_configures_parent_with_alias_and_routes() {
    let mut u = Interface { vlan: None, address: "192.168.1.100/24".parse().unwrap(), mtu: None, routes: vec![] };
    u.routes.push(Route { destination: "192.168.10.151/32".to_string(), gateway: None });
    let rendered: Vec<String> = bringup_commands(&u, "en16").iter().map(|c| c.display()).collect();
    assert_eq!(
        rendered,
        vec![
            "ifconfig en16 inet 192.168.1.100 netmask 255.255.255.0 alias",
            "route add -host 192.168.10.151 -interface en16",
        ]
    );
}

#[test]
fn untagged_skips_in_subnet_gatewayless_route() {
    let mut u = Interface { vlan: None, address: "192.168.10.1/24".parse().unwrap(), mtu: None, routes: vec![] };
    u.routes.push(Route { destination: "192.168.10.152/32".to_string(), gateway: None });
    let rendered: Vec<String> = bringup_commands(&u, "en16").iter().map(|c| c.display()).collect();
    assert_eq!(rendered, vec!["ifconfig en16 inet 192.168.10.1 netmask 255.255.255.0 alias"]);
}
```

Delete the old `bringup_creates_assigns_and_routes` and `interface_routes_without_gateway` tests (replaced above). Update `interface_names_match_vlan_ids` TOML to use `[[interface]]` with `vlan = 10` / `vlan = 11`.

- [ ] **Step 6: Add `device_inet_addresses` and rewrite the `apply` loop in `commands.rs`.**

Change the plan import to `use crate::plan::{bringup_commands, iface_name, interface_names, teardown_commands};`. Add the helper next to `live_interfaces`:

```rust
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
```

Replace the guard + bring-up loop in `apply` (from `let interfaces = interface_names(profile);` through the end of the `for (iface, vlan) in ...` loop) with:

```rust
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
```

- [ ] **Step 7: Rewrite `show_plan` in `commands.rs`.**

```rust
/// Render the full bring-up plan for a profile as displayable command lines.
pub fn show_plan(profile: &Profile, device: &str) -> Vec<String> {
    profile
        .interfaces
        .iter()
        .flat_map(|interface| bringup_commands(interface, device))
        .map(|cmd| cmd.display())
        .collect()
}
```

- [ ] **Step 8: Update `commands.rs` tests and add untagged coverage.**

Update the `profile()` helper TOML to the new key:

```rust
fn profile() -> Profile {
    let p: Profile = toml::from_str(
        "name=\"t\"\ndevice=\"en0\"\n\
         [[interface]]\nvlan=100\naddress=\"192.168.10.2/24\"\n\
         [[interface]]\nvlan=200\naddress=\"10.0.0.5/24\"\n",
    )
    .unwrap();
    p.validate().unwrap();
    p
}
```

Then add:

```rust
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
```

Note: `runner_with_device()` already inserts `"ifconfig -l" -> "lo0 en0"`; leaving `"ifconfig en0"` unset makes `device_inet_addresses` return empty (default `""`), so the first test adds the alias. The `show_plan_lists_commands` test continues to work unchanged (it uses `profile()`).

- [ ] **Step 9: Build and run the full library test suite.**

Run: `cargo test --lib 2>&1 | tail -20`
Expected: PASS — clean build, all `config`/`plan`/`commands`/`device`/`net`/`state` tests pass.

- [ ] **Step 10: Commit.**

```bash
git add src/config.rs src/plan.rs src/commands.rs
git commit -m "feat: generalize schema to [[interface]] with untagged parent-device support"
```

---

### Task 2: Migrate on-disk profiles and validate end-to-end

**Files:**
- Modify: `profiles/example.toml`, `profiles/lum.toml`, `profiles/lum_legacy.toml`, `profiles/halo.toml`

**Interfaces:**
- Consumes: the new schema (Task 1).

- [ ] **Step 1: Migrate the tagged profiles (no behaviour change).**

In `profiles/example.toml`, `profiles/lum.toml`, and `profiles/lum_legacy.toml`: change each `[[vlan]]`→`[[interface]]`, each `id = N`→`vlan = N`, and each `[[vlan.route]]`→`[[interface.route]]`. Leave addresses, routes, comments, and `device` lines unchanged.

- [ ] **Step 2: Rewrite `profiles/halo.toml` with the untagged data path.**

Replace `profiles/halo.toml` with:

```toml
name = "halo"
description = "Halo A3 (LUM Gen FW): untagged UDP data path + VLAN 12 telnet."

# Untagged UDP data path: sensor 192.168.10.151 (out-of-subnet), host
# 192.168.1.100/24, plus the SOME/IP-SD multicast group. vlanctl configures the
# parent device directly and leaves it configured on `down`.
[[interface]]
address = "192.168.1.100/24"

  # Data sensor — out of the host's /24, so reached via an on-link route.
  [[interface.route]]
  destination = "192.168.10.151/32"

  # SOME/IP-SD multicast group.
  [[interface.route]]
  destination = "239.255.0.255/32"

# VLAN 12 — telnet debug. Sensor .152, reached by the connected /24 route.
[[interface]]
vlan = 12
address = "192.168.10.1/24"

# HARDWARE NOTE: requires a USB NIC that transmits 802.1Q tags under macOS.
# The 98:fc:84:ec:ea:f1 dongle (en7) silently drops tags on TX; the Realtek
# dongle (00:e0:4c:..., en16) works. Pin `device` to the good adapter if
# auto-detect might pick the bad one.
```

- [ ] **Step 3: Validate every profile and dry-run `show halo`.**

Run:
```bash
cargo build 2>&1 | tail -3
for p in example lum lum_legacy halo; do ./target/debug/vlanctl validate "$p"; done
./target/debug/vlanctl show halo
```
Expected: each profile prints `<name>: ok`. `show halo` prints (device is auto-detected — `en16` when it is the active wired adapter):
```
ifconfig en16 inet 192.168.1.100 netmask 255.255.255.0 alias
route add -host 192.168.10.151 -interface en16
route add -host 239.255.0.255 -interface en16
ifconfig vlan12 create
ifconfig vlan12 vlan 12 vlandev en16
ifconfig vlan12 inet 192.168.10.1 netmask 255.255.255.0
```

- [ ] **Step 4: Run the full test suite once more.**

Run: `cargo test 2>&1 | tail -5`
Expected: all tests pass.

- [ ] **Step 5: Commit.**

```bash
git add profiles/
git commit -m "feat: migrate profiles to [[interface]]; halo gains untagged data path"
```

---

## Hardware validation (manual, after merge — not a code task)

On a known-good adapter (`en16`): `sudo vlanctl apply halo`, then confirm telnet
`192.168.10.152:23` connects and the untagged data route to `192.168.10.151`
reaches the sensor. **Resolve the `-host ... -interface` self-MAC black-hole risk
for the out-of-subnet data sensor here** — if it black-holes, the fallback is a
static ARP entry (`.151` = `3a:42:f7:79:32:2e`), which would become a follow-up
feature. See the spec's Risks section.
