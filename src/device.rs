use crate::net::{Cmd, CommandRunner};
use anyhow::{Result, bail};

/// Resolve the physical Ethernet device to attach VLANs to.
///
/// If `override_device` is set, use it verbatim. Otherwise auto-detect: take the
/// `en*` interfaces and drop Wi-Fi (which cannot carry 802.1Q VLANs). Auto-pick
/// only when the choice is unambiguous:
/// - exactly one active-link wired interface -> use it;
/// - several active -> error (ask the user to pin `device`);
/// - none active but exactly one wired interface exists -> use it;
/// - none active and several wired exist -> error (ask the user to pin `device`).
pub fn resolve_device<R: CommandRunner>(
    runner: &mut R,
    override_device: Option<&str>,
) -> Result<String> {
    if let Some(dev) = override_device {
        return Ok(dev.to_string());
    }

    let wifi = wifi_devices(runner)?;
    let listing = runner.run(&Cmd::new("ifconfig", &["-l"]))?;
    let wired: Vec<String> = listing
        .split_whitespace()
        .filter(|name| name.starts_with("en"))
        .filter(|name| !wifi.iter().any(|w| w == name))
        .map(|s| s.to_string())
        .collect();

    if wired.is_empty() {
        bail!("no wired Ethernet (enX) interface found; set `device` in the profile");
    }

    let mut active = Vec::new();
    for name in &wired {
        if interface_is_active(runner, name)? {
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
fn wifi_devices<R: CommandRunner>(runner: &mut R) -> Result<Vec<String>> {
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
fn interface_is_active<R: CommandRunner>(runner: &mut R, iface: &str) -> Result<bool> {
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

    /// Two Wi-Fi (en0) + USB-LAN (en7) hardware ports, as `networksetup` prints them.
    fn hardware_ports() -> String {
        "\nHardware Port: Wi-Fi\nDevice: en0\nEthernet Address: 84:2f:57:5f:c3:1a\n\n\
         Hardware Port: USB 10/100/1000 LAN\nDevice: en7\nEthernet Address: 98:fc:84:ec:ea:f1\n\n\
         Hardware Port: Ethernet Adapter (en4)\nDevice: en4\nEthernet Address: d6:d5:7e:e5:7c:1b\n"
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
        let dev = resolve_device(&mut r, Some("en42")).unwrap();
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
        assert_eq!(resolve_device(&mut r, None).unwrap(), "en7");
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
        let err = resolve_device(&mut r, None).unwrap_err().to_string();
        assert!(err.contains("multiple active"));
        assert!(err.contains("en7") && err.contains("en4"));
    }

    #[test]
    fn uses_sole_wired_when_none_active() {
        let mut r = runner();
        // Only one wired candidate (en7); no status seeded -> inactive.
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 en7".to_string());
        assert_eq!(resolve_device(&mut r, None).unwrap(), "en7");
    }

    #[test]
    fn errors_when_multiple_wired_none_active() {
        let mut r = runner();
        // Two wired candidates (en4, en7), neither with an active link.
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 en4 en7".to_string());
        let err = resolve_device(&mut r, None).unwrap_err().to_string();
        assert!(err.contains("none has an active link"));
        assert!(err.contains("en4") && err.contains("en7"));
    }

    #[test]
    fn errors_when_only_wifi() {
        let mut r = runner();
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0".to_string());
        assert!(resolve_device(&mut r, None).is_err());
    }
}
