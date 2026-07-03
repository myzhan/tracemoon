//! Version-parameterized Lua memory layouts.
//!
//! tracemoon supports more than one Lua version, and their internal C structs
//! differ. Rather than a single set of `#[repr(C)]` structs (see `types.rs`,
//! kept as the verified Lua 5.4 reference), the walker reads scalar fields at
//! offsets described by a [`LuaLayout`] chosen for the detected [`LuaVersion`].
//!
//! Offsets were verified by compiling `offsetof`/`sizeof` against each version's
//! headers on x86_64 (Lua 5.4.7 and the Lua 5.5.0 bundled with skynet).

use std::fmt;
use std::str::FromStr;

use crate::error::{Result, TracemoonError};
use crate::lua::types::{LUA_TSTRING, LUA_TTHREAD};
use crate::process::{read_struct, ProcessMemoryReader};

/// A supported Lua version (memory-layout family).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LuaVersion {
    Lua54,
    Lua55,
}

impl LuaVersion {
    pub fn as_str(&self) -> &'static str {
        match self {
            LuaVersion::Lua54 => "5.4",
            LuaVersion::Lua55 => "5.5",
        }
    }
}

impl fmt::Display for LuaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LuaVersion {
    type Err = TracemoonError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "5.4" | "54" | "504" => Ok(LuaVersion::Lua54),
            "5.5" | "55" | "505" => Ok(LuaVersion::Lua55),
            other => Err(TracemoonError::UnsupportedLuaVersion {
                found: other.to_string(),
            }),
        }
    }
}

/// Describes the memory layout of a specific Lua version's internal structs.
///
/// All fields are byte offsets/sizes for 64-bit targets.
#[derive(Debug, Clone, Copy)]
pub struct LuaLayout {
    pub version: LuaVersion,

    // --- lua_State ---
    pub ls_tt: usize,
    pub ls_status: usize,
    pub ls_ci: usize,
    pub ls_stack: usize,
    pub ls_base_ci: usize,
    pub ls_size: usize,
    /// If true, `top`/`stack`/`stack_last` and `CallInfo.func` are absolute
    /// pointers (StkIdRel union). If false, `func` may be a relative offset.
    pub stk_is_pointer: bool,

    // --- CallInfo ---
    pub ci_func: usize,
    pub ci_previous: usize,
    pub ci_savedpc: usize,
    pub ci_callstatus: usize,
    pub ci_size: usize,
    /// `callstatus` width: true = u32 (5.5), false = u16 (5.4).
    pub callstatus_is_u32: bool,
    /// Mask selecting the "call is running a C function" bit.
    pub cist_c_mask: u32,

    // --- Proto (identical across 5.4/5.5, but parameterized for safety) ---
    pub pr_linedefined: usize,
    pub pr_lineinfo: usize,
    pub pr_abslineinfo: usize,
    pub pr_sizelineinfo: usize,
    pub pr_sizeabslineinfo: usize,
    pub pr_code: usize,
    pub pr_source: usize,
    pub pr_upvalues: usize,
    pub pr_sizeupvalues: usize,

    // --- TValue / StackValue ---
    pub tvalue_tt: usize,
    pub stackvalue_size: usize,

    // --- Closures / Upvaldesc ---
    pub lcl_p: usize,
    pub ccl_f: usize,
    pub uv_name: usize,
    pub uv_size: usize,

    // --- TString ---
    pub ts_tt: usize,
    pub ts_shrlen: usize,
    pub ts_lnglen: usize,
    /// Offset of inline short-string bytes.
    pub ts_short_data: usize,
    /// Offset of long-string data (a pointer if `ts_long_is_pointer`, else inline).
    pub ts_long_data: usize,
    /// 5.5 stores long-string bytes behind a `contents` pointer; 5.4 inlines them.
    pub ts_long_is_pointer: bool,
}

impl LuaLayout {
    pub fn for_version(version: LuaVersion) -> Self {
        match version {
            LuaVersion::Lua54 => Self::lua54(),
            LuaVersion::Lua55 => Self::lua55(),
        }
    }

    fn lua54() -> Self {
        LuaLayout {
            version: LuaVersion::Lua54,
            ls_tt: 8,
            ls_status: 10,
            ls_ci: 32,
            ls_stack: 48,
            ls_base_ci: 96,
            ls_size: 200,
            stk_is_pointer: false,
            ci_func: 0,
            ci_previous: 16,
            ci_savedpc: 32,
            ci_callstatus: 58,
            ci_size: 64,
            callstatus_is_u32: false,
            cist_c_mask: 1 << 1, // 0x02
            pr_linedefined: 44,
            pr_lineinfo: 88,
            pr_abslineinfo: 96,
            pr_sizelineinfo: 28,
            pr_sizeabslineinfo: 40,
            pr_code: 64,
            pr_source: 112,
            pr_upvalues: 80,
            pr_sizeupvalues: 16,
            tvalue_tt: 8,
            stackvalue_size: 16,
            lcl_p: 24,
            ccl_f: 24,
            uv_name: 0,
            uv_size: 16,
            ts_tt: 8,
            ts_shrlen: 11,
            ts_lnglen: 16,
            ts_short_data: 24,
            ts_long_data: 24,
            ts_long_is_pointer: false,
        }
    }

