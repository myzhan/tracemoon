//! Stack walking for Lua 5.4

use std::collections::HashSet;

use log::debug;

use crate::error::{Result, TracemoonError};
use crate::lua::types::{CallInfo, LuaState, Proto, LClosure, TValue, TString, MAX_STACK_DEPTH, StkIdRel, Upvaldesc};
use crate::lua::types::{is_lua_closure, is_light_c_function, is_c_closure};
use crate::process::{ProcessMemoryReader, read_struct};

/// A single stack frame
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StackFrame {
    /// Source file name (empty for C functions)
    pub source: String,
    /// Function name (may be empty for anonymous functions)
    pub name: String,
    /// Line number (-1 for C functions or unknown)
    pub line: i32,
    /// Whether this is a C function
    pub is_c: bool,
    /// Function address (for C functions or as fallback identifier)
    pub func_addr: usize,
}

impl StackFrame {
    /// Create a C function frame
    pub fn c_function(name: String, addr: usize) -> Self {
        Self {
            source: String::new(),
            name,
            line: -1,
            is_c: true,
            func_addr: addr,
        }
    }

    /// Create a Lua function frame
    pub fn lua_function(source: String, name: String, line: i32) -> Self {
        Self {
            source,
            name,
            line,
            is_c: false,
            func_addr: 0,
        }
    }

    /// Format the frame for collapsed stack output
    pub fn format(&self) -> String {
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

/// Stack walker for traversing Lua call stacks
pub struct StackWalker<'a> {
    reader: &'a dyn ProcessMemoryReader,
}

impl<'a> StackWalker<'a> {
    /// Create a new stack walker
    pub fn new(reader: &'a dyn ProcessMemoryReader) -> Self {
        Self { reader }
    }

    /// Walk the call stack starting from lua_State address
    /// Returns frames from deepest (current function) to shallowest (main)
    pub fn walk(&self, lua_state_addr: usize) -> Result<Vec<StackFrame>> {
        let lua_state: LuaState = read_struct(self.reader, lua_state_addr)?;
        
        debug!("lua_State at 0x{:x}:", lua_state_addr);
        debug!("  tt={} (expected {}), status={}, nci={}", 
               lua_state.tt, crate::lua::types::LUA_TTHREAD, lua_state.status, lua_state.nci);
        debug!("  ci=0x{:x}, stack=0x{:x}", lua_state.ci, lua_state.stack);
        
        if !lua_state.is_valid() {
            debug!("lua_State validation failed");
            return Err(TracemoonError::InvalidLuaState { addr: lua_state_addr });
        }

        // stack is an absolute pointer to TValue array
        let stack_base = lua_state.stack as usize;
        let base_ci_addr = lua_state_addr + std::mem::offset_of!(LuaState, base_ci);
        
        debug!("stack_base=0x{:x}, base_ci_addr=0x{:x}", stack_base, base_ci_addr);
        
        let mut frames = Vec::new();
        let mut visited = HashSet::new();
        let mut ci_addr = lua_state.ci as usize;
        
        // Walk the CallInfo chain
        while ci_addr != 0 && ci_addr != base_ci_addr {
            debug!("Walking CallInfo at 0x{:x}", ci_addr);
            
            // Loop detection
            if !visited.insert(ci_addr) {
                return Err(TracemoonError::CallInfoLoop { addr: ci_addr });
            }
            
            // Max depth check
            if frames.len() >= MAX_STACK_DEPTH {
                return Err(TracemoonError::MaxStackDepthExceeded { max_depth: MAX_STACK_DEPTH });
            }
            
            // Read CallInfo
            let ci: CallInfo = read_struct(self.reader, ci_addr)?;
            debug!("  CallInfo: func={}, callstatus=0x{:x}, previous=0x{:x}", 
                   ci.func, ci.callstatus, ci.previous);
            
            // Get the frame
            match self.get_frame(&ci, stack_base) {
                Ok(frame) => {
                    debug!("  Frame: {}", frame.format());
                    frames.push(frame);
                }
                Err(e) => {
                    debug!("  Failed to get frame: {:?}", e);
                }
            }
            
            // Move to previous frame
            ci_addr = ci.previous as usize;
        }
        
        debug!("Walked {} frames", frames.len());
        
        // Frames are currently deepest-first (current at index 0)
        // This is the expected order for profiling
        Ok(frames)
    }

