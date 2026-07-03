//! py-spy-style `top` view: hottest functions by self (%Own) and total (%Total) time.

use std::collections::HashMap;

use crate::output::Aggregator;

/// Per-function sample counts derived from collapsed stacks.
#[derive(Debug, Clone)]
pub struct FnStat {
    pub function: String,
    /// Samples where this function was the leaf (currently executing) frame.
    pub own: u64,
    /// Samples where this function appeared anywhere in the stack.
    pub total: u64,
}

/// Compute per-function `own`/`total` counts from aggregated collapsed stacks.
///
/// Keys are `base;mid;…;leaf` (base-to-current order, as produced by the aggregator). A
/// function's `total` counts every stack it appears in; `own` counts only stacks where it is
/// the leaf. A function appearing multiple times in one stack is counted once toward `total`.
pub fn function_stats(stacks: &HashMap<String, u64>) -> Vec<FnStat> {
    let mut own: HashMap<&str, u64> = HashMap::new();
    let mut total: HashMap<&str, u64> = HashMap::new();

    for (key, &count) in stacks {
        let frames: Vec<&str> = key.split(';').collect();
        if frames.is_empty() {
            continue;
        }

        // total: each distinct frame in this stack gets `count` once.
        let mut seen = std::collections::HashSet::new();
        for &frame in &frames {
            if seen.insert(frame) {
                *total.entry(frame).or_insert(0) += count;
            }
        }

        // own: the leaf frame (last, current function).
        if let Some(&leaf) = frames.last() {
            *own.entry(leaf).or_insert(0) += count;
        }
    }

    let mut stats: Vec<FnStat> = total
        .into_iter()
        .map(|(function, total)| FnStat {
            function: function.to_string(),
            own: own.get(function).copied().unwrap_or(0),
            total,
        })
        .collect();

    // Sort by own desc, then total desc, then name for stable output.
    stats.sort_by(|a, b| {
        b.own
            .cmp(&a.own)
            .then(b.total.cmp(&a.total))
            .then(a.function.cmp(&b.function))
    });
    stats
}

/// Render the live top screen (caller is responsible for clearing the terminal first).
pub fn render(agg: &Aggregator, label: &str, elapsed: f64, max_rows: usize) -> String {
    let mut out = String::new();
    let successful = agg.successful_samples();

    out.push_str(&format!(
        "tracemoon top — {}   ({:.1}s, {} samples, {:.1}% ok)\n",
        label,
        elapsed,
        agg.total_samples(),
        agg.success_rate(),
    ));
    out.push_str("Press Ctrl+C to stop.\n\n");
    out.push_str("  %Own   %Total  Function\n");

    if successful == 0 {
        out.push_str("  (waiting for samples…)\n");
        return out;
    }

    let denom = successful as f64;
    for stat in function_stats(agg.get_stacks()).into_iter().take(max_rows) {
        let own_pct = (stat.own as f64 / denom) * 100.0;
        let total_pct = (stat.total as f64 / denom) * 100.0;
        out.push_str(&format!(
            "  {:>5.1}%  {:>5.1}%  {}\n",
            own_pct, total_pct, stat.function
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_function_stats_own_and_total() {
        let mut stacks = HashMap::new();
        // main -> a -> b   (b is leaf)   x10
        stacks.insert("main;a;b".to_string(), 10u64);
        // main -> a        (a is leaf)   x5
        stacks.insert("main;a".to_string(), 5u64);

        let stats = function_stats(&stacks);
        let get = |name: &str| stats.iter().find(|s| s.function == name).unwrap();

        // main appears in both stacks -> total 15, never a leaf -> own 0
        assert_eq!(get("main").total, 15);
        assert_eq!(get("main").own, 0);

        // a appears in both -> total 15, leaf in the second -> own 5
        assert_eq!(get("a").total, 15);
        assert_eq!(get("a").own, 5);

        // b appears once -> total 10, leaf -> own 10
        assert_eq!(get("b").total, 10);
        assert_eq!(get("b").own, 10);

        // sorted by own desc: b (10) first
        assert_eq!(stats[0].function, "b");
    }

    #[test]
    fn test_function_counted_once_per_stack_for_total() {
        let mut stacks = HashMap::new();
        // recursive-ish: f appears twice in one stack
        stacks.insert("main;f;f".to_string(), 3u64);
        let stats = function_stats(&stacks);
        let f = stats.iter().find(|s| s.function == "f").unwrap();
        // total counted once per stack, not twice
        assert_eq!(f.total, 3);
        // f is the leaf
        assert_eq!(f.own, 3);
    }
}
