//! Process memory reader abstraction

use crate::error::{Result, TracemoonError};
use std::mem;

/// Trait for reading memory from a target process
/// 
/// Note: This trait is designed to be object-safe (dyn-compatible).
/// Generic methods are provided as standalone functions instead.
pub trait ProcessMemoryReader: Send + Sync {
    /// Read raw bytes from the target process
    fn read_memory(&self, addr: usize, len: usize) -> Result<Vec<u8>>;
    
    /// Get the process ID
    fn pid(&self) -> i32;
    
    /// Check if an address is readable
    fn is_readable(&self, addr: usize) -> bool {
        self.read_memory(addr, 1).is_ok()
    }
}

/// Read a struct from memory using a ProcessMemoryReader
pub fn read_struct<T: Sized + Copy>(reader: &dyn ProcessMemoryReader, addr: usize) -> Result<T> {
    let size = mem::size_of::<T>();
    let bytes = reader.read_memory(addr, size)?;
    
    if bytes.len() < size {
        return Err(TracemoonError::MemoryRead {
            addr,
            source: std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("Expected {} bytes, got {}", size, bytes.len()),
            ),
        });
    }
    
    // Safety: We've verified the size matches
    let ptr = bytes.as_ptr() as *const T;
    Ok(unsafe { *ptr })
}

/// Read a pointer (usize) from memory
pub fn read_pointer(reader: &dyn ProcessMemoryReader, addr: usize) -> Result<usize> {
    read_struct::<usize>(reader, addr)
}

/// Read a null-terminated string from memory (up to max_len bytes)
pub fn read_string(reader: &dyn ProcessMemoryReader, addr: usize, max_len: usize) -> Result<String> {
    let bytes = reader.read_memory(addr, max_len)?;
    
    // Find null terminator
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    
    String::from_utf8(bytes[..len].to_vec())
        .map_err(|e| TracemoonError::Other(format!("Invalid UTF-8: {}", e)))
}

/// Memory region information
#[derive(Debug, Clone)]
pub struct MemoryRegion {
    pub start: usize,
    pub end: usize,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
    pub path: Option<String>,
}

impl MemoryRegion {
    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.start && addr < self.end
    }
    
    pub fn size(&self) -> usize {
        self.end - self.start
    }
}
