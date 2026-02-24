//! macOS-specific memory reader using Mach VM APIs

use crate::error::{Result, TracemoonError};
use crate::process::reader::ProcessMemoryReader;
use read_process_memory::{CopyAddress, ProcessHandle, Pid as RpmPid};
use remoteprocess::Pid;
use std::io;

/// macOS memory reader using read-process-memory crate
pub struct MacOSMemoryReader {
    pid: Pid,
    handle: ProcessHandle,
}

impl MacOSMemoryReader {
    /// Create a new macOS memory reader for the given process
    /// 
    /// Note: This requires elevated privileges (sudo) on macOS due to
    /// task_for_pid restrictions.
    pub fn new(pid: Pid) -> Result<Self> {
        let handle: ProcessHandle = (pid as RpmPid).try_into()
            .map_err(|e: std::io::Error| {
                let err_str = e.to_string();
                if err_str.contains("Permission denied") || 
                   err_str.contains("not permitted") ||
                   err_str.contains("KERN_FAILURE") {
                    TracemoonError::PermissionDenied { pid }
                } else if err_str.contains("No such process") ||
                          err_str.contains("ESRCH") {
                    TracemoonError::ProcessNotFound { pid }
                } else {
                    TracemoonError::Other(format!("Failed to attach to process {}: {}", pid, e))
                }
            })?;
        
        Ok(Self { pid, handle })
    }
}

impl ProcessMemoryReader for MacOSMemoryReader {
    fn read_memory(&self, addr: usize, len: usize) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; len];
        
        self.handle.copy_address(addr, &mut buffer)
            .map_err(|e| TracemoonError::MemoryRead {
                addr,
                source: io::Error::new(io::ErrorKind::Other, e.to_string()),
            })?;
        
        Ok(buffer)
    }
    
    fn pid(&self) -> i32 {
        self.pid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_self_read() {
        // Test reading from our own process
        let pid = std::process::id() as Pid;
        
        // This typically requires sudo on macOS
        match MacOSMemoryReader::new(pid) {
            Ok(reader) => {
                // Try to read a known value
                let value: u64 = 0xDEADBEEF;
                let addr = &value as *const u64 as usize;
                
                match reader.read_memory(addr, 8) {
                    Ok(bytes) => {
                        let read_value = u64::from_ne_bytes(bytes[..8].try_into().unwrap());
                        assert_eq!(read_value, value);
                    }
                    Err(e) => {
                        println!("Read failed (expected without sudo): {}", e);
                    }
                }
            }
            Err(e) => {
                println!("Could not create reader (expected without sudo): {}", e);
            }
        }
    }
}
