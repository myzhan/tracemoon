//! Symbol resolution for C functions

use crate::symbols::cache::SymbolCache;
use log::debug;
use object::{Object, ObjectSymbol, SymbolKind};
use remoteprocess::Pid;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Parsed symbols from a single binary/library
struct BinarySymbols {
    /// Sorted by address for efficient lookup: (address, name)
    symbols: Vec<(u64, String)>,
}

impl BinarySymbols {
    fn from_file(path: &str) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        let file = object::File::parse(&*data).ok()?;
        
        let mut symbols: Vec<(u64, String)> = file
            .symbols()
            .chain(file.dynamic_symbols())
            .filter(|s| s.kind() == SymbolKind::Text)
            .filter_map(|s| {
                let name = s.name().ok()?;
                if name.is_empty() {
                    return None;
                }
                // Strip leading underscore on macOS
                let name = name.strip_prefix('_').unwrap_or(name);
                Some((s.address(), name.to_string()))
            })
            .collect();
        
        symbols.sort_by_key(|&(addr, _)| addr);
        symbols.dedup_by_key(|s| s.0);
        
        debug!("Loaded {} symbols from {}", symbols.len(), path);
        
        Some(Self { symbols })
    }
    
    /// Find the symbol containing the given offset
    fn lookup(&self, offset: u64) -> Option<&str> {
        // Binary search for the largest address <= offset
        match self.symbols.binary_search_by_key(&offset, |&(addr, _)| addr) {
            Ok(i) => Some(&self.symbols[i].1),
            Err(0) => None,
            Err(i) => Some(&self.symbols[i - 1].1),
        }
    }
}

/// Resolves symbols (function names) from addresses
pub struct SymbolResolver {
    pid: Pid,
    cache: Mutex<SymbolCache>,
    /// Binary symbol tables: path -> symbols
    binary_symbols: Mutex<BTreeMap<String, Option<BinarySymbols>>>,
}

impl SymbolResolver {
    /// Create a new symbol resolver
    pub fn new(pid: Pid) -> Self {
        Self {
            pid,
            cache: Mutex::new(SymbolCache::new(10000)),
            binary_symbols: Mutex::new(BTreeMap::new()),
        }
    }

    /// Resolve a C function address to its symbol name
    pub fn resolve_c_function(&self, addr: usize) -> Option<String> {
        // Check cache first
        {
            let cache = self.cache.lock().unwrap();
            if let Some(name) = cache.get(addr) {
                return Some(name.clone());
            }
        }
        
        // Resolve via memory maps + symbol table
        let name = self.lookup_symbol(addr);
        
        // Cache the result
        if let Some(ref name) = name {
            let mut cache = self.cache.lock().unwrap();
            cache.insert(addr, name.clone());
        }
        
        name
    }

    /// Look up symbol using proc-maps and binary symbol tables
    fn lookup_symbol(&self, addr: usize) -> Option<String> {
        let maps = proc_maps::get_process_maps(self.pid as proc_maps::Pid).ok()?;
        
        // Find the base address of the mapping containing this address
        // On macOS, we need the start of the first mapping for the binary
        let mut containing_map = None;
        let mut binary_path = None;
        
        for map in &maps {
            let start = map.start();
            let end = start + map.size();
            
            if addr >= start && addr < end {
                if let Some(path) = map.filename() {
                    binary_path = Some(path.to_string_lossy().to_string());
                    containing_map = Some(start);
                    break;
                }
            }
        }
        
        let path = binary_path?;
        let map_start = containing_map?;
        let offset = addr - map_start;
        
        // Extract filename for display
        let filename = path
            .rsplit('/')
            .next()
            .unwrap_or(&path)
            .to_string();
        
        // Try to resolve the actual function name from the symbol table
        let mut binary_cache = self.binary_symbols.lock().unwrap();
        let symbols = binary_cache
            .entry(path.clone())
            .or_insert_with(|| BinarySymbols::from_file(&path));
        
        if let Some(ref syms) = symbols {
            // On macOS, the offset in the file often matches virtual address directly
            // Try with the raw offset first, then with typical macOS base addresses
            if let Some(name) = syms.lookup(offset as u64) {
                return Some(name.to_string());
            }
            
            // Try with typical Mach-O text segment base (0x100000000 for main binary)
            let macho_addr = 0x100000000u64 + offset as u64;
            if let Some(name) = syms.lookup(macho_addr) {
                return Some(name.to_string());
            }
            
            // Try raw address
            if let Some(name) = syms.lookup(addr as u64) {
                return Some(name.to_string());
            }
        }
        
        // Fallback: return filename+offset
        Some(format!("{}+0x{:x}", filename, offset))
    }

    /// Resolve multiple addresses efficiently
    pub fn resolve_batch(&self, addrs: &[usize]) -> Vec<Option<String>> {
        addrs.iter().map(|&addr| self.resolve_c_function(addr)).collect()
    }

    /// Pre-populate the cache by loading symbols from proc-maps
    pub fn preload(&self) {
        if let Ok(maps) = proc_maps::get_process_maps(self.pid as proc_maps::Pid) {
            let mut binary_cache = self.binary_symbols.lock().unwrap();
            let mut seen = std::collections::HashSet::new();
            
            for map in &maps {
                if let Some(path) = map.filename() {
                    let path_str = path.to_string_lossy().to_string();
                    if seen.insert(path_str.clone()) {
                        binary_cache
                            .entry(path_str.clone())
                            .or_insert_with(|| BinarySymbols::from_file(&path_str));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolver_creation() {
        let resolver = SymbolResolver::new(1);
        // Just verify it creates without panic
        assert_eq!(resolver.pid, 1);
    }
}
