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

use log::{debug, info, warn};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid as NixPid;
use remoteprocess::Pid;

use crate::error::{Result, TracemoonError};

/// A child process spawned by tracemoon.
///
/// On Linux the child is left stopped at its exec boundary after `spawn`; call
/// [`SpawnedChild::resume`] to let it run once the sampler is ready.
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

        let child = cmd.spawn().map_err(|e| {
            TracemoonError::Other(format!("Failed to spawn '{}': {}", argv[0], e))
        })?;
        let pid = child.id() as Pid;
        // Leak the std Child handle so the kernel keeps us as the parent; we reap the
        // child ourselves via waitpid so exit status stays available after detach.
        std::mem::forget(child);

        info!("Spawned child process {} ({})", pid, argv.join(" "));

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
                    return Err(TracemoonError::Other(format!(
                        "Child {} did not reach exec-stop: {:?}",
                        pid, status
                    )));
                }
                Err(e) => {
                    return Err(TracemoonError::Other(format!(
                        "waitpid failed for child {}: {}",
                        pid, e
                    )));
                }
            }
        }

        Ok(Self { pid })
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

        warn!("Child {} did not exit after SIGTERM; sending SIGKILL", self.pid);
        let _ = kill(nix_pid, Signal::SIGKILL);
        let _ = waitpid(nix_pid, None);
    }
}
