#[cfg(test)]
use crate::config::Profile;
use crate::config::{Interface, Route};
use crate::net::{Cmd, CommandRunner};
use anyhow::Result;
use ipnet::IpNet;

/// Per-operating-system command generation and host-state reading.
///
/// Two of the emission methods return **decisions, not syntax**, and that is
/// deliberate. The straight-line logic this trait replaces looked universal
/// but encoded macOS semantics; a port that shared it would silently break
/// the other platforms. See the dft-side design §4.3.1.
///
/// The state-reading methods (`list_devices`, `addresses_on`, `is_wireless`,
/// `link_is_active`) exist for the same reason: they parse the output of
/// OS-specific tools, and a wrong parser silently mis-detects hardware rather
/// than erroring, so each platform must supply its own rather than inherit
/// one that happens to compile. That said, none of the four has a production
/// caller yet: `apply`, `down`, `status`, and `resolve_device` all still call
/// the macOS parsers in `commands`/`device` directly. These methods are
/// declared now for the Linux and Windows backends to implement; wiring the
/// real call sites over to them is host-detection work with its own review
/// surface (a wrong parser there silently picks the wrong NIC), not a
/// byproduct of declaring the trait.
///
/// Route and static-ARP command **syntax** is behind this seam as of
/// [`Platform::route_commands`]. It previously was not: shared code built
/// BSD `route add ...`/`arp -s ...` for every platform and delegated only
/// the *decision* of whether to emit a host route, so a non-BSD backend
/// rendered unusable commands. The decision-only hook it replaced
/// (`wants_onlink_host_route`) could express *whether* to route but not
/// *how*.
pub trait Platform {
    /// Short identifier for logs and error messages, e.g. `"macos"`.
    fn name(&self) -> &'static str;

    /// Interface name for a profile entry. `vlan11` on macOS, `eth0.11` on
    /// Linux (or `vlan11` when the dotted form would exceed the kernel's
    /// name limit), `vEthernet (IrisVlan11)` on Windows. Untagged entries
    /// return the parent device.
    ///
    /// This is the single source of every sub-interface name: bring-up,
    /// teardown, the collision guard in `commands::apply`, and creation
    /// recording all read it, so a platform's naming rule lives here and
    /// nowhere else.
    fn iface_name(&self, interface: &Interface, device: &str) -> String;

    /// Ordered commands to bring one interface up: interface creation,
    /// address, and MTU. Routes are a separate concern
    /// ([`Platform::route_commands`], appended by [`bringup_commands_for`]),
    /// so implementations must not include them here.
    fn bringup_commands(&self, interface: &Interface, device: &str) -> Vec<Cmd>;

    /// Commands to tear one interface down.
    fn teardown_commands(&self, iface: &str) -> Vec<Cmd>;

    /// Ordered commands for ONE route on `iface`, including any static-ARP
    /// entry — both the decision of what to emit and the syntax to emit it
    /// in. `in_subnet` is true when the destination falls inside `iface`'s
    /// own connected subnet, which [`bringup_commands_for`] computes.
    ///
    /// The platforms genuinely disagree here, which is why this is behind
    /// the seam rather than shared. For a *gatewayless in-subnet*
    /// destination macOS must emit **no** route: an interface-scoped host
    /// route there installs a permanent self-MAC LLINFO entry and
    /// black-holes the traffic, so the connected route has to resolve it.
    /// Linux emits one regardless — `ip route add X/32 dev Y` makes the
    /// destination genuinely on-link and the kernel ARPs for it, which is
    /// the only way to reach several hosts sharing one subnet across
    /// different VLANs. The command vocabulary differs too: BSD
    /// `route`/`arp` against Linux `ip route`/`ip neigh`.
    fn route_commands(&self, route: &Route, in_subnet: bool, iface: &str) -> Vec<Cmd>;

    /// Whether teardown may revert configuration applied to the **parent**
    /// device (as opposed to a VLAN sub-interface it created).
    ///
    /// macOS answers `false`: an untagged entry aliases an address onto the
    /// physical device and that persists by design. Windows must answer
    /// `true`, because its untagged equivalent is a vSwitch binding that
    /// re-plumbs the NIC and cannot be left behind.
    ///
    /// No code reads this answer today: an untagged entry is an address
    /// alias, and undoing one needs `-alias <addr>` syntax that the generic
    /// [`Platform::teardown_commands`] has no way to express (see the
    /// comment in `commands::apply` for the full reasoning). A platform that
    /// must actually revert parent configuration needs its own recording and
    /// teardown path built for that purpose — implementing this method as
    /// `true` alone gets a backend nothing.
    fn reverts_parent_config(&self) -> bool;

    /// Names of every network device currently on the host, physical and
    /// virtual, in the OS's own naming.
    ///
    /// Declared for the Linux and Windows backends; not yet
    /// consumed anywhere. `apply`/`down`/`status`/`resolve_device` still call
    /// `commands::live_interfaces` (the macOS parser) directly.
    fn list_devices(&self, runner: &mut dyn CommandRunner) -> Result<Vec<String>>;

