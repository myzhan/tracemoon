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
/// (mapping base minus the file's link base) to translate the linked vaddr into a
/// runtime address.
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

        let wanted = link_name(&file, symbol_name);
        let sym_vaddr = file
            .symbols()
            .chain(file.dynamic_symbols())
            // Undefined imports (`H@GLIBC_…`) have address 0 and would match anything.
            .filter(|s| !s.is_undefined())
            .find(|s| s.name().map(|n| n == wanted.as_ref()).unwrap_or(false))
            .map(|s| s.address());

        let sym_vaddr = match sym_vaddr {
            Some(v) if v != 0 => v,
            _ => continue,
        };

        let file_base_vaddr = link_base(&file);
        let bias = (base as u64).saturating_sub(file_base_vaddr);
        let runtime = (sym_vaddr + bias) as usize;
        debug!(
            "Resolved symbol {} @ vaddr 0x{:x} -> runtime 0x{:x} (base 0x{:x}, link base 0x{:x}) in {}",
            symbol_name, sym_vaddr, runtime, base, file_base_vaddr, path
        );
        return Some(runtime);
    }

    None
}

/// The in-file name of `symbol_name` for this object format.
///
/// Mach-O prefixes C symbols with an underscore (`H` is stored as `_H`), so a raw name
/// comparison silently fails there and skynet detection degenerates to single-VM mode.
/// Only the Mach-O *lookup* name is normalized; ELF names are used verbatim.
fn link_name<'a>(file: &object::File, symbol_name: &'a str) -> std::borrow::Cow<'a, str> {
    match file.format() {
        object::BinaryFormat::MachO => std::borrow::Cow::Owned(format!("_{}", symbol_name)),
        _ => std::borrow::Cow::Borrowed(symbol_name),
    }
}

/// The file's link-time base vaddr, used to turn a runtime address into a file vaddr.
///
/// ELF: the lowest `PT_LOAD` vaddr (0 for PIE, non-zero for non-PIE).
/// Mach-O: `__TEXT`'s vmaddr. `__PAGEZERO` is *not* a usable base — it has `vmaddr = 0` but
/// maps no file content, while a PIE executable's `__TEXT` sits at `0x100000000`. Taking the
/// minimum over all segments therefore loses `0x100000000` and makes every symbol lookup in
/// the main executable miss.
fn link_base(file: &object::File) -> u64 {
    match file.format() {
        object::BinaryFormat::MachO => file
            .segments()
            // `ObjectSegment::name` is `Result<Option<&str>>` in object 0.36.
            .find(|s| s.name().ok().flatten() == Some("__TEXT"))
            .map(|s| s.address())
            .unwrap_or(0),
        _ => file.segments().map(|s| s.address()).min().unwrap_or(0),
    }
}

/// Parsed symbols from a single binary/library
struct BinarySymbols {
    /// Sorted by address for efficient lookup: (address, name)
    symbols: Vec<(u64, String)>,
    /// The file's link-time base vaddr (see [`link_base`]).
    link_base: u64,
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

        let base = link_base(&file);

        debug!("Loaded {} symbols from {}", symbols.len(), path);

        Some(Self {
            symbols,
            link_base: base,
        })
    }

    /// Find the symbol containing the given offset
    fn lookup(&self, offset: u64) -> Option<&str> {
        // Binary search for the largest address <= offset
        match self
            .symbols
            .binary_search_by_key(&offset, |&(addr, _)| addr)
        {
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
            //   vaddr = addr - load_base + link_base
            let file_vaddr = (addr as u64)
                .wrapping_sub(load_base as u64)
                .wrapping_add(syms.link_base);
            if let Some(name) = syms.lookup(file_vaddr) {
                return Some(name.to_string());
            }
        }

        // Fallback: return filename + file offset
        Some(format!("{}+0x{:x}", filename, addr - load_base))
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

    fn current_binary() -> object::File<'static> {
        // The test binary itself is a real ELF / Mach-O file to parse.
        let path = std::env::current_exe().expect("test binary path");
        let data = std::fs::read(path).expect("read test binary");
        let leaked: &'static [u8] = Box::leak(data.into_boxed_slice());
        object::File::parse(leaked).expect("parse test binary")
    }

    #[test]
    fn test_resolver_creation() {
        let resolver = SymbolResolver::new(1);
        // Just verify it creates without panic
        assert_eq!(resolver.pid, 1);
    }

    /// Regression: `__PAGEZERO` (`vmaddr = 0`, no file content) used to be picked as the
    /// Mach-O link base, which shifted every main-executable lookup by `0x100000000`.
    #[test]
    fn test_link_base_excludes_pagezero() {
        let file = current_binary();
        let base = link_base(&file);

        #[cfg(target_os = "macos")]
        assert_eq!(
            base, 0x1_0000_0000,
            "Mach-O link base must be __TEXT's vmaddr"
        );

        #[cfg(target_os = "linux")]
        assert!(
            base < 0x1000_0000,
            "ELF link base is a PT_LOAD vaddr, got 0x{:x}",
            base
        );
    }

    /// Mach-O stores C symbols with a leading underscore, so a raw `"H"` comparison never
    /// matches and skynet detection silently degrades to single-VM mode.
    #[test]
    fn test_link_name_handles_macho_underscore() {
        let file = current_binary();
        let name = link_name(&file, "H");

        if file.format() == object::BinaryFormat::MachO {
            assert_eq!(name, "_H");
        } else {
            assert_eq!(name, "H");
        }
    }

    /// The two directions of the bias math must be exact inverses for the same base.
    #[test]
    fn test_bias_math_roundtrips() {
        for (link_base, load_base, sym_vaddr) in [
            (0u64, 0x5555_5555_0000u64, 0x1234u64),
            (0x1_0000_0000, 0x1_0000_5000, 0x1_0000_2a30),
        ] {
            // runtime = vaddr - link_base + load_base
            let runtime = sym_vaddr.wrapping_sub(link_base).wrapping_add(load_base);
            // vaddr = runtime - load_base + link_base
            let back = runtime.wrapping_sub(load_base).wrapping_add(link_base);
            assert_eq!(back, sym_vaddr);
        }
    }
}
