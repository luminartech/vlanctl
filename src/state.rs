use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
const STATE_PATH: &str = "/usr/local/var/vlanctl/state.json";

/// Persistent record of what vlanctl has brought up.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    /// Name of the currently active profile, if any.
    pub active_profile: Option<String>,
    /// Interface names (e.g. "vlan0") this tool created for the active profile.
    pub interfaces: Vec<String>,
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
        };
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), state);
        std::fs::remove_file(&path).unwrap();
    }
}
