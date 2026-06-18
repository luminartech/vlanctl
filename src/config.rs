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
    /// Next-hop gateway. If omitted, the route is scoped to the VLAN's own
    /// interface (`route add ... -interface vlanN`) instead of a gateway.
    #[serde(default)]
    pub gateway: Option<IpAddr>,
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
        let err = profile_with("[[interface]]\nvlan = 5000\naddress = \"1.1.1.1/24\"\n").unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn rejects_empty_vlan_list() {
        let err = profile_with("").unwrap_err();
        assert!(err.to_string().contains("no [[interface]] entries"));
    }

    #[test]
    fn rejects_ipv6_address() {
        let err = profile_with("[[interface]]\nvlan = 100\naddress = \"fe80::1/64\"\n").unwrap_err();
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
}
