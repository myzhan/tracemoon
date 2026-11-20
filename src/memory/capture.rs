//! Bounded Lua 5.4 heap capture. Only the stopped-process phase reads remote memory.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;

use crate::error::{Result, TracemoonError};
use crate::lua::{LuaLayout, LuaVersion};
use crate::process::{read_struct, ProcessMemoryReader, SkynetDiscovery};

use super::layout as l;
use super::snapshot::{Edge, Heap, Object, Role, Snapshot, Strength};

const MAX_SLOTS: usize = 4_000_000;
const PREVIEW: usize = 8;
const STRING_PREVIEW: usize = 96;

#[derive(Default)]
pub struct DiscoveredHeap {
    pub services: Vec<String>,
    pub active_threads: Vec<usize>,
}

pub struct BudgetReader {
    pub inner: Box<dyn ProcessMemoryReader>,
    pub deadline: Instant,
    pub max_bytes: u64,
    pub bytes: AtomicU64,
    pub running: Arc<AtomicBool>,
}

impl BudgetReader {
    pub fn check(&self) -> Result<()> {
        if !self.running.load(Ordering::Relaxed) || Instant::now() >= self.deadline {
            return Err(bad("Capture cancelled or read time budget exhausted"));
        }
        Ok(())
    }
}

impl ProcessMemoryReader for BudgetReader {
    fn read_memory(&self, addr: usize, len: usize) -> Result<Vec<u8>> {
        self.check()?;
        crate::process::reader::validate_addr(addr, len)?;
        if len > 1024 * 1024
            || self
                .bytes
                .fetch_add(len as u64, Ordering::Relaxed)
                .saturating_add(len as u64)
                > self.max_bytes
        {
            return Err(bad("Capture read-byte budget exhausted"));
        }
        let bytes = self.inner.read_memory(addr, len)?;
        if bytes.len() != len {
            return Err(bad("Short remote read during heap capture"));
        }
        Ok(bytes)
    }
}

pub fn at(base: usize, offset: usize) -> usize {
    base.saturating_add(offset)
}
fn bad(message: &str) -> TracemoonError {
    TracemoonError::Other(message.into())
}
fn ptr(r: &dyn ProcessMemoryReader, base: usize, offset: usize) -> Result<usize> {
    Ok(read_struct::<u64>(r, at(base, offset))? as usize)
}

/// Chunk scanning avoids one syscall per aligned word while the target is paused.
pub fn discover(
    r: &BudgetReader,
    pid: i32,
    h: Option<usize>,
    services: &[String],
    state: Option<usize>,
) -> Result<BTreeMap<usize, DiscoveredHeap>> {
    let ranges = if h.is_none() && state.is_none() && services.is_empty() {
        proc_maps::get_process_maps(pid as proc_maps::Pid)
            .map_err(|e| bad(&format!("maps: {e}")))?
            .into_iter()
            .filter(|m| m.is_read() && m.is_write())
            .map(|m| (m.start(), m.size()))
            .collect()
    } else {
        Vec::new()
    };
    discover_ranges(r, &ranges, h, services, state)
}

