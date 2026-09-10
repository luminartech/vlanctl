# Automatic host network configuration for Iris sensors

**Status:** design, **revision 2** (2026-09-08). Spike complete; no production code written.
**Revision 2 adopts `luminartech/vlanctl` as the foundation** rather than
building a parallel implementation — see §0. Revision 1's `NetworkIntent` /
`Plan` / `Step` types are withdrawn; vlanctl's `Profile` / `Interface` /
`Route` / `Cmd` replace them.
**Scope:** EnVision Next (`envision_drivers`, `envision_ui`, `envision`). Linux, Windows, macOS.

## 0. We are adopting `luminartech/vlanctl`

`vlanctl` is an existing Rust crate that already does macOS VLAN bring-up,
profile-driven, with the same plan/apply/down/status shape revision 1 was
designing from scratch. Decision (2026-09-08): **adopt it as the foundation.**

What it already has, and we therefore do not build:
- A TOML **profile** schema — `Profile { name, description, device,
  interfaces }`, `Interface { vlan: Option<u16>, address: IpNet, mtu, routes }`,
  `Route { destination, gateway: Option<IpAddr>, mac: Option<String> }`, with
  `Profile::load` + `Profile::validate`.
- A platform-neutral **`Cmd` + `CommandRunner`** seam (`src/net.rs`), with a
  `RecordingRunner` that powers `--dry-run` *and* the tests, including a
  `fail_at` hook for exercising rollback.
- `plan.rs`, which turns a profile into an ordered `Vec<Cmd>`; `state.rs`
  tracking the active profile; `device.rs` / `commands.rs` reading host state.
- Working profiles for real hardware: `lum` (Iris), `lum_legacy`, `halo`,
  `setup`.
- **`docs/vlan-sensor-reachability.md`** — the best account of this problem
  anywhere, and the source of most of §2.1 and §8 below.

What it does **not** have, which is what this design adds:
1. **It is macOS-only.** No `cfg(target_os)` anywhere; it shells to `ifconfig`
   and `networksetup -listallhardwareports` unconditionally. Linux and Windows
   need backends.
2. **It is a binary, not a library** — no lib target, so EnVision cannot
   depend on it. Adding one is the first task.
3. **It has no observation.** Profiles are hand-authored. EnVision needs to
   *derive* a profile from what is actually on the wire, because a
   reprovisioned sensor does not match any static profile (§4.2).

So the split of work is: **vlanctl gains a lib target, a platform seam, and
Linux/Windows backends; EnVision gains observation and a UI, and depends on
vlanctl for everything else.**

## 1. Problem

An operator plugs in an Iris sensor and EnVision does not see it, because the
host has no network interface on the sensor's subnets. Today the fix is manual:
read the Iris Quickstart Guide, identify the right NIC, and hand-configure VLAN
sub-interfaces, addresses and a multicast route. The user guide states outright
that EnVision "won't reach into your host or switch config to fix them."

On Linux that manual step must be repeated **every boot** — `ip link add ...
type vlan` does not persist. Windows cannot create VLAN sub-interfaces at all
through any documented OS mechanism.

Goal: EnVision detects what the host needs, explains it, and — with the
operator's consent — applies it.

## 2. The model: tagging is per-path, not per-sensor

This is the fact that invalidates most first guesses, including several made
early in the spike.

| Path | Transport | Tagging | Sensor address | Host / client address |
|---|---|---|---|---|
| Point cloud / datapath | UDP 4370/4371 | **ALWAYS UNTAGGED** | sourced from the SOME/IP address | destination defaults to **`192.168.11.87`** (or a group) |
| Diagnostics (DoIP/UDS) | UDP+TCP 13400 | **VLAN 10** | `192.168.10.150` | **`192.168.10.90`** |
| SOME/IP (SD + control) | UDP 30490 + service ports | **VLAN 11** | `192.168.11.151` | `192.168.11.87` |
| Telnet | TCP | **VLAN 12** on Iris; **untagged** on Halo | `192.168.12.152` | `192.168.10.1/24` |

Addresses above are the **Iris** layout, taken from vlanctl's `lum.toml`
(revision 1 wrongly had the VLAN 10 host at `.10.1`). **Layouts differ by
product** — vlanctl's `halo.toml` puts the untagged data path on host
`192.168.1.100/24` with the sensor *out of subnet* at `192.168.10.151`. Confirm
addresses from the device, never from this table.

### 2.0 UNRESOLVED CONTRADICTION: is the datapath actually untagged?

Flagged 2026-09-09 by adversarial review and verified against the code.
`iris_config` says one VLAN id governs **both** paths on EthIf1:

```rust
pub const ETHIF1_VLAN_ID_ABOUT: &str = "Virtual LAN (VLAN) ID for UDP data and SOME/IP";
// ethif1_vlan_id_default() -> 11
```

