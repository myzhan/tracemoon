pub mod discovery;
pub mod reader;
pub mod native_stack;
pub mod skynet;
pub mod spawn;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

pub use discovery::{detect_lua_version, LuaStateDiscovery};
pub use reader::{ProcessMemoryReader, read_struct, read_pointer, read_string};
pub use native_stack::NativeFrame;
pub use skynet::SkynetDiscovery;
pub use spawn::SpawnedChild;

use crate::error::Result;
use remoteprocess::Pid;

/// Create a platform-specific process memory reader
pub fn create_reader(pid: Pid) -> Result<Box<dyn ProcessMemoryReader>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::LinuxMemoryReader::new(pid)?))
    }
    
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacOSMemoryReader::new(pid)?))
    }
    
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(crate::error::TracemoonError::Other(
            "Unsupported platform".to_string()
        ))
    }
}

/// Get native stack frames for a process
pub fn get_native_frames(pid: Pid, reader: &dyn ProcessMemoryReader) -> Result<Vec<NativeFrame>> {
    #[cfg(target_os = "macos")]
    {
        native_stack::macos::get_native_frames(nix::unistd::Pid::from_raw(pid), reader)
    }
    
    #[cfg(target_os = "linux")]
    {
        native_stack::linux::get_native_frames(nix::unistd::Pid::from_raw(pid), reader)
    }
    
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Ok(Vec::new())
    }
}
