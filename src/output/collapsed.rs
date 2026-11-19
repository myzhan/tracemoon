//! Collapsed stack format and flamegraph SVG output

use crate::output::Aggregator;
use inferno::flamegraph::{self, Direction, Options as FlamegraphOptions};
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

    /// Write to a file
    pub fn write_file(&self, path: &str) -> io::Result<()> {
        let file = std::fs::File::create(path)?;
        let mut writer = io::BufWriter::new(file);
        self.write(&mut writer)?;
        // Flush explicitly: `BufWriter`'s Drop swallows a failing final write, which would
        // report success for a full disk / failed sync.
        writer.flush()
    }

    /// Generate output as a string
    ///
    /// Not an inherent `to_string`: this is a format conversion, not a `Display` rendering.
    pub fn to_collapsed_string(&self) -> String {
        let mut buffer = Vec::new();
        self.write(&mut buffer)
            .expect("Writing to Vec should not fail");
        String::from_utf8(buffer).expect("Output should be valid UTF-8")
    }

    /// Write statistics to stderr
    ///
    /// Ticks and stacks are reported separately: one tick yields one stack in single-VM
    /// mode but one stack *per service* in skynet mode, so the success rate is always
    /// computed over ticks while stack percentages use the sampled-stack count.
    pub fn write_stats<W: Write>(&self, writer: &mut W, duration_secs: f64) -> io::Result<()> {
        writeln!(writer, "\nSampling statistics:")?;
        writeln!(writer, "  Duration: {:.1}s", duration_secs)?;
        writeln!(
            writer,
            "  Sampling ticks: {}",
            self.aggregator.total_ticks()
        )?;
        writeln!(
            writer,
            "  Successful ticks: {}",
            self.aggregator.successful_ticks()
        )?;
        writeln!(writer, "  Failed ticks: {}", self.aggregator.failed_ticks())?;
        writeln!(
            writer,
            "  Success rate: {:.1}%",
            self.aggregator.success_rate()
        )?;
        writeln!(
            writer,
            "  Stacks sampled: {}",
            self.aggregator.sampled_stacks()
        )?;
        writeln!(
            writer,
            "  Unique stacks: {}",
            self.aggregator.unique_stacks()
        )?;

        // Top stacks (displayed hierarchically for readability)
        let top = self.aggregator.top_stacks(5);
        if !top.is_empty() {
            writeln!(writer, "\n  Top stacks:")?;
            let total = self.aggregator.sampled_stacks() as f64;
            for (stack, count) in top {
                let pct = (*count as f64 / total) * 100.0;
                writeln!(writer, "    [{:.1}%] ({} samples)", pct, count)?;

                // Split by semicolon and display with indentation (bottom-up)
                let frames: Vec<&str> = stack.split(';').collect();
                for (i, frame) in frames.iter().enumerate() {
                    let indent = "      ";
                    let prefix = if i == frames.len() - 1 {
                        "└─"
                    } else {
                        "├─"
                    };
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
        let collapsed = self.to_collapsed_string();
        let reader = Cursor::new(collapsed.as_bytes());
        let lines: Vec<String> = reader.lines().collect::<io::Result<Vec<_>>>()?;

        let mut opts = FlamegraphOptions::default();
        opts.title = title.to_string();
        opts.count_name = "samples".to_string();
        opts.direction = Direction::Straight; // bottom-up (callers at bottom)
        opts.min_width = 0.1;
        opts.flame_chart = false;

        flamegraph::from_lines(&mut opts, lines.iter().map(|s| s.as_str()), writer)
            .map_err(|e| io::Error::other(e.to_string()))
    }

    /// Write flamegraph SVG to a file
    pub fn write_flamegraph_file(&self, path: &str, title: &str) -> io::Result<()> {
        let file = std::fs::File::create(path)?;
        let mut writer = io::BufWriter::new(file);
        self.write_flamegraph_svg(&mut writer, title)?;
        writer.flush()
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
            agg.add_tick(vec![vec![
                StackFrame::lua_function("main.lua".to_string(), "func_a".to_string(), 10),
                StackFrame::c_function("main".to_string(), 0),
            ]]);
        }

        for _ in 0..5 {
            agg.add_tick(vec![vec![
                StackFrame::lua_function("main.lua".to_string(), "func_b".to_string(), 20),
                StackFrame::c_function("main".to_string(), 0),
            ]]);
        }

        let output = CollapsedOutput::new(&agg);
        let result = output.to_collapsed_string();

        // Check that output contains expected content
        assert!(result.contains("[C]:main;main.lua:10:func_a 10"));
        assert!(result.contains("[C]:main;main.lua:20:func_b 5"));
    }

    #[test]
    fn test_empty_output() {
        let agg = Aggregator::new();
        let output = CollapsedOutput::new(&agg);
        let result = output.to_collapsed_string();

        assert!(result.is_empty());
    }
}
