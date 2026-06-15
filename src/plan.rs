use crate::config::{Profile, Vlan};
use crate::net::Cmd;

/// One VLAN's bring-up: the interface name chosen plus its ordered commands.
pub struct VlanBringup {
    pub interface: String,
    pub commands: Vec<Cmd>,
}

/// Build the ordered commands to bring up a single VLAN on `device`,
/// using the interface name `iface` (e.g. "vlan0").
pub fn bringup_commands(iface: &str, device: &str, vlan: &Vlan) -> Vec<Cmd> {
    let id = vlan.id.to_string();
    let addr = vlan.address.addr().to_string();
    let netmask = ipv4_netmask(vlan.address.prefix_len());

    let mut cmds = vec![
        Cmd::new("ifconfig", &[iface, "create", "vlan", &id, "vlandev", device]),
        Cmd::new("ifconfig", &[iface, "inet", &addr, "netmask", &netmask]),
    ];
    if let Some(mtu) = vlan.mtu {
        cmds.push(Cmd::new("ifconfig", &[iface, "mtu", &mtu.to_string()]));
    }
    for route in &vlan.routes {
        cmds.push(Cmd::new(
            "route",
            &["add", &route.destination, &route.gateway.to_string()],
        ));
    }
    cmds
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

/// Allocate interface names for a profile's VLANs, starting at the first free
/// vlan unit number not already present in `existing` (live interface names).
pub fn allocate_interfaces(profile: &Profile, existing: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    let mut unit = 0u32;
    for _ in &profile.vlans {
        loop {
            let candidate = format!("vlan{unit}");
            if !existing.contains(&candidate) && !names.contains(&candidate) {
                names.push(candidate);
                unit += 1;
                break;
            }
            unit += 1;
        }
    }
    names
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
            gateway: "192.168.10.1".parse::<IpAddr>().unwrap(),
        });
        let cmds = bringup_commands("vlan0", "en10", &v);
        let rendered: Vec<String> = cmds.iter().map(|c| c.display()).collect();
        assert_eq!(
            rendered,
            vec![
                "ifconfig vlan0 create vlan 100 vlandev en10",
                "ifconfig vlan0 inet 192.168.10.2 netmask 255.255.255.0",
                "ifconfig vlan0 mtu 1500",
                "route add 192.168.20.0/24 192.168.10.1",
            ]
        );
    }

    #[test]
    fn teardown_destroys_interface() {
        assert_eq!(teardown_commands("vlan3")[0].display(), "ifconfig vlan3 destroy");
    }

    #[test]
    fn allocate_skips_existing_units() {
        let mut p: Profile = toml::from_str(
            "name=\"t\"\n[[vlan]]\nid=1\naddress=\"1.1.1.1/24\"\n[[vlan]]\nid=2\naddress=\"2.2.2.2/24\"\n",
        )
        .unwrap();
        p.validate().unwrap();
        let names = allocate_interfaces(&p, &["vlan0".to_string()]);
        assert_eq!(names, vec!["vlan1", "vlan2"]);
    }
}
