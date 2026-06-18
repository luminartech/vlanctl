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

### Best-effort commands (`net.rs`)

`arp -d <host>` returns a non-zero exit when no entry exists, but the existing
`SystemRunner` treats any non-zero exit as a hard failure (and apply rolls
back). To clear a possibly-absent stale entry without failing apply, `Cmd` gains
a `best_effort` flag:

```rust
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub best_effort: bool,   // run failure is ignored (not a hard error)
}
```

`Cmd::new(...)` sets `best_effort: false` (all existing call sites unchanged). Add
`Cmd::new_best_effort(program, args)` (or a `.best_effort()` builder) for the
`arp -d` command. `display()` is unchanged (the flag does not affect rendering);
`RecordingRunner` records best-effort commands like any other.

### Command generation (`plan.rs`)

In the per-route loop of `bringup_commands`, after the existing handling of a
gatewayless route, if the route has a `mac`, append a best-effort `arp -d`
(clears any stale/auto self-MAC entry) followed by the `arp -s`, using the
destination's bare host address:

```
route add -host 192.168.10.151 -interface en16    # existing (skipped if in-subnet)
arp -d 192.168.10.151                              # new, best-effort (no-op if absent)
arp -s 192.168.10.151 3a:42:f7:79:32:2e            # new
```

- The `arp -d`/`arp -s` pair is emitted whenever a gatewayless route carries a
  `mac`, ordered immediately after that route's own command(s). The existing
  in-subnet skip still governs whether the `route add` line is emitted; the
  `arp -s` line is what forces resolution for an out-of-subnet host.
- `mac` on a route with a gateway, or on a non-/32, never reaches command
  generation — validation rejects it first.

### Lifecycle (apply / down / state)

Mostly unchanged from the untagged-interface design:

- **Best-effort handling:** apply's command-run loops (both the tagged and
  untagged branches) must, on a command error, check `cmd.best_effort`: if set,
  log/ignore and continue; otherwise roll back as today. This lets the
  `arp -d` no-op when no entry exists.
- **Idempotency:** apply skips an untagged interface entirely when its alias
  address is already configured, so its `arp -d`/`arp -s` do not re-run on
  re-apply.
- **down / state:** untagged config (including its ARP entry) persists; `down`
  only destroys recorded `vlan<id>` interfaces, which drops their ARP entries.

### Profile

`halo.toml`: add `mac = "3a:42:f7:79:32:2e"` to the `192.168.10.151/32` route.

## Risk / validation checkpoint

The defensive `arp -d` before `arp -s` clears any auto/stale self-MAC entry, so
the static entry should be the sole one at the real MAC. Hardware validation
must still confirm `arp -an` shows `.151` at `3a:42:f7:79:32:2e` as a single
entry and the data path reaches the sensor.

## Testing

Unit tests (existing `RecordingRunner` style, no hardware):

- **config:** a gatewayless `/32` route with a valid `mac` parses; reject `mac`
  with a gateway; reject `mac` on a non-/32 (e.g. `/24` or `"default"`); reject a
  malformed `mac` string.
- **plan:** a gatewayless `/32` route with `mac` emits the `route add -host ...
  -interface <iface>` line, then a best-effort `arp -d <host>`, then
  `arp -s <host> <mac>`; a route without `mac` emits no `arp` command
  (regression).
- **net:** a `best_effort` command whose run fails does not error apply (e.g. via
  `RecordingRunner` with `fail_at` pointed at the `arp -d`, the apply still
  succeeds and proceeds to `arp -s`).

Hardware validation (manual, on a good adapter): `vlanctl apply halo`, confirm
`arp -an` shows `.151` at `3a:42:f7:79:32:2e` (single entry, not self-MAC), and
the untagged data path reaches the sensor.
