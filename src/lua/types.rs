//! Lua 5.4 internal structure definitions
//!
//! These structures must match the memory layout of Lua 5.4's internal C structures.
//! See: https://www.lua.org/source/5.4/lstate.h.html
//!
//! This module is a *reference model*: the structs, constants and helpers exist so the
//! hardcoded offsets in [`crate::lua::LuaLayout`] can be cross-checked with
//! `offset_of!`/`size_of` in tests, and so the reader has the C layout next to it in
//! readable form. The profiler itself reads fields through `LuaLayout`, so most items here
//! are intentionally unused outside of tests.
#![allow(dead_code)]

use std::mem;

/// Lua type tags
pub const LUA_TNIL: u8 = 0;
pub const LUA_TBOOLEAN: u8 = 1;
pub const LUA_TLIGHTUSERDATA: u8 = 2;
pub const LUA_TNUMBER: u8 = 3;
pub const LUA_TSTRING: u8 = 4;
pub const LUA_TTABLE: u8 = 5;
pub const LUA_TFUNCTION: u8 = 6;
pub const LUA_TUSERDATA: u8 = 7;
pub const LUA_TTHREAD: u8 = 8;

/// CallInfo status flags
pub const CIST_OAH: u16 = 1 << 0; // original value of 'allowhook'
pub const CIST_C: u16 = 1 << 1; // call is running a C function
pub const CIST_FRESH: u16 = 1 << 2; // call is on a fresh "luaV_execute" frame
pub const CIST_HOOKED: u16 = 1 << 3; // call is running a debug hook
pub const CIST_YPCALL: u16 = 1 << 4; // doing a yieldable protected call
pub const CIST_TAIL: u16 = 1 << 5; // call was tail called
pub const CIST_HOOKYIELD: u16 = 1 << 6; // last hook called yielded
pub const CIST_FIN: u16 = 1 << 7; // function "called" a finalizer
pub const CIST_TRAN: u16 = 1 << 8; // 'ci' has transfer information
pub const CIST_CLSRET: u16 = 1 << 9; // function is closing tbc variables

/// Maximum stack frames to walk (safety limit)
pub const MAX_STACK_DEPTH: usize = 1000;

/// Lua 5.4 version number (504 = 5.4.x)
pub const LUA_VERSION_NUM: i32 = 504;

/// StkIdRel is a ptrdiff_t (pointer difference) in Lua 5.4
/// It represents an offset from lua_State.stack
pub type StkIdRel = isize;

/// Instruction type (32-bit)
pub type Instruction = u32;

/// TValue - Tagged value (Lua value with type)
/// Size: 16 bytes on 64-bit (8 byte value + 1 byte tag + 7 padding)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TValue {
    pub value: Value,
    pub tt_: u8, // type tag with variant bits
    pub _padding: [u8; 7],
}

/// Value union - can hold various Lua types
#[repr(C)]
#[derive(Clone, Copy)]
pub union Value {
    pub gc: u64, // GCObject* for collectible objects
    pub p: u64,  // void* for light userdata
    pub b: i32,  // boolean
    pub f: u64,  // lua_CFunction
    pub i: i64,  // integer
    pub n: f64,  // number (float)
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Safe to read as u64 for debug purposes
        write!(f, "Value({:#x})", unsafe { self.gc })
    }
}

