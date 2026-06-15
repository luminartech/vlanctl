use crate::config::{Profile, Vlan};
use crate::net::Cmd;
use ipnet::IpNet;

/// Build the ordered commands to bring up a single VLAN on `device`,
/// using the interface name `iface` (e.g. "vlan0").
pub fn bringup_commands(iface: &str, device: &str, vlan: &Vlan) -> Vec<Cmd> {
    let id = vlan.id.to_string();
    let addr = vlan.address.addr().to_string();
    let netmask = ipv4_netmask(vlan.address.prefix_len());

    // macOS applies the 802.1Q tag and parent binding in a *separate* call from
    // `create`: a combined `create vlan <id> vlandev <dev>` creates the pseudo-
    // device but silently leaves it unbound (vlan 0, parent <none>). `vlan` and
    // `vlandev` must both be set together, after the interface exists.
    let mut cmds = vec![
        Cmd::new("ifconfig", &[iface, "create"]),
        Cmd::new("ifconfig", &[iface, "vlan", &id, "vlandev", device]),
        Cmd::new("ifconfig", &[iface, "inet", &addr, "netmask", &netmask]),
    ];
    if let Some(mtu) = vlan.mtu {
        cmds.push(Cmd::new("ifconfig", &[iface, "mtu", &mtu.to_string()]));
    }
    for route in &vlan.routes {
        match &route.gateway {
            // Gateway route: next-hop is an explicit address.
            Some(gateway) => {
                let gateway = gateway.to_string();
                cmds.push(Cmd::new("route", &["add", &route.destination, &gateway]));
            }
            // Interface-scoped route via this VLAN's own interface. Required
            // when several VLANs share a subnet, and for per-interface
            // multicast (e.g. SOME/IP-SD).
            None => cmds.push(interface_route_command(&route.destination, iface)),
        }
    }
    cmds
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

/// Interface name for each VLAN: `vlan<id>`, so the kernel interface number
/// matches the 802.1Q id (VLAN 10 -> `vlan10`). VLAN ids are unique within a
/// profile, so the resulting names are unique too.
pub fn interface_names(profile: &Profile) -> Vec<String> {
    profile
        .vlans
        .iter()
        .map(|v| format!("vlan{}", v.id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Route, Vlan};
    use std::net::IpAddr;

    fn vlan(id: u16, cidr: &str) -> Vlan {
        Vlan {
            id,
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
    fn bringup_creates_assigns_and_routes() {
        let mut v = vlan(100, "192.168.10.2/24");
        v.mtu = Some(1500);
        v.routes.push(Route {
            destination: "192.168.20.0/24".to_string(),
            gateway: Some("192.168.10.1".parse::<IpAddr>().unwrap()),
        });
        let cmds = bringup_commands("vlan0", "en10", &v);
        let rendered: Vec<String> = cmds.iter().map(|c| c.display()).collect();
        assert_eq!(
            rendered,
            vec![
                "ifconfig vlan0 create",
                "ifconfig vlan0 vlan 100 vlandev en10",
                "ifconfig vlan0 inet 192.168.10.2 netmask 255.255.255.0",
                "ifconfig vlan0 mtu 1500",
                "route add 192.168.20.0/24 192.168.10.1",
            ]
        );
    }

    #[test]
    fn interface_routes_without_gateway() {
        let mut v = vlan(10, "192.168.10.90/24");
        // Host route to the sensor, pinned to this VLAN's interface.
        v.routes.push(Route {
            destination: "192.168.10.150/32".to_string(),
            gateway: None,
        });
        // A network route (non-/32) uses -net with the CIDR.
        v.routes.push(Route {
            destination: "239.255.0.0/24".to_string(),
            gateway: None,
        });
        let rendered: Vec<String> = bringup_commands("vlan0", "en10", &v)
            .iter()
            .map(|c| c.display())
            .collect();
        assert!(rendered.contains(&"route add -host 192.168.10.150 -interface vlan0".to_string()));
        assert!(rendered.contains(&"route add -net 239.255.0.0/24 -interface vlan0".to_string()));
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
            "name=\"t\"\n[[vlan]]\nid=10\naddress=\"1.1.1.1/24\"\n[[vlan]]\nid=11\naddress=\"2.2.2.2/24\"\n",
        )
        .unwrap();
        p.validate().unwrap();
        assert_eq!(interface_names(&p), vec!["vlan10", "vlan11"]);
    }
}
