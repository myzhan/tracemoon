//! tracemoon - A sampling profiler for Lua (5.4 / 5.5)
//!
//! Similar to py-spy, tracemoon attaches to a running Lua process and
//! samples the call stack periodically to generate a profile. It supports
//! plain single-VM Lua as well as skynet processes hosting many Lua VMs.

#[macro_use]
extern crate log;

mod config;
mod error;
mod lua;
mod output;
mod process;
mod sampler;
mod symbols;

use std::process::ExitCode;

use config::{Config, RunMode};
use error::TracemoonError;
use sampler::{run_dump, run_profiler, run_profiler_spawn, run_top};

fn check_permissions() {
    #[cfg(target_os = "macos")]
    {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("Warning: tracemoon may require root privileges on macOS.");
            eprintln!("If you encounter permission errors, try running with sudo.");
            eprintln!();
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Check if we can ptrace
        let status = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope");
        if let Ok(scope) = status {
            let scope = scope.trim();
            if scope != "0" {
                eprintln!("Warning: ptrace_scope is set to {} (restricted).", scope);
                eprintln!("You may need to run with sudo or set ptrace_scope to 0:");
                eprintln!("  sudo sysctl kernel.yama.ptrace_scope=0");
                eprintln!();
            }
        }
    }
}

fn run() -> Result<(), TracemoonError> {
    let config = Config::from_commandline();

    // dump/top validate the target internally (via build_target); record keeps its
    // existing attach-vs-spawn split so spawn owns the child lifecycle.
    match config.mode {
        RunMode::Dump => return run_dump(config.pid, config.command.clone(), config),
        RunMode::Top => return run_top(config.pid, config.command.clone(), config),
        RunMode::Record => {}
    }

    match (config.pid, config.command.clone()) {
        (Some(_), Some(_)) => {
            return Err(TracemoonError::Config(
                "Specify either -p <PID> to attach or a command after -- to spawn, not both."
                    .to_string(),
            ));
        }
        (Some(pid), None) => {
            info!("Starting profiler for PID {}", pid);
            run_profiler(pid, config)?;
        }
        (None, Some(argv)) => {
            info!("Spawning and profiling: {}", argv.join(" "));
            run_profiler_spawn(argv, config)?;
        }
        (None, None) => {
            return Err(TracemoonError::Config(
                "No target specified. Use -p <PID> to attach, or -- <command> to spawn."
                    .to_string(),
            ));
        }
    }

    Ok(())
}

fn main() -> ExitCode {
    // Initialize logging
    env_logger::builder()
        .format_timestamp_millis()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();

    // Check permissions and print warnings
    check_permissions();

    // Run the profiler
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(TracemoonError::PermissionDenied { pid }) => {
            eprintln!("Error: Permission denied accessing process {}.", pid);
            eprintln!();
            #[cfg(target_os = "macos")]
            eprintln!("On macOS, try running with sudo:");
            #[cfg(target_os = "linux")]
            eprintln!("On Linux, try running with sudo or adjust ptrace_scope:");
            eprintln!("  sudo tracemoon record -p {}", pid);
            ExitCode::from(1)
        }
        Err(TracemoonError::ProcessNotFound { pid }) => {
            eprintln!("Error: Process {} not found.", pid);
            eprintln!("Make sure the PID is correct and the process is running.");
            ExitCode::from(2)
        }
        Err(TracemoonError::LuaStateNotFound) => {
            eprintln!("Error: Could not find lua_State in the target process.");
            eprintln!();
            eprintln!("Possible causes:");
            eprintln!("  - The process is not running Lua");
            eprintln!("  - Lua is not currently active (no Lua code running)");
            eprintln!("  - The Lua version is not supported (only Lua 5.4 / 5.5 are supported)");
            eprintln!("  - Debug symbols are stripped and lua_State cannot be located");
            ExitCode::from(3)
        }
        Err(TracemoonError::ProcessExited) => {
            eprintln!("Note: Target process exited during profiling.");
            ExitCode::SUCCESS // This is expected behavior
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            ExitCode::from(1)
        }
    }
}
