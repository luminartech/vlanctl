# vlanctl — Programmatic VLAN Profile Management for macOS

**Date:** 2026-06-15
**Status:** Approved design, pending implementation plan

## Problem

Working with Luminar lidar sensors on a MacBook Pro requires VLAN-tagged
Ethernet connectivity. Different sensors (and test rigs) use different VLAN IDs,
IP addresses, subnets, and routes. Configuring these by hand with `ifconfig` and
`route` is tedious, error-prone, and not reproducible.

We want a programmatic, version-controlled way to:

1. **Switch between sensor profiles** — apply/tear down a chosen named config on demand.
2. **Bring up many VLANs at once** — a single profile can stand up a multi-VLAN test rig.
3. **Reproduce setups** — profiles are checked into the repo instead of typed by hand.

Auto-detection of a connected sensor's VLAN/IP (sniffing the link) is explicitly
out of scope — considered overkill for now.

## Solution Overview

A Rust CLI, `vlanctl`, invoked as `sudo vlanctl <command>`. It reads declarative
TOML **profile** files (one file per profile), each describing one or more VLANs
with their IP addresses and routes, and applies or tears them down by shelling
out to `ifconfig` and `route`.

## Profile Format (TOML)

One file per profile, stored in a `profiles/` directory and checked into the repo.

```toml
# profiles/iris_bench.toml
name = "iris_bench"
description = "Iris sensor on the test bench"

# Optional: pin the physical Ethernet adapter the VLANs attach to (vlandev).
# Omit to auto-detect the active USB/Thunderbolt Ethernet device.
# device = "en10"

[[vlan]]
id = 100                      # 802.1Q VLAN tag (1..=4094)
address = "192.168.10.2/24"   # CIDR; netmask derived from prefix
# mtu = 1500                  # optional

  [[vlan.route]]
  destination = "192.168.20.0/24"
  gateway = "192.168.10.1"

[[vlan]]
id = 200
address = "10.0.0.5/24"
```

### Field semantics

- `name` (string, required): profile identifier; should match the filename stem.
- `description` (string, optional): human-readable note.
- `device` (string, optional): the physical Ethernet interface (e.g. `en10`) the
  VLANs attach to via `vlandev`. If omitted, the tool auto-detects the active
  Ethernet device; a profile-level value overrides the auto-detection.
- `[[vlan]]` (array, ≥1): each entry is one VLAN sub-interface.
  - `id` (int, required): 802.1Q tag, 1–4094.
  - `address` (string, required): CIDR notation (e.g. `192.168.10.2/24`).
  - `mtu` (int, optional): interface MTU.
  - `[[vlan.route]]` (array, optional): routes to add once the VLAN is up.
    - `destination` (string, required): CIDR or `default`.
    - `gateway` (string, optional): next-hop IP. If present, emits
      `route add <destination> <gateway>`. If omitted, the route is scoped to
      this VLAN's own interface: a single host (`/32`) emits
      `route add -host <ip> -interface vlanN`, otherwise
      `route add -net <cidr> -interface vlanN`. Interface-scoped routes are
      required when VLANs share a subnet and for per-interface multicast
      (e.g. SOME/IP-SD).

## Interface Binding

The physical adapter is **auto-detected by default** (active Ethernet device,
typically a USB or Thunderbolt adapter), with a **per-profile `device` override**.
This is the most ergonomic option: it works when the adapter enumerates as a
different `enX` between sessions, but still allows pinning when needed.

## Privilege Model

The tool is intended to be **run as a whole under `sudo`** (`sudo vlanctl apply
<profile>`), because macOS requires root for `ifconfig` VLAN creation and `route`
changes. On startup, mutating commands check for root and exit with a clear error
if not elevated. Read-only commands (`list`, `show`, `validate`) do not require root.

## Commands

