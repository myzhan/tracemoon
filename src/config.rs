//! Command-line configuration

use clap::{crate_name, crate_version, Arg, Command};
use log::info;
use remoteprocess::Pid;

/// Default sample rate in Hz
pub const DEFAULT_SAMPLE_RATE: u32 = 100;

/// Default duration (0 = run until Ctrl+C)
pub const DEFAULT_DURATION: u64 = 0;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// PID of the target Lua process
    pub pid: Option<Pid>,
    
    /// Sample rate in Hz (samples per second)
    pub sample_rate: u32,
    
    /// Duration in seconds (0 = run until Ctrl+C)
    pub duration: u64,
    
    /// Output file path (None = stdout)
    pub output_file: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pid: None,
            sample_rate: DEFAULT_SAMPLE_RATE,
            duration: DEFAULT_DURATION,
            output_file: None,
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
        let pid_arg = Arg::new("pid")
            .short('p')
            .long("pid")
            .value_name("PID")
            .help("PID of a running Lua process")
            .required(true)
            .takes_value(true);

        let rate_arg = Arg::new("rate")
            .short('r')
            .long("rate")
            .value_name("HZ")
            .help("Sample rate in Hz (samples per second)")
            .default_value("100")
            .takes_value(true);

        let duration_arg = Arg::new("duration")
            .short('d')
            .long("duration")
            .value_name("SECONDS")
            .help("Duration in seconds (0 = run until Ctrl+C)")
            .default_value("0")
            .takes_value(true);

        let output_arg = Arg::new("output")
            .short('o')
            .long("output")
            .value_name("FILE")
            .help("Output file path (.svg for flamegraph, .txt for collapsed stacks)")
            .default_value("flamegraph.svg")
            .takes_value(true);

        let record_cmd = Command::new("record")
            .about("Record a Lua profile")
            .arg(pid_arg)
            .arg(rate_arg)
            .arg(duration_arg)
            .arg(output_arg);

        let app = Command::new(crate_name!())
            .version(crate_version!())
            .about("A sampling profiler for Lua 5.4")
            .subcommand_required(true)
            .subcommand(record_cmd);

        let matches = app.try_get_matches_from(args)?;

        let mut config = Config::default();

        if let Some(("record", sub_matches)) = matches.subcommand() {
            // Parse PID (required)
            if let Some(pid_str) = sub_matches.value_of("pid") {
                config.pid = Some(
                    pid_str
                        .parse()
                        .expect("Invalid PID: must be a positive integer"),
                );
            }

            // Parse sample rate
            if let Some(rate_str) = sub_matches.value_of("rate") {
                config.sample_rate = rate_str
                    .parse()
                    .expect("Invalid rate: must be a positive integer");
                
                if config.sample_rate == 0 {
                    config.sample_rate = DEFAULT_SAMPLE_RATE;
                }
            }

            // Parse duration
            if let Some(duration_str) = sub_matches.value_of("duration") {
                config.duration = duration_str
                    .parse()
                    .expect("Invalid duration: must be a non-negative integer");
            }

            // Parse output file
            config.output_file = sub_matches.value_of("output").map(String::from);
        }

        info!("Configuration: {:?}", config);
        Ok(config)
    }
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
}