    fn lua55() -> Self {
        LuaLayout {
            version: LuaVersion::Lua55,
            ls_tt: 8,
            ls_status: 11,
            ls_ci: 32,
            ls_stack: 48,
            ls_base_ci: 96,
            ls_size: 208,
            stk_is_pointer: true,
            ci_func: 0,
            ci_previous: 16,
            ci_savedpc: 32,
            ci_callstatus: 60,
            ci_size: 64,
            callstatus_is_u32: true,
            cist_c_mask: 1 << 15, // 0x8000
            pr_linedefined: 44,
            pr_lineinfo: 88,
            pr_abslineinfo: 96,
            pr_sizelineinfo: 28,
            pr_sizeabslineinfo: 40,
            pr_code: 64,
            pr_source: 112,
            pr_upvalues: 80,
            pr_sizeupvalues: 16,
            tvalue_tt: 8,
            stackvalue_size: 16,
            lcl_p: 24,
            ccl_f: 24,
            uv_name: 0,
            uv_size: 16,
            ts_tt: 8,
            ts_shrlen: 11,
            ts_lnglen: 24,
            ts_short_data: 32, // inline at `contents` field
            ts_long_data: 32,  // `contents` pointer
            ts_long_is_pointer: true,
        }
    }

    // --- lua_State readers ---

    pub fn state_tt(&self, r: &dyn ProcessMemoryReader, addr: usize) -> Result<u8> {
        read_struct::<u8>(r, addr + self.ls_tt)
    }

    pub fn state_status(&self, r: &dyn ProcessMemoryReader, addr: usize) -> Result<u8> {
        read_struct::<u8>(r, addr + self.ls_status)
    }

    pub fn state_ci(&self, r: &dyn ProcessMemoryReader, addr: usize) -> Result<usize> {
        Ok(read_struct::<u64>(r, addr + self.ls_ci)? as usize)
    }

    pub fn state_stack(&self, r: &dyn ProcessMemoryReader, addr: usize) -> Result<usize> {
        Ok(read_struct::<u64>(r, addr + self.ls_stack)? as usize)
    }

    pub fn base_ci_addr(&self, state_addr: usize) -> usize {
        state_addr + self.ls_base_ci
    }

    /// Lightweight validity check usable for discovery scanning.
    pub fn is_valid_state(&self, r: &dyn ProcessMemoryReader, addr: usize) -> bool {
        if addr < 0x1000 {
            return false;
        }
        match self.state_tt(r, addr) {
            Ok(tt) if tt == LUA_TTHREAD => {}
            _ => return false,
        }
        match self.state_status(r, addr) {
            Ok(status) if status <= 6 => {}
            _ => return false,
        }
        let ci = self.state_ci(r, addr).unwrap_or(0);
        let stack = self.state_stack(r, addr).unwrap_or(0);
        ci > 0x1000 && stack > 0x1000
    }

    // --- CallInfo readers ---

    pub fn ci_previous(&self, r: &dyn ProcessMemoryReader, ci: usize) -> Result<usize> {
        Ok(read_struct::<u64>(r, ci + self.ci_previous)? as usize)
    }

    pub fn ci_savedpc(&self, r: &dyn ProcessMemoryReader, ci: usize) -> Result<usize> {
        Ok(read_struct::<u64>(r, ci + self.ci_savedpc)? as usize)
    }

    pub fn ci_is_c(&self, r: &dyn ProcessMemoryReader, ci: usize) -> Result<bool> {
        let status = if self.callstatus_is_u32 {
            read_struct::<u32>(r, ci + self.ci_callstatus)?
        } else {
            read_struct::<u16>(r, ci + self.ci_callstatus)? as u32
        };
        Ok(status & self.cist_c_mask != 0)
    }

    /// Resolve the address of the function-slot TValue for a CallInfo.
    pub fn func_slot_addr(&self, r: &dyn ProcessMemoryReader, ci: usize, stack_base: usize) -> Result<usize> {
        let func = read_struct::<i64>(r, ci + self.ci_func)?;
        if self.stk_is_pointer {
            return Ok(func as usize);
        }
        // Legacy 5.4 heuristic: large value close to stack base => absolute pointer.
        let func_val = func as usize;
        if func_val > 0x10000 && func_val > stack_base / 2 {
            Ok(func_val)
        } else {
            Ok(stack_base + (func as usize) * self.stackvalue_size)
        }
    }

    // --- TString reader ---

