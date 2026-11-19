//! Native (C) stack walking for process introspection.
//!
//! On Linux x86_64 the native stack is unwound via `remoteprocess`'s libunwind-ptrace
//! backend: [`NativeUnwinder`] holds a reusable libunwind address space, and each call to
//! [`NativeUnwinder::frames`] seizes+interrupts the target's main thread (via
//! `remoteprocess::Thread::lock`), walks the unwind cursor, then detaches. This works in
//! spawn mode without root — we are the child's parent, so ptrace seize is permitted even
//! under `kernel.yama.ptrace_scope=1`.
//!
//! On macOS the frame-pointer walker below (`NativeStackWalker` + `macos::get_native_frames`)
//! is used via `task_for_pid`. On unsupported platforms native stacks are empty.

use crate::error::Result;
use crate::process::ProcessMemoryReader;
use remoteprocess::Pid;

/// A native stack frame
#[derive(Debug, Clone)]
pub struct NativeFrame {
    /// Instruction pointer (return address)
    pub ip: usize,
}

/// Reusable native-stack unwinder for one target process.
///
/// Created once per sampler (single-VM mode only) and reused every tick so the libunwind
/// address space isn't rebuilt at the sample rate. `new` returns `None` on platforms where
/// native unwinding isn't supported, in which case the native merge is simply skipped.
pub struct NativeUnwinder {
    pid: Pid,
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    unwinder: remoteprocess::Unwinder,
}

impl NativeUnwinder {
    /// Build an unwinder for `pid`. `Some` only on supported platforms (Linux x86_64, or
    /// macOS x86_64/arm64); `None` elsewhere.
    pub fn new(pid: Pid) -> Option<Self> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let process = remoteprocess::Process::new(pid).ok()?;
            let unwinder = process.unwinder().ok()?;
            Some(Self { pid, unwinder })
        }

        #[cfg(all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            Some(Self { pid })
        }

        #[cfg(not(any(
            all(target_os = "linux", target_arch = "x86_64"),
            all(
                target_os = "macos",
                any(target_arch = "x86_64", target_arch = "aarch64")
            )
        )))]
        {
            let _ = pid;
            None
        }
    }

    /// Capture the target's native call stack (innermost frame first). Best-effort: any
    /// failure yields an empty vec — the Lua stack is always the primary signal.
    pub fn frames(&self, reader: &dyn ProcessMemoryReader) -> Vec<NativeFrame> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let _ = reader;
            self.linux_frames()
        }

        #[cfg(all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            macos::get_native_frames(nix::unistd::Pid::from_raw(self.pid), reader)
                .unwrap_or_default()
        }

        #[cfg(not(any(
            all(target_os = "linux", target_arch = "x86_64"),
            all(
                target_os = "macos",
                any(target_arch = "x86_64", target_arch = "aarch64")
            )
        )))]
        {
            let _ = reader;
            Vec::new()
        }
    }

    /// Lock the main thread and walk the libunwind cursor.
    ///
    /// Note: `Thread::lock` and `SpawnedChild::has_exited` both `waitpid` the same child, but
    /// they run sequentially within a sample tick (never concurrently), and failures here are
    /// non-fatal — do not "fix" flakiness by dropping the lock; the lock is required for
    /// libunwind-ptrace to read the target's registers/memory.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn linux_frames(&self) -> Vec<NativeFrame> {
        use log::debug;

        // Main thread: tid == pid. The single-VM interpreter runs here.
        let thread = match remoteprocess::Thread::new(self.pid) {
            Ok(t) => t,
            Err(e) => {
                debug!("native: Thread::new failed: {}", e);
                return Vec::new();
            }
        };

        let _lock = match thread.lock() {
            Ok(l) => l,
            Err(e) => {
                debug!("native: thread lock failed: {}", e);
                return Vec::new();
            }
        };

        let cursor = match self.unwinder.cursor(&thread) {
            Ok(c) => c,
            Err(e) => {
                debug!("native: cursor failed: {}", e);
                return Vec::new();
            }
        };

        let mut frames = Vec::new();
        for ip in cursor {
            match ip {
                Ok(ip) => frames.push(NativeFrame { ip: ip as usize }),
                Err(_) => break,
            }
            if frames.len() >= 128 {
                break;
            }
        }
        frames
    }
}

