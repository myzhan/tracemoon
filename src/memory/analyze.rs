//! Root paths and bounded candidate analysis, after the process has resumed.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write;

use super::snapshot::{Heap, Role, Snapshot, Strength};

/// Conservative weak-key handling: conditional values are not promoted by ephemeron
/// convergence yet. This underestimates strong reachability rather than inventing holders.
pub fn classify(snapshot: &mut Snapshot) {
    let mut unresolved = 0;
    for heap in &mut snapshot.heaps {
        let modes: HashMap<_, _> = heap
            .objects
            .iter()
            .map(|o| (o.address, o.mode.clone()))
            .collect();
        let strings: HashSet<_> = heap
            .objects
            .iter()
            .filter(|o| o.kind == "string")
            .map(|o| o.address)
            .collect();
        for object in &mut heap.objects {
            if object.kind != "table" {
                continue;
            }
            let mode = if object.metatable == 0 {
                Some(None)
            } else {
                modes.get(&object.metatable).cloned()
            };
            for edge in &mut object.edges {
                if edge.role == Role::Reference {
                    continue;
                }
                let Some(mode) = &mode else {
                    edge.strength = Strength::Unknown;
                    continue;
                };
                let mode = mode.as_deref().unwrap_or("");
                let weak_keys = mode.contains('k');
                let weak_values = mode.contains('v');
                edge.strength = match edge.role {
                    Role::TableKey if weak_keys && !strings.contains(&edge.target) => {
                        Strength::Weak
                    }
                    Role::TableValue | Role::ArrayValue
                        if weak_values && !strings.contains(&edge.target) =>
                    {
                        Strength::Weak
                    }
                    Role::TableValue if weak_keys => Strength::Conditional,
                    _ => Strength::Strong,
                };
            }
        }
        unresolved += heap
            .roots
            .iter()
            .chain(heap.objects.iter().flat_map(|o| &o.edges))
            .filter(|edge| !modes.contains_key(&edge.target))
            .count();
    }
    if unresolved != 0 {
        snapshot.incomplete = true;
        let diagnostic =
            format!("{unresolved} object references/roots were not resolved in the captured graph");
        if !snapshot.diagnostics.contains(&diagnostic) && snapshot.diagnostics.len() < 64 {
            snapshot.diagnostics.push(diagnostic);
        }
    }
}

pub(super) struct Graph<'a> {
    heap: &'a Heap,
    index: HashMap<u64, usize>,
    // Object index -> (parent index, edge label). None parent denotes a root.
    parents: HashMap<usize, (Option<usize>, String)>,
    business_parents: HashMap<usize, (Option<usize>, String)>,
    ids: Vec<usize>,
    incoming: Vec<Vec<(usize, usize)>>,
    pub(super) unresolved: usize,
}

impl<'a> Graph<'a> {
    pub(super) fn new(heap: &'a Heap) -> Self {
        let index: HashMap<_, _> = heap
            .objects
            .iter()
            .enumerate()
            .map(|(i, o)| (o.address, i))
            .collect();
        let mut graph = Self {
            heap,
            index,
            parents: HashMap::new(),
            business_parents: HashMap::new(),
            ids: vec![0; heap.objects.len()],
            incoming: vec![Vec::new(); heap.objects.len()],
            unresolved: 0,
        };
        let mut ordered: Vec<_> = (0..heap.objects.len()).collect();
        ordered.sort_by_key(|&i| heap.objects[i].address);
        for (id, i) in ordered.into_iter().enumerate() {
            graph.ids[i] = id + 1;
        }
        for (i, object) in heap.objects.iter().enumerate() {
            for (e, edge) in object.edges.iter().enumerate() {
                if let Some(&target) = graph.index.get(&edge.target) {
                    graph.incoming[target].push((i, e));
                } else {
                    graph.unresolved += 1;
                }
            }
        }
        let mut queue = VecDeque::new();
        for root in &heap.roots {
            if let Some(&i) = graph.index.get(&root.target) {
                if root.strength == Strength::Strong && !graph.parents.contains_key(&i) {
                    graph.parents.insert(i, (None, root.label.clone()));
                    queue.push_back(i);
                }
            } else {
                graph.unresolved += 1;
            }
        }
        while let Some(i) = queue.pop_front() {
            for edge in &heap.objects[i].edges {
                if edge.strength != Strength::Strong {
                    continue;
                }
                if let Some(&target) = graph.index.get(&edge.target) {
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        graph.parents.entry(target)
                    {
                        entry.insert((Some(i), edge.label.clone()));
                        queue.push_back(target);
                    }
                }
            }
        }
        graph.find_business_paths();
        graph
    }

