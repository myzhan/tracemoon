//! Linux ELF64 core input. Missing PT_LOAD bytes are errors, never zero-filled:
//! p_memsz may describe mappings deliberately excluded from the dump.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use object::{Object, ObjectSymbol};

use crate::error::{Result, TracemoonError};
use crate::process::ProcessMemoryReader;

use super::{capture, layout, snapshot};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    pub exe: Option<PathBuf>,
    pub lua54: bool,
    pub state: Option<usize>,
    pub services: Vec<String>,
}

#[derive(Clone, Debug)]
struct Segment {
    address: u64,
    memory: u64,
    offset: u64,
    size: u64,
    flags: u32,
    kind: u32,
}

struct Mapping {
    start: u64,
    offset: u64,
    path: String,
}

struct Image {
    file: File,
    loads: Vec<Segment>,
    mappings: Vec<Mapping>,
    phdr: Option<u64>,
    machine: u16,
    executable: Option<(Vec<u8>, Vec<Segment>, u64)>,
}

fn bad(message: impl Into<String>) -> TracemoonError {
    TracemoonError::Other(format!("ELF core: {}", message.into()))
}

fn word(bytes: &[u8], offset: usize) -> Result<u64> {
    bytes
        .get(offset..offset.saturating_add(8))
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| bad("truncated 64-bit field"))
}
fn dword(bytes: &[u8], offset: usize) -> Result<u32> {
    bytes
        .get(offset..offset.saturating_add(4))
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| bad("truncated 32-bit field"))
}
fn half(bytes: &[u8], offset: usize) -> Result<u16> {
    bytes
        .get(offset..offset.saturating_add(2))
        .and_then(|b| b.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| bad("truncated 16-bit field"))
}

fn header(bytes: &[u8], core: bool) -> Result<u16> {
    if !cfg!(all(target_pointer_width = "64", target_endian = "little")) {
        return Err(bad(
            "offline core analysis requires a little-endian 64-bit host",
        ));
    }
    if bytes.len() < 64
        || &bytes[..4] != b"\x7fELF"
        || bytes[4] != 2
        || bytes[5] != 1
        || bytes[6] != 1
    {
        return Err(bad("expected a little-endian ELF64 file"));
    }
    let kind = half(bytes, 16)?;
    if (core && kind != 4) || (!core && kind != 2 && kind != 3) {
        return Err(bad(if core {
            "ELF input is not ET_CORE"
        } else {
            "--exe must be an ELF executable"
        }));
    }
    let machine = half(bytes, 18)?;
    if !matches!(machine, 62 | 183) {
        return Err(bad("only Linux x86_64 and aarch64 cores are supported"));
    }
    Ok(machine)
}

fn program_headers(file: &File, h: &[u8]) -> Result<Vec<Segment>> {
    let base = word(h, 32)?;
    let stride = half(h, 54)? as u64;
    let count = half(h, 56)? as u64;
    if stride != 56 || count == 0 || count == 0xffff {
        return Err(bad("invalid or unsupported program-header table"));
    }
    let end = base
        .checked_add(stride * count)
        .ok_or_else(|| bad("program-header overflow"))?;
    if end > file.metadata()?.len() {
        return Err(bad("truncated program-header table"));
    }
    let mut result = Vec::new();
    for i in 0..count {
        let mut bytes = [0; 56];
        file.read_exact_at(&mut bytes, base + i * stride)?;
        let s = Segment {
            kind: dword(&bytes, 0)?,
            flags: dword(&bytes, 4)?,
            offset: word(&bytes, 8)?,
            address: word(&bytes, 16)?,
            size: word(&bytes, 32)?,
            memory: word(&bytes, 40)?,
        };
        if s.address.checked_add(s.memory).is_none()
            || s.offset.checked_add(s.size).is_none()
            || (s.kind == 1 && s.size > s.memory)
        {
            return Err(bad("invalid segment range"));
        }
        result.push(s);
    }
    Ok(result)
}