/// Native stack walker - captures C-level call stacks (frame-pointer chain, used on macOS)
pub struct NativeStackWalker<'a> {
    reader: &'a dyn ProcessMemoryReader,
}

impl<'a> NativeStackWalker<'a> {
    pub fn new(reader: &'a dyn ProcessMemoryReader) -> Self {
        Self { reader }
    }

    /// Walk the native stack starting from given frame pointer and instruction pointer
    /// Returns frames from current (top) to oldest (bottom)
    pub fn walk(&self, initial_fp: usize, initial_ip: usize) -> Result<Vec<NativeFrame>> {
        let mut frames = Vec::new();
        let mut fp = initial_fp;

        // Add the initial frame
        if initial_ip != 0 {
            frames.push(NativeFrame { ip: initial_ip });
        }

        // Walk frame pointer chain (x86_64 calling convention)
        // Each frame: [saved_rbp, return_address]
        let max_depth = 128;
        let mut visited = std::collections::HashSet::new();

        while fp != 0 && frames.len() < max_depth {
            // Detect loops
            if !visited.insert(fp) {
                break;
            }

            // Validate frame pointer looks reasonable
            if !(0x1000..=0x7fff_ffff_ffff).contains(&fp) {
                break;
            }

            // Read saved frame pointer and return address
            // Frame layout: [saved_rbp (8 bytes), return_address (8 bytes)]
            let frame_data = match self.reader.read_memory(fp, 16) {
                Ok(data) => data,
                Err(_) => break,
            };

            let saved_fp = u64::from_ne_bytes([
                frame_data[0],
                frame_data[1],
                frame_data[2],
                frame_data[3],
                frame_data[4],
                frame_data[5],
                frame_data[6],
                frame_data[7],
            ]) as usize;

            let return_addr = u64::from_ne_bytes([
                frame_data[8],
                frame_data[9],
                frame_data[10],
                frame_data[11],
                frame_data[12],
                frame_data[13],
                frame_data[14],
                frame_data[15],
            ]) as usize;

            // Validate return address looks like code
            if return_addr == 0 || return_addr < 0x1000 {
                break;
            }

            frames.push(NativeFrame { ip: return_addr });

            // Move to next frame
            if saved_fp <= fp {
                // Stack grows down, so saved_fp should be greater
                break;
            }
            fp = saved_fp;
        }

        Ok(frames)
    }
}

#[cfg(target_os = "macos")]
pub mod macos {
    use super::*;
    use nix::unistd::Pid;
    use std::mem;

    // Mach types
    type MachPort = u32;
    type KernReturn = i32;
    type ThreadState = u32;

    const KERN_SUCCESS: KernReturn = 0;

    // x86_64 thread state flavor (only meaningful when built for x86_64)
    #[cfg(target_arch = "x86_64")]
    const X86_THREAD_STATE64: ThreadState = 4;
    #[cfg(target_arch = "x86_64")]
    const X86_THREAD_STATE64_COUNT: u32 = 42;

    // ARM64 thread state flavor
    const ARM_THREAD_STATE64: ThreadState = 6;
    const ARM_THREAD_STATE64_COUNT: u32 = 68;

    #[cfg(target_arch = "x86_64")]
    #[repr(C)]
    #[derive(Debug, Default)]
    struct X86ThreadState64 {
        rax: u64,
        rbx: u64,
        rcx: u64,
        rdx: u64,
        rdi: u64,
        rsi: u64,
        rbp: u64,
        rsp: u64,
        r8: u64,
        r9: u64,
        r10: u64,
        r11: u64,
        r12: u64,
        r13: u64,
        r14: u64,
        r15: u64,
        rip: u64,
        rflags: u64,
        cs: u64,
        fs: u64,
        gs: u64,
    }

    #[repr(C)]
    #[derive(Debug, Default)]
    struct Arm64ThreadState {
        x: [u64; 29], // x0-x28
        fp: u64,      // x29 (frame pointer)
        lr: u64,      // x30 (link register)
        sp: u64,      // stack pointer
        pc: u64,      // program counter
        cpsr: u32,    // current program status register
        _pad: u32,
    }