    fn find_business_paths(&mut self) {
        // A short stack-slot path often wins the ordinary root BFS, although a
        // module field gives the operator a much better place to investigate.
        // This second traversal only changes presentation, never reachability.
        let mut modules = Vec::new();
        for root in &self.heap.roots {
            if root.label != "registry" || root.strength != Strength::Strong {
                continue;
            }
            let Some(&registry) = self.index.get(&root.target) else {
                continue;
            };
            for loaded in &self.heap.objects[registry].edges {
                if loaded.label != "[\"_LOADED\"]" || loaded.strength != Strength::Strong {
                    continue;
                }
                let Some(&loaded) = self.index.get(&loaded.target) else {
                    continue;
                };
                for module in &self.heap.objects[loaded].edges {
                    if module.strength != Strength::Strong
                        || module.role == Role::TableKey
                        || module.label == "[\"_G\"]"
                    {
                        continue;
                    }
                    if let Some(&i) = self.index.get(&module.target) {
                        modules.push((module.label.clone(), i, loaded));
                    }
                }
            }
        }
        modules.sort();
        let loaded_tables: HashSet<_> = modules.iter().map(|&(_, _, loaded)| loaded).collect();
        let mut queue = VecDeque::new();
        for (label, i, _) in modules {
            if let std::collections::hash_map::Entry::Vacant(entry) = self.business_parents.entry(i)
            {
                entry.insert((None, format!("registry → [\"_LOADED\"] → {label}")));
                queue.push_back(i);
            }
        }
        while let Some(i) = queue.pop_front() {
            let mut edges: Vec<_> = self.heap.objects[i].edges.iter().collect();
            edges.sort_by_key(|edge| &edge.label);
            for edge in edges {
                if edge.strength != Strength::Strong
                    || edge.role == Role::TableKey
                    || matches!(
                        edge.label.as_str(),
                        "upvalue \"_ENV\"" | "[\"_G\"]" | "metatable" | "prototype" | "source"
                    )
                {
                    continue;
                }
                let Some(&target) = self.index.get(&edge.target) else {
                    continue;
                };
                if loaded_tables.contains(&target) {
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    self.business_parents.entry(target)
                {
                    entry.insert((Some(i), edge.label.clone()));
                    queue.push_back(target);
                }
            }
        }
    }

    pub(super) fn path(&self, i: usize) -> Option<String> {
        let parents = if self.business_parents.contains_key(&i) {
            &self.business_parents
        } else {
            &self.parents
        };
        let mut current = i;
        let mut parts = Vec::new();
        for _ in 0..64 {
            let (parent, label) = parents.get(&current)?;
            parts.push(label.clone());
            if let Some(parent) = parent {
                // A closure source is a holding clue, not an allocation site.
                if self.heap.objects[*parent].kind == "Lua closure" {
                    parts.push(format!("function {}", self.heap.objects[*parent].summary));
                }
                current = *parent;
            } else {
                parts.reverse();
                return Some(parts.join(" → "));
            }
        }
        parts.push("[path prefix omitted after 64 edges]".into());
        parts.reverse();
        Some(parts.join(" → "))
    }

    pub(super) fn reachable(&self, start: usize, budget: &mut usize) -> Option<u64> {
        let mut seen = HashSet::new();
        let mut stack = vec![start];
        let mut bytes = 0u64;
        while let Some(i) = stack.pop() {
            if !seen.insert(i) {
                continue;
            }
            if *budget == 0 {
                return None;
            }
            *budget -= 1;
            bytes = bytes.checked_add(self.heap.objects[i].shallow_bytes)?;
            for edge in &self.heap.objects[i].edges {
                if *budget == 0 {
                    return None;
                }
                *budget -= 1;
                if edge.strength == Strength::Strong {
                    if let Some(&target) = self.index.get(&edge.target) {
                        stack.push(target);
                    }
                }
            }
        }
        Some(bytes)
    }

    pub(super) fn describe(
        &self,
        i: usize,
        reachable: Option<u64>,
        addresses: bool,
        out: &mut String,
    ) {
        let o = &self.heap.objects[i];
        write!(
            out,
            "\n  Object #{} / {} / {}",
            self.ids[i],
            o.kind,
            self.display(&o.summary, addresses)
        )
        .unwrap();
        if addresses {
            write!(out, " / @0x{:x}", o.address).unwrap();
        }
        writeln!(out).unwrap();
        if o.shared {
            writeln!(
                out,
                "    Ownership: shared code/string object (excluded from service private totals)"
            )
            .unwrap();
        }
        writeln!(
            out,
            "    Shallow: {} | reachable via strong edges: {}",
            size(o.shallow_bytes),
            reachable
                .map(size)
                .unwrap_or("not calculated (analysis budget)".into())
        )
        .unwrap();
        writeln!(
            out,
            "    Direct incoming references: {}",
            self.incoming[i].len()
        )
        .unwrap();
        let primary = self.path(i);
        if let Some(path) = &primary {
            writeln!(
                out,
                "    Strong holding path: {}",
                self.display_path(path, addresses)
            )
            .unwrap();
        } else {
            writeln!(
                out,
                "    Strong holding path: unconfirmed (does not imply garbage)"
            )
            .unwrap();
        }
        let mut shown = 0;
        let mut paths = HashSet::new();
        for &(holder, edge_index) in &self.incoming[i] {
            let edge = &self.heap.objects[holder].edges[edge_index];
            if edge.strength != Strength::Strong {
                continue;
            }
            if !addresses
                && self.business_parents.contains_key(&i)
                && self.heap.objects[holder].kind == "thread"
            {
                // The module path already explains ownership. Keep register-slot
                // references available in detailed output without crowding that path.
                continue;
            }
            if let Some(path) = self.path(holder) {
                let path = format!("{path} → {}", edge.label);
                if primary.as_ref() == Some(&path) || !paths.insert(path.clone()) {
                    continue;
                }
                write!(
                    out,
                    "    Other holder: {}",
                    self.display_path(&path, addresses)
                )
                .unwrap();
                if addresses {
                    write!(out, " (holder @0x{:x})", self.heap.objects[holder].address).unwrap();
                }
                writeln!(out).unwrap();
                shown += 1;
                if shown == 2 {
                    break;
                }
            }
        }
        for preview in &o.preview {
            writeln!(out, "      {}", self.display(preview, addresses)).unwrap();
        }
        if o.entries > o.preview.len() as u64 {
            writeln!(
                out,
                "      ... showing {} / {} entries",
                o.preview.len(),
                o.entries
            )
            .unwrap();
        }
    }

    pub(super) fn display_path(&self, path: &str, addresses: bool) -> String {
        if addresses {
            return self.display(path, true);
        }
        let path = if let Some(business) = path.strip_prefix("registry → [\"_LOADED\"] → ") {
            business
                .split(" → ")
                .map(|part| {
                    serde_json::from_str::<Vec<String>>(part)
                        .ok()
                        .filter(|keys| keys.len() == 1)
                        .and_then(|mut keys| keys.pop())
                        .unwrap_or_else(|| part.into())
                })
                .collect::<Vec<_>>()
                .join(" → ")
        } else {
            path.into()
        };
        self.display(&path, false)
    }

    pub(super) fn display(&self, text: &str, addresses: bool) -> String {
        if addresses {
            return text.into();
        }
        let mut output = String::new();
        let mut offset = 0;
        let mut quoted = false;
        let mut escaped = false;
        while offset < text.len() {
            let rest = &text[offset..];
            let prefix = if !quoted && rest.starts_with("@0x") {
                Some("@0x")
            } else if !quoted && rest.starts_with("native:0x") {
                Some("native:0x")
            } else {
                None
            };
            if let Some(prefix) = prefix {
                let digits = rest[prefix.len()..]
                    .bytes()
                    .take_while(u8::is_ascii_hexdigit)
                    .count();
                if digits != 0 {
                    let address =
                        u64::from_str_radix(&rest[prefix.len()..prefix.len() + digits], 16).ok();
                    if prefix == "native:0x" {
                        output.push_str("native value");
                    } else if let Some(i) =
                        address.and_then(|address| self.index.get(&address).copied())
                    {
                        let object = &self.heap.objects[i];
                        write!(output, "{} #{}", object.kind, self.ids[i]).unwrap();
                        if object.kind == "table" {
                            write!(
                                output,
                                " ({} {})",
                                object.entries,
                                if object.entries == 1 {
                                    "entry"
                                } else {
                                    "entries"
                                }
                            )
                            .unwrap();
                        }
                    } else {
                        output.push_str("unresolved object");
                    }
                    offset += prefix.len() + digits;
                    continue;
                }
            }
            let ch = rest.chars().next().expect("nonempty text suffix");
            output.push(ch);
            if escaped {
                escaped = false;
            } else if quoted && ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = !quoted;
            }
            offset += ch.len_utf8();
        }
        output
    }

