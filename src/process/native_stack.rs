//! Native (C) stack walking for process introspection

use crate::error::Result;
use crate::process::ProcessMemoryReader;

/// A native stack frame
#[derive(Debug, Clone)]
pub struct NativeFrame {
    /// Instruction pointer (return address)
    pub ip: usize,
    /// Frame pointer (for debugging)
    pub fp: usize,
}

/// Native stack walker - captures C-level call stacks
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
            frames.push(NativeFrame { ip: initial_ip, fp: initial_fp });
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
            if fp < 0x1000 || fp > 0x7fffffffffff {
                break;
            }
            
            // Read saved frame pointer and return address
            // Frame layout: [saved_rbp (8 bytes), return_address (8 bytes)]
            let frame_data = match self.reader.read_memory(fp, 16) {
                Ok(data) => data,
                Err(_) => break,
            };
            
            let saved_fp = u64::from_ne_bytes([
                frame_data[0], frame_data[1], frame_data[2], frame_data[3],
                frame_data[4], frame_data[5], frame_data[6], frame_data[7],
            ]) as usize;
            
            let return_addr = u64::from_ne_bytes([
                frame_data[8], frame_data[9], frame_data[10], frame_data[11],
                frame_data[12], frame_data[13], frame_data[14], frame_data[15],
            ]) as usize;
            
            // Validate return address looks like code
            if return_addr == 0 || return_addr < 0x1000 {
                break;
            }
            
            frames.push(NativeFrame {
                ip: return_addr,
                fp: saved_fp,
            });
            
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
    const THREAD_STATE_NONE: ThreadState = 0;
    
    // x86_64 thread state flavor
    const X86_THREAD_STATE64: ThreadState = 4;
    const X86_THREAD_STATE64_COUNT: u32 = 42;
    
    // ARM64 thread state flavor  
    const ARM_THREAD_STATE64: ThreadState = 6;
    const ARM_THREAD_STATE64_COUNT: u32 = 68;

    #[repr(C)]
    #[derive(Debug, Default)]
    struct X86ThreadState64 {
        rax: u64, rbx: u64, rcx: u64, rdx: u64,
        rdi: u64, rsi: u64, rbp: u64, rsp: u64,
        r8: u64, r9: u64, r10: u64, r11: u64,
        r12: u64, r13: u64, r14: u64, r15: u64,
        rip: u64, rflags: u64,
        cs: u64, fs: u64, gs: u64,
    }

    #[repr(C)]
    #[derive(Debug, Default)]
    struct Arm64ThreadState {
        x: [u64; 29],  // x0-x28
        fp: u64,       // x29 (frame pointer)
        lr: u64,       // x30 (link register)
        sp: u64,       // stack pointer
        pc: u64,       // program counter
        cpsr: u32,     // current program status register
        _pad: u32,
    }

    extern "C" {
        fn task_for_pid(target: MachPort, pid: i32, task: *mut MachPort) -> KernReturn;
        fn task_threads(task: MachPort, threads: *mut *mut MachPort, count: *mut u32) -> KernReturn;
        fn thread_get_state(
            thread: MachPort,
            flavor: ThreadState,
            state: *mut u8,
            count: *mut u32,
        ) -> KernReturn;
        fn mach_task_self() -> MachPort;
        fn vm_deallocate(task: MachPort, addr: usize, size: usize) -> KernReturn;
        fn thread_suspend(thread: MachPort) -> KernReturn;
        fn thread_resume(thread: MachPort) -> KernReturn;
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

            // Get threads
            let mut threads: *mut MachPort = std::ptr::null_mut();
            let mut thread_count: u32 = 0;
            let kr = task_threads(task, &mut threads, &mut thread_count);
            if kr != KERN_SUCCESS || thread_count == 0 {
                return Ok(Vec::new());
            }

            let mut all_frames = Vec::new();

            // Get state of first thread (main thread)
            if thread_count > 0 {
                let thread = *threads;
                
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
            }

            // Clean up
            let threads_size = thread_count as usize * mem::size_of::<MachPort>();
            vm_deallocate(mach_task_self(), threads as usize, threads_size);

            Ok(all_frames)
        }
    }
}

#[cfg(target_os = "linux")]
pub mod linux {
    use super::*;
    use nix::unistd::Pid;
    use nix::sys::ptrace;
    use nix::sys::wait::waitpid;

    /// Get native stack frames for a process on Linux
    pub fn get_native_frames(
        pid: Pid,
        reader: &dyn ProcessMemoryReader,
    ) -> Result<Vec<NativeFrame>> {
        // On Linux, we need ptrace to read registers
        // The process should already be attached via remoteprocess
        
        #[cfg(target_arch = "x86_64")]
        {
            // Try to read registers using ptrace
            match ptrace::getregs(pid) {
                Ok(regs) => {
                    let walker = NativeStackWalker::new(reader);
                    walker.walk(regs.rbp as usize, regs.rip as usize)
                }
                Err(_) => Ok(Vec::new()),
            }
        }
        
        #[cfg(target_arch = "aarch64")]
        {
            // ARM64 register access is different
            // For now, return empty
            Ok(Vec::new())
        }
        
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            Ok(Vec::new())
        }
    }
}
