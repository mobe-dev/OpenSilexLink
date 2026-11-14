//! Bounded subprocess execution. Call from `spawn_blocking`, never a Tokio worker.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const OUTPUT_LIMIT: usize = 16 * 1024;

#[derive(Default)]
pub struct CommandRunner {
    // A killed child can remain in uninterruptible kernel sleep, or an escalated
    // child may reject our signal. Keep it and refuse overlapping commands until
    // try_wait confirms exit. Never block a worker (or shutdown) waiting to reap it.
    pending: Mutex<Option<Child>>,
}

fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: fd belongs to the live pipe; fcntl does not take ownership.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain(pipe: &mut impl Read, output: &mut Vec<u8>) -> io::Result<()> {
    let mut buf = [0; 4096];
    // Bound each pass as well as retained output: a noisy child must not stop
    // deadline checks. Nonblocking reads also handle descendants keeping pipes open.
    for _ in 0..16 {
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buf[..n.min(OUTPUT_LIMIT - output.len())]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn kill_group(child: &mut Child) {
    // SAFETY: each child starts a new process group with its own PID. Signal the
    // entire group to include wrapper children as well as the direct child.
    if unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) } != 0 {
        tracing::warn!(pid = child.id(), error = %io::Error::last_os_error(), "cannot kill command process group");
    }
    let _ = child.kill();
}

impl CommandRunner {
    pub fn ensure_idle(&self) -> Result<()> {
        Self::reap(&mut self.pending.lock().unwrap())
    }

    fn reap(pending: &mut Option<Child>) -> Result<()> {
        if let Some(child) = pending.as_mut() {
            if child.try_wait()?.is_none() {
                bail!("previous timed-out command (pid {}) is still running; refusing overlapping operations", child.id());
            }
            *pending = None;
        }
        Ok(())
    }

    pub fn run(&self, command: &mut Command, deadline: Duration) -> Result<Output> {
        let mut pending = self.pending.lock().unwrap();
        Self::reap(&mut pending)?;
        let label = format!("{command:?}");
        let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped())
            .stderr(Stdio::piped()).process_group(0).spawn()
            .with_context(|| format!("starting {label}"))?;
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let result = (|| -> Result<Output> {
            nonblocking(&stdout)?;
            nonblocking(&stderr)?;
            let start = Instant::now();
            let mut out = Vec::new();
            let mut err = Vec::new();
            loop {
                drain(&mut stdout, &mut out)?;
                drain(&mut stderr, &mut err)?;
                if let Some(status) = child.try_wait()? {
                    // Collect bytes written between the drain and process exit.
                    drain(&mut stdout, &mut out)?;
                    drain(&mut stderr, &mut err)?;
                    return Ok(Output { status, stdout: out, stderr: err });
                }
                if start.elapsed() >= deadline {
                    bail!("{label} timed out after {:.1}s (pid {}); killing process group; stderr: {}",
                        deadline.as_secs_f64(), child.id(), String::from_utf8_lossy(&err).trim());
                }
                std::thread::sleep(Duration::from_millis(10).min(deadline.saturating_sub(start.elapsed())));
            }
        })();
        if result.is_err() {
            kill_group(&mut child);
            // Do not call wait: SIGKILL cannot immediately release D-state USB I/O.
            // Reap ordinary killed children promptly, with a separate short bound.
            let reap_deadline = Instant::now() + Duration::from_millis(50);
            loop {
                if matches!(child.try_wait(), Ok(Some(_))) { return result; }
                if Instant::now() >= reap_deadline { break; }
                std::thread::sleep(Duration::from_millis(5));
            }
            *pending = Some(child);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_failure_status() {
        let runner = CommandRunner::default();
        let out = runner.run(Command::new("/bin/sh").args(["-c", "printf out; printf err >&2; exit 7"]), Duration::from_secs(2)).unwrap();
        assert_eq!(out.status.code(), Some(7));
        assert_eq!(out.stdout, b"out");
        assert_eq!(out.stderr, b"err");
    }

    #[test]
    fn timeout_kills_wrappers_and_descendants_and_allows_later_commands() {
        let runner = CommandRunner::default();
        let start = Instant::now();
        let error = runner.run(Command::new("/bin/sh").args(["-c", "sleep 30 & echo $! >&2; wait"]), Duration::from_millis(250)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
        let descendant: u32 = error.to_string().rsplit("stderr: ").next().unwrap().trim().parse().unwrap();
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{descendant}/stat"));
            // An orphaned zombie is already dead; its new parent owns reaping.
            let running = stat.ok().and_then(|s| s.rsplit_once(')').map(|(_, rest)| rest.trim_start().starts_with('Z')))
                == Some(false);
            if !running { break; }
            if start.elapsed() >= Duration::from_secs(2) {
                // SAFETY: this PID was just read from our test child's output.
                unsafe { libc::kill(descendant as i32, libc::SIGKILL); }
                panic!("command timeout left a running descendant");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Poll reaping, never wait indefinitely even if the regression returns.
        while runner.ensure_idle().is_err() {
            assert!(start.elapsed() < Duration::from_secs(2));
            std::thread::sleep(Duration::from_millis(10));
        }
        let out = runner.run(&mut Command::new("/bin/true"), Duration::from_secs(1)).unwrap();
        assert!(out.status.success());
    }

    #[test]
    fn refuses_new_commands_while_a_previous_child_is_still_running() {
        let runner = CommandRunner::default();
        let child = Command::new("/bin/sleep").arg("30").process_group(0).spawn().unwrap();
        *runner.pending.lock().unwrap() = Some(child);
        let error = runner.run(&mut Command::new("/bin/true"), Duration::from_millis(100)).unwrap_err();
        let mut child = runner.pending.lock().unwrap().take().unwrap();
        kill_group(&mut child);
        child.wait().unwrap();
        assert!(error.to_string().contains("refusing overlapping"));
    }

    #[test]
    fn noisy_output_cannot_fill_pipes_or_prevent_timeout_checks() {
        let runner = CommandRunner::default();
        let start = Instant::now();
        let error = runner.run(Command::new("/bin/sh").args(["-c", "while :; do printf 'noisy output\\n'; printf 'error\\n' >&2; done"]), Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(error.to_string().len() < OUTPUT_LIMIT + 1024);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
