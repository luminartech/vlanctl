use anyhow::{Context, Result, bail};
use ipnet::IpNet;
use serde::Deserialize;
use std::net::IpAddr;
use std::path::Path;

#[derive(Debug, Deserialize, PartialEq)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default, rename = "interface")]
    pub interfaces: Vec<Interface>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Interface {
    /// 802.1Q VLAN tag. `None` means untagged: configure the parent device directly.
    #[serde(default)]
    pub vlan: Option<u16>,
    pub address: IpNet,
    #[serde(default)]
    pub mtu: Option<u32>,
    #[serde(default, rename = "route")]
    pub routes: Vec<Route>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Route {
    pub destination: String,
    /// Next-hop gateway. If omitted, the route is scoped to the owning
    /// interface (`route add ... -interface <iface>`) instead of a gateway.
    #[serde(default)]
    pub gateway: Option<IpAddr>,
    /// Static ARP entry (`arp -s <host> <mac>`) for an on-link host the kernel
    /// cannot resolve itself. Only valid on a gatewayless /32 route.
    #[serde(default)]
    pub mac: Option<String>,
}

impl Profile {
    /// Load and validate a profile from a TOML file.
    pub fn load(path: &Path) -> Result<Profile> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading profile {}", path.display()))?;
        let profile: Profile =
            toml::from_str(&text).with_context(|| format!("parsing profile {}", path.display()))?;
        profile.validate()?;
        Ok(profile)
    }

    /// Semantic validation beyond what the type system enforces.
    pub fn validate(&self) -> Result<()> {
        if self.interfaces.is_empty() {
            bail!("profile '{}' has no [[interface]] entries", self.name);
        }
        let mut seen = std::collections::HashSet::new();
        let mut untagged = 0;
        for iface in &self.interfaces {
            match iface.vlan {
                None => untagged += 1,
                Some(id) => {
                    if !(1..=4094).contains(&id) {
                        bail!("vlan id {} out of range (1..=4094)", id);
                    }
                    if !seen.insert(id) {
                        bail!("duplicate vlan id {} in profile '{}'", id, self.name);
                    }
                }
            }
            // Only IPv4 is supported: netmask computation assumes a /0../32 prefix.
            if !iface.address.addr().is_ipv4() {
                bail!(
                    "interface address {} is not IPv4 (IPv6 is unsupported)",
                    iface.address
                );
            }
            for route in &iface.routes {
                if route.destination != "default" && route.destination.parse::<IpNet>().is_err() {
                    bail!(
                        "route destination '{}' is not a CIDR or \"default\"",
                        route.destination
                    );
                }
                // `ip route add <dest> via <gw> dev <iface>` requires the
                // gateway to be on-link on that interface — the kernel
                // rejects anything else with EINVAL ("Nexthop has invalid
                // gateway") part-way through an apply, forcing a rollback.
                // macOS's gateway-only `route add <dest> <gw>` resolved it
                // through the routing table, so this shape was previously
                // accepted there; reject it here, where the message can
                // explain itself. Every gateway in the shipped profiles is
                // on-link, so nothing in-tree is affected.
                if let Some(gateway) = &route.gateway
                    && !iface.address.contains(gateway)
                {
                    bail!(
                        "route '{}' has gateway {} outside the interface's own subnet {} \
                         (the gateway must be on-link on the interface the route is \
                         pinned to)",
                        route.destination,
                        gateway,
                        iface.address
                    );
                }
                if let Some(mac) = &route.mac {
                    if route.gateway.is_some() {
                        bail!(
                            "route '{}' has both a gateway and a mac (mutually exclusive)",
                            route.destination
                        );
                    }
                    let is_host = route
                        .destination
                        .parse::<IpNet>()
                        .map(|n| n.prefix_len() == n.max_prefix_len())
                        .unwrap_or(false);
                    if !is_host {
                        bail!(
                            "route '{}' has a mac but is not a single host (/32 required)",
                            route.destination
                        );
                    }
                    if !is_valid_mac(mac) {
                        bail!("route '{}' has an invalid MAC '{}'", route.destination, mac);
                    }
                }
            }
        }
        if untagged > 1 {
            bail!(
                "profile '{}' has {} untagged interfaces (at most one allowed)",
                self.name,
                untagged
            );
        }
        Ok(())
    }
}

