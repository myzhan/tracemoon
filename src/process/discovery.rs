//! lua_State discovery in target process

use crate::error::{Result, TracemoonError};
use crate::lua::types::{LuaState, LUA_TTHREAD};
use crate::process::{ProcessMemoryReader, read_struct};
use log::{debug, info};
use remoteprocess::Pid;

/// Discovers lua_State address in a target process
pub struct LuaStateDiscovery<'a> {
    reader: &'a dyn ProcessMemoryReader,
    pid: Pid,
}

impl<'a> LuaStateDiscovery<'a> {
    /// Create a new discovery instance
    pub fn new(reader: &'a dyn ProcessMemoryReader, pid: Pid) -> Self {
        Self { reader, pid }
    }

    /// Find lua_State address using various strategies
    pub fn find(&self) -> Result<usize> {
        info!("Searching for lua_State in process {}", self.pid);
        
        // Strategy 1: Try symbol lookup via proc-maps
        if let Ok(addr) = self.find_by_proc_maps() {
            info!("Found lua_State via proc-maps at 0x{:x}", addr);
            return Ok(addr);
        }
        
        // Strategy 2: Scan memory for lua_State patterns
        if let Ok(addr) = self.find_by_scan() {
            info!("Found lua_State via memory scan at 0x{:x}", addr);
            return Ok(addr);
        }
        
        Err(TracemoonError::LuaStateNotFound)
    }

    /// Find lua_State via proc-maps memory regions
    fn find_by_proc_maps(&self) -> Result<usize> {
        debug!("Attempting proc-maps based lua_State discovery");
        
        // Use proc_maps crate to get memory regions
        let maps = proc_maps::get_process_maps(self.pid as proc_maps::Pid)
            .map_err(|e| TracemoonError::Other(format!("Failed to get memory maps: {}", e)))?;
        
        // Look for regions that might contain Lua
        for map in &maps {
            let filename = map.filename();
            if let Some(path) = filename {
                let path_str = path.to_string_lossy();
                // Look for lua library
                if path_str.contains("lua") || path_str.contains("Lua") {
                    debug!("Found Lua-related region: {} at 0x{:x}", path_str, map.start());
                }
            }
        }
        
        // For now, fall through to scanning
        Err(TracemoonError::LuaStateNotFound)
    }

    /// Scan memory regions for lua_State patterns
    fn find_by_scan(&self) -> Result<usize> {
        debug!("Attempting memory scan for lua_State");
        
        // Use proc_maps to get memory regions
        let maps = proc_maps::get_process_maps(self.pid as proc_maps::Pid)
            .map_err(|e| TracemoonError::Other(format!("Failed to get memory maps: {}", e)))?;
        
        // Scan heap and data regions
        for map in &maps {
            // Skip non-readable regions
            if !map.is_read() {
                continue;
            }
            
            // Focus on heap, data, and anonymous mappings
            let is_interesting = match map.filename() {
                Some(path) => {
                    let name = path.to_string_lossy();
                    name.contains("heap") || 
                    name.contains("[heap]") ||
                    name.is_empty() ||
                    name.contains("lua")
                }
                None => true,  // Anonymous mappings
            };
            
            if !is_interesting {
                continue;
            }
            
            let start = map.start();
            let end = start + map.size();
            
            // Don't scan huge regions
            if end - start > 100 * 1024 * 1024 {
                continue;
            }
            
            debug!("Scanning region 0x{:x} - 0x{:x}", start, end);
            
            // Scan for pointer-aligned addresses
            let step = std::mem::size_of::<usize>();
            let mut addr = start;
            
            while addr + std::mem::size_of::<LuaState>() <= end {
                if self.validate_lua_state(addr) {
                    // Additional validation: check some pointers
                    if let Ok(state) = read_struct::<LuaState>(self.reader, addr) {
                        // Verify pointers are within reasonable bounds
                        let ci = state.ci as usize;
                        let stack = state.stack as usize;
                        
                        if ci > 0x1000 && stack > 0x1000 && 
                           self.reader.is_readable(ci) &&
                           self.reader.is_readable(stack) {
                            return Ok(addr);
                        }
                    }
                }
                
                addr += step;
            }
        }
        
        Err(TracemoonError::LuaStateNotFound)
    }

    /// Validate that an address contains a valid lua_State
    fn validate_lua_state(&self, addr: usize) -> bool {
        if addr == 0 || addr < 0x1000 {
            return false;
        }
        
        let state: LuaState = match read_struct(self.reader, addr) {
            Ok(s) => s,
            Err(_) => return false,
        };
        
        // Check type tag
        if state.tt != LUA_TTHREAD {
            return false;
        }
        
        // Check status is valid
        if state.status > 5 {
            return false;
        }
        
        // Check pointers are non-null
        if state.ci == 0 || state.stack == 0 {
            return false;
        }
        
        // Check stack ordering
        if state.top < 0 || state.stack_last < state.top {
            return false;
        }
        
        // Check nci is reasonable (not too large)
        if state.nci > 10000 {
            return false;
        }
        
        true
    }
}

#[cfg(test)]
mod tests {
    // Tests would require a running Lua process
}
