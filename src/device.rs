use crate::net::{Cmd, CommandRunner};
use crate::plan::Platform;
use anyhow::{Result, bail};

/// Resolve the physical Ethernet device to attach VLANs to.
///
/// If `override_device` is set, use it verbatim. Otherwise auto-detect: take
/// the platform's candidate devices ([`Platform::is_candidate_device`]) and
/// drop Wi-Fi (which cannot carry 802.1Q VLANs). Auto-pick
/// only when the choice is unambiguous:
/// - exactly one active-link wired interface -> use it;
/// - several active -> error (ask the user to pin `device`);
/// - none active but exactly one wired interface exists -> use it;
/// - none active and several wired exist -> error (ask the user to pin `device`).
pub fn resolve_device(
    platform: &dyn Platform,
    runner: &mut dyn CommandRunner,
    override_device: Option<&str>,
) -> Result<String> {
    if let Some(dev) = override_device {
        return Ok(dev.to_string());
    }

    // Every probe below goes through `platform`, so this works on any host
    // with a backend rather than only on macOS. It previously shelled
    // `ifconfig -l` + `networksetup` directly and filtered candidates by an
    // `en` prefix — none of which exists or applies on Linux, where the
    // parent is typically `eth0`.
    let devices = platform.list_devices(runner)?;
    let mut wired = Vec::new();
    for name in devices {
        if !platform.is_candidate_device(&name) {
            continue;
        }
        if platform.is_wireless(runner, &name)? {
            continue;
        }
        wired.push(name);
    }

    if wired.is_empty() {
        bail!("no wired Ethernet interface found on this host; set `device` in the profile");
    }

    let mut active = Vec::new();
    for name in &wired {
        if platform.link_is_active(runner, name)? {
            active.push(name.clone());
        }
    }

    match active.as_slice() {
        [one] => Ok(one.clone()),
        many if many.len() > 1 => bail!(
            "multiple active Ethernet interfaces ({}); set `device` in the profile to choose one",
            many.join(", ")
        ),
        // No active link: only auto-pick when there is a single wired adapter;
        // otherwise refuse to guess and make the user choose.
        _ if wired.len() == 1 => Ok(wired[0].clone()),
        _ => bail!(
            "multiple wired Ethernet interfaces and none has an active link ({}); \
             set `device` in the profile to choose one",
            wired.join(", ")
        ),
    }
}

/// Devices whose hardware port is Wi-Fi, from `networksetup -listallhardwareports`.
/// Output is a series of blocks; a `Hardware Port:` line names the port and the
/// following `Device:` line names its `enN` interface.
///
/// `pub(crate)`: also the parser behind [`crate::plan::Platform::is_wireless`].
pub(crate) fn wifi_devices<R: CommandRunner + ?Sized>(runner: &mut R) -> Result<Vec<String>> {
    let out = runner.run(&Cmd::new("networksetup", &["-listallhardwareports"]))?;
    let mut wifi = Vec::new();
    let mut current_is_wifi = false;
    for line in out.lines() {
        let line = line.trim();
        if let Some(port) = line.strip_prefix("Hardware Port:") {
            let port = port.trim().to_ascii_lowercase();
            current_is_wifi = port.contains("wi-fi") || port.contains("airport");
        } else if let Some(dev) = line.strip_prefix("Device:")
            && current_is_wifi
        {
            wifi.push(dev.trim().to_string());
        }
    }
    Ok(wifi)
}

