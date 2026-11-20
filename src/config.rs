//! Command-line configuration

use clap::{crate_name, crate_version, Arg, Command};
use log::info;
use remoteprocess::Pid;

use crate::lua::LuaVersion;

/// Default sample rate in Hz
pub const DEFAULT_SAMPLE_RATE: u32 = 100;

/// Default duration (0 = run until Ctrl+C)
pub const DEFAULT_DURATION: u64 = 0;

/// Output mode selected by the subcommand.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RunMode {
    Memory,
    /// Sample for a duration and write a flamegraph / collapsed file.
    Record,
    /// Print a one-shot snapshot of the current stack(s) and exit.
    Dump,
    /// Live-updating terminal view of the hottest functions.
    Top,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub memory: Option<crate::memory::MemoryCommand>,
    /// Output mode (record / dump / top).
    pub mode: RunMode,

    /// PID of the target Lua process
    pub pid: Option<Pid>,

    /// Sample rate in Hz (samples per second)
    pub sample_rate: u32,

    /// Duration in seconds (0 = run until Ctrl+C)
    pub duration: u64,

    /// Output file path (None = stdout)
    pub output_file: Option<String>,

    /// Force a Lua version/layout (None = auto-detect).
    pub lua_version: Option<LuaVersion>,

    /// Command to spawn and profile (after `--`). Mutually exclusive with `pid`.
    pub command: Option<Vec<String>>,

    /// Exact skynet service labels or :xxxxxxxx handles; empty selects all services.
    pub services: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            memory: None,
            mode: RunMode::Record,
            pid: None,
            sample_rate: DEFAULT_SAMPLE_RATE,
            duration: DEFAULT_DURATION,
            output_file: None,
            lua_version: None,
            command: None,
            services: Vec::new(),
        }
    }
}

impl Config {
    /// Parse configuration from command line
    pub fn from_commandline() -> Config {
        let args: Vec<String> = std::env::args().collect();
        Config::from_args(&args).unwrap_or_else(|e| e.exit())
    }

    /// Parse configuration from argument list
    pub fn from_args(args: &[String]) -> clap::Result<Config> {
        let record_cmd = Command::new("record")
            .about("Record a Lua profile to a flamegraph / collapsed file")
            .args(shared_args())
            .arg(
                Arg::new("output")
                    .short('o')
                    .long("output")
                    .value_name("FILE")
                    .help("Output file path (.svg for flamegraph, .txt for collapsed stacks)")
                    .default_value("flamegraph.svg")
                    .takes_value(true),
            );

        let dump_cmd = Command::new("dump")
            .about("Print a one-shot snapshot of the current stack(s) and exit")
            .arg(pid_arg())
            .arg(lua_version_arg())
            .arg(service_arg())
            .arg(command_arg());

        let top_cmd = Command::new("top")
            .about("Live-updating terminal view of the hottest functions")
            .args(shared_args());

        let app = Command::new(crate_name!())
            .version(crate_version!())
            .about("A sampling profiler for Lua (5.4 / 5.5, incl. skynet multi-VM)")
            .subcommand_required(true)
            .subcommand(crate::memory::cli())
            .subcommand(record_cmd)
            .subcommand(dump_cmd)
            .subcommand(top_cmd);

        let matches = app.try_get_matches_from(args)?;

        let mut config = Config::default();
        if let Some(("mem", args)) = matches.subcommand() {
            config.mode = RunMode::Memory;
            config.memory = Some(crate::memory::parse(args)?);
            return Ok(config);
        }

        let (mode, sub_matches) = match matches.subcommand() {
            Some(("record", m)) => (RunMode::Record, m),
            Some(("dump", m)) => (RunMode::Dump, m),
            Some(("top", m)) => (RunMode::Top, m),
            _ => return Ok(config),
        };
        config.mode = mode;
        config.services = sub_matches
            .values_of("service")
            .map(|values| values.map(String::from).collect())
            .unwrap_or_default();

        // PID (optional; attach path)
        if let Some(pid_str) = sub_matches.value_of("pid") {
            let pid: i32 = pid_str.parse().map_err(|_| {
                clap::Error::raw(
                    clap::ErrorKind::InvalidValue,
                    format!("Invalid PID '{}': must be a positive integer\n", pid_str),
                )
            })?;
            // `parse::<i32>()` happily accepts `-1`; a non-positive pid is never a target
            // (and `kill(0, …)`/`kill(-1, …)` mean "every process in the group").
            if pid <= 0 {
                return Err(clap::Error::raw(
                    clap::ErrorKind::InvalidValue,
                    format!("Invalid PID '{}': must be a positive integer\n", pid_str),
                ));
            }
            config.pid = Some(pid);
        }

        // Sample rate (record/top only; dump has no rate arg)
        if mode != RunMode::Dump {
            if let Some(rate_str) = sub_matches.value_of("rate") {
                let rate: u32 = rate_str.parse().map_err(|_| {
                    clap::Error::raw(
                        clap::ErrorKind::InvalidValue,
                        format!("Invalid rate '{}': must be a positive integer\n", rate_str),
                    )
                })?;
                config.sample_rate = if rate == 0 { DEFAULT_SAMPLE_RATE } else { rate };
            }
        }

        // Duration (record/top only)
        if mode != RunMode::Dump {
            if let Some(duration_str) = sub_matches.value_of("duration") {
                config.duration = duration_str.parse().map_err(|_| {
                    clap::Error::raw(
                        clap::ErrorKind::InvalidValue,
                        format!(
                            "Invalid duration '{}': must be a non-negative integer\n",
                            duration_str
                        ),
                    )
                })?;
            }
        }

        // Output file (record only)
        if mode == RunMode::Record {
            config.output_file = sub_matches.value_of("output").map(String::from);
        }

        // Optional Lua version override
        if let Some(v) = sub_matches.value_of("lua-version") {
            let parsed = v.parse().map_err(|_| {
                clap::Error::raw(
                    clap::ErrorKind::InvalidValue,
                    format!("Invalid --lua-version '{}': expected 5.4 or 5.5\n", v),
                )
            })?;
            config.lua_version = Some(parsed);
        }

        // Trailing spawn command (after `--`)
        if let Some(values) = sub_matches.values_of("command") {
            let argv: Vec<String> = values.map(String::from).collect();
            if !argv.is_empty() {
                config.command = Some(argv);
            }
        }

        info!("Configuration: {:?}", config);
        Ok(config)
    }
}

