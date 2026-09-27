# vlanctl

Apply and tear down named VLAN profiles for working with differently-configured
lidar sensors. Supports macOS (`ifconfig`/`route`), Linux (`ip`) and Windows
(Hyper-V virtual adapters plus `netsh`); the backend is chosen from the host at
run time.

## Installation

```
cargo install vlanctl
```

Profiles are read from `--profiles-dir` (default `./profiles`). The crate ships
`profiles/example.toml` as a starting point; the sensor profiles used on our
benches are bench configuration and live in the project repository rather than
the published crate.

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
read from. `apply` with no profile argument defaults to the name `lum`; supply
your own `profiles/lum.toml`, or name a profile explicitly.

`apply` and `show` also take `--device <name>`, which picks the parent adapter
the VLANs attach to and overrides any `device` field in the profile. The parent
is **host-local** — macOS numbers adapters `enN` per machine, so a USB dongle
can be `en7` on one Mac and `en12` on another, while Linux uses
`eth0`/`enp*`/`enx*` and Windows uses the adapter's display name (`Ethernet 2`)
— so the shipped profiles deliberately do not pin one. Auto-detect takes the
single active wired adapter and skips Wi-Fi (which cannot carry 802.1Q VLANs);
use `--device` when it is ambiguous or picks wrong. On Windows there is no
auto-detect and `--device` is required — see below for why.

### Windows

Windows has no general 802.1Q sub-interface, so the Windows backend uses
Hyper-V: it binds the parent adapter to an external virtual switch named
`vlanctl` and creates one management-OS virtual adapter per `[[interface]]`
entry, in access mode for that entry's VLAN. Addresses, routes and static
neighbor entries are set with `netsh`. What that means in practice:

- **Hyper-V must be enabled** (`Enable-WindowsOptionalFeature -Online
  -FeatureName Microsoft-Hyper-V-All`, which reboots). It is available on
  Windows 10/11 Pro, Enterprise and Education and on Windows Server, not on
  Home. vlanctl never enables it.
- **`apply` and `down` need an elevated shell** (Run as administrator), the
  Windows equivalent of `sudo`.
- **Binding the parent takes it away from the host.** While the switch is up
  the parent adapter has no addressing of its own, so pointing vlanctl at the
  machine's uplink disconnects the machine until `down`. That is why `--device`
  is required on Windows: vlanctl will not guess.
- **Interface names are the adapters Windows creates**: `vEthernet (vlan11)`
  for VLAN 11, `vEthernet (untagged)` for an untagged entry. Because the
  parent's own stack is gone once bound, an untagged entry is a virtual adapter
  too, and unlike on macOS and Linux it is removed on `down`.
- **Everything persists across a reboot** — the switch, its adapters, their
  addresses and routes. One thing does not recover by itself: if the parent
  adapter is absent at boot (a dock that did not enumerate, a USB NIC that was
  unplugged), Hyper-V leaves the switch unbound and does not rebind it when the
  adapter returns. `vlanctl down` then `vlanctl apply` recovers it.
- The Hyper-V cmdlets run through `powershell.exe -Command`, which is not
  subject to script execution policy (no script file is involved), and
  `netsh` is a plain executable, so nothing here needs signing or a policy
  change.

## Library use

`vlanctl` is also a library. A consumer supplies a `CommandRunner` and a
`Platform`:

```rust
use vlanctl::{commands, config::Profile, net::RecordingRunner, plan::MacOs};

let profile = Profile::load("profiles/example.toml".as_ref())?;
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
vlanctl = { version = "0.1.0", default-features = false }
```

## Profiles

Profiles live in `profiles/*.toml`, one file per profile. Each profile describes
one or more interfaces with their IP address and routes:

```toml
name = "example"
description = "Sample two-VLAN sensor profile"

# device = "en10"   # optional and discouraged: the parent adapter is
                    # host-local, so prefer `--device` on the command line.
                    # Omit to auto-detect.

[[interface]]
vlan = 100
address = "192.168.10.2/24"

  [[interface.route]]
  destination = "192.168.20.0/24"
  gateway = "192.168.10.1"

[[interface]]
vlan = 200
address = "10.0.0.5/24"
mtu = 1500
```

Each `[[interface]]` takes:

- **`vlan`** — the 802.1Q tag. **Optional**: omit it for an *untagged* interface,
  and vlanctl configures the parent device directly instead of creating a VLAN
  sub-interface. On macOS and Linux an untagged interface is left configured on
  `down`, since vlanctl did not create the device; on Windows it is a virtual
  adapter vlanctl created, and is removed.
- **`address`** — required, in CIDR form.
- **`mtu`** — optional.
- **`[[interface.route]]`** — zero or more routes, described below.

### Routes

Each `[[interface.route]]` is one of two kinds:

- **Gateway route** — has a `gateway`, and is emitted as a next-hop route.
- **Interface-scoped route** — omit `gateway`, and the route is bound to that
  interface instead: a single host (`/32`) is emitted as a host route, anything
  else as a network route.

The exact commands are the host backend's business — macOS renders
`route add -host <ip> -interface vlan11`, Linux `ip route add <ip> dev eth0.11`,
Windows `netsh interface ipv4 add route <ip>/32 "vEthernet (vlan11)"`.
`vlanctl show <profile>` prints what would run on the current host.

Interface-scoped routes are needed when several VLANs share a subnet (so a
sensor's traffic is pinned to the right interface) and for per-interface
multicast (e.g. SOME/IP-SD discovery):

```toml
[[interface]]
vlan = 11
address = "192.168.10.87/24"

  [[interface.route]]            # reach the sensor via this VLAN's interface
  destination = "192.168.10.151/32"

  [[interface.route]]            # SOME/IP-SD multicast on this interface
  destination = "239.255.0.255/32"
```

A gatewayless `/32` route may also carry a **`mac`**, which adds a static ARP
entry (`arp -s <host> <mac>`) for an on-link host that does not answer ARP
itself:

```toml
  [[interface.route]]
  destination = "192.168.10.151/32"
  mac = "00:00:5e:00:53:01"
```

See `profiles/example.toml`, which uses a gateway route; the interface-scoped
form above is what real sensor profiles use.

## Notes

- Mutating commands (`apply`, `down`) modify network interfaces and must run
  under `sudo`, or from an elevated shell on Windows. Read-only commands
  (`list`, `show`, `validate`, `status`) do not.
- `apply` tears down any currently-active profile first, then brings up the new
  one. If a step fails mid-way, it rolls back the interfaces it created.
- State (the active profile and the interfaces vlanctl created) is tracked at
  `/usr/local/var/vlanctl/state.json` on macOS and Linux and at
  `%ProgramData%\vlanctl\state.json` on Windows. macOS assigns arbitrary `vlanN`
  unit numbers, so vlanctl records what it created in order to tear it down
  later.
- `--dry-run` previews the exact commands without executing them and without
  touching the state file.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
