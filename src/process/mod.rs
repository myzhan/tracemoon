pub mod discovery;
pub mod native_stack;
pub mod reader;
pub mod skynet;
pub mod spawn;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

pub use discovery::{detect_lua_version, LuaStateDiscovery};
pub use native_stack::NativeUnwinder;
pub use reader::{read_string, read_struct, ProcessMemoryReader};
pub use skynet::SkynetDiscovery;
pub use spawn::SpawnedChild;

use crate::error::Result;
use remoteprocess::Pid;

/// Cheap "is this pid still alive" probe.
///
/// Attach mode has no `waitpid` to lean on (the target is not our child), so this is how the
/// sampling loop notices that the process it is profiling has gone away.
pub fn process_exists(pid: Pid) -> bool {
    if pid <= 0 {
        return false;
    }

    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{}", pid)).exists()
    }

    #[cfg(target_os = "macos")]
    {
        // Signal 0 performs the permission/existence checks without delivering anything.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        true
    }
}

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
            "Unsupported platform".to_string(),
        ))
    }
}
