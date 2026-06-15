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

A global `--profiles-dir <dir>` (default `profiles`) selects where profiles are
read from.

## Profiles

Profiles live in `profiles/*.toml`, one file per profile. Each profile describes
one or more VLANs with their IP address and routes:

```toml
name = "example"
description = "Sample two-VLAN sensor profile"

# device = "en10"   # optional: pin the physical adapter; omit to auto-detect

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

See `profiles/example.toml`.

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
