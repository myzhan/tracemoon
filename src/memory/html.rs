//! Standalone HTML presentation over the same graph analysis as the text report.
//! Snapshot strings are always escaped as HTML text/attributes, never interpolated in JS.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;

use super::analyze::Graph;
use super::snapshot::{Heap, Object, Role, Snapshot};

struct Service<'a> {
    heap: &'a Heap,
    graph: Graph<'a>,
    ordinal: usize,
    private: u64,
    confirmed: u64,
    modules: Vec<(usize, Option<u64>)>,
    others: Vec<(usize, Option<u64>)>,
}

impl<'a> Service<'a> {
    fn new(heap: &'a Heap, ordinal: usize, top: usize) -> Self {
        let graph = Graph::new(heap);
        let mut extra: Vec<_> = heap
            .objects
            .iter()
            .enumerate()
            .filter_map(|(i, o)| {
                (o.kind == "table"
                    && !o.shared
                    && graph
                        .business_module(i)
                        .is_some_and(|m| !runtime_module(&m)))
                .then_some(i)
            })
            .collect();
        extra.sort_by_key(|&i| {
            (
                std::cmp::Reverse(heap.objects[i].entries),
                std::cmp::Reverse(heap.objects[i].shallow_bytes),
                heap.objects[i].address,
            )
        });
        extra.truncate(64);
        let mut budget = 20_000_000;
        let candidates = graph.candidates(&mut budget, &extra);
        let is_module = |i: usize, reachable: Option<u64>| {
            let o = &heap.objects[i];
            o.kind == "table"
                && !o.shared
                && (o.entries > 1 || reachable.unwrap_or(0) >= 16 * 1024)
                && graph
                    .business_module(i)
                    .is_some_and(|m| !runtime_module(&m))
        };
        let has_field = candidates
            .iter()
            .any(|&(i, r)| is_module(i, r) && !graph.is_module_root(i));
        let (mut modules, mut others): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|&(i, r)| is_module(i, r) && (!has_field || !graph.is_module_root(i)));
        modules.truncate(top);
        others.truncate(top);
        let private = heap
            .objects
            .iter()
            .filter(|o| !o.shared)
            .fold(0u64, |n, o| n.saturating_add(o.shallow_bytes));
        let confirmed = heap
            .objects
            .iter()
            .enumerate()
            .filter(|&(i, o)| !o.shared && graph.confirmed(i))
            .fold(0u64, |n, (_, o)| n.saturating_add(o.shallow_bytes));
        Self {
            heap,
            graph,
            ordinal,
            private,
            confirmed,
            modules,
            others,
        }
    }
}

fn runtime_module(name: &str) -> bool {
    name == "skynet"
        || name.starts_with("skynet.")
        || matches!(
            name,
            "_G" | "package"
                | "coroutine"
                | "table"
                | "io"
                | "os"
                | "string"
                | "math"
                | "utf8"
                | "debug"
        )
}

fn escape(text: &str) -> String {
    let mut result = String::new();
    for ch in text.chars() {
        result.push_str(match ch {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' => "&quot;",
            '\'' => "&#39;",
            _ => {
                result.push(ch);
                continue;
            }
        });
    }
    result
}