/// Whether `ifconfig <iface>` reports `status: active`.
///
/// `pub(crate)`: also the parser behind [`crate::plan::Platform::link_is_active`].
pub(crate) fn interface_is_active<R: CommandRunner + ?Sized>(
    runner: &mut R,
    iface: &str,
) -> Result<bool> {
    let out = runner.run(&Cmd::new("ifconfig", &[iface]))?;
    Ok(out.lines().any(|line| {
        line.trim()
            .strip_prefix("status:")
            .is_some_and(|status| status.trim() == "active")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::RecordingRunner;
    use crate::plan::{MacOs, Platform};

    /// Two Wi-Fi (en0) + USB-LAN (en7) hardware ports, as `networksetup` prints them.
    fn hardware_ports() -> String {
        "\nHardware Port: Wi-Fi\nDevice: en0\nEthernet Address: 00:00:5e:00:53:03\n\n\
         Hardware Port: USB 10/100/1000 LAN\nDevice: en7\nEthernet Address: 00:00:5e:00:53:04\n\n\
         Hardware Port: Ethernet Adapter (en4)\nDevice: en4\nEthernet Address: 00:00:5e:00:53:05\n"
            .to_string()
    }

    fn runner() -> RecordingRunner {
        let mut r = RecordingRunner::default();
        r.stdout.insert(
            "networksetup -listallhardwareports".to_string(),
            hardware_ports(),
        );
        r
    }

    #[test]
    fn override_wins() {
        let mut r = RecordingRunner::default();
        let dev = resolve_device(&crate::plan::MacOs, &mut r, Some("en42")).unwrap();
        assert_eq!(dev, "en42");
        assert!(
            r.commands.is_empty(),
            "override should not probe the system"
        );
    }

    #[test]
    fn excludes_wifi_and_prefers_active_wired() {
        let mut r = runner();
        r.stdout.insert(
            "ifconfig -l".to_string(),
            "lo0 en0 en7 en4 bridge0".to_string(),
        );
        // Wi-Fi (en0) is active too, but must be excluded.
        r.stdout
            .insert("ifconfig en0".to_string(), "\tstatus: active\n".to_string());
        r.stdout
            .insert("ifconfig en7".to_string(), "\tstatus: active\n".to_string());
        r.stdout.insert(
            "ifconfig en4".to_string(),
            "\tstatus: inactive\n".to_string(),
        );
        assert_eq!(
            resolve_device(&crate::plan::MacOs, &mut r, None).unwrap(),
            "en7"
        );
    }

    /// The whole point of gap (2)+(5): auto-detect must work on a Linux
    /// host. `eth0` does not match the macOS `en` prefix, and `ifconfig -l`
    /// / `networksetup` do not exist there, so before this the resolve
    /// refused on the very hardware the Linux backend targets.
    #[test]
    fn resolves_eth0_on_linux_through_the_platform() {
        let mut r = RecordingRunner::default();
        r.stdout.insert(
            "ip -json link show".to_string(),
            r#"[{"ifname":"lo"},{"ifname":"eth0"},{"ifname":"docker0"},{"ifname":"vlan11"}]"#
                .to_string(),
        );
        r.stdout.insert(
            "ls /sys/class/net/eth0".to_string(),
            "carrier operstate".to_string(),
        );
        r.stdout.insert(
            "cat /sys/class/net/eth0/carrier".to_string(),
            "1\n".to_string(),
        );
        assert_eq!(
            resolve_device(&crate::plan::Linux, &mut r, None).unwrap(),
            "eth0"
        );
    }

    #[test]
    fn errors_when_multiple_active_wired() {
        let mut r = runner();
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 en7 en4".to_string());
        r.stdout
            .insert("ifconfig en7".to_string(), "\tstatus: active\n".to_string());
        r.stdout
            .insert("ifconfig en4".to_string(), "\tstatus: active\n".to_string());
        let err = resolve_device(&crate::plan::MacOs, &mut r, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("multiple active"));
        assert!(err.contains("en7") && err.contains("en4"));
    }

    #[test]
    fn uses_sole_wired_when_none_active() {
        let mut r = runner();
        // Only one wired candidate (en7); no status seeded -> inactive.
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 en7".to_string());
        assert_eq!(
            resolve_device(&crate::plan::MacOs, &mut r, None).unwrap(),
            "en7"
        );
    }

    #[test]
    fn errors_when_multiple_wired_none_active() {
        let mut r = runner();
        // Two wired candidates (en4, en7), neither with an active link.
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 en4 en7".to_string());
        let err = resolve_device(&crate::plan::MacOs, &mut r, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("none has an active link"));
        assert!(err.contains("en4") && err.contains("en7"));
    }

    #[test]
    fn errors_when_only_wifi() {
        let mut r = runner();
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0".to_string());
        assert!(resolve_device(&crate::plan::MacOs, &mut r, None).is_err());
    }

    #[test]
    fn macos_lists_devices_through_the_platform() {
        let mut r = RecordingRunner::default();
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 en7".to_string());
        let devs = MacOs.list_devices(&mut r).expect("listing succeeds");
        assert_eq!(devs, vec!["lo0", "en0", "en7"]);
    }

    #[test]
    fn macos_detects_wireless_from_networksetup() {
        let mut r = RecordingRunner::default();
        r.stdout.insert(
            "networksetup -listallhardwareports".to_string(),
            // Two ports as networksetup prints them; en0 is Wi-Fi.
            "Hardware Port: Wi-Fi\nDevice: en0\n\nHardware Port: USB 10/100/1000 LAN\nDevice: en7\n".to_string(),
        );
        assert!(MacOs.is_wireless(&mut r, "en0").expect("query succeeds"));
        assert!(!MacOs.is_wireless(&mut r, "en7").expect("query succeeds"));
    }

    #[test]
    fn macos_reads_link_status_through_the_platform() {
        let mut r = RecordingRunner::default();
        r.stdout
            .insert("ifconfig en7".to_string(), "\tstatus: active\n".to_string());
        r.stdout.insert(
            "ifconfig en4".to_string(),
            "\tstatus: inactive\n".to_string(),
        );
        assert!(MacOs.link_is_active(&mut r, "en7").expect("query succeeds"));
        assert!(!MacOs.link_is_active(&mut r, "en4").expect("query succeeds"));
    }

    #[test]
    fn macos_reads_addresses_through_the_platform() {
        let mut r = RecordingRunner::default();
        r.stdout.insert(
            "ifconfig en0".to_string(),
            "\tinet 192.168.1.100 netmask 0xffffff00 broadcast 192.168.1.255".to_string(),
        );
        let addrs = MacOs.addresses_on(&mut r, "en0").expect("query succeeds");
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].addr().to_string(), "192.168.1.100");
        assert_eq!(addrs[0].prefix_len(), 24);
    }
}
