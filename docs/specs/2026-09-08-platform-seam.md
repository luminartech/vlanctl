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

Route and static-ARP command syntax is **not** behind this seam, and should
not be assumed to be. `append_route_commands` and `interface_route_command`
in `src/plan.rs` build every `route add ...` and `arp -s ...` command in
shared code, for every platform, and that syntax is BSD/macOS-shaped. Only
the *decision* of whether to emit a host route is delegated
(`wants_onlink_host_route`, below) — the syntax that decision controls is
not. A non-BSD backend (Linux's `ip route`, for instance) will need a
per-platform route-emission hook added to `Platform` before it can render
correct route commands. `src/plan.rs`'s
`route_syntax_is_currently_shared_and_bsd_shaped_a_known_limitation` test
pins today's shared-BSD behavior so that closing this gap later is a
deliberate test edit rather than silent drift; treat that hook as the first
piece of the Linux/Windows backend work described below, not something
already in place.

Two of the trait's methods that do exist today are not syntax at all. They
are decisions, and they are decisions because the straight-line logic they
replace looked like a universal networking rule but was actually encoding
one operating system's semantics:

- **`wants_onlink_host_route(in_subnet: bool) -> bool`** — whether a
  gatewayless route to a destination inside the interface's own subnet
  should get an explicit interface-scoped host route. On macOS, adding such
  a route for an in-subnet destination installs a self-MAC ARP (LLINFO)
  entry and black-holes traffic to that host; macOS relies on the
  already-connected route to resolve it, so `vlanctl` must *skip* emitting
  one. On Linux the same operation (`ip route add <host>/32 dev <iface>`) is
  not just harmless, it is the only way to reach several hosts that share
  one subnet but sit behind different VLAN interfaces — omitting it there
  would make those hosts unreachable rather than merely redundant. A single
  hardcoded answer is wrong on one of the two platforms no matter which way
  it is hardcoded, which is why this has to be a per-`Platform` question
  rather than a constant.

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

**The seam is not yet complete, and two of the remaining gaps fail
silently.** Anyone writing a backend should read this list first; an earlier
draft of this document claimed there were only two gaps, which was wrong.

1. **Route and ARP syntax is still shared and BSD-shaped.** `Platform`
   delegates the *decision* (`wants_onlink_host_route`) but not the syntax:
   `append_route_commands` and `interface_route_command` emit
   `route add -host/-net … -interface` and `arp -s` for every platform.
   `Platform` needs a per-platform route-emission hook. Pinned by
   `route_syntax_is_currently_shared_and_bsd_shaped_a_known_limitation`,
   which is expected to fail — and must be rewritten — when that hook lands.
2. **The four host-state methods are declared but unconsumed.**
   `apply`/`down`/`status`/`resolve_device` still call the macOS parsers
   directly.
3. **SILENT: `apply` decides what to record by matching `ifconfig`
   argument shape.** In `commands.rs`, a created interface is recorded only
   when the emitted command's second argument is literally `create`. Under a
   Linux-shaped `ip link add link eth0 name eth0.11 type vlan id 11`, that
   argument is `link`, so **nothing is recorded: rollback destroys nothing,
   the state file stays empty, and `down` becomes a no-op.** A backend that
   does not emit `ifconfig … create` must change this test, or it silently
   loses teardown and rollback entirely.
4. **SILENT: the pre-apply collision guard hardcodes the macOS interface
   name.** `interface_names()` builds `format!("vlan{id}")` independently of
   `Platform::iface_name`. With a backend naming interfaces `eth0.11`, the
   guard looks for `vlan11`, never matches, and **never fires** — so `apply`
   will not refuse to touch a pre-existing interface it did not create.
5. **Device candidate selection is macOS-shaped.** `resolve_device` filters
   candidates by an `en`-prefixed name, which is not a parser and so is not
   covered by the host-state methods above.
6. Minor: the CLI's read-only-probe classification and its dry-run
   `ifconfig -l` seed are macOS-specific and sit outside `Platform`.

Once (1)-(4) at least are closed, each backend supplies its own
interface/route command syntax, its own answers to
`wants_onlink_host_route` and `reverts_parent_config`, and its own
host-state parsing, exactly as `MacOs` does today.

`host_platform()` (`src/plan.rs`) is the single place a binary selects a
`Platform` for a **real, mutating** operation (`apply`/`down`); it currently
returns `MacOs` on macOS and returns an `Err` — never a panic — everywhere
else, which is what a new backend needs to change to bring that platform
online. Rendering a **preview** (`show`, `apply --dry-run`, `down
--dry-run`) goes through the separate `preview_platform()` instead, which
always succeeds: a preview touches no real system and must keep working on
any host even before a real backend exists for it, so it is not gated on
host detection the way a mutating operation is.

## Cross-reference

The design that motivated this seam — including why `vlanctl` is being
adopted as the foundation for cross-platform host network autoconfiguration
rather than reimplemented, and the fuller rationale behind treating
`wants_onlink_host_route` as a decision rather than as syntax — lives at
`docs/superpowers/specs/2026-09-08-cross-platform-host-network-autoconfig-design.md`
in the `dft` repository, §0 and §4.3.1.