    /// Read a Lua string (source name / upvalue name) from a TString address.
    /// Strips a leading `@` (Lua's file-source marker).
    pub fn read_lua_string(&self, r: &dyn ProcessMemoryReader, ts_addr: usize) -> Result<String> {
        if ts_addr == 0 {
            return Ok(String::new());
        }
        let tt = read_struct::<u8>(r, ts_addr + self.ts_tt)?;
        if tt & 0x0F != LUA_TSTRING {
            return Ok(String::new());
        }
        let is_short = (tt & 0x30) == 0;

        let (len, data_addr) = if is_short {
            let len = read_struct::<u8>(r, ts_addr + self.ts_shrlen)? as usize;
            (len, ts_addr + self.ts_short_data)
        } else {
            let len = read_struct::<u64>(r, ts_addr + self.ts_lnglen)? as usize;
            let data_addr = if self.ts_long_is_pointer {
                read_struct::<u64>(r, ts_addr + self.ts_long_data)? as usize
            } else {
                ts_addr + self.ts_long_data
            };
            (len, data_addr)
        };

        if len == 0 || len > 4096 || data_addr == 0 {
            return Ok(String::new());
        }

        let bytes = r.read_memory(data_addr, len)?;
        let s = String::from_utf8_lossy(&bytes).to_string();
        Ok(s.strip_prefix('@').map(String::from).unwrap_or(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::types::{CallInfo, LClosure, LuaState, Proto, TString, TValue, Upvaldesc};
    use std::mem::{offset_of, size_of};

    #[test]
    fn lua54_matches_reference_structs() {
        let l = LuaLayout::for_version(LuaVersion::Lua54);
        assert_eq!(l.ls_tt, offset_of!(LuaState, tt));
        assert_eq!(l.ls_status, offset_of!(LuaState, status));
        assert_eq!(l.ls_ci, offset_of!(LuaState, ci));
        assert_eq!(l.ls_stack, offset_of!(LuaState, stack));
        assert_eq!(l.ls_base_ci, offset_of!(LuaState, base_ci));
        assert_eq!(l.ls_size, size_of::<LuaState>());

        assert_eq!(l.ci_func, offset_of!(CallInfo, func));
        assert_eq!(l.ci_previous, offset_of!(CallInfo, previous));
        assert_eq!(l.ci_savedpc, offset_of!(CallInfo, u_l_savedpc));
        assert_eq!(l.ci_callstatus, offset_of!(CallInfo, callstatus));
        assert_eq!(l.ci_size, size_of::<CallInfo>());

        assert_eq!(l.pr_linedefined, offset_of!(Proto, linedefined));
        assert_eq!(l.pr_lineinfo, offset_of!(Proto, lineinfo));
        assert_eq!(l.pr_abslineinfo, offset_of!(Proto, abslineinfo));
        assert_eq!(l.pr_sizelineinfo, offset_of!(Proto, sizelineinfo));
        assert_eq!(l.pr_sizeabslineinfo, offset_of!(Proto, sizeabslineinfo));
        assert_eq!(l.pr_code, offset_of!(Proto, code));
        assert_eq!(l.pr_source, offset_of!(Proto, source));
        assert_eq!(l.pr_upvalues, offset_of!(Proto, upvalues));
        assert_eq!(l.pr_sizeupvalues, offset_of!(Proto, sizeupvalues));

        assert_eq!(l.tvalue_tt, offset_of!(TValue, tt_));
        assert_eq!(l.stackvalue_size, size_of::<TValue>());
        assert_eq!(l.lcl_p, offset_of!(LClosure, p));
        assert_eq!(l.uv_name, offset_of!(Upvaldesc, name));
        assert_eq!(l.uv_size, size_of::<Upvaldesc>());

        assert_eq!(l.ts_tt, offset_of!(TString, tt));
        assert_eq!(l.ts_shrlen, offset_of!(TString, shrlen));
        assert_eq!(l.ts_lnglen, offset_of!(TString, u_lnglen));
        assert_eq!(l.ts_short_data, size_of::<TString>());
    }

    #[test]
    fn lua55_documented_offsets() {
        let l = LuaLayout::for_version(LuaVersion::Lua55);
        // Verified via offsetof against Lua 5.5.0 headers (x86_64).
        assert_eq!(l.ls_size, 208);
        assert_eq!((l.ls_ci, l.ls_stack, l.ls_base_ci), (32, 48, 96));
        assert_eq!(l.ls_status, 11);
        assert_eq!((l.ci_callstatus, l.callstatus_is_u32), (60, true));
        assert_eq!(l.cist_c_mask, 0x8000);
        assert!(l.stk_is_pointer);
        assert_eq!((l.ts_lnglen, l.ts_short_data, l.ts_long_data), (24, 32, 32));
        assert!(l.ts_long_is_pointer);
    }

    #[test]
    fn version_parsing() {
        assert_eq!("5.4".parse::<LuaVersion>().unwrap(), LuaVersion::Lua54);
        assert_eq!("5.5".parse::<LuaVersion>().unwrap(), LuaVersion::Lua55);
        assert!("6.0".parse::<LuaVersion>().is_err());
    }
}
