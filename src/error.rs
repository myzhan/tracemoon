use thiserror::Error;

#[derive(Error, Debug)]
pub enum TracemoonError {
    #[error("Failed to read process memory at address 0x{addr:x}: {source}")]
    MemoryRead {
        addr: usize,
        #[source]
        source: std::io::Error,
    },

    #[error("Process {pid} not found or not accessible")]
    ProcessNotFound { pid: i32 },

    #[error("Permission denied accessing process {pid}. Try running with sudo")]
    PermissionDenied { pid: i32 },

    #[error("Could not find lua_State in target process")]
    LuaStateNotFound,

    #[error("Invalid lua_State structure at address 0x{addr:x}")]
    InvalidLuaState { addr: usize },

    #[error("Failed to resolve symbol: {name}")]
    SymbolResolutionFailed { name: String },

    #[error("Stack walk exceeded maximum depth ({max_depth})")]
    MaxStackDepthExceeded { max_depth: usize },

    #[error("Detected loop in CallInfo chain at address 0x{addr:x}")]
    CallInfoLoop { addr: usize },

    #[error("Target process exited during profiling")]
    ProcessExited,

    #[error("Invalid Lua version: expected 5.4, found {found}")]
    UnsupportedLuaVersion { found: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, TracemoonError>;