/// Both live mappings and core PT_LOAD ranges use the same validated VM discovery.
pub fn discover_ranges(
    r: &BudgetReader,
    ranges: &[(usize, usize)],
    h: Option<usize>,
    services: &[String],
    state: Option<usize>,
) -> Result<BTreeMap<usize, DiscoveredHeap>> {
    let mut heaps = BTreeMap::<usize, DiscoveredHeap>::new();
    let layout = if h.is_some() {
        LuaLayout::for_skynet(LuaVersion::Lua54)
    } else {
        LuaLayout::for_version(LuaVersion::Lua54)
    };
    if let Some(h) = h {
        let vms = SkynetDiscovery::new(r, &layout, h).enumerate()?;
        for vm in vms {
            r.check()?;
            if !services.is_empty()
                && !services
                    .iter()
                    .any(|s| s == &vm.label || s == &format!(":{:08x}", vm.handle))
            {
                continue;
            }
            let g = valid_global(r, vm.lua_state_addr)?;
            let heap = heaps.entry(g).or_default();
            heap.services
                .push(format!("{} ({:08x})", vm.label, vm.handle));
            heap.active_threads.push(vm.lua_state_addr);
        }
    } else if !services.is_empty() {
        return Err(bad("--service requires a skynet target"));
    } else if let Some(state) = state {
        heaps.insert(
            valid_global(r, state)?,
            DiscoveredHeap {
                services: vec!["single VM".into()],
                active_threads: Vec::new(),
            },
        );
    } else {
        for &(base, size) in ranges {
            let end = at(base, size);
            let mut start = base;
            while start < end {
                r.check()?;
                let len = (end - start).min(64 * 1024);
                // Include the next aligned word's tag/status when a state straddles
                // chunks; advance by len so candidates are not counted twice.
                match r.read_memory(start, (end - start).min(len + 16)) {
                    Ok(bytes) => {
                        for offset in (0..len.min(bytes.len().saturating_sub(10))).step_by(8) {
                            if bytes[offset + 8] == 8 && bytes[offset + 10] <= 6 {
                                if let Ok(g) = valid_global(r, at(start, offset)) {
                                    heaps.entry(g).or_insert_with(|| DiscoveredHeap {
                                        services: vec!["single VM".into()],
                                        active_threads: Vec::new(),
                                    });
                                }
                            }
                        }
                    }
                    Err(e) => {
                        r.check()?;
                        if r.bytes.load(Ordering::Relaxed) > r.max_bytes {
                            return Err(e);
                        }
                    }
                }
                start = at(start, len);
            }
        }
    }
    if heaps.is_empty() {
        return Err(TracemoonError::LuaStateNotFound);
    }
    Ok(heaps)
}

fn valid_global(r: &dyn ProcessMemoryReader, state: usize) -> Result<usize> {
    if read_struct::<u8>(r, at(state, 8))? != 8 {
        return Err(bad("Invalid Lua thread tag"));
    }
    let g = ptr(r, state, l::GLOBAL)?;
    let main = ptr(r, g, l::MAIN_THREAD)?;
    if ptr(r, main, l::GLOBAL)? != g
        || read_struct::<u8>(r, at(main, 8))? != 8
        || read_struct::<u8>(r, at(g, l::REGISTRY + 8))? != 0x45
    {
        return Err(bad("Lua global state validation failed"));
    }
    Ok(g)
}

pub fn collect(
    r: &BudgetReader,
    heaps: BTreeMap<usize, DiscoveredHeap>,
    snapshot: &mut Snapshot,
    max_objects: usize,
    max_edges: usize,
    layout: l::HeapLayout,
) {
    let mut decoder = Decoder {
        layout,
        r,
        edges: 0,
        max_edges,
        strings: HashMap::new(),
    };
    let mut total_objects = 0;
    for (g, discovered) in heaps {
        let mut heap = Heap {
            global: g as u64,
            services: discovered.services,
            gc_state: 255,
            roots: Vec::new(),
            objects: Vec::new(),
        };
        for thread in discovered.active_threads {
            heap.roots.push(edge(
                thread as u64,
                "skynet active coroutine",
                Strength::Strong,
            ));
        }
        if let Err(e) = collect_heap(
            &mut decoder,
            g,
            &mut heap,
            &mut total_objects,
            max_objects,
            snapshot,
        ) {
            diagnostic(snapshot, format!("heap 0x{g:x}: {e}"));
        }
        snapshot.heaps.push(heap);
    }
}

fn diagnostic(snapshot: &mut Snapshot, message: String) {
    snapshot.incomplete = true;
    if snapshot.diagnostics.len() < 64 {
        snapshot.diagnostics.push(message);
    }
}