    /// Extract frame information from a CallInfo
    fn get_frame(&self, ci: &CallInfo, stack_base: usize) -> Result<StackFrame> {
        if ci.is_c_function() {
            self.get_c_frame(ci, stack_base)
        } else {
            self.get_lua_frame(ci, stack_base)
        }
    }

    /// Get frame info for a C function
    fn get_c_frame(&self, ci: &CallInfo, stack_base: usize) -> Result<StackFrame> {
        // Calculate function slot address
        // Note: In some Lua 5.4 builds, func is an absolute pointer, not relative offset
        let func_slot_addr = self.calculate_func_addr(ci.func, stack_base);
        debug!("  C frame: func_slot_addr=0x{:x}", func_slot_addr);
        
        let tvalue: TValue = read_struct(self.reader, func_slot_addr)?;
        
        // The function pointer is in the value
        let func_addr = unsafe { tvalue.value.f as usize };
        
        Ok(StackFrame::c_function(String::new(), func_addr))
    }

    /// Get frame info for a Lua function
    fn get_lua_frame(&self, ci: &CallInfo, stack_base: usize) -> Result<StackFrame> {
        // Calculate function slot address
        // Note: In some Lua 5.4 builds, func is an absolute pointer, not relative offset
        let func_slot_addr = self.calculate_func_addr(ci.func, stack_base);
        debug!("  Lua frame: func_slot_addr=0x{:x}", func_slot_addr);
        
        let tvalue: TValue = read_struct(self.reader, func_slot_addr)?;
        
        let tt = tvalue.tt_;
        debug!("  TValue tt=0x{:x}, value=0x{:x}", tt, unsafe { tvalue.value.gc });
        
        // Check for light C function (e.g., math.sin, math.cos)
        // tt=0x16 = LUA_VLCF: the value IS the C function pointer directly
        if is_light_c_function(tt) {
            let func_addr = unsafe { tvalue.value.f as usize };
            debug!("  Light C function at 0x{:x}", func_addr);
            return Ok(StackFrame::c_function(String::new(), func_addr));
        }
        
        // Check for C closure (e.g., some library functions)
        if is_c_closure(tt) {
            let closure_addr = unsafe { tvalue.value.gc as usize };
            debug!("  C closure at 0x{:x}", closure_addr);
            // Read CClosure.f at offset 24 (same as LClosure.p)
            let f_addr = closure_addr + 24;
            if let Ok(f_bytes) = self.reader.read_memory(f_addr, 8) {
                let func_ptr = u64::from_ne_bytes([
                    f_bytes[0], f_bytes[1], f_bytes[2], f_bytes[3],
                    f_bytes[4], f_bytes[5], f_bytes[6], f_bytes[7],
                ]) as usize;
                return Ok(StackFrame::c_function(String::new(), func_ptr));
            }
            return Ok(StackFrame::c_function(String::new(), 0));
        }
        
        if !is_lua_closure(tt) {
            // Non-function type on the stack (e.g., tt=0x3 is a number)
            // This can happen during stack transitions. Skip silently.
            let base_type = tt & 0x0F;
            debug!("  Non-function TValue (tt=0x{:x}, base_type={}), skipping", tt, base_type);
            return Err(TracemoonError::Other(
                format!("non-function TValue tt=0x{:x}", tt)
            ));
        }
        
        // Read the closure
        let closure_addr = unsafe { tvalue.value.gc as usize };
        debug!("  Reading LClosure at 0x{:x}", closure_addr);
        let closure: LClosure = read_struct(self.reader, closure_addr)?;
        
        // Read the Proto
        let proto_addr = closure.p as usize;
        if proto_addr == 0 {
            return Ok(StackFrame::lua_function(
                String::new(),
                "[no proto]".to_string(),
                -1,
            ));
        }
        
        debug!("  Reading Proto at 0x{:x}", proto_addr);
        let proto: Proto = read_struct(self.reader, proto_addr)?;
        debug!("  Proto: numparams={}, is_vararg={}, linedefined={}, source=0x{:x}", 
               proto.numparams, proto.is_vararg, proto.linedefined, proto.source);
        
        // Read the source filename
        let source = self.read_source_name(proto.source as usize)?;
        debug!("  Source: {}", source);
        
        // Calculate line number from savedpc
        let line = self.calculate_line(&proto, ci)?;
        
        // Try to get function name from debug info
        let name = self.read_function_name(&proto)?;
        debug!("  Function name: {}", if name.is_empty() { "<anonymous>" } else { &name });
        
        Ok(StackFrame::lua_function(source, name, line))
    }

