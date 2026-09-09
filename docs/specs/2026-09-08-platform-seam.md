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

## `Platform` owns syntax and two decisions

Every command the library used to hardcode as `ifconfig`/`route` syntax now
comes from a `Platform` trait (`src/plan.rs`). A `Platform` implementation
supplies the interface-naming convention, the ordered commands to bring an
interface up or tear it down, and the tools used to read back host state —
this is the part that varies "in size," so to speak: it's still command
syntax, one dialect per operating system.

But two of the trait's methods are not syntax at all. They are decisions,
and they are decisions because the straight-line logic they replace looked
like a universal networking rule but was actually encoding one operating
system's semantics:

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
  cannot inherit that answer: Windows has no equivalent of an address alias
  on a physical adapter. Its untagged equivalent is a virtual-switch binding
  that re-plumbs the physical NIC itself, so leaving it in place on teardown
  would leave the network stack in a different state than before `vlanctl`
  touched it. A Windows `Platform` must answer `true` and actually revert
  it.

Both methods are answered per platform for the same underlying reason:
the operation that looks identical in shape (add a host route; leave/undo
parent config) has a materially different effect on the underlying network
stack, and only the platform backend knows which effect is correct.

## `Platform` also owns host-state parsing

The trait does not stop at command emission. `list_devices`, `addresses_on`,
`is_wireless`, and `link_is_active` all read back host network state, and on
macOS they do it by parsing the output of `ifconfig` and
`networksetup -listallhardwareports`. Those are macOS-specific tools with
macOS-specific output formats; a Linux or Windows backend has no `ifconfig`
or `networksetup` to shell out to; it has its own state-reading conventions
entirely. Routing state-reading through `Platform` means a new backend must
supply its own parser rather than silently inheriting macOS's and either
failing to compile against tools that do not exist on that OS, or worse,
compiling against them and producing wrong answers at runtime.

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
next pieces of work; each will supply its own command syntax, its own
answers to `wants_onlink_host_route` and `reverts_parent_config`, and its
own host-state parsing, exactly as `MacOs` does today. `host_platform()`
(`src/plan.rs`) is the single place a binary selects a `Platform` for the
host it is running on; it currently returns `MacOs` on macOS and refuses
(rather than guessing) everywhere else, which is what a new backend needs
to change to bring that platform online.

## Cross-reference

The design that motivated this seam — including why `vlanctl` is being
adopted as the foundation for cross-platform host network autoconfiguration
rather than reimplemented, and the fuller rationale behind treating
`wants_onlink_host_route` as a decision rather than as syntax — lives at
`docs/superpowers/specs/2026-09-08-cross-platform-host-network-autoconfig-design.md`
in the `dft` repository, §0 and §4.3.1.