fn pid_arg() -> Arg<'static> {
    Arg::new("pid")
        .short('p')
        .long("pid")
        .value_name("PID")
        .help("Attach to a running Lua process by PID (mutually exclusive with -- <cmd>)")
        .takes_value(true)
}

fn lua_version_arg() -> Arg<'static> {
    Arg::new("lua-version")
        .long("lua-version")
        .value_name("VERSION")
        .help("Force the Lua version layout (5.4 or 5.5). Default: auto-detect")
        .takes_value(true)
}

fn command_arg() -> Arg<'static> {
    Arg::new("command")
        .value_name("COMMAND")
        .help("Command to spawn and profile (after --). Mutually exclusive with -p")
        .last(true)
        .multiple_values(true)
        .takes_value(true)
}

fn service_arg() -> Arg<'static> {
    Arg::new("service")
        .long("service")
        .value_name("NAME_OR_HANDLE")
        .help("Select a skynet service by exact label or :xxxxxxxx handle (repeatable)")
        .takes_value(true)
        .multiple_occurrences(true)
}

/// Args shared by `record` and `top` (dump uses a narrower subset).
fn shared_args() -> Vec<Arg<'static>> {
    vec![
        pid_arg(),
        Arg::new("rate")
            .short('r')
            .long("rate")
            .value_name("HZ")
            .help("Sample rate in Hz (samples per second)")
            .default_value("100")
            .takes_value(true),
        Arg::new("duration")
            .short('d')
            .long("duration")
            .value_name("SECONDS")
            .help("Duration in seconds (0 = run until Ctrl+C)")
            .default_value("0")
            .takes_value(true),
        lua_version_arg(),
        service_arg(),
        command_arg(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.pid, None);
        assert_eq!(config.sample_rate, 100);
        assert_eq!(config.duration, 0);
        assert_eq!(config.output_file, None);
    }

    #[test]
    fn service_filters_are_repeatable_in_every_mode() {
        for mode in ["record", "top", "dump"] {
            let args = [
                "tracemoon",
                mode,
                "-p",
                "123",
                "--service",
                ".logger",
                "--service",
                ":01000012",
            ]
            .map(String::from);
            let config = Config::from_args(&args).unwrap();
            assert_eq!(config.services, [".logger", ":01000012"]);
        }
    }

    #[test]
    fn test_parse_record_command() {
        let args = vec![
            "tracemoon".to_string(),
            "record".to_string(),
            "-p".to_string(),
            "12345".to_string(),
            "-r".to_string(),
            "200".to_string(),
            "-d".to_string(),
            "30".to_string(),
            "-o".to_string(),
            "output.txt".to_string(),
        ];

        let config = Config::from_args(&args).unwrap();
        assert_eq!(config.pid, Some(12345));
        assert_eq!(config.sample_rate, 200);
        assert_eq!(config.duration, 30);
        assert_eq!(config.output_file, Some("output.txt".to_string()));
    }

    #[test]
    fn test_parse_minimal_command() {
        let args = vec![
            "tracemoon".to_string(),
            "record".to_string(),
            "-p".to_string(),
            "1234".to_string(),
        ];

        let config = Config::from_args(&args).unwrap();
        assert_eq!(config.pid, Some(1234));
        assert_eq!(config.sample_rate, DEFAULT_SAMPLE_RATE);
        assert_eq!(config.duration, DEFAULT_DURATION);
        assert_eq!(config.output_file, Some("flamegraph.svg".to_string()));
    }

    #[test]
    fn test_parse_spawn_command() {
        let args = vec![
            "tracemoon".to_string(),
            "record".to_string(),
            "-d".to_string(),
            "5".to_string(),
            "--".to_string(),
            "./foo".to_string(),
            "bar".to_string(),
            "baz".to_string(),
        ];

        let config = Config::from_args(&args).unwrap();
        assert_eq!(config.pid, None);
        assert_eq!(config.duration, 5);
        assert_eq!(
            config.command,
            Some(vec![
                "./foo".to_string(),
                "bar".to_string(),
                "baz".to_string()
            ])
        );
    }

    #[test]
    fn test_parse_attach_has_no_command() {
        let args = vec![
            "tracemoon".to_string(),
            "record".to_string(),
            "-p".to_string(),
            "5".to_string(),
        ];

        let config = Config::from_args(&args).unwrap();
        assert_eq!(config.pid, Some(5));
        assert_eq!(config.command, None);
    }

    /// `0` and negative pids are never valid targets (`kill(0, …)` signals the whole
    /// process group), so the CLI must reject them instead of profiling something else.
    #[test]
    fn test_rejects_non_positive_pid() {
        for pid in ["0", "-7"] {
            let args = vec![
                "tracemoon".to_string(),
                "record".to_string(),
                "-p".to_string(),
                pid.to_string(),
            ];

            let err = Config::from_args(&args).unwrap_err();
            assert!(
                err.kind() == clap::ErrorKind::InvalidValue
                    || err.kind() == clap::ErrorKind::UnknownArgument,
                "unexpected error for pid {pid}: {err:?}"
            );
        }
    }

    #[test]
    fn test_record_mode_default() {
        let args = vec![
            "tracemoon".to_string(),
            "record".to_string(),
            "-p".to_string(),
            "5".to_string(),
        ];
        let config = Config::from_args(&args).unwrap();
        assert_eq!(config.mode, RunMode::Record);
    }

    #[test]
    fn test_parse_dump_attach() {
        let args = vec![
            "tracemoon".to_string(),
            "dump".to_string(),
            "-p".to_string(),
            "1234".to_string(),
        ];
        let config = Config::from_args(&args).unwrap();
        assert_eq!(config.mode, RunMode::Dump);
        assert_eq!(config.pid, Some(1234));
        assert_eq!(config.command, None);
    }

    #[test]
    fn test_parse_dump_spawn() {
        let args = vec![
            "tracemoon".to_string(),
            "dump".to_string(),
            "--".to_string(),
            "lua".to_string(),
            "x.lua".to_string(),
        ];
        let config = Config::from_args(&args).unwrap();
        assert_eq!(config.mode, RunMode::Dump);
        assert_eq!(config.pid, None);
        assert_eq!(
            config.command,
            Some(vec!["lua".to_string(), "x.lua".to_string()])
        );
    }

    #[test]
    fn test_parse_top_spawn() {
        let args = vec![
            "tracemoon".to_string(),
            "top".to_string(),
            "-r".to_string(),
            "250".to_string(),
            "--".to_string(),
            "lua".to_string(),
            "x.lua".to_string(),
        ];
        let config = Config::from_args(&args).unwrap();
        assert_eq!(config.mode, RunMode::Top);
        assert_eq!(config.pid, None);
        assert_eq!(config.sample_rate, 250);
        assert_eq!(
            config.command,
            Some(vec!["lua".to_string(), "x.lua".to_string()])
        );
    }

    #[test]
    fn test_parse_top_attach() {
        let args = vec![
            "tracemoon".to_string(),
            "top".to_string(),
            "-p".to_string(),
            "9".to_string(),
        ];
        let config = Config::from_args(&args).unwrap();
        assert_eq!(config.mode, RunMode::Top);
        assert_eq!(config.pid, Some(9));
    }
}
