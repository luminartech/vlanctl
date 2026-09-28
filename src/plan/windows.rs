//! Windows backend: Hyper-V management-OS virtual adapters stand in for the
//! VLAN sub-interfaces Windows cannot create natively.
//!
//! # The model
//!
//! Windows has no general 802.1Q sub-interface. A few NIC vendors ship one in
//! their driver, but nothing the OS offers on every adapter. What Windows does
//! offer everywhere Hyper-V is available is a virtual switch: bind the parent
//! NIC to an external switch, and each management-OS virtual adapter on that
//! switch can be put in *access* mode for one VLAN id. The switch inserts
//! and strips the tag; the virtual adapter looks to the rest of the stack
//! like an ordinary NIC on that VLAN. That is the whole backend:
//!
//! - one external switch, named [`SWITCH_NAME`], bound to the parent device
//!   with `AllowManagementOS` **off**, so the parent's own TCP/IP binding
//!   goes away and nothing untagged is plumbed by accident;
//! - one management-OS virtual adapter per `[[interface]]` entry, in access
//!   mode for a tagged entry and untagged mode for an untagged one;
//! - `netsh` for the address, MTU, routes and static neighbor entries, in
//!   the same argv form the other backends use.
//!
//! `netsh` rather than the `Net*` cmdlets for addressing is a bench finding,
//! not a preference: `New-NetIPAddress` fails on a virtual adapter whose
//! parent has no link (`Inconsistent parameters PolicyStore PersistentStore
//! and Dhcp Enabled`) and, when it does succeed, does not always reach the
//! persistent store. `netsh interface ipv4 set address` works in both states
//! and persists.
//!
//! # What this means for a caller
//!
//! **Binding the parent takes it away from the host.** Once the switch is
//! bound, the parent adapter carries no address of its own until `down`
//! removes the switch again. Point this backend at the machine's uplink and
//! the machine loses its uplink. That is why [`WindowsHyperV`] answers `true` to
//! [`Platform::claims_parent_exclusively`] and device auto-detection refuses
//! to guess here: name the sensor-facing adapter explicitly.
//!
//! **The parent is not a Linux-style interface name.** `device` is the
//! adapter's *name* as `Get-NetAdapter` shows it — `Ethernet 2`, not a GUID
//! and not the driver description.
//!
//! **Interface names are the adapters Windows creates.** Hyper-V names a
//! management-OS virtual adapter `vEthernet (<name>)`, and that alias is the
//! name every Windows tool keys on, so it is what [`Platform::iface_name`]
//! returns and what the state file records: `vEthernet (vlan11)` for VLAN
//! 11, `vEthernet (untagged)` for an untagged entry. The untagged entry is
//! an adapter this backend created, so unlike macOS and Linux it is torn
//! down on `down`.
//!
//! **Everything persists across a reboot.** The switch, its virtual
//! adapters, their VLAN ids, the `netsh` addresses and the routes all come
//! back; nothing here needs a boot-time reapply. One thing does not come
//! back by itself: if the parent adapter is absent at boot (a dock that
//! failed to enumerate, a USB NIC that was unplugged), Hyper-V reports the
//! switch as internal and does **not** rebind it when the adapter returns.
//! `down` followed by `apply` recovers it.
//!
//! **Hyper-V must be enabled**, with its PowerShell module (`Enable-
//! WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All`, which
//! reboots). This backend never enables it. It is available on Windows 10/11
//! Pro, Enterprise and Education and on Windows Server; not on Home.
//!
//! **Each command is one elevated process.** Hyper-V cmdlets and `netsh`
//! both require administrator rights, so the [`CommandRunner`] must run
//! elevated. The cmdlets are reached through `powershell.exe -Command`,
//! which is not subject to script execution policy (that governs script
//! *files*; there are none here), and `-ExecutionPolicy Bypass` is passed
//! anyway so a host set to `Restricted` behaves the same. Every value that
//! reaches a PowerShell script is a single-quoted literal built by
//! [`ps_literal`], which is a complete escape for that context: inside
//! single quotes PowerShell interprets nothing but a doubled quote.

use super::{Platform, ipv4_netmask};
use crate::config::{Interface, Route};
use crate::net::{Cmd, CommandRunner};
use crate::state::ParentConfig;
use anyhow::{Context, Result};
use ipnet::IpNet;
use std::net::{IpAddr, Ipv4Addr};

/// Name of the Hyper-V external switch this backend creates on the parent
/// device. One per host: a physical adapter binds to at most one external
/// switch, and every entry of a profile shares the one parent.
pub const SWITCH_NAME: &str = "vlanctl";

/// Hyper-V's name for the virtual adapter of an untagged entry. Tagged
/// entries are `vlan<id>`, matching the other backends' vocabulary.
const UNTAGGED_VNIC: &str = "untagged";

/// The interpreter every cmdlet goes through. Windows PowerShell 5.1 ships
/// with every supported Windows and is where the Hyper-V module lives;
/// `pwsh` is not assumed.
pub(super) const POWERSHELL: &str = "powershell.exe";

/// Every script starts with this. `-Command` reports success unless the
/// *last* statement failed, and cmdlet errors are non-terminating by
/// default; `Stop` turns each one into a terminating error so a failure
/// anywhere in the script fails the command, which is what rollback keys on.
const SCRIPT_PREFIX: &str = "$ErrorActionPreference = 'Stop'; ";

/// How long the create script waits for the new adapter to be visible to
/// the IP stack before giving up: 40 polls of 500 ms. The adapter usually
/// appears within a second or two; the bound is for a Hyper-V that never
/// finishes.
const ADAPTER_WAIT_POLLS: u32 = 40;

/// How long the parent-restore script waits for the parent to be back in
/// the IP stack after the switch is removed: 40 polls of 500 ms, the same
/// bound as [`ADAPTER_WAIT_POLLS`]. `Remove-VMSwitch` returns before
/// Windows has rebound TCP/IP to the parent, and `netsh` cannot set an
/// address on an adapter the stack does not list yet.
const PARENT_RETURN_POLLS: u32 = 40;

/// Windows, through Hyper-V. See the module documentation for the model.
pub struct WindowsHyperV;