/// True if `s` is six colon-separated groups of one or two hex digits.
fn is_valid_mac(s: &str) -> bool {
    let groups: Vec<&str> = s.split(':').collect();
    groups.len() == 6
        && groups
            .iter()
            .all(|g| (1..=2).contains(&g.len()) && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_vlan_profile() {
        let toml = r#"
name = "iris_bench"
description = "bench"

[[interface]]
vlan = 100
address = "192.168.10.2/24"

  [[interface.route]]
  destination = "192.168.20.0/24"
  gateway = "192.168.10.1"

[[interface]]
vlan = 200
address = "10.0.0.5/24"
"#;
        let p: Profile = toml::from_str(toml).unwrap();
        assert_eq!(p.name, "iris_bench");
        assert_eq!(p.interfaces.len(), 2);
        assert_eq!(p.interfaces[0].vlan, Some(100));
        assert_eq!(p.interfaces[0].routes.len(), 1);
        assert_eq!(p.interfaces[1].routes.len(), 0);
    }

    fn profile_with(vlans: &str) -> Result<Profile> {
        let toml = format!("name = \"t\"\n{vlans}");
        let p: Profile = toml::from_str(&toml)?;
        p.validate()?;
        Ok(p)
    }

    #[test]
    fn rejects_a_gateway_outside_the_interfaces_own_subnet() {
        // `ip route add <dest> via <gw> dev <iface>` requires the gateway to
        // be on-link on that interface; the kernel answers EINVAL ("Nexthop
        // has invalid gateway") otherwise, mid-apply, and the whole apply
        // rolls back. macOS's `route add <dest> <gw>` resolved the gateway
        // through the routing table instead, so this shape used to work
        // there. Reject it at parse time, where the message can say why.
        let err = profile_with(
            "[[interface]]\nvlan = 11\naddress = \"192.168.11.87/24\"\n\n  \
             [[interface.route]]\n  destination = \"192.168.20.0/24\"\n  \
             gateway = \"10.0.0.1\"\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("gateway") && msg.contains("10.0.0.1"),
            "the error must name the offending gateway: {msg}"
        );
    }

    #[test]
    fn accepts_a_gateway_inside_the_interfaces_own_subnet() {
        // Guards the rule above against over-rejection: every gateway in the
        // shipped profiles is on-link, and they must keep parsing.
        profile_with(
            "[[interface]]\nvlan = 100\naddress = \"192.168.10.2/24\"\n\n  \
             [[interface.route]]\n  destination = \"192.168.20.0/24\"\n  \
             gateway = \"192.168.10.1\"\n",
        )
        .expect("an on-link gateway must be accepted");
    }

    #[test]
    fn rejects_duplicate_vlan_ids() {
        let err = profile_with(
            "[[interface]]\nvlan = 100\naddress = \"1.1.1.1/24\"\n\
             [[interface]]\nvlan = 100\naddress = \"2.2.2.2/24\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate vlan id 100"));
    }

    #[test]
    fn rejects_out_of_range_id() {
        let err =
            profile_with("[[interface]]\nvlan = 5000\naddress = \"1.1.1.1/24\"\n").unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn rejects_empty_vlan_list() {
        let err = profile_with("").unwrap_err();
        assert!(err.to_string().contains("no [[interface]] entries"));
    }

    #[test]
    fn rejects_ipv6_address() {
        let err =
            profile_with("[[interface]]\nvlan = 100\naddress = \"fe80::1/64\"\n").unwrap_err();
        assert!(err.to_string().contains("not IPv4"));
    }

    #[test]
    fn rejects_invalid_route_destination() {
        let err = profile_with(
            "[[interface]]\nvlan = 100\naddress = \"192.168.1.2/24\"\n\
             [[interface.route]]\ndestination = \"not-a-cidr\"\ngateway = \"192.168.1.1\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a CIDR"));
    }

    #[test]
    fn accepts_default_route_destination() {
        profile_with(
            "[[interface]]\nvlan = 100\naddress = \"192.168.1.2/24\"\n\
             [[interface.route]]\ndestination = \"default\"\ngateway = \"192.168.1.1\"\n",
        )
        .unwrap();
    }

    #[test]
    fn route_without_gateway_is_interface_scoped() {
        let p = profile_with(
            "[[interface]]\nvlan = 10\naddress = \"192.168.10.90/24\"\n\
             [[interface.route]]\ndestination = \"192.168.10.150/32\"\n",
        )
        .unwrap();
        assert_eq!(p.interfaces[0].routes[0].gateway, None);
        assert_eq!(p.interfaces[0].routes[0].destination, "192.168.10.150/32");
    }

    #[test]
    fn parses_untagged_interface() {
        let p = profile_with(
            "[[interface]]\naddress = \"192.168.1.100/24\"\n\
             [[interface.route]]\ndestination = \"192.168.10.151/32\"\n\
             [[interface]]\nvlan = 12\naddress = \"192.168.10.1/24\"\n",
        )
        .unwrap();
        assert_eq!(p.interfaces[0].vlan, None);
        assert_eq!(p.interfaces[0].routes.len(), 1);
        assert_eq!(p.interfaces[1].vlan, Some(12));
    }

    #[test]
    fn rejects_two_untagged_interfaces() {
        let err = profile_with(
            "[[interface]]\naddress = \"192.168.1.100/24\"\n\
             [[interface]]\naddress = \"192.168.2.100/24\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("at most one"));
    }

    #[test]
    fn accepts_mac_on_gatewayless_host_route() {
        let p = profile_with(
            "[[interface]]\naddress = \"192.168.1.100/24\"\n\
             [[interface.route]]\ndestination = \"192.168.10.151/32\"\nmac = \"00:00:5e:00:53:01\"\n",
        )
        .unwrap();
        assert_eq!(
            p.interfaces[0].routes[0].mac.as_deref(),
            Some("00:00:5e:00:53:01")
        );
    }

    #[test]
    fn rejects_mac_with_gateway() {
        let err = profile_with(
            "[[interface]]\naddress = \"192.168.1.2/24\"\n\
             [[interface.route]]\ndestination = \"192.168.10.151/32\"\ngateway = \"192.168.1.1\"\nmac = \"00:00:5e:00:53:01\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("gateway"));
    }

    #[test]
    fn rejects_mac_on_non_host_route() {
        let err = profile_with(
            "[[interface]]\naddress = \"192.168.1.2/24\"\n\
             [[interface.route]]\ndestination = \"192.168.10.0/24\"\nmac = \"00:00:5e:00:53:01\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("/32") || err.to_string().contains("single host"));
    }

    #[test]
    fn rejects_malformed_mac() {
        let err = profile_with(
            "[[interface]]\naddress = \"192.168.1.2/24\"\n\
             [[interface.route]]\ndestination = \"192.168.10.151/32\"\nmac = \"not-a-mac\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("MAC") || err.to_string().contains("mac"));
    }
}