fn collect_heap(
    d: &mut Decoder<'_>,
    g: usize,
    heap: &mut Heap,
    total: &mut usize,
    max_objects: usize,
    snapshot: &mut Snapshot,
) -> Result<()> {
    heap.gc_state = read_struct(d.r, at(g, d.layout.gc_state))?;
    if let Some(target) = value_target(&d.r.read_memory(at(g, l::REGISTRY), 16)?)? {
        heap.roots.push(edge(target, "registry", Strength::Strong));
    }
    for (off, label) in [
        (l::MAIN_THREAD, "main thread"),
        (l::MEMERR, "runtime memory-error string"),
    ] {
        heap.roots
            .push(edge(ptr(d.r, g, off)? as u64, label, Strength::Strong));
    }
    for (base, count, name) in [
        (l::TM_NAMES, 25, "runtime tag name"),
        (l::METATABLES, 9, "runtime type metatable"),
    ] {
        for i in 0..count {
            let target = ptr(d.r, g, base + i * 8)?;
            if target != 0 {
                heap.roots.push(edge(
                    target as u64,
                    &format!("{name}[{i}]"),
                    Strength::Strong,
                ));
            }
        }
    }
    let mut seen = HashSet::new();
    for (off, name) in l::LISTS {
        let mut address = ptr(d.r, g, off)?;
        while address != 0 {
            d.r.check()?;
            if *total >= max_objects {
                return Err(bad("Object budget exhausted"));
            }
            if !seen.insert(address) {
                return Err(bad("Repeated object in GC ownership chains"));
            }
            *total += 1;
            let next = ptr(d.r, address, 0)?;
            if name == "fixedgc" {
                heap.roots.push(edge(
                    address as u64,
                    "runtime fixed object",
                    Strength::Strong,
                ));
            }
            match d.object(address) {
                Ok(object) => heap.objects.push(object),
                Err(e) => {
                    diagnostic(snapshot, format!("object 0x{address:x} in {name}: {e}"));
                    if d.edges >= d.max_edges {
                        return Err(e);
                    }
                }
            }
            address = next;
        }
    }
    if let Some(shared_age) = d.layout.shared_age {
        // Skynet moves shared Proto/string objects outside a service's ownership
        // lists. Follow only the fork's explicit shared marker and supported types.
        // Do not treat arbitrary out-of-heap references as additional live objects.
        let mut pending: Vec<_> = heap
            .roots
            .iter()
            .chain(heap.objects.iter().flat_map(|o| &o.edges))
            .map(|edge| edge.target as usize)
            .collect();
        while let Some(address) = pending.pop() {
            if seen.contains(&address) {
                continue;
            }
            if *total >= max_objects {
                return Err(bad("Object budget exhausted"));
            }
            let tag = read_struct::<u8>(d.r, at(address, 8))?;
            let marked = read_struct::<u8>(d.r, at(address, 9))?;
            if marked & 7 != shared_age || !matches!(tag, 4 | 20 | 10) {
                diagnostic(
                    snapshot,
                    format!("unresolved non-shared reference 0x{address:x}"),
                );
                seen.insert(address);
                continue;
            }
            seen.insert(address);
            *total += 1;
            let object = d.object(address)?;
            pending.extend(object.edges.iter().map(|edge| edge.target as usize));
            heap.objects.push(object);
        }
    }
    Ok(())
}

pub fn edge(target: u64, label: &str, strength: Strength) -> Edge {
    Edge {
        target,
        label: label.into(),
        strength,
        role: Role::Reference,
    }
}
fn bits(bytes: &[u8]) -> u64 {
    u64::from_ne_bytes(bytes[..8].try_into().expect("fixed-width decoded field"))
}
fn value_target(bytes: &[u8]) -> Result<Option<u64>> {
    match bytes[8] {
        0 | 16 | 32 | 1 | 17 | 2 | 3 | 19 | 22 => Ok(None),
        0x44 | 0x54 | 0x45 | 0x46 | 0x66 | 0x47 | 0x48 => Ok(Some(bits(bytes))),
        _ => Err(bad("Invalid TValue tag in object references")),
    }
}

struct Decoder<'a> {
    layout: l::HeapLayout,
    r: &'a BudgetReader,
    edges: usize,
    max_edges: usize,
    strings: HashMap<usize, String>,
}

