# Static ARP (`mac =`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a gatewayless `/32` route declare a static ARP entry via `mac =`, so vlanctl emits `arp -d`/`arp -s` to resolve an on-link host the macOS kernel can't ARP for (the Halo `.151` data path).

**Architecture:** Add `best_effort` to `Cmd` (failures ignored by apply) for the `arp -d`. Add `mac: Option<String>` to `Route`, validated to gatewayless `/32` with a well-formed MAC. `bringup_commands` appends `arp -d <host>` (best-effort) then `arp -s <host> <mac>` after a mac route's route line. apply tolerates best-effort command failures.

**Tech Stack:** Rust, `serde`/`toml`, `ipnet`, `anyhow`. Tests use `RecordingRunner` (no hardware).

## Global Constraints

- `mac` is permitted ONLY on a gatewayless route whose destination is a single host (`/32`, i.e. `prefix_len() == max_prefix_len()`); reject `mac` with a gateway, on a non-/32, or on `"default"`, and reject a malformed MAC. A valid MAC is six colon-separated groups of one or two hex digits.
- The defensive `arp -d <host>` is emitted as a **best-effort** command (its failure must not fail apply); it precedes `arp -s <host> <mac>`.
- The existing in-subnet skip still governs the `route add` line; the `arp -s` is emitted regardless (it is what forces resolution).
- Existing behaviour with `mac = None` is unchanged; all existing `Cmd::new` call sites stay `best_effort: false`.
- down / state unchanged. Untagged config (incl. ARP entry) persists.
- Spec: `docs/superpowers/specs/2026-06-18-static-arp-design.md`.

---

### Task 1: Add a best-effort flag to `Cmd` (`net.rs`)

**Files:**
- Modify: `src/net.rs`

**Interfaces:**
- Produces: `Cmd { program: String, args: Vec<String>, best_effort: bool }`; `Cmd::new(program, args) -> Cmd` (best_effort=false, unchanged signature); `Cmd::new_best_effort(program, args) -> Cmd` (best_effort=true).

- [ ] **Step 1: Write the failing test.**

Add to the `tests` module of `src/net.rs`:

```rust
#[test]
fn best_effort_flag_is_set_correctly() {
    assert_eq!(Cmd::new("arp", &["-s", "x"]).best_effort, false);
    let be = Cmd::new_best_effort("arp", &["-d", "x"]);
    assert_eq!(be.best_effort, true);
    assert_eq!(be.display(), "arp -d x"); // flag does not affect rendering
}
```

- [ ] **Step 2: Run it to verify it fails.**

Run: `cargo test --bin vlanctl net::tests::best_effort 2>&1 | tail -10`
Expected: FAIL — `best_effort` field / `new_best_effort` not found.

- [ ] **Step 3: Add the field and constructor.**

In `src/net.rs`, add the field to the struct:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// When true, a run failure is ignored by apply instead of being a hard
    /// error (used for `arp -d`, which fails when no entry exists).
    pub best_effort: bool,
}
```

Update `Cmd::new` to set it and add `new_best_effort`:

```rust
    pub fn new(program: &str, args: &[&str]) -> Cmd {
        Cmd {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            best_effort: false,
        }
    }

    /// Like `new`, but a run failure is ignored by apply.
    pub fn new_best_effort(program: &str, args: &[&str]) -> Cmd {
        let mut cmd = Cmd::new(program, args);
        cmd.best_effort = true;
        cmd
    }
```

`display()` is unchanged.

- [ ] **Step 4: Run tests to verify they pass.**

Run: `cargo test --bin vlanctl net:: 2>&1 | tail -10`
Expected: PASS (all `net::tests::*`).

- [ ] **Step 5: Commit.**

```bash
git add src/net.rs
git commit -m "feat: add best_effort flag to Cmd"
```

---

### Task 2: Add `mac` to `Route` with validation (`config.rs`)

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `Route { destination: String, gateway: Option<IpAddr>, mac: Option<String> }`; validation rejecting invalid `mac` usage.

- [ ] **Step 1: Write failing tests.**

Add to the `tests` module of `src/config.rs`:

```rust
#[test]
fn accepts_mac_on_gatewayless_host_route() {
    let p = profile_with(
        "[[interface]]\naddress = \"192.168.1.100/24\"\n\
         [[interface.route]]\ndestination = \"192.168.10.151/32\"\nmac = \"3a:42:f7:79:32:2e\"\n",
    )
    .unwrap();
    assert_eq!(p.interfaces[0].routes[0].mac.as_deref(), Some("3a:42:f7:79:32:2e"));
}