    pub(super) fn object_id(&self, i: usize) -> usize {
        self.ids[i]
    }

    pub(super) fn object_index(&self, address: u64) -> Option<usize> {
        self.index.get(&address).copied()
    }

    pub(super) fn holding_paths(&self, i: usize) -> Vec<String> {
        let mut paths = self.path(i).into_iter().collect::<Vec<_>>();
        for &(holder, e) in &self.incoming[i] {
            let edge = &self.heap.objects[holder].edges[e];
            if edge.strength != Strength::Strong || self.heap.objects[holder].kind == "thread" {
                continue;
            }
            if let Some(path) = self.path(holder) {
                let path = format!("{path} → {}", edge.label);
                if !paths.contains(&path) {
                    paths.push(path);
                }
                if paths.len() == 3 {
                    break;
                }
            }
        }
        paths
    }

    pub(super) fn confirmed(&self, i: usize) -> bool {
        self.parents.contains_key(&i)
    }

    pub(super) fn is_module_root(&self, i: usize) -> bool {
        self.business_parents
            .get(&i)
            .is_some_and(|(parent, _)| parent.is_none())
    }

    pub(super) fn business_module(&self, mut i: usize) -> Option<String> {
        for _ in 0..64 {
            let (parent, label) = self.business_parents.get(&i)?;
            if let Some(parent) = parent {
                i = *parent;
            } else {
                let label = label.strip_prefix("registry → [\"_LOADED\"] → ")?;
                let mut modules: Vec<String> = serde_json::from_str(label).ok()?;
                return modules.pop();
            }
        }
        None
    }

