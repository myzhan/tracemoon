//! Layout-driven Lua call-stack walking (Lua 5.4 / 5.5).

use std::collections::HashSet;

use log::debug;

use crate::error::{Result, TracemoonError};
use crate::lua::layout::LuaLayout;
use crate::lua::types::{is_c_closure, is_light_c_function, is_lua_closure, MAX_STACK_DEPTH};
use crate::process::reader::validate_addr;
use crate::process::{read_struct, ProcessMemoryReader};

/// Upper bound on the `Proto.lineinfo` bytes read for a single frame.
///
/// A real function never needs more than its instruction count. Anything larger is a
/// corrupt/stale `sizelineinfo`, and reading it would mean allocating megabytes per frame.
const MAX_LINEINFO_BYTES: usize = 1 << 20;

/// `ls_byte` value in `Proto.lineinfo` that redirects to the absolute line table.
const ABSLINEINFO_MARKER: i8 = -128;

/// Byte stride of one `AbsLineInfo` entry (`int pc; int line;`).
const ABSLINEINFO_STRIDE: usize = 8;

/// How a stack slot's `TValue` decodes into something callable.
enum FunctionRef {
    /// `TValue.value` is a `LClosure*`.
    Lua(usize),
    /// `TValue.value` is a `lua_CFunction`, or the `ccl_f` of a `CClosure`.
    C(usize),
}

/// A single stack frame.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StackFrame {
    /// Source file name (empty for C functions).
    pub source: String,
    /// Function name (may be empty for anonymous functions).
    pub name: String,
    /// Line number (-1 for C functions or unknown).
    pub line: i32,
    /// Whether this is a C function.
    pub is_c: bool,
    /// Function address (for C functions or as fallback identifier).
    pub func_addr: usize,
    /// If set, the frame is rendered verbatim (used for synthetic root labels
    /// such as `[service:name]` in multi-VM mode).
    pub raw_label: Option<String>,
}

impl StackFrame {
    /// Create a C function frame.
    pub fn c_function(name: String, addr: usize) -> Self {
        Self {
            source: String::new(),
            name,
            line: -1,
            is_c: true,
            func_addr: addr,
            raw_label: None,
        }
    }

    /// Create a Lua function frame.
    pub fn lua_function(source: String, name: String, line: i32) -> Self {
        Self {
            source,
            name,
            line,
            is_c: false,
            func_addr: 0,
            raw_label: None,
        }
    }

    /// Create a synthetic root frame rendered verbatim (e.g. a service label).
    pub fn root(label: String) -> Self {
        Self {
            source: String::new(),
            name: String::new(),
            line: -1,
            is_c: false,
            func_addr: 0,
            raw_label: Some(label),
        }
    }

    /// Format the frame for collapsed stack output.
    pub fn format(&self) -> String {
        if let Some(label) = &self.raw_label {
            return label.clone();
        }
        if self.is_c {
            if self.name.is_empty() {
                format!("[C]:0x{:x}", self.func_addr)
            } else {
                format!("[C]:{}", self.name)
            }
        } else {
            let source = if self.source.is_empty() {
                "?"
            } else {
                &self.source
            };

            if self.name.is_empty() {
                if self.line > 0 {
                    format!("{}:{}", source, self.line)
                } else {
                    source.to_string()
                }
            } else if self.line > 0 {
                format!("{}:{}:{}", source, self.line, self.name)
            } else {
                format!("{}:{}", source, self.name)
            }
        }
    }
}

/// Stack walker for traversing Lua call stacks.
pub struct StackWalker<'a> {
    reader: &'a dyn ProcessMemoryReader,
    layout: &'a LuaLayout,
}

impl<'a> StackWalker<'a> {
    /// Create a new stack walker for a given Lua layout.
    pub fn new(reader: &'a dyn ProcessMemoryReader, layout: &'a LuaLayout) -> Self {
        Self { reader, layout }
    }