So the sensor's own configuration model puts **UDP data on the VLAN-11
interface**, which contradicts "the point cloud is always untagged" — a
requirement taken from the user, and the reason the Windows topology needs a
trunk vNIC with `NativeVlanId 0` (§6.3). The only evidence for "untagged" is
observed traffic from the reprovisioned bench sensor `.102` plus the field
guide's Halo table.

Either the datapath ignores `EthIf1_VlanId`, or it does not and §6.3's
native-VLAN-0 design is solving a non-problem. **One `tcpdump -e` on a
factory-config Iris settles it, and nothing downstream should be built until
it is settled.**

A second, smaller contradiction of the same shape: the user stated Halo
telnet is "currently untagged", but vlanctl's hardware-validated
`profiles/halo.toml` is titled "untagged UDP data path + **VLAN 12
telnet**". The working profile and the stated requirement disagree.

### 2.1 The shared-subnet problem — the actual hard part

Several sensor endpoints sit in **one IP subnet reachable only over distinct
VLANs**: DoIP at `192.168.10.150` on VLAN 10 and telnet at `192.168.10.152` on
VLAN 12 are both in `192.168.10.0/24`. Host routing tables key on destination
**prefix**, so a single connected `/24` cannot send different hosts out
different interfaces.

Revision 1 missed this entirely and modelled one subnet per path. It is the
central routing difficulty, and its answer is **per-host `/32` routes pinned to
an interface** — which is exactly the operation whose safety differs per
platform (§7, §8). vlanctl's profiles already express it: a gatewayless `/32`
`[[interface.route]]`.

Also note the sensor's VLAN interfaces **share one MAC**, while its untagged
point-cloud interface has its own. So MAC-based disambiguation is not available
across VLANs.

SD multicast is `239.255.0.255:30490`. **Sensor and host addresses are
deliberately in separate columns** — the datapath's *destination* is the host
at `.87`, while its *source* is the sensor at `.151`, and conflating the two
is an easy and consequential mistake.

Confirmed by the user 2026-09-08: **these are the shipping defaults** —
SOME/IP on VLAN 11, DoIP on VLAN 10, Telnet on VLAN 12 for Iris (untagged for
Halo), and the **point cloud is always untagged**. So the tagged case is the
normal case, and the tagging of a path is *platform*-dependent as well as
config-dependent.

Two consequences that shape everything below:

1. **Point-cloud receive never needed VLAN work.** `capture_interface::classify`
   strips 802.1Q transparently and renders off the physical NIC in promiscuous
   mode. Any design that scopes the datapath into VLAN provisioning is
   over-scoped.
2. **Defaults are not guarantees.** The bench sensor `.102` is reprovisioned
   for the Cart17 ROS setup and emits *everything* untagged (109,649 frames
   captured, zero tagged). A reprovisioned sensor is a real thing an operator
   will plug in, so the system observes what is actually on the wire (§4.2) and
   treats the defaults as a prior, not a fact.

## 3. Non-goals

- Configuring switches. Host-side only.
- Changing sensor configuration to avoid the problem. Decided: the host adapts.
- Owning persistent OS network configuration (NetworkManager profiles, netplan,
  `/etc/network/interfaces`). See §7.
- Windows Home support via Hyper-V. It has no Hyper-V; it gets the guided tier.
- Shipping any third-party kernel driver. Ruled out — see §6.5.

## 4. Architecture

### 4.1 Three layers, already one-and-a-half built

```
observe   →   plan   →   apply
(what is       (what the      (make it so,
 the wire       host should    reversibly)
 doing?)        look like)
```

`NetworkAssistant` (3.4k lines, on `main`) already covers *observe* for host
state: `interfaces()`, `link_state()`, `route_exists_to()`,
`route_egress_interface()`, `parent_interface()`, `multicast_reachable()`,
`ip_conflict_on()`, `sensor_reachable()`, plus `DFT_VLAN`/`SOMEIP_VLAN` and a
`vlan_id` field. `connection_state::CheckId` already names every condition we
would remediate. None of that changes.

What is missing is *wire* observation (§4.2) and the whole *plan/apply* half
(§4.3). Keep them as separate traits: the assistant diagnoses and must stay
side-effect free; the configurator changes the machine.

### 4.2 Wire observation — `WireProfile`

Cheap, because we already hold `CAP_NET_RAW` and already open a promiscuous
capture. Sniff the candidate parent NIC for a few seconds and summarise:

```rust
pub struct WireProfile {
    /// 802.1Q ids observed, with frame counts. Empty ⇒ untagged wire.
    pub vlan_ids: BTreeMap<u16, usize>,
    /// Source IPv4s seen, by subnet, and whether each arrived tagged.
    pub sources: Vec<ObservedSource>,
    /// SOME/IP-SD offers seen, and their tagging.
    pub sd_offers: Vec<ObservedSd>,
    /// Datapath (4370/4371) destinations seen — unicast host IP or group.
    pub datapath_dests: Vec<IpAddr>,
    pub observed_for: Duration,
}
```

