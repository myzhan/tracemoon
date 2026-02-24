//! lua_State operations and validation

use crate::error::{Result, TracemoonError};
use crate::lua::types::{LuaState, LUA_TTHREAD};
use crate::process::{ProcessMemoryReader, read_struct};

/// Reader for lua_State structures from a remote process
pub struct LuaStateReader<'a> {
    reader: &'a dyn ProcessMemoryReader,
    lua_state_addr: usize,
}

impl<'a> LuaStateReader<'a> {
    /// Create a new LuaStateReader
    pub fn new(reader: &'a dyn ProcessMemoryReader, lua_state_addr: usize) -> Self {
        Self {
            reader,
            lua_state_addr,
        }
    }

    /// Read the lua_State structure from remote process
    pub fn read_state(&self) -> Result<LuaState> {
        read_struct(self.reader, self.lua_state_addr)
    }

    /// Validate that the address contains a valid lua_State
    pub fn validate(&self) -> Result<()> {
        let state = self.read_state()?;
        
        // Check type tag
        if state.tt != LUA_TTHREAD {
            return Err(TracemoonError::InvalidLuaState {
                addr: self.lua_state_addr,
            });
        }

        // Check status is valid (0-5)
        if state.status > 5 {
            return Err(TracemoonError::InvalidLuaState {
                addr: self.lua_state_addr,
            });
        }

        // Check ci pointer is not null
        if state.ci == 0 {
            return Err(TracemoonError::InvalidLuaState {
                addr: self.lua_state_addr,
            });
        }

        // Check stack pointer is not null
        if state.stack == 0 {
            return Err(TracemoonError::InvalidLuaState {
                addr: self.lua_state_addr,
            });
        }

        // Check stack ordering: stack < top < stack_last
        // Note: top and stack_last are offsets from stack in Lua 5.4
        if state.top < 0 || state.stack_last < state.top {
            return Err(TracemoonError::InvalidLuaState {
                addr: self.lua_state_addr,
            });
        }

        Ok(())
    }

    /// Get the current CallInfo address
    pub fn get_current_ci(&self) -> Result<usize> {
        let state = self.read_state()?;
        Ok(state.ci as usize)
    }

    /// Get the base CallInfo address (embedded in lua_State)
    pub fn get_base_ci_addr(&self) -> usize {
        // base_ci is embedded in lua_State, calculate its offset
        self.lua_state_addr + std::mem::offset_of!(LuaState, base_ci)
    }

    /// Get the stack base address
    pub fn get_stack_base(&self) -> Result<usize> {
        let state = self.read_state()?;
        Ok(state.stack as usize)
    }

    /// Get the global_State address
    pub fn get_global_state(&self) -> Result<usize> {
        let state = self.read_state()?;
        Ok(state.l_g as usize)
    }

    /// Get the lua_State address
    pub fn addr(&self) -> usize {
        self.lua_state_addr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base_ci_offset() {
        // Verify we can calculate offset correctly
        let offset = std::mem::offset_of!(LuaState, base_ci);
        println!("base_ci offset in lua_State: {}", offset);
        assert!(offset > 0);
    }
}