    /// Calculate the function slot address from CallInfo.func
    /// 
    /// In Lua 5.4, func can be either:
    /// 1. A relative offset (StkIdRel) from stack base - small values
    /// 2. An absolute pointer (StkId) - large values close to stack address
    /// 
    /// We detect which one based on the value magnitude.
    fn calculate_func_addr(&self, func: StkIdRel, stack_base: usize) -> usize {
        let func_val = func as usize;
        
        // If func looks like a pointer (large value, similar magnitude to stack_base),
        // use it directly. Otherwise, treat it as an offset.
        if func_val > 0x10000 && func_val > stack_base / 2 {
            // Looks like an absolute pointer
            debug!("  func=0x{:x} appears to be absolute pointer", func_val);
            func_val
        } else {
            // Relative offset - multiply by TValue size
            let addr = stack_base + (func as usize) * std::mem::size_of::<TValue>();
            debug!("  func={} is relative offset, addr=0x{:x}", func, addr);
            addr
        }
    }

    /// Read the source filename from a TString
    fn read_source_name(&self, tstring_addr: usize) -> Result<String> {
        if tstring_addr == 0 {
            debug!("  TString address is null");
            return Ok(String::new());
        }
        
        debug!("  Reading TString at 0x{:x}", tstring_addr);
        let tstring: TString = read_struct(self.reader, tstring_addr)?;
        debug!("  TString: tt=0x{:x}, extra={}, shrlen={}, hash=0x{:x}, u_lnglen={}", 
               tstring.tt, tstring.extra, tstring.shrlen, tstring.hash, tstring.u_lnglen);
        debug!("  TString is_short={}", tstring.is_short());
        
        let len = tstring.len();
        debug!("  TString length: {}", len);
        
        if len == 0 || len > 1024 {
            debug!("  Invalid TString length: {}", len);
            return Ok(String::new());
        }
        
        // String data follows TString header
        let data_addr = tstring_addr + TString::data_offset();
        debug!("  TString::data_offset() = {}", TString::data_offset());
        debug!("  Reading string data at 0x{:x}, len={}", data_addr, len);
        let bytes = self.reader.read_memory(data_addr, len)?;
        
        // Remove leading '@' if present (Lua convention for file sources)
        let s = String::from_utf8_lossy(&bytes).to_string();
        debug!("  Read string: '{}'", s);
        if s.starts_with('@') {
            Ok(s[1..].to_string())
        } else {
            Ok(s)
        }
    }