impl Image {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let mut h = [0; 64];
        file.read_exact_at(&mut h, 0)?;
        let machine = header(&h, true)?;
        let segments = program_headers(&file, &h)?;
        let mut image = Self {
            file,
            loads: Vec::new(),
            mappings: Vec::new(),
            phdr: None,
            machine,
            executable: None,
        };
        let mut note_bytes = 0u64;
        for s in segments {
            if s.kind == 1 && s.memory != 0 {
                image.loads.push(s);
            } else if s.kind == 4 {
                note_bytes = note_bytes.saturating_add(s.size);
                if note_bytes > 64 * 1024 * 1024 {
                    return Err(bad("core notes exceed 64 MiB"));
                }
                let mut notes = vec![0; s.size as usize];
                image.file.read_exact_at(&mut notes, s.offset)?;
                image.notes(&notes)?;
            }
        }
        image.loads.sort_by_key(|s| s.address);
        if image.loads.is_empty() {
            return Err(bad("no loadable memory segments"));
        }
        for pair in image.loads.windows(2) {
            if pair[0].address + pair[0].memory > pair[1].address {
                return Err(bad("overlapping memory segments"));
            }
        }
        Ok(image)
    }

    fn notes(&mut self, bytes: &[u8]) -> Result<()> {
        let mut offset = 0usize;
        while bytes.len().saturating_sub(offset) >= 12 {
            let namesz = dword(bytes, offset)? as usize;
            let descsz = dword(bytes, offset + 4)? as usize;
            let kind = dword(bytes, offset + 8)?;
            let name = offset + 12;
            let desc = name
                .checked_add(namesz.saturating_add(3) & !3)
                .ok_or_else(|| bad("note overflow"))?;
            let next = desc
                .checked_add(descsz.saturating_add(3) & !3)
                .ok_or_else(|| bad("note overflow"))?;
            let owner = bytes
                .get(name..name.saturating_add(namesz))
                .ok_or_else(|| bad("truncated note name"))?;
            let data = bytes
                .get(desc..desc.saturating_add(descsz))
                .ok_or_else(|| bad("truncated note data"))?;
            if owner.starts_with(b"CORE\0") || owner == b"CORE" {
                if kind == 0x46494c45 {
                    self.file_note(data)?;
                }
                if kind == 6 {
                    // `split_first_chunk` keeps the fixed-width walk explicit without
                    // tripping clippy's `chunks_exact` → `as_chunks` suggestion (Rust 1.88+).
                    let mut entries = data;
                    while let Some((entry, rest)) = entries.split_first_chunk::<16>() {
                        if word(entry, 0)? == 3 {
                            self.phdr = Some(word(entry, 8)?);
                        }
                        entries = rest;
                    }
                }
            }
            offset = next;
        }
        Ok(())
    }

    fn file_note(&mut self, data: &[u8]) -> Result<()> {
        let count = word(data, 0)?;
        let page = word(data, 8)?;
        if count > 1_000_000 || page == 0 || !page.is_power_of_two() {
            return Err(bad("invalid NT_FILE header"));
        }
        let end = 16 + count as usize * 24;
        let names = data
            .get(end..)
            .ok_or_else(|| bad("truncated NT_FILE mappings"))?;
        let mut names = names.split(|b| *b == 0);
        for i in 0..count as usize {
            let offset = 16 + i * 24;
            let start = word(data, offset)?;
            let end = word(data, offset + 8)?;
            let file_offset = word(data, offset + 16)?
                .checked_mul(page)
                .ok_or_else(|| bad("NT_FILE offset overflow"))?;
            let name = names
                .next()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad("missing NT_FILE path"))?;
            if end <= start {
                return Err(bad("invalid NT_FILE mapping"));
            }
            self.mappings.push(Mapping {
                start,
                offset: file_offset,
                path: String::from_utf8_lossy(name).into_owned(),
            });
        }
        Ok(())
    }

    fn attach_executable(&mut self, path: &Path) -> Result<Option<usize>> {
        let file = File::open(path)?;
        if file.metadata()?.len() > 512 * 1024 * 1024 {
            return Err(bad("--exe exceeds 512 MiB"));
        }
        let bytes = std::fs::read(path)?;
        let machine = header(&bytes, false)?;
        if machine != self.machine {
            return Err(bad("--exe architecture differs from core"));
        }
        let segments = program_headers(&file, &bytes)?;
        let phoff = word(&bytes, 32)?;
        let linked_phdr = segments
            .iter()
            .find(|s| s.kind == 1 && phoff >= s.offset && phoff < s.offset + s.size)
            .map(|s| s.address + (phoff - s.offset));
        let bias = self
            .phdr
            .zip(linked_phdr)
            .and_then(|(runtime, linked)| runtime.checked_sub(linked))
            .or_else(|| {
                self.mappings
                    .iter()
                    .filter(|m| Path::new(&m.path).file_name() == path.file_name())
                    .find_map(|m| {
                        segments
                            .iter()
                            .filter(|s| {
                                s.kind == 1 && m.offset >= s.offset && m.offset < s.offset + s.size
                            })
                            .find_map(|s| m.start.checked_sub(s.address + (m.offset - s.offset)))
                    })
            })
            .ok_or_else(|| {
                bad("cannot locate --exe in core (missing AT_PHDR / NT_FILE mapping)")
            })?;
        // Build-id notes are immutable. Compare dumped bytes before enabling any file
        // fallback; a different build can have identical symbol names and wrong offsets.
        for s in segments
            .iter()
            .filter(|s| s.kind == 4 && s.size <= 1024 * 1024)
        {
            if let Some(address) = bias.checked_add(s.address) {
                if let Ok(dumped) = self.read_memory(address as usize, s.size as usize) {
                    let expected = bytes
                        .get(s.offset as usize..(s.offset + s.size) as usize)
                        .ok_or_else(|| bad("truncated executable note"))?;
                    if dumped != expected {
                        return Err(bad(
                            "--exe notes differ from dumped executable; provide the matching build",
                        ));
                    }
                }
            }
        }
        let obj = object::File::parse(bytes.as_slice()).map_err(|e| bad(format!("--exe: {e}")))?;
        let h = obj
            .symbols()
            .chain(obj.dynamic_symbols())
            .find(|s| !s.is_undefined() && s.address() != 0 && s.name().ok() == Some("H"))
            .map(|s| {
                bias.checked_add(s.address())
                    .ok_or_else(|| bad("symbol address overflow"))
            })
            .transpose()?;
        // H can live in anonymous BSS beyond NT_FILE's last file-backed page.
        // Validate its contents through the memory reader, not NT_FILE membership.
        self.executable = Some((bytes, segments, bias));
        Ok(h.map(|a| a as usize))
    }

    fn detect_version(&self) -> Result<()> {
        fn version(bytes: &[u8]) -> Option<bool> {
            if bytes.windows(7).any(|s| s == b"Lua 5.5") {
                Some(false)
            } else if bytes.windows(7).any(|s| s == b"Lua 5.4") {
                Some(true)
            } else {
                None
            }
        }
        let mut found = self.executable.as_ref().and_then(|(b, _, _)| version(b));
        let mut budget = 64 * 1024 * 1024;
        for s in self.loads.iter().filter(|s| s.flags & 2 == 0) {
            let len = s.size.min(budget).min(8 * 1024 * 1024) as usize;
            let mut bytes = vec![0; len];
            if self.file.read_exact_at(&mut bytes, s.offset).is_ok() {
                match version(&bytes) {
                    Some(false) => return Err(bad("Lua 5.5 heap analysis is not supported")),
                    Some(true) => found = Some(true),
                    None => {}
                }
            }
            budget -= len as u64;
            if budget == 0 {
                break;
            }
        }
        match found {
            Some(true) => Ok(()),
            Some(false) => Err(bad("Lua 5.5 heap analysis is not supported")),
            None => Err(bad("cannot detect Lua version; provide --exe or --lua-version 5.4 for a verified target")),
        }
    }
}

