//! Context stored alongside a profile, including whether the Lua layout was confirmed.

use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::lua::LuaVersion;
use crate::output::Aggregator;

pub struct ProfileMetadata<'a> {
    pub config: &'a Config,
    pub pid: i32,
    pub lua_version: LuaVersion,
    pub layout_confirmed: bool,
    pub started_at: SystemTime,
    pub elapsed: Duration,
    pub aggregator: &'a Aggregator,
}

impl ProfileMetadata<'_> {
    pub fn write(&self, path: &str) -> io::Result<()> {
        let command = self
            .config
            .command
            .as_ref()
            .map_or("null".into(), |args| string_array(args));
        let started_at = self
            .started_at
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_secs_f64();
        let agg = self.aggregator;
        let json = format!(
            concat!(
                "{{\n  \"schema_version\": 1,\n  \"tracemoon\": {},\n",
                "  \"pid\": {},\n  \"command\": {},\n  \"services\": {},\n",
                "  \"lua_version\": {},\n  \"layout_confirmed\": {},\n",
                "  \"sample_rate\": {},\n  \"started_at_unix_secs\": {},\n",
                "  \"duration_secs\": {},\n  \"ticks\": {},\n",
                "  \"failed_ticks\": {},\n  \"sampled_stacks\": {},\n",
                "  \"success_rate\": {}\n}}\n"
            ),
            quote(env!("CARGO_PKG_VERSION")),
            self.pid,
            command,
            string_array(&self.config.services),
            quote(&self.lua_version.to_string()),
            self.layout_confirmed,
            self.config.sample_rate,
            started_at,
            self.elapsed.as_secs_f64(),
            agg.total_ticks(),
            agg.failed_ticks(),
            agg.sampled_stacks(),
            agg.success_rate(),
        );
        std::fs::write(path, json)
    }
}

fn string_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|s| quote(s))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn quote(value: &str) -> String {
    use std::fmt::Write;
    let mut result = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\u{0}'..='\u{1f}' => {
                // Writing into a String cannot fail.
                write!(result, "\\u{:04x}", ch as u32).expect("String write");
            }
            _ => result.push(ch),
        }
    }
    result.push('"');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_strings_escape_paths_commands_and_all_controls() {
        assert_eq!(
            quote("a\"\\\n\t\0中"),
            "\"a\\\"\\\\\\u000a\\u0009\\u0000中\""
        );
        for ch in '\u{0}'..='\u{1f}' {
            assert!(!quote(&ch.to_string()).contains(ch));
        }
    }

    #[test]
    fn sidecar_preserves_target_context_and_tick_denominator() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.txt.meta.json");
        let config = Config {
            command: Some(vec!["lua".into(), "a\"b.lua".into()]),
            services: vec![":01000012".into()],
            ..Config::default()
        };
        let mut aggregator = Aggregator::new();
        aggregator.add_tick(vec![
            vec![crate::lua::StackFrame::root("one".into())],
            vec![crate::lua::StackFrame::root("two".into())],
        ]);
        aggregator.add_failed();
        ProfileMetadata {
            config: &config,
            pid: 123,
            lua_version: LuaVersion::Lua54,
            layout_confirmed: false,
            started_at: UNIX_EPOCH + Duration::from_secs(100),
            elapsed: Duration::from_millis(1500),
            aggregator: &aggregator,
        }
        .write(path.to_str().unwrap())
        .unwrap();
        let json = std::fs::read_to_string(path).unwrap();
        for expected in [
            "\"command\": [\"lua\", \"a\\\"b.lua\"]",
            "\"layout_confirmed\": false",
            "\"duration_secs\": 1.5",
            "\"ticks\": 2",
            "\"sampled_stacks\": 2",
            "\"success_rate\": 50",
        ] {
            assert!(json.contains(expected), "missing {expected} in {json}");
        }
    }
}