/// CallInfo - Information about a call
/// This is the core structure for stack walking
///
/// Layout on 64-bit (verified against Lua 5.4.7):
/// Offset  Field
/// 0       func (StkIdRel = ptrdiff_t = 8 bytes)
/// 8       top (StkIdRel)
/// 16      previous (CallInfo*)
/// 24      next (CallInfo*)
/// 32      u.l.savedpc or u.c.k (union, 8 bytes)
/// 40      u.l.trap or u.c.old_errfunc (4 bytes)
/// 44      u.l.nextraargs or padding (4 bytes)
/// 48      u2 union (8 bytes)
/// 56      nresults (i16)
/// 58      callstatus (u16)
/// Total: 60 bytes, padded to 64
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CallInfo {
    pub func: StkIdRel, // 0: function index in the stack (offset from stack base)
    pub top: StkIdRel,  // 8: top for this function
    pub previous: u64,  // 16: *CallInfo - previous in chain
    pub next: u64,      // 24: *CallInfo - next in chain
    // Union for Lua/C function specific data
    pub u_l_savedpc: u64,    // 32: For Lua functions: const Instruction *savedpc
    pub u_l_trap: i32,       // 40: For Lua functions: volatile l_signalT trap
    pub u_l_nextraargs: i32, // 44: For Lua functions: int nextraargs
    // u2 union
    pub u2: u64,           // 48: funcidx/nyield/nres/transferinfo
    pub nresults: i16,     // 56: expected number of results
    pub callstatus: u16,   // 58: call status flags
    pub _padding: [u8; 4], // 60-63: padding to 64 bytes
}

impl CallInfo {
    /// Check if this is a C function call
    pub fn is_c_function(&self) -> bool {
        self.callstatus & CIST_C != 0
    }

    /// Check if this is a Lua function call
    pub fn is_lua_function(&self) -> bool {
        !self.is_c_function()
    }

    /// Check if this was a tail call
    pub fn is_tail_call(&self) -> bool {
        self.callstatus & CIST_TAIL != 0
    }

    /// Get the saved program counter (only valid for Lua functions)
    pub fn savedpc(&self) -> u64 {
        self.u_l_savedpc
    }
}

/// lua_State - The Lua thread/coroutine state
///
/// Layout on 64-bit macOS/Linux (verified against Lua 5.4.7):
/// Offset  Field            Size
/// 0       next             8 (GCObject*)
/// 8       tt               1
/// 9       marked           1
/// 10      status           1
/// 11      allowhook        1
/// 12      nci              2
/// 14      [padding]        2
/// 16      top              8 (StkIdRel = ptrdiff_t)
/// 24      l_G              8 (global_State*)
/// 32      ci               8 (CallInfo*)  <- KEY FIELD
/// 40      stack_last       8 (StkIdRel)
/// 48      stack            8 (StkId = TValue*)  <- KEY FIELD
/// 56      openupval        8
/// 64      tbclist          8 (StkIdRel)
/// 72      gclist           8
/// 80      twups            8
/// 88      errorJmp         8
/// 96      base_ci          64 (CallInfo embedded)
/// 160     hook             8
/// 168     errfunc          8
/// 176     nCcalls          4
/// 180     oldpc            4
/// 184     basehookcount    4
/// 188     hookcount        4
/// 192     hookmask         4
/// 196     [padding]        4
/// Total: 200 bytes
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LuaState {
    // CommonHeader (GCObject *next, lu_byte tt, lu_byte marked)
    pub next: u64,  // 0: GCObject *next
    pub tt: u8,     // 8: type tag (should be LUA_TTHREAD = 8)
    pub marked: u8, // 9: GC mark bits

    // lua_State specific fields (right after CommonHeader, no extra padding)
    pub status: u8,     // 10: thread status
    pub allowhook: u8,  // 11: allow hooks flag
    pub nci: u16,       // 12: number of items in 'ci' list
    pub _padding1: u16, // 14: padding to align top to 8 bytes

    pub top: StkIdRel, // 16: first free slot in the stack (offset from stack)
    pub l_g: u64,      // 24: global_State *l_G
    pub ci: u64,       // 32: CallInfo *ci - current call info (KEY FIELD)
    pub stack_last: StkIdRel, // 40: end of stack (offset)
    pub stack: u64,    // 48: StkId stack - stack base (absolute pointer, KEY FIELD)
    pub openupval: u64, // 56: UpVal *openupval
    pub tbclist: StkIdRel, // 64: list of to-be-closed variables
    pub gclist: u64,   // 72: GCObject *gclist
    pub twups: u64,    // 80: lua_State *twups
    pub errorjmp: u64, // 88: lua_longjmp *errorJmp
    pub base_ci: CallInfo, // 96: base CallInfo (64 bytes)
    pub hook: u64,     // 160: lua_Hook hook
    pub errfunc: isize, // 168: ptrdiff_t errfunc
    pub nccalls: u32,  // 176: l_uint32 nCcalls
    pub oldpc: i32,    // 180: int oldpc
    pub basehookcount: i32, // 184: int basehookcount
    pub hookcount: i32, // 188: int hookcount
    pub hookmask: i32, // 192: volatile l_signalT hookmask
    pub _padding2: u32, // 196: padding
}

