# Reaching VLAN-segmented sensors from a host — findings

A field guide to the problems (and fixes) we hit bringing up a multi-VLAN
Luminar sensor from a host. Written from macOS experience — that's all `vlanctl`
targets today — but most of the failure modes and diagnostics are general, so
the platform-specific notes are called out explicitly.

## The setup

One physical sensor exposes several endpoints, each on its own 802.1Q VLAN (or
untagged), and **several of them share one IP subnet** across different VLANs:

| Endpoint   | VLAN     | Sensor IP        | Host IP          | Notes |
|------------|----------|------------------|------------------|-------|
| Point cloud (UDP) | untagged | `192.168.10.151` | `192.168.1.100/24` | data path; host is a *different* subnet |
| SOME/IP    | 11 (Iris)| `192.168.10.151` | —                | service discovery via multicast |
| DoIP       | 10 (Iris)| `192.168.10.150` | —                | |
| Telnet (debug) | 12   | `192.168.10.152` | `192.168.10.1/24` | |

The sensor's VLAN interfaces share one MAC (`3a:42:f7:79:32:2e`); the untagged
point-cloud interface has its own (`00:11:c6:01:36:22`). Layouts differ by
product (the Iris VLAN-10/11 layout vs the Halo untagged-data + VLAN-12 layout),
so confirm addresses from the device, not assumptions.

The hard part: **multiple destinations in one IP subnet (`192.168.10.0/24`)
reachable only on distinct L2 segments (VLANs).** Host routing tables are keyed
by destination prefix, so a single connected `/24` can't send different hosts
out different interfaces.

## Failure modes we hit (and how to recognize them)

### 1. USB Ethernet adapter silently drops 802.1Q tags on TX (macOS)

The single most expensive red herring. Symptom: you send correctly-tagged
frames (confirmed by `tcpdump -e` on the parent interface), the sensor's tagged
multicast arrives fine (RX works), but the sensor **never answers anything you
send** — no ARP reply, no ICMP, no TCP SYN-ACK.

Cause: some USB NIC chipsets (we saw it on a generic "USB 10/100/1000 LAN",
MAC `98:fc:84:…`) do not insert the VLAN tag on transmit under macOS. `tcpdump`
on the host shows the tag because it captures the frame *before* the driver, so
the host looks correct while the wire frame is untagged/mangled. RX-tag handling
and TX-tag handling are separate paths; RX working tells you nothing about TX.

Fix: use a NIC that tags on TX. A Realtek RTL8153 (`00:e0:4c:…`) worked; the
generic dongle did not. **Confirm by capturing on a second host** on the same
segment while the suspect host transmits: if your `who-has` shows up untagged or
not at all there, the adapter is the problem.

> Likely macOS-specific in this exact form, but "NIC/driver doesn't do what you
> think on TX" is worth ruling out anywhere. Capturing from a second vantage
> point is the general technique.

### 2. `route add -host X -interface Y` self-MAC black-hole (macOS / BSD)

Symptom: `ping: sendto: Cannot allocate memory`, or a route/ARP entry where the
destination resolves to **your own MAC**, permanent:

```
192.168.10.151  0:e0:4c:68:a:1d  UHLS  en16          (route gateway = our MAC)
? (192.168.10.151) at 0:e0:4c:68:a:1d on en16 permanent
```

Cause: on macOS, an interface-scoped host route fills the link-layer with the
*interface's own* address rather than ARPing for the destination. Traffic loops
to ourselves and dies. This happens whether the destination is in-subnet or out.

This is **fundamentally different from Linux**, where `ip route add X/32 dev Y
src Z` makes X genuinely on-link and the kernel ARPs for it. The macOS behavior
is the reason the obvious port of a working Linux bring-up script fails here.

Fixes:
- If the destination is *in* the interface's connected subnet, don't add a host
  route at all — the connected route ARPs normally. (`vlanctl` skips in-subnet
  gatewayless routes for this reason.)