    /// Walk the call stack starting from a lua_State address.
    /// Returns frames from deepest (current function) to shallowest (main).
    pub fn walk(&self, lua_state_addr: usize) -> Result<Vec<StackFrame>> {
        let l = self.layout;

        if !l.is_valid_state(self.reader, lua_state_addr) {
            debug!("lua_State validation failed at 0x{:x}", lua_state_addr);
            return Err(TracemoonError::InvalidLuaState {
                addr: lua_state_addr,
            });
        }

        let stack_base = l.state_stack(self.reader, lua_state_addr)?;
        let base_ci_addr = l.base_ci_addr(lua_state_addr);

        debug!(
            "stack_base=0x{:x}, base_ci_addr=0x{:x}",
            stack_base, base_ci_addr
        );

        let mut frames = Vec::new();
        let mut visited = HashSet::new();
        let mut ci_addr = l.state_ci(self.reader, lua_state_addr)?;

        while ci_addr != 0 && ci_addr != base_ci_addr {
            if !visited.insert(ci_addr) {
                return Err(TracemoonError::CallInfoLoop { addr: ci_addr });
            }
            // Bound the *visited node count*, not the extracted frame count: a CallInfo
            // that fails to decode contributes no frame but still costs a remote read, so
            // counting frames would let a torn chain run far past the intended limit.
            if visited.len() > MAX_STACK_DEPTH {
                return Err(TracemoonError::MaxStackDepthExceeded {
                    max_depth: MAX_STACK_DEPTH,
                });
            }

            match self.get_frame(ci_addr, stack_base) {
                Ok(frame) => {
                    debug!("  Frame: {}", frame.format());
                    frames.push(frame);
                }
                Err(e) => debug!("  Failed to get frame at 0x{:x}: {:?}", ci_addr, e),
            }

            ci_addr = l.ci_previous(self.reader, ci_addr)?;
        }

        debug!("Walked {} frames", frames.len());
        Ok(frames)
    }

    /// `base + offset` for remote addresses. Saturating so a torn/garbage base pointer
    /// cannot panic the walker with an overflow; the resulting address fails validation
    /// in the reader instead.
    fn at(&self, base: usize, offset: usize) -> usize {
        base.saturating_add(offset)
    }

    /// Extract frame information from a CallInfo.
    ///
    /// The frame kind comes from the function slot's `TValue` tag, **not** from the
    /// `CIST_C` bit of `callstatus`. `CIST_C` only says "a C function is running": the slot
    /// may hold a light C function (`value` IS the `lua_CFunction`) or a C closure
    /// (`value` is a `CClosure*` whose `ccl_f` holds the real pointer). Trusting `CIST_C`
    /// alone reports the closure object's address as the function address.
    fn get_frame(&self, ci_addr: usize, stack_base: usize) -> Result<StackFrame> {
        let func_slot_addr = self
            .layout
            .func_slot_addr(self.reader, ci_addr, stack_base)?;
        match self.decode_function(func_slot_addr)? {
            FunctionRef::C(func_addr) => Ok(StackFrame::c_function(String::new(), func_addr)),
            FunctionRef::Lua(closure_addr) => self.get_lua_frame(ci_addr, closure_addr),
        }
    }

    /// Decode a function-slot `TValue` into something callable.
    fn decode_function(&self, func_slot_addr: usize) -> Result<FunctionRef> {
        let l = self.layout;
        let value = read_struct::<u64>(self.reader, func_slot_addr)? as usize;
        let tt = read_struct::<u8>(self.reader, self.at(func_slot_addr, l.tvalue_tt))?;
        debug!("  TValue tt=0x{:x}, value=0x{:x}", tt, value);

        // Light C function: the value IS the function pointer.
        if is_light_c_function(tt) {
            return Ok(FunctionRef::C(value));
        }

        // C closure: the value is a CClosure*, the function pointer sits at `ccl_f`.
        if is_c_closure(tt) {
            let f_addr = read_struct::<u64>(self.reader, self.at(value, l.ccl_f))? as usize;
            return Ok(FunctionRef::C(f_addr));
        }

        if is_lua_closure(tt) {
            return Ok(FunctionRef::Lua(value));
        }

        Err(TracemoonError::Other(format!(
            "non-function TValue tt=0x{:x}",
            tt
        )))
    }

    /// Build a Lua frame from an already-decoded `LClosure*`.
    fn get_lua_frame(&self, ci_addr: usize, closure_addr: usize) -> Result<StackFrame> {
        let l = self.layout;
        let proto_addr = read_struct::<u64>(self.reader, self.at(closure_addr, l.lcl_p))? as usize;
        if proto_addr == 0 {
            return Ok(StackFrame::lua_function(
                String::new(),
                "[no proto]".to_string(),
                -1,
            ));
        }

        let source_ts = read_struct::<u64>(self.reader, self.at(proto_addr, l.pr_source))? as usize;
        let source = l.read_lua_string(self.reader, source_ts)?;
        let line = self.calculate_line(proto_addr, ci_addr)?;
        let name = self.function_name(proto_addr)?;

        Ok(StackFrame::lua_function(source, name, line))
    }