impl LuaState {
    /// Get the size of the structure
    pub fn size() -> usize {
        mem::size_of::<Self>()
    }

    /// Validate that this looks like a valid lua_State
    pub fn is_valid(&self) -> bool {
        // Check type tag is LUA_TTHREAD
        if self.tt != LUA_TTHREAD {
            return false;
        }
        // Check status is in valid range (0-5 for Lua statuses)
        if self.status > 5 {
            return false;
        }
        // Check ci pointer is not null
        if self.ci == 0 {
            return false;
        }
        // Check stack pointer is not null
        if self.stack == 0 {
            return false;
        }
        true
    }

    /// Debug print the lua_State fields
    pub fn debug_print(&self) {
        println!("lua_State debug:");
        println!("  next: 0x{:x}", self.next);
        println!("  tt: {} (expected {})", self.tt, LUA_TTHREAD);
        println!("  marked: {}", self.marked);
        println!("  status: {}", self.status);
        println!("  allowhook: {}", self.allowhook);
        println!("  nci: {}", self.nci);
        println!("  top: {}", self.top);
        println!("  l_G: 0x{:x}", self.l_g);
        println!("  ci: 0x{:x}", self.ci);
        println!("  stack_last: {}", self.stack_last);
        println!("  stack: 0x{:x}", self.stack);
    }
}

/// Proto - Function prototype (bytecode and debug info)
///
/// Layout from Lua 5.4 source (lobject.h):
/// Offset  Field            Size
/// 0       next             8 (GCObject*)
/// 8       tt               1
/// 9       marked           1
/// 10      numparams        1
/// 11      is_vararg        1
/// 12      maxstacksize     1
/// 13-15   [padding]        3 (align to 4)
/// 16      sizeupvalues     4
/// 20      sizek            4
/// 24      sizecode         4
/// 28      sizelineinfo     4
/// 32      sizep            4
/// 36      sizelocvars      4
/// 40      sizeabslineinfo  4
/// 44      linedefined      4
/// 48      lastlinedefined  4
/// 52-55   [padding]        4 (align to 8)
/// 56      k                8
/// 64      code             8
/// 72      p                8
/// 80      upvalues         8
/// 88      lineinfo         8
/// 96      abslineinfo      8
/// 104     locvars          8
/// 112     source           8
/// 120     gclist           8
/// Total: 128 bytes
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Proto {
    // CommonHeader (10 bytes)
    pub next: u64,  // 0: GCObject *next
    pub tt: u8,     // 8
    pub marked: u8, // 9

    // Proto fields (after CommonHeader)
    pub numparams: u8,        // 10: number of fixed parameters
    pub is_vararg: u8,        // 11: is vararg function
    pub maxstacksize: u8,     // 12: max stack size for this function
    pub _padding1: [u8; 3],   // 13-15: padding to align int to 4 bytes
    pub sizeupvalues: i32,    // 16: size of upvalues array
    pub sizek: i32,           // 20: size of constants array
    pub sizecode: i32,        // 24: size of bytecode
    pub sizelineinfo: i32,    // 28: size of line info
    pub sizep: i32,           // 32: size of nested proto array
    pub sizelocvars: i32,     // 36: size of local variables array
    pub sizeabslineinfo: i32, // 40: size of absolute line info
    pub linedefined: i32,     // 44: first line of definition
    pub lastlinedefined: i32, // 48: last line of definition
    pub _padding2: [u8; 4],   // 52-55: padding to align pointers to 8 bytes
    pub k: u64,               // 56: TValue *k - constants array
    pub code: u64,            // 64: Instruction *code - bytecode
    pub p: u64,               // 72: Proto **p - nested protos
    pub upvalues: u64,        // 80: Upvaldesc *upvalues
    pub lineinfo: u64,        // 88: ls_byte *lineinfo (relative line info)
    pub abslineinfo: u64,     // 96: AbsLineInfo *abslineinfo
    pub locvars: u64,         // 104: LocVar *locvars
    pub source: u64,          // 112: TString *source - source file name
    pub gclist: u64,          // 120: GCObject *gclist
}

