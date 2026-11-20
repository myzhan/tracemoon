//! Whole-process stop for heap capture. The CPU unwinder's main-thread lock is insufficient.

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::{BTreeMap, HashSet};
    use std::time::{Duration, Instant};

    use nix::sys::{
        ptrace,
        signal::Signal,
        wait::{waitpid, WaitPidFlag, WaitStatus},
    };
    use nix::unistd::Pid;

    use crate::error::{Result, TracemoonError};

    pub struct FrozenProcess {
        // None means seized but not yet observed stopped; Some(None) is interrupt-stop.
        threads: BTreeMap<i32, Option<Option<Signal>>>,
        originally_stopped: HashSet<i32>,
    }

    fn tasks(pid: i32) -> Result<Vec<i32>> {
        let mut tids = Vec::new();
        for entry in std::fs::read_dir(format!("/proc/{pid}/task"))? {
            if let Ok(tid) = entry?.file_name().to_string_lossy().parse() {
                tids.push(tid);
            }
        }
        Ok(tids)
    }

    impl FrozenProcess {
        pub fn new(pid: i32, deadline: Instant) -> Result<Self> {
            if pid <= 0 || pid == std::process::id() as i32 {
                return Err(TracemoonError::Config("Cannot freeze this PID".into()));
            }
            let mut guard = Self {
                threads: BTreeMap::new(),
                originally_stopped: HashSet::new(),
            };
            loop {
                let tids = tasks(pid)?;
                if tids.is_empty() {
                    return Err(TracemoonError::ProcessExited);
                }
                // Once every observed thread is stopped, none can create another thread.
                // Re-enumeration closes the race with clones made before their parent stopped.
                if tids
                    .iter()
                    .all(|tid| guard.threads.get(tid).is_some_and(Option::is_some))
                {
                    return Ok(guard);
                }
                for tid in tids {
                    check_deadline(deadline)?;
                    if !guard.threads.contains_key(&tid) {
                        let status =
                            match std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/status"))
                            {
                                Ok(status) => status,
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                                Err(e) => return Err(e.into()),
                            };
                        if status.lines().any(|line| {
                            line.starts_with("State:")
                                && line.split_whitespace().nth(1) == Some("T")
                        }) {
                            guard.originally_stopped.insert(tid);
                        }
                        match ptrace::seize(Pid::from_raw(tid), ptrace::Options::empty()) {
                            Ok(()) => {}
                            Err(nix::errno::Errno::ESRCH) => continue,
                            Err(e) => {
                                return Err(TracemoonError::Other(format!(
                                    "seize thread {tid}: {e}"
                                )))
                            }
                        }
                        guard.threads.insert(tid, None);
                        match ptrace::interrupt(Pid::from_raw(tid)) {
                            Ok(()) => {}
                            Err(nix::errno::Errno::ESRCH) => {
                                guard.threads.remove(&tid);
                                continue;
                            }
                            Err(e) => {
                                return Err(TracemoonError::Other(format!(
                                    "interrupt thread {tid}: {e}"
                                )))
                            }
                        }
                    }
                    if guard.threads[&tid].is_some() {
                        continue;
                    }
                    loop {
                        check_deadline(deadline)?;
                        match guard.poll(tid)? {
                            true => break,
                            false => std::thread::sleep(Duration::from_millis(1)),
                        }
                    }
                }
            }
        }

        fn poll(&mut self, tid: i32) -> Result<bool> {
            match waitpid(
                Pid::from_raw(tid),
                Some(WaitPidFlag::__WALL | WaitPidFlag::WNOHANG),
            ) {
                Ok(WaitStatus::PtraceEvent(_, sig, event)) if event == libc::PTRACE_EVENT_STOP => {
                    // Re-deliver job-control stops; never send SIGCONT to the target.
                    let signal = if self.originally_stopped.contains(&tid) {
                        Some(Signal::SIGSTOP)
                    } else if sig == Signal::SIGTRAP {
                        None
                    } else {
                        Some(sig)
                    };
                    self.threads.insert(tid, Some(signal));
                    Ok(true)
                }
                Ok(WaitStatus::Stopped(_, sig)) => {
                    self.threads.insert(tid, Some(Some(sig)));
                    Ok(true)
                }
                Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) => {
                    self.threads.remove(&tid);
                    Ok(true)
                }
                Err(nix::errno::Errno::ESRCH | nix::errno::Errno::ECHILD) => {
                    self.threads.remove(&tid);
                    Ok(true)
                }
                Ok(_) => Ok(false),
                Err(e) => Err(TracemoonError::Other(format!("wait thread {tid}: {e}"))),
            }
        }

        pub fn resume(mut self) -> Result<()> {
            let tids: Vec<_> = self.threads.keys().copied().collect();
            for tid in tids {
                let signal = self.threads[&tid].flatten();
                match ptrace::detach(Pid::from_raw(tid), signal) {
                    Ok(()) | Err(nix::errno::Errno::ESRCH) => {
                        self.threads.remove(&tid);
                    }
                    Err(e) => {
                        return Err(TracemoonError::Other(format!("detach thread {tid}: {e}")))
                    }
                }
            }
            Ok(())
        }
    }

    impl Drop for FrozenProcess {
        fn drop(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(2);
            let tids: Vec<_> = self.threads.keys().copied().collect();
            for tid in tids {
                if self.threads.get(&tid) == Some(&None) {
                    let _ = ptrace::interrupt(Pid::from_raw(tid));
                    while Instant::now() < deadline && matches!(self.poll(tid), Ok(false)) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                let signal = self.threads.get(&tid).copied().flatten().flatten();
                if let Err(e) = ptrace::detach(Pid::from_raw(tid), signal) {
                    if e != nix::errno::Errno::ESRCH {
                        log::error!("Failed to detach thread {tid}: {e}");
                    }
                }
            }
        }
    }

    fn check_deadline(deadline: Instant) -> Result<()> {
        if Instant::now() >= deadline {
            return Err(TracemoonError::Other(
                "Heap capture pause budget exceeded".into(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::{Child, Command, Stdio};

        struct Worker(Child);
        impl Drop for Worker {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        #[test]
        fn worker() {
            let Ok(path) = std::env::var("TRACEMOON_FREEZE_WORKER") else {
                return;
            };
            for _ in 0..4 {
                std::thread::spawn(|| loop {
                    std::thread::sleep(Duration::from_millis(10));
                });
            }
            let mut count = 0u64;
            loop {
                std::fs::write(&path, count.to_string()).unwrap();
                count += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn spawn(path: &std::path::Path) -> Worker {
            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "process::freeze::linux::tests::worker",
                    "--nocapture",
                ])
                .env("TRACEMOON_FREEZE_WORKER", path)
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            let worker = Worker(child);
            let deadline = Instant::now() + Duration::from_secs(3);
            while !path.exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            worker
        }

        fn state(pid: i32) -> String {
            std::fs::read_to_string(format!("/proc/{pid}/status"))
                .unwrap()
                .lines()
                .find(|line| line.starts_with("State:"))
                .unwrap()
                .to_owned()
        }

        #[test]
        fn stops_every_thread_and_resumes_on_success_and_error_cleanup() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("heartbeat");
            let worker = spawn(&path);
            let pid = worker.0.id() as i32;
            for explicit_resume in [true, false] {
                let frozen =
                    FrozenProcess::new(pid, Instant::now() + Duration::from_secs(2)).unwrap();
                let tids = tasks(pid).unwrap();
                assert!(tids.len() >= 5);
                for tid in tids {
                    assert!(state(tid).contains("t (tracing stop)"));
                }
                let before = std::fs::read(&path).unwrap();
                std::thread::sleep(Duration::from_millis(30));
                assert_eq!(before, std::fs::read(&path).unwrap());
                if explicit_resume {
                    frozen.resume().unwrap();
                } else {
                    drop(frozen);
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                while std::fs::read(&path).unwrap() == before {
                    assert!(Instant::now() < deadline, "worker did not resume");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            assert!(FrozenProcess::new(pid, Instant::now()).is_err());
            assert!(!state(pid).contains("tracing stop"));
        }

        #[test]
        fn preserves_an_existing_job_control_stop() {
            let dir = tempfile::tempdir().unwrap();
            let worker = spawn(&dir.path().join("heartbeat"));
            let pid = worker.0.id() as i32;
            nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGSTOP).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !state(pid).contains("T (stopped)") {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            FrozenProcess::new(pid, deadline).unwrap().resume().unwrap();
            std::thread::sleep(Duration::from_millis(30));
            assert!(state(pid).contains("T (stopped)"));
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::FrozenProcess;

#[cfg(not(target_os = "linux"))]
pub struct FrozenProcess;

#[cfg(not(target_os = "linux"))]
impl FrozenProcess {
    pub fn new(_pid: i32, _deadline: std::time::Instant) -> crate::error::Result<Self> {
        Err(crate::error::TracemoonError::Config(
            "Heap capture currently supports Linux only; offline mem report/inspect are portable"
                .into(),
        ))
    }

    pub fn resume(self) -> crate::error::Result<()> {
        Ok(())
    }
}