    /// Try to read function name from Proto's debug info
    /// 
    /// In Lua 5.4, function names are not stored directly in Proto.
    /// However, we can try to get the name from:
    /// 1. The first upvalue name (for named functions, often "_ENV" or the function name)
    /// 2. Fall back to showing function definition line
    fn read_function_name(&self, proto: &Proto) -> Result<String> {
        // Try to get name from upvalues - the first upvalue is often "_ENV" for main chunks
        // or could indicate the function name in some cases
        if proto.upvalues != 0 && proto.sizeupvalues > 0 {
            let upvaldesc_size = std::mem::size_of::<Upvaldesc>();
            
            // Check all upvalues for a meaningful name
            for i in 0..proto.sizeupvalues.min(8) as usize {
                let upval_addr = proto.upvalues as usize + i * upvaldesc_size;
                if let Ok(upval) = read_struct::<Upvaldesc>(self.reader, upval_addr) {
                    if upval.name != 0 {
                        if let Ok(name) = self.read_source_name(upval.name as usize) {
                            // Skip "_ENV" which is the global environment
                            if !name.is_empty() && name != "_ENV" {
                                return Ok(name);
                            }
                        }
                    }
                }
            }
        }
        
        // For main chunk or anonymous functions, create a descriptive name
        if proto.linedefined == 0 {
            // Main chunk
            return Ok("main".to_string());
        }
        
        // Anonymous function - return empty, will be shown as source:line
        Ok(String::new())
    }

    /// Calculate line number from saved program counter
    /// 
    /// In Lua 5.4, lineinfo is an array of relative line offsets (signed bytes).
    /// To get the actual line, we need to sum up offsets from linedefined.
    /// Value -128 indicates we should look up abslineinfo instead.
    fn calculate_line(&self, proto: &Proto, ci: &CallInfo) -> Result<i32> {
        let savedpc = ci.savedpc() as usize;
        let code_start = proto.code as usize;
        
        if savedpc == 0 || code_start == 0 {
            return Ok(proto.linedefined);
        }
        
        // Calculate PC offset (in instructions, each instruction is 4 bytes)
        if savedpc < code_start {
            return Ok(proto.linedefined);
        }
        
        let pc_offset = (savedpc - code_start) / std::mem::size_of::<u32>();
        
        // If no lineinfo available, return linedefined
        if proto.lineinfo == 0 || proto.sizelineinfo == 0 {
            return Ok(proto.linedefined);
        }
        
        // Clamp pc_offset to valid range
        let pc_offset = (pc_offset as i32).min(proto.sizelineinfo - 1).max(0) as usize;
        
        if pc_offset == 0 {
            return Ok(proto.linedefined);
        }
        
        // Read lineinfo array and accumulate offsets
        // lineinfo contains signed bytes (relative line offsets)
        let lineinfo_addr = proto.lineinfo as usize;
        let bytes = self.reader.read_memory(lineinfo_addr, pc_offset)?;
        
        let mut line = proto.linedefined;
        let mut abs_index = 0usize;
        
        for (i, &byte) in bytes.iter().enumerate() {
            let rel_line = byte as i8;
            
            if rel_line == -128 {
                // ABSLINEINFO marker - look up in abslineinfo array
                // AbsLineInfo is { int pc; int line; } = 8 bytes
                if proto.abslineinfo != 0 && abs_index < proto.sizeabslineinfo as usize {
                    let abs_addr = proto.abslineinfo as usize + abs_index * 8;
                    if let Ok(abs_bytes) = self.reader.read_memory(abs_addr, 8) {
                        // Read the line field (second i32)
                        let abs_line = i32::from_ne_bytes([
                            abs_bytes[4], abs_bytes[5], abs_bytes[6], abs_bytes[7]
                        ]);
                        line = abs_line;
                    }
                    abs_index += 1;
                }
            } else {
                line += rel_line as i32;
            }
            
            // Early exit if we've reached our target PC
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
        
        let lua_frame = StackFrame::lua_function(
            "test.lua".to_string(),
            "my_func".to_string(),
            42,
        );
        assert_eq!(lua_frame.format(), "test.lua:42:my_func");
        
        let lua_frame_anon = StackFrame::lua_function(
            "test.lua".to_string(),
            String::new(),
            42,
        );
        assert_eq!(lua_frame_anon.format(), "test.lua:42");
    }
}
