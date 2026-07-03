//! Sampling coordinator for Lua profiling

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use remoteprocess::Pid;

use crate::config::{Config, RunMode};
use crate::error::{Result, TracemoonError};
use crate::lua::{LuaLayout, LuaVersion, StackFrame, StackWalker};
use crate::output::{Aggregator, CollapsedOutput};
use crate::process::{
    create_reader, detect_lua_version, LuaStateDiscovery, NativeUnwinder, ProcessMemoryReader,
    SkynetDiscovery, SpawnedChild,
};
use crate::symbols::SymbolResolver;

/// Profiling target: attach to an existing PID, or a child we spawned and control.
pub enum Target {
    /// Attach to an already-running process.
    Attach(Pid),
    /// A child process spawned by tracemoon (held at exec until sampling starts).
    Spawn(SpawnedChild),
}

impl Target {
    fn pid(&self) -> Pid {
        match self {
            Target::Attach(pid) => *pid,
            Target::Spawn(child) => child.pid(),
        }
    }
}

/// Sampling mode: a single plain-Lua VM, or a skynet process with many VMs.
enum Mode {
    /// Single VM (plain Lua). The lua_State address is discovered lazily (0 = not yet
    /// found) so spawn mode can start before the Lua VM is initialized.
    Single { addr: Cell<usize> },
    /// Skynet multi-VM; re-enumerated each tick from the `H` global.
    Skynet { h_addr: usize },
}

/// Main sampler coordinator
pub struct Sampler {
    target: Target,
    config: Config,
    reader: Box<dyn ProcessMemoryReader>,
    layout: LuaLayout,
    mode: Mode,
    /// Native C-stack unwinder (single-VM mode only; `None` in skynet mode or on
    /// unsupported platforms). Reused across ticks to avoid rebuilding the unwind state.
    native_unwinder: Option<NativeUnwinder>,
    symbol_resolver: SymbolResolver,
    aggregator: Aggregator,
    running: Arc<AtomicBool>,
    start_time: Instant,
}

impl Sampler {
    /// Create a new sampler for the given target (attach or spawned child).
    pub fn new(target: Target, config: Config) -> Result<Self> {
        let pid = target.pid();
        info!("Creating sampler for process {}", pid);

        // Create memory reader
        let reader = create_reader(pid)?;

        // Pick the Lua layout: explicit override, else auto-detect, else 5.4.
        let version = config
            .lua_version
            .or_else(|| detect_lua_version(pid))
            .unwrap_or_else(|| {
                warn!("Could not detect Lua version; defaulting to 5.4");
                LuaVersion::Lua54
            });
        info!("Using Lua {} memory layout", version);
        let layout = LuaLayout::for_version(version);

        // Choose mode: skynet multi-VM if the `H` symbol resolves, else single.
        // Single-VM discovery is deferred to the first sample tick (the lua_State may not
        // exist yet in spawn mode, where the child is frozen at exec).
        let mode = if let Some(h_addr) = SkynetDiscovery::resolve_h(pid) {
            info!("skynet mode: H global at 0x{:x}", h_addr);
            Mode::Skynet { h_addr }
        } else {
            info!("single-VM mode: lua_State will be discovered on first sample");
            Mode::Single { addr: Cell::new(0) }
        };

        // Native C-stack unwinder is meaningful only for a single VM: a ptrace register
        // snapshot maps to one worker thread, not to a specific skynet service.
        let native_unwinder = match &mode {
            Mode::Single { .. } => NativeUnwinder::new(pid),
            Mode::Skynet { .. } => None,
        };

        // Create symbol resolver
        let symbol_resolver = SymbolResolver::new(pid);
        symbol_resolver.preload();

        Ok(Self {
            target,
            config,
            reader,
            layout,
            mode,
            native_unwinder,
            symbol_resolver,
            aggregator: Aggregator::new(),
            running: Arc::new(AtomicBool::new(true)),
            start_time: Instant::now(),
        })
    }

    fn pid(&self) -> Pid {
        self.target.pid()
    }