impl Proto {
    /// Calculate line number from program counter offset
    /// The lineinfo array contains relative line offsets
    pub fn get_line(&self, _pc_offset: usize) -> i32 {
        // For now, return linedefined as fallback
        // Full implementation needs to read lineinfo array from remote memory
        self.linedefined
    }
}

/// LocVar - Local variable debug info
///
/// Layout from Lua 5.4 source (lobject.h):
/// typedef struct LocVar {
///   TString *varname;
///   int startpc;  /* first point where variable is active */
///   int endpc;    /* first point where variable is dead */
/// } LocVar;
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LocVar {
    pub varname: u64, // 0: TString *varname
    pub startpc: i32, // 8: first point where variable is active
    pub endpc: i32,   // 12: first point where variable is dead
}

/// Upvaldesc - Upvalue description
///
/// Layout from Lua 5.4 source (lobject.h):
/// typedef struct Upvaldesc {
///   TString *name;  /* upvalue name (for debug information) */
///   lu_byte instack;  /* whether it is in stack (register) */
///   lu_byte idx;  /* index of upvalue (in stack or in outer function's list) */
///   lu_byte kind;  /* kind of corresponding variable */
/// } Upvaldesc;
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Upvaldesc {
    pub name: u64,         // 0: TString *name
    pub instack: u8,       // 8: whether it is in stack
    pub idx: u8,           // 9: index of upvalue
    pub kind: u8,          // 10: kind of corresponding variable
    pub _padding: [u8; 5], // 11-15: padding
}

/// TString type tag variants (for distinguishing short vs long strings)
pub const LUA_VSHRSTR: u8 = LUA_TSTRING; // Short string (variant 0 in the high 4 bits)
pub const LUA_VLNGSTR: u8 = LUA_TSTRING | (1 << 4); // Long string

/// TString - Lua string header
///
/// Layout from Lua 5.4 source (lobject.h):
/// typedef struct TString {
///   CommonHeader;            // 10 bytes (next:8 + tt:1 + marked:1)
///   lu_byte extra;           // reserved words for short strings; "has hash" for longs
///   lu_byte shrlen;          // length for short strings
///   unsigned int hash;       // hash value
///   union {
///     size_t lnglen;         // length for long strings
///     struct TString *hnext; // linked list for hash table (short strings)
///   } u;
/// } TString;
///
/// Total: 10 + 1 + 1 + 4 + 8 = 24 bytes (with padding after marked)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TString {
    // CommonHeader (10 bytes + padding)
    pub next: u64,  // 0: GCObject *next
    pub tt: u8,     // 8: type tag (0x04=short, 0x14=long)
    pub marked: u8, // 9: GC mark bits

    // TString fields
    pub extra: u8,  // 10: reserved words for short strings
    pub shrlen: u8, // 11: length for short strings
    pub hash: u32,  // 12: hash value
    pub u_lnglen: usize, // 16: length for long strings (or hnext for short)
                    // Actual string data follows immediately after this header at offset 24
}

impl TString {
    /// Check if this is a short string
    pub fn is_short(&self) -> bool {
        // Short strings have tt=0x04 (LUA_VSHRSTR)
        // Long strings have tt=0x14 (LUA_VLNGSTR)
        (self.tt & 0x30) == 0
    }

    /// Get the length of the string
    pub fn len(&self) -> usize {
        // Short strings: length in shrlen
        // Long strings: length in u_lnglen
        if self.is_short() {
            self.shrlen as usize
        } else {
            self.u_lnglen
        }
    }

    /// Get the offset to the actual string data
    pub fn data_offset() -> usize {
        mem::size_of::<Self>()
    }
}

