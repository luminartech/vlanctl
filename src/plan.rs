use crate::config::{Interface, Profile, Route};
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
/// one that happens to compile.
pub trait Platform {
    /// Short identifier for logs and error messages, e.g. `"macos"`.
    fn name(&self) -> &'static str;

    /// Interface name for a profile entry. `vlan11` on macOS, `eth0.11` on
    /// Linux, `vEthernet (IrisVlan11)` on Windows. Untagged entries return
    /// the parent device.
    fn iface_name(&self, interface: &Interface, device: &str) -> String;

    /// Ordered commands to bring one interface up: interface creation,
    /// address, and MTU. Route emission is shared logic that lives in
    /// [`bringup_commands_for`], gated on [`Platform::wants_onlink_host_route`],
    /// so implementations must not include routes here.
    fn bringup_commands(&self, interface: &Interface, device: &str) -> Vec<Cmd>;

    /// Commands to tear one interface down.
    fn teardown_commands(&self, iface: &str) -> Vec<Cmd>;

    /// Whether a **gatewayless** route should emit an interface-scoped host
    /// route when the destination is `in_subnet`.
    ///
    /// macOS answers `false` for an in-subnet destination: an
    /// interface-scoped host route there installs a self-MAC LLINFO entry
    /// and black-holes the traffic, so the connected route must be left to
    /// resolve it. Linux answers `true` — `ip route add X/32 dev Y` makes the
    /// destination genuinely on-link, and it is the only way to reach
    /// several hosts that share one subnet across different VLANs.
    fn wants_onlink_host_route(&self, in_subnet: bool) -> bool;

    /// Whether teardown may revert configuration applied to the **parent**
    /// device (as opposed to a VLAN sub-interface it created).
    ///
    /// macOS answers `false`: an untagged entry aliases an address onto the
    /// physical device and that persists by design. Windows must answer
    /// `true`, because its untagged equivalent is a vSwitch binding that
    /// re-plumbs the NIC and cannot be left behind.
    fn reverts_parent_config(&self) -> bool;

    /// Names of every network device currently on the host, from `ifconfig -l`.
    fn list_devices(&self, runner: &mut dyn CommandRunner) -> Result<Vec<String>>;

