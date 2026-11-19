//! lua_State discovery and Lua version detection in a target process.

use std::collections::HashSet;

use log::debug;
use remoteprocess::Pid;

use crate::error::{Result, TracemoonError};
use crate::lua::{LuaLayout, LuaVersion};
use crate::process::ProcessMemoryReader;

/// Discovers lua_State address(es) in a target process by memory scanning.
pub struct LuaStateDiscovery<'a> {
    reader: &'a dyn ProcessMemoryReader,
    layout: &'a LuaLayout,
    pid: Pid,
}

impl<'a> LuaStateDiscovery<'a> {
    /// Create a new discovery instance for a given layout.
    pub fn new(reader: &'a dyn ProcessMemoryReader, layout: &'a LuaLayout, pid: Pid) -> Self {
        Self {
            reader,
            layout,
            pid,
        }
    }

    /// Find every plausible lua_State address by scanning writable regions.
    pub fn find_all(&self) -> Result<Vec<usize>> {
        let maps = proc_maps::get_process_maps(self.pid as proc_maps::Pid)
            .map_err(|e| TracemoonError::Other(format!("Failed to get memory maps: {}", e)))?;

        let mut found = Vec::new();
        let mut seen = HashSet::new();
        let step = std::mem::size_of::<usize>();

        for map in &maps {
            if !map.is_read() {
                continue;
            }

            // Focus on heap / anonymous / lua-backed regions; skip huge maps.
            let is_interesting = match map.filename() {
                Some(path) => {
                    let name = path.to_string_lossy();
                    name.contains("heap") || name.is_empty() || name.contains("lua")
                }
                None => true,
            };
            if !is_interesting {
                continue;
            }

            let start = map.start();
            let end = start + map.size();
            if end - start > 512 * 1024 * 1024 {
                continue;
            }

            debug!("Scanning region 0x{:x} - 0x{:x}", start, end);

            let mut addr = start;
            while addr + self.layout.ls_size <= end {
                if self.layout.is_valid_state(self.reader, addr) && seen.insert(addr) {
                    found.push(addr);
                }
                addr += step;
            }
        }

        debug!("Scan found {} candidate lua_State(s)", found.len());
        Ok(found)
    }
}

/// Detect the target's Lua version by scanning its mapped executable files on
/// disk for the `Lua 5.N` release string embedded in rodata.
pub fn detect_lua_version(pid: Pid) -> Option<LuaVersion> {
    let maps = proc_maps::get_process_maps(pid as proc_maps::Pid).ok()?;

    let mut seen = HashSet::new();
    for map in &maps {
        let path = match map.filename() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        if !seen.insert(path.clone()) {
            continue;
        }

        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };

        // Later versions are more specific; check 5.5 before 5.4.
        if find_bytes(&data, b"Lua 5.5").is_some() {
            debug!("Detected Lua 5.5 from {}", path.display());
            return Some(LuaVersion::Lua55);
        }
        if find_bytes(&data, b"Lua 5.4").is_some() {
            debug!("Detected Lua 5.4 from {}", path.display());
            return Some(LuaVersion::Lua54);
        }
    }

    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_bytes() {
        assert_eq!(find_bytes(b"hello Lua 5.4 world", b"Lua 5.4"), Some(6));
        assert_eq!(find_bytes(b"nothing here", b"Lua 5.5"), None);
    }
}