impl ProcessMemoryReader for Image {
    fn read_memory(&self, addr: usize, len: usize) -> Result<Vec<u8>> {
        crate::process::reader::validate_addr(addr, len)?;
        if len > 1024 * 1024 {
            return Err(bad("single read exceeds 1 MiB"));
        }
        let mut out = Vec::with_capacity(len);
        let mut address = addr as u64;
        while out.len() < len {
            let index = self.loads.partition_point(|s| s.address <= address);
            let s = index
                .checked_sub(1)
                .and_then(|i| self.loads.get(i))
                .filter(|s| address < s.address + s.memory)
                .ok_or_else(|| bad(format!("unmapped address 0x{address:x}")))?;
            let offset = address - s.address;
            let n = ((s.memory - offset) as usize).min(len - out.len());
            if offset < s.size {
                let n = n.min((s.size - offset) as usize);
                let base = out.len();
                out.resize(base + n, 0);
                self.file
                    .read_exact_at(&mut out[base..], s.offset + offset)?;
                address += n as u64;
            } else if let Some((bytes, segments, bias)) = &self.executable {
                // Only immutable, file-backed bytes from the supplied binary can be
                // recovered. Never reconstruct writable memory or omitted heap pages.
                let original = address.checked_sub(*bias);
                let source = segments.iter().find(|p| {
                    s.flags & 2 == 0
                        && p.kind == 1
                        && p.flags & 2 == 0
                        && original.is_some_and(|a| a >= p.address && a < p.address + p.size)
                });
                let p =
                    source.ok_or_else(|| bad(format!("missing dumped page at 0x{address:x}")))?;
                let delta = address - bias - p.address;
                let n = n.min((p.size - delta) as usize);
                let start = (p.offset + delta) as usize;
                out.extend_from_slice(
                    bytes
                        .get(start..start + n)
                        .ok_or_else(|| bad("truncated executable segment"))?,
                );
                address += n as u64;
            } else {
                return Err(bad(format!("missing dumped page at 0x{address:x}")));
            }
        }
        Ok(out)
    }
}

