//! Portable, versioned heap graph. Addresses identify objects only within this snapshot.

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, TracemoonError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub tool_version: String,
    pub lua_version: String,
    #[serde(default = "default_abi")]
    pub lua_abi: String,
    #[serde(default)]
    pub capture_method: Option<String>,
    pub pid: i32,
    pub captured_unix_secs: u64,
    pub pause_ms: u64,
    pub read_bytes: u64,
    pub limits: CaptureLimits,
    pub pointer_width: u8,
    pub target_endian: String,
    pub incomplete: bool,
    pub diagnostics: Vec<String>,
    pub heaps: Vec<Heap>,
}

fn default_abi() -> String {
    "lua54".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaptureLimits {
    pub pause_ms: u64,
    pub objects: usize,
    pub edges: usize,
    pub read_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heap {
    pub global: u64,
    pub services: Vec<String>,
    pub gc_state: u8,
    pub roots: Vec<Edge>,
    pub objects: Vec<Object>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Object {
    pub address: u64,
    pub kind: String,
    #[serde(default)]
    pub shared: bool,
    pub shallow_bytes: u64,
    pub summary: String,
    pub entries: u64,
    pub preview: Vec<String>,
    pub edges: Vec<Edge>,
    pub metatable: u64,
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub target: u64,
    pub label: String,
    pub strength: Strength,
    pub role: Role,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Reference,
    TableKey,
    TableValue,
    ArrayValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Strength {
    Strong,
    Weak,
    Conditional,
    Unknown,
}

impl Snapshot {
    pub fn is_core(&self) -> bool {
        self.capture_method
            .as_deref()
            .is_some_and(|m| m.starts_with("Linux ELF coredump"))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        // A unique sibling temporary file avoids clobbering another capture's temp file.
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(format!(".{}.tmp", std::process::id()));
        let tmp = std::path::PathBuf::from(tmp);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&tmp)?;
        let result = (|| {
            let mut writer = BufWriter::new(file);
            serde_json::to_writer(&mut writer, self).map_err(json_error)?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        })();
        // Only remove our temporary file if it was actually created by this call.
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    pub fn read(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        if file.metadata()?.len() > 2 * 1024 * 1024 * 1024 {
            return Err(TracemoonError::Config(
                "Snapshot exceeds 2 GiB offline input limit".into(),
            ));
        }
        let snapshot: Self = serde_json::from_reader(BufReader::new(file)).map_err(json_error)?;
        if snapshot.schema_version != 1 || snapshot.lua_version != "5.4" {
            return Err(TracemoonError::Config(
                "Unsupported heap snapshot schema or Lua version".into(),
            ));
        }
        for heap in &snapshot.heaps {
            let mut seen = std::collections::HashSet::new();
            if heap.objects.iter().any(|o| !seen.insert(o.address)) {
                return Err(TracemoonError::Config(
                    "Duplicate object addresses in snapshot".into(),
                ));
            }
        }
        Ok(snapshot)
    }
}

fn json_error(e: serde_json::Error) -> TracemoonError {
    TracemoonError::Other(format!("Heap snapshot JSON: {e}"))
}