fn bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GiB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn key(label: &str) -> String {
    serde_json::from_str::<Vec<serde_json::Value>>(label)
        .ok()
        .and_then(|mut a| a.pop())
        .map(|v| {
            v.as_str()
                .map(String::from)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_else(|| label.into())
}

fn short_path(path: &str) -> String {
    path.strip_prefix("registry → [\"_LOADED\"] → ")
        .unwrap_or(path)
        .split(" → ")
        .map(key)
        .collect::<Vec<_>>()
        .join(" › ")
}

/// Collapse the closure/upvalue storage hops, keeping the source location available
/// as a tooltip. These are two references to one object, not two allocations.
fn holder_description(path: &str) -> (&'static str, String) {
    let mut parts: Vec<_> = path.split(" → ").collect();
    if parts.last() == Some(&"value[0]") && parts.len() >= 3 {
        let n = parts.len();
        if let Some(name) = parts[n - 2].strip_prefix("upvalue ") {
            if parts[n - 3].starts_with("function @") {
                let name = serde_json::from_str::<String>(name).unwrap_or_else(|_| name.into());
                parts.truncate(n - 3);
                return (
                    "函数通过捕获变量持有",
                    format!("{} → 捕获变量 {name}", parts.join(" → ")),
                );
            }
        }
    }
    let label = if !path.contains(" → function @") && parts.len() > 1 {
        "字段直接持有"
    } else {
        "强引用路径"
    };
    (label, path.into())
}

fn kind(kind: &str) -> &str {
    match kind {
        "table" => "表",
        "string" => "字符串",
        "thread" => "协程",
        "Lua closure" => "Lua 函数",
        "C closure" => "C 闭包",
        "upvalue" => "捕获变量",
        "userdata" => "用户数据",
        "prototype" => "代码 Proto",
        _ => kind,
    }
}

fn composition(heap: &Heap) -> BTreeMap<&str, u64> {
    let mut types = BTreeMap::<&str, u64>::new();
    for o in &heap.objects {
        if !o.shared {
            let entry = types.entry(&o.kind).or_default();
            *entry = entry.saturating_add(o.shallow_bytes);
        }
    }
    types
}

/// Summary only describes stored examples; it does not infer every entry's schema.
fn sample_summary(service: &Service<'_>, o: &Object) -> String {
    let labels: Vec<_> = o
        .preview
        .iter()
        .filter_map(|line| line.split_once(" → ").map(|(k, _)| key(k)))
        .collect();
    let prefix = labels
        .first()
        .map(|label| label.trim_end_matches(|c: char| c.is_ascii_digit()))
        .unwrap_or("");
    let pattern = if !prefix.is_empty()
        && labels.len() > 1
        && labels.iter().all(|l| {
            l.len() > prefix.len()
                && l.starts_with(prefix)
                && l[prefix.len()..].chars().all(|c| c.is_ascii_digit())
        }) {
        format!("{prefix}*")
    } else {
        labels
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(" / ")
    };
    let child = o
        .edges
        .iter()
        .find(|e| matches!(e.role, Role::TableValue | Role::ArrayValue))
        .and_then(|edge| service.graph.object_index(edge.target))
        .map(|i| &service.heap.objects[i]);
    match child {
        Some(child) => {
            let fields = child
                .preview
                .iter()
                .filter_map(|p| p.split_once(" → ").map(|(k, _)| key(k)))
                .take(3)
                .collect::<Vec<_>>()
                .join("、");
            format!(
                "样例：{pattern} → {}{}",
                child.kind,
                if fields.is_empty() {
                    String::new()
                } else {
                    format!(" · 字段 {fields}")
                }
            )
        }
        None if !pattern.is_empty() => format!("样例字段：{pattern}"),
        _ => "没有保存内容样例".into(),
    }
}

fn service_name(heap: &Heap) -> String {
    heap.services
        .iter()
        .map(|label| {
            if let Some((name, handle)) = label.rsplit_once(" (") {
                if let Some(handle) = handle.strip_suffix(')') {
                    if handle.len() == 8 && handle.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return name.to_owned();
                    }
                }
            }
            label.clone()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn object_details(
    out: &mut String,
    service: &Service<'_>,
    i: usize,
    reachable: Option<u64>,
    addresses: bool,
) {
    let graph = &service.graph;
    let o = &service.heap.objects[i];
    out.push_str("<div class=\"detail-grid\"><section><h3>谁持有这个对象</h3>");
    let paths = graph.holding_paths(i);
    if paths.is_empty() {
        out.push_str("<p class=\"uncertain\">未确认强持有路径；不代表垃圾。</p>");
    } else if paths.len() > 1 {
        out.push_str("<p class=\"muted\">以下引用共同持有同一个对象，并非多份内存。任一强引用仍存在，对象就无法被 GC 回收。</p>");
    }
    for path in paths {
        let full = graph.display_path(&path, addresses);
        let (label, readable) = holder_description(&graph.display_path(&path, false));
        write!(
            out,
            "<div class=\"holding-reference\"><span class=\"muted\">{}</span><code class=\"holder\" title=\"{}\">{}</code></div>",
            label,
            escape(&full),
            escape(if addresses { &full } else { &readable })
        )
        .unwrap();
    }
    out.push_str("</section><section><h3>内容样例</h3>");
    write!(
        out,
        "<p class=\"muted\">{}</p>",
        escape(&sample_summary(service, o))
    )
    .unwrap();
    if !o.preview.is_empty() {
        out.push_str(
            "<table class=\"samples\"><thead><tr><th>键 / 字段</th><th>值</th></tr></thead><tbody>",
        );
        for preview in &o.preview {
            let (k, value) = preview.split_once(" → ").unwrap_or(("", preview));
            write!(
                out,
                "<tr><td>{}</td><td>{}</td></tr>",
                escape(&key(k)),
                escape(&graph.display(value, addresses))
            )
            .unwrap();
        }
        write!(
            out,
            "</tbody></table><p class=\"muted\">显示 {} / {} 项</p>",
            o.preview.len(),
            o.entries
        )
        .unwrap();
    }
    out.push_str("</section></div>");
    let mut raw = String::new();
    graph.describe(i, reachable, addresses, &mut raw);
    write!(
        out,
        "<details class=\"raw\"><summary>源码与底层详情</summary><pre>{}</pre></details>",
        escape(raw.trim())
    )
    .unwrap();
}

pub fn report(snapshot: &Snapshot, top: usize, addresses: bool) -> String {
    let mut services: Vec<_> = snapshot
        .heaps
        .iter()
        .enumerate()
        .map(|(i, heap)| Service::new(heap, i, top))
        .collect();
    services.sort_by_key(|s| std::cmp::Reverse(s.private));
    let total = services
        .iter()
        .fold(0u64, |n, s| n.saturating_add(s.private));
    let confirmed = services
        .iter()
        .fold(0u64, |n, s| n.saturating_add(s.confirmed));
    let shared: HashMap<_, _> = snapshot
        .heaps
        .iter()
        .flat_map(|h| &h.objects)
        .filter(|o| o.shared)
        .map(|o| (o.address, o.shallow_bytes))
        .collect();
    let shared_bytes = shared.values().fold(0u64, |n, &b| n.saturating_add(b));
    let objects = snapshot
        .heaps
        .iter()
        .flat_map(|h| &h.objects)
        .filter(|o| !o.shared)
        .count()
        + shared.len();
    let cooperative = snapshot
        .capture_method
        .as_deref()
        .is_some_and(|m| m.contains("cooperative"));
    let mut rows: Vec<_> = services
        .iter()
        .flat_map(|s| {
            s.modules
                .iter()
                .map(move |&(i, r)| (s, i, r, true))
                .chain(s.others.iter().map(move |&(i, r)| (s, i, r, false)))
        })
        .collect();
    rows.sort_by_key(|&(s, i, r, _)| {
        (
            r.is_none(),
            std::cmp::Reverse(r.unwrap_or(0)),
            s.ordinal,
            s.heap.objects[i].address,
        )
    });
    let module_count = rows.iter().filter(|r| r.3).count();
    let default_scope = if module_count == 0 { "all" } else { "module" };
    let mut out = String::from("<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>tracemoon mem · 内存排行</title><style>");
    out.push_str(include_str!("html/style.css"));
    out.push_str("</style></head><body><main><header><h1>tracemoon <span>mem</span></h1>");
    write!(
        out,
        "<span class=\"status {}\">{}</span>",
        if snapshot.incomplete { "warning" } else { "ok" },
        if snapshot.incomplete {
            "快照不完整"
        } else {
            "读取完整"
        },
    )
    .unwrap();
    if snapshot.is_core() {
        write!(out, "<span class=\"snapshot-time\">ELF core · Lua {} · 离线分析 · 原始采集时间未知</span></header>", escape(&snapshot.lua_version)).unwrap();
    } else {
        write!(out, "<span class=\"snapshot-time\">PID {} · Lua {} · <time data-unix=\"{}\">Unix {}</time></span></header>", snapshot.pid, escape(&snapshot.lua_version), snapshot.captured_unix_secs, snapshot.captured_unix_secs).unwrap();
    }
    write!(out, "<div class=\"summary-line\"><span>服务 <b>{}</b></span><span>私有 <b>{}</b></span><span>共享 <b>{}</b></span><span>对象 <b>{}</b></span></div>",
        services.len(), bytes(total), bytes(shared_bytes), objects).unwrap();
    if cooperative {
        out.push_str("<p class=\"provenance\">测试数据 · 真实 skynet 服务协作导出；各服务分别采集，非全进程同时暂停快照</p>");
    }
    if snapshot.incomplete {
        out.push_str("<div class=\"alert\" role=\"status\">采集不完整，统计可能低估实际内存。<a href=\"#diagnostics\">查看诊断</a></div>");
    }
    write!(out, "<div class=\"toolbar\"><input id=\"object-search\" type=\"search\" aria-label=\"搜索对象\" placeholder=\"服务 / 持有路径 / 字段…\"><select id=\"service-filter\" aria-label=\"筛选服务\"><option value=\"\">所有服务</option>").unwrap();
    for service in &services {
        write!(
            out,
            "<option value=\"s{}\">{}</option>",
            service.ordinal,
            escape(&service_name(service.heap))
        )
        .unwrap();
    }
    write!(out, "</select><select id=\"scope-filter\" aria-label=\"对象范围\" data-default=\"{}\"><option value=\"module\" {}>模块容器 ({})</option><option value=\"all\" {}>全部候选 ({})</option></select><button id=\"reset-filters\" type=\"button\">重置</button><span id=\"match-count\" aria-live=\"polite\"></span></div>",
        default_scope, if default_scope == "module" { "selected" } else { "" }, module_count,
        if default_scope == "all" { "selected" } else { "" }, rows.len()).unwrap();
    out.push_str("<div class=\"table-wrap\"><table class=\"memory-table\" aria-label=\"内存对象排行\"><thead><tr><th class=\"rank\">#</th>");
    for (key, label, title) in [
        ("service", "服务", "所属服务"),
        ("kind", "类型", "Lua 对象类型"),
        ("entries", "条目", "表条目数；非表显示 —"),
        ("self", "自身", "对象直接拥有的存储"),
        ("reachable", "可达 ↓", "强引用可达大小，可能重叠，不可相加"),
        ("path", "持有位置", "点击展开引用路径和内容样例"),
    ] {
        write!(
            out,
            "<th {}><button type=\"button\" data-sort-key=\"{}\" title=\"{}\">{}</button></th>",
            if key == "reachable" {
                "aria-sort=\"descending\""
            } else {
                ""
            },
            key,
            title,
            label
        )
        .unwrap();
    }
    out.push_str("</tr></thead><tbody id=\"object-rows\">");
    let mut rank = 0;
    for (order, (service, i, reachable, module)) in rows.iter().enumerate() {
        let o = &service.heap.objects[*i];
        let path = service.graph.path(*i);
        let label = path
            .as_ref()
            .map(|p| short_path(p))
            .unwrap_or_else(|| format!("未确认路径 · {} #{}", o.kind, service.graph.object_id(*i)));
        let sample = sample_summary(service, o);
        let name = service_name(service.heap);
        let hidden = default_scope == "module" && !module;
        if !hidden {
            rank += 1;
        }
        write!(out, "<tr class=\"object-row\" data-service=\"s{}\" data-name=\"{}\" data-kind=\"{}\" data-entries=\"{}\" data-self=\"{}\" data-reachable=\"{}\" data-path=\"{}\" data-module=\"{}\" data-order=\"{}\" data-detail=\"detail-{}\" data-search=\"{}\" {}><td class=\"rank\">{}</td><td class=\"service\">{}</td><td class=\"kind\">{}</td><td class=\"number\">{}</td><td class=\"number\">{}</td><td class=\"number reachable\">{}</td><td class=\"path\"><button class=\"row-toggle\" type=\"button\" aria-expanded=\"false\" aria-controls=\"detail-{}\" title=\"{}\">{}</button>{}</td></tr><tr class=\"row-details\" id=\"detail-{}\" hidden><td colspan=\"7\">",
            service.ordinal, escape(&name), escape(&o.kind), if o.kind == "table" { o.entries.to_string() } else { "-1".into() },
            o.shallow_bytes, reachable.map(|r| r.to_string()).unwrap_or_else(|| "-1".into()), escape(&label), module,
            order, order, escape(&format!("{name} {label} {} {sample}", o.summary)), if hidden { "hidden" } else { "" }, rank,
            escape(&name), escape(&o.kind), if o.kind == "table" { o.entries.to_string() } else { "—".into() },
            bytes(o.shallow_bytes), reachable.map(bytes).unwrap_or_else(|| "—".into()), order, escape(&label), escape(&label),
            if o.shared { "<span class=\"shared\">共享</span>" } else { "" }, order).unwrap();
        object_details(&mut out, service, *i, *reachable, addresses);
        out.push_str("</td></tr>");
    }
    out.push_str("</tbody></table><p id=\"no-matches\" hidden>没有匹配对象</p></div><p class=\"hint\">点击列标题排序，点击一行展开。可达大小可能重叠，不能相加；不是精确独占大小。</p>");
    write!(out, "<details class=\"report-notes\" {}><summary>快照信息与统计口径</summary><div class=\"notes\"><p>服务私有内存 {}；已确认强持有 {}；未确认 {}。未确认可达不等于垃圾，单次快照不能判断泄漏。</p><p>共享对象跨服务按地址去重；私有总量不含原生外部分配、分配器开销等，不等于 RSS。</p><p>候选排行有数量和计算预算限制，每个服务最多展示 {} 个模块容器及 {} 个其他对象。弱键条件传播与终结器语义尚未完整建模。</p><p>ABI：{} · 工具版本：{} · 暂停 {} · 采集来源：{}</p>",
        if snapshot.incomplete { "open" } else { "" }, bytes(total), bytes(confirmed), bytes(total.saturating_sub(confirmed)), top, top,
        escape(&snapshot.lua_abi), escape(&snapshot.tool_version), if snapshot.is_core() { "未记录（离线 core）".into() } else { format!("{} ms", snapshot.pause_ms) },
        escape(snapshot.capture_method.as_deref().unwrap_or("未记录"))).unwrap();
    for service in &services {
        write!(
            out,
            "<p><b>{}</b> · 私有 {} · 未解析引用 {} · GC 状态 {}</p><ul>",
            escape(&service_name(service.heap)),
            bytes(service.private),
            service.graph.unresolved,
            service.heap.gc_state
        )
        .unwrap();
        for (name, value) in composition(service.heap) {
            write!(out, "<li>{} {}</li>", escape(kind(name)), bytes(value)).unwrap();
        }
        out.push_str("</ul>");
    }
    if !snapshot.diagnostics.is_empty() {
        out.push_str("<div id=\"diagnostics\"><h3>采集诊断</h3><ul>");
        for message in &snapshot.diagnostics {
            write!(out, "<li>{}</li>", escape(message)).unwrap();
        }
        out.push_str("</ul></div>");
    }
    out.push_str("</div></details></main><script>");
    out.push_str(include_str!("html/report.js"));
    out.push_str("</script></body></html>");
    out
}

#[cfg(test)]
mod tests {
    use super::holder_description;

    #[test]
    fn holder_explains_closure_capture_without_exposing_storage_slots() {
        assert_eq!(
            holder_description("tracemoon.fixture → flush → function @/tmp/worker.lua:10 → upvalue \"sessions\" → value[0]"),
            ("函数通过捕获变量持有", "tracemoon.fixture → flush → 捕获变量 sessions".into())
        );
        assert_eq!(
            holder_description("tracemoon.fixture → sessions"),
            ("字段直接持有", "tracemoon.fixture → sessions".into())
        );
        assert_eq!(
            holder_description("registry"),
            ("强引用路径", "registry".into())
        );
    }
}
