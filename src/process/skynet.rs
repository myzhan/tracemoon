//! Skynet-aware multi-VM discovery.
//!
//! A skynet process hosts many Lua VMs — one running coroutine (`snlua.activeL`)
//! per service. This walks the exported `H` (`struct handle_storage *`) global to
//! enumerate every service slot, and for each `snlua` service resolves its live
//! `activeL` and registered service name.
//!
//! Offsets verified against skynet's `skynet-src/skynet_handle.c`,
//! `skynet_server.c` and the `snlua` service on x86_64:
//! - `handle_storage`: `slot_size`(int)@16, `slot`(skynet_context**)@24,
//!   `name_count`(int)@36, `name`(handle_name*)@40.
//! - `handle_name`: `char* name`@0, `uint32_t handle`@8 (16-byte stride).
//! - `skynet_context`: `void* instance`@0 (=`snlua*`), `handle`(u32)@96.
//! - `snlua`: `activeL`@40.

use log::{debug, info};
use remoteprocess::Pid;

use crate::error::Result;
use crate::lua::LuaLayout;
use crate::process::{read_string, read_struct, ProcessMemoryReader};
use crate::symbols::resolve_symbol_address;

// --- handle_storage offsets ---
const HS_SLOT_SIZE: usize = 16;
const HS_SLOT: usize = 24;
const HS_NAME_COUNT: usize = 36;
const HS_NAME: usize = 40;
// --- handle_name ---
const HN_STRIDE: usize = 16;
const HN_NAME: usize = 0;
const HN_HANDLE: usize = 8;
// --- skynet_context ---
const CTX_INSTANCE: usize = 0;
const CTX_HANDLE: usize = 96;
// --- snlua ---
const SNLUA_ACTIVEL: usize = 40;

/// One discovered Lua VM (a skynet service's running coroutine).
#[derive(Debug, Clone)]
pub struct LuaVm {
    pub lua_state_addr: usize,
    pub label: String,
}

/// Skynet-aware discovery, keyed off the runtime address of the `H` global.
pub struct SkynetDiscovery<'a> {
    reader: &'a dyn ProcessMemoryReader,
    layout: &'a LuaLayout,
    h_addr: usize,
}

impl<'a> SkynetDiscovery<'a> {
    /// Try to detect a skynet process by resolving the `H` symbol, returning
    /// its runtime address. Returns `None` for non-skynet targets.
    pub fn resolve_h(pid: Pid) -> Option<usize> {
        let h_addr = resolve_symbol_address(pid, "H")?;
        info!("Detected skynet process (H @ 0x{:x})", h_addr);
        Some(h_addr)
    }

    /// Build a discovery bound to an already-resolved `H` runtime address.
    pub fn new(reader: &'a dyn ProcessMemoryReader, layout: &'a LuaLayout, h_addr: usize) -> Self {
        Self {
            reader,
            layout,
            h_addr,
        }
    }

    /// Enumerate every profilable Lua service in the process.
    pub fn enumerate(&self) -> Result<Vec<LuaVm>> {
        let r = self.reader;

        // H is a pointer variable; deref to get the handle_storage struct.
        let hs = read_struct::<u64>(r, self.h_addr)? as usize;
        if hs < 0x1000 {
            debug!("H points to invalid handle_storage 0x{:x}", hs);
            return Ok(Vec::new());
        }

        let slot_size = read_struct::<i32>(r, hs + HS_SLOT_SIZE)? as usize;
        let slot = read_struct::<u64>(r, hs + HS_SLOT)? as usize;
        let name_count = read_struct::<i32>(r, hs + HS_NAME_COUNT)?;
        let name_arr = read_struct::<u64>(r, hs + HS_NAME)? as usize;

        if slot < 0x1000 || slot_size == 0 || slot_size > 1_000_000 {
            debug!(
                "Implausible handle_storage: slot=0x{:x} slot_size={}",
                slot, slot_size
            );
            return Ok(Vec::new());
        }

        let mut vms = Vec::new();
        for i in 0..slot_size {
            let ctx = match read_struct::<u64>(r, slot + i * 8) {
                Ok(v) => v as usize,
                Err(_) => continue,
            };
            if ctx < 0x1000 {
                continue;
            }

            // instance is a snlua* for snlua services (0/garbage for C services).
            let instance = match read_struct::<u64>(r, ctx + CTX_INSTANCE) {
                Ok(v) => v as usize,
                Err(_) => continue,
            };
            if instance < 0x1000 {
                continue;
            }

            let active_l = match read_struct::<u64>(r, instance + SNLUA_ACTIVEL) {
                Ok(v) => v as usize,
                Err(_) => continue,
            };
            if active_l < 0x1000 || !self.layout.is_valid_state(r, active_l) {
                continue;
            }

            let handle = read_struct::<u32>(r, ctx + CTX_HANDLE).unwrap_or(0);
            let label = self
                .resolve_name(name_arr, name_count, handle)
                .unwrap_or_else(|| format!(":{:08x}", handle));

            debug!("service {} -> activeL 0x{:x}", label, active_l);
            vms.push(LuaVm {
                lua_state_addr: active_l,
                label,
            });
        }

        debug!("Enumerated {} Lua service(s)", vms.len());
        Ok(vms)
    }

    /// Look up a service's registered name by handle in the name table.
    fn resolve_name(&self, name_arr: usize, name_count: i32, handle: u32) -> Option<String> {
        if name_arr < 0x1000 || name_count <= 0 || handle == 0 {
            return None;
        }
        for i in 0..name_count as usize {
            let entry = name_arr + i * HN_STRIDE;
            let h = read_struct::<u32>(self.reader, entry + HN_HANDLE).ok()?;
            if h != handle {
                continue;
            }
            let name_ptr = read_struct::<u64>(self.reader, entry + HN_NAME).ok()? as usize;
            if name_ptr < 0x1000 {
                return None;
            }
            let name = read_string(self.reader, name_ptr, 128).ok()?;
            if name.is_empty() {
                return None;
            }
            return Some(name);
        }
        None
    }
}