    /// IPv4 addresses currently configured on `device`, each with its prefix
    /// length. Other address families are excluded.
    ///
    /// Declared for the Linux and Windows backends; not yet
    /// consumed anywhere. `apply`/`down`/`status`/`resolve_device` still call
    /// `commands::device_inet_addresses` (the macOS parser) directly.
    fn addresses_on(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<Vec<IpNet>>;

    /// Whether `device` is a wireless (Wi-Fi) adapter, which device
    /// detection must never pick as the sensor link.
    ///
    /// Declared for the Linux and Windows backends; not yet
    /// consumed anywhere. `resolve_device` still calls `device::wifi_devices`
    /// (the macOS parser) directly.
    fn is_wireless(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool>;

    /// Whether `device` currently has a live link — carrier from a connected
    /// peer. An interface that is administratively down, or up with nothing
    /// plugged in, answers `false`; only a genuine failure to ask is an
    /// `Err`.
    ///
    /// Declared for the Linux and Windows backends; not yet
    /// consumed anywhere. `resolve_device` still calls
    /// `device::interface_is_active` (the macOS parser) directly.
    fn link_is_active(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool>;

    /// Whether `cmd` is this platform's *interface creation* command, and so
    /// the interface it names must be recorded for teardown and rollback.
    ///
    /// This exists because the recording test used to match `ifconfig`
    /// argument shape directly, which silently recorded nothing on any other
    /// platform — leaving rollback with nothing to undo and `down` a no-op.
    fn records_created_interface(&self, cmd: &Cmd) -> bool;
}

/// macOS / BSD. The behavior this crate shipped before the `Platform` seam
/// existed, extracted unchanged.
pub struct MacOs;

impl Platform for MacOs {
    fn name(&self) -> &'static str {
        "macos"
    }

    fn iface_name(&self, interface: &Interface, device: &str) -> String {
        iface_name(interface, device)
    }

    fn bringup_commands(&self, interface: &Interface, device: &str) -> Vec<Cmd> {
        create_address_mtu_commands(interface, device)
    }

    fn teardown_commands(&self, iface: &str) -> Vec<Cmd> {
        teardown_commands(iface)
    }

    fn route_commands(&self, route: &Route, in_subnet: bool, iface: &str) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        match &route.gateway {
            Some(gateway) => {
                let gateway = gateway.to_string();
                cmds.push(Cmd::new("route", &["add", &route.destination, &gateway]));
            }
            None => {
                // An interface-scoped host route to an in-subnet destination
                // installs a permanent self-MAC LLINFO entry and black-holes
                // the traffic; the connected route resolves it correctly.
                if !in_subnet {
                    cmds.push(interface_route_command(&route.destination, iface));
                }
                // Static ARP for an on-link host the kernel can't resolve.
                // The `-host ... -interface` route above leaves an LLINFO
                // entry resolving to our own MAC; `arp -s` overwrites it with
                // the real one. No preceding `arp -d`: on macOS that deletes
                // the freshly-added host route. Validation guarantees
                // mac => gatewayless /32, so the parse always succeeds.
                if let Some(mac) = &route.mac
                    && let Ok(net) = route.destination.parse::<IpNet>()
                {
                    let host = net.addr().to_string();
                    cmds.push(Cmd::new("arp", &["-s", &host, mac]));
                }
            }
        }
        cmds
    }

    fn reverts_parent_config(&self) -> bool {
        // An untagged entry aliases an address onto the physical device.
        // Teardown deliberately leaves it: the device may carry unrelated
        // configuration this crate did not create.
        false
    }

    fn list_devices(&self, runner: &mut dyn CommandRunner) -> Result<Vec<String>> {
        // `ifconfig -l`: one line of space-separated names.
        crate::commands::live_interfaces(runner)
    }

    fn addresses_on(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<Vec<IpNet>> {
        // The `inet <addr> netmask <hex>` lines of `ifconfig <device>`.
        crate::commands::device_inet_addresses(runner, device)
    }

    fn is_wireless(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool> {
        // `networksetup -listallhardwareports`: the device under a Wi-Fi
        // hardware port.
        Ok(crate::device::wifi_devices(runner)?
            .iter()
            .any(|w| w == device))
    }

    fn link_is_active(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool> {
        // `ifconfig <device>` reporting `status: active`.
        crate::device::interface_is_active(runner, device)
    }

    fn records_created_interface(&self, cmd: &Cmd) -> bool {
        // `ifconfig <name> create`
        cmd.program == "ifconfig" && cmd.args.get(1).map(|a| a == "create").unwrap_or(false)
    }
}

/// Longest interface name the Linux kernel accepts: `IFNAMSIZ` (16) less
/// the terminating NUL.
const LINUX_IFNAMSIZ_MAX: usize = 15;

/// Widest a VLAN id's decimal form can be, per the `1..=4094` range
/// `Profile::validate` enforces (`src/config.rs`).
const LINUX_MAX_VLAN_ID_DIGITS: usize = 4;

/// Longest parent device name for which `<parent>.<id>` fits
/// [`LINUX_IFNAMSIZ_MAX`] for *every* valid VLAN id, not just the one at
/// hand: the widest suffix is `.` plus [`LINUX_MAX_VLAN_ID_DIGITS`] digits
/// (`.4094`, 5 bytes), so `15 - 5 = 10`. Naming must be decided from this
/// worst case — see [`Linux::iface_name`] for why.
const LINUX_MAX_DOTTED_PARENT_LEN: usize = LINUX_IFNAMSIZ_MAX - 1 - LINUX_MAX_VLAN_ID_DIGITS;

/// Linux. Commands mirror the recipe already proven on this hardware by the
/// `ip`-based bring-up script this crate's shell-out approach replaces.
pub struct Linux;

impl Platform for Linux {
    fn name(&self) -> &'static str {
        "linux"
    }

    fn iface_name(&self, interface: &Interface, device: &str) -> String {
        match interface.vlan {
            Some(id) => {
                // 8021q convention: `<parent>.<id>` — when the *parent*
                // fits, using the widest possible suffix
                // ([`LINUX_MAX_DOTTED_PARENT_LEN`]) rather than this
                // interface's own id. The kernel caps a name at
                // `IFNAMSIZ - 1` (15) bytes, and a predictable USB NIC name
                // (`enx` + 12 MAC hex digits) is already 15, so `ip link
                // add` would reject any suffix on it.
                //
                // Deciding per-interface (by this id's own width) instead
                // of per-parent would let one profile mix naming schemes on
                // one NIC: an 11-byte parent fits `<parent>.999` (15 bytes)
                // but overflows at `<parent>.4094` (16 bytes), so id 999
                // would get a dotted name and id 4094 would get the
                // fallback on the very same device — confusing to read in
                // `ip link` output. Deciding from the parent alone keeps
                // naming consistent across every interface in a profile.
                //
                // Overflow falls back to `vlan<id>` rather than truncating
                // the parent: in an `enx` name the trailing hex digits are
                // the device-unique half of the MAC, so trimming them would
                // make two same-vendor NICs collide on one name — a silent
                // failure. `vlan<id>` cannot collide within a profile
                // (validation rejects duplicate ids and a profile has one
                // parent), it is the name the proven `ip`-based recipe and
                // macOS both use, and `vlan4094` is 8 bytes, so it always
                // fits.
                if device.len() <= LINUX_MAX_DOTTED_PARENT_LEN {
                    format!("{device}.{id}")
                } else {
                    format!("vlan{id}")
                }
            }
            None => device.to_string(),
        }
    }

    fn bringup_commands(&self, interface: &Interface, device: &str) -> Vec<Cmd> {
        let name = self.iface_name(interface, device);
        let addr = interface.address.to_string();
        let mut cmds = Vec::new();
        if let Some(id) = interface.vlan {
            let id = id.to_string();
            // Raise the parent before adding anything on it, as the recipe
            // does. A NIC left admin-down after boot would otherwise leave
            // every sub-interface LOWERLAYERDOWN while every command
            // succeeds. `ip link set ... up` is idempotent, so repeating it
            // per tagged entry is harmless.
            cmds.push(Cmd::new("ip", &["link", "set", device, "up"]));
            cmds.push(Cmd::new(
                "ip",
                &[
                    "link", "add", "link", device, "name", &name, "type", "vlan", "id", &id,
                ],
            ));
            cmds.push(Cmd::new("ip", &["link", "set", &name, "up"]));
        }
        // `ip addr add` takes CIDR directly — no netmask conversion needed.
        cmds.push(Cmd::new("ip", &["addr", "add", &addr, "dev", &name]));
        if let Some(mtu) = interface.mtu {
            cmds.push(Cmd::new(
                "ip",
                &["link", "set", &name, "mtu", &mtu.to_string()],
            ));
        }
        cmds
    }

    fn teardown_commands(&self, iface: &str) -> Vec<Cmd> {
        // Deleting the link drops its addresses and routes with it.
        vec![Cmd::new("ip", &["link", "del", iface])]
    }

    fn route_commands(&self, route: &Route, _in_subnet: bool, iface: &str) -> Vec<Cmd> {
        // `in_subnet` is deliberately ignored: unlike macOS, `ip route add
        // X/32 dev Y` makes X genuinely on-link and the kernel ARPs for it.
        // That is the only way to reach several hosts that share one subnet
        // across different VLANs, which is exactly the in-subnet case.
        let mut cmds = Vec::new();
        match &route.gateway {
            Some(gateway) => {
                let gateway = gateway.to_string();
                cmds.push(Cmd::new(
                    "ip",
                    &[
                        "route",
                        "add",
                        &route.destination,
                        "via",
                        &gateway,
                        "dev",
                        iface,
                    ],
                ));
            }
            None => {
                cmds.push(Cmd::new(
                    "ip",
                    &["route", "add", &route.destination, "dev", iface],
                ));
                // `ip neigh add` is the Linux equivalent of `arp -s`. No
                // self-MAC hazard here, so no ordering constraint against
                // the route above — but it is kept in the same order as
                // macOS so the two backends read alike.
                if let Some(mac) = &route.mac
                    && let Ok(net) = route.destination.parse::<IpNet>()
                {
                    let host = net.addr().to_string();
                    cmds.push(Cmd::new(
                        "ip",
                        &["neigh", "add", &host, "lladdr", mac, "dev", iface],
                    ));
                }
            }
        }
        cmds
    }

    fn reverts_parent_config(&self) -> bool {
        // `ip addr del <cidr> dev <parent>` removes exactly what was added.
        true
    }

    fn list_devices(&self, runner: &mut dyn CommandRunner) -> Result<Vec<String>> {
        let out = runner.run(&Cmd::new("ip", &["-json", "link", "show"]))?;
        let entries: Vec<serde_json::Value> = serde_json::from_str(&out)?;
        Ok(entries
            .iter()
            .filter_map(|e| e.get("ifname")?.as_str().map(str::to_owned))
            .collect())
    }

    fn addresses_on(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<Vec<IpNet>> {
        let out = runner.run(&Cmd::new("ip", &["-json", "addr", "show", "dev", device]))?;
        let entries: Vec<serde_json::Value> = serde_json::from_str(&out)?;
        let mut nets = Vec::new();
        for e in &entries {
            for a in e
                .get("addr_info")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                if a.get("family").and_then(|f| f.as_str()) != Some("inet") {
                    continue;
                }
                let (Some(local), Some(len)) = (
                    a.get("local").and_then(|l| l.as_str()),
                    a.get("prefixlen").and_then(|p| p.as_u64()),
                ) else {
                    continue;
                };
                if let Ok(net) = format!("{local}/{len}").parse::<IpNet>() {
                    nets.push(net);
                }
            }
        }
        Ok(nets)
    }

    fn is_wireless(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool> {
        // `/sys/class/net/<dev>/wireless` exists only for wireless devices.
        // Look for it in a directory listing rather than encoding the answer
        // in an exit status (`test -d`): a runner that answers every command
        // with empty output — a dry run, an unmapped stub — then reports
        // wired, the safe default, instead of turning every NIC wireless. A
        // listing that fails outright (no such device) is still an error.
        let out = runner.run(&Cmd::new("ls", &[&format!("/sys/class/net/{device}")]))?;
        Ok(out.split_whitespace().any(|entry| entry == "wireless"))
    }

    fn link_is_active(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool> {
        // `carrier` is the recipe's proven signal (`cat carrier 2>/dev/null
        // || echo 0`). The kernel answers EINVAL for it while the interface
        // is admin-down, so the read fails; that is the answer "no link",
        // not a failure to answer. `operstate` would always be readable but
        // reports `unknown` for drivers that do not maintain it, which
        // would misfile a live NIC as inactive — so keep carrier and treat
        // an unreadable one as down.
        let out = runner.run(&Cmd::new(
            "cat",
            &[&format!("/sys/class/net/{device}/carrier")],
        ));
        Ok(out.is_ok_and(|s| s.trim() == "1"))
    }

    fn records_created_interface(&self, cmd: &Cmd) -> bool {
        cmd.program == "ip"
            && cmd.args.first().map(|a| a == "link").unwrap_or(false)
            && cmd.args.get(1).map(|a| a == "add").unwrap_or(false)
    }
}

/// The platform this build targets, for a **real (mutating)** operation —
/// `apply` or `down` without `--dry-run`.
///
/// Returns [`MacOs`] on macOS and [`Linux`] on Linux. Every other platform
/// returns an `Err`: a Windows backend is not implemented yet, and returning
/// a silently-wrong platform to run mutating commands against a real network
/// stack would be far worse than refusing. A caller that already has a
/// `Platform` for this OS (for example, an embedder with its own backend)
/// should construct it explicitly rather than calling this function.
///
/// This is deliberately not used to render a **preview** (`show`, `apply
/// --dry-run`, `down --dry-run`): a preview touches no real system and must
/// keep working on every host, so it renders through [`preview_platform`]
/// instead. See that function's doc for why the two must not be conflated.
pub fn host_platform() -> Result<Box<dyn Platform>> {
    // `consts::OS` is fixed at compile time, so this is the `cfg` dispatch it
    // reads as — written as a match so the refusal below is compiled, and
    // testable, on every host rather than only on one without a backend.
    match std::env::consts::OS {
        "macos" => Ok(Box::new(MacOs)),
        "linux" => Ok(Box::new(Linux)),
        os => Err(no_backend_error(os)),
    }
}

/// The refusal [`host_platform`] returns on a host with no backend. Its
/// wording is a small contract of its own: it must name the platforms that
/// are supported and must not leak internal planning labels, and the test
/// for that runs on every host.
fn no_backend_error(os: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "no Platform implementation for {os} yet; only '{}' and '{}' are \
         implemented today. A Windows backend is not implemented yet — \
         construct a Platform explicitly if you have one for this OS.",
        MacOs.name(),
        Linux.name(),
    )
}

/// The fixed reference platform used to render a **preview** — `show`,
/// `apply --dry-run`, `down --dry-run` — none of which touch a real system.
///
/// Unlike [`host_platform`], this never fails: a preview must keep working
/// on any host regardless of whether this build has a real backend for it,
/// exactly as it did before the `Platform` seam existed. It intentionally
/// renders macOS/BSD command syntax on every host until a real Linux or
/// Windows `Platform` exists to preview instead.
#[must_use]
pub fn preview_platform() -> &'static dyn Platform {
    &MacOs
}

/// Interface name this entry configures: `vlan<id>` for a tagged interface,
/// or the parent `device` itself for an untagged one.
///
/// `pub(crate)`: this is macOS's fixed-decision naming, kept only for the
/// free-function equivalence tests; a real caller goes through
/// [`Platform::iface_name`] instead so the naming convention isn't
/// hardcoded on the library's public surface.
pub(crate) fn iface_name(interface: &Interface, device: &str) -> String {
    match interface.vlan {
        Some(id) => format!("vlan{id}"),
        None => device.to_string(),
    }
}

/// Build the ordered bring-up commands for one interface, taking every
/// platform-specific decision from `platform`.
///
/// `Platform::bringup_commands` owns interface creation, address and MTU;
/// [`Platform::route_commands`] owns each route and its static-ARP entry.
/// This function owns only the order and the one piece of context no
/// backend can compute for itself: whether a destination is in the
/// interface's own subnet.
pub fn bringup_commands_for(
    platform: &dyn Platform,
    interface: &Interface,
    device: &str,
) -> Vec<Cmd> {
    let name = platform.iface_name(interface, device);
    let mut cmds = platform.bringup_commands(interface, device);
    for route in &interface.routes {
        let in_subnet = destination_in_subnet(&route.destination, &interface.address);
        cmds.extend(platform.route_commands(route, in_subnet, &name));
    }
    cmds
}

/// Interface creation, address assignment, and MTU only — no routes. Shared
/// by the free [`bringup_commands`] (kept as macOS's fixed-decision baseline
/// for the equivalence test) and by [`MacOs::bringup_commands`], which owns
/// only this part; routes are a [`bringup_commands_for`] concern.
fn create_address_mtu_commands(interface: &Interface, device: &str) -> Vec<Cmd> {
    let name = iface_name(interface, device);
    let addr = interface.address.addr().to_string();
    let netmask = ipv4_netmask(interface.address.prefix_len());

    let mut cmds = Vec::new();
    match interface.vlan {
        // Tagged: create the vlan pseudo-device, then bind tag+parent in a
        // SEPARATE call (a combined `create ... vlandev` leaves it unbound),
        // then assign the address.
        Some(id) => {
            let id = id.to_string();
            cmds.push(Cmd::new("ifconfig", &[&name, "create"]));
            cmds.push(Cmd::new(
                "ifconfig",
                &[&name, "vlan", &id, "vlandev", device],
            ));
            cmds.push(Cmd::new(
                "ifconfig",
                &[&name, "inet", &addr, "netmask", &netmask],
            ));
        }
        // Untagged: add the address as an alias on the parent device. `down`
        // never removes it; apply skips this interface when the address is
        // already configured.
        None => {
            cmds.push(Cmd::new(
                "ifconfig",
                &[&name, "inet", &addr, "netmask", &netmask, "alias"],
            ));
        }
    }
    if let Some(mtu) = interface.mtu {
        cmds.push(Cmd::new("ifconfig", &[&name, "mtu", &mtu.to_string()]));
    }
    cmds
}

/// Append route (and static-ARP) commands for `routes` to `cmds`, with
/// macOS's decision (`!in_subnet`) hardcoded.
///
/// `#[cfg(test)]`: this is the INDEPENDENT baseline that
/// `macos_impl_matches_the_free_functions_it_replaces` compares
/// [`MacOs::route_commands`] against, so it deliberately keeps its own copy
/// of the emission logic. Delegating to `MacOs::route_commands` would make
/// that equivalence test compare macOS against itself and pass vacuously.
/// Production route emission goes through [`Platform::route_commands`].
#[cfg(test)]
fn append_route_commands(cmds: &mut Vec<Cmd>, routes: &[Route], addr: &IpNet, iface: &str) {
    for route in routes {
        match &route.gateway {
            Some(gateway) => {
                let gateway = gateway.to_string();
                cmds.push(Cmd::new("route", &["add", &route.destination, &gateway]));
            }
            None => {
                // A gatewayless destination inside this interface's own
                // connected subnet is reached by the connected route; an
                // explicit `-host ... -interface` route would install a
                // self-MAC ARP entry that breaks resolution, so a platform
                // that shares that hazard skips it.
                let in_subnet = destination_in_subnet(&route.destination, addr);
                if !in_subnet {
                    cmds.push(interface_route_command(&route.destination, iface));
                }
                // Static ARP for an on-link host the kernel can't resolve. The
                // `-host ... -interface` route above leaves an LLINFO entry that
                // would otherwise resolve to our own MAC (a self-MAC black-hole);
                // `arp -s` overwrites it with the real MAC. (No preceding
                // `arp -d`: on macOS that deletes the freshly-added host route,
                // breaking the `arp -s`.) Validation guarantees mac => gatewayless
                // /32, so the address parse below always succeeds.
                if let Some(mac) = &route.mac
                    && let Ok(net) = route.destination.parse::<IpNet>()
                {
                    let host = net.addr().to_string();
                    cmds.push(Cmd::new("arp", &["-s", &host, mac]));
                }
            }
        }
    }
}

/// Build the ordered commands to bring up one interface on `device`.
///
/// This is macOS's hardcoded decision (`!in_subnet`), kept as the baseline
/// the equivalence test compares `bringup_commands_for(&MacOs, ...)`
/// against. [`bringup_commands_for`] is the platform-aware equivalent used
/// by every production call site (`commands::apply`/`down`/`show_plan`).
///
/// `pub(crate)` and `#[cfg(test)]`: macOS's fixed-decision baseline, kept
/// only for that equivalence test; no production caller uses this directly
/// any more.
#[cfg(test)]
pub(crate) fn bringup_commands(interface: &Interface, device: &str) -> Vec<Cmd> {
    let name = iface_name(interface, device);
    let mut cmds = create_address_mtu_commands(interface, device);
    append_route_commands(&mut cmds, &interface.routes, &interface.address, &name);
    cmds
}

/// **macOS's** names for a profile's tagged entries (`vlan<id>`), in order.
/// Untagged entries configure the parent device and are not named here.
///
/// This is not what the profile creates on every platform: Linux names a
/// sub-interface `<parent>.<id>` when that fits, and Windows differently
/// again. It must not be used to build the collision guard or anything else
/// that has to agree with what bring-up actually creates — a guard built on
/// it would never match a live `eth0.11` and would silently never fire. The
/// only correct source for such names is [`Platform::iface_name`] on the
/// resolved device, as `commands::apply` does.
///
/// `#[cfg(test)]` and private: kept for the naming test below alone; no
/// production code has any business calling it.
#[cfg(test)]
fn interface_names(profile: &Profile) -> Vec<String> {
    profile
        .interfaces
        .iter()
        .filter_map(|i| i.vlan.map(|id| format!("vlan{id}")))
        .collect()
}

/// True when `destination` (a CIDR or bare address, e.g. `192.168.11.151/32`)
/// falls entirely within the VLAN's own connected subnet `addr`. Such a
/// destination is reached by the connected route and needs no explicit route.
/// Non-CIDR destinations like `default` are never contained.
fn destination_in_subnet(destination: &str, addr: &IpNet) -> bool {
    destination
        .parse::<IpNet>()
        .map(|dest| addr.contains(&dest))
        .unwrap_or(false)
}

/// Build a `route add ... -interface <iface>` command for a destination with no
/// gateway. A single-host destination (e.g. a `/32`) uses `-host` with the bare
/// address; anything else uses `-net` with the original CIDR.
fn interface_route_command(destination: &str, iface: &str) -> Cmd {
    if destination == "default" {
        return Cmd::new("route", &["add", "default", "-interface", iface]);
    }
    if let Ok(net) = destination.parse::<IpNet>()
        && net.prefix_len() == net.max_prefix_len()
    {
        let host = net.addr().to_string();
        return Cmd::new("route", &["add", "-host", &host, "-interface", iface]);
    }
    Cmd::new("route", &["add", "-net", destination, "-interface", iface])
}

/// Commands to tear down a single interface. Destroying the vlan interface
/// also drops its addresses and routes.
///
/// `pub(crate)`: macOS's fixed-decision teardown, kept for the free-function
/// equivalence tests; a real caller goes through
/// [`Platform::teardown_commands`] instead.
pub(crate) fn teardown_commands(iface: &str) -> Vec<Cmd> {
    vec![Cmd::new("ifconfig", &[iface, "destroy"])]
}

/// Render a dotted-quad netmask from an IPv4 prefix length.
fn ipv4_netmask(prefix: u8) -> String {
    let bits: u32 = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix as u32)
    };
    format!(
        "{}.{}.{}.{}",
        (bits >> 24) & 0xff,
        (bits >> 16) & 0xff,
        (bits >> 8) & 0xff,
        bits & 0xff
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Interface, Route};
    use crate::net::RecordingRunner;
    use std::net::IpAddr;

    /// `host_platform()` must refuse cleanly (`Err`, never a panic) on a host
    /// with no `Platform` implementation at all (e.g. Windows).
    ///
    /// Linux now has a real backend (see
    /// `host_platform_resolves_to_linux_on_linux` below), so this can no
    /// longer be asserted on every non-macOS host — only on one with neither
    /// backend. The wording of the refusal is checked unconditionally in
    /// `no_backend_error_names_supported_platforms_without_phase_labels`.
    #[test]
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn host_platform_refuses_cleanly_with_no_backend() {
        let err = host_platform()
            .map(|_| ())
            .expect_err("no Platform implementation exists for this OS yet");
        assert_eq!(
            err.to_string(),
            no_backend_error(std::env::consts::OS).to_string()
        );
    }

    /// The refusal must name a supported platform and the host it refused,
    /// and must not leak internal planning labels. Tested through the helper
    /// so this holds on every host, not only on one where `host_platform()`
    /// actually refuses — that test never runs where this crate is developed.
    #[test]
    fn no_backend_error_names_supported_platforms_without_phase_labels() {
        let msg = no_backend_error("windows").to_string();
        assert!(msg.contains("windows"), "must name the host OS: {msg}");
        assert!(
            msg.contains("macos") && msg.contains("linux"),
            "expected the error to name the supported platforms: {msg}"
        );
        assert!(
            !msg.contains("1a") && !msg.contains("1b"),
            "error text must not leak internal plan-phase labels: {msg}"
        );
    }

    /// On Linux, `host_platform()` now resolves to the Linux backend instead
    /// of refusing — the complement of the refuse-cleanly case above.
    #[test]
    #[cfg(target_os = "linux")]
    fn host_platform_resolves_to_linux_on_linux() {
        let platform = host_platform().expect("Linux backend must be available on Linux");
        assert_eq!(platform.name(), "linux");
    }

    /// The preview platform must always succeed, unlike `host_platform()` —
    /// a preview must keep working on any host.
    #[test]
    fn preview_platform_is_always_available() {
        assert_eq!(preview_platform().name(), "macos");
    }

    fn tagged(id: u16, cidr: &str) -> Interface {
        Interface {
            vlan: Some(id),
            address: cidr.parse().unwrap(),
            mtu: None,
            routes: vec![],
        }
    }

    #[test]
    fn netmask_from_prefix() {
        assert_eq!(ipv4_netmask(24), "255.255.255.0");
        assert_eq!(ipv4_netmask(16), "255.255.0.0");
        assert_eq!(ipv4_netmask(8), "255.0.0.0");
    }

    #[test]
    fn tagged_creates_assigns_and_routes() {
        let mut v = tagged(100, "192.168.10.2/24");
        v.mtu = Some(1500);
        v.routes.push(Route {
            destination: "192.168.20.0/24".to_string(),
            gateway: Some("192.168.10.1".parse::<IpAddr>().unwrap()),
            mac: None,
        });
        let rendered: Vec<String> = bringup_commands(&v, "en10")
            .iter()
            .map(|c| c.display())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "ifconfig vlan100 create",
                "ifconfig vlan100 vlan 100 vlandev en10",
                "ifconfig vlan100 inet 192.168.10.2 netmask 255.255.255.0",
                "ifconfig vlan100 mtu 1500",
                "route add 192.168.20.0/24 192.168.10.1",
            ]
        );
    }

    #[test]
    fn tagged_routes_without_gateway() {
        let mut v = tagged(10, "192.168.10.90/24");
        v.routes.push(Route {
            destination: "192.168.10.150/32".to_string(),
            gateway: None,
            mac: None,
        });
        v.routes.push(Route {
            destination: "10.9.9.9/32".to_string(),
            gateway: None,
            mac: None,
        });
        v.routes.push(Route {
            destination: "239.255.0.0/24".to_string(),
            gateway: None,
            mac: None,
        });
        let rendered: Vec<String> = bringup_commands(&v, "en10")
            .iter()
            .map(|c| c.display())
            .collect();
        assert!(!rendered.iter().any(|c| c.contains("192.168.10.150"))); // in-subnet -> skipped
        assert!(rendered.contains(&"route add -host 10.9.9.9 -interface vlan10".to_string()));
        assert!(rendered.contains(&"route add -net 239.255.0.0/24 -interface vlan10".to_string()));
    }

    #[test]
    fn untagged_configures_parent_with_alias_and_routes() {
        let mut u = Interface {
            vlan: None,
            address: "192.168.1.100/24".parse().unwrap(),
            mtu: None,
            routes: vec![],
        };
        u.routes.push(Route {
            destination: "192.168.10.151/32".to_string(),
            gateway: None,
            mac: None,
        });
        let rendered: Vec<String> = bringup_commands(&u, "en16")
            .iter()
            .map(|c| c.display())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "ifconfig en16 inet 192.168.1.100 netmask 255.255.255.0 alias",
                "route add -host 192.168.10.151 -interface en16",
            ]
        );
    }

    #[test]
    fn untagged_skips_in_subnet_gatewayless_route() {
        let mut u = Interface {
            vlan: None,
            address: "192.168.10.1/24".parse().unwrap(),
            mtu: None,
            routes: vec![],
        };
        u.routes.push(Route {
            destination: "192.168.10.152/32".to_string(),
            gateway: None,
            mac: None,
        });
        let rendered: Vec<String> = bringup_commands(&u, "en16")
            .iter()
            .map(|c| c.display())
            .collect();
        assert_eq!(
            rendered,
            vec!["ifconfig en16 inet 192.168.10.1 netmask 255.255.255.0 alias"]
        );
    }

    #[test]
    fn destination_in_subnet_detects_containment() {
        let addr: IpNet = "192.168.11.87/24".parse().unwrap();
        assert!(destination_in_subnet("192.168.11.151/32", &addr));
        assert!(!destination_in_subnet("239.255.0.255/32", &addr));
        assert!(!destination_in_subnet("10.0.0.1/32", &addr));
        assert!(!destination_in_subnet("default", &addr));
    }

    #[test]
    fn teardown_destroys_interface() {
        assert_eq!(
            teardown_commands("vlan3")[0].display(),
            "ifconfig vlan3 destroy"
        );
    }

    #[test]
    fn interface_names_match_vlan_ids() {
        let p: Profile = toml::from_str(
            "name=\"t\"\n[[interface]]\nvlan=10\naddress=\"1.1.1.1/24\"\n[[interface]]\nvlan=11\naddress=\"2.2.2.2/24\"\n",
        )
        .unwrap();
        p.validate().unwrap();
        assert_eq!(interface_names(&p), vec!["vlan10", "vlan11"]);
    }

    #[test]
    fn gatewayless_host_route_with_mac_emits_static_arp() {
        let mut u = Interface {
            vlan: None,
            address: "192.168.1.100/24".parse().unwrap(),
            mtu: None,
            routes: vec![],
        };
        u.routes.push(Route {
            destination: "192.168.10.151/32".to_string(),
            gateway: None,
            mac: Some("3a:42:f7:79:32:2e".to_string()),
        });
        let cmds = bringup_commands(&u, "en16");
        let rendered: Vec<String> = cmds.iter().map(|c| c.display()).collect();
        assert_eq!(
            rendered,
            vec![
                "ifconfig en16 inet 192.168.1.100 netmask 255.255.255.0 alias",
                "route add -host 192.168.10.151 -interface en16",
                "arp -s 192.168.10.151 3a:42:f7:79:32:2e",
            ]
        );
    }

    /// A platform whose answers are the opposite of macOS on both decisions,
    /// proving the trait actually drives behavior rather than documenting it.
    struct Contrarian;

    impl Platform for Contrarian {
        fn name(&self) -> &'static str {
            "contrarian"
        }
        fn iface_name(&self, interface: &Interface, device: &str) -> String {
            match interface.vlan {
                Some(id) => format!("{device}.{id}"),
                None => device.to_string(),
            }
        }
        fn bringup_commands(&self, _i: &Interface, _d: &str) -> Vec<Cmd> {
            vec![Cmd::new("true", &[])]
        }
        fn teardown_commands(&self, iface: &str) -> Vec<Cmd> {
            vec![Cmd::new("false", &[iface])]
        }
        // Opposite of macOS's `!in_subnet`, and DERIVED from the flag
        // rather than constant, so a test can tell whether `in_subnet` is
        // actually plumbed through. The program name is deliberately
        // neither `route` nor `ip`: this is a test double proving the seam
        // is honored, not a second Linux backend.
        fn route_commands(&self, route: &Route, in_subnet: bool, iface: &str) -> Vec<Cmd> {
            if !in_subnet {
                return vec![];
            }
            vec![Cmd::new(
                "contrarian-route",
                &["add", &route.destination, "dev", iface],
            )]
        }
        fn reverts_parent_config(&self) -> bool {
            true
        }
        // Host-state reads are untested through Contrarian — it exists to
        // prove the emission/decision methods are honored, not to exercise
        // parsing. Fixed answers are enough to keep it implementing the
        // trait.
        fn list_devices(&self, _runner: &mut dyn CommandRunner) -> Result<Vec<String>> {
            Ok(vec![])
        }
        fn addresses_on(
            &self,
            _runner: &mut dyn CommandRunner,
            _device: &str,
        ) -> Result<Vec<IpNet>> {
            Ok(vec![])
        }
        fn is_wireless(&self, _runner: &mut dyn CommandRunner, _device: &str) -> Result<bool> {
            Ok(false)
        }
        fn link_is_active(&self, _runner: &mut dyn CommandRunner, _device: &str) -> Result<bool> {
            Ok(false)
        }
        fn records_created_interface(&self, cmd: &Cmd) -> bool {
            cmd.program == "ip"
                && cmd.args.first().map(|a| a == "link").unwrap_or(false)
                && cmd.args.get(1).map(|a| a == "add").unwrap_or(false)
        }
    }

    #[test]
    fn a_platform_controls_interface_naming() {
        let i = Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: None,
            routes: vec![],
        };
        assert_eq!(Contrarian.iface_name(&i, "eth0"), "eth0.11");
    }

    #[test]
    fn a_platform_derives_the_onlink_decision_from_in_subnet() {
        // Same route, both values of `in_subnet`: emission must follow the
        // flag, which is what proves `bringup_commands_for` plumbs it.
        let route = Route {
            destination: "192.168.11.200/32".to_string(),
            gateway: None,
            mac: None,
        };
        assert_eq!(
            Contrarian
                .route_commands(&route, true, "eth0.11")
                .iter()
                .map(|c| c.display())
                .collect::<Vec<_>>(),
            vec!["contrarian-route add 192.168.11.200/32 dev eth0.11"]
        );
        assert!(
            Contrarian
                .route_commands(&route, false, "eth0.11")
                .is_empty()
        );
    }

    #[test]
    fn a_platform_controls_reverting_parent_config() {
        assert!(Contrarian.reverts_parent_config());
    }

    #[test]
    fn linux_emits_ip_route_and_ip_neigh_not_bsd_route_and_arp() {
        let route = Route {
            destination: "192.168.11.151/32".to_string(),
            gateway: None,
            mac: Some("3a:42:f7:79:32:2e".to_string()),
        };
        let rendered: Vec<String> = Linux
            .route_commands(&route, true, "eth0.11")
            .iter()
            .map(|c| c.display())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "ip route add 192.168.11.151/32 dev eth0.11",
                "ip neigh add 192.168.11.151 lladdr 3a:42:f7:79:32:2e dev eth0.11",
            ],
            "got {rendered:?}"
        );
        assert!(!rendered.iter().any(|c| c.contains("arp -s")));
    }

    #[test]
    fn macos_route_emission_is_unchanged_by_the_move() {
        let route = Route {
            destination: "192.168.11.151/32".to_string(),
            gateway: None,
            mac: Some("3a:42:f7:79:32:2e".to_string()),
        };
        // In-subnet on macOS: no route at all (self-MAC black-hole), arp only.
        let rendered: Vec<String> = MacOs
            .route_commands(&route, true, "vlan11")
            .iter()
            .map(|c| c.display())
            .collect();
        assert!(
            !rendered.iter().any(|c| c.starts_with("route ")),
            "got {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|c| c == "arp -s 192.168.11.151 3a:42:f7:79:32:2e")
        );
    }

    #[test]
    fn macos_out_of_subnet_still_emits_host_route_then_arp_in_that_order() {
        let route = Route {
            destination: "192.168.10.151/32".to_string(),
            gateway: None,
            mac: Some("3a:42:f7:79:32:2e".to_string()),
        };
        let rendered: Vec<String> = MacOs
            .route_commands(&route, false, "en7")
            .iter()
            .map(|c| c.display())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "route add -host 192.168.10.151 -interface en7",
                "arp -s 192.168.10.151 3a:42:f7:79:32:2e",
            ],
            "order matters: arp -s overwrites the self-MAC entry the route installs, \
             and an `arp -d` between them deletes the route"
        );
    }

    #[test]
    fn macos_impl_matches_the_free_functions_it_replaces() {
        let tagged = Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: Some(1500),
            routes: vec![
                Route {
                    destination: "239.255.0.255/32".to_string(),
                    gateway: None,
                    mac: None,
                },
                // In-subnet gatewayless: the in-subnet decision must come
                // out the same on both sides, or this fixture would not
                // actually be exercising it (all other routes here are
                // out-of-subnet).
                Route {
                    destination: "192.168.11.151/32".to_string(),
                    gateway: None,
                    mac: None,
                },
            ],
        };
        let untagged = Interface {
            vlan: None,
            address: "192.168.1.100/24".parse().unwrap(),
            mtu: None,
            routes: vec![Route {
                destination: "192.168.10.151/32".to_string(),
                gateway: None,
                mac: Some("3a:42:f7:79:32:2e".to_string()),
            }],
        };
        for i in [&tagged, &untagged] {
            assert_eq!(
                bringup_commands_for(&MacOs, i, "en7"),
                bringup_commands(i, "en7"),
                "routing bring-up through the platform must not change macOS output"
            );
            assert_eq!(MacOs.iface_name(i, "en7"), iface_name(i, "en7"));
        }
        assert_eq!(
            MacOs.teardown_commands("vlan11"),
            teardown_commands("vlan11")
        );
    }

    #[test]
    fn macos_skips_an_in_subnet_gatewayless_route_and_keeps_parent_config() {
        let route = Route {
            destination: "192.168.11.151/32".to_string(),
            gateway: None,
            mac: None,
        };
        assert!(
            MacOs.route_commands(&route, true, "en7").is_empty(),
            "an in-subnet interface-scoped host route self-MACs on macOS"
        );
        assert!(
            !MacOs.route_commands(&route, false, "en7").is_empty(),
            "out-of-subnet still needs the route"
        );
        assert!(
            !MacOs.reverts_parent_config(),
            "untagged parent config persists by design on macOS"
        );
    }

    #[test]
    fn macos_emits_no_route_for_a_gatewayless_in_subnet_destination() {
        // Retargeted from `MacOs.bringup_commands(...)`: routes now live in
        // `bringup_commands_for`, so exercising `MacOs.bringup_commands`
        // directly would pass this assertion vacuously — no platform impl
        // emits routes any more, in-subnet or not. Going through
        // `bringup_commands_for` still exercises the real hazard this test
        // guards: `Contrarian::route_commands` proves the same route WOULD
        // be emitted for a platform that wants on-link host routes, so this
        // is the in-subnet decision actually being honored, not a route loop
        // that no longer runs.
        let i = Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: None,
            routes: vec![Route {
                destination: "192.168.11.151/32".to_string(),
                gateway: None,
                mac: None,
            }],
        };
        let rendered: Vec<String> = bringup_commands_for(&MacOs, &i, "en7")
            .iter()
            .map(|c| c.display())
            .collect();
        assert!(
            !rendered.iter().any(|c| c.starts_with("route ")),
            "an in-subnet gatewayless destination must not get an interface-scoped \
             host route (it self-MACs and black-holes on macOS): {rendered:?}"
        );

        // Guard against the assertion above becoming vacuous: the same
        // in-subnet route, through a platform that wants on-link host
        // routes, must still be emitted.
        let other: Vec<String> = bringup_commands_for(&Contrarian, &i, "eth0")
            .iter()
            .map(|c| c.display())
            .collect();
        assert!(
            other.iter().any(|c| c.contains("192.168.11.151")),
            "sanity check failed: a platform that wants on-link host routes \
             must still emit one for the same in-subnet destination: {other:?}"
        );
    }

    #[test]
    fn macos_emits_a_route_add_for_a_gatewayed_destination() {
        let i = Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: None,
            routes: vec![Route {
                destination: "192.168.20.0/24".to_string(),
                gateway: Some("192.168.11.1".parse().unwrap()),
                mac: None,
            }],
        };
        let rendered: Vec<String> = bringup_commands_for(&MacOs, &i, "en7")
            .iter()
            .map(|c| c.display())
            .collect();
        assert!(
            rendered.contains(&"route add 192.168.20.0/24 192.168.11.1".to_string()),
            "expected a route add for the gatewayed destination: {rendered:?}"
        );
    }

    #[test]
    fn a_non_ifconfig_platform_still_records_what_it_created() {
        // The bug: recording keyed off `args[1] == "create"`, which is
        // ifconfig-shaped. A Linux-shaped `ip link add link eth0 name
        // eth0.11 ...` has `link` there, so nothing was recorded and
        // rollback/down silently became no-ops.
        let create = Cmd::new(
            "ip",
            &[
                "link", "add", "link", "eth0", "name", "eth0.11", "type", "vlan", "id", "11",
            ],
        );
        assert!(
            Contrarian.records_created_interface(&create),
            "a platform must be able to recognize its own creation command"
        );
        let not_create = Cmd::new("ip", &["addr", "add", "192.168.11.87/24", "dev", "eth0.11"]);
        assert!(!Contrarian.records_created_interface(&not_create));
    }

    #[test]
    fn macos_still_recognizes_ifconfig_create_and_nothing_else() {
        assert!(MacOs.records_created_interface(&Cmd::new("ifconfig", &["vlan11", "create"])));
        assert!(!MacOs.records_created_interface(&Cmd::new(
            "ifconfig",
            &[
                "vlan11",
                "inet",
                "192.168.11.87",
                "netmask",
                "255.255.255.0"
            ]
        )));
    }

    #[test]
    fn the_collision_guard_uses_the_platforms_own_interface_names() {
        // The bug: the guard hardcoded vlan<id>, so a platform naming
        // interfaces eth0.11 was never guarded at all. Drive the real guard
        // in `commands::apply` with a platform whose naming is not macOS's,
        // and check both directions: its own name is refused, and macOS's
        // name for the same entry is not mistaken for a collision.
        let p = Profile {
            name: "t".to_owned(),
            description: None,
            device: Some("eth0".to_owned()),
            interfaces: vec![tagged(11, "192.168.11.87/24")],
        };
        let state_path = std::env::temp_dir().join("vlanctl-plan-guard-naming.json");
        let _ = std::fs::remove_file(&state_path);

        // `apply` still lists live interfaces through `ifconfig -l`
        // regardless of platform; stub it with eth0.11 already present.
        let mut r = RecordingRunner::default();
        r.stdout
            .insert("ifconfig -l".to_owned(), "lo0 eth0 eth0.11".to_owned());
        let err = crate::commands::apply(&mut r, &Contrarian, &p, &state_path, true)
            .expect_err("the platform's own interface name must be refused");
        assert!(
            err.to_string().contains("eth0.11 already exists"),
            "expected the guard to name eth0.11: {err}"
        );

        // A live vlan11 is what macOS would call this entry; under a
        // platform that names it eth0.11 it is somebody else's interface.
        let mut r = RecordingRunner::default();
        r.stdout
            .insert("ifconfig -l".to_owned(), "lo0 eth0 vlan11".to_owned());
        crate::commands::apply(&mut r, &Contrarian, &p, &state_path, true)
            .expect("a foreign naming scheme's interface is not a collision");
    }

    #[test]
    fn the_route_decision_comes_from_the_platform_not_hardcoded() {
        // A gatewayless destination INSIDE the interface's own subnet.
        // macOS must skip it; a platform that wants on-link host routes must
        // emit it. Same profile, different commands.
        let i = Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: None,
            routes: vec![Route {
                destination: "192.168.11.151/32".to_string(),
                gateway: None,
                mac: None,
            }],
        };

        let mac = bringup_commands_for(&MacOs, &i, "en7");
        assert!(
            !mac.iter().any(|c| c.display().contains("-host")),
            "macOS must not emit an in-subnet host route: {mac:?}"
        );

        let other = bringup_commands_for(&Contrarian, &i, "eth0");
        assert!(
            other.iter().any(|c| c.display().contains("192.168.11.151")),
            "a platform wanting on-link host routes must emit one: {other:?}"
        );
    }

    #[test]
    fn linux_names_vlan_interfaces_after_the_parent() {
        let i = Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: None,
            routes: vec![],
        };
        assert_eq!(Linux.iface_name(&i, "eth0"), "eth0.11");
        let untagged = Interface {
            vlan: None,
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: None,
            routes: vec![],
        };
        assert_eq!(Linux.iface_name(&untagged, "eth0"), "eth0");
    }

    /// `<parent>.<id>` is kept only while the *parent* fits
    /// [`LINUX_MAX_DOTTED_PARENT_LEN`] (10 bytes: the kernel's 15-byte
    /// `IFNAMSIZ` limit less the widest possible suffix, `.4094`); past
    /// that the name falls back to `vlan<id>`. The decision depends only on
    /// the parent's length, never on this interface's own id, so every
    /// VLAN configured on one parent gets the same naming scheme.
    #[test]
    fn linux_falls_back_to_vlan_id_when_the_dotted_name_would_overflow_ifnamsiz() {
        // A short parent is always dotted, whatever the id's width.
        assert_eq!(
            Linux.iface_name(&tagged(11, "10.0.0.1/24"), "eth0"),
            "eth0.11"
        );
        assert_eq!(
            Linux.iface_name(&tagged(4094, "10.0.0.1/24"), "eth0"),
            "eth0.4094"
        );

        // Exactly at the boundary: a 10-byte parent + the widest suffix
        // (".4094") is 15 bytes, which still fits.
        let parent10 = "enp0s31f6a";
        assert_eq!(parent10.len(), 10);
        assert_eq!(
            Linux.iface_name(&tagged(4094, "10.0.0.1/24"), parent10),
            "enp0s31f6a.4094"
        );
        assert_eq!(
            Linux.iface_name(&tagged(1, "10.0.0.1/24"), parent10),
            "enp0s31f6a.1"
        );

        // One byte over the boundary: an 11-byte parent falls back for
        // *every* id, including a short one that would itself have fit
        // (`enp0s31f6ab.999` is only 15 bytes). This is the case the old
        // per-interface rule got wrong, mixing schemes on one parent; the
        // new rule keeps both ids on the same scheme.
        let parent11 = "enp0s31f6ab";
        assert_eq!(parent11.len(), 11);
        let short_id_name = Linux.iface_name(&tagged(999, "10.0.0.1/24"), parent11);
        let max_id_name = Linux.iface_name(&tagged(4094, "10.0.0.1/24"), parent11);
        assert_eq!(short_id_name, "vlan999");
        assert_eq!(max_id_name, "vlan4094");
        // Same scheme: both fall back, neither is dotted with the parent.
        assert!(!short_id_name.starts_with(parent11));
        assert!(!max_id_name.starts_with(parent11));
        assert!(short_id_name.starts_with("vlan") && max_id_name.starts_with("vlan"));

        // A 15-byte `enx`-style parent overflows at both id-width extremes.
        let parent15 = "enx001122334455";
        assert_eq!(parent15.len(), 15);
        assert_eq!(
            Linux.iface_name(&tagged(1, "10.0.0.1/24"), parent15),
            "vlan1"
        );
        assert_eq!(
            Linux.iface_name(&tagged(4094, "10.0.0.1/24"), parent15),
            "vlan4094"
        );

        // The fallback never overflows itself, and the untagged entry is
        // still the parent, however long.
        assert!(
            Linux
                .iface_name(&tagged(4094, "10.0.0.1/24"), parent15)
                .len()
                <= 15
        );
        let untagged = Interface {
            vlan: None,
            address: "10.0.0.1/24".parse().unwrap(),
            mtu: None,
            routes: vec![],
        };
        assert_eq!(Linux.iface_name(&untagged, parent15), parent15);
    }

    /// The fallback name flows into every command that names the
    /// sub-interface — creation, up, address — because they all read it
    /// from `iface_name` rather than re-deriving it.
    #[test]
    fn linux_bringup_uses_the_fallback_name_consistently() {
        let cmds = Linux.bringup_commands(&tagged(11, "192.168.11.87/24"), "enx001122334455");
        let rendered: Vec<String> = cmds.iter().map(|c| c.display()).collect();
        assert_eq!(
            rendered,
            vec![
                "ip link set enx001122334455 up",
                "ip link add link enx001122334455 name vlan11 type vlan id 11",
                "ip link set vlan11 up",
                "ip addr add 192.168.11.87/24 dev vlan11",
            ]
        );
        assert!(
            !rendered.iter().any(|c| c.contains("enx001122334455.11")),
            "no command may re-derive the dotted name: {rendered:?}"
        );
    }

    #[test]
    fn linux_tagged_bringup_raises_the_parent_then_adds_ups_and_addresses() {
        // The parent must come up first, as the proven recipe does: on a
        // host whose NIC sits admin-down after boot, sub-interfaces created
        // on it are set up successfully yet stay LOWERLAYERDOWN — every
        // command succeeds and nothing works.
        let i = Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: Some(9000),
            routes: vec![],
        };
        let rendered: Vec<String> = Linux
            .bringup_commands(&i, "eth0")
            .iter()
            .map(|c| c.display())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "ip link set eth0 up",
                "ip link add link eth0 name eth0.11 type vlan id 11",
                "ip link set eth0.11 up",
                "ip addr add 192.168.11.87/24 dev eth0.11",
                "ip link set eth0.11 mtu 9000",
            ]
        );
    }

    #[test]
    fn linux_untagged_addresses_the_parent_and_creates_nothing() {
        let i = Interface {
            vlan: None,
            address: "192.168.1.100/24".parse().unwrap(),
            mtu: None,
            routes: vec![],
        };
        let rendered: Vec<String> = Linux
            .bringup_commands(&i, "eth0")
            .iter()
            .map(|c| c.display())
            .collect();
        assert!(
            !rendered.iter().any(|c| c.contains("link add")),
            "got {rendered:?}"
        );
        assert_eq!(rendered, vec!["ip addr add 192.168.1.100/24 dev eth0"]);
    }

    #[test]
    fn linux_teardown_deletes_the_link() {
        assert_eq!(
            Linux
                .teardown_commands("eth0.11")
                .iter()
                .map(|c| c.display())
                .collect::<Vec<_>>(),
            vec!["ip link del eth0.11"]
        );
    }

    #[test]
    fn linux_wants_the_onlink_host_route_that_macos_refuses() {
        // The platform split that justifies the seam: on Linux
        // `ip route add X/32 dev Y` makes X genuinely on-link and the kernel
        // ARPs for it, which is how several hosts sharing one subnet across
        // different VLANs are reached at all. macOS answers the opposite
        // because an interface-scoped host route there installs a self-MAC
        // entry and black-holes the traffic.
        let route = Route {
            destination: "192.168.11.151/32".to_string(),
            gateway: None,
            mac: None,
        };
        assert!(!Linux.route_commands(&route, true, "eth0.11").is_empty());
        assert!(!Linux.route_commands(&route, false, "eth0.11").is_empty());
        assert!(MacOs.route_commands(&route, true, "vlan11").is_empty());
    }

    #[test]
    fn linux_reverts_parent_config_because_it_can_delete_one_address() {
        // `ip addr del <cidr> dev <parent>` removes exactly the address we
        // added, unlike macOS where an alias teardown needs `-alias` the
        // generic path cannot express.
        assert!(Linux.reverts_parent_config());
    }

    #[test]
    fn linux_recognizes_its_own_creation_command() {
        assert!(Linux.records_created_interface(&Cmd::new(
            "ip",
            &[
                "link", "add", "link", "eth0", "name", "eth0.11", "type", "vlan", "id", "11"
            ]
        )));
        assert!(!Linux.records_created_interface(&Cmd::new(
            "ip",
            &["addr", "add", "192.168.11.87/24", "dev", "eth0.11"]
        )));
        // Raising the parent creates nothing, so it must not be recorded —
        // otherwise rollback would `ip link del` the physical NIC.
        assert!(!Linux.records_created_interface(&Cmd::new("ip", &["link", "set", "eth0", "up"])));
    }

    #[test]
    fn linux_link_is_active_reads_carrier() {
        let mut r = RecordingRunner::default();
        r.stdout.insert(
            "cat /sys/class/net/eth0/carrier".to_string(),
            "1\n".to_string(),
        );
        r.stdout.insert(
            "cat /sys/class/net/eth1/carrier".to_string(),
            "0\n".to_string(),
        );
        assert!(Linux.link_is_active(&mut r, "eth0").unwrap());
        assert!(!Linux.link_is_active(&mut r, "eth1").unwrap());
    }

    #[test]
    fn linux_link_is_active_treats_an_unreadable_carrier_as_down() {
        // The kernel answers EINVAL for `carrier` on an admin-down
        // interface, so `cat` exits non-zero and the runner reports an
        // error. That is the answer "no link", not a failure to answer;
        // propagating it would abort device detection on any host with an
        // idle NIC.
        let mut r = RecordingRunner {
            fail_at: Some(0),
            ..Default::default()
        };
        assert!(
            !Linux
                .link_is_active(&mut r, "eth0")
                .expect("an unreadable carrier is not an error"),
        );
        assert_eq!(
            r.commands.len(),
            1,
            "the failed read must have been attempted"
        );
    }

    #[test]
    fn linux_is_wireless_looks_for_the_wireless_sysfs_entry() {
        let mut r = RecordingRunner::default();
        r.stdout.insert(
            "ls /sys/class/net/wlan0".to_string(),
            "address\ncarrier\nphy80211\nwireless\n".to_string(),
        );
        r.stdout.insert(
            "ls /sys/class/net/eth0".to_string(),
            "address\ncarrier\ndevice\n".to_string(),
        );
        assert!(Linux.is_wireless(&mut r, "wlan0").unwrap());
        assert!(!Linux.is_wireless(&mut r, "eth0").unwrap());
    }

    #[test]
    fn linux_is_wireless_defaults_to_wired_for_an_unstubbed_device() {
        // The answer comes from the listing's contents, not from whether the
        // command succeeded: a runner that answers every command with empty
        // output (a dry run, an unmapped stub) must not turn every NIC
        // wireless.
        let mut r = RecordingRunner::default();
        assert!(!Linux.is_wireless(&mut r, "eth0").unwrap());
    }

    #[test]
    fn linux_host_state_reads_use_ip_json_not_ifconfig() {
        let mut r = RecordingRunner::default();
        r.stdout.insert(
            "ip -json link show".to_string(),
            r#"[{"ifname":"lo"},{"ifname":"eth0"}]"#.to_string(),
        );
        assert_eq!(Linux.list_devices(&mut r).unwrap(), vec!["lo", "eth0"]);
    }

    #[test]
    fn linux_addresses_on_keeps_only_complete_inet_entries() {
        let mut r = RecordingRunner::default();
        r.stdout.insert(
            "ip -json addr show dev eth0".to_string(),
            r#"[{
                "ifname": "eth0",
                "addr_info": [
                    {"family": "inet", "local": "192.168.11.87", "prefixlen": 24},
                    {"family": "inet6", "local": "fe80::1", "prefixlen": 64},
                    {"family": "inet", "prefixlen": 24},
                    {"family": "inet", "local": "192.168.11.88"}
                ]
            }]"#
            .to_string(),
        );
        let nets = Linux.addresses_on(&mut r, "eth0").unwrap();
        assert_eq!(
            nets,
            vec!["192.168.11.87/24".parse::<IpNet>().unwrap()],
            "expected the inet6 entry excluded and both incomplete inet \
             entries (missing local, missing prefixlen) skipped: {nets:?}"
        );
    }
}