    extern "C" {
        fn task_for_pid(target: MachPort, pid: i32, task: *mut MachPort) -> KernReturn;
        fn task_threads(task: MachPort, threads: *mut *mut MachPort, count: *mut u32)
            -> KernReturn;
        fn thread_get_state(
            thread: MachPort,
            flavor: ThreadState,
            state: *mut u8,
            count: *mut u32,
        ) -> KernReturn;
        fn mach_task_self() -> MachPort;
        fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
        fn vm_deallocate(task: MachPort, addr: usize, size: usize) -> KernReturn;
        fn thread_suspend(thread: MachPort) -> KernReturn;
        fn thread_resume(thread: MachPort) -> KernReturn;
    }

    /// Owns a send-right to the target task, releasing it on drop.
    ///
    /// `task_for_pid` hands out a user reference on *every* call, and this runs once per
    /// sample tick — not releasing it leaks Mach port references at the sampling rate.
    struct TaskPort(MachPort);

    impl Drop for TaskPort {
        fn drop(&mut self) {
            unsafe {
                mach_port_deallocate(mach_task_self(), self.0);
            }
        }
    }

    /// Owns the thread list returned by `task_threads`.
    ///
    /// That call transfers one send-right per returned thread port plus the array itself, so
    /// both the ports and the array must be given back — deallocating only the array (as this
    /// code used to) leaks a port reference per thread per tick.
    struct ThreadPorts {
        task: MachPort,
        ptr: *mut MachPort,
        count: u32,
    }

    impl Drop for ThreadPorts {
        fn drop(&mut self) {
            unsafe {
                for i in 0..self.count as usize {
                    mach_port_deallocate(self.task, *self.ptr.add(i));
                }
                if !self.ptr.is_null() {
                    vm_deallocate(
                        mach_task_self(),
                        self.ptr as usize,
                        self.count as usize * mem::size_of::<MachPort>(),
                    );
                }
            }
        }
    }

    /// Get native stack frames for a process on macOS
    pub fn get_native_frames(
        pid: Pid,
        reader: &dyn ProcessMemoryReader,
    ) -> Result<Vec<NativeFrame>> {
        unsafe {
            // Get task port for the target process
            let mut task: MachPort = 0;
            let kr = task_for_pid(mach_task_self(), pid.as_raw(), &mut task);
            if kr != KERN_SUCCESS {
                return Ok(Vec::new()); // Silently fail, native stack is optional
            }
            let task = TaskPort(task);

            // Get threads
            let mut threads: *mut MachPort = std::ptr::null_mut();
            let mut thread_count: u32 = 0;
            let kr = task_threads(task.0, &mut threads, &mut thread_count);
            if kr != KERN_SUCCESS || thread_count == 0 {
                // `task_threads` may still have handed us an array on failure.
                if !threads.is_null() {
                    drop(ThreadPorts {
                        task: task.0,
                        ptr: threads,
                        count: thread_count,
                    });
                }
                return Ok(Vec::new());
            }
            let thread_ports = ThreadPorts {
                task: task.0,
                ptr: threads,
                count: thread_count,
            };

            let mut all_frames = Vec::new();

            // Get state of first thread (main thread)
            let thread = *thread_ports.ptr;

            // Suspend thread for consistent state
            thread_suspend(thread);

            // Try x86_64 first, then ARM64
            #[cfg(target_arch = "x86_64")]
            {
                let mut state = X86ThreadState64::default();
                let mut count = X86_THREAD_STATE64_COUNT;
                let kr = thread_get_state(
                    thread,
                    X86_THREAD_STATE64,
                    &mut state as *mut _ as *mut u8,
                    &mut count,
                );

                if kr == KERN_SUCCESS {
                    let walker = NativeStackWalker::new(reader);
                    if let Ok(frames) = walker.walk(state.rbp as usize, state.rip as usize) {
                        all_frames = frames;
                    }
                }
            }

            #[cfg(target_arch = "aarch64")]
            {
                let mut state = Arm64ThreadState::default();
                let mut count = ARM_THREAD_STATE64_COUNT;
                let kr = thread_get_state(
                    thread,
                    ARM_THREAD_STATE64,
                    &mut state as *mut _ as *mut u8,
                    &mut count,
                );

                if kr == KERN_SUCCESS {
                    let walker = NativeStackWalker::new(reader);
                    if let Ok(frames) = walker.walk(state.fp as usize, state.pc as usize) {
                        all_frames = frames;
                    }
                }
            }

            // Resume thread
            thread_resume(thread);

            // Dropping `thread_ports`/`task` releases the Mach port references.
            Ok(all_frames)
        }
    }
}