    /// Get a handle to stop the sampler
    pub fn get_stop_handle(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    /// Run the sampling loop
    pub fn run(&mut self) -> Result<()> {
        info!(
            "Starting sampling at {} Hz for {} seconds",
            self.config.sample_rate,
            if self.config.duration == 0 {
                "unlimited".to_string()
            } else {
                self.config.duration.to_string()
            }
        );

        // For spawned children: resume the child right before sampling begins, so we
        // capture from the very start of its execution.
        if let Target::Spawn(child) = &self.target {
            child.resume()?;
        }

        self.start_time = Instant::now();
        let sample_interval = Duration::from_secs_f64(1.0 / self.config.sample_rate as f64);
        let duration_limit = if self.config.duration > 0 {
            Some(Duration::from_secs(self.config.duration))
        } else {
            None
        };

        let mut consecutive_failures = 0;
        const MAX_CONSECUTIVE_FAILURES: u32 = 10;

        // Live `top` view refreshes on this cadence.
        let render_interval = Duration::from_secs(1);
        let mut last_render = Instant::now() - render_interval;

        while self.running.load(Ordering::Relaxed) {
            let sample_start = Instant::now();

            // Check duration limit
            if let Some(limit) = duration_limit {
                if self.start_time.elapsed() >= limit {
                    info!("Duration limit reached");
                    break;
                }
            }

            // For spawned children: stop if the child has exited on its own.
            if let Target::Spawn(child) = &self.target {
                if child.has_exited() {
                    info!("Spawned child exited");
                    break;
                }
            }

            // Take a sample
            match self.sample() {
                Ok(stacks) => {
                    if stacks.is_empty() {
                        self.aggregator.add_failed();
                    } else {
                        for frames in stacks {
                            self.aggregator.add(frames);
                        }
                    }
                    consecutive_failures = 0;
                }
                Err(TracemoonError::ProcessExited) => {
                    warn!("Target process exited");
                    break;
                }
                Err(e) => {
                    debug!("Sample failed: {:?}", e);
                    self.aggregator.add_failed();
                    consecutive_failures += 1;

                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        warn!(
                            "Too many consecutive failures ({}), continuing anyway",
                            consecutive_failures
                        );
                        consecutive_failures = 0;
                    }
                }
            }

            // Live `top` rendering.
            if self.config.mode == RunMode::Top && last_render.elapsed() >= render_interval {
                self.render_top();
                last_render = Instant::now();
            }

            // Sleep until next sample
            let elapsed = sample_start.elapsed();
            if elapsed < sample_interval {
                thread::sleep(sample_interval - elapsed);
            }
        }

        info!("Sampling complete");
        Ok(())
    }

    /// Clear the terminal and redraw the live top-functions view.
    fn render_top(&self) {
        use std::io::Write;
        let screen = crate::output::top::render(
            &self.aggregator,
            &self.target_label(),
            self.start_time.elapsed().as_secs_f64(),
            30,
        );
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        // \x1b[2J clears the screen, \x1b[H moves the cursor home.
        let _ = write!(handle, "\x1b[2J\x1b[H{}", screen);
        let _ = handle.flush();
    }

    /// Take a single snapshot of the current stack(s) (for `dump`).
    pub fn capture_stacks(&self) -> Result<Vec<Vec<StackFrame>>> {
        self.sample()
    }

    /// Human-readable target label for output headers/titles.
    fn target_label(&self) -> String {
        match &self.target {
            Target::Spawn(_) => self
                .config
                .command
                .as_ref()
                .and_then(|argv| argv.first())
                .map(|c| c.rsplit('/').next().unwrap_or(c.as_str()).to_string())
                .unwrap_or_else(|| format!("PID {}", self.pid())),
            Target::Attach(pid) => format!("PID {}", pid),
        }
    }

    /// Terminate the spawned child, if any. No-op in attach mode.
    fn terminate_child(&self) {
        if let Target::Spawn(child) = &self.target {
            child.terminate();
        }
    }

    /// Take a single sample, returning zero or more stacks to aggregate.
    fn sample(&self) -> Result<Vec<Vec<StackFrame>>> {
        match &self.mode {
            Mode::Single { addr } => {
                // Lazily discover the lua_State (spawn mode: it may not exist yet).
                let mut lua_state_addr = addr.get();
                if lua_state_addr == 0 {
                    match self.discover_single_vm() {
                        Some(found) => {
                            info!("single-VM mode: lua_State at 0x{:x}", found);
                            addr.set(found);
                            lua_state_addr = found;
                        }
                        None => return Ok(Vec::new()), // retry next tick
                    }
                }
                Ok(vec![self.sample_single(lua_state_addr)?])
            }
            Mode::Skynet { h_addr } => self.sample_skynet(*h_addr),
        }
    }