impl WindowsHyperV {
    /// Whether `cmd` is one of this backend's read-only probes.
    ///
    /// A dry run executes exactly the commands this returns `true` for, so
    /// it is a strict allowlist of the probe shapes the backend emits — the
    /// same contract as the CLI's own `is_read_only_probe`, extended here
    /// because a PowerShell command line cannot be matched on `argv` alone.
    /// Every statement of the script must be one this backend's probes are
    /// built from; a script with any other statement, including any that
    /// creates, sets or removes anything, is not a probe.
    #[must_use]
    pub fn is_read_only_probe(cmd: &Cmd) -> bool {
        if cmd.program != POWERSHELL {
            return false;
        }
        let Some(script) = powershell_script(cmd) else {
            return false;
        };
        script
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .all(|statement| {
                statement == "$ErrorActionPreference = 'Stop'"
                    || statement.starts_with("Get-NetAdapter | ForEach-Object")
                    || statement.starts_with("Get-NetIPAddress -InterfaceAlias")
                    || statement.starts_with("$a = Get-NetAdapter | Where-Object")
                    || statement.starts_with("if (-not $a) { throw")
                    || statement.starts_with("$a.")
                    // The parent-config probe ([`parent_config_probe`]).
                    || statement.starts_with("$i = Get-NetIPInterface -InterfaceAlias")
                    || statement.starts_with("if ($i) { 'dhcp ' + $i.Dhcp }")
                    || statement.starts_with("Get-NetRoute -InterfaceAlias")
                    || statement == "exit 0"
            })
    }
}