| Command | Root | Description |
|---------|------|-------------|
| `vlanctl list` | no | List available profiles in `profiles/`. |
| `vlanctl show <profile>` | no | Print the resolved plan — the exact `ifconfig`/`route` commands that would run. |
| `vlanctl validate <profile>` | no | Schema + semantic validation (valid CIDRs, VLAN ID range, no duplicate VLAN IDs within a profile). |
| `vlanctl apply <profile> [--dry-run]` | yes | Bring up the profile. |
| `vlanctl down [<profile>] [--dry-run]` | yes | Tear down a profile (defaults to the currently active one). |
| `vlanctl status` | no | Show what is currently up, reconciled against live `ifconfig`. |

`--dry-run` is available on every mutating command and prints the exact commands
without executing them.

## Apply / Down Semantics

`apply` is **imperative with state tracking** (a reconcile/diff engine was
considered and rejected as YAGNI for swapping a handful of profiles):

1. Load and fully **validate** the profile before touching the network.
2. If another profile is currently active (per the state file), **tear it down** first.
3. Resolve the physical device (auto-detect or `device` override).
4. The interface for a VLAN is `vlan<id>` — the kernel unit number is chosen to
   match the 802.1Q id (VLAN 10 → `vlan10`). If `vlan<id>` already exists and was
   not created by vlanctl, apply aborts with a clear error before touching anything.
5. For each VLAN, in order:
   - `ifconfig vlan<id> create vlan <id> vlandev <device>`.
   - Assign address: `ifconfig vlan<id> inet <ip> netmask <mask>` (and `mtu` if set).
   - Add each route (gateway or interface-scoped, per the route definition).
6. **Record** every interface created and the active profile name in the state file.

### Rollback

If any step fails mid-apply, the tool **rolls back** everything it created during
that run (destroying created `vlanN` interfaces, which also drops their routes),
leaving the system in its pre-apply state rather than half-configured.

### State file

`apply`/`down`/`status` rely on a small JSON state file at
`/usr/local/var/vlanctl/state.json` recording the active profile and the
interfaces the tool created. Interface names are now derivable (`vlan<id>`), but
the state still records which profile is active so `down`/`status` work without
re-reading a profile, and so teardown is robust to later profile edits. `status`
reconciles the recorded state against live `ifconfig` output and flags drift.

`down` destroys the recorded interfaces for the target profile and clears the
corresponding state.

## Internal Structure

Small, independently testable units:

- **`config`** — load and validate TOML profiles (serde). Owns the profile schema
  and all semantic validation.
- **`net`** — a `CommandRunner` trait abstracting execution of `ifconfig`/`route`.
  - Real implementation shells out to the system binaries.
  - Mock implementation records issued commands; powers both unit tests and `--dry-run`.
- **`vlan`** — builds the create/destroy + address-assignment command sequences for a VLAN.
- **`route`** — builds add/delete command sequences for routes.
- **`state`** — persist and read the active profile + created interfaces (state file I/O).
- **`cli`** — clap subcommand definitions and argument parsing.
- **`main`** — wiring, root check for mutating commands, top-level error reporting.

## Error Handling

- Validation runs fully before any network mutation; invalid profiles abort with a
  descriptive error and zero side effects.
- Mutating commands without root abort early with a clear "run under sudo" message.
- Mid-apply failures trigger rollback of that run's changes (see above).
- `status` surfaces drift between recorded state and live interfaces rather than
  silently trusting either.

## Testing Strategy

- **Config parsing**: fixture-based tests for valid and invalid profiles
  (bad CIDR, out-of-range VLAN ID, duplicate IDs, missing required fields).
- **Apply/down/rollback logic**: driven through the `CommandRunner` trait with a
  mock that records commands and can simulate failures, verifying both the
  happy-path command sequence and correct rollback on a simulated failure.
- **Dry-run**: assert generated command sequences match expectations.
- **Optional integration test** (root-gated, off by default): create and destroy a
  throwaway VLAN against a real interface.

## Location

A standalone Cargo crate at `/Users/zacharyheylmun/dev/luminar/vlanctl/`.
Specs live in `docs/specs/`, implementation plans in `docs/plans/`.

## Out of Scope

- Auto-detecting a connected sensor's VLAN/IP by sniffing traffic.
- A declarative reconcile/diff engine (down-then-up is sufficient).
- DHCP on VLAN interfaces (sensors use static addressing).
- GUI / menu-bar integration.
