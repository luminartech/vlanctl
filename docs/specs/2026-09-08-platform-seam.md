# The `Platform` seam

**Date:** 2026-09-08

## `vlanctl` is now a library plus a CLI

`vlanctl` started as a macOS-only binary: it read a profile, and shelled out
to `ifconfig`, `route`, and `networksetup` to bring VLAN interfaces up or
down. That logic now lives in a library target (`src/lib.rs`), and the
binary (`src/main.rs`) is a thin consumer of it — argument parsing and a
root check, nothing more. A host application can depend on the library
directly and drive the same profile-apply/tear-down/status logic in-process,
without shelling out to the `vlanctl` binary or parsing its output.

The CLI itself is now optional. `clap` sits behind a `cli` feature that is
on by default, so `cargo build`/`cargo run` behave exactly as before, but a
library consumer can pull in `vlanctl` with `default-features = false` and
get no argument parser at all. Nothing about the library's public API
depends on `clap` or on any CLI type.

## `Platform` owns interface syntax and two decisions — not route syntax yet

A `Platform` trait (`src/plan.rs`) now owns the interface-naming convention
and the ordered commands to bring an interface up or tear it down. Each
implementation supplies its own syntax for those: `MacOs` renders `ifconfig
vlan10 create` / `vlan 10 vlandev en7`; a future Linux backend would render
its own `ip link add ...` instead.

Route and static-ARP command syntax **is** behind this seam, as of
`Platform::route_commands`. It was not originally: `append_route_commands`
and `interface_route_command` built every `route add ...` and `arp -s ...`
command in shared, BSD/macOS-shaped code for every platform, and only the
*decision* of whether to emit a host route was delegated. That was gap (1)
below, now closed. `append_route_commands` still exists but is
`#[cfg(test)]`, retained as the independent baseline the equivalence test
compares `MacOs::route_commands` against — production emission goes through
the trait.

One of the trait's methods is not syntax at all. It is a decision, and it is
a decision because the straight-line logic it replaced looked like a
universal networking rule but was actually encoding one operating system's
semantics:

- **`reverts_parent_config() -> bool`** — whether tearing a profile down may
  undo configuration applied to the *parent* network device, as opposed to
  a VLAN sub-interface `vlanctl` created and owns outright. An untagged
  profile entry on macOS aliases an address directly onto the physical
  device; macOS deliberately answers `false` here and leaves that alias in
  place on teardown, because the device may be carrying configuration
  `vlanctl` did not create and has no business removing. A Windows backend
  cannot inherit that answer: even though `netsh` can bind more than one
  IPv4 address to a single physical adapter, `vlanctl`'s untagged equivalent
  on Windows is a virtual-switch binding that re-plumbs the physical NIC
  itself, not an address alias, so leaving it in place on teardown would
  leave the network stack in a different state than before `vlanctl` touched
  it. A Windows `Platform` must answer `true` and actually revert it.

Both methods are answered per platform for the same underlying reason:
the operation that looks identical in shape (add a host route; leave/undo
parent config) has a materially different effect on the underlying network
stack, and only the platform backend knows which effect is correct.

## `Platform` also declares host-state parsing — not yet wired up

The trait also declares four host-state readers: `list_devices`,
`addresses_on`, `is_wireless`, and `link_is_active`. On macOS they parse the
output of `ifconfig` and `networksetup -listallhardwareports`, tools with no
Linux or Windows equivalent, so each backend will need its own parser rather
than a name that happens to compile against another OS's tools.

None of the four has a production caller yet, though. `apply`, `down`,
`status`, and `resolve_device` still call the macOS parsers in `commands`
(`live_interfaces`, `device_inet_addresses`) and `device` (`wifi_devices`,
`interface_is_active`) directly — exactly as they did before this trait
existed. Routing those call sites through `Platform` is host-detection work
in its own right: a wrong parser there silently picks the wrong NIC rather
than erroring, so it is deliberately left as the first task for whichever
change adds the first non-macOS backend, not folded into declaring the
trait's shape.

This is also why no method on `Platform` has a default implementation. That
omission is deliberate, not an oversight: a default body is itself an
inherited behavior, and the entire point of the trait is that a backend
should never get a for-free implementation of another operating system's
networking rules. Every implementation, including `MacOs`, spells out every
method, and adding a new trait method is a compile error for every backend
until it decides its own answer.

## `MacOs` is the pre-existing behavior, unchanged

The `MacOs` implementation of `Platform` is the extraction of what `vlanctl`
already did before this seam existed, moved behind the trait without
changing it. Its route-skipping and parent-config-preserving answers are the
`false`/`false` half of the two decisions above, expressed as the trait
requires rather than as free-standing logic. Equivalence with the
pre-existing behavior is what the test suite establishes: the commands
`vlanctl` renders for macOS today are the same commands it rendered before
the trait existed.

## `require_root` stays in the binary

Root elevation is checked in `src/main.rs`, not in the library. That split
exists because "must run as root" is a CLI-shaped assumption, not a library
one: a process embedding `vlanctl` as a library may already hold the
specific Linux capability it needs (`CAP_NET_ADMIN`) without running as
root at all, and a blanket root check baked into the library would refuse
to let it apply a profile even though it has the exact privilege the
underlying `ifconfig`/`ip`/route-table operations require. The library
performs no privilege check of its own; it is the caller's responsibility to
ensure the process has whatever privilege the underlying OS calls demand,
and the CLI's `require_root` is simply the check appropriate to *that*
caller.

## Linux and Windows backends are the expected next steps

This work adds the seam, not the additional backends. A Linux `Platform`
implementation and a Windows `Platform` implementation are the anticipated
next pieces of work.