    /// Discover the single plain-Lua VM. Among all scan candidates, pick the first whose
    /// stack actually walks to a non-empty result — the loose `tt==thread` scan alone
    /// yields many false positives, especially early in a spawned process's life.
    fn discover_single_vm(&self) -> Option<usize> {
        let discovery = LuaStateDiscovery::new(self.reader.as_ref(), &self.layout, self.pid());
        let candidates = discovery.find_all().unwrap_or_default();
        let walker = StackWalker::new(self.reader.as_ref(), &self.layout);
        candidates.into_iter().find(|&addr| {
            matches!(walker.walk(addr), Ok(frames) if !frames.is_empty())
        })
    }

    /// Sample a single VM, merging native (C) context with the Lua stack.
    fn sample_single(&self, lua_state_addr: usize) -> Result<Vec<StackFrame>> {
        let walker = StackWalker::new(self.reader.as_ref(), &self.layout);
        let mut lua_frames = walker.walk(lua_state_addr)?;

        // Resolve C function symbols for Lua frames
        for frame in &mut lua_frames {
            if frame.is_c && frame.name.is_empty() && frame.func_addr != 0 {
                if let Some(name) = self.symbol_resolver.resolve_c_function(frame.func_addr) {
                    frame.name = name;
                }
            }
        }

        // Get native (C) stack frames from the reusable unwinder (best-effort).
        let native_frames = self
            .native_unwinder
            .as_ref()
            .map(|u| u.frames(self.reader.as_ref()))
            .unwrap_or_default();
        let native_stack_frames: Vec<StackFrame> = native_frames
            .iter()
            .map(|nf| {
                let name = self
                    .symbol_resolver
                    .resolve_c_function(nf.ip)
                    .unwrap_or_default();
                StackFrame::c_function(name, nf.ip)
            })
            .collect();

        // Merge into one deepest-first stack (current frame at index 0, as the aggregator
        // expects): the Lua frames sit on top, the native C interpreter chain
        // (luaV_execute → … → main → _libc_start_main) below. Both sources are already
        // innermost-first, so we append the native chain after the Lua frames without
        // reversing. Unnamed C frames are dropped when a native chain is present to avoid
        // duplicating the interpreter's own C frames that also show up on the Lua CallInfo.
        let mut merged = Vec::new();
        for frame in lua_frames {
            if frame.is_c && frame.name.is_empty() && !native_stack_frames.is_empty() {
                continue;
            }
            merged.push(frame);
        }
        for frame in native_stack_frames {
            if frame.name.is_empty() {
                continue;
            }
            merged.push(frame);
        }

        Ok(merged)
    }

    /// Sample all skynet VMs. Each service becomes its own stack with a
    /// `[service:name]` root frame. Native merging is skipped: a ptrace
    /// register snapshot maps to one worker thread, not to a specific VM.
    fn sample_skynet(&self, h_addr: usize) -> Result<Vec<Vec<StackFrame>>> {
        let disc = SkynetDiscovery::new(self.reader.as_ref(), &self.layout, h_addr);
        let vms = disc.enumerate()?;

        let walker = StackWalker::new(self.reader.as_ref(), &self.layout);
        let mut stacks = Vec::new();

        for vm in vms {
            let mut lua_frames = match walker.walk(vm.lua_state_addr) {
                Ok(f) => f,
                Err(e) => {
                    debug!("walk failed for {} @ 0x{:x}: {:?}", vm.label, vm.lua_state_addr, e);
                    continue;
                }
            };

            for frame in &mut lua_frames {
                if frame.is_c && frame.name.is_empty() && frame.func_addr != 0 {
                    if let Some(name) = self.symbol_resolver.resolve_c_function(frame.func_addr) {
                        frame.name = name;
                    }
                }
            }

            // Prepend a synthetic per-service root frame.
            let mut stack = Vec::with_capacity(lua_frames.len() + 1);
            stack.extend(lua_frames);
            stack.push(StackFrame::root(format!("[service:{}]", vm.label)));
            stacks.push(stack);
        }

        Ok(stacks)
    }

    /// Generate output
    pub fn generate_output(&self) -> Result<()> {
        let duration = self.start_time.elapsed().as_secs_f64();
        let output = CollapsedOutput::new(&self.aggregator);

        let output_path = self.config.output_file.as_deref().unwrap_or("profile.txt");

        if output_path.ends_with(".svg") {
            let title = format!("tracemoon - {} ({:.1}s)", self.target_label(), duration);
            output
                .write_flamegraph_file(output_path, &title)
                .map_err(TracemoonError::Io)?;
            info!("Flamegraph SVG written to {}", output_path);
        } else {
            output.write_file(output_path).map_err(TracemoonError::Io)?;
            info!("Output written to {}", output_path);
        }

        output.write_stats_stderr(duration).map_err(TracemoonError::Io)?;

        Ok(())
    }

