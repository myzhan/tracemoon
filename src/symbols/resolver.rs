//! Symbol resolution for C functions

use crate::symbols::cache::SymbolCache;
use log::debug;
use object::{Object, ObjectSegment, ObjectSymbol, SymbolKind};
use remoteprocess::Pid;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Resolve a named symbol to its runtime address in a target process.
///
/// Scans each mapped binary for `symbol_name`, then applies the PIE load bias
/// (mapping base minus the file's lowest loadable vaddr) to translate the
/// linked vaddr into a runtime address.
pub fn resolve_symbol_address(pid: Pid, symbol_name: &str) -> Option<usize> {
    let maps = proc_maps::get_process_maps(pid as proc_maps::Pid).ok()?;

    // Lowest mapped start address per backing file (the load base).
    let mut file_base: BTreeMap<String, usize> = BTreeMap::new();
    for map in &maps {
        if let Some(path) = map.filename() {
            let key = path.to_string_lossy().to_string();
            let start = map.start();
            file_base
                .entry(key)
                .and_modify(|b| {
                    if start < *b {
                        *b = start;
                    }
                })
                .or_insert(start);
        }
    }

    for (path, &base) in &file_base {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let file = match object::File::parse(&*data) {
            Ok(f) => f,
            Err(_) => continue,
        };

        let sym_vaddr = file
            .symbols()
            .chain(file.dynamic_symbols())
            .find(|s| s.name().map(|n| n == symbol_name).unwrap_or(false))
            .map(|s| s.address());

        let sym_vaddr = match sym_vaddr {
            Some(v) if v != 0 => v,
            _ => continue,
        };

        // Lowest loadable segment vaddr = the file's link base (0 for PIE).
        let min_vaddr = file
            .segments()
            .map(|s| s.address())
            .min()
            .unwrap_or(0);

        let bias = base as u64 - min_vaddr;
        let runtime = (sym_vaddr + bias) as usize;
        debug!(
            "Resolved symbol {} @ vaddr 0x{:x} -> runtime 0x{:x} (base 0x{:x}, min_vaddr 0x{:x}) in {}",
            symbol_name, sym_vaddr, runtime, base, min_vaddr, path
        );
        return Some(runtime);
    }

    None
}

/// Parsed symbols from a single binary/library
struct BinarySymbols {
    /// Sorted by address for efficient lookup: (address, name)
    symbols: Vec<(u64, String)>,
    /// Lowest loadable segment vaddr (the file's link base; 0 for PIE).
    min_vaddr: u64,
}

impl BinarySymbols {
    fn from_file(path: &str) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        let file = object::File::parse(&*data).ok()?;

        let mut symbols: Vec<(u64, String)> = file
            .symbols()
            .chain(file.dynamic_symbols())
            // Only defined text symbols. Undefined imports (e.g. `getenv@GLIBC_2.2.5`)
            // have address 0 and would otherwise catch any too-small offset lookup.
            .filter(|s| s.kind() == SymbolKind::Text && !s.is_undefined())
            .filter_map(|s| {
                let addr = s.address();
                if addr == 0 {
                    return None;
                }
                let name = s.name().ok()?;
                if name.is_empty() {
                    return None;
                }
                // Strip leading underscore on macOS
                let name = name.strip_prefix('_').unwrap_or(name);
                Some((addr, name.to_string()))
            })
            .collect();

        symbols.sort_by_key(|&(addr, _)| addr);
        symbols.dedup_by_key(|s| s.0);

        let min_vaddr = file.segments().map(|s| s.address()).min().unwrap_or(0);

        debug!("Loaded {} symbols from {}", symbols.len(), path);

        Some(Self { symbols, min_vaddr })
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

        // Identify the backing file for this address, and that file's load base
        // (the LOWEST mapping start for the file — a file spans several segment
        // mappings, and only the first corresponds to the ELF link base).
        let mut binary_path = None;
        for map in &maps {
            let start = map.start();
            let end = start + map.size();
            if addr >= start && addr < end {
                if let Some(path) = map.filename() {
                    binary_path = Some(path.to_string_lossy().to_string());
                    break;
                }
            }
        }
        let path = binary_path?;

        let load_base = maps
            .iter()
            .filter(|m| {
                m.filename()
                    .map(|p| p.to_string_lossy() == path.as_str())
                    .unwrap_or(false)
            })
            .map(|m| m.start())
            .min()?;

        // Extract filename for display
        let filename = path.rsplit('/').next().unwrap_or(&path).to_string();

        // Try to resolve the actual function name from the symbol table
        let mut binary_cache = self.binary_symbols.lock().unwrap();
        let symbols = binary_cache
            .entry(path.clone())
            .or_insert_with(|| BinarySymbols::from_file(&path));

        if let Some(ref syms) = symbols {
            // Translate the runtime address back to a file vaddr:
            //   vaddr = addr - load_base + min_vaddr
            let file_vaddr = (addr as u64)
                .wrapping_sub(load_base as u64)
                .wrapping_add(syms.min_vaddr);
            if let Some(name) = syms.lookup(file_vaddr) {
                return Some(name.to_string());
            }
        }

        // Fallback: return filename + file offset
        Some(format!("{}+0x{:x}", filename, addr - load_base))
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