impl Platform for WindowsHyperV {
    fn name(&self) -> &'static str {
        "windows-hyperv"
    }

    fn iface_name(&self, interface: &Interface, _device: &str) -> String {
        // The alias Hyper-V gives the virtual adapter, for tagged AND
        // untagged entries: the parent's own stack is gone once the switch
        // is bound, so an untagged entry is an adapter too, not an address
        // on the parent as it is elsewhere.
        adapter_alias(&vnic_name(interface))
    }

    fn bringup_commands(&self, interface: &Interface, device: &str) -> Vec<Cmd> {
        let vnic = vnic_name(interface);
        let alias = adapter_alias(&vnic);
        let mut cmds = vec![powershell(&create_script(interface, device))];
        // `netsh ... set address <alias> static <ip> <mask>` — the form
        // proven on the bench (see the module doc for why not
        // `New-NetIPAddress`). Positional rather than `name=` keyword form
        // because that is the form that was proven with an alias containing
        // spaces, and the two are documented as equivalent.
        let addr = interface.address.addr().to_string();
        let mask = ipv4_netmask(interface.address.prefix_len());
        cmds.push(Cmd::new(
            "netsh",
            &[
                "interface",
                "ipv4",
                "set",
                "address",
                &alias,
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
                    &alias,
                    &format!("mtu={mtu}"),
                    "store=persistent",
                ],
            ));
        }
        cmds
    }

    fn teardown_commands(&self, iface: &str) -> Vec<Cmd> {
        // Removing the virtual adapter drops its address and routes with it.
        // The switch goes when its last adapter does, so the parent returns
        // to its own stack on the final teardown and not before. `iface` is
        // normally an alias this backend produced; an unrecognised name is
        // treated as a bare virtual-adapter name rather than ignored, so a
        // hand-edited state file still tears down what it names.
        let vnic = vnic_from_alias(iface).unwrap_or(iface);
        vec![powershell(&teardown_script(vnic))]
    }

    fn route_commands(&self, route: &Route, _in_subnet: bool, iface: &str) -> Vec<Cmd> {
        // `in_subnet` is ignored for the same reason as on Linux: an
        // interface route with no next hop makes the destination on-link
        // and the stack ARPs for it, which is what reaching several hosts
        // sharing one subnet across VLANs needs. Windows has no self-MAC
        // hazard of the macOS kind.
        let mut cmds = Vec::new();
        let prefix = route_prefix(&route.destination);
        match &route.gateway {
            Some(gateway) => {
                let gateway = gateway.to_string();
                cmds.push(Cmd::new(
                    "netsh",
                    &[
                        "interface",
                        "ipv4",
                        "add",
                        "route",
                        &prefix,
                        iface,
                        &gateway,
                    ],
                ));
            }
            None => {
                cmds.push(Cmd::new(
                    "netsh",
                    &["interface", "ipv4", "add", "route", &prefix, iface],
                ));
                // `add neighbors` is Windows's static ARP entry. It replaces
                // an existing entry for the address, so like Linux's `ip
                // neigh replace` it cannot fail on a stale learned one.
                // Validation guarantees mac => gatewayless /32, so the
                // address parse always succeeds.
                if let Some(mac) = &route.mac
                    && let Ok(net) = route.destination.parse::<IpNet>()
                {
                    let host = net.addr().to_string();
                    let mac = windows_mac(mac);
                    cmds.push(Cmd::new(
                        "netsh",
                        &["interface", "ipv4", "add", "neighbors", iface, &host, &mac],
                    ));
                }
            }
        }
        cmds
    }

    fn existing_vlans(
        &self,
        _runner: &mut dyn CommandRunner,
        _parent: &str,
    ) -> Result<Vec<(u16, String)>> {
        // Deliberately empty, and no probe: Hyper-V keys a management-OS
        // adapter on its name, not on its VLAN id — several adapters may
        // sit in access mode for the same id on one switch — so the
        // name-based guard is the whole constraint.
        Ok(Vec::new())
    }

    fn is_candidate_device(&self, name: &str) -> bool {
        // A name predicate only, like the other backends. Windows adapter
        // names are free text (`Ethernet`, `Ethernet 2`, `Wi-Fi`), so this
        // excludes what is definitely not a parent: our own and anyone
        // else's Hyper-V adapters, loopback, Bluetooth PAN, and the
        // Wi-Fi Direct placeholders (`Local Area Connection* 1`). Wireless
        // is `is_wireless`'s question, but `Wi-Fi` is excluded by name
        // too, as `wl` is on Linux.
        !name.starts_with("vEthernet")
            && !name.starts_with("Wi-Fi")
            && !name.contains('*')
            && !name.contains("Loopback")
            && !name.contains("Bluetooth")
    }

    fn reverts_parent_config(&self) -> bool {
        // The parent's binding to the switch is the parent configuration,
        // and the last teardown removes it. Unlike the other backends this
        // is not a statement about an address on the parent — the parent
        // holds none while bound.
        true
    }

    fn claims_parent_exclusively(&self) -> bool {
        // Binding the parent removes its own TCP/IP binding. Guessing the
        // wrong adapter takes the host off its network, so auto-detection
        // is refused here and the caller must name the device.
        true
    }

    fn list_devices(&self, runner: &mut dyn CommandRunner) -> Result<Vec<String>> {
        // One adapter name per line. `Get-NetAdapter` without `-IncludeHidden`
        // is the list an operator sees in the Network Connections panel.
        let out = runner.run(&powershell("Get-NetAdapter | ForEach-Object { $_.Name }"))?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect())
    }

    fn addresses_on(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<Vec<IpNet>> {
        // `<address>/<prefix>` per line. An adapter that does not exist
        // yields nothing rather than an error, and that is load-bearing:
        // `apply` asks this about an untagged entry's adapter *before*
        // creating it, and an error there would refuse every untagged
        // apply.
        //
        // The trailing `exit 0` is what makes that true. `-ErrorAction
        // SilentlyContinue` hides the cmdlet's "no matching address" error,
        // but `-Command` still exits 1 when the last statement's `$?` is
        // false — measured on a live host, that is exactly what happens for
        // a missing alias. `exit 0` runs only if the script got that far: a
        // *terminating* error earlier (the cmdlet not found, the module
        // missing) still ends the script first, under `Stop`, with exit 1.
        let script = format!(
            "Get-NetIPAddress -InterfaceAlias {} -AddressFamily IPv4 -ErrorAction \
             SilentlyContinue | ForEach-Object {{ $_.IPAddress + '/' + $_.PrefixLength }}; \
             exit 0",
            ps_literal(device)
        );
        let out = runner.run(&powershell(&script))?;
        Ok(out
            .lines()
            .filter_map(|line| line.trim().parse::<IpNet>().ok())
            .collect())
    }

    fn is_wireless(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool> {
        // `PhysicalMediaType` is `Native 802.11` for Wi-Fi and `Wireless
        // WAN` for cellular; wired Ethernet reports `802.3`. Exact-name
        // match through `Where-Object`: `-Name` takes wildcards, and an
        // adapter name is free text.
        let out = runner
            .run(&adapter_property_probe(device, "PhysicalMediaType"))
            .with_context(|| format!("cannot determine whether {device} is wireless"))?;
        let media = out.trim();
        Ok(media.contains("802.11") || media.contains("Wireless"))
    }

    fn link_is_active(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool> {
        // `Status` is `Up` with carrier; `Disconnected`, `Disabled` and
        // `Not Present` are all "no link". A missing adapter is an error
        // from the probe, which is the "genuine failure to ask" the trait
        // reserves `Err` for.
        let out = runner
            .run(&adapter_property_probe(device, "Status"))
            .with_context(|| format!("cannot determine link state for {device}"))?;
        Ok(out.trim() == "Up")
    }

    fn records_created_interface(&self, cmd: &Cmd) -> bool {
        // The create script is the one that runs `Add-VMNetworkAdapter`.
        cmd.program == POWERSHELL
            && powershell_script(cmd)
                .is_some_and(|s| s.contains("Add-VMNetworkAdapter -ManagementOS"))
    }

    fn parent_snapshot(
        &self,
        runner: &mut dyn CommandRunner,
        device: &str,
    ) -> Result<Option<ParentConfig>> {
        // Binding the parent to the switch clears its addresses, and
        // removing the switch does not bring them back (measured
        // 2026-09-28: a static `192.168.11.87/24` came back from
        // `Remove-VMSwitch` as "static, no address", i.e. APIPA). Read
        // what is there now, before `New-VMSwitch` runs, so `down` can put
        // it back.
        let out = runner
            .run(&parent_config_probe(device))
            .with_context(|| format!("cannot read the IPv4 configuration of {device}"))?;
        Ok(parse_parent_config(device, &out))
    }

    fn parent_restore_commands(&self, parent: &ParentConfig) -> Vec<Cmd> {
        restore_parent_commands(parent)
    }
}

/// The probe [`WindowsHyperV::parent_snapshot`] runs: one line per fact,
/// each tagged with what it is so the parse cannot mistake one for another.
///
/// - `dhcp Enabled|Disabled` from the IPv4 interface object. Its absence
///   means the adapter has no IPv4 stack at all — it is already bound to a
///   switch, or TCP/IP is unbound — and there is nothing to record.
/// - `address <ip>/<prefix>` for each manually configured address. A DHCP
///   lease or an APIPA address (`PrefixOrigin` `Dhcp` / `WellKnown`) is not
///   configuration and is not restored.
/// - `gateway <ip>` for the default route on the adapter, if any.
///
/// The trailing `exit 0` is for the same reason as in
/// [`WindowsHyperV::addresses_on`]: a missing alias leaves the last
/// statement's `$?` false, and `-Command` would exit 1 over it.
fn parent_config_probe(device: &str) -> Cmd {
    let dev = ps_literal(device);
    powershell(&format!(
        "$i = Get-NetIPInterface -InterfaceAlias {dev} -AddressFamily IPv4 \
         -ErrorAction SilentlyContinue; \
         if ($i) {{ 'dhcp ' + $i.Dhcp }}; \
         Get-NetIPAddress -InterfaceAlias {dev} -AddressFamily IPv4 -ErrorAction SilentlyContinue \
         | Where-Object {{ $_.PrefixOrigin -eq 'Manual' }} \
         | ForEach-Object {{ 'address ' + $_.IPAddress + '/' + $_.PrefixLength }}; \
         Get-NetRoute -InterfaceAlias {dev} -DestinationPrefix '0.0.0.0/0' -AddressFamily IPv4 \
         -ErrorAction SilentlyContinue | ForEach-Object {{ 'gateway ' + $_.NextHop }}; \
         exit 0"
    ))
}

/// Parse [`parent_config_probe`]'s output. `None` when the probe reported
/// no IPv4 interface, which is the "nothing to put back" answer.
fn parse_parent_config(device: &str, out: &str) -> Option<ParentConfig> {
    let mut dhcp = None;
    let mut addresses = Vec::new();
    let mut gateway = None;
    for line in out.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("dhcp ") {
            dhcp = Some(v.trim().eq_ignore_ascii_case("Enabled"));
        } else if let Some(v) = line.strip_prefix("address ") {
            if let Ok(net) = v.trim().parse::<IpNet>()
                && matches!(net, IpNet::V4(_))
            {
                addresses.push(net);
            }
        } else if let Some(v) = line.strip_prefix("gateway ")
            && let Ok(ip) = v.trim().parse::<Ipv4Addr>()
            && !ip.is_unspecified()
        {
            gateway = Some(ip);
        }
    }
    Some(ParentConfig {
        device: device.to_string(),
        dhcp: dhcp?,
        addresses,
        gateway,
    })
}

/// The commands that put a recorded parent configuration back, after the
/// switch that took the parent over is gone.
///
/// First a wait for the parent to be listed by the IP stack again —
/// `Remove-VMSwitch` returns before TCP/IP is rebound — then `netsh`, in
/// the same positional form bring-up uses: `set address ... dhcp` for a
/// DHCP device, `set address ... static <ip> <mask> [gateway]` for the
/// first static address and `add address` for any further one. A static
/// device that held no address has nothing to put back and gets no
/// commands at all, not even the wait.
fn restore_parent_commands(parent: &ParentConfig) -> Vec<Cmd> {
    let statics: Vec<(String, String)> = parent
        .addresses
        .iter()
        .filter_map(|net| match net.addr() {
            IpAddr::V4(addr) => Some((addr.to_string(), ipv4_netmask(net.prefix_len()))),
            IpAddr::V6(_) => None,
        })
        .collect();
    if !parent.dhcp && statics.is_empty() {
        return Vec::new();
    }
    let dev = parent.device.as_str();
    let mut cmds = vec![powershell(&parent_return_script(dev))];
    if parent.dhcp {
        cmds.push(Cmd::new(
            "netsh",
            &["interface", "ipv4", "set", "address", dev, "dhcp"],
        ));
    }
    for (index, (addr, mask)) in statics.iter().enumerate() {
        if index == 0 && !parent.dhcp {
            let mut args = vec![
                "interface",
                "ipv4",
                "set",
                "address",
                dev,
                "static",
                addr,
                mask,
            ];
            let gateway = parent.gateway.map(|g| g.to_string());
            if let Some(gateway) = gateway.as_deref() {
                args.push(gateway);
            }
            cmds.push(Cmd::new("netsh", &args));
        } else {
            cmds.push(Cmd::new(
                "netsh",
                &["interface", "ipv4", "add", "address", dev, addr, mask],
            ));
        }
    }
    cmds
}

/// The script that waits for `device` to be back in the IPv4 stack after
/// the switch released it, so the `netsh` that follows has something to
/// configure. The throw names the adapter so a stuck rebind is legible.
fn parent_return_script(device: &str) -> String {
    let dev = ps_literal(device);
    format!(
        "$tries = 0; \
         while (-not (Get-NetIPInterface -InterfaceAlias {dev} -AddressFamily IPv4 \
         -ErrorAction SilentlyContinue)) {{ \
         if (++$tries -gt {PARENT_RETURN_POLLS}) {{ \
         throw 'adapter ' + {dev} + ' did not return to the IP stack after the switch was removed' }}; \
         Start-Sleep -Milliseconds 500 }}"
    )
}

/// Hyper-V's name for the virtual adapter of one entry.
fn vnic_name(interface: &Interface) -> String {
    match interface.vlan {
        Some(id) => format!("vlan{id}"),
        None => UNTAGGED_VNIC.to_string(),
    }
}

/// The adapter alias Windows assigns to a management-OS virtual adapter.
fn adapter_alias(vnic: &str) -> String {
    format!("vEthernet ({vnic})")
}

/// The virtual-adapter name inside an alias of [`adapter_alias`]'s shape.
fn vnic_from_alias(alias: &str) -> Option<&str> {
    alias.strip_prefix("vEthernet (")?.strip_suffix(')')
}

/// One `powershell.exe -Command <script>` invocation.
///
/// The script is a single argv element: `Command::args` quotes it as one
/// argument, PowerShell receives it as one command string, and nothing in
/// between re-splits it. Scripts contain no double quotes by construction,
/// which keeps that quoting trivial.
pub(super) fn powershell(script: &str) -> Cmd {
    let script = format!("{SCRIPT_PREFIX}{script}");
    Cmd::new(
        POWERSHELL,
        &[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ],
    )
}

/// The script inside a [`powershell`] command, if `cmd` has that shape.
pub(super) fn powershell_script(cmd: &Cmd) -> Option<&str> {
    match cmd.args.as_slice() {
        [no_profile, non_interactive, exec, bypass, command, script]
            if no_profile == "-NoProfile"
                && non_interactive == "-NonInteractive"
                && exec == "-ExecutionPolicy"
                && bypass == "Bypass"
                && command == "-Command" =>
        {
            Some(script)
        }
        _ => None,
    }
}

/// A PowerShell single-quoted string literal for `s`.
///
/// Inside single quotes PowerShell performs no interpolation and no escape
/// processing; the only special character is `'`, written as `''`. That
/// makes this a complete escape for any value: an adapter name, a profile
/// field, anything that must not be able to end the literal or start a
/// statement.
pub(super) fn ps_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `netsh` takes `0.0.0.0/0` where a profile says `default`.
pub(super) fn route_prefix(destination: &str) -> String {
    if destination == "default" {
        "0.0.0.0/0".to_string()
    } else {
        destination.to_string()
    }
}

/// `netsh` writes a MAC with dashes; profiles are validated in the colon
/// form. Dashes pass through unchanged.
pub(super) fn windows_mac(mac: &str) -> String {
    mac.replace(':', "-")
}

/// A probe for one property of the adapter named exactly `device`.
pub(super) fn adapter_property_probe(device: &str, property: &str) -> Cmd {
    powershell(&format!(
        "$a = Get-NetAdapter | Where-Object {{ $_.Name -eq {lit} }}; \
         if (-not $a) {{ throw 'no adapter named ' + {lit} }}; $a.{property}",
        lit = ps_literal(device)
    ))
}

/// The script that creates one entry's virtual adapter, and the switch if
/// this is the first.
///
/// Atomic from `apply`'s point of view, which is what makes the shared
/// switch fit a per-interface create/teardown model: either the adapter
/// exists at the end (recorded, and the switch with it), or nothing this
/// script created is left behind. Without that, a switch created here and
/// an adapter that then failed to appear would leave the parent bound to a
/// switch that `rollback` — which only knows recorded adapters — would
/// never remove.
fn create_script(interface: &Interface, device: &str) -> String {
    let vnic = ps_literal(&vnic_name(interface));
    let alias = ps_literal(&adapter_alias(&vnic_name(interface)));
    let switch = ps_literal(SWITCH_NAME);
    let device = ps_literal(device);
    let vlan_mode = match interface.vlan {
        Some(id) => format!("-Access -VlanId {id}"),
        None => "-Untagged".to_string(),
    };
    format!(
        "$created = $false; \
         if (-not (Get-VMSwitch -Name {switch} -ErrorAction SilentlyContinue)) {{ \
         New-VMSwitch -Name {switch} -NetAdapterName {device} -AllowManagementOS $false | Out-Null; \
         $created = $true }}; \
         try {{ \
         $swType = (Get-VMSwitch -Name {switch}).SwitchType; \
         if ($swType -ne 'External') {{ \
         throw 'switch ' + {switch} + ' is ' + $swType + ', not External — ' + \
         {device} + ' was not bound (Hyper-V accepted the request without ' + \
         'erroring, which happens when the adapter cannot be claimed as an ' + \
         'external uplink right now)' }}; \
         Add-VMNetworkAdapter -ManagementOS -SwitchName {switch} -Name {vnic} | Out-Null; \
         Set-VMNetworkAdapterVlan -ManagementOS -VMNetworkAdapterName {vnic} {vlan_mode}; \
         $tries = 0; \
         while (-not (Get-NetAdapter -InterfaceAlias {alias} -ErrorAction SilentlyContinue)) {{ \
         if (++$tries -gt {ADAPTER_WAIT_POLLS}) {{ throw 'adapter ' + {alias} + ' did not appear' }}; \
         Start-Sleep -Milliseconds 500 }} \
         }} catch {{ \
         Remove-VMNetworkAdapter -ManagementOS -Name {vnic} -ErrorAction SilentlyContinue; \
         if ($created) {{ Remove-VMSwitch -Name {switch} -Force -ErrorAction SilentlyContinue }}; \
         throw }}"
    )
}

/// The script that removes one virtual adapter, and the switch when that
/// was its last one.
///
/// The adapter's persistent routes go first, explicitly. Removing the
/// adapter drops them from the active table, but `netsh ... add route`
/// also wrote them to the persistent store, and a persistent route whose
/// adapter is gone has been seen to linger there and surface against the
/// parent's own interface index after the switch released it (bench,
/// 2026-09-28: the profile's `/32` routes still listed after a full
/// revert). Both cmdlets run with `SilentlyContinue` because an adapter
/// with no routes is the ordinary case, not a failure.
fn teardown_script(vnic: &str) -> String {
    let alias = ps_literal(&adapter_alias(vnic));
    let vnic = ps_literal(vnic);
    let switch = ps_literal(SWITCH_NAME);
    format!(
        "if (Get-VMNetworkAdapter -ManagementOS -Name {vnic} -ErrorAction SilentlyContinue) {{ \
         Get-NetRoute -InterfaceAlias {alias} -PolicyStore PersistentStore -ErrorAction SilentlyContinue \
         | Remove-NetRoute -Confirm:$false -ErrorAction SilentlyContinue; \
         Remove-VMNetworkAdapter -ManagementOS -Name {vnic} }}; \
         if ((Get-VMSwitch -Name {switch} -ErrorAction SilentlyContinue) -and \
         -not (Get-VMNetworkAdapter -ManagementOS -SwitchName {switch} -ErrorAction SilentlyContinue)) {{ \
         Remove-VMSwitch -Name {switch} -Force }}"
    )
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

    // ── naming ──────────────────────────────────────────────────────────

    #[test]
    fn a_tagged_entry_is_the_vethernet_alias_of_its_vlan() {
        assert_eq!(
            WindowsHyperV.iface_name(&tagged(11, "192.168.11.87/24"), "Ethernet 2"),
            "vEthernet (vlan11)"
        );
    }

    #[test]
    fn an_untagged_entry_is_an_adapter_too_not_the_parent() {
        let i = Interface {
            vlan: None,
            address: "192.168.1.100/24".parse().unwrap(),
            mtu: None,
            routes: vec![],
        };
        // The parent's own stack is gone once it is bound to the switch, so
        // an untagged entry cannot live there as it does on macOS/Linux.
        assert_eq!(
            WindowsHyperV.iface_name(&i, "Ethernet 2"),
            "vEthernet (untagged)"
        );
    }

    #[test]
    fn the_alias_round_trips_to_the_adapter_name() {
        assert_eq!(vnic_from_alias("vEthernet (vlan11)"), Some("vlan11"));
        assert_eq!(vnic_from_alias("vEthernet (untagged)"), Some("untagged"));
        assert_eq!(vnic_from_alias("Ethernet 2"), None);
        assert_eq!(vnic_from_alias("vEthernet (vlan11) 2"), None);
    }

    // ── rendering ───────────────────────────────────────────────────────

    #[test]
    fn rendering_of_a_tagged_profile_with_routes_is_pinned() {
        let i = Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: None,
            routes: vec![
                Route {
                    destination: "192.168.11.151/32".to_string(),
                    gateway: None,
                    mac: None,
                },
                Route {
                    destination: "239.255.0.255/32".to_string(),
                    gateway: None,
                    mac: None,
                },
            ],
        };
        let rendered = render(&bringup_commands_for(&WindowsHyperV, &i, "Ethernet 2"));
        assert_eq!(
            rendered,
            vec![
                format!(
                    "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \
                     $ErrorActionPreference = 'Stop'; {}",
                    create_script(&i, "Ethernet 2")
                ),
                "netsh interface ipv4 set address vEthernet (vlan11) static 192.168.11.87 \
                 255.255.255.0"
                    .to_string(),
                // In-subnet gatewayless: a route anyway, as on Linux — the
                // stack ARPs for an on-link destination.
                "netsh interface ipv4 add route 192.168.11.151/32 vEthernet (vlan11)".to_string(),
                "netsh interface ipv4 add route 239.255.0.255/32 vEthernet (vlan11)".to_string(),
            ],
            "Windows rendering changed"
        );
    }

    #[test]
    fn the_create_script_is_pinned() {
        let i = tagged(11, "192.168.11.87/24");
        assert_eq!(
            create_script(&i, "Ethernet 2"),
            "$created = $false; \
             if (-not (Get-VMSwitch -Name 'vlanctl' -ErrorAction SilentlyContinue)) { \
             New-VMSwitch -Name 'vlanctl' -NetAdapterName 'Ethernet 2' -AllowManagementOS $false | Out-Null; \
             $created = $true }; \
             try { \
             $swType = (Get-VMSwitch -Name 'vlanctl').SwitchType; \
             if ($swType -ne 'External') { \
             throw 'switch ' + 'vlanctl' + ' is ' + $swType + ', not External — ' + \
             'Ethernet 2' + ' was not bound (Hyper-V accepted the request without ' + \
             'erroring, which happens when the adapter cannot be claimed as an ' + \
             'external uplink right now)' }; \
             Add-VMNetworkAdapter -ManagementOS -SwitchName 'vlanctl' -Name 'vlan11' | Out-Null; \
             Set-VMNetworkAdapterVlan -ManagementOS -VMNetworkAdapterName 'vlan11' -Access -VlanId 11; \
             $tries = 0; \
             while (-not (Get-NetAdapter -InterfaceAlias 'vEthernet (vlan11)' -ErrorAction SilentlyContinue)) { \
             if (++$tries -gt 40) { throw 'adapter ' + 'vEthernet (vlan11)' + ' did not appear' }; \
             Start-Sleep -Milliseconds 500 } \
             } catch { \
             Remove-VMNetworkAdapter -ManagementOS -Name 'vlan11' -ErrorAction SilentlyContinue; \
             if ($created) { Remove-VMSwitch -Name 'vlanctl' -Force -ErrorAction SilentlyContinue }; \
             throw }"
        );
    }

    /// Bench-measured 2026-09-28: `New-VMSwitch` can accept a request it
    /// cannot actually fulfil — no terminating error, just an `Internal`
    /// switch with no adapter bound — when the requested adapter cannot be
    /// claimed as an external uplink right now (observed after manually
    /// toggling the adapter's Hyper-V extensibility binding outside this
    /// backend's own create/teardown lifecycle). Every vNIC added to that
    /// switch then reports created successfully and shows `Disconnected`
    /// forever, and the apply as a whole reports `ok: true`. This pins that
    /// the create script itself catches that shape and never reports
    /// success over it: the `SwitchType` check runs before
    /// `Add-VMNetworkAdapter`, inside the same `try` the adapter-wait
    /// timeout uses, so both failure shapes roll back through the same
    /// path.
    #[test]
    fn the_create_script_checks_switch_type_before_adding_the_vnic() {
        let script = create_script(&tagged(11, "192.168.11.87/24"), "Ethernet 2");
        let switch_check_pos = script
            .find("$swType = (Get-VMSwitch -Name 'vlanctl')")
            .unwrap();
        let add_vnic_pos = script.find("Add-VMNetworkAdapter").unwrap();
        assert!(
            switch_check_pos < add_vnic_pos,
            "the SwitchType check must run before a vNIC is ever added to the switch"
        );
        let try_pos = script.find("try {").unwrap();
        let catch_pos = script.find("} catch {").unwrap();
        assert!(
            try_pos < switch_check_pos && switch_check_pos < catch_pos,
            "the check must be inside the try block so a throw reaches the rollback catch"
        );
        assert!(
            script.contains("not External"),
            "the failure must name what went wrong, not just that something did: {script}"
        );
    }

    #[test]
    fn an_untagged_entry_creates_an_untagged_adapter_and_a_static_neighbor() {
        let i = Interface {
            vlan: None,
            address: "192.168.1.100/24".parse().unwrap(),
            mtu: Some(1500),
            routes: vec![Route {
                destination: "192.168.10.151/32".to_string(),
                gateway: None,
                mac: Some("00:00:5e:00:53:01".to_string()),
            }],
        };
        let cmds = bringup_commands_for(&WindowsHyperV, &i, "Ethernet 2");
        let script = powershell_script(&cmds[0]).unwrap();
        assert!(
            script.contains("-Name 'untagged' | Out-Null")
                && script.contains("-VMNetworkAdapterName 'untagged' -Untagged;"),
            "untagged adapter in untagged mode: {script}"
        );
        assert_eq!(
            render(&cmds[1..]),
            vec![
                "netsh interface ipv4 set address vEthernet (untagged) static 192.168.1.100 \
                 255.255.255.0",
                "netsh interface ipv4 set subinterface vEthernet (untagged) mtu=1500 \
                 store=persistent",
                "netsh interface ipv4 add route 192.168.10.151/32 vEthernet (untagged)",
                // The MAC in netsh's dashed form.
                "netsh interface ipv4 add neighbors vEthernet (untagged) 192.168.10.151 \
                 00-00-5e-00-53-01",
            ]
        );
    }

    #[test]
    fn a_gateway_route_names_the_next_hop_and_default_becomes_a_zero_prefix() {
        let route = Route {
            destination: "default".to_string(),
            gateway: Some("192.168.11.1".parse().unwrap()),
            mac: None,
        };
        assert_eq!(
            render(&WindowsHyperV.route_commands(&route, false, "vEthernet (vlan11)")),
            vec!["netsh interface ipv4 add route 0.0.0.0/0 vEthernet (vlan11) 192.168.11.1"]
        );
    }

    #[test]
    fn teardown_removes_the_adapter_and_the_switch_when_it_was_the_last() {
        let rendered = render(&WindowsHyperV.teardown_commands("vEthernet (vlan11)"));
        assert_eq!(rendered.len(), 1);
        let script = powershell_script(&WindowsHyperV.teardown_commands("vEthernet (vlan11)")[0])
            .unwrap()
            .to_string();
        assert_eq!(
            script,
            "$ErrorActionPreference = 'Stop'; \
             if (Get-VMNetworkAdapter -ManagementOS -Name 'vlan11' -ErrorAction SilentlyContinue) { \
             Get-NetRoute -InterfaceAlias 'vEthernet (vlan11)' -PolicyStore PersistentStore -ErrorAction SilentlyContinue \
             | Remove-NetRoute -Confirm:$false -ErrorAction SilentlyContinue; \
             Remove-VMNetworkAdapter -ManagementOS -Name 'vlan11' }; \
             if ((Get-VMSwitch -Name 'vlanctl' -ErrorAction SilentlyContinue) -and \
             -not (Get-VMNetworkAdapter -ManagementOS -SwitchName 'vlanctl' -ErrorAction SilentlyContinue)) { \
             Remove-VMSwitch -Name 'vlanctl' -Force }"
        );
    }

    // ── the parent's own configuration ──────────────────────────────────

    /// Bench, 2026-09-28: after Apply → Revert the parent adapter was back
    /// on its own stack, still marked static, and holding no address —
    /// `New-VMSwitch` had cleared the static `192.168.11.87/24` and
    /// `Remove-VMSwitch` did not restore it. The host sat on APIPA and
    /// nothing could reach the sensor's subnet until an operator re-entered
    /// the address by hand. These pin the record-and-restore round trip.
    #[test]
    fn the_parent_probe_is_read_only_and_parses_a_static_configuration() {
        let probe = parent_config_probe("Ethernet 2");
        assert!(
            WindowsHyperV::is_read_only_probe(&probe),
            "{}",
            probe.display()
        );
        let script = powershell_script(&probe).unwrap();
        assert!(script.contains("Get-NetIPInterface -InterfaceAlias 'Ethernet 2'"));
        assert!(script.contains("$_.PrefixOrigin -eq 'Manual'"));
        assert!(script.ends_with("exit 0"));

        let parsed = parse_parent_config(
            "Ethernet 2",
            "dhcp Disabled\r\naddress 192.168.11.87/24\r\naddress 192.168.10.90/24\r\n\
             gateway 192.168.11.1\r\n",
        )
        .unwrap();
        assert_eq!(parsed.device, "Ethernet 2");
        assert!(!parsed.dhcp);
        assert_eq!(
            parsed.addresses,
            vec![
                "192.168.11.87/24".parse::<IpNet>().unwrap(),
                "192.168.10.90/24".parse::<IpNet>().unwrap()
            ]
        );
        assert_eq!(parsed.gateway, Some("192.168.11.1".parse().unwrap()));
    }

    #[test]
    fn the_parent_probe_parses_dhcp_and_nothing_at_all() {
        // DHCP: recorded as such, with the lease deliberately not captured.
        let dhcp = parse_parent_config("Ethernet 2", "dhcp Enabled\n").unwrap();
        assert!(dhcp.dhcp);
        assert!(dhcp.addresses.is_empty());
        assert_eq!(dhcp.gateway, None);
        // No IPv4 interface at all (already bound to a switch): nothing to
        // put back, so nothing is recorded.
        assert_eq!(parse_parent_config("Ethernet 2", ""), None);
        assert_eq!(
            parse_parent_config("Ethernet 2", "address 10.0.0.1/8\n"),
            None
        );
        // A zero next hop is "no gateway", and an IPv6 address is not ours.
        let odd = parse_parent_config(
            "Ethernet 2",
            "dhcp Disabled\naddress fe80::1/64\ngateway 0.0.0.0\n",
        )
        .unwrap();
        assert!(odd.addresses.is_empty());
        assert_eq!(odd.gateway, None);
    }

    #[test]
    fn restoring_a_static_parent_waits_for_it_and_sets_every_address() {
        let parent = ParentConfig {
            device: "Ethernet 2".to_string(),
            dhcp: false,
            addresses: vec![
                "192.168.11.87/24".parse().unwrap(),
                "192.168.10.90/24".parse().unwrap(),
            ],
            gateway: Some("192.168.11.1".parse().unwrap()),
        };
        let cmds = WindowsHyperV.parent_restore_commands(&parent);
        let wait = powershell_script(&cmds[0]).unwrap();
        assert!(
            wait.contains("Get-NetIPInterface -InterfaceAlias 'Ethernet 2'")
                && wait.contains("did not return to the IP stack"),
            "{wait}"
        );
        assert_eq!(
            render(&cmds[1..]),
            vec![
                "netsh interface ipv4 set address Ethernet 2 static 192.168.11.87 255.255.255.0 \
                 192.168.11.1",
                "netsh interface ipv4 add address Ethernet 2 192.168.10.90 255.255.255.0",
            ]
        );
        // None of the restore is a probe: a dry run must not run it.
        for cmd in &cmds {
            assert!(!WindowsHyperV::is_read_only_probe(cmd), "{}", cmd.display());
        }
    }

    #[test]
    fn restoring_a_dhcp_parent_puts_it_back_on_dhcp_and_a_bare_static_one_is_left_alone() {
        let dhcp = ParentConfig {
            device: "Ethernet 2".to_string(),
            dhcp: true,
            addresses: vec![],
            gateway: None,
        };
        let cmds = WindowsHyperV.parent_restore_commands(&dhcp);
        assert_eq!(cmds.len(), 2);
        assert_eq!(
            cmds[1].display(),
            "netsh interface ipv4 set address Ethernet 2 dhcp"
        );
        // Static with no address is what the bench found *after* the bug;
        // recording it must not make `down` invent a configuration.
        let bare = ParentConfig {
            device: "Ethernet 2".to_string(),
            dhcp: false,
            addresses: vec![],
            gateway: None,
        };
        assert!(WindowsHyperV.parent_restore_commands(&bare).is_empty());
    }

    #[test]
    fn teardown_of_an_unrecognised_name_treats_it_as_an_adapter_name() {
        // A hand-edited state file naming the adapter without its alias
        // still tears it down, rather than silently doing nothing.
        let cmds = WindowsHyperV.teardown_commands("vlan11");
        assert!(
            powershell_script(&cmds[0])
                .unwrap()
                .contains("-Name 'vlan11'")
        );
    }

    #[test]
    fn only_the_create_script_is_recorded() {
        let i = tagged(11, "192.168.11.87/24");
        let cmds = bringup_commands_for(&WindowsHyperV, &i, "Ethernet 2");
        let recorded: Vec<bool> = cmds
            .iter()
            .map(|c| WindowsHyperV.records_created_interface(c))
            .collect();
        assert_eq!(recorded, vec![true, false]);
        assert!(!WindowsHyperV.records_created_interface(&WindowsHyperV.teardown_commands("x")[0]));
    }

    // ── escaping ────────────────────────────────────────────────────────

    #[test]
    fn a_quote_in_a_device_name_cannot_end_the_literal() {
        // `'` is the only character with meaning inside a single-quoted
        // PowerShell string, and doubling it is its only escape.
        assert_eq!(ps_literal("Ethernet 2"), "'Ethernet 2'");
        assert_eq!(ps_literal("it's"), "'it''s'");
        assert_eq!(ps_literal("$env:X; Remove-Item"), "'$env:X; Remove-Item'");
        let script = create_script(
            &tagged(11, "192.168.11.87/24"),
            "x'; Remove-VMSwitch -Name 'vlanctl",
        );
        assert!(script.contains("-NetAdapterName 'x''; Remove-VMSwitch -Name ''vlanctl'"));
    }

    #[test]
    fn scripts_carry_no_double_quotes() {
        // The one argv element that reaches PowerShell is quoted by
        // `Command::args`; a `"` inside would be the character that
        // quoting has to escape, and PowerShell's own command-line
        // parsing of `-Command` is not something to lean on.
        let i = Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: Some(9000),
            routes: vec![Route {
                destination: "192.168.11.151/32".to_string(),
                gateway: None,
                mac: Some("00:00:5e:00:53:01".to_string()),
            }],
        };
        let mut all = bringup_commands_for(&WindowsHyperV, &i, "Ethernet 2");
        all.extend(WindowsHyperV.teardown_commands("vEthernet (vlan11)"));
        all.push(adapter_property_probe("Ethernet 2", "Status"));
        for cmd in &all {
            if let Some(script) = powershell_script(cmd) {
                assert!(!script.contains('"'), "double quote in script: {script}");
            }
        }
    }

    // ── probes and their parsers ─────────────────────────────────────────

    fn runner_with(cmd: &Cmd, output: &str) -> RecordingRunner {
        let mut runner = RecordingRunner::default();
        runner.stdout.insert(cmd.display(), output.to_string());
        runner
    }

    #[test]
    fn list_devices_reads_one_adapter_name_per_line() {
        let probe = powershell("Get-NetAdapter | ForEach-Object { $_.Name }");
        let mut runner = runner_with(&probe, "Ethernet 2\r\nWi-Fi\r\nvEthernet (vlan11)\r\n");
        assert_eq!(
            WindowsHyperV.list_devices(&mut runner).unwrap(),
            vec!["Ethernet 2", "Wi-Fi", "vEthernet (vlan11)"]
        );
    }

    #[test]
    fn addresses_on_parses_cidrs_and_an_absent_adapter_has_none() {
        let probe = powershell(
            "Get-NetIPAddress -InterfaceAlias 'vEthernet (vlan11)' -AddressFamily IPv4 \
             -ErrorAction SilentlyContinue | ForEach-Object { $_.IPAddress + '/' + $_.PrefixLength }; \
             exit 0",
        );
        let mut runner = runner_with(&probe, "192.168.11.87/24\r\n169.254.12.34/16\r\n");
        assert_eq!(
            WindowsHyperV
                .addresses_on(&mut runner, "vEthernet (vlan11)")
                .unwrap(),
            vec![
                "192.168.11.87/24".parse::<IpNet>().unwrap(),
                "169.254.12.34/16".parse::<IpNet>().unwrap()
            ]
        );
        // Before the adapter exists — the untagged idempotency check runs
        // here — the probe prints nothing, and that must read as "no
        // addresses", not as a failure.
        let mut runner = runner_with(&probe, "");
        assert_eq!(
            WindowsHyperV
                .addresses_on(&mut runner, "vEthernet (vlan11)")
                .unwrap(),
            Vec::<IpNet>::new()
        );
    }

    #[test]
    fn is_wireless_reads_the_physical_media_type() {
        let mut runner = runner_with(
            &adapter_property_probe("Wi-Fi", "PhysicalMediaType"),
            "Native 802.11\r\n",
        );
        assert!(WindowsHyperV.is_wireless(&mut runner, "Wi-Fi").unwrap());
        let mut runner = runner_with(
            &adapter_property_probe("Ethernet 2", "PhysicalMediaType"),
            "802.3\r\n",
        );
        assert!(
            !WindowsHyperV
                .is_wireless(&mut runner, "Ethernet 2")
                .unwrap()
        );
    }

    #[test]
    fn link_is_active_only_for_status_up() {
        let mut runner = runner_with(&adapter_property_probe("Ethernet 2", "Status"), "Up\r\n");
        assert!(
            WindowsHyperV
                .link_is_active(&mut runner, "Ethernet 2")
                .unwrap()
        );
        let mut runner = runner_with(
            &adapter_property_probe("Ethernet 2", "Status"),
            "Disconnected\r\n",
        );
        assert!(
            !WindowsHyperV
                .link_is_active(&mut runner, "Ethernet 2")
                .unwrap()
        );
    }

    #[test]
    fn a_missing_adapter_is_a_failure_to_ask_not_an_answer() {
        // The real probe `throw`s for an adapter that is not there, which
        // `SystemRunner` surfaces as a failed command. `fail_at` stands in
        // for that here, and the failure must propagate as `Err` — never
        // become "not wireless" or "no link".
        let failing = || RecordingRunner {
            fail_at: Some(0),
            ..RecordingRunner::default()
        };
        let err = WindowsHyperV
            .link_is_active(&mut failing(), "Nope")
            .unwrap_err();
        assert!(err.to_string().contains("Nope"), "{err}");
        assert!(WindowsHyperV.is_wireless(&mut failing(), "Nope").is_err());
    }

    #[test]
    fn candidate_devices_exclude_virtual_wireless_and_placeholder_adapters() {
        assert!(WindowsHyperV.is_candidate_device("Ethernet"));
        assert!(WindowsHyperV.is_candidate_device("Ethernet 2"));
        assert!(!WindowsHyperV.is_candidate_device("vEthernet (vlan11)"));
        assert!(!WindowsHyperV.is_candidate_device("vEthernet (Default Switch)"));
        assert!(!WindowsHyperV.is_candidate_device("Wi-Fi"));
        assert!(!WindowsHyperV.is_candidate_device("Local Area Connection* 1"));
        assert!(!WindowsHyperV.is_candidate_device("Loopback Pseudo-Interface 1"));
        assert!(!WindowsHyperV.is_candidate_device("Bluetooth Network Connection"));
    }

    // ── the dry-run allowlist ────────────────────────────────────────────

    #[test]
    fn every_probe_is_read_only_and_nothing_else_is() {
        for probe in [
            powershell("Get-NetAdapter | ForEach-Object { $_.Name }"),
            powershell(
                "Get-NetIPAddress -InterfaceAlias 'Ethernet 2' -AddressFamily IPv4 \
                 -ErrorAction SilentlyContinue | ForEach-Object { $_.IPAddress + '/' + $_.PrefixLength }; \
                 exit 0",
            ),
            adapter_property_probe("Ethernet 2", "Status"),
            adapter_property_probe("Ethernet 2", "PhysicalMediaType"),
            parent_config_probe("Ethernet 2"),
        ] {
            assert!(
                WindowsHyperV::is_read_only_probe(&probe),
                "{}",
                probe.display()
            );
        }
        // A probe shape with a mutation smuggled into the device name is
        // not a probe, for the parent probe as for the others.
        let smuggled = parent_config_probe("x'; Remove-VMSwitch -Name 'vlanctl");
        assert!(!WindowsHyperV::is_read_only_probe(&smuggled));
        let i = tagged(11, "192.168.11.87/24");
        for mutation in bringup_commands_for(&WindowsHyperV, &i, "Ethernet 2")
            .iter()
            .chain(WindowsHyperV.teardown_commands("vEthernet (vlan11)").iter())
        {
            assert!(
                !WindowsHyperV::is_read_only_probe(mutation),
                "{}",
                mutation.display()
            );
        }
        // A probe shape with a mutation smuggled into the device name is
        // not a probe: the literal keeps it inert, and the allowlist keeps
        // it out of a dry run regardless.
        let smuggled = adapter_property_probe("x'; Remove-VMSwitch -Name 'vlanctl", "Status");
        assert!(!WindowsHyperV::is_read_only_probe(&smuggled));
        assert!(!WindowsHyperV::is_read_only_probe(&Cmd::new(
            "powershell.exe",
            &["-Command", "Get-NetAdapter"]
        )));
        assert!(!WindowsHyperV::is_read_only_probe(&Cmd::new(
            "netsh",
            &["interface", "ipv4", "show", "addresses"]
        )));
    }
}
