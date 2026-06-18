# Static ARP entries for routes in vlanctl

**Date:** 2026-06-18
**Status:** Approved design, pending implementation
**Builds on:** `2026-06-18-untagged-interface-design.md`

## Problem

The Halo untagged data path reaches the sensor at `192.168.10.151`, which is
*out of subnet* relative to the host's `192.168.1.100/24`. On macOS the
gatewayless route to it renders as `route add -host 192.168.10.151 -interface en16`,
and macOS turns that into a **permanent self-MAC ARP entry** (the destination is
recorded at the host's own MAC), black-holing the path. Confirmed on hardware
(adapter `en16`):

```
192.168.10.151  0:e0:4c:68:a:1d  UHLS  en16            (route gw = our MAC)
? (192.168.10.151) at 0:e0:4c:68:a:1d on en16 permanent (our own MAC)
```

The sensor does not answer ARP on this path, but its MAC is known
(`3a:42:f7:79:32:2e`, shared across its interfaces). A static ARP entry makes
frames egress to the real MAC (proven earlier in the session). This spec adds a
way to declare that static ARP entry in a profile.

## Goals

- Let a route declare a static ARP (`mac`) for an on-link host the kernel can't
  resolve itself, so the Halo untagged data path reaches `.151`.
- Keep all existing route behaviour unchanged when `mac` is absent.

## Non-goals

- Dynamic MAC discovery (harvesting from SOME/IP-SD, etc.).
- Static ARP for anything but a single host (no network/multicast entries).
- Removing static ARP entries on `down` for the untagged path (it persists, per
  the untagged-interface design).

## Design

### Schema (`config.rs`)

Add an optional `mac` field to `Route`:

```rust
#[derive(Debug, Deserialize, PartialEq)]
pub struct Route {
    pub destination: String,
    #[serde(default)]
    pub gateway: Option<IpAddr>,
    #[serde(default)]
    pub mac: Option<String>,
}
```

Profile usage:

```toml
[[interface.route]]
destination = "192.168.10.151/32"
mac = "3a:42:f7:79:32:2e"
```

Validation — when `mac` is `Some`:

- `gateway` must be `None` (a static ARP entry and a gateway next-hop are
  mutually exclusive). Otherwise bail.
- `destination` must parse as a single host: an `IpNet` whose
  `prefix_len() == max_prefix_len()` (a `/32`). Otherwise bail (`"default"` and
  any non-/32 are rejected).
- `mac` must be a syntactically valid 48-bit MAC: six colon-separated
  groups of one or two hexadecimal digits (e.g. `3a:42:f7:79:32:2e`).
  Otherwise bail.

`mac = None` is unchanged behaviour.

### Command generation (`plan.rs`)

In the per-route loop of `bringup_commands`, after the existing handling of a
gatewayless route, if the route has a `mac`, append a static-ARP command using
the destination's bare host address:

```
route add -host 192.168.10.151 -interface en16    # existing (skipped if in-subnet)
arp -s 192.168.10.151 3a:42:f7:79:32:2e            # new, when mac is set
```

- The `arp -s` is emitted whenever a gatewayless route carries a `mac`, ordered
  immediately after that route's own command(s). The existing in-subnet skip
  still governs whether the `route add` line is emitted; the `arp -s` line is
  what forces resolution for an out-of-subnet host.
- `mac` on a route with a gateway, or on a non-/32, never reaches command
  generation — validation rejects it first.

### Lifecycle (apply / down / state)

Unchanged from the untagged-interface design:

- **Idempotency:** apply skips an untagged interface entirely when its alias
  address is already configured, so its `arp -s` does not re-run on re-apply.
- **down / state:** untagged config (including its ARP entry) persists; `down`
  only destroys recorded `vlan<id>` interfaces, which drops their ARP entries.

### Profile

`halo.toml`: add `mac = "3a:42:f7:79:32:2e"` to the `192.168.10.151/32` route.

## Risk / validation checkpoint

macOS may auto-install a self-MAC ARP entry for the `-host ... -interface`
route. Hardware validation must confirm that `arp -s` yields a **single** entry
at the real MAC (`3a:42:f7:79:32:2e`), not a duplicate alongside a stale
self-MAC entry. If a stale entry remains, the fix is a best-effort
`arp -d <host>` emitted immediately before `arp -s <host> <mac>` (tolerating its
failure when no entry exists). This is the one open implementation risk; resolve
it during the hardware check before merge.

## Testing

Unit tests (existing `RecordingRunner` style, no hardware):

- **config:** a gatewayless `/32` route with a valid `mac` parses; reject `mac`
  with a gateway; reject `mac` on a non-/32 (e.g. `/24` or `"default"`); reject a
  malformed `mac` string.
- **plan:** a gatewayless `/32` route with `mac` emits the `route add -host ...
  -interface <iface>` line followed by `arp -s <host> <mac>`; a route without
  `mac` emits no `arp` command (regression).

Hardware validation (manual, on a good adapter): `vlanctl apply halo`, confirm
`arp -an` shows `.151` at `3a:42:f7:79:32:2e` (single entry, not self-MAC), and
the untagged data path reaches the sensor.