pub fn is_core(path: &Path) -> Result<bool> {
    let file = File::open(path)?;
    let mut magic = [0; 4];
    if file.read_exact_at(&mut magic, 0).is_err() {
        return Ok(false);
    }
    if magic.starts_with(&[0x1f, 0x8b])
        || magic == [0x28, 0xb5, 0x2f, 0xfd]
        || magic == [0xfd, 0x37, 0x7a, 0x58]
    {
        return Err(bad("compressed input is not supported; decompress the core first (for systemd, use coredumpctl dump -o core.dump)"));
    }
    Ok(&magic == b"\x7fELF")
}

pub fn read(path: &Path, options: &Options) -> Result<snapshot::Snapshot> {
    let mut image = Image::open(path)?;
    let skynet_mapping = image.mappings.iter().any(|m| {
        Path::new(&m.path)
            .file_name()
            .is_some_and(|n| n == "skynet")
    });
    if options.exe.is_none() && skynet_mapping {
        return Err(bad(
            "skynet cores require --exe with the matching unstripped skynet binary",
        ));
    }
    let h = options
        .exe
        .as_ref()
        .map(|p| image.attach_executable(p))
        .transpose()?
        .flatten();
    if skynet_mapping && h.is_none() {
        return Err(bad(
            "cannot resolve skynet H; supply a matching unstripped --exe",
        ));
    }
    if !options.lua54 {
        image.detect_version()?;
    }
    if h.is_some() && options.state.is_some() {
        return Err(bad("--state is only for single-VM cores"));
    }
    let ranges: Vec<_> = image
        .loads
        .iter()
        .filter(|s| s.flags & 6 == 6 && s.size != 0)
        .map(|s| (s.address as usize, s.size as usize))
        .collect();
    let mut diagnostics = Vec::new();
    let file_len = image.file.metadata()?.len();
    if image.loads.iter().any(|s| s.offset + s.size > file_len) {
        diagnostics.push("Core file is truncated; some dumped pages are unavailable".into());
    }
    if image
        .loads
        .iter()
        .any(|s| s.flags & 2 != 0 && s.size < s.memory)
    {
        diagnostics.push(
            "Some writable mappings were excluded from the core; heap coverage may be incomplete"
                .into(),
        );
    }
    let r = capture::BudgetReader {
        inner: Box::new(image),
        deadline: Instant::now() + Duration::from_secs(60),
        max_bytes: 1024 * 1024 * 1024,
        bytes: AtomicU64::new(0),
        running: Arc::new(AtomicBool::new(true)),
    };
    let heaps = capture::discover_ranges(&r, &ranges, h, &options.services, options.state)?;
    let mut s = snapshot::Snapshot {
        schema_version: 1,
        tool_version: env!("CARGO_PKG_VERSION").into(),
        lua_version: "5.4".into(),
        lua_abi: if h.is_some() { "skynet-lua54" } else { "lua54" }.into(),
        capture_method: Some(
            "Linux ELF coredump (offline); dump time and process pause duration unavailable".into(),
        ),
        pid: 0,
        captured_unix_secs: 0,
        pause_ms: 0,
        read_bytes: 0,
        limits: snapshot::CaptureLimits {
            pause_ms: 0,
            objects: 1_000_000,
            edges: 2_000_000,
            read_bytes: r.max_bytes,
        },
        pointer_width: 64,
        target_endian: "little".into(),
        incomplete: !diagnostics.is_empty(),
        diagnostics,
        heaps: Vec::new(),
    };
    capture::collect(
        &r,
        heaps,
        &mut s,
        1_000_000,
        2_000_000,
        if h.is_some() {
            layout::HeapLayout::skynet54()
        } else {
            layout::HeapLayout::lua54()
        },
    );
    s.read_bytes = r.bytes.load(Ordering::Relaxed);
    Ok(s)
}

