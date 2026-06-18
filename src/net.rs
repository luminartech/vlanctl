use anyhow::{Result, bail};
use std::process::Command;

/// One external command to execute (program + args), e.g. `ifconfig vlan0 create`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// When true, a run failure is ignored by apply instead of being a hard
    /// error (used for `arp -d`, which fails when no entry exists).
    pub best_effort: bool,
}

impl Cmd {
    pub fn new(program: &str, args: &[&str]) -> Cmd {
        Cmd {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            best_effort: false,
        }
    }

    /// Like `new`, but a run failure is ignored by apply.
    pub fn new_best_effort(program: &str, args: &[&str]) -> Cmd {
        let mut cmd = Cmd::new(program, args);
        cmd.best_effort = true;
        cmd
    }

    /// Render as a shell-like string for display (show / dry-run).
    pub fn display(&self) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.args.iter().cloned());
        parts.join(" ")
    }
}

/// Abstraction over running system commands so logic is testable.
pub trait CommandRunner {
    /// Run a command, returning its stdout on success.
    fn run(&mut self, cmd: &Cmd) -> Result<String>;
}

/// Records commands instead of running them. Powers --dry-run and tests.
/// `fail_at` makes `run` return an error the Nth time (0-based) to test rollback.
#[derive(Default)]
pub struct RecordingRunner {
    pub commands: Vec<Cmd>,
    pub fail_at: Option<usize>,
    pub stdout: std::collections::HashMap<String, String>,
}

impl CommandRunner for RecordingRunner {
    fn run(&mut self, cmd: &Cmd) -> Result<String> {
        let index = self.commands.len();
        self.commands.push(cmd.clone());
        if Some(index) == self.fail_at {
            bail!("simulated failure running `{}`", cmd.display());
        }
        Ok(self.stdout.get(&cmd.display()).cloned().unwrap_or_default())
    }
}

/// Runs commands for real via std::process::Command.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&mut self, cmd: &Cmd) -> Result<String> {
        let output = Command::new(&cmd.program).args(&cmd.args).output()?;
        if !output.status.success() {
            bail!(
                "command `{}` failed: {}",
                cmd.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_runner_captures_commands() {
        let mut r = RecordingRunner::default();
        r.run(&Cmd::new("ifconfig", &["vlan0", "create"])).unwrap();
        assert_eq!(r.commands.len(), 1);
        assert_eq!(r.commands[0].display(), "ifconfig vlan0 create");
    }

    #[test]
    fn recording_runner_fails_at_index() {
        let mut r = RecordingRunner {
            fail_at: Some(1),
            ..Default::default()
        };
        assert!(r.run(&Cmd::new("a", &[])).is_ok());
        assert!(r.run(&Cmd::new("b", &[])).is_err());
    }

    #[test]
    fn best_effort_flag_is_set_correctly() {
        assert_eq!(Cmd::new("arp", &["-s", "x"]).best_effort, false);
        let be = Cmd::new_best_effort("arp", &["-d", "x"]);
        assert_eq!(be.best_effort, true);
        assert_eq!(be.display(), "arp -d x"); // flag does not affect rendering
    }
}