#[test]
fn rejects_mac_with_gateway() {
    let err = profile_with(
        "[[interface]]\naddress = \"192.168.1.2/24\"\n\
         [[interface.route]]\ndestination = \"192.168.10.151/32\"\ngateway = \"192.168.1.1\"\nmac = \"3a:42:f7:79:32:2e\"\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("gateway"));
}

#[test]
fn rejects_mac_on_non_host_route() {
    let err = profile_with(
        "[[interface]]\naddress = \"192.168.1.2/24\"\n\
         [[interface.route]]\ndestination = \"192.168.10.0/24\"\nmac = \"3a:42:f7:79:32:2e\"\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("/32") || err.to_string().contains("single host"));
}

#[test]
fn rejects_malformed_mac() {
    let err = profile_with(
        "[[interface]]\naddress = \"192.168.1.2/24\"\n\
         [[interface.route]]\ndestination = \"192.168.10.151/32\"\nmac = \"not-a-mac\"\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("MAC") || err.to_string().contains("mac"));
}
```

- [ ] **Step 2: Run to verify they fail.**

Run: `cargo test --bin vlanctl config::tests::accepts_mac config::tests::rejects_mac config::tests::rejects_malformed 2>&1 | tail -15`
Expected: FAIL — `mac` field unknown / not validated.

- [ ] **Step 3: Add the field.**

In `src/config.rs`, add to `Route`:

```rust
#[derive(Debug, Deserialize, PartialEq)]
pub struct Route {
    pub destination: String,
    /// Next-hop gateway. If omitted, the route is scoped to the owning
    /// interface (`route add ... -interface <iface>`) instead of a gateway.
    #[serde(default)]
    pub gateway: Option<IpAddr>,
    /// Static ARP entry (`arp -s <host> <mac>`) for an on-link host the kernel
    /// cannot resolve itself. Only valid on a gatewayless /32 route.
    #[serde(default)]
    pub mac: Option<String>,
}
```

- [ ] **Step 4: Add a MAC-format helper and validation.**

Add this free function in `src/config.rs` (outside `impl Profile`):

```rust
/// True if `s` is six colon-separated groups of one or two hex digits.
fn is_valid_mac(s: &str) -> bool {
    let groups: Vec<&str> = s.split(':').collect();
    groups.len() == 6
        && groups
            .iter()
            .all(|g| (1..=2).contains(&g.len()) && g.bytes().all(|b| b.is_ascii_hexdigit()))
}
```

In `validate`, inside the `for route in &iface.routes` loop, after the existing
destination check, add:

```rust
            if let Some(mac) = &route.mac {
                if route.gateway.is_some() {
                    bail!(
                        "route '{}' has both a gateway and a mac (mutually exclusive)",
                        route.destination
                    );
                }
                let is_host = route
                    .destination
                    .parse::<IpNet>()
                    .map(|n| n.prefix_len() == n.max_prefix_len())
                    .unwrap_or(false);
                if !is_host {
                    bail!(
                        "route '{}' has a mac but is not a single host (/32 required)",
                        route.destination
                    );
                }
                if !is_valid_mac(mac) {
                    bail!("route '{}' has an invalid MAC '{}'", route.destination, mac);
                }
            }
```

- [ ] **Step 5: Run tests.**

Run: `cargo test --bin vlanctl config:: 2>&1 | tail -15`
Expected: PASS (all `config::tests::*`, including the four new ones).

- [ ] **Step 6: Commit.**

```bash
git add src/config.rs
git commit -m "feat: add validated mac field to Route"
```

---

### Task 3: Emit static-ARP commands and tolerate best-effort failures (`plan.rs`, `commands.rs`)

**Files:**
- Modify: `src/plan.rs`, `src/commands.rs`

**Interfaces:**
- Consumes: `Cmd::new_best_effort` (Task 1), `Route.mac` (Task 2).
- Produces: `bringup_commands` emits `arp -d`/`arp -s` for mac routes; apply ignores best-effort failures.

- [ ] **Step 1: Write failing plan test.**

Add to the `tests` module of `src/plan.rs`:

```rust
#[test]
fn gatewayless_host_route_with_mac_emits_static_arp() {
    let mut u = Interface { vlan: None, address: "192.168.1.100/24".parse().unwrap(), mtu: None, routes: vec![] };
    u.routes.push(Route {
        destination: "192.168.10.151/32".to_string(),
        gateway: None,
        mac: Some("3a:42:f7:79:32:2e".to_string()),
    });
    let cmds = bringup_commands(&u, "en16");
    let rendered: Vec<String> = cmds.iter().map(|c| c.display()).collect();
    assert_eq!(
        rendered,
        vec![
            "ifconfig en16 inet 192.168.1.100 netmask 255.255.255.0 alias",
            "route add -host 192.168.10.151 -interface en16",
            "arp -d 192.168.10.151",
            "arp -s 192.168.10.151 3a:42:f7:79:32:2e",
        ]
    );
    // The arp -d must be best-effort.
    let arp_d = cmds.iter().find(|c| c.args.first().map(|a| a == "-d").unwrap_or(false)).unwrap();
    assert!(arp_d.best_effort);
}
```

Note: the existing `tagged`/`untagged` test constructors build `Route` with struct literals — those now need the `mac: None` field. Update every `Route { ... }` literal in the `plan.rs` tests to include `mac: None`.

- [ ] **Step 2: Run to verify it fails.**

Run: `cargo test --bin vlanctl plan:: 2>&1 | tail -15`
Expected: FAIL — `Route` missing `mac` field in literals / no arp commands emitted.

- [ ] **Step 3: Emit the arp commands in `bringup_commands`.**

In `src/plan.rs`, replace the per-route loop in `bringup_commands` with:

```rust
    for route in &interface.routes {
        match &route.gateway {
            Some(gateway) => {
                let gateway = gateway.to_string();
                cmds.push(Cmd::new("route", &["add", &route.destination, &gateway]));
            }
            None => {
                // A gatewayless destination inside this interface's own
                // connected subnet is reached by the connected route; an
                // explicit `-host ... -interface` route would install a
                // self-MAC ARP entry that breaks resolution, so it is skipped.
                if !destination_in_subnet(&route.destination, &interface.address) {
                    cmds.push(interface_route_command(&route.destination, &name));
                }
                // Static ARP for an on-link host the kernel can't resolve.
                // Validation guarantees mac => gatewayless /32, so the address
                // parse below always succeeds.
                if let Some(mac) = &route.mac
                    && let Ok(net) = route.destination.parse::<IpNet>()
                {
                    let host = net.addr().to_string();
                    cmds.push(Cmd::new_best_effort("arp", &["-d", &host]));
                    cmds.push(Cmd::new("arp", &["-s", &host, mac]));
                }
            }
        }
    }
```

- [ ] **Step 4: Update the other `plan.rs` test `Route` literals.**

In every existing `plan.rs` test that constructs `Route { destination, gateway }`, add `mac: None`. (Tests: `tagged_creates_assigns_and_routes`, `tagged_routes_without_gateway`, `untagged_configures_parent_with_alias_and_routes`, `untagged_skips_in_subnet_gatewayless_route`, and any others.)

- [ ] **Step 5: Write failing commands test for best-effort tolerance.**

Add to the `tests` module of `src/commands.rs`:

```rust
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
fn apply_tolerates_best_effort_arp_d_failure() {
    let state_path = std::env::temp_dir().join("vlanctl-apply-best-effort.json");

    // First run, no failures: capture the command list and find the arp -d index.
    let _ = std::fs::remove_file(&state_path);
    let mut r = runner_with_device();
    let created = apply(&mut r, &mac_profile(), &state_path, false).unwrap();
    assert!(created.is_empty()); // untagged-only profile records nothing
    let rendered: Vec<String> = r.commands.iter().map(|c| c.display()).collect();
    assert!(rendered.contains(&"arp -d 192.168.10.151".to_string()));
    assert!(rendered.contains(&"arp -s 192.168.10.151 3a:42:f7:79:32:2e".to_string()));
    let arp_d_index = r
        .commands
        .iter()
        .position(|c| c.args.first().map(|a| a == "-d").unwrap_or(false))
        .unwrap();

    // Second run: remove the state file first so no `down` runs (keeping command
    // indices identical to the first run), make `arp -d` fail, and confirm apply
    // still succeeds and still runs `arp -s`.
    let _ = std::fs::remove_file(&state_path);
    let mut r2 = runner_with_device();
    r2.fail_at = Some(arp_d_index);
    apply(&mut r2, &mac_profile(), &state_path, false).unwrap(); // must NOT error
    let rendered2: Vec<String> = r2.commands.iter().map(|c| c.display()).collect();
    assert!(rendered2.contains(&"arp -s 192.168.10.151 3a:42:f7:79:32:2e".to_string()));
}
```

- [ ] **Step 6: Run to verify it fails.**

Run: `cargo test --bin vlanctl commands::tests::apply_tolerates_best_effort 2>&1 | tail -15`
Expected: FAIL — apply errors when `arp -d` fails (best-effort not yet honored).

- [ ] **Step 7: Honor `best_effort` in apply's run loops.**

In `src/commands.rs`, in the **untagged** branch's run loop, change the error handling to:

```rust
                for cmd in bringup_commands(interface, &device) {
                    if let Err(e) = runner.run(&cmd) {
                        if cmd.best_effort {
                            continue;
                        }
                        rollback(runner, &created);
                        return Err(e).with_context(|| {
                            format!("applying profile '{}'; rolled back", profile.name)
                        });
                    }
                }
```

In the **tagged** branch's run loop, change the `Err(e)` arm to:

```rust
                        Err(e) => {
                            if cmd.best_effort {
                                continue;
                            }
                            rollback(runner, &created);
                            return Err(e).with_context(|| {
                                format!("applying profile '{}'; rolled back", profile.name)
                            });
                        }
```

- [ ] **Step 8: Run the full suite.**

Run: `cargo test 2>&1 | tail -8`
Expected: PASS — all tests, including the new plan and commands tests.

- [ ] **Step 9: Commit.**

```bash
git add src/plan.rs src/commands.rs
git commit -m "feat: emit arp -d/arp -s for mac routes; apply tolerates best-effort failures"
```

---

### Task 4: Add `mac` to halo's data route and validate end-to-end

**Files:**
- Modify: `profiles/halo.toml`

**Interfaces:**
- Consumes: the new schema (Tasks 1-3).

- [ ] **Step 1: Add the static ARP to halo's `.151` route.**

In `profiles/halo.toml`, change the data-sensor route block to:

```toml
  # Data sensor — out of the host's /24, reached via an on-link route + static
  # ARP (the sensor doesn't answer ARP; macOS would otherwise self-MAC it).
  [[interface.route]]
  destination = "192.168.10.151/32"
  mac = "3a:42:f7:79:32:2e"
```

Leave the multicast route and the `vlan = 12` block unchanged.

- [ ] **Step 2: Validate and dry-run `show halo`.**

Run:
```bash
cargo build 2>&1 | tail -3
./target/debug/vlanctl validate halo
./target/debug/vlanctl show halo
```
Expected: `halo: ok`, and `show halo` includes (device auto-detected, e.g. `en16`):
```
ifconfig en16 inet 192.168.1.100 netmask 255.255.255.0 alias
route add -host 192.168.10.151 -interface en16
arp -d 192.168.10.151
arp -s 192.168.10.151 3a:42:f7:79:32:2e
route add -host 239.255.0.255 -interface en16
ifconfig vlan12 create
ifconfig vlan12 vlan 12 vlandev en16
ifconfig vlan12 inet 192.168.10.1 netmask 255.255.255.0
```

- [ ] **Step 3: Run the full test suite.**

Run: `cargo test 2>&1 | tail -5`
Expected: all tests pass.

- [ ] **Step 4: Commit.**

```bash
git add profiles/halo.toml
git commit -m "feat: static ARP for halo data sensor (.151)"
```

---

## Hardware validation (manual, after merge — not a code task)

On a good adapter (`en16`): `sudo vlanctl apply halo`, then confirm
`arp -an | grep 192.168.10.151` shows a **single** entry at `3a:42:f7:79:32:2e`
(not the host's own MAC), the data path reaches the sensor, and telnet
`192.168.10.152:23` still connects.