#[cfg(test)]
pub(super) fn fixture(path: &Path, regions: &[(u64, Vec<u8>, u64, u32)], notes: &[u8]) {
    let count = regions.len() + usize::from(!notes.is_empty());
    let mut bytes = vec![0; 64 + count * 56];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16..18].copy_from_slice(&4u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&(count as u16).to_le_bytes());
    for (i, (address, data, memory, flags)) in regions.iter().enumerate() {
        let mut p = [0u8; 56];
        p[..4].copy_from_slice(&1u32.to_le_bytes());
        p[4..8].copy_from_slice(&flags.to_le_bytes());
        p[8..16].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        p[16..24].copy_from_slice(&address.to_le_bytes());
        p[32..40].copy_from_slice(&(data.len() as u64).to_le_bytes());
        p[40..48].copy_from_slice(&memory.to_le_bytes());
        bytes[64 + i * 56..120 + i * 56].copy_from_slice(&p);
        bytes.extend_from_slice(data);
    }
    if !notes.is_empty() {
        let offset = 64 + regions.len() * 56;
        let start = bytes.len() as u64;
        bytes[offset..offset + 4].copy_from_slice(&4u32.to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&start.to_le_bytes());
        bytes[offset + 32..offset + 40].copy_from_slice(&(notes.len() as u64).to_le_bytes());
        bytes.extend_from_slice(notes);
    }
    std::fs::write(path, bytes).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(kind: u32, data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&5u32.to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(b"CORE\0\0\0\0");
        bytes.extend_from_slice(data);
        bytes.resize((bytes.len() + 3) & !3, 0);
        bytes
    }

    /// Tiny executable with a real ELF symbol table, two PT_LOADs and an immutable
    /// GNU note. This checks relocation without depending on the host's ELF linker.
    fn executable(path: &Path, base: u64) -> Vec<u8> {
        let mut b = vec![0; 0x400];
        b[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        b[16..18].copy_from_slice(&(if base == 0 { 3u16 } else { 2u16 }).to_le_bytes());
        b[18..20].copy_from_slice(&62u16.to_le_bytes());
        b[20..24].copy_from_slice(&1u32.to_le_bytes());
        b[32..40].copy_from_slice(&64u64.to_le_bytes());
        b[40..48].copy_from_slice(&0x300u64.to_le_bytes());
        b[52..54].copy_from_slice(&64u16.to_le_bytes());
        b[54..56].copy_from_slice(&56u16.to_le_bytes());
        b[56..58].copy_from_slice(&3u16.to_le_bytes());
        b[58..60].copy_from_slice(&64u16.to_le_bytes());
        b[60..62].copy_from_slice(&4u16.to_le_bytes());
        b[62..64].copy_from_slice(&2u16.to_le_bytes());
        for (i, (kind, flags, offset, address, size, memory)) in [
            (1u32, 4u32, 0u64, base, 0x200u64, 0x200u64),
            (1, 6, 0x200, base + 0x1000, 8, 16),
            (4, 4, 0x180, base + 0x180, 20, 20),
        ]
        .into_iter()
        .enumerate()
        {
            let p = 64 + i * 56;
            b[p..p + 4].copy_from_slice(&kind.to_le_bytes());
            b[p + 4..p + 8].copy_from_slice(&flags.to_le_bytes());
            for (off, value) in [(8, offset), (16, address), (32, size), (40, memory)] {
                b[p + off..p + off + 8].copy_from_slice(&value.to_le_bytes());
            }
        }
        for (offset, value) in [(0x180, 4u32), (0x184, 4), (0x188, 3)] {
            b[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        b[0x18c..0x190].copy_from_slice(b"GNU\0");
        b[0x190..0x194].copy_from_slice(b"abcd");
        b[0x260..0x263].copy_from_slice(b"\0H\0");
        b[0x298..0x29c].copy_from_slice(&1u32.to_le_bytes());
        b[0x29c] = 0x11;
        b[0x29e..0x2a0].copy_from_slice(&1u16.to_le_bytes());
        b[0x2a0..0x2a8].copy_from_slice(&(base + 0x1000).to_le_bytes());
        for (index, kind, address, offset, size, link, stride) in [
            (1, 1u32, base + 0x1000, 0x200u64, 8u64, 0u32, 0u64),
            (2, 3, 0, 0x260, 3, 0, 0),
            (3, 2, 0, 0x280, 48, 2, 24),
        ] {
            let s = 0x300 + index * 64;
            b[s + 4..s + 8].copy_from_slice(&kind.to_le_bytes());
            for (off, value) in [(16, address), (24, offset), (32, size), (56, stride)] {
                b[s + off..s + off + 8].copy_from_slice(&value.to_le_bytes());
            }
            b[s + 40..s + 44].copy_from_slice(&link.to_le_bytes());
        }
        std::fs::write(path, &b).unwrap();
        b
    }

    #[test]
    fn resolves_pie_and_non_pie_h_and_rejects_a_different_executable_build() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("renamed-skynet");
        let core = dir.path().join("core");
        for base in [0, 0x400000] {
            let b = executable(&exe, base);
            let runtime = if base == 0 { 0x700000 } else { base };
            let mut auxv = Vec::new();
            for n in [3u64, runtime + 64, 0, 0] {
                auxv.extend_from_slice(&n.to_le_bytes());
            }
            fixture(
                &core,
                &[
                    (runtime, b[..0x200].to_vec(), 0x200, 4),
                    (runtime + 0x1000, vec![0; 8], 16, 6),
                ],
                &note(6, &auxv),
            );
            let mut image = Image::open(&core).unwrap();
            assert_eq!(
                image.attach_executable(&exe).unwrap(),
                Some((runtime + 0x1000) as usize)
            );
            let mut without_auxv = Image::open(&core).unwrap();
            without_auxv.phdr = None;
            without_auxv.mappings.push(Mapping {
                start: runtime,
                offset: 0,
                path: exe.to_string_lossy().into_owned(),
            });
            assert_eq!(
                without_auxv.attach_executable(&exe).unwrap(),
                Some((runtime + 0x1000) as usize)
            );
            assert!(image.read_memory((runtime + 0x1008) as usize, 1).is_err());
            // Excluded read-only content can come from the explicit executable.
            fixture(
                &core,
                &[
                    (runtime, vec![], 0x200, 4),
                    (runtime + 0x1000, vec![], 16, 6),
                ],
                &note(6, &auxv),
            );
            let mut image = Image::open(&core).unwrap();
            image.attach_executable(&exe).unwrap();
            assert_eq!(
                image.read_memory((runtime + 0x190) as usize, 4).unwrap(),
                b"abcd"
            );
            assert!(image.read_memory((runtime + 0x1000) as usize, 8).is_err());
            // Runtime mprotect can make originally immutable bytes mutable.
            fixture(&core, &[(runtime, vec![], 0x200, 6)], &note(6, &auxv));
            let mut image = Image::open(&core).unwrap();
            image.attach_executable(&exe).unwrap();
            assert!(image.read_memory((runtime + 0x190) as usize, 4).is_err());
            fixture(
                &core,
                &[(runtime, b[..0x200].to_vec(), 0x200, 4)],
                &note(6, &auxv),
            );
            let mut wrong = b;
            wrong[0x190] = b'x';
            std::fs::write(&exe, wrong).unwrap();
            assert!(Image::open(&core)
                .unwrap()
                .attach_executable(&exe)
                .unwrap_err()
                .to_string()
                .contains("matching build"));
        }
    }

    #[test]
    fn reads_virtual_addresses_across_segments_but_never_fills_missing_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("core");
        fixture(
            &path,
            &[
                (0x1000, b"abcd".to_vec(), 4, 6),
                (0x1004, b"ef".to_vec(), 4, 6),
            ],
            &[],
        );
        let image = Image::open(&path).unwrap();
        assert_eq!(image.read_memory(0x1002, 4).unwrap(), b"cdef");
        assert!(image
            .read_memory(0x1004, 4)
            .unwrap_err()
            .to_string()
            .contains("missing dumped page"));
        assert!(image.read_memory(0, 8).is_err());
        assert!(image.read_memory(usize::MAX - 1, 8).is_err());
        assert!(image.read_memory(0x2000, 1).is_err());
    }

    #[test]
    fn validates_core_format_and_rejects_overlapping_or_truncated_headers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("core");
        fixture(
            &path,
            &[(0x1000, vec![0; 8], 8, 6), (0x1004, vec![0; 8], 8, 6)],
            &[],
        );
        assert!(Image::open(&path)
            .err()
            .unwrap()
            .to_string()
            .contains("overlapping"));
        fixture(&path, &[(0x1000, vec![0; 8], 8, 6)], &[]);
        let original = std::fs::read(&path).unwrap();
        for (offset, value) in [(4, 1), (5, 2), (16, 2), (18, 3)] {
            let mut bytes = original.clone();
            bytes[offset] = value;
            std::fs::write(&path, bytes).unwrap();
            assert!(Image::open(&path).is_err());
        }
        std::fs::write(&path, &original[..80]).unwrap();
        assert!(Image::open(&path).is_err());
    }

    #[test]
    fn parses_linux_file_mappings_and_auxv_without_opening_recorded_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("core");
        let mut files = Vec::new();
        for n in [1u64, 4096, 0x400000, 0x402000, 2] {
            files.extend_from_slice(&n.to_le_bytes());
        }
        files.extend_from_slice(b"/unavailable/original/skynet\0");
        let mut notes = note(0x46494c45, &files);
        let mut auxv = Vec::new();
        for n in [3u64, 0x400040, 0, 0] {
            auxv.extend_from_slice(&n.to_le_bytes());
        }
        notes.extend(note(6, &auxv));
        fixture(&path, &[(0x400000, vec![0; 4096], 4096, 4)], &notes);
        let image = Image::open(&path).unwrap();
        assert_eq!(image.phdr, Some(0x400040));
        assert_eq!(image.mappings[0].offset, 8192);
        assert_eq!(image.mappings[0].start, 0x400000);
        assert!(read(&path, &Options::default())
            .unwrap_err()
            .to_string()
            .contains("skynet cores require --exe"));
        fixture(
            &path,
            &[(0x1000, vec![0; 8], 8, 6)],
            &note(0x46494c45, &[0xff; 16]),
        );
        assert!(Image::open(&path).is_err());
    }
}