    /// Name of a Lua function.
    ///
    /// Lua does not record a function's own name in its `Proto`: `Upvaldesc.name` holds the
    /// names of *captured variables*, so using it labels a function with its callee (a
    /// `local function process_data() ... compute_heavy() ... end` would be reported as
    /// `compute_heavy`). Only the main chunk is unambiguous; everything else stays unnamed
    /// and renders as `source:line`.
    fn function_name(&self, proto_addr: usize) -> Result<String> {
        let linedefined =
            read_struct::<i32>(self.reader, self.at(proto_addr, self.layout.pr_linedefined))?;
        Ok(if linedefined == 0 {
            "main".to_string()
        } else {
            String::new()
        })
    }

    /// Calculate line number from the saved program counter and Proto lineinfo.
    ///
    /// `savedpc` points at the *next* instruction (Lua's `currentpc = savedpc - 1`), so
    /// accumulating deltas `0..=pc_offset-1` — i.e. exactly `pc_offset` of them — already
    /// yields the current instruction's line. Do not subtract one from `savedpc` again; the
    /// remaining off-by-one risk is only in the *clamp*, whose legal maximum is
    /// `sizelineinfo` (one entry per instruction), not `sizelineinfo - 1`.
    fn calculate_line(&self, proto_addr: usize, ci_addr: usize) -> Result<i32> {
        let l = self.layout;
        let linedefined = read_struct::<i32>(self.reader, self.at(proto_addr, l.pr_linedefined))?;
        let code_start = read_struct::<u64>(self.reader, self.at(proto_addr, l.pr_code))? as usize;
        let savedpc = l.ci_savedpc(self.reader, ci_addr)?;
        let lineinfo_addr =
            read_struct::<u64>(self.reader, self.at(proto_addr, l.pr_lineinfo))? as usize;
        let sizelineinfo = read_struct::<i32>(self.reader, self.at(proto_addr, l.pr_sizelineinfo))?;

        if savedpc == 0
            || code_start == 0
            || savedpc < code_start
            || lineinfo_addr == 0
            || sizelineinfo <= 0
        {
            return Ok(linedefined);
        }

        let pc_offset = (savedpc - code_start) / std::mem::size_of::<u32>();
        // A savedpc outside this function's own line table means the frame was torn (or
        // stale): prefer `linedefined` over a bogus, potentially huge read.
        if pc_offset == 0 || pc_offset > sizelineinfo as usize || pc_offset > MAX_LINEINFO_BYTES {
            return Ok(linedefined);
        }

        validate_addr(lineinfo_addr, pc_offset)?;
        let bytes = self.reader.read_memory(lineinfo_addr, pc_offset)?;

        let abslineinfo_addr =
            read_struct::<u64>(self.reader, self.at(proto_addr, l.pr_abslineinfo))? as usize;
        let sizeabslineinfo =
            read_struct::<i32>(self.reader, self.at(proto_addr, l.pr_sizeabslineinfo))?.max(0)
                as usize;

        let mut line = linedefined;
        let mut abs_index = 0usize;
        for (i, &byte) in bytes.iter().enumerate() {
            let rel_line = byte as i8;
            if rel_line == ABSLINEINFO_MARKER {
                if abslineinfo_addr != 0 && abs_index < sizeabslineinfo {
                    let abs_addr = self.at(abslineinfo_addr, abs_index * ABSLINEINFO_STRIDE);
                    if let Ok(abs_line) = read_struct::<i32>(self.reader, self.at(abs_addr, 4)) {
                        line = abs_line;
                    }
                    abs_index += 1;
                }
            } else {
                line = line.saturating_add(rel_line as i32);
            }
            if i >= pc_offset - 1 {
                break;
            }
        }

        Ok(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::types::{LUA_TSTRING, LUA_TTHREAD, LUA_VCCL, LUA_VLCF, LUA_VLCL};
    use crate::lua::LuaVersion;
    use crate::process::reader::testing::WindowReader;

    // A flat image of one Lua VM: state, stack, one CallInfo, its function, its Proto and
    // the Proto's source string. Everything is addressed off these constants so the image
    // reads like the real structure graph.
    const STATE: usize = 0x1000;
    const STACK: usize = 0x1100;
    const CI: usize = 0x1200;
    const CLOSURE: usize = 0x1300;
    const PROTO: usize = 0x1400;
    const SOURCE_TS: usize = 0x1500;
    const LINEINFO: usize = 0x1600;
    const CODE: usize = 0x2000;
    const C_FUNC: usize = 0x3000;

    /// Build the shared part of the image: a valid `lua_State` owning one non-base CallInfo
    /// whose function slot holds `func_tt` at `func_value`.
    fn base_image(layout: &LuaLayout, func_tt: u8, func_value: usize) -> WindowReader {
        let mut r = WindowReader::new(0x1000, 0x1000);

        r.write_u8(STATE + layout.ls_tt, LUA_TTHREAD);
        r.write_u8(STATE + layout.ls_status, 0);
        r.write_u64(STATE + layout.ls_ci, CI as u64);
        r.write_u64(STATE + layout.ls_stack, STACK as u64);

        // 5.4 may store the function slot as a stack-relative index, 5.5 as a pointer.
        let func_slot = STACK + 3 * layout.stackvalue_size;
        let func_field = if layout.stk_is_pointer {
            func_slot as u64
        } else {
            3
        };
        r.write_u64(CI + layout.ci_func, func_field);
        r.write_u64(CI + layout.ci_previous, 0);
        // savedpc points at the *next* instruction: index 2 here.
        r.write_u64(CI + layout.ci_savedpc, (CODE + 2 * 4) as u64);

        r.write_u64(func_slot, func_value as u64);
        r.write_u8(func_slot + layout.tvalue_tt, func_tt);

        r
    }

    /// Turn the image into a Lua-closure frame with a line table of `[1, 2, 1, 1]`.
    fn lua_image(layout: &LuaLayout, linedefined: i32, sizelineinfo: i32) -> WindowReader {
        let mut r = base_image(layout, LUA_VLCL, CLOSURE);

        r.write_u64(CLOSURE + layout.lcl_p, PROTO as u64);

        r.write_i32(PROTO + layout.pr_linedefined, linedefined);
        r.write_u64(PROTO + layout.pr_source, SOURCE_TS as u64);
        r.write_u64(PROTO + layout.pr_code, CODE as u64);
        r.write_u64(PROTO + layout.pr_lineinfo, LINEINFO as u64);
        r.write_i32(PROTO + layout.pr_sizelineinfo, sizelineinfo);
        r.write_u64(PROTO + layout.pr_abslineinfo, 0);
        r.write_i32(PROTO + layout.pr_sizeabslineinfo, 0);

        // Short string "@t.lua": Lua marks file sources with a leading '@'.
        r.write_u8(SOURCE_TS + layout.ts_tt, LUA_TSTRING);
        r.write_u8(SOURCE_TS + layout.ts_shrlen, 6);
        r.write(SOURCE_TS + layout.ts_short_data, b"@t.lua");

        r.write(LINEINFO, &[1, 2, 1, 1]);

        r
    }

    #[test]
    fn test_lua_frame_line_accumulates_to_current_instruction() {
        for version in [LuaVersion::Lua54, LuaVersion::Lua55] {
            let layout = LuaLayout::for_version(version);
            let reader = lua_image(&layout, 10, 4);
            let frames = StackWalker::new(&reader, &layout).walk(STATE).unwrap();

            assert_eq!(frames.len(), 1, "{version:?}");
            // savedpc is index 2, so the line is linedefined + delta[0] + delta[1].
            assert_eq!(frames[0].source, "t.lua", "{version:?}");
            assert_eq!(frames[0].line, 13, "{version:?}");
            assert_eq!(frames[0].format(), "t.lua:13", "{version:?}");
        }
    }

    #[test]
    fn test_main_chunk_is_named_main_and_others_stay_unnamed() {
        let layout = LuaLayout::for_version(LuaVersion::Lua54);

        let reader = lua_image(&layout, 0, 4);
        let frames = StackWalker::new(&reader, &layout).walk(STATE).unwrap();
        assert_eq!(frames[0].name, "main");

        let reader = lua_image(&layout, 10, 4);
        let frames = StackWalker::new(&reader, &layout).walk(STATE).unwrap();
        assert_eq!(frames[0].name, "");
    }

    /// Upvalue names are captured *variable* names, not the function's own name: a
    /// `local function process_data() ... compute_heavy() ... end` captures `compute_heavy`
    /// and used to be reported under that name.
    #[test]
    fn test_upvalue_names_are_not_used_as_function_name() {
        let layout = LuaLayout::for_version(LuaVersion::Lua54);
        let mut reader = lua_image(&layout, 10, 4);

        // One upvalue, named "compute_heavy".
        let upvalues = 0x1700;
        let upval_name_ts = 0x1800;
        reader.write_u64(PROTO + layout.pr_upvalues, upvalues as u64);
        reader.write_i32(PROTO + layout.pr_sizeupvalues, 1);
        reader.write_u64(upvalues + layout.uv_name, upval_name_ts as u64);
        reader.write_u8(upval_name_ts + layout.ts_tt, LUA_TSTRING);
        reader.write_u8(upval_name_ts + layout.ts_shrlen, 5);
        reader.write(upval_name_ts + layout.ts_short_data, b"heavy");

        let frames = StackWalker::new(&reader, &layout).walk(STATE).unwrap();
        assert_eq!(frames[0].name, "");
        assert_eq!(frames[0].format(), "t.lua:13");
    }

    /// A pc outside the function's own line table means a torn read: fall back to the
    /// function's definition line rather than reading past the table.
    #[test]
    fn test_pc_past_line_table_falls_back_to_linedefined() {
        let layout = LuaLayout::for_version(LuaVersion::Lua54);
        let mut reader = lua_image(&layout, 10, 2);
        // pc index 2 is one past a 2-entry table.
        reader.write_u64(CI + layout.ci_savedpc, (CODE + 3 * 4) as u64);

        let frames = StackWalker::new(&reader, &layout).walk(STATE).unwrap();
        assert_eq!(frames[0].line, 10);
    }

    #[test]
    fn test_c_closure_resolves_real_function_pointer() {
        let layout = LuaLayout::for_version(LuaVersion::Lua54);
        let mut reader = base_image(&layout, LUA_VCCL, CLOSURE);
        reader.write_u64(CLOSURE + layout.ccl_f, C_FUNC as u64);

        let frames = StackWalker::new(&reader, &layout).walk(STATE).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_c);
        // The reported address must be `ccl_f`, not the CClosure object itself.
        assert_eq!(frames[0].func_addr, C_FUNC);
    }

    #[test]
    fn test_light_c_function_uses_value_as_address() {
        let layout = LuaLayout::for_version(LuaVersion::Lua54);
        let reader = base_image(&layout, LUA_VLCF, C_FUNC);

        let frames = StackWalker::new(&reader, &layout).walk(STATE).unwrap();
        assert!(frames[0].is_c);
        assert_eq!(frames[0].func_addr, C_FUNC);
    }

    #[test]
    fn test_callinfo_loop_is_reported() {
        let layout = LuaLayout::for_version(LuaVersion::Lua54);
        let mut reader = lua_image(&layout, 10, 4);
        reader.write_u64(CI + layout.ci_previous, CI as u64);

        let err = StackWalker::new(&reader, &layout).walk(STATE).unwrap_err();
        assert!(matches!(err, TracemoonError::CallInfoLoop { .. }));
    }

    #[test]
    fn test_corrupt_state_is_rejected() {
        let layout = LuaLayout::for_version(LuaVersion::Lua54);
        let mut reader = lua_image(&layout, 10, 4);
        reader.write_u8(STATE + layout.ls_tt, 0);

        let err = StackWalker::new(&reader, &layout).walk(STATE).unwrap_err();
        assert!(matches!(err, TracemoonError::InvalidLuaState { .. }));
    }

    #[test]
    fn test_stack_frame_format() {
        let c_frame = StackFrame::c_function("lua_pcall".to_string(), 0x12345678);
        assert_eq!(c_frame.format(), "[C]:lua_pcall");

        let c_frame_unknown = StackFrame::c_function(String::new(), 0x12345678);
        assert_eq!(c_frame_unknown.format(), "[C]:0x12345678");

        let lua_frame = StackFrame::lua_function("test.lua".to_string(), "my_func".to_string(), 42);
        assert_eq!(lua_frame.format(), "test.lua:42:my_func");

        let lua_frame_anon = StackFrame::lua_function("test.lua".to_string(), String::new(), 42);
        assert_eq!(lua_frame_anon.format(), "test.lua:42");

        let root = StackFrame::root("[service:bench_seri]".to_string());
        assert_eq!(root.format(), "[service:bench_seri]");
    }
}
