//! Spawn mode: launch the target as a child process so no startup samples are missed.
//!
//! On Linux the child calls `PTRACE_TRACEME` before `execve`, so the kernel stops it
//! with `SIGTRAP` immediately after exec (before any user code runs). The parent catches
//! that exec-stop, builds the sampler while the child is frozen, then `ptrace::detach`es
//! to resume it — sampling starts at `t=0` of the program's own execution.
//!
//! On macOS there is no ptrace-hold; spawn is best-effort "spawn then sample immediately".

use std::process::Command;
use std::time::{Duration, Instant};

use log::{info, warn};
// Only the exec-stop / ptrace-detach logging is Linux-specific.
#[cfg(target_os = "linux")]
use log::debug;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid as NixPid;
use remoteprocess::Pid;

use crate::error::{Result, TracemoonError};

/// A child process spawned by tracemoon.
///
/// On Linux the child is left stopped at its exec boundary after `spawn`; call
/// [`SpawnedChild::resume`] to let it run once the sampler is ready.
///
/// The handle is a lifecycle owner: dropping it terminates and reaps a still-running child.
/// That covers every early-return path in the caller (a failed `Sampler::new`, a failing
/// signal-handler install, a `spawn` that never reaches its exec-stop) without each of them
/// having to remember to clean up — otherwise tracemoon leaves an orphaned target behind.
pub struct SpawnedChild {
    pid: Pid,
}

impl SpawnedChild {
    /// Spawn `argv` as a child. On Linux the child is stopped at the post-exec `SIGTRAP`
    /// and remains frozen until [`resume`](Self::resume) is called.
    pub fn spawn(argv: &[String]) -> Result<Self> {
        if argv.is_empty() {
            return Err(TracemoonError::Config(
                "No command given to spawn".to_string(),
            ));
        }

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    nix::sys::ptrace::traceme()
                        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                    Ok(())
                });
            }
        }

        let child = cmd
            .spawn()
            .map_err(|e| TracemoonError::Other(format!("Failed to spawn '{}': {}", argv[0], e)))?;
        let pid = child.id() as Pid;

        // `std::process::Child` has no Drop impl (dropping it neither waits nor kills), so
        // releasing the handle keeps us the parent without losing the ability to reap the
        // child ourselves via `waitpid` — which is what keeps the exit status available
        // after the ptrace detach.
        drop(child);

        info!("Spawned child process {} ({})", pid, argv.join(" "));

        // From here on the child is ours to clean up, including on the error paths below.
        let spawned = Self { pid };

        #[cfg(target_os = "linux")]
        {
            let nix_pid = NixPid::from_raw(pid);
            match waitpid(nix_pid, None) {
                Ok(WaitStatus::Stopped(_, Signal::SIGTRAP)) => {
                    debug!("Child {} stopped at exec (SIGTRAP)", pid);
                }
                Ok(WaitStatus::Stopped(_, sig)) => {
                    warn!("Child {} stopped with unexpected signal {:?}", pid, sig);
                }
                Ok(status) => {
                    spawned.terminate();
                    return Err(TracemoonError::Other(format!(
                        "Child {} did not reach exec-stop: {:?}",
                        pid, status
                    )));
                }
                Err(e) => {
                    spawned.terminate();
                    return Err(TracemoonError::Other(format!(
                        "waitpid failed for child {}: {}",
                        pid, e
                    )));
                }
            }
        }

        Ok(spawned)
    }

    /// PID of the spawned child.
    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Resume the child so it starts running. On Linux this detaches the tracer (which
    /// also resumes execution). No-op on macOS.
    pub fn resume(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            nix::sys::ptrace::detach(NixPid::from_raw(self.pid), None).map_err(|e| {
                TracemoonError::Other(format!("Failed to resume child {}: {}", self.pid, e))
            })?;
            debug!("Resumed (detached) child {}", self.pid);
        }
        Ok(())
    }

    /// Non-blocking check of whether the child has exited. Reaps it if so.
    pub fn has_exited(&self) -> bool {
        match waitpid(NixPid::from_raw(self.pid), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => true,
            Ok(_) => false,
            // ECHILD etc: no longer our child / already reaped ⇒ treat as gone.
            Err(_) => true,
        }
    }

    /// Terminate the child if it is still alive: SIGTERM, then SIGKILL if it lingers.
    ///
    /// Idempotent: a child that already exited is only reaped.
    pub fn terminate(&self) {
        if self.has_exited() {
            return;
        }

        let nix_pid = NixPid::from_raw(self.pid);
        info!("Terminating child {} (SIGTERM)", self.pid);
        let _ = kill(nix_pid, Signal::SIGTERM);

        // Give it a short grace period to exit cleanly.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if self.has_exited() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        warn!(
            "Child {} did not exit after SIGTERM; sending SIGKILL",
            self.pid
        );
        let _ = kill(nix_pid, Signal::SIGKILL);
        let _ = waitpid(nix_pid, None);
    }
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        // Never panic here: this runs on error paths, including during unwind.
        self.terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `SpawnedChild` for a pid that is already gone must be a no-op on drop.
    #[cfg(unix)]
    #[test]
    fn test_terminate_is_idempotent_for_dead_process() {
        let child = SpawnedChild { pid: i32::MAX };
        child.terminate();
        child.terminate();
    }
}