**The seam is not yet complete.** Anyone writing a backend should read this
list first; an earlier draft of this document claimed there were only two
gaps, which was wrong. Gaps are kept numbered as first published — a closed
one is marked CLOSED rather than removed, both because the reasoning is what
a Windows backend author needs and because other documents cite these
numbers. As of the Linux backend (`feat/1b-linux-backend`), the two SILENT
gaps (3) and (4) are closed, gap (1) is closed, and a new one (7) is open.

1. **CLOSED. Was: route and ARP syntax was shared and BSD-shaped.**
   `Platform` delegated the *decision* (`wants_onlink_host_route`) but not
   the syntax: `append_route_commands` and `interface_route_command` emitted
   `route add -host/-net … -interface` and `arp -s` for every platform, so a
   non-BSD backend rendered unusable commands.
   Now `Platform::route_commands(&Route, in_subnet, iface) -> Vec<Cmd>` owns
   both the decision and the syntax: `MacOs` renders BSD `route`/`arp -s`
   and skips the in-subnet host route, `Linux` renders `ip route`/`ip neigh`
   and emits it. `wants_onlink_host_route` is **gone** — it could express
   whether to route but not how, which is why it could not close this gap on
   its own. The pin
   `route_syntax_is_currently_shared_and_bsd_shaped_a_known_limitation` is
   deleted; it asserted the behavior this change fixes.
2. **The four host-state methods are declared but unconsumed.**
   `apply`/`down`/`status`/`resolve_device` still call the macOS parsers
   directly.
3. **CLOSED. Was SILENT: `apply` decided what to record by matching
   `ifconfig` argument shape.** A created interface was recorded only when
   the emitted command's second argument was literally `create`. Under a
   Linux-shaped `ip link add link eth0 name eth0.11 type vlan id 11` that
   argument is `link`, so nothing was recorded: rollback destroyed nothing,
   the state file stayed empty, and `down` became a no-op.
   Now `Platform::records_created_interface(&Cmd) -> bool`, so each backend
   declares which of its own commands is the creating one.
4. **CLOSED. Was SILENT: the pre-apply collision guard hardcoded the macOS
   interface name.** `interface_names()` built `format!("vlan{id}")`
   independently of `Platform::iface_name`, so with a backend naming
   interfaces `eth0.11` the guard looked for `vlan11`, never matched, and
   never fired — `apply` would not refuse to touch a pre-existing interface
   it did not create.
   Now `apply` builds the guard list itself, from the tagged entries mapped
   through `platform.iface_name(i, &device)` on the **resolved** device it
   already holds. Deliberately *not* a trait method: a second naming entry
   point is the thing that caused this bug, so guard/bring-up agreement is
   structural rather than comment-enforced. `plan::interface_names` survives
   only as `#[cfg(test)]`, documented as macOS's naming and not to be used
   for a guard.
5. **Device candidate selection is macOS-shaped.** `resolve_device` filters
   candidates by an `en`-prefixed name, which is not a parser and so is not
   covered by the host-state methods above.
6. Minor: the CLI's read-only-probe classification and its dry-run
   `ifconfig -l` seed are macOS-specific and sit outside `Platform`.
7. **NEW, and now user-visible: preview and apply disagree on Linux.**
   `preview_platform()` is fixed at `MacOs` while `host_platform()` resolves
   `Linux`, so on a Linux host `vlanctl apply lum --dry-run` prints
   `ifconfig vlan10 create …` while the real `apply` would run `ip link add
   …`. Verified on this branch. It was harmless while no non-macOS backend
   existed; it is not now. The repoint is not a one-liner — the two return
   different shapes (`&'static dyn Platform` vs `Result<Box<dyn Platform>>`)
   — which is why it was deferred rather than folded into the backend.
   Related: (2) leaves `apply`/`down` calling `ifconfig -l` through
   `commands::live_interfaces`, so a real `apply` on Linux fails loudly on
   that call before mutating anything.

Once (2) is closed, each backend supplies its own interface/route command
syntax, its own answers to `route_commands` and `reverts_parent_config`,
and its own host-state parsing, exactly as `MacOs` does today.

`host_platform()` (`src/plan.rs`) is the single place a binary selects a
`Platform` for a **real, mutating** operation (`apply`/`down`); it returns
`MacOs` on macOS, `Linux` on Linux, and an `Err` — never a panic —
everywhere else. Adding an arm there is what brings a new platform online.

Rendering a **preview** (`show`, `apply --dry-run`, `down --dry-run`) goes
through the separate `preview_platform()` instead, which always succeeds so
that a preview keeps working on any host even before a real backend exists
for it. Two caveats, both of which an earlier draft of this document got
wrong by asserting that a preview touches no real system:

- `show` runs `resolve_device` against a **real** runner, so a preview does
  read host state even though it mutates nothing.
- Because `preview_platform()` is still fixed at `MacOs`, a preview no
  longer agrees with the apply it is previewing on any host with a backend
  of its own. See gap (7).

## Cross-reference

The design that motivated this seam — including why `vlanctl` is being
adopted as the foundation for cross-platform host network autoconfiguration
rather than reimplemented, and the fuller rationale behind treating the
in-subnet host-route question as a decision rather than as syntax (it was
`wants_onlink_host_route` there, now folded into `route_commands`) — lives at
`docs/superpowers/specs/2026-09-08-cross-platform-host-network-autoconfig-design.md`
**in this repository**, §0 and §4.3.1. (It was authored in the `dft` repo,
where that path is git-ignored; the copy here is the tracked one, and §4.3.1
is also where the Linux IFNAMSIZ naming rule is recorded.)