- If it's *out* of subnet and the device won't ARP for it, add a **static ARP
  entry** (see #4).

### 3. The sensor doesn't answer ARP on some interfaces

Automotive/comms ECUs often expose services (DoIP, SOME/IP, telnet) but don't
run a general ARP responder or ICMP echo on every interface, and may expect a
statically-configured peer. So:

- **Don't use `ping` as your reachability probe** — a sensor that ignores ICMP
  will look dead even when the data path is fine. Test the real service (e.g.
  `nc <ip> 23` for telnet) instead.
- You can often harvest the sensor's MAC passively from its own traffic (we read
  `.151`'s MAC out of its SOME/IP-SD multicast with `tcpdump -e`) and feed it to
  a static ARP entry.

Note: on a *good* adapter the sensor here did answer ARP for the telnet
interface — so "sensor doesn't ARP" and "adapter doesn't tag" (#1) look
identical from the host. Rule out the adapter first.

### 4. Static ARP for an out-of-subnet host — and the `arp -d` trap (macOS)

When a host must reach an out-of-subnet device that won't ARP for itself, set a
static entry. The working macOS sequence is exactly two commands:

```
route add -host 192.168.10.151 -interface en16
arp -s 192.168.10.151 3a:42:f7:79:32:2e
```

The `arp -s` overwrites the LLINFO self-MAC entry (#2) with the real MAC; frames
then egress to the sensor. Result:

```
? (192.168.10.151) at 3a:42:f7:79:32:2e on en16 permanent   (single, real MAC)
```

**Trap:** do *not* insert a defensive `arp -d <host>` between those two
commands. On macOS `arp -d` deletes the freshly-added cloning host route, so the
host is no longer on-link and the following `arp -s` fails (the destination falls
back to the default route via another interface). We added that `arp -d` "to be
safe" and it was the bug. `route add` + `arp -s` alone is clean.

## Diagnostic techniques that paid off

- **`tcpdump -e -i <parent>`** to see Ethernet headers including the VLAN tag.
  Capture on the *parent* device, not the vlan sub-interface.
- **BPF filter gotcha:** a plain `arp` filter matches `ethertype 0x0806` and
  therefore **silently drops 802.1Q-tagged ARP** (outer ethertype is `0x8100`).
  Use `'vlan or arp'`, or `'host <ip> or (vlan and host <ip>)'`, when tagged
  frames are in play. We chased a ghost for a while because of this.
- **Read the ARP table critically:** `at <our-own-MAC> ... permanent` means a
  black-hole, not a resolved neighbor; `(incomplete)` means a real request went
  out and got no answer (a genuinely different signal); a real neighbor MAC that
  ages out is success.
- **`route -n get <ip>`** flags tell the story: `REJECT` = failed-ARP black-hole;
  `GATEWAY` when you expected an interface route = your host route got deleted.
- **Two-vantage capture** distinguishes "we didn't send it" from "they didn't
  answer" — essential for the adapter-TX problem (#1).
- macOS `sudo` in a non-interactive shell: use an askpass helper
  (`SUDO_ASKPASS=… sudo -A …`) with `osascript` for a GUI password prompt.

## How this maps to `vlanctl` (macOS)

- Profiles are a list of `[[interface]]` entries with an optional `vlan` tag.
  Tagged → a `vlan<id>` sub-interface; untagged → the parent device directly.
- A gatewayless `/32` route may carry `mac = "…"`, which emits the
  `route add -host` + `arp -s` pair above. Gatewayless in-subnet destinations
  emit no route (connected route handles them).
- The design rationale for untagged interfaces and static ARP lives in this
  project's spec documents, which are working notes kept outside version
  control — ask, or recover an earlier committed revision with
  `git log --diff-filter=D -- docs/superpowers/specs/`.

## Porting notes for other platforms

- **Linux:** the self-MAC black-hole (#2) does not occur — `ip route add X/32
  dev Y src Z` makes X on-link and the kernel ARPs. VLANs via
  `ip link add … type vlan id N`. Static ARP (`ip neigh replace`) is the analog
  of #4 when a device won't ARP — note `replace`, not `add`: `arp -s`
  overwrites an existing entry whereas `ip neigh add` fails with `File exists`,
  which matters on a parent device that persists across applies and may already
  hold a learned entry. The shared-subnet routing problem is the same;
  per-host `/32` dev routes are the usual answer.
- **Windows:** VLAN tagging is typically a NIC-driver setting, not an OS
  construct; static ARP via `netsh interface ip add neighbors`. The adapter-TX
  issue (#1) is just as real — validate on the actual hardware.
- **Application-level binding** (`IP_BOUND_IF` / `SO_BINDTODEVICE`) is the most
  robust cross-platform way to force same-subnet traffic out a specific
  interface, sidestepping routing-table ambiguity entirely — worth considering
  if the consuming app can bind its sockets.
