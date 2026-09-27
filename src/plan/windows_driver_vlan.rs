//! Windows backend that uses the adapter driver's own 802.1Q setting: one
//! VLAN per physical adapter, no Hyper-V.
//!
//! # The model
//!
//! NDIS defines a standardized driver keyword, `VlanID`, that most wired
//! adapter drivers (Realtek, Intel, Broadcom) implement: with it set, the
//! driver inserts that tag on every frame it transmits and accepts only
//! frames carrying it on receive, stripping the tag before the IP stack
//! sees them. The physical adapter becomes an access port for that one
//! VLAN, and its ordinary address, routes and neighbor entries then apply
//! to the VLAN. That is the whole backend:
//!
//! - the profile entry's VLAN id goes into the parent adapter's `VlanID`
//!   keyword (0 for an untagged entry, which is the keyword's own "no
//!   VLAN" value);
//! - the address, MTU, routes and static neighbor entries go onto the
//!   parent adapter itself through `netsh`, exactly as the Hyper-V backend
//!   puts them onto a virtual adapter.
//!
//! # What this means for a caller
//!
//! **One VLAN per adapter.** The keyword is a single value, so this backend
//! refuses a profile with more than one entry
//! ([`Platform::validate_profile`]). Several VLANs on one adapter at the
//! same time is [`WindowsHyperV`](super::WindowsHyperV)'s job. Where a
//! profile needs only one VLAN at a time — a sensor's live-data VLAN, with
//! diagnostics on another VLAN applied as a separate profile when needed —
//! this backend is the simpler and, on the adapters it has been measured
//! on, the more robust of the two. It has no virtual switch to bind, so it
//! has no interaction with the adapter's multicast filter, which under
//! Hyper-V has been seen to drop a sensor's service-discovery groups.
//!
//! **The interface is the parent adapter.** [`Platform::iface_name`] returns
//! `device` itself, and the state file records it, so `down` knows to
//! reset the keyword and return the address to DHCP. That makes this the
//! one backend whose teardown reverts *parent* configuration
//! ([`Platform::reverts_parent_config`]), and it is why the created
//! interface is recorded even though nothing new appears in the adapter
//! list.
//!
//! **Setting the keyword restarts the adapter.** The link drops for a few
//! seconds and the adapter re-enumerates; anything discovered over it
//! before the apply will see its peers go quiet for that long. The create
//! command waits for the keyword to read back and the adapter to be
//! listed again before the address is set.
//!
//! **A peer that cached the address's MAC keeps using it.** This backend
//! keeps the parent's MAC, so switching from a Hyper-V profile back to
//! this one moves the address to a different MAC. A sensor that resolved
//! the address while the Hyper-V profile was up keeps sending to the
//! virtual adapter's MAC until its own datapath restarts. Nothing here
//! can tell it; the caller has to.
//!
//! Elevation, `powershell.exe -Command` and the literal escape
//! ([`ps_literal`]) are as for the Hyper-V backend.

#[cfg(test)]
use super::windows::adapter_property_probe;
use super::windows::{POWERSHELL, WindowsHyperV, powershell, powershell_script, ps_literal};
use super::{Platform, ipv4_netmask};
use crate::config::{Interface, Profile, Route};
use crate::net::{Cmd, CommandRunner};
use anyhow::{Result, bail};
use ipnet::IpNet;

/// The standardized NDIS keyword this backend sets. Its value is the
/// 802.1Q id; `0` means no VLAN.
const VLAN_KEYWORD: &str = "VlanID";

/// How long the apply waits for the keyword to read back and the adapter
/// to reappear after the restart the change causes: 40 polls of 500 ms.
const RESTART_WAIT_POLLS: u32 = 40;

/// Windows, through the adapter driver's `VlanID` keyword. See the module
/// documentation for the model and its one-VLAN limit.
pub struct WindowsDriverVlan;

