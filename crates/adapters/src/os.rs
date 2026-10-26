// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The OS bridge: the single, quarantined gateway through which the
//! system runs external programs.
//!
//! Everything the brain does to the outside world — reach the router over `ssh`,
//! publish a notification with `curl` — is expressed here as an argv plus an
//! optional environment, optional stdin, and a **hard timeout**. Because the
//! timeout is mandatory and enforced by killing the child, no call can ever block
//! forever (bounded execution). Raw bytes returned here are untrusted
//! until a parser validates them.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Result of running an external command. The exit code is optional because a
/// killed child has no code. Byte buffers are raw and untrusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    /// True only on a clean zero exit.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Lossily decode stdout as text for parsing at the boundary.
    #[must_use]
    pub fn stdout_text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }
}

/// Why an external command could not be run to completion. Errors are values
///: no path here unwinds the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunError {
    /// The empty argv was passed — a programming error, caught defensively.
    EmptyArgv,
    /// The program could not be spawned (missing binary, permission denied).
    Spawn,
    /// The command exceeded its deadline and was killed.
    Timeout,
    /// An I/O error occurred while talking to the child.
    Io,
    /// The remote host's SSH key was unknown or did not match the pinned /
    /// known key, so the connection was refused before any credential was sent.
    HostKeyRejected,
}

/// The contract every command execution goes through. Kept as a trait so the
/// real OS can be swapped for a fake in tests (injected I/O).
pub trait CommandRunner {
    /// Run `argv` (with `env` overrides and optional `stdin`) to completion or
    /// until `timeout` elapses, whichever comes first.
    fn run(
        &self,
        argv: &[String],
        env: &[(String, String)],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<CommandOutput, RunError>;
}

/// Runs commands through the real operating system.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

/// How often to check whether the child has exited while enforcing the deadline.
/// Small enough to be responsive, large enough not to spin the CPU.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        argv: &[String],
        env: &[(String, String)],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<CommandOutput, RunError> {
        let (program, args) = argv.split_first().ok_or(RunError::EmptyArgv)?;

        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }

        let mut child = command.spawn().map_err(|_| RunError::Spawn)?;

        // Feed stdin (small payloads only) and close it so the child can proceed.
        match (stdin, child.stdin.take()) {
            (Some(bytes), Some(mut sink)) => sink.write_all(bytes).map_err(|_| RunError::Io)?,
            _ => { /* nothing to write; the take() above closed the pipe */ }
        }

        // Drain stdout/stderr on separate threads so a full pipe buffer can never
        // deadlock the child while the main thread enforces the deadline. This
        // extra code buys guaranteed non-blocking (correctness over brevity).
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let stdout_reader = std::thread::spawn(move || drain(stdout_pipe));
        let stderr_reader = std::thread::spawn(move || drain(stderr_pipe));

        let started = Instant::now();
        let outcome = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status.code()),
                Ok(None) => {
                    if started.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(RunError::Timeout);
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(RunError::Io);
                }
            }
        };

        // The pipes close when the child exits or is killed, so both readers
        // finish; a panicked reader degrades to empty output rather than a crash.
        let stdout = stdout_reader.join().unwrap_or_else(|_| Vec::new());
        let stderr = stderr_reader.join().unwrap_or_else(|_| Vec::new());

        outcome.map(|code| CommandOutput {
            code,
            stdout,
            stderr,
        })
    }
}

/// Read a pipe to EOF, returning whatever bytes arrived. An I/O error yields the
/// bytes read so far rather than failing the whole command.
fn drain(pipe: Option<impl Read>) -> Vec<u8> {
    let mut buffer = Vec::new();
    if let Some(mut source) = pipe {
        let _ = source.read_to_end(&mut buffer);
    }
    buffer
}

#[cfg(test)]
mod tests {
    use super::{CommandRunner, RunError, SystemRunner};
    use std::time::Duration;

    fn argv<const N: usize>(parts: [&str; N]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn runs_a_command_and_captures_stdout() -> Result<(), RunError> {
        let out = SystemRunner.run(
            &argv(["sh", "-c", "printf hello"]),
            &[],
            None,
            Duration::from_secs(5),
        )?;
        assert!(out.success());
        assert_eq!(out.stdout_text(), "hello");
        Ok(())
    }

    #[test]
    fn feeds_stdin() -> Result<(), RunError> {
        let out = SystemRunner.run(
            &argv(["cat"]),
            &[],
            Some(b"echo me"),
            Duration::from_secs(5),
        )?;
        assert_eq!(out.stdout_text(), "echo me");
        Ok(())
    }

    #[test]
    fn passes_environment() -> Result<(), RunError> {
        let env = [("SOVEREIGN_TEST".to_string(), "42".to_string())];
        let out = SystemRunner.run(
            &argv(["sh", "-c", "printf %s \"$SOVEREIGN_TEST\""]),
            &env,
            None,
            Duration::from_secs(5),
        )?;
        assert_eq!(out.stdout_text(), "42");
        Ok(())
    }

    #[test]
    fn kills_a_command_that_overruns_its_deadline() {
        let result = SystemRunner.run(
            &argv(["sh", "-c", "sleep 10"]),
            &[],
            None,
            Duration::from_millis(150),
        );
        assert_eq!(result, Err(RunError::Timeout));
    }

    #[test]
    fn reports_a_missing_binary() {
        let result = SystemRunner.run(
            &argv(["definitely-not-a-real-binary-xyz"]),
            &[],
            None,
            Duration::from_secs(2),
        );
        assert_eq!(result, Err(RunError::Spawn));
    }

    #[test]
    fn rejects_empty_argv() {
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            SystemRunner.run(&empty, &[], None, Duration::from_secs(1)),
            Err(RunError::EmptyArgv)
        );
    }
}