    pub(super) fn candidates(
        &self,
        budget: &mut usize,
        extra: &[usize],
    ) -> Vec<(usize, Option<u64>)> {
        let objects = &self.heap.objects;
        let mut shallow: Vec<_> = (0..objects.len()).collect();
        shallow.sort_by_key(|&i| {
            (
                std::cmp::Reverse(objects[i].shallow_bytes),
                objects[i].address,
            )
        });
        let mut entries = shallow.clone();
        entries.sort_by_key(|&i| (std::cmp::Reverse(objects[i].entries), objects[i].address));
        let mut incoming = shallow.clone();
        incoming.sort_by_key(|&i| {
            (
                std::cmp::Reverse(self.incoming[i].len()),
                objects[i].address,
            )
        });
        let mut candidates: Vec<_> = shallow
            .into_iter()
            .take(64)
            .chain(entries.into_iter().take(64))
            .chain(incoming.into_iter().take(64))
            .chain(extra.iter().copied())
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        let mut results: Vec<_> = candidates
            .into_iter()
            .map(|i| (i, self.reachable(i, budget)))
            .collect();
        results.sort_by_key(|&(i, reachable)| {
            (
                std::cmp::Reverse(reachable.unwrap_or(objects[i].shallow_bytes)),
                objects[i].address,
            )
        });
        results
    }
}

fn size(bytes: u64) -> String {
    format!("{:.2} MiB ({bytes} B)", bytes as f64 / (1024.0 * 1024.0))
}

pub fn report(snapshot: &Snapshot, top: usize, inspect: Option<u64>) -> String {
    report_with_addresses(snapshot, top, inspect, false)
}

