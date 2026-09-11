# vlanctl

Apply and tear down named VLAN profiles on macOS for working with
differently-configured lidar sensors.

## Usage

```
vlanctl list                       # list profiles in ./profiles
vlanctl show <profile>             # print the commands a profile would run
vlanctl validate <profile>         # check a profile without applying
sudo vlanctl apply [profile]       # bring it up (defaults to the `lum` profile)
vlanctl apply [profile] --dry-run  # print commands without running them
sudo vlanctl apply lum --device eth0   # pin the parent adapter for this run
sudo vlanctl down                  # tear down the active profile
vlanctl status                     # what is currently up
```

A global `--profiles-dir <dir>` (default `profiles`) selects where profiles are
read from.

`apply` and `show` also take `--device <name>`, which picks the parent adapter
the VLANs attach to and overrides any `device` field in the profile. The parent
is **host-local** — macOS numbers adapters `enN` per machine, so a USB dongle
can be `en7` on one Mac and `en12` on another, while Linux uses
`eth0`/`enp*`/`enx*` — so the shipped profiles deliberately do not pin one.
Auto-detect takes the single active wired adapter and skips Wi-Fi (which cannot
carry 802.1Q VLANs); use `--device` when it is ambiguous or picks wrong.

## Library use

`vlanctl` is also a library. A consumer supplies a `CommandRunner` and a
`Platform`:

```rust
use vlanctl::{commands, config::Profile, net::RecordingRunner, plan::MacOs};

let profile = Profile::load("profiles/lum.toml".as_ref())?;
let mut runner = RecordingRunner::default();      // or SystemRunner to execute
// 4th argument overrides the parent device. Profiles do not pin one — it is
// host-local — so a consumer supplies it, or passes `None` to auto-detect
// against the live system.
commands::apply(&mut runner, &MacOs, &profile, Some("en7"), &state_path, true)?;
for cmd in &runner.commands {
    println!("{}", cmd.display());
}
```

Depend on it without the CLI's argument parser:

```toml
vlanctl = { git = "…", default-features = false }
```

## Profiles

Profiles live in `profiles/*.toml`, one file per profile. Each profile describes
one or more VLANs with their IP address and routes:

```toml
name = "example"
description = "Sample two-VLAN sensor profile"

# device = "en10"   # optional and discouraged: the parent adapter is
                    # host-local, so prefer `--device` on the command line.
                    # Omit to auto-detect.

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

### Routes

Each `[[vlan.route]]` is one of two kinds:

- **Gateway route** — has a `gateway`, emitted as `route add <destination> <gateway>`.
- **Interface-scoped route** — omit `gateway`, and the route is bound to that
  VLAN's own interface: a single host (`/32`) becomes
  `route add -host <ip> -interface vlanN`, anything else
  `route add -net <cidr> -interface vlanN`.

Interface-scoped routes are needed when several VLANs share a subnet (so a
sensor's traffic is pinned to the right interface) and for per-interface
multicast (e.g. SOME/IP-SD discovery):

```toml
[[vlan]]
id = 11
address = "192.168.10.87/24"

  [[vlan.route]]            # reach the sensor via this VLAN's interface
  destination = "192.168.10.151/32"

  [[vlan.route]]            # SOME/IP-SD multicast on this interface
  destination = "239.255.0.255/32"
```

See `profiles/example.toml` (gateway route) and `profiles/lum.toml` /
`profiles/lum_legacy.toml` (interface-scoped routes for real sensor setups).

## Notes

- Mutating commands (`apply`, `down`) modify network interfaces and must run
  under `sudo`. Read-only commands (`list`, `show`, `validate`, `status`) do not.
- `apply` tears down any currently-active profile first, then brings up the new
  one. If a step fails mid-way, it rolls back the interfaces it created.
- State (the active profile and the `vlanN` interfaces vlanctl created) is tracked
  at `/usr/local/var/vlanctl/state.json`. macOS assigns arbitrary `vlanN` unit
  numbers, so vlanctl records what it created in order to tear it down later.
- `--dry-run` previews the exact `ifconfig`/`route` commands without executing
  them and without touching the state file.