    /// Get the aggregator (for testing)
    pub fn aggregator(&self) -> &Aggregator {
        &self.aggregator
    }
}

/// Run the profiler by attaching to an existing PID.
pub fn run_profiler(pid: Pid, config: Config) -> Result<()> {
    let mut sampler = Sampler::new(Target::Attach(pid), config)?;

    // Setup Ctrl+C handler
    let running = sampler.get_stop_handle();
    ctrlc_setup(running)?;

    // Run sampling loop
    sampler.run()?;

    // Generate output
    sampler.generate_output()?;

    Ok(())
}

/// Run the profiler by spawning a child process and profiling from its start.
pub fn run_profiler_spawn(argv: Vec<String>, mut config: Config) -> Result<()> {
    let child = SpawnedChild::spawn(&argv)?;
    config.command = Some(argv);

    let mut sampler = Sampler::new(Target::Spawn(child), config)?;

    // Setup Ctrl+C handler
    let running = sampler.get_stop_handle();
    ctrlc_setup(running)?;

    // Run sampling loop (resumes the child, samples, stops on exit/duration/Ctrl+C)
    let run_result = sampler.run();

    // Terminate the child if it's still alive, regardless of how sampling ended.
    if let Target::Spawn(child) = &sampler.target {
        child.terminate();
    }

    run_result?;

    // Generate output
    sampler.generate_output()?;

    Ok(())
}

/// Build a sampler for either an attach PID or a spawned command.
fn build_target(pid: Option<Pid>, argv: Option<Vec<String>>) -> Result<(Target, Option<Vec<String>>)> {
    match (pid, argv) {
        (Some(pid), None) => Ok((Target::Attach(pid), None)),
        (None, Some(argv)) => {
            let child = SpawnedChild::spawn(&argv)?;
            Ok((Target::Spawn(child), Some(argv)))
        }
        (Some(_), Some(_)) => Err(TracemoonError::Config(
            "Specify either -p <PID> to attach or a command after -- to spawn, not both."
                .to_string(),
        )),
        (None, None) => Err(TracemoonError::Config(
            "No target specified. Use -p <PID> to attach, or -- <command> to spawn.".to_string(),
        )),
    }
}

/// Print a one-shot snapshot of the current stack(s) and exit (`dump` mode).
pub fn run_dump(pid: Option<Pid>, argv: Option<Vec<String>>, mut config: Config) -> Result<()> {
    let (target, spawned_argv) = build_target(pid, argv)?;
    if let Some(argv) = spawned_argv {
        config.command = Some(argv);
    }

    let sampler = Sampler::new(target, config)?;

    // For a spawned child, let it run so it has a stack to snapshot.
    if let Target::Spawn(child) = &sampler.target {
        child.resume()?;
    }

    // Retry until we get a non-empty snapshot or the deadline passes (spawned VMs and lazy
    // single-VM discovery may need a few ticks to warm up).
    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut stacks = Vec::new();
    loop {
        if let Ok(s) = sampler.capture_stacks() {
            if !s.is_empty() {
                stacks = s;
                break;
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        if let Target::Spawn(child) = &sampler.target {
            if child.has_exited() {
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    sampler.terminate_child();

    let out = crate::output::dump::format(&stacks, &sampler.target_label(), sampler.pid());
    print!("{}", out);

    Ok(())
}

/// Live-updating terminal view of the hottest functions (`top` mode).
pub fn run_top(pid: Option<Pid>, argv: Option<Vec<String>>, mut config: Config) -> Result<()> {
    let (target, spawned_argv) = build_target(pid, argv)?;
    if let Some(argv) = spawned_argv {
        config.command = Some(argv);
    }

    let mut sampler = Sampler::new(target, config)?;

    let running = sampler.get_stop_handle();
    ctrlc_setup(running)?;

    // run() drives the live rendering for Top mode; resumes/stops the child as needed.
    let run_result = sampler.run();

    sampler.terminate_child();

    run_result?;

    Ok(())
}

/// Setup Ctrl+C signal handler
fn ctrlc_setup(running: Arc<AtomicBool>) -> Result<()> {
    ctrlc::set_handler(move || {
        info!("Received Ctrl+C, stopping...");
        running.store(false, Ordering::Relaxed);
    })
    .map_err(|e| TracemoonError::Other(format!("Failed to set signal handler: {}", e)))?;

    Ok(())
}

#[cfg(test)]
mod tests {

    // Integration tests would require a running Lua process
}
