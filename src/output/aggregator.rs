//! Stack sample aggregation

use crate::lua::StackFrame;
use std::collections::HashMap;

/// Aggregates stack samples and counts occurrences
pub struct Aggregator {
    /// Map from stack trace (as string key) to count
    stacks: HashMap<String, u64>,
    /// Total number of samples collected
    total_samples: u64,
    /// Number of failed samples
    failed_samples: u64,
}

impl Aggregator {
    /// Create a new aggregator
    pub fn new() -> Self {
        Self {
            stacks: HashMap::new(),
            total_samples: 0,
            failed_samples: 0,
        }
    }

    /// Add a successful stack sample
    pub fn add(&mut self, frames: Vec<StackFrame>) {
        self.total_samples += 1;
        
        if frames.is_empty() {
            return;
        }
        
        // Convert frames to collapsed stack format (key for aggregation)
        let key = Self::frames_to_key(&frames);
        
        *self.stacks.entry(key).or_insert(0) += 1;
    }

    /// Record a failed sample
    pub fn add_failed(&mut self) {
        self.total_samples += 1;
        self.failed_samples += 1;
    }

    /// Convert frames to a semicolon-separated key
    /// Frames should be deepest-first (current function first)
    /// Output is reversed so base is first (standard flamegraph format)
    fn frames_to_key(frames: &[StackFrame]) -> String {
        // Reverse to get base-to-current order for flamegraph
        frames.iter()
            .rev()
            .map(|f| f.format())
            .collect::<Vec<_>>()
            .join(";")
    }

    /// Get all aggregated stacks with counts
    pub fn get_stacks(&self) -> &HashMap<String, u64> {
        &self.stacks
    }

    /// Get total number of samples
    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }

    /// Get number of successful samples
    pub fn successful_samples(&self) -> u64 {
        self.total_samples - self.failed_samples
    }

    /// Get number of failed samples  
    pub fn failed_samples(&self) -> u64 {
        self.failed_samples
    }

    /// Get number of unique stacks
    pub fn unique_stacks(&self) -> usize {
        self.stacks.len()
    }

    /// Calculate success rate as percentage
    pub fn success_rate(&self) -> f64 {
        if self.total_samples == 0 {
            100.0
        } else {
            (self.successful_samples() as f64 / self.total_samples as f64) * 100.0
        }
    }

    /// Get the top N most frequent stacks
    pub fn top_stacks(&self, n: usize) -> Vec<(&String, &u64)> {
        let mut sorted: Vec<_> = self.stacks.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        sorted.truncate(n);
        sorted
    }

    /// Merge another aggregator into this one
    pub fn merge(&mut self, other: Aggregator) {
        for (key, count) in other.stacks {
            *self.stacks.entry(key).or_insert(0) += count;
        }
        self.total_samples += other.total_samples;
        self.failed_samples += other.failed_samples;
    }

    /// Clear all data
    pub fn clear(&mut self) {
        self.stacks.clear();
        self.total_samples = 0;
        self.failed_samples = 0;
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

    #[test]
    fn test_aggregator_basic() {
        let mut agg = Aggregator::new();
        
        let frames1 = vec![
            StackFrame::lua_function("test.lua".to_string(), "inner".to_string(), 10),
            StackFrame::lua_function("test.lua".to_string(), "outer".to_string(), 5),
        ];
        
        let frames2 = vec![
            StackFrame::lua_function("test.lua".to_string(), "inner".to_string(), 10),
            StackFrame::lua_function("test.lua".to_string(), "outer".to_string(), 5),
        ];
        
        let frames3 = vec![
            StackFrame::lua_function("test.lua".to_string(), "other".to_string(), 20),
        ];
        
        agg.add(frames1);
        agg.add(frames2);
        agg.add(frames3);
        
        assert_eq!(agg.total_samples(), 3);
        assert_eq!(agg.unique_stacks(), 2);
        assert_eq!(agg.success_rate(), 100.0);
    }

    #[test]
    fn test_aggregator_failed() {
        let mut agg = Aggregator::new();
        
        agg.add(vec![StackFrame::c_function("test".to_string(), 0x1000)]);
        agg.add_failed();
        agg.add_failed();
        
        assert_eq!(agg.total_samples(), 3);
        assert_eq!(agg.successful_samples(), 1);
        assert_eq!(agg.failed_samples(), 2);
        assert!((agg.success_rate() - 33.33).abs() < 0.1);
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
