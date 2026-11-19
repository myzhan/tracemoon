//! Stack sample aggregation

use crate::lua::StackFrame;
use std::collections::HashMap;

/// Aggregates stack samples and counts occurrences.
///
/// Two counters are kept apart on purpose, because multi-VM (skynet) sampling makes them
/// diverge: one *tick* of the sampling loop can yield many stacks (one per service), so a
/// success rate computed over stacks and a failure count computed over ticks would not be
/// comparable (10 good services followed by one dead tick would read as "90.9% ok").
pub struct Aggregator {
    /// Map from stack trace (as string key) to count
    stacks: HashMap<String, u64>,
    /// Sampling ticks attempted — the success-rate denominator.
    total_ticks: u64,
    /// Ticks that produced no usable (non-empty) stack.
    failed_ticks: u64,
    /// Non-empty stacks aggregated — the percentage denominator.
    sampled_stacks: u64,
}

impl Aggregator {
    /// Create a new aggregator
    pub fn new() -> Self {
        Self {
            stacks: HashMap::new(),
            total_ticks: 0,
            failed_ticks: 0,
            sampled_stacks: 0,
        }
    }

    /// Record one sampling tick: every non-empty stack inside contributes one sample, and
    /// the tick counts as failed when none of them did.
    pub fn add_tick(&mut self, stacks: Vec<Vec<StackFrame>>) {
        self.total_ticks += 1;

        let mut usable = 0u64;
        for frames in stacks {
            if frames.is_empty() {
                continue;
            }
            usable += 1;
            self.add_stack(frames);
        }

        if usable == 0 {
            self.failed_ticks += 1;
        }
    }

    /// Add a single non-empty stack sample.
    pub fn add_stack(&mut self, frames: Vec<StackFrame>) {
        debug_assert!(!frames.is_empty(), "empty stacks must not be aggregated");
        if frames.is_empty() {
            return;
        }

        self.sampled_stacks += 1;
        let key = Self::frames_to_key(&frames);
        *self.stacks.entry(key).or_insert(0) += 1;
    }

    /// Record a failed sampling tick.
    pub fn add_failed(&mut self) {
        self.total_ticks += 1;
        self.failed_ticks += 1;
    }

    /// Convert frames to a semicolon-separated key
    /// Frames should be deepest-first (current function first)
    /// Output is reversed so base is first (standard flamegraph format)
    fn frames_to_key(frames: &[StackFrame]) -> String {
        // Reverse to get base-to-current order for flamegraph
        frames
            .iter()
            .rev()
            .map(|f| f.format())
            .collect::<Vec<_>>()
            .join(";")
    }

    /// Get all aggregated stacks with counts
    pub fn get_stacks(&self) -> &HashMap<String, u64> {
        &self.stacks
    }

    /// Number of sampling ticks attempted.
    pub fn total_ticks(&self) -> u64 {
        self.total_ticks
    }

    /// Number of ticks that produced at least one usable stack.
    pub fn successful_ticks(&self) -> u64 {
        self.total_ticks - self.failed_ticks
    }

    /// Number of failed ticks.
    pub fn failed_ticks(&self) -> u64 {
        self.failed_ticks
    }

    /// Number of non-empty stacks aggregated (≥ successful ticks in multi-VM mode).
    pub fn sampled_stacks(&self) -> u64 {
        self.sampled_stacks
    }

    /// Get number of unique stacks
    pub fn unique_stacks(&self) -> usize {
        self.stacks.len()
    }

    /// Calculate tick success rate as a percentage
    pub fn success_rate(&self) -> f64 {
        if self.total_ticks == 0 {
            100.0
        } else {
            (self.successful_ticks() as f64 / self.total_ticks as f64) * 100.0
        }
    }

    /// Get the top N most frequent stacks
    pub fn top_stacks(&self, n: usize) -> Vec<(&String, &u64)> {
        let mut sorted: Vec<_> = self.stacks.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        sorted.truncate(n);
        sorted
    }
}

impl Default for Aggregator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(name: &str) -> StackFrame {
        StackFrame::lua_function("test.lua".to_string(), name.to_string(), 10)
    }

    #[test]
    fn test_aggregator_basic() {
        let mut agg = Aggregator::new();

        let stack = vec![frame("inner"), frame("outer")];
        agg.add_tick(vec![stack.clone()]);
        agg.add_tick(vec![stack]);
        agg.add_tick(vec![vec![frame("other")]]);

        assert_eq!(agg.total_ticks(), 3);
        assert_eq!(agg.sampled_stacks(), 3);
        assert_eq!(agg.unique_stacks(), 2);
        assert_eq!(agg.success_rate(), 100.0);
    }

    #[test]
    fn test_aggregator_failed() {
        let mut agg = Aggregator::new();

        agg.add_tick(vec![vec![StackFrame::c_function(
            "test".to_string(),
            0x1000,
        )]]);
        agg.add_failed();
        agg.add_failed();

        assert_eq!(agg.total_ticks(), 3);
        assert_eq!(agg.successful_ticks(), 1);
        assert_eq!(agg.failed_ticks(), 2);
        assert!((agg.success_rate() - 33.33).abs() < 0.1);
    }

    /// The regression this guards: a multi-VM tick used to count one sample *per VM* on
    /// success but only one on failure, so mixing them produced impossible rates.
    #[test]
    fn test_multi_vm_tick_success_rate_is_per_tick() {
        let mut agg = Aggregator::new();

        // One tick, ten services → ten samples but a single successful tick.
        let ten_vms = (0..10)
            .map(|i| {
                vec![
                    frame(&format!("vm{}", i)),
                    StackFrame::root("[service:x]".into()),
                ]
            })
            .collect();
        agg.add_tick(ten_vms);
        // Next tick: every VM fails.
        agg.add_tick(Vec::new());

        assert_eq!(agg.total_ticks(), 2);
        assert_eq!(agg.successful_ticks(), 1);
        assert_eq!(agg.sampled_stacks(), 10);
        assert!((agg.success_rate() - 50.0).abs() < 0.01);
    }

    /// An empty stack is not a usable sample: it must not inflate the success rate.
    #[test]
    fn test_empty_stack_is_not_a_success() {
        let mut agg = Aggregator::new();

        agg.add_tick(vec![Vec::new()]);

        assert_eq!(agg.total_ticks(), 1);
        assert_eq!(agg.failed_ticks(), 1);
        assert_eq!(agg.sampled_stacks(), 0);
        assert_eq!(agg.unique_stacks(), 0);
        assert_eq!(agg.success_rate(), 0.0);
    }

    #[test]
    fn test_frames_to_key() {
        let frames = vec![
            StackFrame::lua_function("a.lua".to_string(), "inner".to_string(), 10),
            StackFrame::lua_function("b.lua".to_string(), "outer".to_string(), 5),
            StackFrame::c_function("main".to_string(), 0),
        ];

        let key = Aggregator::frames_to_key(&frames);
        // Should be reversed: base to current
        assert_eq!(key, "[C]:main;b.lua:5:outer;a.lua:10:inner");
    }
}
