//! One-shot `dump` formatting: render captured stacks for stdout.

use crate::lua::StackFrame;

/// Format captured stacks (each deepest-first, as returned by the walker) for `dump`.
///
/// For skynet stacks the synthetic `[service:name]` root frame (last element) becomes the
/// per-stack header; remaining frames are printed current-first (`#0` = executing function).
pub fn format(stacks: &[Vec<StackFrame>], label: &str, pid: i32) -> String {
    let mut out = String::new();
    out.push_str(&format!("tracemoon dump — {} (pid {})\n", label, pid));

    if stacks.is_empty() {
        out.push_str("\n(no stack captured)\n");
        return out;
    }

    for (i, stack) in stacks.iter().enumerate() {
        out.push('\n');

        // A synthetic service-root frame (raw_label) is the shallowest (last) frame.
        let (header, frames): (String, &[StackFrame]) = match stack.last() {
            Some(f) if f.raw_label.is_some() => {
                (f.raw_label.clone().unwrap(), &stack[..stack.len() - 1])
            }
            _ => (format!("Stack #{}", i), stack.as_slice()),
        };

        out.push_str(&header);
        out.push('\n');

        for (depth, frame) in frames.iter().enumerate() {
            out.push_str(&format!("    #{:<2} {}\n", depth, frame.format()));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_single_stack() {
        let stacks = vec![vec![
            StackFrame::lua_function("a.lua".to_string(), "inner".to_string(), 10),
            StackFrame::c_function("main".to_string(), 0x1000),
        ]];
        let out = format(&stacks, "lua", 42);
        assert!(out.contains("pid 42"));
        assert!(out.contains("Stack #0"));
        assert!(out.contains("#0  a.lua:10:inner"));
        assert!(out.contains("#1  [C]:main"));
    }

    #[test]
    fn test_format_skynet_service_header() {
        let stacks = vec![vec![
            StackFrame::lua_function("bench.lua".to_string(), "run".to_string(), 5),
            StackFrame::root("[service:bench_seri]".to_string()),
        ]];
        let out = format(&stacks, "skynet", 7);
        assert!(out.contains("[service:bench_seri]"));
        assert!(out.contains("#0  bench.lua:5:run"));
        // the root label must not also appear as a numbered frame
        assert!(!out.contains("#1  [service:bench_seri]"));
    }

    #[test]
    fn test_format_empty() {
        let out = format(&[], "lua", 1);
        assert!(out.contains("(no stack captured)"));
    }
}
