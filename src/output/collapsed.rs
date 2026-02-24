//! Collapsed stack format and flamegraph SVG output

use crate::output::Aggregator;
use inferno::flamegraph::{self, Options as FlamegraphOptions, Direction};
use std::io::{self, BufRead, Cursor, Write};

/// Generates collapsed stack format output
pub struct CollapsedOutput<'a> {
    aggregator: &'a Aggregator,
}

impl<'a> CollapsedOutput<'a> {
    /// Create a new collapsed output generator
    pub fn new(aggregator: &'a Aggregator) -> Self {
        Self { aggregator }
    }

    /// Write collapsed stacks to a writer
    pub fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut stacks: Vec<_> = self.aggregator.get_stacks().iter().collect();
        
        // Sort by count (descending) for readability
        stacks.sort_by(|a, b| b.1.cmp(a.1));
        
        for (stack, count) in stacks {
            writeln!(writer, "{} {}", stack, count)?;
        }
        
        Ok(())
    }

    /// Write to stdout
    pub fn write_stdout(&self) -> io::Result<()> {
        let stdout = io::stdout();
        let mut handle = stdout.lock();
        self.write(&mut handle)
    }

    /// Write to a file
    pub fn write_file(&self, path: &str) -> io::Result<()> {
        let file = std::fs::File::create(path)?;
        let mut writer = io::BufWriter::new(file);
        self.write(&mut writer)
    }

    /// Generate output as a string
    pub fn to_string(&self) -> String {
        let mut buffer = Vec::new();
        self.write(&mut buffer).expect("Writing to Vec should not fail");
        String::from_utf8(buffer).expect("Output should be valid UTF-8")
    }

    /// Write statistics to stderr
    pub fn write_stats<W: Write>(&self, writer: &mut W, duration_secs: f64) -> io::Result<()> {
        writeln!(writer, "\nSampling statistics:")?;
        writeln!(writer, "  Duration: {:.1}s", duration_secs)?;
        writeln!(writer, "  Samples collected: {}", self.aggregator.total_samples())?;
        writeln!(writer, "  Successful samples: {}", self.aggregator.successful_samples())?;
        writeln!(writer, "  Failed samples: {}", self.aggregator.failed_samples())?;
        writeln!(writer, "  Unique stacks: {}", self.aggregator.unique_stacks())?;
        writeln!(writer, "  Success rate: {:.1}%", self.aggregator.success_rate())?;
        
        // Top stacks (displayed hierarchically for readability)
        let top = self.aggregator.top_stacks(5);
        if !top.is_empty() {
            writeln!(writer, "\n  Top stacks:")?;
            let total = self.aggregator.successful_samples() as f64;
            for (stack, count) in top {
                let pct = (*count as f64 / total) * 100.0;
                writeln!(writer, "    [{:.1}%] ({} samples)", pct, count)?;
                
                // Split by semicolon and display with indentation (bottom-up)
                let frames: Vec<&str> = stack.split(';').collect();
                for (i, frame) in frames.iter().enumerate() {
                    let indent = "      ";
                    let prefix = if i == frames.len() - 1 { "└─" } else { "├─" };
                    writeln!(writer, "{}{} {}", indent, prefix, frame)?;
                }
                writeln!(writer)?;
            }
        }
        
        Ok(())
    }

    /// Write stats to stderr
    pub fn write_stats_stderr(&self, duration_secs: f64) -> io::Result<()> {
        let stderr = io::stderr();
        let mut handle = stderr.lock();
        self.write_stats(&mut handle, duration_secs)
    }

    /// Generate flamegraph SVG from the collapsed stacks
    pub fn write_flamegraph_svg<W: Write>(&self, writer: &mut W, title: &str) -> io::Result<()> {
        // Generate collapsed stack text
        let collapsed = self.to_string();
        let reader = Cursor::new(collapsed.as_bytes());
        let lines: Vec<String> = reader.lines().collect::<io::Result<Vec<_>>>()?;
        
        let mut opts = FlamegraphOptions::default();
        opts.title = title.to_string();
        opts.count_name = "samples".to_string();
        opts.direction = Direction::Straight; // bottom-up (callers at bottom)
        opts.min_width = 0.1;
        opts.flame_chart = false;
        
        flamegraph::from_lines(
            &mut opts,
            lines.iter().map(|s| s.as_str()),
            writer,
        ).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }

    /// Write flamegraph SVG to a file
    pub fn write_flamegraph_file(&self, path: &str, title: &str) -> io::Result<()> {
        let file = std::fs::File::create(path)?;
        let mut writer = io::BufWriter::new(file);
        self.write_flamegraph_svg(&mut writer, title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::StackFrame;

    #[test]
    fn test_collapsed_output() {
        let mut agg = Aggregator::new();
        
        // Add some samples
        for _ in 0..10 {
            agg.add(vec![
                StackFrame::lua_function("main.lua".to_string(), "func_a".to_string(), 10),
                StackFrame::c_function("main".to_string(), 0),
            ]);
        }
        
        for _ in 0..5 {
            agg.add(vec![
                StackFrame::lua_function("main.lua".to_string(), "func_b".to_string(), 20),
                StackFrame::c_function("main".to_string(), 0),
            ]);
        }
        
        let output = CollapsedOutput::new(&agg);
        let result = output.to_string();
        
        // Check that output contains expected content
        assert!(result.contains("[C]:main;main.lua:10:func_a 10"));
        assert!(result.contains("[C]:main;main.lua:20:func_b 5"));
    }

    #[test]
    fn test_empty_output() {
        let agg = Aggregator::new();
        let output = CollapsedOutput::new(&agg);
        let result = output.to_string();
        
        assert!(result.is_empty());
    }
}