This is what makes the design honest rather than assumption-driven, and it is
the thing that turns today's `VlanSubinterfacePresent` false alarm (§9) into a
correct verdict. A sensor whose SD offers arrive untagged needs **no** VLAN
interface, and the check must say so.

Caveat learned the hard way: **hardware VLAN offload can hide tags from
capture.** On Linux, `txvlan`/`rxvlan` offload makes the NIC insert/strip the
tag, so libpcap sees untagged frames on a tagged wire. Windows' NDIS has the
same shape (tag in out-of-band metadata). Empirically Npcap on the bench NIC
*did* show tags at the driver's default `*PriorityVLANTag`, so no registry
change is warranted — but `WireProfile` must report *"no tags observed"* rather
than *"the wire is untagged"*, and the UI must not present the inference as
certainty.

### 4.3 Profiles, not a parallel type system

Revision 1 defined `NetworkIntent` / `PathIntent` / `Plan` / `Step` /
`NetworkConfigurator`. **All of that is withdrawn** — vlanctl already has the
equivalents, tested and running against real hardware:

| Revision 1 | vlanctl |
|---|---|
| `NetworkIntent` | `config::Profile` |
| `PathIntent` | `config::Interface` + `config::Route` |
| `Plan` / `Step` | `Vec<net::Cmd>` from `plan::bringup_commands` |
| `NetworkConfigurator::apply` | `net::CommandRunner` (`SystemRunner`) |
| dry-run / preview | `net::RecordingRunner` |
| `MockNetworkConfigurator` | `net::RecordingRunner` (with `fail_at`) |

EnVision's contribution is the **derivation** vlanctl lacks:

```rust
/// Derive a vlanctl profile for one segment from what is on the wire.
///
/// Keyed on the segment (parent device), never on a sensor: sensors share
/// subnets, so N sensors on one NIC need the same host presence as one.
pub fn derive_profile(
    device: &str,
    platform: SensorPlatform,
    wire: &WireProfile,
) -> vlanctl::config::Profile;
```

That returns the same type a hand-authored `lum.toml` parses into, so a derived
profile and a curated one are interchangeable — the operator can export what we
derived, edit it, and feed it back.

### 4.3.1 The platform seam belongs in vlanctl, and it owns *decisions*

`net.rs` is already platform-neutral. The macOS specifics live in `plan.rs`
(command syntax) and `device.rs` / `commands.rs` (parsing `ifconfig` and
`networksetup` output). So the seam is a trait in vlanctl:

```rust
pub trait Platform {
    /// Interface name for a tagged entry: `vlan11` (macOS), `eth0.11`
    /// (Linux, subject to the IFNAMSIZ rule below),
    /// `vEthernet (IrisVlan11)` (Windows).
    fn iface_name(&self, interface: &Interface, device: &str) -> String;
    fn bringup_commands(&self, interface: &Interface, device: &str) -> Vec<Cmd>;
    fn teardown_commands(&self, iface: &str) -> Vec<Cmd>;
    fn list_devices(&self, runner: &mut dyn CommandRunner) -> Result<Vec<String>>;
    fn addresses_on(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<Vec<IpNet>>;
    /// Whether a gatewayless in-subnet `/32` should emit an interface-scoped
    /// route. **This is a decision, not syntax** — see below.
    fn wants_onlink_host_route(&self, in_subnet: bool) -> bool;
}
```

**Linux naming is not simply `<parent>.<id>` — IFNAMSIZ forces a fallback.**
The kernel caps an interface name at `IFNAMSIZ - 1` = 15 bytes, and a
predictable USB NIC name (`enx` + 12 MAC hex digits) is *already* 15, so any
suffix on it makes `ip link add` fail. The rule is therefore:

> Decide from the **parent alone**: a parent of 10 bytes or fewer yields
> `<parent>.<id>`; anything longer yields `vlan<id>`.

Ten is derived, not chosen: 15 less the dot less the widest valid id (`4094`,
4 digits). Two properties are load-bearing, and both were arrived at by
rejecting the obvious alternative:

- **Fall back, do not truncate.** In an `enx` name the trailing hex digits are
  the device-unique half of the MAC and the leading ones are the vendor OUI,
  so right-truncating collides two same-vendor NICs on one name — a silent
  failure, in a seam whose whole theme is eliminating those. `vlan<id>` cannot
  collide within a profile (validation rejects duplicate ids, and a profile
  has exactly one parent device), it is the name both the proven `ip`-based
  recipe and macOS already use, and `vlan4094` is 8 bytes, so it always fits.
- **Decide per parent, not per interface.** Measuring the *rendered* name
  would let one profile mix schemes on one NIC, because the suffix width
  varies with the id: an 11-byte parent fits `<parent>.999` (15 bytes) but
  overflows at `<parent>.4094` (16), so the same device would show a dotted
  name for one VLAN and a fallback name for another.