pub fn report_with_addresses(
    snapshot: &Snapshot,
    top: usize,
    inspect: Option<u64>,
    addresses: bool,
) -> String {
    let mut out = String::new();
    if snapshot.is_core() {
        writeln!(
            out,
            "Lua heap / Linux ELF core / Lua {} / dump time unavailable",
            snapshot.lua_version
        )
        .unwrap();
    } else {
        writeln!(
            out,
            "Lua heap snapshot / PID {} / Lua {} / Unix time {}",
            snapshot.pid, snapshot.lua_version, snapshot.captured_unix_secs
        )
        .unwrap();
    }
    writeln!(out, "Heap ABI: {}", snapshot.lua_abi).unwrap();
    if let Some(method) = &snapshot.capture_method {
        writeln!(out, "Capture method: {method}").unwrap();
    }
    writeln!(
        out,
        "Pause: {} | reader bytes requested: {} | capture: {}",
        if snapshot.is_core() {
            "unavailable (offline core)".into()
        } else {
            format!("{} ms", snapshot.pause_ms)
        },
        snapshot.read_bytes,
        if snapshot.incomplete {
            "INCOMPLETE"
        } else {
            "enumeration finished"
        }
    )
    .unwrap();
    writeln!(
        out,
        "Strong reachability is a lower bound; ephemerons/finalizers are not fully modeled."
    )
    .unwrap();
    writeln!(out, "Unconfirmed objects are NOT classified as garbage. Native allocations, allocator overhead, global state and string hash buckets are excluded.").unwrap();
    writeln!(out, "Reachable totals include sharing: do not add entries together. No allocation sites or leak verdicts are inferred.\n").unwrap();
    let mut ranked: Vec<_> = snapshot
        .heaps
        .iter()
        .map(|h| {
            (
                h.objects
                    .iter()
                    .filter(|o| !o.shared)
                    .fold(0u64, |sum, o| sum.saturating_add(o.shallow_bytes)),
                h,
            )
        })
        .collect();
    ranked.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    writeln!(
        out,
        "Service allocation ranking (private parsed object bytes, not RSS):"
    )
    .unwrap();
    for (bytes, heap) in &ranked {
        writeln!(
            out,
            "  {:<32} {} / {} objects",
            heap.services.join(", "),
            size(*bytes),
            heap.objects.iter().filter(|o| !o.shared).count()
        )
        .unwrap();
    }
    writeln!(out).unwrap();
    let shared: HashMap<_, _> = snapshot
        .heaps
        .iter()
        .flat_map(|heap| &heap.objects)
        .filter(|o| o.shared)
        .map(|o| (o.address, o.shallow_bytes))
        .collect();
    if !shared.is_empty() {
        writeln!(out, "Shared code/string objects: {} / {} unique objects across selected services (counted once)",
            size(shared.values().fold(0u64, |sum, &bytes| sum.saturating_add(bytes))), shared.len()).unwrap();
    }
    let mut inspect_found = false;
    for (total, heap) in ranked {
        let graph = Graph::new(heap);
        let strong: u64 = graph.parents.keys().fold(0u64, |sum, &i| {
            if heap.objects[i].shared {
                sum
            } else {
                sum.saturating_add(heap.objects[i].shallow_bytes)
            }
        });
        write!(out, "Service(s): {}", heap.services.join(", ")).unwrap();
        if addresses {
            write!(
                out,
                " / global 0x{:x} / GC state {}",
                heap.global, heap.gc_state
            )
            .unwrap();
        }
        writeln!(out).unwrap();
        writeln!(
            out,
            "  Parsed private allocation: {} / {} objects",
            size(total),
            heap.objects.iter().filter(|o| !o.shared).count()
        )
        .unwrap();
        let referenced_shared = heap
            .objects
            .iter()
            .filter(|o| o.shared)
            .fold(0u64, |sum, o| sum.saturating_add(o.shallow_bytes));
        if referenced_shared != 0 {
            writeln!(
                out,
                "  Referenced shared code/strings: {} (overlaps other services)",
                size(referenced_shared)
            )
            .unwrap();
        }
        writeln!(
            out,
            "  Confirmed strong: {} | unconfirmed: {}",
            size(strong),
            size(total.saturating_sub(strong))
        )
        .unwrap();
        writeln!(out, "  Unresolved references/roots: {}", graph.unresolved).unwrap();
        let non_strong = heap
            .objects
            .iter()
            .flat_map(|o| &o.edges)
            .filter(|e| e.strength != Strength::Strong)
            .count();
        writeln!(out, "  Weak/conditional/unknown edges: {non_strong}").unwrap();
        let mut types = BTreeMap::<&str, (u64, usize)>::new();
        for o in &heap.objects {
            if o.shared {
                continue;
            }
            let entry = types.entry(&o.kind).or_default();
            entry.0 = entry.0.saturating_add(o.shallow_bytes);
            entry.1 += 1;
        }
        for (kind, (bytes, count)) in types {
            writeln!(out, "    {kind:<14} {} / {count} objects", size(bytes)).unwrap();
        }
        let mut budget = 20_000_000;
        if let Some(address) = inspect {
            if let Some(&i) = graph.index.get(&address) {
                graph.describe(i, graph.reachable(i, &mut budget), addresses, &mut out);
                inspect_found = true;
            }
        } else {
            let results = graph.candidates(&mut budget, &[]);
            writeln!(
                out,
                "  Large-object candidates (bounded shortlist, not retained-size ranking):"
            )
            .unwrap();
            for (i, reachable) in results.into_iter().take(top) {
                graph.describe(i, reachable, addresses, &mut out);
            }
        }
        writeln!(out).unwrap();
    }
    if inspect.is_some() && !inspect_found {
        writeln!(out, "Object not present in this snapshot.").unwrap();
    }
    for message in &snapshot.diagnostics {
        writeln!(out, "INCOMPLETE: {message}").unwrap();
    }
    out
}
