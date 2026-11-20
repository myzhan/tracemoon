//! Single heap captures and portable offline ownership reports.

mod analyze;
mod capture;
mod core;
mod html;
mod layout;
mod snapshot;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Arg, ArgMatches, Command};

use crate::error::{Result, TracemoonError};
use crate::lua::LuaVersion;
use crate::process::{
    create_reader, detect_lua_version, freeze::FrozenProcess, SkynetDiscovery, SpawnedChild,
};

#[derive(Debug, Clone, PartialEq)]
pub enum MemoryCommand {
    Capture(CaptureOptions),
    Report {
        input: PathBuf,
        core: core::Options,
        output: Option<PathBuf>,
        top: usize,
        object: Option<u64>,
        show_addresses: bool,
        format: ReportFormat,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    Text,
    Html,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaptureOptions {
    pub pid: Option<i32>,
    pub command: Option<Vec<String>>,
    pub output: PathBuf,
    pub services: Vec<String>,
    pub version: Option<LuaVersion>,
    pub state: Option<usize>,
    pub max_pause_ms: u64,
    pub max_objects: usize,
    pub max_edges: usize,
    pub max_read_mib: u64,
    pub startup_ms: u64,
}

fn option(name: &'static str, default: &'static str, help: &'static str) -> Arg<'static> {
    Arg::new(name)
        .long(name)
        .takes_value(true)
        .default_value(default)
        .help(help)
}

pub fn cli() -> Command<'static> {
    let capture = Command::new("capture")
        .about("Pause a Linux Lua 5.4 process and save a heap graph")
        .arg(
            Arg::new("pid")
                .short('p')
                .long("pid")
                .takes_value(true)
                .conflicts_with("command")
                .required_unless_present("command"),
        )
        .arg(
            Arg::new("command")
                .last(true)
                .multiple_values(true)
                .takes_value(true),
        )
        .arg(
            Arg::new("output")
                .short('o')
                .long("output")
                .takes_value(true)
                .default_value("heap.tmoon"),
        )
        .arg(
            Arg::new("service")
                .long("service")
                .takes_value(true)
                .multiple_occurrences(true),
        )
        .arg(
            Arg::new("lua-version")
                .long("lua-version")
                .takes_value(true)
                .possible_values(["5.4"])
                .help("Force the standard Lua 5.4 heap ABI; otherwise detect"),
        )
        .arg(
            Arg::new("state")
                .long("state")
                .takes_value(true)
                .conflicts_with("service")
                .help("Known lua_State address in hex; bypass single-VM discovery"),
        )
        .arg(option(
            "max-pause-ms",
            "2000",
            "Best-effort freeze and capture budget (cleanup may take longer)",
        ))
        .arg(option(
            "max-objects",
            "1000000",
            "Maximum GC objects across all selected heaps",
        ))
        .arg(option(
            "max-edges",
            "2000000",
            "Maximum captured object references",
        ))
        .arg(option(
            "max-read-mib",
            "256",
            "Remote read-byte budget, including discovery",
        ))
        .arg(option(
            "startup-ms",
            "200",
            "Time for a spawned child to initialize before capture",
        ));
    let report = Command::new("report")
        .about("Analyze a heap snapshot or Linux ELF core without accessing the target")
        .arg(Arg::new("input").required(true))
        .arg(
            Arg::new("exe")
                .long("exe")
                .takes_value(true)
                .help("Matching ELF executable for a core (skynet requires symbols including H)"),
        )
        .arg(
            Arg::new("lua-version")
                .long("lua-version")
                .takes_value(true)
                .possible_values(["5.4"])
                .help("Verified core Lua version when automatic detection is unavailable"),
        )
        .arg(
            Arg::new("state")
                .long("state")
                .takes_value(true)
                .conflicts_with("service")
                .help("Known lua_State address in a single-VM core"),
        )
        .arg(
            Arg::new("service")
                .long("service")
                .takes_value(true)
                .multiple_occurrences(true)
                .help("Select a skynet service in a core"),
        )
        .arg(
            Arg::new("format")
                .long("format")
                .takes_value(true)
                .possible_values(["text", "html"])
                .help("Report format (default: html for .html/.htm output, otherwise text)"),
        )
        .arg(
            Arg::new("show-addresses")
                .long("show-addresses")
                .help("Include raw addresses for debugging and exact object inspection"),
        )
        .arg(
            Arg::new("output")
                .short('o')
                .long("output")
                .takes_value(true),
        )
        .arg(option(
            "top",
            "10",
            "Maximum large objects shown per heap (1-100)",
        ));
    let inspect = Command::new("inspect")
        .about("Show a captured object's holding paths and preview")
        .arg(Arg::new("input").required(true))
        .arg(Arg::new("exe").long("exe").takes_value(true))
        .arg(
            Arg::new("lua-version")
                .long("lua-version")
                .takes_value(true)
                .possible_values(["5.4"]),
        )
        .arg(Arg::new("state").long("state").takes_value(true))
        .arg(
            Arg::new("show-addresses")
                .long("show-addresses")
                .help("Include raw addresses for debugging and exact object inspection"),
        )
        .arg(
            Arg::new("object")
                .long("object")
                .takes_value(true)
                .required(true),
        );
    Command::new("mem")
        .about("Capture and inspect Lua heap ownership")
        .subcommand_required(true)
        .subcommand(capture)
        .subcommand(report)
        .subcommand(inspect)
}

pub fn parse(args: &ArgMatches) -> clap::Result<MemoryCommand> {
    fn invalid(message: &str) -> clap::Error {
        clap::Error::raw(clap::ErrorKind::InvalidValue, message)
    }
    fn number(args: &ArgMatches, name: &str, max: u64) -> clap::Result<u64> {
        let value = args
            .value_of(name)
            .unwrap_or("")
            .parse::<u64>()
            .map_err(|_| invalid(&format!("Invalid --{name}")))?;
        if value == 0 || value > max {
            return Err(invalid(&format!("--{name} must be between 1 and {max}")));
        }
        Ok(value)
    }
    fn address(text: &str) -> clap::Result<u64> {
        let address = u64::from_str_radix(text.strip_prefix("0x").unwrap_or(text), 16)
            .map_err(|_| invalid("Invalid hex address"))?;
        if !(0x1000..=0x00ff_ffff_ffff_ffff).contains(&address) {
            return Err(invalid("Invalid user-space address"));
        }
        Ok(address)
    }
    let (command, args) = args
        .subcommand()
        .ok_or_else(|| invalid("Expected a mem subcommand"))?;
    if command == "capture" {
        let pid = args
            .value_of("pid")
            .map(|s| {
                s.parse::<i32>()
                    .map_err(|_| invalid("PID must be positive"))
            })
            .transpose()?;
        if pid.is_some_and(|pid| pid <= 0) {
            return Err(invalid("PID must be positive"));
        }
        let version = args
            .value_of("lua-version")
            .map(|v| {
                v.parse::<LuaVersion>()
                    .map_err(|_| invalid("Invalid Lua version"))
            })
            .transpose()?;
        if version == Some(LuaVersion::Lua55) {
            return Err(invalid("Heap capture currently supports Lua 5.4 only"));
        }
        return Ok(MemoryCommand::Capture(CaptureOptions {
            pid,
            command: args
                .values_of("command")
                .map(|v| v.map(String::from).collect()),
            output: PathBuf::from(args.value_of("output").unwrap_or("heap.tmoon")),
            services: args
                .values_of("service")
                .map(|v| v.map(String::from).collect())
                .unwrap_or_default(),
            version,
            state: args
                .value_of("state")
                .map(address)
                .transpose()?
                .map(|a| a as usize),
            max_pause_ms: number(args, "max-pause-ms", 60_000)?,
            max_objects: number(args, "max-objects", 4_000_000)? as usize,
            max_edges: number(args, "max-edges", 8_000_000)? as usize,
            max_read_mib: number(args, "max-read-mib", 4096)?,
            startup_ms: number(args, "startup-ms", 60_000)?,
        }));
    }
    let output = if command == "report" {
        args.value_of("output").map(PathBuf::from)
    } else {
        None
    };
    let format = if command == "report" {
        match args.value_of("format") {
            Some("html") => ReportFormat::Html,
            Some("text") => ReportFormat::Text,
            _ if output
                .as_ref()
                .and_then(|path| path.extension())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm")
                }) =>
            {
                ReportFormat::Html
            }
            _ => ReportFormat::Text,
        }
    } else {
        ReportFormat::Text
    };
    Ok(MemoryCommand::Report {
        core: core::Options {
            exe: args.value_of("exe").map(PathBuf::from),
            lua54: args.is_present("lua-version"),
            state: args
                .value_of("state")
                .map(address)
                .transpose()?
                .map(|a| a as usize),
            services: if command == "report" {
                args.values_of("service")
                    .map(|v| v.map(String::from).collect())
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
        },
        format,
        show_addresses: args.is_present("show-addresses"),
        input: PathBuf::from(args.value_of("input").unwrap_or_default()),
        output,
        top: if command == "report" {
            number(args, "top", 100)? as usize
        } else {
            1
        },
        object: if command == "inspect" {
            Some(address(args.value_of("object").unwrap_or_default())?)
        } else {
            None
        },
    })
}

pub fn run(command: MemoryCommand) -> Result<()> {
    match command {
        MemoryCommand::Capture(options) => capture_process(options),
        MemoryCommand::Report {
            input,
            core,
            output,
            top,
            object,
            show_addresses,
            format,
        } => {
            let mut snapshot = if core::is_core(&input)? {
                core::read(&input, &core)?
            } else {
                if core != core::Options::default() {
                    return Err(TracemoonError::Config(
                        "--exe, --lua-version, --state and --service apply only to ELF core input"
                            .into(),
                    ));
                }
                snapshot::Snapshot::read(&input)?
            };
            if object.is_some_and(|address| {
                !snapshot
                    .heaps
                    .iter()
                    .any(|heap| heap.objects.iter().any(|o| o.address == address))
            }) {
                return Err(TracemoonError::Config(
                    "Object address is not present in this snapshot".into(),
                ));
            }
            analyze::classify(&mut snapshot);
            let report = match format {
                ReportFormat::Text => {
                    analyze::report_with_addresses(&snapshot, top, object, show_addresses)
                }
                ReportFormat::Html => html::report(&snapshot, top, show_addresses),
            };
            if let Some(output) = output {
                std::fs::write(output, report)?;
            } else {
                print!("{report}");
            }
            Ok(())
        }
    }
}

fn capture_process(options: CaptureOptions) -> Result<()> {
    if !cfg!(all(target_os = "linux", target_pointer_width = "64")) {
        return Err(TracemoonError::Config(
            "Heap capture requires 64-bit Linux; offline report/inspect work on macOS too".into(),
        ));
    }
    let parent = options
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    if !parent.is_dir() {
        return Err(TracemoonError::Config(
            "Snapshot output directory does not exist".into(),
        ));
    }
    let running = Arc::new(AtomicBool::new(true));
    let signal = running.clone();
    ctrlc::set_handler(move || {
        signal.store(false, Ordering::Relaxed);
    })
    .map_err(|e| TracemoonError::Other(format!("Install capture signal handler: {e}")))?;
    let child = options
        .command
        .as_ref()
        .map(|argv| SpawnedChild::spawn(argv))
        .transpose()?;
    let pid = match (&child, options.pid) {
        (Some(child), None) => {
            child.resume()?;
            std::thread::sleep(Duration::from_millis(options.startup_ms));
            child.pid()
        }
        (None, Some(pid)) => pid,
        _ => return Err(TracemoonError::Config("Specify exactly one target".into())),
    };
    let version = options.version.or_else(|| detect_lua_version(pid));
    if version != Some(LuaVersion::Lua54) {
        return Err(TracemoonError::Config(
            "Heap capture requires Lua 5.4; use --lua-version 5.4 only for a verified 5.4 target"
                .into(),
        ));
    }
    let h = SkynetDiscovery::resolve_h(pid);
    if h.is_some() && options.state.is_some() {
        return Err(TracemoonError::Config(
            "--state is only for single-VM targets".into(),
        ));
    }
    let inner = create_reader(pid)?;
    let start = Instant::now();
    let deadline = start + Duration::from_millis(options.max_pause_ms);
    let frozen = FrozenProcess::new(pid, deadline)?;
    let reader = capture::BudgetReader {
        inner,
        deadline,
        max_bytes: options.max_read_mib * 1024 * 1024,
        bytes: AtomicU64::new(0),
        running,
    };
    let heaps = capture::discover(&reader, pid, h, &options.services, options.state);
    let mut snapshot = snapshot::Snapshot {
        schema_version: 1,
        tool_version: env!("CARGO_PKG_VERSION").into(),
        lua_version: "5.4".into(),
        lua_abi: if h.is_some() { "skynet-lua54" } else { "lua54" }.into(),
        capture_method: Some("external all-thread freeze".into()),
        pid,
        captured_unix_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        pause_ms: 0,
        read_bytes: 0,
        limits: snapshot::CaptureLimits {
            pause_ms: options.max_pause_ms,
            objects: options.max_objects,
            edges: options.max_edges,
            read_bytes: options.max_read_mib * 1024 * 1024,
        },
        pointer_width: 64,
        target_endian: if cfg!(target_endian = "little") {
            "little"
        } else {
            "big"
        }
        .into(),
        incomplete: false,
        diagnostics: Vec::new(),
        heaps: Vec::new(),
    };
    match heaps {
        Ok(heaps) => capture::collect(
            &reader,
            heaps,
            &mut snapshot,
            options.max_objects,
            options.max_edges,
            if h.is_some() {
                layout::HeapLayout::skynet54()
            } else {
                layout::HeapLayout::lua54()
            },
        ),
        Err(e) => {
            snapshot.incomplete = true;
            snapshot.diagnostics.push(format!("VM discovery: {e}"));
        }
    }
    // No target reads, graph analysis, or disk writes after this point.
    if let Err(e) = frozen.resume() {
        snapshot.incomplete = true;
        snapshot.diagnostics.push(format!("Target resume: {e}"));
    }
    snapshot.pause_ms = start.elapsed().as_millis() as u64;
    snapshot.read_bytes = reader.bytes.load(Ordering::Relaxed);
    if let Some(child) = &child {
        child.terminate();
    }
    analyze::classify(&mut snapshot);
    snapshot.write(&options.output)?;
    print!("{}", analyze::report(&snapshot, 10, None));
    eprintln!("Heap graph saved to {}", options.output.display());
    if snapshot.incomplete {
        return Err(TracemoonError::Other(
            "Incomplete heap capture saved; see snapshot diagnostics".into(),
        ));
    }
    Ok(())
}