The cost, accepted deliberately: naming is host-dependent, so the same profile
yields `eth0.11` on one box and `vlan11` on another. Every consumer — bring-up,
teardown, the pre-apply collision guard, state recording — reads
`Platform::iface_name`, so they agree automatically; nothing may re-derive a
name independently.

**Why `wants_onlink_host_route` is a platform decision and not shared logic.**
vlanctl currently skips a gatewayless in-subnet `/32` because on macOS an
interface-scoped host route installs a **self-MAC** LLINFO entry and
black-holes the traffic (§8). On **Linux there is no such problem** — `ip route
add X/32 dev Y` makes the destination genuinely on-link and the kernel ARPs for
it — and that per-host route is precisely how the §2.1 shared-subnet problem is
solved. So the same profile must yield *different* command sets, and the
difference is semantic. Putting this behind the trait is the single most
important structural point in this revision: a naive port that shares the skip
rule would silently disable Linux's answer to shared subnets.

The macOS impl is today's `plan.rs` extracted verbatim, with no behaviour
change, so the existing tests keep passing and the refactor is reviewable on
its own.

### 4.4 A partial apply persists — so rollback is not a nicety

Discovered on the bench: a failed apply left a Windows machine with one vNIC,
no addresses and no route, and **a reboot faithfully restored that broken
state**. Hyper-V configuration is persistent, so a half-finished apply is not
transient damage the operator can reboot away.

Therefore: `apply` must record every completed step; a failure must offer
`revert`; and startup must `reconcile` rather than assume the machine is either
clean or fully configured.

## 5. Parent-NIC selection — the safety-critical decision

Binding the wrong NIC takes the operator's network down. On Windows a vSwitch
bound with `-AllowManagementOS $false` severs the host's path entirely; my own
probe did exactly this to a corporate LAN port during the spike.

Rule, and it is a hard one:

> **Never claim a NIC that carries the host's default route, and never claim a
> NIC that holds a non-link-local IPv4, unless the operator explicitly names
> it.**

Selection order:

1. Operator's explicit choice (a preference), always wins.
2. Exactly one carrier-up, wired, non-virtual NIC with **no** IPv4 (or only
   APIPA `169.254.x`) — the dedicated sensor port. This is the good case.
3. Otherwise **refuse and ask**, listing candidates with their addresses.
   Ambiguity is an error, not a guess.

The shared-link topology is legitimate and common — the bench setup is one dock
port into a dumb switch carrying both the corporate uplink and the sensor — so
"this NIC has an address" cannot be a hard refusal on its own. It is a refusal
*to auto-select*, and on Windows it forces the `-AllowManagementOS $true`
variant (§6.3) so untagged host traffic survives.

## 6. Windows

The hardest platform and the one the spike validated in detail.

### 6.1 Mechanism: Hyper-V management vNICs

Windows has no `netsh` verb, no WMI class, and no supported per-adapter route to
802.1Q sub-interfaces. A Hyper-V external vSwitch with management-OS vNICs does
provide them, natively and Microsoft-signed:

```powershell
New-VMSwitch -Name IrisSwitch -NetAdapterName <nic> -AllowManagementOS $false
Add-VMNetworkAdapter -ManagementOS -Name IrisVlan10 -SwitchName IrisSwitch
Set-VMNetworkAdapterVlan -ManagementOS -VMNetworkAdapterName IrisVlan10 -Access -VlanId 10
```

**Bench-proven:** bidirectional ICMP over both tagged VLANs (request *and*
reply, 14 frames each on VLAN 10 and 11, against a Linux tagged-traffic
source); survives reboot with VLAN assignments intact; tags cross the wire and
Npcap sees them at the driver's default `*PriorityVLANTag`.

### 6.2 Consent, not a silent "Fix this"

Windows apply is qualitatively heavier than Linux and must be gated on an
explicit consent screen stating:

- Hyper-V must be enabled if it is not, and **that reboots the machine**
  (`Enable-WindowsOptionalFeature -Online -FeatureName 'Microsoft-Hyper-V-All' -All`
  — quote the name; an unquoted comma parses as an array).
- The chosen NIC will be re-plumbed onto a virtual switch.
- Enabling the hypervisor can break older VirtualBox/VMware installs.
- What will be created, and that it persists until reverted.

The reboot itself is **not** a scope problem and this design does not treat it
as one: the alternative is a manual step on every boot forever, so one reboot
once is a net improvement. What *is* load-bearing is that the operator chose it
knowingly.

### 6.3 Topology, and why the trunk vNIC matters

Naive access-only topology has a hole: the datapath is **untagged** (§2), and
an external vSwitch with only access-mode vNICs has no port to deliver untagged
frames to, so **the point cloud would be dropped.**

Resolved by a trunk vNIC with `NativeVlanId 0`, which receives untagged traffic
*and* all allowed tagged VLANs:

