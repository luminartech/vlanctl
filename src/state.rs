use anyhow::{Context, Result};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
const STATE_PATH: &str = "/usr/local/var/vlanctl/state.json";

/// The parent device's own IPv4 configuration as it stood before an apply
/// took the device over, recorded so `down` can put it back.
///
/// Only a backend whose apply *destroys* the parent's configuration records
/// one. On macOS and Linux a VLAN sub-interface sits beside the parent's own
/// addressing and nothing is lost, so the state file carries `None`. The
/// Hyper-V backend binds the parent to an external switch, and Windows
/// clears the parent's addresses when it does — measured 2026-09-28: a
/// parent with a static `192.168.11.87/24` came back from `Remove-VMSwitch`
/// still marked static but holding no address at all, so it sat on an APIPA
/// address and nothing on the host could reach the sensor's subnet again.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ParentConfig {
    /// The parent device, in the platform's own naming (`Ethernet 2`).
    pub device: String,
    /// Whether the device obtained its address by DHCP. A DHCP device is
    /// put back on DHCP; a static one gets its addresses back.
    pub dhcp: bool,
    /// The statically configured IPv4 addresses, with prefix length. Empty
    /// for a DHCP device (its lease is not something to restore by hand)
    /// and for a static device that had none.
    pub addresses: Vec<IpNet>,
    /// The default gateway on the device, if it had one.
    pub gateway: Option<Ipv4Addr>,
}

/// Persistent record of what vlanctl has brought up.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    /// Name of the currently active profile, if any.
    pub active_profile: Option<String>,
    /// Interface names (e.g. "vlan0") this tool created for the active profile.
    pub interfaces: Vec<String>,
    /// [`Platform::name`](crate::plan::Platform::name) of the backend that
    /// applied the active profile, so `down` runs through the same one.
    /// `None` in a state file written before this field existed; a reader
    /// then falls back to the host's default backend, which is the only
    /// backend such a file can have come from.
    #[serde(default)]
    pub backend: Option<String>,
    /// What the parent device carried before the active profile was applied,
    /// for the backends whose apply destroys it
    /// ([`Platform::parent_snapshot`](crate::plan::Platform::parent_snapshot)).
    /// `None` when nothing needs putting back, and in a state file written
    /// before this field existed.
    #[serde(default)]
    pub parent: Option<ParentConfig>,
}

impl State {
    /// Where the CLI keeps its state: `/usr/local/var/vlanctl/state.json` on
    /// macOS and Linux, `%ProgramData%\vlanctl\state.json` on Windows. A
    /// library consumer chooses its own path and need not use this.
    pub fn default_path() -> PathBuf {
        #[cfg(windows)]
        {
            // The machine-wide application-data root, which is what a state
            // file written by an elevated process and read back by the next
            // one should live under. The variable is always set on a
            // running Windows; the literal is the value it has had since
            // Vista, kept only so an oddly-scrubbed environment still gets
            // a sensible path rather than a relative one.
            std::env::var_os("ProgramData")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
                .join("vlanctl")
                .join("state.json")
        }
        #[cfg(not(windows))]
        {
            PathBuf::from(STATE_PATH)
        }
    }

    /// Load state from `path`; a missing file yields the default (empty) state.
    pub fn load(path: &Path) -> Result<State> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Save state to `path`, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_default_state() {
        let path = std::env::temp_dir().join("vlanctl-nonexistent-xyz.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(State::load(&path).unwrap(), State::default());
    }

    #[test]
    fn round_trips_through_disk() {
        let path = std::env::temp_dir().join("vlanctl-test-state.json");
        let state = State {
            active_profile: Some("example_bench".to_string()),
            interfaces: vec!["vlan0".to_string(), "vlan1".to_string()],
            backend: Some("macos".to_string()),
            parent: None,
        };
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), state);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_state_file_from_before_the_backend_field_still_loads() {
        let path = std::env::temp_dir().join("vlanctl-test-state-old.json");
        std::fs::write(
            &path,
            r#"{ "active_profile": "lum", "interfaces": ["vlan10", "vlan11"] }"#,
        )
        .unwrap();
        let state = State::load(&path).unwrap();
        assert_eq!(state.active_profile.as_deref(), Some("lum"));
        assert_eq!(state.backend, None);
        std::fs::remove_file(&path).unwrap();
    }
}
