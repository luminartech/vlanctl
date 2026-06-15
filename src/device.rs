use crate::net::{Cmd, CommandRunner};
use anyhow::{bail, Result};

/// Resolve the physical Ethernet device to attach VLANs to.
/// If `override_device` is set, use it; otherwise pick the first active
/// Ethernet interface reported by `ifconfig`.
pub fn resolve_device<R: CommandRunner>(
    runner: &mut R,
    override_device: Option<&str>,
) -> Result<String> {
    if let Some(dev) = override_device {
        return Ok(dev.to_string());
    }
    let output = runner.run(&Cmd::new("ifconfig", &["-l"]))?;
    // `ifconfig -l` prints a space-separated list of interface names.
    let candidate = output
        .split_whitespace()
        .find(|name| name.starts_with("en"));
    match candidate {
        Some(name) => Ok(name.to_string()),
        None => bail!("could not auto-detect an Ethernet (enX) device; set `device` in the profile"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::RecordingRunner;

    #[test]
    fn override_wins() {
        let mut r = RecordingRunner::default();
        let dev = resolve_device(&mut r, Some("en42")).unwrap();
        assert_eq!(dev, "en42");
        assert!(r.commands.is_empty(), "override should not query ifconfig");
    }

    #[test]
    fn picks_first_en_interface() {
        let mut r = RecordingRunner::default();
        r.stdout
            .insert("ifconfig -l".to_string(), "lo0 en0 en10 bridge0".to_string());
        let dev = resolve_device(&mut r, None).unwrap();
        assert_eq!(dev, "en0");
    }

    #[test]
    fn errors_when_no_en_interface() {
        let mut r = RecordingRunner::default();
        r.stdout.insert("ifconfig -l".to_string(), "lo0 bridge0".to_string());
        assert!(resolve_device(&mut r, None).is_err());
    }
}