impl Platform for WindowsDriverVlan {
    fn name(&self) -> &'static str {
        "windows-driver-vlan"
    }

    fn iface_name(&self, _interface: &Interface, device: &str) -> String {
        // The parent adapter is the interface: the keyword changes what the
        // adapter is, not what exists.
        device.to_string()
    }

    fn bringup_commands(&self, interface: &Interface, device: &str) -> Vec<Cmd> {
        let mut cmds = vec![powershell(&set_vlan_script(
            interface.vlan.unwrap_or(0),
            device,
        ))];
        let addr = interface.address.addr().to_string();
        let mask = ipv4_netmask(interface.address.prefix_len());
        cmds.push(Cmd::new(
            "netsh",
            &[
                "interface",
                "ipv4",
                "set",
                "address",
                device,
                "static",
                &addr,
                &mask,
            ],
        ));
        if let Some(mtu) = interface.mtu {
            cmds.push(Cmd::new(
                "netsh",
                &[
                    "interface",
                    "ipv4",
                    "set",
                    "subinterface",
                    device,
                    &format!("mtu={mtu}"),
                    "store=persistent",
                ],
            ));
        }
        cmds
    }

    fn teardown_commands(&self, iface: &str) -> Vec<Cmd> {
        // The address first, while the adapter is stable; then the keyword,
        // which restarts it. DHCP is where a sensor-facing adapter rests
        // when nothing is applied: it is what the adapter had before the
        // first apply on every host this was measured on, and a link-local
        // address is what it settles to with no server on the wire.
        vec![
            Cmd::new(
                "netsh",
                &["interface", "ipv4", "set", "address", iface, "dhcp"],
            ),
            powershell(&set_vlan_script(0, iface)),
        ]
    }

    fn route_commands(&self, route: &Route, in_subnet: bool, iface: &str) -> Vec<Cmd> {
        // Same stack, same `netsh`, same reasoning as the Hyper-V backend.
        WindowsHyperV.route_commands(route, in_subnet, iface)
    }

    fn existing_vlans(
        &self,
        _runner: &mut dyn CommandRunner,
        _parent: &str,
    ) -> Result<Vec<(u16, String)>> {
        // The parent holds at most one VLAN and this backend owns it; there
        // is no second interface to collide with.
        Ok(Vec::new())
    }

    fn is_candidate_device(&self, name: &str) -> bool {
        WindowsHyperV.is_candidate_device(name)
    }

    fn reverts_parent_config(&self) -> bool {
        // Everything this backend does is parent configuration, and
        // teardown undoes all of it.
        true
    }

    fn claims_parent_exclusively(&self) -> bool {
        // With the keyword set the adapter transmits tagged only, so a host
        // that relied on it untagged loses that use until `down`. A wrong
        // guess disconnects the host; the caller names the device.
        true
    }

    fn validate_profile(&self, profile: &Profile) -> Result<()> {
        if profile.interfaces.len() > 1 {
            bail!(
                "profile '{}' has {} interface entries, and the driver VLAN backend \
                 configures one VLAN per adapter (the `VlanID` keyword holds a single \
                 id); apply one VLAN at a time as separate profiles, or use the Hyper-V \
                 backend for several VLANs on one adapter",
                profile.name,
                profile.interfaces.len()
            );
        }
        Ok(())
    }

    fn list_devices(&self, runner: &mut dyn CommandRunner) -> Result<Vec<String>> {
        WindowsHyperV.list_devices(runner)
    }

    fn addresses_on(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<Vec<IpNet>> {
        WindowsHyperV.addresses_on(runner, device)
    }

    fn is_wireless(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool> {
        WindowsHyperV.is_wireless(runner, device)
    }

    fn link_is_active(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool> {
        WindowsHyperV.link_is_active(runner, device)
    }

    fn records_created_interface(&self, cmd: &Cmd) -> bool {
        // Setting the keyword is the moment the parent becomes something
        // this backend must put back, so it is what gets recorded — but
        // only the *apply* direction: the teardown script sets the keyword
        // too, and recording that would re-record the interface `down` is
        // removing.
        cmd.program == POWERSHELL
            && powershell_script(cmd).is_some_and(|s| {
                s.contains("Set-NetAdapterAdvancedProperty") && !s.contains("-RegistryValue '0'")
            })
    }
}

/// The script that sets the parent's `VlanID` keyword and waits out the
/// adapter restart it causes.
///
/// The wait is on two things, in order: the keyword reading back with the
/// new value, and the adapter being listed again. Link state is
/// deliberately not waited for — a cable may not be in, and `netsh` sets
/// an address on a link-down adapter without complaint. The throw names
/// the adapter so a stuck restart is legible.
fn set_vlan_script(vlan: u16, device: &str) -> String {
    let dev = ps_literal(device);
    let value = ps_literal(&vlan.to_string());
    format!(
        "Set-NetAdapterAdvancedProperty -Name {dev} -RegistryKeyword {kw} -RegistryValue {value}; \
         $tries = 0; \
         while (-not ((Get-NetAdapterAdvancedProperty -Name {dev} -RegistryKeyword {kw} \
         -ErrorAction SilentlyContinue).RegistryValue -eq {value} -and \
         (Get-NetAdapter | Where-Object {{ $_.Name -eq {dev} }}))) {{ \
         if (++$tries -gt {RESTART_WAIT_POLLS}) {{ throw 'adapter ' + {dev} + ' did not come back after setting {VLAN_KEYWORD}' }}; \
         Start-Sleep -Milliseconds 500 }}",
        kw = ps_literal(VLAN_KEYWORD),
    )
}

/// The probe this backend uses to read the keyword back, for tests and the
/// dry-run allowlist.
#[cfg(test)]
fn keyword_probe(device: &str) -> Cmd {
    adapter_property_probe(device, "Status")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::RecordingRunner;
    use crate::plan::bringup_commands_for;

    fn tagged(id: u16, cidr: &str) -> Interface {
        Interface {
            vlan: Some(id),
            address: cidr.parse().unwrap(),
            mtu: None,
            routes: vec![],
        }
    }

    fn render(cmds: &[Cmd]) -> Vec<String> {
        cmds.iter().map(Cmd::display).collect()
    }

    #[test]
    fn the_interface_is_the_parent_adapter_itself() {
        assert_eq!(
            WindowsDriverVlan.iface_name(&tagged(11, "192.168.11.87/24"), "Ethernet 2"),
            "Ethernet 2"
        );
    }

    #[test]
    fn rendering_of_a_tagged_entry_is_pinned() {
        let i = Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: Some(1500),
            routes: vec![Route {
                destination: "239.255.0.255/32".to_string(),
                gateway: None,
                mac: None,
            }],
        };
        let cmds = bringup_commands_for(&WindowsDriverVlan, &i, "Ethernet 2");
        assert_eq!(
            powershell_script(&cmds[0]).unwrap(),
            "$ErrorActionPreference = 'Stop'; \
             Set-NetAdapterAdvancedProperty -Name 'Ethernet 2' -RegistryKeyword 'VlanID' -RegistryValue '11'; \
             $tries = 0; \
             while (-not ((Get-NetAdapterAdvancedProperty -Name 'Ethernet 2' -RegistryKeyword 'VlanID' \
             -ErrorAction SilentlyContinue).RegistryValue -eq '11' -and \
             (Get-NetAdapter | Where-Object { $_.Name -eq 'Ethernet 2' }))) { \
             if (++$tries -gt 40) { throw 'adapter ' + 'Ethernet 2' + ' did not come back after setting VlanID' }; \
             Start-Sleep -Milliseconds 500 }"
        );
        assert_eq!(
            render(&cmds[1..]),
            vec![
                "netsh interface ipv4 set address Ethernet 2 static 192.168.11.87 255.255.255.0",
                "netsh interface ipv4 set subinterface Ethernet 2 mtu=1500 store=persistent",
                "netsh interface ipv4 add route 239.255.0.255/32 Ethernet 2",
            ]
        );
    }

    #[test]
    fn an_untagged_entry_sets_the_keyword_to_zero() {
        let i = Interface {
            vlan: None,
            address: "192.168.1.100/24".parse().unwrap(),
            mtu: None,
            routes: vec![],
        };
        let cmds = bringup_commands_for(&WindowsDriverVlan, &i, "Ethernet 2");
        assert!(
            powershell_script(&cmds[0])
                .unwrap()
                .contains("-RegistryKeyword 'VlanID' -RegistryValue '0';")
        );
    }

    #[test]
    fn teardown_returns_the_adapter_to_dhcp_and_no_vlan_in_that_order() {
        let cmds = WindowsDriverVlan.teardown_commands("Ethernet 2");
        assert_eq!(
            cmds[0].display(),
            "netsh interface ipv4 set address Ethernet 2 dhcp"
        );
        assert!(
            powershell_script(&cmds[1])
                .unwrap()
                .contains("-RegistryKeyword 'VlanID' -RegistryValue '0';")
        );
    }

    #[test]
    fn only_the_apply_direction_of_the_keyword_is_recorded() {
        let cmds = bringup_commands_for(
            &WindowsDriverVlan,
            &tagged(11, "192.168.11.87/24"),
            "Ethernet 2",
        );
        let recorded: Vec<bool> = cmds
            .iter()
            .map(|c| WindowsDriverVlan.records_created_interface(c))
            .collect();
        assert_eq!(recorded, vec![true, false]);
        // The teardown sets the keyword too, to 0; that must not re-record
        // the parent `down` is putting back.
        for cmd in WindowsDriverVlan.teardown_commands("Ethernet 2") {
            assert!(
                !WindowsDriverVlan.records_created_interface(&cmd),
                "{}",
                cmd.display()
            );
        }
    }

    #[test]
    fn a_profile_with_two_entries_is_refused_before_anything_runs() {
        let one: Profile =
            toml::from_str("name=\"live\"\n[[interface]]\nvlan=11\naddress=\"192.168.11.87/24\"\n")
                .unwrap();
        assert!(WindowsDriverVlan.validate_profile(&one).is_ok());
        let two: Profile = toml::from_str(
            "name=\"iris\"\n[[interface]]\nvlan=10\naddress=\"192.168.10.90/24\"\n\
             [[interface]]\nvlan=11\naddress=\"192.168.11.87/24\"\n",
        )
        .unwrap();
        let err = WindowsDriverVlan
            .validate_profile(&two)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("one VLAN per adapter") && err.contains("Hyper-V"),
            "{err}"
        );
    }

    #[test]
    fn the_keyword_script_carries_no_double_quotes_and_escapes_the_device() {
        let script = set_vlan_script(11, "x'; Remove-VMSwitch -Name 'vlanctl");
        assert!(!script.contains('"'));
        assert!(script.contains("-Name 'x''; Remove-VMSwitch -Name ''vlanctl'"));
    }

    #[test]
    fn host_probes_are_the_hyperv_backends_and_stay_read_only() {
        let mut runner = RecordingRunner::default();
        let _ = WindowsDriverVlan.list_devices(&mut runner);
        let _ = WindowsDriverVlan.addresses_on(&mut runner, "Ethernet 2");
        let _ = WindowsDriverVlan.link_is_active(&mut runner, "Ethernet 2");
        let _ = WindowsDriverVlan.is_wireless(&mut runner, "Ethernet 2");
        assert_eq!(runner.commands.len(), 4);
        for cmd in &runner.commands {
            assert!(WindowsHyperV::is_read_only_probe(cmd), "{}", cmd.display());
        }
        assert!(WindowsHyperV::is_read_only_probe(&keyword_probe(
            "Ethernet 2"
        )));
        // And the keyword script is a mutation.
        let set = powershell(&set_vlan_script(11, "Ethernet 2"));
        assert!(!WindowsHyperV::is_read_only_probe(&set));
    }
}