/// LClosure - Lua closure (function + upvalues)
///
/// Layout from Lua 5.4 source (lobject.h):
/// ClosureHeader = CommonHeader + nupvalues + gclist
/// Offset  Field            Size
/// 0       next             8 (GCObject*)
/// 8       tt               1
/// 9       marked           1
/// 10      nupvalues        1
/// 11-15   [padding]        5 (align to 8)
/// 16      gclist           8
/// 24      p                8 (Proto*)
/// Total: 32 bytes (not counting upvals array)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LClosure {
    // CommonHeader (10 bytes)
    pub next: u64,  // 0: GCObject *next
    pub tt: u8,     // 8
    pub marked: u8, // 9

    // ClosureHeader continuation
    pub nupvalues: u8,      // 10
    pub _padding1: [u8; 5], // 11-15: padding to align gclist to 8
    pub gclist: u64,        // 16: GCObject *gclist
    pub p: u64,             // 24: Proto *p - the prototype
                            // UpVal *upvals[] follows at offset 32
}

/// CClosure - C closure
///
/// Layout similar to LClosure but with function pointer instead of Proto*
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CClosure {
    // CommonHeader (10 bytes)
    pub next: u64,  // 0: GCObject *next
    pub tt: u8,     // 8
    pub marked: u8, // 9

    // ClosureHeader continuation
    pub nupvalues: u8,      // 10
    pub _padding1: [u8; 5], // 11-15: padding to align gclist to 8
    pub gclist: u64,        // 16: GCObject *gclist
    pub f: u64,             // 24: lua_CFunction f - the C function pointer
                            // TValue upvalue[] follows at offset 32
}

/// Closure type tag variants
pub const LUA_VLCL: u8 = LUA_TFUNCTION; // Lua closure (variant 0 in the high 4 bits)
pub const LUA_VLCF: u8 = LUA_TFUNCTION | (1 << 4); // light C function
pub const LUA_VCCL: u8 = LUA_TFUNCTION | (2 << 4); // C closure

/// Check if a type tag indicates a Lua closure
pub fn is_lua_closure(tt: u8) -> bool {
    (tt & 0x3F) == LUA_VLCL
}

/// Check if a type tag indicates a C closure
pub fn is_c_closure(tt: u8) -> bool {
    (tt & 0x3F) == LUA_VCCL
}