| vNIC | Mode | Purpose |
|---|---|---|
| `IrisVlan10` | `Access` VLAN 10 | **TX** for DoIP/UDS; holds `192.168.10.x` |
| `IrisVlan11` | `Access` VLAN 11 | **TX** for SOME/IP; holds `192.168.11.x` |
| `IrisVlan12` | `Access` VLAN 12 | **TX** for Telnet; holds `192.168.12.x`. Iris only — omitted on a Halo-only segment |
| `IrisTrunk` | `Trunk`, allowed `10,11,12`, native `0` | **RX/capture**: untagged datapath *and* every tagged VLAN |

```powershell
Set-VMNetworkAdapterVlan -ManagementOS -VMNetworkAdapterName IrisTrunk `
    -Trunk -AllowedVlanIdList 10,11,12 -NativeVlanId 0
Set-VMNetworkAdapter IrisVlan10 -PortMirroring Source
Set-VMNetworkAdapter IrisVlan11 -PortMirroring Source
Set-VMNetworkAdapter IrisVlan12 -PortMirroring Source
Set-VMNetworkAdapter IrisTrunk  -PortMirroring Destination
```

**Port mirroring is required**, not optional: a trunk port does not see unicast
bound for other vNIC ports. That is correct L2 switch behaviour, not a bug.
Bench-proven with mirroring on: one trunk capture showed both conversations
tagged, `vlan 10` and `vlan 11`, 14 ICMP frames each.

This is the finding that changes the economics. Without it, a vSwitch-bound NIC
**vanishes from Npcap entirely** (`dumpcap -D` listed 14 devices with no
`Ethernet`), and one sensor's traffic splits across three adapters — which
EnVision's single-capture-plus-widened-BPF model (`SideTapConfig::doip_host`)
cannot express. With the trunk vNIC, capture stays on **one** adapter, and
`classify()` plus `vlan_aware_filter`'s `(f) or (vlan and (f))` already handle
exactly that shape. **No capture-side rework is implied.**

`CaptureIntent` therefore resolves to `IrisTrunk` on Windows, and to the parent
NIC on Linux/macOS.

### 6.4 Windows-specific requirements the spike surfaced

- **Address with `netsh`, not the `Net*` cmdlets.** `New-NetIPAddress` fails
  with *"Inconsistent parameters PolicyStore PersistentStore and Dhcp Enabled"*
  when the NIC is carrier-less. Proven to be **link state**, not ordering and
  not DHCP: with the link up it succeeds in either order. `netsh interface ipv4
  set address name="<alias>" static <ip> <mask>` works in both cases and
  persists. Use it (or the IP Helper persistent-store API).
- **Dock-attach reconcile.** If the uplink NIC is absent at boot, the vSwitch
  **silently demotes to `Internal`** and **never rebinds when the NIC returns**.
  Nothing is logged. Fix: on NIC arrival (WMI `__InstanceCreationEvent` on
  `Win32_NetworkAdapter`, or poll), if `Get-VMSwitch` reports `Internal`, run
  `Set-VMSwitch -NetAdapterName <nic>` — ~15 s, non-disruptive to vNICs, IPs,
  routes or VLAN assignments. This is `reconcile()`.
- **`AllowManagementOS` is decided by §5, and both branches are supported:**

  | Parent NIC | Flag | Untagged host traffic |
  |---|---|---|
  | Dedicated sensor port (no IPv4) | `$false` | none needed — nothing else uses the NIC |
  | Shared link (carries the uplink too) | `$true` | management vNIC keeps corporate DHCP |

  The shared-link case is the bench setup and probably the common customer
  one, so `$true` is not an edge case. Note the trunk vNIC's native VLAN 0
  does **not** substitute for the management vNIC here: it receives untagged
  frames for capture, but it carries our datapath address, not the
  operator's DHCP lease.

  **Mitigation for the `$true` duplicate-vNIC hazard.** `Set-VMSwitch`
  during reconcile re-creates the default host vNIC without noticing the
  original survived, leaving **two host vNICs sharing one MAC** on one
  switch — an L2 hazard while standing (cleaned up by `Remove-VMSwitch`,
  so not a leak). `reconcile()` must therefore:
  1. enumerate `Get-VMNetworkAdapter -ManagementOS` **before** the rebind
     and record GUIDs;
  2. run `Set-VMSwitch -NetAdapterName <nic>`;
  3. enumerate again, and remove any vNIC that is new, untagged, shares a
     MAC with a pre-existing one, and is **not** in our `Applied` set.

  Step 3's predicate is deliberately narrow — never remove a vNIC we did
  not just cause to appear.
- **Key on adapter GUID, never `ifIndex`.** GUIDs and MACs are stable across
  reboot and dock re-attach; `ifIndex` is not (observed 41/54/87 → 9/12/20).
  This matches what `netdev` reports as `NetworkInterface::name` on Windows.
- **New vNICs land in the `Public` firewall profile**, where inbound UDP is
  blocked — that sinks SD discovery while leaving outbound DoIP unaffected.
  The plan must add a scoped inbound rule for the SD port, and revert must
  remove it.
- **A carrier-less parent leaves every vNIC address `Tentative`** and route
  *selection* prefers the uplink, so `RouteToSensor` reads false and
  `ip_conflict_on` reads `Unavailable`. Correct, but it means "cable in" is a
  precondition for a green Connection section, not merely for traffic. The UI
  must say so rather than showing an unexplained red.

### 6.5 Rejected Windows mechanisms

| Option | Why not |
|---|---|
| Wintun + userspace bridge | Prebuilt-binaries licence forbids redistribution "without prior written consent from WireGuard LLC"; bundling `wintun.dll` is redistribution. Moot now that Hyper-V works. |
| `tap-windows6` | OpenVPN state the inf+cat+sys bundle and NSIS installer lack reference counting and "third-party vendors should not redistribute them"; attestation-signed builds are OS-version-limited. |
| Own NDIS driver | Needs Microsoft attestation/WHQL via Partner Center. Our Authenticode/SSL.com work is **user-mode only** and does not cover a kernel driver. |
| Per-adapter "VLAN ID" property | One VLAN at a time; cannot serve 10 and 11 together. Degraded fallback only. |
| Intel PROSet `Add-IntelNetVLAN` | Intel-only, being removed from newer adapters. |

### 6.6 Guided fallback tier — still mandatory, but narrower

**Windows Home is out of scope** (decided 2026-09-08). It has no Hyper-V, and
Pro or better is a documented requirement for live sensor work on Windows.
Detect the SKU and say so plainly rather than failing obscurely.

The guided tier is still required, for the cases we cannot fix on a *supported*
SKU: IT policy refusing the optional feature, the operator declining the
consent screen, or `apply()` failing. Those hosts get the *observe + plan* half
with apply disabled — the Connection section names the exact missing VLAN,
address and route and offers copy-pasteable commands. This is also every
platform's fallback when `plan()` succeeds but `apply()` cannot run.

## 7. Linux

Cheapest platform, and the one whose routing model actually suits the problem.

- **No prompt needed — but ONLY in-process.** EnVision already holds
  `CAP_NET_ADMIN`, not just `CAP_NET_RAW` (`crates/envision/scripts/postinst`
  and `crates/envision/scripts/setup.sh` both apply
  `cap_net_raw,cap_net_admin=eip`).
  **CORRECTION (2026-09-09): "or `ip`" was wrong.** File capabilities do not
  survive an `exec` into `/sbin/ip` — the child's permitted set is
  `F(inheritable) ∩ P(inheritable)`, and `ip` carries no file caps — so
  shelling out loses the capability unless EnVision first raises **ambient**
  capabilities (`prctl(PR_CAP_AMBIENT_RAISE)`). This matters because
  vlanctl's entire model is shelling out through `Cmd`/`SystemRunner`. So the
  Linux backend must either use **rtnetlink in-process** or raise ambient caps
  before spawning. Note also the capability exists only on `.deb`-installed
  builds or after the first-run `pkexec setcap` flow — a `cargo run` build has
  none.
- **RESOLVED (2026-09-10, measured): shell out, with an ambient-cap raise
  before spawn.** rtnetlink is not needed, so vlanctl's `Vec<Cmd>` contract
  stands and dry-run/`show` keep working for Linux. What the measurement
  added, and it is the part that is easy to get wrong: `PR_CAP_AMBIENT_RAISE`
  needs the capability in the process's **permitted and inheritable** sets,
  and `setcap cap_net_admin=eip` only sets the *file's* inheritable bit, which
  on exec feeds the permitted set alone. The process's own inheritable set is
  whatever the parent shell had — empty. So the raise fails outright unless
  `capset` adds `CAP_NET_ADMIN` to pI first (allowed without `CAP_SETPCAP`,
  since pI' may be any subset of `pI | pP` and pP already holds it). Measured
  pI `0x0` → `0x1000`, after which `ip link add … type vlan` succeeds
  unprivileged. The sequence is **capset(pI) → PR_CAP_AMBIENT_RAISE → spawn**,
  and it belongs to the *consuming process* (EnVision's own startup), not to
  the `Platform` impl, which only emits commands.
  Still unconfirmed: the capabilities actually present on a shipped `.deb`
  build — the spike host had no installed `envision` to run `getcap` against.
- **`ip route add X/32 dev Y` works as intended** — the destination becomes
  genuinely on-link and the kernel ARPs. This is the answer to §2.1's
  shared-subnet problem, and it is why `Platform::wants_onlink_host_route`
  returns `true` here and `false` on macOS.
- **Static ARP** where a sensor will not answer: `ip neigh add <ip> lladdr
  <mac> dev <iface>` — the analogue of macOS's `arp -s`, without the
  `arp -d` trap.
- **Persistence: deliberately none.** `ip link add ... type vlan` does not
  survive a reboot, and writing NetworkManager or netplan profiles would mean
  owning distro-specific system state that the operator's own tools manage.
  Re-apply idempotently at startup instead; vlanctl's apply already skips
  interfaces whose address is present, so a re-apply on a configured host is a
  no-op.

## 8. macOS — already working; do not re-derive it

vlanctl runs here today. The reason this section exists is that macOS is **not
a port of Linux**, and revision 1 wrongly assumed it was ("start with
`osascript` + `ifconfig`"). The traps, from
`vlanctl/docs/vlan-sensor-reachability.md`:

1. **The self-MAC black-hole.** `route add -host X -interface Y` fills the
   link layer with *the interface's own* address instead of ARPing for the
   destination; traffic loops back and dies, reported as
   `ping: sendto: Cannot allocate memory` or an ARP entry showing our own MAC
   as `permanent`. Happens whether or not the destination is in-subnet. This is
   "the reason the obvious port of a working Linux bring-up script fails here."
2. **Static ARP is a two-command pair, and the obvious defensive step breaks
   it.** `route add -host <ip> -interface <if>` then `arp -s <ip> <mac>`.
   **Never insert `arp -d` between them** — on macOS that deletes the
   freshly-added cloning route, so the `arp -s` fails and traffic falls back to
   the default route on another interface. vlanctl's comments record this as a
   bug they actually shipped and removed.
3. **Some USB adapters silently drop 802.1Q on transmit.** RX working tells you
   *nothing* about TX, and `tcpdump` on the host shows the tag because it
   captures before the driver — so the host looks correct while the wire frame
   is untagged. Seen on a generic dongle (`98:fc:84:…`); a Realtek RTL8153
   (`00:e0:4c:…`) works. **Two-vantage capture** is the only reliable
   diagnosis. This is a hardware caveat to surface in the UI, not something we
   can fix.
4. **Sensors may answer neither ICMP nor ARP on every interface.** Do not use
   `ping` as a reachability probe; probe the real service. The sensor's MAC can
   be harvested passively from its own SOME/IP-SD multicast and fed to a static
   ARP entry — which is a job `WireProfile` (§4.2) is already positioned to do.
5. GUI password prompts for a non-interactive shell: `SUDO_ASKPASS=… sudo -A`
   with an `osascript` helper.

**macOS work in this project is therefore integration, not implementation**:
extract today's behaviour behind `Platform` (§4.3.1) without changing it.

## 9. Bugs this work must fix

1. **`CheckId::VlanSubinterfacePresent` false-alarms on an untagged path** —
   it tells the operator to create a VLAN that must not exist. With
   `WireProfile` it becomes a correct verdict. This is live on `main`.
2. **`CheckId::RouteToSensor` hardcodes `192.168.10.150`**
   (`DFT_DEFAULT_DOIP_ADDR`) — must come from `NetworkIntent`.
3. **`route_exists_to` false-green on Windows** (branch
   `feat/windows-network-assistant`, `48a85e27`): `best_route` returns `None`
   for both "API failed" and "no route" — its own comment notes `rc != 0` is
   normally `ERROR_NETWORK_UNREACHABLE` — and the caller maps `None => true`.
   So *no route at all* reports as reachable. Linux's graceful `true` fires only
   on genuine I/O failure. **Fix: discriminate on `rc`**;
   `ERROR_NETWORK_UNREACHABLE`/`ERROR_NOT_FOUND` ⇒ `false`. Masked on the bench
   because Wi-Fi supplied a default route; it bites on a host wired solely to
   the sensor.
4. **Two pre-existing `-D warnings` violations on the Windows cfg path**, in
   files that branch never touched: `capture_interface/src/recording.rs:519`
   (`unused_self` on `fn pump_live_into_backlog(&mut self) {}`) and
   `envision_drivers/src/sensor_runtime.rs:1951` (unused `MockIrisClient`
   import). Clear these before adding any Windows clippy job.

## 10. Testing

The design deliberately concentrates decisions in `plan()` so that the
untestable part is thin.

- **Unit (no hardware, all platforms):** `plan()` against a
  `MockNetworkAssistant` + synthetic `WireProfile`. Covers tagged vs untagged
  worlds, the §5 refusal rules, idempotency (`satisfied`), and step ordering.
  This is where the coverage should live.
- **`MockNetworkConfigurator`**, mirroring the existing `MockNetworkAssistant`,
  for UI and shell tests.
- **Linux integration, in CI:** genuinely feasible with a network namespace and
  a `veth` pair — create real VLAN sub-interfaces, apply, assert, revert. The
  `run-sensor-sim` skill already uses netns for a comparable purpose.
- **Windows and macOS:** manual bench matrix, documented, with the tagged
  traffic generated by a Linux peer rather than a sensor (`~/dev/iris_vlan_tagsrc.sh`
  proved this works: dock-to-dock cable, no sensor, real 802.1Q on the wire).
  Per-platform apply backends must stay dumb enough that this is acceptable.
- **Do not** attempt to CI the Windows apply path. There is no VLAN-capable NIC
  and no Hyper-V in the runners.

## 11. Phasing — now spans two repositories

| Phase | Repo | Content |
|---|---|---|
| **1a** | vlanctl | Add a **lib target**; extract today's macOS behaviour behind `Platform` (§4.3.1) with **no behaviour change**. Existing tests must pass untouched. |
| **1b** | vlanctl | **Linux `Platform` impl** — `ip link add … type vlan`, `ip addr`, `ip route add X/32 dev Y`, `ip neigh add`, and `wants_onlink_host_route == true`. |
| **1c** | dft | `WireProfile` observation (§4.2), `derive_profile` (§4.3), fix the two Connection-section bugs (§9.1, §9.2), and a **dry-run preview UI** driven by `RecordingRunner`. Applies nothing. |
| **2** | dft | Linux **apply** via `SystemRunner` — no prompt (we hold `CAP_NET_ADMIN`), idempotent re-apply at startup. |
| **3** | vlanctl + dft | **Windows `Platform` impl** — Hyper-V vSwitch, access vNICs, trunk vNIC + port mirroring, `netsh` addressing, dock-attach reconcile — plus the consent screen. The bulk of the remaining work. |
| **4** | dft | Wire macOS apply into EnVision (vlanctl already works standalone there). |
| **5** | dft | Hardware-event reconcile (NIC arrival on Windows). |

Phases 1a and 1b are vlanctl PRs and can proceed in parallel with 1c, which
only needs the lib target to exist. **Cross-repo sequencing:** 1a lands and is
tagged before dft pins it; treat the pin bump as one commit (pointer + version
requirement together), the same discipline the `simple_doip` submodule uses.

**Parallel track worth starting now:** the `SO_BINDTODEVICE` / `IP_BOUND_IF`
investigation (§12). If application-level binding works, Phases 2–4 shrink
substantially and some of Phase 3 may become unnecessary — so answering it
early is cheap insurance against building the expensive version.

## 12. Decisions taken, and what remains open

**Resolved (user, 2026-09-08):**

1. **Sensors ship tagged** — SOME/IP VLAN 11, DoIP VLAN 10. The tagged case is
   the normal case; the Windows work is required, not optional.
2. **The point cloud is always untagged.** Makes the trunk vNIC's
   `NativeVlanId 0` load-bearing on Windows.
3. **Telnet is VLAN 12 on Iris, untagged on Halo.** Platform-dependent, so the
   platform→VLAN mapping is data, not branches.
4. **Windows Home is out of scope.** Pro or better is a documented requirement.
5. **Multi-sensor is expected but not first.** Sensors share subnets, so
   profiles key on the segment, never the sensor.
6. **Adopt vlanctl** rather than build a parallel implementation (§0).

**Open, in priority order:**

- **`SO_BINDTODEVICE` / `IP_BOUND_IF` — the one that could shrink the
  project.** vlanctl's own porting notes call application-level binding "the
  most robust cross-platform way to force same-subnet traffic out a specific
  interface, sidestepping routing-table ambiguity entirely — worth considering
  if the consuming app can bind its sockets."
  **CORRECTION (2026-09-09): my earlier "EnVision binds nothing today —
  verified" was WRONG, and wrong in a way worth remembering.** I grepped
  `crates/` for `SO_BINDTODEVICE` / `IP_BOUND_IF` / `bind_device` and reported
  a verified negative — but `simple-someip` is an **external crate** (v0.12),
  not in `crates/`, so that search structurally could not have found the
  answer. In fact `iris_someip_client` passes `interface: Ipv4Addr` straight
  through to it, and it binds that address and sets `IP_MULTICAST_IF`. So
  **SOME/IP already binds; only DoIP (`simple_doip`) and telnet are
  unbound.** A negative result from a search whose scope excludes the answer
  is not a verification. If our SOME/IP, DoIP and datapath
  clients bound per-interface, §2.1's shared-subnet problem largely evaporates
  and Windows' topology has less to achieve. Costs a spike, not a commitment.
- **The untagged-ARP question for `192.168.11.87`** (Windows strong host
  model). Still needs a factory-config sensor. Note vlanctl shows this class of
  problem is real and solved-by-static-ARP on macOS, so the fix shape is known
  even if the Windows behaviour is not.
- **Whether a derived profile should be persisted** as a `.toml` the operator
  can edit and re-feed. The types make it free; the question is whether we want
  to own operator-edited files.
- **Halo's telnet is "currently" untagged.** A firmware change makes it a data
  edit, provided the mapping stays data.
- **Multi-sensor profile composition** beyond the shared-subnet base case.
- vlanctl's own `docs/superpowers/plans/2026-06-18-*.md` (17 KB and 23 KB) and
  its 38 KB implementation plan are **unread**. Read before starting 1a — they
  likely contain decisions this design would otherwise re-litigate.