    /// IPv4 addresses currently configured on `device`, from the `inet` lines
    /// of `ifconfig <device>`.
    fn addresses_on(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<Vec<IpNet>>;

    /// Whether `device`'s hardware port is Wi-Fi, from
    /// `networksetup -listallhardwareports`.
    fn is_wireless(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool>;

    /// Whether `ifconfig <device>` reports `status: active`.
    fn link_is_active(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool>;
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

    fn wants_onlink_host_route(&self, in_subnet: bool) -> bool {
        // An interface-scoped host route to an in-subnet destination installs
        // a permanent self-MAC LLINFO entry and black-holes the traffic; the
        // connected route resolves it correctly instead.
        !in_subnet
    }

    fn reverts_parent_config(&self) -> bool {
        // An untagged entry aliases an address onto the physical device.
        // Teardown deliberately leaves it: the device may carry unrelated
        // configuration this crate did not create.
        false
    }

    fn list_devices(&self, runner: &mut dyn CommandRunner) -> Result<Vec<String>> {
        crate::commands::live_interfaces(runner)
    }

    fn addresses_on(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<Vec<IpNet>> {
        crate::commands::device_inet_addresses(runner, device)
    }

    fn is_wireless(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool> {
        Ok(crate::device::wifi_devices(runner)?
            .iter()
            .any(|w| w == device))
    }

    fn link_is_active(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool> {
        crate::device::interface_is_active(runner, device)
    }
}

/// The platform this build targets.
///
/// Returns [`MacOs`] on macOS. Other platforms are unimplemented until 1b
/// (Linux) and 3 (Windows) — a caller on those platforms must construct a
/// `Platform` explicitly rather than relying on this.
#[must_use]
pub fn host_platform() -> Box<dyn Platform> {
    #[cfg(target_os = "macos")]
    {
        Box::new(MacOs)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Deliberate: 1a adds the seam, not the backends. Returning a
        // silently-wrong platform would be far worse than refusing.
        unimplemented!(
            "no Platform implementation for this OS yet; \
             construct one explicitly (Linux lands in 1b, Windows in 3)"
        )
    }
}

/// Interface name this entry configures: `vlan<id>` for a tagged interface,
/// or the parent `device` itself for an untagged one.
pub fn iface_name(interface: &Interface, device: &str) -> String {
    match interface.vlan {
        Some(id) => format!("vlan{id}"),
        None => device.to_string(),
    }
}

/// Build the ordered bring-up commands for one interface, taking every
/// platform-specific decision from `platform`.
///
/// `Platform::bringup_commands` owns interface creation, address and MTU;
/// routes are shared logic gated on one platform decision
/// ([`Platform::wants_onlink_host_route`]), so this function appends them
/// once here rather than duplicating the loop in every backend.
pub fn bringup_commands_for(
    platform: &dyn Platform,
    interface: &Interface,
    device: &str,
) -> Vec<Cmd> {
    let name = platform.iface_name(interface, device);
    let mut cmds = platform.bringup_commands(interface, device);
    append_route_commands(
        &mut cmds,
        &interface.routes,
        &interface.address,
        &name,
        |in_subnet| platform.wants_onlink_host_route(in_subnet),
    );
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

/// Append route (and static-ARP) commands for `routes` to `cmds`. Shared by
/// the free [`bringup_commands`] (macOS's hardcoded decision) and
/// [`bringup_commands_for`] (the platform's decision), so the route-building
/// logic exists exactly once.
fn append_route_commands(
    cmds: &mut Vec<Cmd>,
    routes: &[Route],
    addr: &IpNet,
    iface: &str,
    wants_onlink_host_route: impl Fn(bool) -> bool,
) {
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
                if wants_onlink_host_route(in_subnet) {
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
pub fn bringup_commands(interface: &Interface, device: &str) -> Vec<Cmd> {
    let name = iface_name(interface, device);
    let mut cmds = create_address_mtu_commands(interface, device);
    append_route_commands(
        &mut cmds,
        &interface.routes,
        &interface.address,
        &name,
        |in_subnet| !in_subnet,
    );
    cmds
}

/// Names of the VLAN sub-interfaces this profile creates (`vlan<id>`), in order.
/// Untagged interfaces configure the parent device and are not named here, so
/// they are never recorded in state or torn down.
pub fn interface_names(profile: &Profile) -> Vec<String> {
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
pub fn teardown_commands(iface: &str) -> Vec<Cmd> {
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
    use std::net::IpAddr;

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
        // Opposite of macOS's `!in_subnet`: derived from the flag, not a
        // constant, so a test can tell whether the parameter is actually
        // plumbed through.
        fn wants_onlink_host_route(&self, in_subnet: bool) -> bool {
            in_subnet
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
        assert!(Contrarian.wants_onlink_host_route(true));
        assert!(!Contrarian.wants_onlink_host_route(false));
    }

    #[test]
    fn a_platform_controls_reverting_parent_config() {
        assert!(Contrarian.reverts_parent_config());
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
                // In-subnet gatewayless: `wants_onlink_host_route` must
                // answer the same on both sides, or this fixture would not
                // actually be exercising that decision (all other routes
                // here are out-of-subnet).
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
        assert!(
            !MacOs.wants_onlink_host_route(true),
            "an in-subnet interface-scoped host route self-MACs on macOS"
        );
        assert!(
            MacOs.wants_onlink_host_route(false),
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
        // guards: `Contrarian::wants_onlink_host_route` proves the same route
        // WOULD be emitted for a platform that wants on-link host routes, so
        // this is `wants_onlink_host_route` actually being honored, not a
        // route loop that no longer runs.
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
            other.iter().any(|c| c.starts_with("route ")),
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
}
