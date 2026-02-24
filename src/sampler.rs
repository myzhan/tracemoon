//! Sampling coordinator for Lua profiling

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use remoteprocess::Pid;

use crate::config::Config;
use crate::error::{Result, TracemoonError};
use crate::lua::{StackFrame, StackWalker};
use crate::output::{Aggregator, CollapsedOutput};
use crate::process::{create_reader, get_native_frames, LuaStateDiscovery, ProcessMemoryReader};
use crate::symbols::SymbolResolver;

/// Main sampler coordinator
pub struct Sampler {
    pid: Pid,
    config: Config,
    reader: Box<dyn ProcessMemoryReader>,
    lua_state_addr: usize,
    symbol_resolver: SymbolResolver,
    aggregator: Aggregator,
    running: Arc<AtomicBool>,
    start_time: Instant,
}

impl Sampler {
    /// Create a new sampler for the given process
    pub fn new(pid: Pid, config: Config) -> Result<Self> {
        info!("Creating sampler for process {}", pid);
        
        // Create memory reader
        let reader = create_reader(pid)?;
        
        // Discover lua_State address
        let discovery = LuaStateDiscovery::new(reader.as_ref(), pid);
        let lua_state_addr = discovery.find()?;
        
        info!("Found lua_State at 0x{:x}", lua_state_addr);
        
        // Create symbol resolver
        let symbol_resolver = SymbolResolver::new(pid);
        symbol_resolver.preload();
        
        Ok(Self {
            pid,
            config,
            reader,
            lua_state_addr,
            symbol_resolver,
            aggregator: Aggregator::new(),
            running: Arc::new(AtomicBool::new(true)),
            start_time: Instant::now(),
        })
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
        
        self.start_time = Instant::now();
        let sample_interval = Duration::from_secs_f64(1.0 / self.config.sample_rate as f64);
        let duration_limit = if self.config.duration > 0 {
            Some(Duration::from_secs(self.config.duration))
        } else {
            None
        };
        
        let mut consecutive_failures = 0;
        const MAX_CONSECUTIVE_FAILURES: u32 = 10;
        
        while self.running.load(Ordering::Relaxed) {
            let sample_start = Instant::now();
            
            // Check duration limit
            if let Some(limit) = duration_limit {
                if self.start_time.elapsed() >= limit {
                    info!("Duration limit reached");
                    break;
                }
            }
            
            // Take a sample
            match self.sample() {
                Ok(frames) => {
                    self.aggregator.add(frames);
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
            
            // Sleep until next sample
            let elapsed = sample_start.elapsed();
            if elapsed < sample_interval {
                thread::sleep(sample_interval - elapsed);
            }
        }
        
        info!("Sampling complete");
        Ok(())
    }

    /// Take a single sample
    fn sample(&self) -> Result<Vec<StackFrame>> {
        // Get Lua stack frames
        let walker = StackWalker::new(self.reader.as_ref());
        let mut lua_frames = walker.walk(self.lua_state_addr)?;
        
        // Resolve C function symbols for Lua frames
        for frame in &mut lua_frames {
            if frame.is_c && frame.name.is_empty() && frame.func_addr != 0 {
                if let Some(name) = self.symbol_resolver.resolve_c_function(frame.func_addr) {
                    frame.name = name;
                }
            }
        }
        
        // Get native (C) stack frames
        let native_frames = get_native_frames(self.pid, self.reader.as_ref()).unwrap_or_default();
        
        // Convert native frames to StackFrames and resolve symbols
        let native_stack_frames: Vec<StackFrame> = native_frames
            .iter()
            .map(|nf| {
                let name = self.symbol_resolver
                    .resolve_c_function(nf.ip)
                    .unwrap_or_default();
                StackFrame::c_function(name, nf.ip)
            })
            .collect();
        
        // Merge stacks: native frames at bottom, Lua frames on top
        // The native stack shows the C call chain leading to Lua execution
        // We want: [native bottom] -> [lua main] -> [lua functions] -> [native top (math.sin etc)]
        
        // Find where Lua and native stacks overlap
        // Native stack typically contains: main -> lua_pcall -> luaV_execute -> ... -> current
        // We want to show the full native context
        
        // Strategy: Put native stack at bottom, replace generic Lua C frames with native info
        let mut merged = Vec::new();
        
        // Add native frames (reversed to show bottom-up)
        // Filter out frames that are clearly inside Lua VM internals unless they're interesting
        for frame in native_stack_frames.iter().rev() {
            // Skip empty/unresolved frames in the middle
            if frame.name.is_empty() && merged.len() > 0 {
                continue;
            }
            merged.push(frame.clone());
        }
        
        // Add Lua frames, but skip the generic [unknown] C frames if we have native info
        for frame in lua_frames {
            if frame.is_c && frame.name.is_empty() && !native_stack_frames.is_empty() {
                // Skip generic C frames, we have better native info
                continue;
            }
            merged.push(frame);
        }
        
        Ok(merged)
    }

    /// Generate output
    pub fn generate_output(&self) -> Result<()> {
        let duration = self.start_time.elapsed().as_secs_f64();
        let output = CollapsedOutput::new(&self.aggregator);
        
        let output_path = self.config.output_file.as_deref().unwrap_or("profile.txt");
        
        if output_path.ends_with(".svg") {
            // Generate flamegraph SVG directly
            let title = format!("tracemoon - PID {} ({:.1}s)", self.pid, duration);
            output.write_flamegraph_file(output_path, &title)
                .map_err(TracemoonError::Io)?;
            info!("Flamegraph SVG written to {}", output_path);
        } else {
            // Write collapsed stacks
            output.write_file(output_path).map_err(TracemoonError::Io)?;
            info!("Output written to {}", output_path);
        }
        
        // Write stats to stderr
        output.write_stats_stderr(duration).map_err(TracemoonError::Io)?;
        
        Ok(())
    }

    /// Get the aggregator (for testing)
    pub fn aggregator(&self) -> &Aggregator {
        &self.aggregator
    }
}

/// Run the profiler with signal handling
pub fn run_profiler(pid: Pid, config: Config) -> Result<()> {
    let mut sampler = Sampler::new(pid, config)?;
    
    // Setup Ctrl+C handler
    let running = sampler.get_stop_handle();
    ctrlc_setup(running)?;
    
    // Run sampling loop
    sampler.run()?;
    
    // Generate output
    sampler.generate_output()?;
    
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
    use super::*;

    // Integration tests would require a running Lua process
}
