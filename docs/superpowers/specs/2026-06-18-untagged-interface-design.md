# Untagged interface support in vlanctl

**Date:** 2026-06-18
**Status:** Approved design, pending implementation

## Problem

`vlanctl` is strictly VLAN-centric: a profile is a list of `[[vlan]]` entries, each
of which creates a `vlan<id>` sub-interface on the parent device. There is no way to
configure the **untagged** (native) path on the physical device itself.

The Halo A3 sensor needs both at once (see `~/Downloads/halo_vlan_up.sh`):

- **UDP data path — untagged.** Sensor `192.168.10.151`; host `192.168.1.100/24`
  (a *different* subnet), plus an on-link `/32` route to the sensor and the
  SOME/IP-SD multicast group `239.255.0.255`.
- **Telnet debug — VLAN 12.** Sensor `192.168.10.152`; host `192.168.10.1/24`.

Today only the tagged telnet path is expressible in a profile; the untagged data
path must be brought up by a separate shell script. This spec adds untagged support
so a single profile can express both.

## Goals

- Express an untagged parent-interface path (address + routes) in a profile.
- Keep tagged VLAN behaviour unchanged.
- `apply` of the untagged path is idempotent (safe to re-run; additive across profiles).
- `down` is simple: it only tears down the `vlan<id>` sub-interfaces vlanctl created.

## Non-goals

- Removing/cleaning up untagged parent config on `down` (it persists by design).
- Replicating Linux `ip route ... src <addr>` source-address pinning (see Limitations).
- Solving the genuine shared-subnet multi-VLAN routing case (separate effort).

## Design

### Schema (`config.rs`)

Replace `[[vlan]]` with `[[interface]]`, where the VLAN tag is an **optional** field.
An interface with no `vlan` is untagged and configures the parent device directly.

```toml
name = "halo"
device = "en16"

[[interface]]                      # untagged -> configure the parent device
address = "192.168.1.100/24"
  [[interface.route]]
  destination = "192.168.10.151/32"
  [[interface.route]]
  destination = "239.255.0.255/32"

[[interface]]
vlan = 12                          # tagged -> create vlan12 sub-interface
address = "192.168.10.1/24"
```

Struct changes:

```rust
pub struct Interface {
    pub vlan: Option<u16>,   // None = untagged (configure the parent device)
    pub address: IpNet,
    pub mtu: Option<u32>,
    pub routes: Vec<Route>,
}
```

`Profile.vlans: Vec<Vlan>` becomes `Profile.interfaces: Vec<Interface>`
(`#[serde(rename = "interface")]`). `Route` is unchanged.

Validation:

- At least one `[[interface]]` (unchanged intent).
- **At most one untagged** interface (`vlan = None`) per profile.
- VLAN id range (`1..=4094`) and uniqueness checks apply only to `Some` ids.
- IPv4-only address check unchanged.
- Route destination must be a CIDR or `"default"` (unchanged).

This is a **breaking profile-format change**. There is no back-compat alias for
`[[vlan]]`; the in-repo profiles (`lum`, `lum_legacy`, `halo`) are updated to the new
key as part of the work.

### Command generation (`plan.rs`)

Bring-up branches on `interface.vlan`:

**Tagged (`Some(id)`)** — unchanged from today:

```
ifconfig vlan<id> create
ifconfig vlan<id> vlan <id> vlandev <device>
ifconfig vlan<id> inet <addr> netmask <mask>
[ifconfig vlan<id> mtu <mtu>]
route add ...            # one per route (gateway or -interface vlan<id>)
```

**Untagged (`None`)** — configure the parent `<device>` directly, idempotently:

```
ifconfig <device> inet <addr> netmask <mask> alias    # only if not already present
[ifconfig <device> mtu <mtu>]
route add ...            # one per route, replace semantics (gateway or -interface <device>)
```

Routes keep the existing model: a gateway route when `gateway` is set, otherwise an
interface-scoped route via the owning interface (`vlan<id>` for tagged, `<device>`
for untagged).

Interface naming / state:

- Tagged interfaces record `vlan<id>` in state for teardown (unchanged).
- Untagged interfaces record **nothing** — fire-and-forget.

### Apply / state / down (`commands.rs`, `state.rs`)

- `apply` iterates interfaces. Tagged ones use the existing create → guard → rollback
  path and are recorded in state. Untagged ones run the idempotent parent-config
  commands and are **not** recorded.
- **Idempotency:** before adding the untagged alias, apply checks live device
  addresses and skips if already present; routes use replace semantics. Re-running
  the same profile is a safe no-op. Switching profiles leaves the prior untagged
  config in place (additive).
- The existing "refuse to touch an interface we did not create" guard applies **only
  to `vlan<id>` sub-interfaces**, never to the physical device.
- Rollback on a failed apply only destroys `vlan<id>` interfaces created in that run;
  it never reverts parent config.
- `down` and `state.rs` are **unchanged**: they only deal with the recorded
  `vlan<id>` list. `State.interfaces: Vec<String>` stays as-is.
- `show` / `validate` render the new branch (e.g. `vlanctl show halo` dry-runs both).

### Profiles

- `halo.toml`: untagged data path (`192.168.1.100/24` + `/32` route to `.151` +
  `239.255.0.255` multicast) and `vlan = 12` telnet (`192.168.10.1/24`).
- `lum.toml`, `lum_legacy.toml`: migrate `[[vlan]]` → `[[interface]]` with `vlan = <id>`;
  no behavioural change.

## Risks

- **macOS `-host ... -interface` for the out-of-subnet data sensor.** The untagged
  route to `192.168.10.151` (out-of-subnet relative to `192.168.1.100/24`) renders as
  `route add -host 192.168.10.151 -interface <device>`. macOS turns this into a
  permanent self-MAC ARP entry (black-hole) for *in-subnet* destinations; the
  out-of-subnet case is unvalidated on the working adapter. **Requires a hardware
  check during implementation.** If it black-holes, fall back to a static ARP entry
  (sensor `.151` MAC is `3a:42:f7:79:32:2e`, shared across its VLAN interfaces).

- **Adapter VLAN-TX.** Confirmed this session: some USB NICs silently drop 802.1Q
  tags on transmit under macOS (the `98:fc:84:ec:ea:f1` dongle / `en7`). The Realtek
  dongle (`00:e0:4c:...` / `en16`) works. Out of scope for this feature, but the
  hardware validation must run on a known-good adapter.

## Limitations (documented, not solved in v1)

- **No route source pinning.** `halo_vlan_up.sh` uses `ip route ... src 192.168.1.100`.
  macOS `route add` has no direct `src` equivalent; source is chosen from the egress
  interface's addresses, so a corp/primary address on the device may be picked
  instead. Workarounds: app-level `IP_BOUND_IF`, or keep the data alias as the
  device's only address.

## Testing

Unit tests in the existing style (`RecordingRunner`, no hardware):

- **config:** parse tagged + untagged; reject two untagged interfaces; tag range /
  uniqueness enforced only for `Some` ids; existing IPv4 / empty / route-destination
  checks still pass.
- **plan:** untagged emits `ifconfig <device> ... alias` (not `create` / `vlandev`)
  plus its routes; tagged output unchanged (regression).
- **commands:** apply records only `vlan<id>` (not the parent); a profile with only an
  untagged interface records an empty interface list; idempotent re-apply skips an
  already-present alias.

Hardware validation (manual, on the good adapter): `vlanctl apply halo` brings up both
paths; telnet to `192.168.10.152:23` connects; the untagged data route to
`192.168.10.151` reaches the sensor (resolve the `-host -interface` risk above).