/// Check if a type tag indicates a light C function
pub fn is_light_c_function(tt: u8) -> bool {
    (tt & 0x3F) == LUA_VLCF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_callinfo_flags() {
        let mut ci = CallInfo {
            func: 0,
            top: 0,
            previous: 0,
            next: 0,
            u_l_savedpc: 0,
            u_l_trap: 0,
            u_l_nextraargs: 0,
            u2: 0,
            nresults: 0,
            callstatus: 0,
            _padding: [0; 4],
        };

        assert!(ci.is_lua_function());
        assert!(!ci.is_c_function());

        ci.callstatus = CIST_C;
        assert!(ci.is_c_function());
        assert!(!ci.is_lua_function());
    }

    #[test]
    fn test_struct_sizes() {
        // These sizes should match Lua 5.4 on 64-bit systems
        println!(
            "LuaState size: {} (expected ~200)",
            std::mem::size_of::<LuaState>()
        );
        println!(
            "CallInfo size: {} (expected 64)",
            std::mem::size_of::<CallInfo>()
        );
        println!("Proto size: {}", std::mem::size_of::<Proto>());
        println!("TString size: {}", std::mem::size_of::<TString>());
        println!(
            "TValue size: {} (expected 16)",
            std::mem::size_of::<TValue>()
        );

        // Verify key offsets
        println!(
            "Offset of ci in LuaState: {}",
            std::mem::offset_of!(LuaState, ci)
        );
        println!(
            "Offset of stack in LuaState: {}",
            std::mem::offset_of!(LuaState, stack)
        );
        println!(
            "Offset of base_ci in LuaState: {}",
            std::mem::offset_of!(LuaState, base_ci)
        );

        // Critical size checks
        assert_eq!(
            std::mem::size_of::<CallInfo>(),
            64,
            "CallInfo should be 64 bytes"
        );
        assert_eq!(
            std::mem::size_of::<TValue>(),
            16,
            "TValue should be 16 bytes"
        );
        assert_eq!(
            std::mem::offset_of!(LuaState, ci),
            32,
            "ci should be at offset 32"
        );
        assert_eq!(
            std::mem::offset_of!(LuaState, stack),
            48,
            "stack should be at offset 48"
        );
    }

    #[test]
    fn test_proto_offsets() {
        println!("Proto size: {}", std::mem::size_of::<Proto>());
        println!("Proto field offsets:");
        println!("  next: {}", std::mem::offset_of!(Proto, next));
        println!("  tt: {}", std::mem::offset_of!(Proto, tt));
        println!("  marked: {}", std::mem::offset_of!(Proto, marked));
        println!("  numparams: {}", std::mem::offset_of!(Proto, numparams));
        println!("  is_vararg: {}", std::mem::offset_of!(Proto, is_vararg));
        println!(
            "  maxstacksize: {}",
            std::mem::offset_of!(Proto, maxstacksize)
        );
        println!(
            "  sizeupvalues: {}",
            std::mem::offset_of!(Proto, sizeupvalues)
        );
        println!("  sizek: {}", std::mem::offset_of!(Proto, sizek));
        println!("  sizecode: {}", std::mem::offset_of!(Proto, sizecode));
        println!(
            "  sizelineinfo: {}",
            std::mem::offset_of!(Proto, sizelineinfo)
        );
        println!("  sizep: {}", std::mem::offset_of!(Proto, sizep));
        println!(
            "  sizelocvars: {}",
            std::mem::offset_of!(Proto, sizelocvars)
        );
        println!(
            "  sizeabslineinfo: {}",
            std::mem::offset_of!(Proto, sizeabslineinfo)
        );
        println!(
            "  linedefined: {}",
            std::mem::offset_of!(Proto, linedefined)
        );
        println!(
            "  lastlinedefined: {}",
            std::mem::offset_of!(Proto, lastlinedefined)
        );
        println!("  k: {}", std::mem::offset_of!(Proto, k));
        println!("  code: {}", std::mem::offset_of!(Proto, code));
        println!("  p: {}", std::mem::offset_of!(Proto, p));
        println!("  upvalues: {}", std::mem::offset_of!(Proto, upvalues));
        println!("  lineinfo: {}", std::mem::offset_of!(Proto, lineinfo));
        println!(
            "  abslineinfo: {}",
            std::mem::offset_of!(Proto, abslineinfo)
        );
        println!("  locvars: {}", std::mem::offset_of!(Proto, locvars));
        println!("  source: {}", std::mem::offset_of!(Proto, source));
        println!("  gclist: {}", std::mem::offset_of!(Proto, gclist));
    }

    #[test]
    fn test_tstring_offsets() {
        println!("TString size: {}", std::mem::size_of::<TString>());
        println!("TString field offsets:");
        println!("  next: {}", std::mem::offset_of!(TString, next));
        println!("  tt: {}", std::mem::offset_of!(TString, tt));
        println!("  marked: {}", std::mem::offset_of!(TString, marked));
        println!("  extra: {}", std::mem::offset_of!(TString, extra));
        println!("  shrlen: {}", std::mem::offset_of!(TString, shrlen));
        println!("  hash: {}", std::mem::offset_of!(TString, hash));
        println!("  u_lnglen: {}", std::mem::offset_of!(TString, u_lnglen));
        println!("TString::data_offset(): {}", TString::data_offset());
    }

    #[test]
    fn test_lclosure_offsets() {
        println!("LClosure size: {}", std::mem::size_of::<LClosure>());
        println!("LClosure field offsets:");
        println!("  next: {}", std::mem::offset_of!(LClosure, next));
        println!("  tt: {}", std::mem::offset_of!(LClosure, tt));
        println!("  marked: {}", std::mem::offset_of!(LClosure, marked));
        println!("  nupvalues: {}", std::mem::offset_of!(LClosure, nupvalues));
        println!("  gclist: {}", std::mem::offset_of!(LClosure, gclist));
        println!("  p: {}", std::mem::offset_of!(LClosure, p));
    }
}
