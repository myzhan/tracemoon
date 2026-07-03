//! Layout-driven Lua call-stack walking (Lua 5.4 / 5.5).

use std::collections::HashSet;

use log::debug;

use crate::error::{Result, TracemoonError};
use crate::lua::layout::LuaLayout;
use crate::lua::types::{is_c_closure, is_light_c_function, is_lua_closure, MAX_STACK_DEPTH};
use crate::process::{read_struct, ProcessMemoryReader};

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
            if frames.len() >= MAX_STACK_DEPTH {
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

    /// Extract frame information from a CallInfo address.
    fn get_frame(&self, ci_addr: usize, stack_base: usize) -> Result<StackFrame> {
        if self.layout.ci_is_c(self.reader, ci_addr)? {
            self.get_c_frame(ci_addr, stack_base)
        } else {
            self.get_lua_frame(ci_addr, stack_base)
        }
    }

    /// Get frame info for a C function.
    fn get_c_frame(&self, ci_addr: usize, stack_base: usize) -> Result<StackFrame> {
        let l = self.layout;
        let func_slot_addr = l.func_slot_addr(self.reader, ci_addr, stack_base)?;
        // TValue.value (the C function pointer) is at offset 0 of the slot.
        let func_addr = read_struct::<u64>(self.reader, func_slot_addr)? as usize;
        Ok(StackFrame::c_function(String::new(), func_addr))
    }

    /// Get frame info for a Lua function.
    fn get_lua_frame(&self, ci_addr: usize, stack_base: usize) -> Result<StackFrame> {
        let l = self.layout;
        let func_slot_addr = l.func_slot_addr(self.reader, ci_addr, stack_base)?;

        let value = read_struct::<u64>(self.reader, func_slot_addr)? as usize;
        let tt = read_struct::<u8>(self.reader, func_slot_addr + l.tvalue_tt)?;
        debug!("  TValue tt=0x{:x}, value=0x{:x}", tt, value);

        // Light C function: the value IS the function pointer.
        if is_light_c_function(tt) {
            return Ok(StackFrame::c_function(String::new(), value));
        }

        // C closure: read the function pointer from the closure.
        if is_c_closure(tt) {
            let f_addr = read_struct::<u64>(self.reader, value + l.ccl_f)? as usize;
            return Ok(StackFrame::c_function(String::new(), f_addr));
        }

        if !is_lua_closure(tt) {
            return Err(TracemoonError::Other(format!(
                "non-function TValue tt=0x{:x}",
                tt
            )));
        }

        // Lua closure -> Proto.
        let proto_addr = read_struct::<u64>(self.reader, value + l.lcl_p)? as usize;
        if proto_addr == 0 {
            return Ok(StackFrame::lua_function(
                String::new(),
                "[no proto]".to_string(),
                -1,
            ));
        }

        let source_ts = read_struct::<u64>(self.reader, proto_addr + l.pr_source)? as usize;
        let source = l.read_lua_string(self.reader, source_ts)?;
        let line = self.calculate_line(proto_addr, ci_addr)?;
        let name = self.read_function_name(proto_addr)?;

        Ok(StackFrame::lua_function(source, name, line))
    }

    /// Try to read a function name from Proto debug info (upvalue names).
    fn read_function_name(&self, proto_addr: usize) -> Result<String> {
        let l = self.layout;
        let upvalues = read_struct::<u64>(self.reader, proto_addr + l.pr_upvalues)? as usize;
        let sizeupvalues = read_struct::<i32>(self.reader, proto_addr + l.pr_sizeupvalues)?;
        let linedefined = read_struct::<i32>(self.reader, proto_addr + l.pr_linedefined)?;

        if upvalues != 0 && sizeupvalues > 0 {
            for i in 0..sizeupvalues.min(8) as usize {
                let uv_addr = upvalues + i * l.uv_size;
                if let Ok(name_ts) = read_struct::<u64>(self.reader, uv_addr + l.uv_name) {
                    if name_ts != 0 {
                        if let Ok(name) = l.read_lua_string(self.reader, name_ts as usize) {
                            if !name.is_empty() && name != "_ENV" {
                                return Ok(name);
                            }
                        }
                    }
                }
            }
        }

        if linedefined == 0 {
            return Ok("main".to_string());
        }
        Ok(String::new())
    }

    /// Calculate line number from the saved program counter and Proto lineinfo.
    fn calculate_line(&self, proto_addr: usize, ci_addr: usize) -> Result<i32> {
        let l = self.layout;
        let linedefined = read_struct::<i32>(self.reader, proto_addr + l.pr_linedefined)?;
        let code_start = read_struct::<u64>(self.reader, proto_addr + l.pr_code)? as usize;
        let savedpc = l.ci_savedpc(self.reader, ci_addr)?;

        if savedpc == 0 || code_start == 0 || savedpc < code_start {
            return Ok(linedefined);
        }

        let lineinfo = read_struct::<u64>(self.reader, proto_addr + l.pr_lineinfo)? as usize;
        let sizelineinfo = read_struct::<i32>(self.reader, proto_addr + l.pr_sizelineinfo)?;
        if lineinfo == 0 || sizelineinfo == 0 {
            return Ok(linedefined);
        }

        let pc_offset = (savedpc - code_start) / std::mem::size_of::<u32>();
        let pc_offset = (pc_offset as i32).min(sizelineinfo - 1).max(0) as usize;
        if pc_offset == 0 {
            return Ok(linedefined);
        }

        let bytes = self.reader.read_memory(lineinfo, pc_offset)?;
        let abslineinfo = read_struct::<u64>(self.reader, proto_addr + l.pr_abslineinfo)? as usize;
        let sizeabslineinfo = read_struct::<i32>(self.reader, proto_addr + l.pr_sizeabslineinfo)?;

        let mut line = linedefined;
        let mut abs_index = 0usize;
        for (i, &byte) in bytes.iter().enumerate() {
            let rel_line = byte as i8;
            if rel_line == -128 {
                if abslineinfo != 0 && abs_index < sizeabslineinfo as usize {
                    let abs_addr = abslineinfo + abs_index * 8;
                    if let Ok(abs_line) = read_struct::<i32>(self.reader, abs_addr + 4) {
                        line = abs_line;
                    }
                    abs_index += 1;
                }
            } else {
                line += rel_line as i32;
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