impl Decoder<'_> {
    fn table_edge(&mut self, o: &mut Object, target: u64, label: &str, role: Role) -> Result<()> {
        self.add(o, target, label)?;
        if target != 0 {
            if let Some(edge) = o.edges.last_mut() {
                edge.role = role;
            }
        }
        Ok(())
    }
    fn add(&mut self, o: &mut Object, target: u64, label: &str) -> Result<()> {
        if target == 0 {
            return Ok(());
        }
        self.edges += 1;
        if self.edges > self.max_edges {
            return Err(bad("Reference-edge budget exhausted"));
        }
        o.edges.push(edge(target, label, Strength::Strong));
        Ok(())
    }

    fn string(&mut self, addr: usize) -> Result<String> {
        if let Some(s) = self.strings.get(&addr) {
            return Ok(s.clone());
        }
        let tag = read_struct::<u8>(self.r, at(addr, 8))?;
        if tag != 4 && tag != 20 {
            return Err(bad("Invalid string tag"));
        }
        let len = if tag == 4 {
            read_struct::<u8>(self.r, at(addr, l::STRING_SHORT_LENGTH))? as usize
        } else {
            ptr(self.r, addr, self.layout.string_length)?
        };
        let bytes = self
            .r
            .read_memory(at(addr, self.layout.string_data), len.min(STRING_PREVIEW))?;
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if len > STRING_PREVIEW {
            text.push('…');
        }
        // Cache size is bounded independently of string lengths and graph size.
        if self.strings.len() < 100_000 {
            self.strings.insert(addr, text.clone());
        }
        Ok(text)
    }

    fn value(&mut self, bytes: &[u8]) -> Result<String> {
        let data = bits(bytes);
        Ok(match bytes[8] & 0x3f {
            0 => "nil".into(),
            1 => "false".into(),
            17 => "true".into(),
            3 => (data as i64).to_string(),
            19 => f64::from_bits(data).to_string(),
            4 | 20 => format!("{:?}", self.string(data as usize)?),
            _ if bytes[8] & 0x40 != 0 => format!("@0x{data:x}"),
            2 | 22 => format!("native:0x{data:x}"),
            _ => return Err(bad("Invalid TValue tag")),
        })
    }

    fn values(&mut self, o: &mut Object, base: usize, count: usize, label: &str) -> Result<()> {
        self.slots(base, count, 16, |this, bytes, i| {
            if let Some(target) = value_target(bytes)? {
                this.add(o, target, &format!("{label}[{i}]"))?;
            }
            Ok(())
        })
    }

    fn slots<F>(&mut self, base: usize, count: usize, stride: usize, mut f: F) -> Result<()>
    where
        F: FnMut(&mut Self, &[u8], usize) -> Result<()>,
    {
        if count > MAX_SLOTS {
            return Err(bad("Object array exceeds slot limit"));
        }
        let per_chunk = 64 * 1024 / stride;
        for first in (0..count).step_by(per_chunk) {
            let n = (count - first).min(per_chunk);
            let bytes = self
                .r
                .read_memory(at(base, first.saturating_mul(stride)), n * stride)?;
            if bytes.len() != n * stride {
                return Err(bad("Short object-array read"));
            }
            for (j, slot) in bytes.chunks_exact(stride).enumerate() {
                if j % 128 == 0 {
                    self.r.check()?;
                }
                f(self, slot, first + j)?;
            }
        }
        Ok(())
    }

    fn object(&mut self, addr: usize) -> Result<Object> {
        let tag = read_struct::<u8>(self.r, at(addr, 8))?;
        let mut o = Object {
            address: addr as u64,
            kind: String::new(),
            shared: false,
            shallow_bytes: 0,
            summary: String::new(),
            entries: 0,
            preview: Vec::new(),
            edges: Vec::new(),
            metatable: 0,
            mode: None,
        };
        o.shared = if let Some(age) = self.layout.shared_age {
            read_struct::<u8>(self.r, at(addr, 9))? & 7 == age
        } else {
            false
        };
        match tag {
            4 | 20 => {
                o.kind = "string".into();
                let len = if tag == 4 {
                    read_struct::<u8>(self.r, at(addr, l::STRING_SHORT_LENGTH))? as u64
                } else {
                    ptr(self.r, addr, self.layout.string_length)? as u64
                };
                o.shallow_bytes = (self.layout.string_data as u64 + 1)
                    .checked_add(len)
                    .ok_or_else(|| bad("String size overflow"))?;
                o.summary = format!("{len} bytes {:?}", self.string(addr)?);
            }
            5 => self.table(addr, &mut o)?,
            6 | 38 => {
                o.kind = if tag == 6 { "Lua closure" } else { "C closure" }.into();
                let n = read_struct::<u8>(self.r, at(addr, l::CLOSURE_COUNT))? as usize;
                o.shallow_bytes = (32 + n * if tag == 6 { 8 } else { 16 }) as u64;
                if tag == 38 {
                    self.values(&mut o, at(addr, l::CLOSURE_VALUES), n, "C upvalue")?;
                } else {
                    let proto = ptr(self.r, addr, l::CLOSURE_PROTO)?;
                    self.add(&mut o, proto as u64, "prototype")?;
                    if proto != 0 {
                        o.summary = self.source(proto)?;
                    }
                    let names = ptr(self.r, proto, l::PROTO_UPVALUES)?;
                    let name_count = self.count(proto, l::PROTO_UPVALUE_COUNT)?;
                    for i in 0..n {
                        let upvalue = ptr(self.r, addr, l::CLOSURE_VALUES + i * 8)?;
                        let name = if i < name_count {
                            let name = ptr(self.r, names, i * 16)?;
                            if name == 0 {
                                format!("#{i}")
                            } else {
                                self.string(name)?
                            }
                        } else {
                            format!("#{i}")
                        };
                        self.add(&mut o, upvalue as u64, &format!("upvalue {name:?}"))?;
                    }
                }
            }
            7 => {
                o.kind = "userdata".into();
                let n = read_struct::<u16>(self.r, at(addr, l::USER_COUNT))? as usize;
                let len = ptr(self.r, addr, l::USER_LENGTH)? as u64;
                let header = if n == 0 { 32 } else { 40 + n * 16 };
                o.shallow_bytes = len
                    .checked_add(header as u64)
                    .ok_or_else(|| bad("Userdata size overflow"))?;
                o.summary =
                    format!("{len} payload bytes; {n} user values; native pointees excluded");
                o.metatable = ptr(self.r, addr, l::USER_META)? as u64;
                let mt = o.metatable;
                self.add(&mut o, mt, "metatable")?;
                self.values(&mut o, at(addr, l::USER_VALUES), n, "uservalue")?;
            }
            8 => {
                o.kind = "thread".into();
                let base = ptr(self.r, addr, l::THREAD_STACK)?;
                let top = ptr(self.r, addr, l::THREAD_TOP)?;
                let end = ptr(self.r, addr, l::THREAD_STACK_END)?;
                if base == 0 && top == 0 && end == 0 {
                    o.shallow_bytes = 208;
                } else {
                    if top < base
                        || end < base
                        || top > at(end, 80)
                        || (top - base) % 16 != 0
                        || (end - base) % 16 != 0
                        || (end - base) / 16 > MAX_SLOTS
                    {
                        return Err(bad("Thread stack is inconsistent (possibly reallocating)"));
                    }
                    let nci = read_struct::<u16>(self.r, at(addr, l::THREAD_NCI))? as u64;
                    o.shallow_bytes = 208 + (end - base) as u64 + 80 + nci * 64;
                    self.values(&mut o, base, (top - base) / 16, "stack slot")?;
                }
                let mut up = ptr(self.r, addr, l::THREAD_OPEN_UPVALUE)?;
                let mut seen = HashSet::new();
                while up != 0 {
                    if !seen.insert(up) {
                        return Err(bad("Open-upvalue chain loop"));
                    }
                    self.add(&mut o, up as u64, "open upvalue")?;
                    up = ptr(self.r, up, l::UPVALUE_NEXT)?;
                }
            }
            9 => {
                o.kind = "upvalue".into();
                o.shallow_bytes = 40;
                let value = ptr(self.r, addr, l::UPVALUE_POINTER)?;
                self.values(&mut o, value, 1, "value")?;
            }
            10 => self.proto(addr, &mut o)?,
            _ => return Err(bad(&format!("Unsupported GC object tag {tag}"))),
        }
        Ok(o)
    }

    fn count(&self, addr: usize, off: usize) -> Result<usize> {
        let n = read_struct::<i32>(self.r, at(addr, off))?;
        if n < 0 || n as usize > MAX_SLOTS {
            return Err(bad("Invalid object-array length"));
        }
        Ok(n as usize)
    }

    fn source(&mut self, proto: usize) -> Result<String> {
        let src = ptr(self.r, proto, l::PROTO_SOURCE)?;
        let line = read_struct::<i32>(self.r, at(proto, l::PROTO_LINE))?;
        let source = if src == 0 {
            "[source unavailable]".into()
        } else {
            self.string(src)?
        };
        Ok(format!("{source}:{line}"))
    }

    fn proto(&mut self, addr: usize, o: &mut Object) -> Result<()> {
        o.kind = "prototype".into();
        o.shallow_bytes = l::PROTO_SIZE;
        o.summary = self.source(addr)?;
        self.add(o, ptr(self.r, addr, l::PROTO_SOURCE)? as u64, "source")?;
        for (length, offset, stride, label) in l::PROTO_ARRAYS {
            let count = self.count(addr, length)?;
            o.shallow_bytes += (count * stride) as u64;
            if count == 0 {
                continue;
            }
            let base = ptr(self.r, addr, offset)?;
            match label {
                "constant" => self.values(o, base, count, label)?,
                "proto" | "upvalue-name" | "local-name" => {
                    self.slots(base, count, stride, |this, bytes, i| {
                        this.add(o, bits(bytes), &format!("{label}[{i}]"))
                    })?
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn table(&mut self, addr: usize, o: &mut Object) -> Result<()> {
        o.kind = "table".into();
        let flags = read_struct::<u8>(self.r, at(addr, l::TABLE_FLAGS))?;
        let limit = read_struct::<u32>(self.r, at(addr, l::TABLE_LIMIT))? as usize;
        let array_count = if flags & 128 == 0 {
            limit
        } else {
            limit
                .checked_next_power_of_two()
                .ok_or_else(|| bad("Table array overflow"))?
        };
        let log = read_struct::<u8>(self.r, at(addr, l::TABLE_LOG_NODES))?;
        if log > 22 || array_count > MAX_SLOTS {
            return Err(bad("Table capacity exceeds slot limit"));
        }
        // Dummy hash nodes are shared runtime storage, not allocated to each table.
        let nodes = if ptr(self.r, addr, l::TABLE_LASTFREE)? == 0 {
            0
        } else {
            1usize << log
        };
        o.shallow_bytes = l::TABLE_SIZE + (array_count * 16 + nodes * l::NODE_SIZE) as u64;
        o.metatable = ptr(self.r, addr, l::TABLE_META)? as u64;
        self.add(o, o.metatable, "metatable")?;
        let array = ptr(self.r, addr, l::TABLE_ARRAY)?;
        self.slots(array, array_count, 16, |this, bytes, i| {
            if bytes[8] & 15 == 0 {
                return Ok(());
            }
            o.entries += 1;
            let key = format!("[{}]", i + 1);
            if let Some(target) = value_target(bytes)? {
                this.table_edge(o, target, &key, Role::ArrayValue)?;
            }
            if o.preview.len() < PREVIEW {
                o.preview.push(format!("{key} → {}", this.value(bytes)?));
            }
            Ok(())
        })?;
        let base = ptr(self.r, addr, l::TABLE_NODE)?;
        self.slots(base, nodes, l::NODE_SIZE, |this, bytes, _| {
            if bytes[8] & 15 == 0 {
                return Ok(());
            } // Deleted/dead keys do not retain objects.
            o.entries += 1;
            let mut key = [0u8; 16];
            key[..8].copy_from_slice(&bytes[l::NODE_KEY_VALUE..l::NODE_KEY_VALUE + 8]);
            key[8] = bytes[l::NODE_KEY_TAG];
            let key_name = this.value(&key)?;
            let key_label = format!("[{key_name}]");
            if key_name == "\"__mode\"" && bytes[8] & 15 == 4 {
                let mode = this.string(bits(bytes) as usize)?;
                // A truncated mode can hide a later 'k' or 'v'. We must not turn
                // unknown weak references into confirmed strong holders.
                o.mode = Some(if mode.ends_with('…') {
                    "kv".into()
                } else {
                    mode
                });
            }
            if let Some(target) = value_target(&key)? {
                this.table_edge(o, target, &format!("key {key_label}"), Role::TableKey)?;
            }
            if let Some(target) = value_target(bytes)? {
                this.table_edge(o, target, &key_label, Role::TableValue)?;
            }
            if o.preview.len() < PREVIEW {
                o.preview
                    .push(format!("{key_label} → {}", this.value(bytes)?));
            }
            Ok(())
        })?;
        o.summary = format!(
            "{} entries; array capacity {array_count}; hash capacity {nodes}",
            o.entries
        );
        Ok(())
    }
}
