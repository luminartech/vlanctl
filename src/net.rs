use anyhow::{Result, bail};
use std::process::Command;

/// One external command to execute (program + args), e.g. `ifconfig vlan0 create`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
}

impl Cmd {
    pub fn new(program: &str, args: &[&str]) -> Cmd {
        Cmd {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
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
    /// Successive answers for a command whose output *changes* as earlier
    /// commands run, consumed in order and falling back to [`Self::stdout`]
    /// once exhausted.
    ///
    /// A single fixed answer cannot express the one thing a verification
    /// step exists to notice: that the host looked one way before the
    /// commands ran and another way after. Scripting `ifconfig -l` as
    /// "present" forever makes a successful teardown indistinguishable from
    /// one that silently did nothing.
    pub stdout_queue: std::collections::HashMap<String, std::collections::VecDeque<String>>,
}

impl CommandRunner for RecordingRunner {
    fn run(&mut self, cmd: &Cmd) -> Result<String> {
        let index = self.commands.len();
        self.commands.push(cmd.clone());
        if Some(index) == self.fail_at {
            bail!("simulated failure running `{}`", cmd.display());
        }
        if let Some(queued) = self
            .stdout_queue
            .get_mut(&cmd.display())
            .and_then(std::collections::VecDeque::pop_front)
        {
            return Ok(queued);
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
            // stderr is where a failure's reason belongs, but not every tool
            // puts it there: `netsh` explains a refused route on stdout and
            // leaves stderr empty, which would render as a bare "failed: ".
            // Fall back to stdout so the message carries something.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let reason = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            bail!("command `{}` failed: {reason}", cmd.display());
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
}
