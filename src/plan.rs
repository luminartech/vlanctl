use crate::config::{Interface, Profile};
use crate::net::Cmd;
use ipnet::IpNet;

/// Interface name this entry configures: `vlan<id>` for a tagged interface,
/// or the parent `device` itself for an untagged one.
pub fn iface_name(interface: &Interface, device: &str) -> String {
    match interface.vlan {
        Some(id) => format!("vlan{id}"),
        None => device.to_string(),
    }
}

/// Build the ordered commands to bring up one interface on `device`.
pub fn bringup_commands(interface: &Interface, device: &str) -> Vec<Cmd> {
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
            cmds.push(Cmd::new("ifconfig", &[&name, "vlan", &id, "vlandev", device]));
            cmds.push(Cmd::new("ifconfig", &[&name, "inet", &addr, "netmask", &netmask]));
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
    for route in &interface.routes {
        match &route.gateway {
            Some(gateway) => {
                let gateway = gateway.to_string();
                cmds.push(Cmd::new("route", &["add", &route.destination, &gateway]));
            }
            // A gatewayless destination inside this interface's own connected
            // subnet is reached by the connected route; an explicit
            // `-host ... -interface` route would install a self-MAC ARP entry
            // that breaks resolution, so it is skipped.
            None if destination_in_subnet(&route.destination, &interface.address) => {}
            None => cmds.push(interface_route_command(&route.destination, &name)),
        }
    }
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
        Interface { vlan: Some(id), address: cidr.parse().unwrap(), mtu: None, routes: vec![] }
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
        });
        let rendered: Vec<String> = bringup_commands(&v, "en10").iter().map(|c| c.display()).collect();
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
        v.routes.push(Route { destination: "192.168.10.150/32".to_string(), gateway: None });
        v.routes.push(Route { destination: "10.9.9.9/32".to_string(), gateway: None });
        v.routes.push(Route { destination: "239.255.0.0/24".to_string(), gateway: None });
        let rendered: Vec<String> = bringup_commands(&v, "en10").iter().map(|c| c.display()).collect();
        assert!(!rendered.iter().any(|c| c.contains("192.168.10.150"))); // in-subnet -> skipped
        assert!(rendered.contains(&"route add -host 10.9.9.9 -interface vlan10".to_string()));
        assert!(rendered.contains(&"route add -net 239.255.0.0/24 -interface vlan10".to_string()));
    }

    #[test]
    fn untagged_configures_parent_with_alias_and_routes() {
        let mut u = Interface { vlan: None, address: "192.168.1.100/24".parse().unwrap(), mtu: None, routes: vec![] };
        u.routes.push(Route { destination: "192.168.10.151/32".to_string(), gateway: None });
        let rendered: Vec<String> = bringup_commands(&u, "en16").iter().map(|c| c.display()).collect();
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
        let mut u = Interface { vlan: None, address: "192.168.10.1/24".parse().unwrap(), mtu: None, routes: vec![] };
        u.routes.push(Route { destination: "192.168.10.152/32".to_string(), gateway: None });
        let rendered: Vec<String> = bringup_commands(&u, "en16").iter().map(|c| c.display()).collect();
        assert_eq!(rendered, vec!["ifconfig en16 inet 192.168.10.1 netmask 255.255.255.0 alias"]);
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
}
