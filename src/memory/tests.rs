use std::collections::BTreeMap;
use std::sync::{atomic::AtomicU64, Arc};
use std::time::{Duration, Instant};

use super::capture::{BudgetReader, DiscoveredHeap};
use super::snapshot::{Edge, Heap, Object, Role, Snapshot, Strength};
use super::*;
use crate::process::reader::testing::WindowReader;
use crate::process::ProcessMemoryReader;

fn snapshot() -> Snapshot {
    Snapshot {
        schema_version: 1,
        tool_version: "test".into(),
        lua_version: "5.4".into(),
        lua_abi: "lua54".into(),
        capture_method: None,
        pid: 123,
        captured_unix_secs: 0,
        pause_ms: 10,
        read_bytes: 0,
        limits: snapshot::CaptureLimits::default(),
        pointer_width: 64,
        target_endian: "little".into(),
        incomplete: false,
        diagnostics: vec![],
        heaps: vec![],
    }
}

fn object(address: u64, kind: &str, bytes: u64, edges: Vec<Edge>) -> Object {
    Object {
        address,
        kind: kind.into(),
        shared: false,
        shallow_bytes: bytes,
        summary: "fixture".into(),
        entries: 0,
        preview: vec![],
        edges,
        metatable: 0,
        mode: None,
    }
}

fn heap(objects: Vec<Object>, root: u64) -> Heap {
    Heap {
        global: 0x1000,
        services: vec!["fixture".into()],
        gc_state: 0,
        roots: vec![capture::edge(root, "registry", Strength::Strong)],
        objects,
    }
}

fn reader(inner: impl ProcessMemoryReader + 'static) -> BudgetReader {
    BudgetReader {
        inner: Box::new(inner),
        deadline: Instant::now() + Duration::from_secs(10),
        max_bytes: 32 * 1024 * 1024,
        bytes: AtomicU64::new(0),
        running: Arc::new(AtomicBool::new(true)),
    }
}

fn base_image() -> WindowReader {
    let mut r = WindowReader::new(0x1000, 0x10000);
    r.write_u64(0x1000 + layout::REGISTRY, 0x2000);
    r.write_u8(0x1000 + layout::REGISTRY + 8, 0x45);
    r.write_u64(0x1000 + layout::MAIN_THREAD, 0x3000);
    r.write_u64(0x1000 + 112, 0x2000);
    r.write_u64(0x1000 + layout::MEMERR, 0x4000);
    r.write_u8(0x2008, 5);
    r.write_u64(0x2000, 0x3000);
    r.write_u8(0x3008, 8);
    r.write_u64(0x3000 + layout::GLOBAL, 0x1000);
    r.write_u64(0x3000, 0x4000);
    r.write_u8(0x4008, 4);
    r.write_u8(0x400b, 3);
    r.write(0x4018, b"err");
    r
}

#[test]
fn core_input_discovers_a_vm_and_renders_reports_without_a_pid_or_conversion() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("arbitrary-extension.bin");
    let bytes = base_image().read_memory(0x1000, 0x10000).unwrap();
    core::fixture(&input, &[(0x1000, bytes, 0x10000, 6)], &[]);
    let options = core::Options {
        lua54: true,
        ..Default::default()
    };
    let s = core::read(&input, &options).unwrap();
    assert!(!s.incomplete, "{:?}", s.diagnostics);
    assert_eq!(s.heaps.len(), 1);
    assert_eq!(s.heaps[0].objects.len(), 3);
    let args: Vec<_> = [
        "tracemoon",
        "mem",
        "inspect",
        input.to_str().unwrap(),
        "--lua-version",
        "5.4",
        "--state",
        "0x3000",
        "--object",
        "0x2000",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let config = crate::config::Config::from_args(&args).unwrap();
    run(config.memory.unwrap()).unwrap();
    for (format, filename) in [
        (ReportFormat::Text, "report.txt"),
        (ReportFormat::Html, "report.html"),
    ] {
        let output = dir.path().join(filename);
        run(MemoryCommand::Report {
            input: input.clone(),
            core: options.clone(),
            output: Some(output.clone()),
            top: 10,
            object: None,
            show_addresses: false,
            format,
        })
        .unwrap();
        let report = std::fs::read_to_string(output).unwrap();
        assert!(report.contains("ELF core"));
        assert!(!report.contains("PID 0") && !report.contains("Unix 0"));
    }
    // A heap pointer into an omitted page must produce a visibly incomplete report.
    let mut bytes = base_image().read_memory(0x1000, 0x10000).unwrap();
    bytes[0x3000..0x3008].copy_from_slice(&0x999000u64.to_le_bytes());
    core::fixture(&input, &[(0x1000, bytes, 0x10000, 6)], &[]);
    let s = core::read(&input, &options).unwrap();
    assert!(s.incomplete && !s.diagnostics.is_empty());
    assert!(html::report(&s, 10, false).contains("快照不完整"));
}

fn collect(r: BudgetReader, max_objects: usize, max_edges: usize) -> Snapshot {
    let mut s = snapshot();
    let heaps = BTreeMap::from([(
        0x1000,
        DiscoveredHeap {
            services: vec!["fixture".into()],
            active_threads: vec![],
        },
    )]);
    capture::collect(
        &r,
        heaps,
        &mut s,
        max_objects,
        max_edges,
        layout::HeapLayout::lua54(),
    );
    analyze::classify(&mut s);
    s
}

#[test]
fn table_capacity_counts_allocated_array_and_ignores_dead_hash_keys() {
    let mut r = base_image();
    r.write_u8(0x2000 + layout::TABLE_FLAGS, 128);
    r.write_i32(0x2000 + layout::TABLE_LIMIT, 3); // real capacity = 4
    r.write_u64(0x2000 + layout::TABLE_ARRAY, 0x5000);
    r.write_u64(0x5000, 0x4000);
    r.write_u8(0x5008, 0x44);
    r.write_u64(0x2000 + layout::TABLE_NODE, 0x6000);
    r.write_u64(0x2000 + layout::TABLE_LASTFREE, 0x6018);
    r.write_u8(0x6009, 0x45); // stale key in a nil-valued node must not be followed
    r.write_u64(0x6010, u64::MAX);
    let s = collect(reader(r), 100, 100);
    assert!(!s.incomplete, "{:?}", s.diagnostics);
    let table = &s.heaps[0].objects[0];
    assert_eq!(table.shallow_bytes, 56 + 4 * 16 + 24);
    assert_eq!(table.entries, 1);
    assert_eq!(table.edges.len(), 1);
    assert_eq!(table.edges[0].label, "[1]");
}

#[test]
fn long_strings_are_sized_fully_without_reading_the_whole_payload() {
    let mut r = base_image();
    r.write_u8(0x4008, 20);
    r.write_u64(0x4010, 100_000);
    let s = collect(reader(r), 100, 100);
    assert!(!s.incomplete);
    assert_eq!(s.heaps[0].objects[2].shallow_bytes, 100_025);
    assert!(s.heaps[0].objects[2].summary.contains("100000 bytes"));
}

#[test]
fn corrupt_chains_and_budgets_preserve_a_partial_snapshot() {
    let mut r = base_image();
    r.write_u64(0x4000, 0x2000);
    assert!(collect(reader(r), 100, 100).incomplete);
    let s = collect(reader(base_image()), 1, 100);
    assert!(s.incomplete);
    assert_eq!(s.heaps[0].objects.len(), 1);
    let mut r = reader(base_image());
    r.deadline = Instant::now();
    assert!(collect(r, 100, 100).incomplete);
    let mut r = reader(base_image());
    r.max_bytes = 1;
    assert!(r.read_memory(0x1000, 8).is_err());
}

#[test]
fn open_and_closed_upvalues_read_the_pointed_value() {
    for value in [0x5000usize, 0x2018] {
        let mut r = base_image();
        r.write_u8(0x2008, 9);
        r.write_u64(0x2010, value as u64);
        r.write_u64(value, 0x4000);
        r.write_u8(value + 8, 0x44);
        let s = collect(reader(r), 100, 100);
        assert!(!s.incomplete, "{:?}", s.diagnostics);
        assert_eq!(s.heaps[0].objects[0].edges[0].target, 0x4000);
    }
}

#[test]
fn sharing_cycles_and_unrooted_threads_do_not_inflate_reachability() {
    let mut s = snapshot();
    s.heaps.push(heap(
        vec![
            object(
                1,
                "table",
                10,
                vec![
                    capture::edge(2, "left", Strength::Strong),
                    capture::edge(2, "right", Strength::Strong),
                ],
            ),
            object(
                2,
                "table",
                20,
                vec![capture::edge(1, "cycle", Strength::Strong)],
            ),
            object(
                3,
                "thread",
                100,
                vec![capture::edge(4, "stack slot[0]", Strength::Strong)],
            ),
            object(4, "table", 200, vec![]),
        ],
        1,
    ));
    analyze::classify(&mut s);
    let report = analyze::report(&s, 10, Some(2));
    assert!(report.contains("registry → left"));
    assert!(report.contains("(30 B)"));
    assert!(report.contains("unconfirmed: 0.00 MiB (300 B)"));
}

#[test]
fn readable_report_prefers_a_real_module_holder_over_a_short_stack_path() {
    let mut snapshot = snapshot();
    let mut sessions = object(
        4,
        "table",
        100,
        vec![capture::edge(0xabcdef, "[\"uid:1\"]", Strength::Strong)],
    );
    sessions.entries = 1;
    sessions.preview = vec![
        "[\"uid:1\"] → @0xabcdef".into(),
        "[\"callback\"] → native:0x1234".into(),
    ];
    let mut child = object(0xabcdef, "table", 40, vec![]);
    child.entries = 2;
    snapshot.heaps.push(heap(
        vec![
            object(
                1,
                "table",
                10,
                vec![capture::edge(2, "[\"_LOADED\"]", Strength::Strong)],
            ),
            object(
                2,
                "table",
                10,
                vec![capture::edge(3, "[\"business\"]", Strength::Strong)],
            ),
            object(
                3,
                "table",
                10,
                vec![capture::edge(4, "[\"sessions\"]", Strength::Strong)],
            ),
            sessions,
            child,
            object(
                6,
                "thread",
                10,
                vec![capture::edge(4, "stack slot[17]", Strength::Strong)],
            ),
        ],
        1,
    ));
    snapshot.heaps[0].roots.push(capture::edge(
        6,
        "skynet active coroutine",
        Strength::Strong,
    ));
    let readable = analyze::report(&snapshot, 10, Some(4));
    assert!(
        readable.contains("Strong holding path: business → sessions"),
        "{readable}"
    );
    assert!(readable.contains("table #") && readable.contains("(2 entries)"));
    assert!(readable.contains("native value"));
    assert!(!readable.contains("0x"), "{readable}");
    let detailed = analyze::report_with_addresses(&snapshot, 10, Some(4), true);
    assert!(detailed.contains("@0x4"));
    assert!(detailed.contains("@0xabcdef"));
    assert!(detailed.contains("native:0x1234"));
    assert!(detailed.contains("global 0x1000"));
    assert!(detailed.contains("registry → [\"_LOADED\"] → [\"business\"] → [\"sessions\"]"));
}

#[test]
fn module_path_presentation_never_promotes_a_weak_holder() {
    let mut snapshot = snapshot();
    snapshot.heaps.push(heap(
        vec![
            object(
                1,
                "table",
                10,
                vec![capture::edge(2, "[\"_LOADED\"]", Strength::Strong)],
            ),
            object(
                2,
                "table",
                10,
                vec![capture::edge(3, "[\"business\"]", Strength::Strong)],
            ),
            object(
                3,
                "table",
                10,
                vec![capture::edge(4, "[\"cache\"]", Strength::Weak)],
            ),
            object(4, "table", 20, vec![]),
        ],
        1,
    ));
    let report = analyze::report(&snapshot, 10, Some(4));
    assert!(report.contains("Strong holding path: unconfirmed"));
    assert!(!report.contains("Strong holding path: registry"));
}

#[test]
fn address_like_business_string_contents_are_preserved() {
    let mut snapshot = snapshot();
    let mut table = object(1, "table", 10, vec![]);
    table.preview = vec![r#"["message"] → "@0xabcdef and native:0x1234""#.into()];
    snapshot.heaps.push(heap(vec![table], 1));
    let report = analyze::report(&snapshot, 10, Some(1));
    assert!(report.contains(r#"["message"] → "@0xabcdef and native:0x1234""#));
}

fn html_fixture() -> Snapshot {
    let mut snapshot = snapshot();
    let mut value_edge = capture::edge(5, "[\"uid:1\"]", Strength::Strong);
    value_edge.role = Role::TableValue;
    let mut sessions = object(4, "table", 6200, vec![value_edge]);
    sessions.entries = 200;
    sessions.preview = vec!["[\"uid:1\"] → @0x5".into(), "[\"uid:2\"] → @0x5".into()];
    let mut record = object(5, "table", 80, vec![]);
    record.entries = 1;
    record.preview = vec!["[\"payload\"] → \"sample\"".into()];
    snapshot.heaps.push(heap(
        vec![
            object(
                1,
                "table",
                10,
                vec![capture::edge(2, "[\"_LOADED\"]", Strength::Strong)],
            ),
            object(
                2,
                "table",
                10,
                vec![capture::edge(3, "[\"business\"]", Strength::Strong)],
            ),
            object(
                3,
                "table",
                10,
                vec![capture::edge(4, "[\"sessions\"]", Strength::Strong)],
            ),
            sessions,
            record,
        ],
        1,
    ));
    snapshot
}

#[test]
fn html_report_uses_the_graph_for_containers_counts_and_holding_paths() {
    let snapshot = html_fixture();
    let page = html::report(&snapshot, 10, false);
    assert!(page.starts_with("<!doctype html>"));
    assert!(
        page.contains("class=\"memory-table\"") && page.contains("data-sort-key=\"reachable\"")
    );
    assert!(page.contains("class=\"row-details\"") && page.contains("hidden><td colspan=\"7\""));
    assert!(!page.contains("<aside") && !page.contains("object-card"));
    assert!(page.contains("business › sessions"));
    assert!(page.contains("uid:* → table · 字段 payload"));
    assert!(page.contains("data-reachable=\"6280\"")); // 6200 + 80 bytes
    assert!(page.contains("business → sessions"));
    assert!(!page.contains("@0x") && !page.contains("global 0x"));
    let detailed = html::report(&snapshot, 10, true);
    assert!(detailed.contains("@0x4"));
    // Filters and folding are static, embedded assets; no network or external scripts.
    assert!(!page.contains("<script src=") && !page.contains("<link "));
    assert!(page.contains("id=\"service-filter\"") && page.contains("id=\"object-search\""));
}

#[test]
fn html_report_escapes_snapshot_text_and_keeps_incomplete_provenance_visible() {
    let mut snapshot = html_fixture();
    let hostile = "\"><img src=x onerror=alert(1)></script><script>alert(2)</script>&'";
    snapshot.heaps[0].services = vec![hostile.into()];
    snapshot.heaps[0].objects[3]
        .preview
        .push(format!("[\"message\"] → {hostile}"));
    snapshot.incomplete = true;
    snapshot.diagnostics = vec![hostile.into()];
    snapshot.capture_method = Some("cooperative test export".into());
    let page = html::report(&snapshot, 10, false);
    assert!(!page.contains("<img src=x"));
    assert_eq!(page.matches("<script>").count(), 1);
    assert!(page.contains("&lt;img src=x onerror=alert(1)&gt;"));
    assert!(page.contains("&amp;&#39;"));
    assert!(page.contains("快照不完整") && page.contains("role=\"status\""));
    assert!(page.contains("各服务分别采集，非全进程同时暂停快照"));
}

#[test]
fn report_format_supports_extension_inference_and_explicit_overrides() {
    for (tail, expected) in [
        (vec!["-o", "report.txt"], ReportFormat::Text),
        (vec!["-o", "report.HTML"], ReportFormat::Html),
        (vec!["-o", "report.htm"], ReportFormat::Html),
        (
            vec!["-o", "report.html", "--format", "text"],
            ReportFormat::Text,
        ),
        (vec!["--format", "html"], ReportFormat::Html),
    ] {
        let args: Vec<_> = ["tracemoon", "mem", "report", "heap.tmoon"]
            .into_iter()
            .chain(tail)
            .map(String::from)
            .collect();
        let config = crate::config::Config::from_args(&args).unwrap();
        let MemoryCommand::Report { format, .. } = config.memory.unwrap() else {
            panic!("report expected")
        };
        assert_eq!(format, expected);
    }
    let args = [
        "tracemoon",
        "mem",
        "report",
        "heap.tmoon",
        "--format",
        "pdf",
    ]
    .map(String::from);
    assert!(crate::config::Config::from_args(&args).is_err());
}

#[test]
fn html_report_command_writes_an_offline_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("heap.tmoon");
    let output = dir.path().join("report.html");
    html_fixture().write(&input).unwrap();
    run(MemoryCommand::Report {
        input,
        core: Default::default(),
        output: Some(output.clone()),
        top: 10,
        object: None,
        show_addresses: false,
        format: ReportFormat::Html,
    })
    .unwrap();
    assert!(std::fs::read_to_string(output)
        .unwrap()
        .contains("business › sessions"));
}

#[test]
fn weak_values_ephemerons_and_missing_metatables_are_not_strong_holders() {
    for mode in ["v", "k", "kv"] {
        let mut s = snapshot();
        let mut table = object(
            1,
            "table",
            10,
            vec![Edge {
                target: 3,
                label: "[\"lost\"]".into(),
                strength: Strength::Strong,
                role: Role::TableValue,
            }],
        );
        table.metatable = 2;
        let mut meta = object(2, "table", 10, vec![]);
        meta.mode = Some(mode.into());
        s.heaps
            .push(heap(vec![table, meta, object(3, "table", 100, vec![])], 1));
        analyze::classify(&mut s);
        assert_ne!(s.heaps[0].objects[0].edges[0].strength, Strength::Strong);
        let report = analyze::report(&s, 10, Some(3));
        assert!(report.contains("Strong holding path: unconfirmed"));
        s.heaps[0].objects.remove(1);
        analyze::classify(&mut s);
        assert_eq!(s.heaps[0].objects[0].edges[0].strength, Strength::Unknown);
    }
}

#[test]
fn snapshot_roundtrip_is_portable_and_rejects_unknown_schemas() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("heap.tmoon");
    let mut s = collect(reader(base_image()), 100, 100);
    s.write(&path).unwrap();
    let decoded = Snapshot::read(&path).unwrap();
    assert_eq!(
        analyze::report(&s, 10, None),
        analyze::report(&decoded, 10, None)
    );
    s.schema_version = 99;
    s.write(&path).unwrap();
    assert!(Snapshot::read(&path).is_err());
    let tmp = dir
        .path()
        .join(format!("heap.tmoon.{}.tmp", std::process::id()));
    std::fs::write(&tmp, "existing capture").unwrap();
    assert!(s.write(&path).is_err());
    assert_eq!(std::fs::read_to_string(tmp).unwrap(), "existing capture");
}

#[test]
fn offline_report_command_needs_no_live_process_and_reports_missing_objects() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("heap.tmoon");
    let output = dir.path().join("report.txt");
    collect(reader(base_image()), 100, 100)
        .write(&input)
        .unwrap();
    run(MemoryCommand::Report {
        input: input.clone(),
        core: Default::default(),
        output: Some(output.clone()),
        top: 10,
        object: None,
        show_addresses: false,
        format: ReportFormat::Text,
    })
    .unwrap();
    let report = std::fs::read_to_string(output).unwrap();
    assert!(report.contains("registry"));
    assert!(report.contains("Service allocation ranking"));
    assert!(run(MemoryCommand::Report {
        input,
        core: Default::default(),
        output: None,
        top: 10,
        object: Some(0x9999),
        show_addresses: false,
        format: ReportFormat::Text
    })
    .is_err());
}

#[test]
fn budget_reader_rejects_short_reads_and_overflowing_addresses() {
    let r = reader(crate::process::reader::testing::ShortReader(1));
    assert!(r.read_memory(0x1000, 16).is_err());
    assert!(r.read_memory(usize::MAX - 1, 16).is_err());
}

#[test]
fn skynet_shared_strings_outside_gc_chains_are_followed_and_marked_shared() {
    let mut image = base_image();
    image.write(0x4020, b"err"); // skynet string data offset
    image.write_u64(0x2000 + layout::TABLE_NODE, 0x8000);
    image.write_u64(0x2000 + layout::TABLE_LASTFREE, 0x8018);
    image.write_u64(0x8000, 0x7000);
    image.write_u8(0x8008, 0x44);
    image.write_u8(0x8009, 0x44);
    image.write_u64(0x8010, 0x6000);
    for (address, text) in [(0x6000, b"field".as_slice()), (0x7000, b"value".as_slice())] {
        image.write_u8(address + 8, 4);
        image.write_u8(address + 9, 7); // Skynet G_SHARED
        image.write_u8(address + 11, text.len() as u8);
        image.write(address + 32, text);
    }
    let r = reader(image);
    let mut snapshot = snapshot();
    let heaps = BTreeMap::from([(
        0x1000,
        DiscoveredHeap {
            services: vec!["fixture".into()],
            active_threads: vec![],
        },
    )]);
    capture::collect(
        &r,
        heaps,
        &mut snapshot,
        100,
        100,
        layout::HeapLayout::skynet54(),
    );
    analyze::classify(&mut snapshot);
    assert!(!snapshot.incomplete, "{:?}", snapshot.diagnostics);
    assert_eq!(
        snapshot.heaps[0]
            .objects
            .iter()
            .filter(|o| o.shared)
            .count(),
        2
    );
    assert_eq!(snapshot.heaps[0].objects.len(), 5);
}

#[test]
fn shared_objects_are_counted_once_and_excluded_from_service_private_totals() {
    let mut shared = object(3, "string", 100, vec![]);
    shared.shared = true;
    let mut snapshot = snapshot();
    snapshot.heaps = vec![
        heap(
            vec![
                object(
                    1,
                    "table",
                    10,
                    vec![capture::edge(3, "field", Strength::Strong)],
                ),
                shared.clone(),
            ],
            1,
        ),
        heap(
            vec![
                object(
                    2,
                    "table",
                    20,
                    vec![capture::edge(3, "field", Strength::Strong)],
                ),
                shared,
            ],
            2,
        ),
    ];
    let report = analyze::report(&snapshot, 10, None);
    assert!(report.contains("Shared code/string objects: 0.00 MiB (100 B) / 1 unique objects"));
    assert!(report.contains("Parsed private allocation: 0.00 MiB (10 B)"));
    assert!(report.contains("Parsed private allocation: 0.00 MiB (20 B)"));
}

#[test]
fn memory_cli_validates_limits_targets_and_versions() {
    for args in [
        vec!["mem", "capture", "-p", "0"],
        vec!["mem", "capture", "-p", "123", "--max-edges", "0"],
        vec!["mem", "capture", "-p", "123", "--lua-version", "5.5"],
        vec!["mem", "capture", "-p", "123", "--", "lua"],
        vec!["mem", "report", "heap.tmoon", "--top", "101"],
    ] {
        let args: Vec<_> = std::iter::once("tracemoon")
            .chain(args)
            .map(String::from)
            .collect();
        assert!(crate::config::Config::from_args(&args).is_err(), "{args:?}");
    }
    let args = [
        "tracemoon",
        "mem",
        "capture",
        "-p",
        "123",
        "--service",
        ".logger",
        "--service",
        ":01000012",
    ]
    .map(String::from);
    let config = crate::config::Config::from_args(&args).unwrap();
    assert_eq!(config.mode, crate::config::RunMode::Memory);
    let MemoryCommand::Capture(options) = config.memory.unwrap() else {
        panic!("capture expected")
    };
    assert_eq!(options.services, [".logger", ":01000012"]);
}

/// Optional real-header/real-allocator fixture, enabled by the developer or CI.
/// The C helper exports its own allocations; no ptrace or root is needed on macOS.
#[test]
fn real_lua54_allocator_dump_has_business_paths_and_no_missing_objects() {
    let Ok(fixture) = std::env::var("TRACEMOON_HEAP_FIXTURE") else {
        return;
    };
    assert_allocator_fixture(fixture, layout::HeapLayout::lua54());
}

#[test]
fn real_skynet_allocator_dump_has_business_paths_and_no_missing_objects() {
    let Ok(fixture) = std::env::var("TRACEMOON_SKYNET_HEAP_FIXTURE") else {
        return;
    };
    assert_allocator_fixture(fixture, layout::HeapLayout::skynet54());
}

fn assert_allocator_fixture(fixture: String, heap_layout: layout::HeapLayout) {
    struct Regions(Vec<(usize, Vec<u8>)>);
    impl ProcessMemoryReader for Regions {
        fn read_memory(&self, addr: usize, len: usize) -> Result<Vec<u8>> {
            for (base, bytes) in &self.0 {
                if let Some(offset) = addr.checked_sub(*base) {
                    if offset.saturating_add(len) <= bytes.len() {
                        return Ok(bytes[offset..offset + len].to_vec());
                    }
                }
            }
            Err(TracemoonError::Other(format!(
                "unmapped fixture address 0x{addr:x}"
            )))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("heap.bin");
    assert!(std::process::Command::new(fixture)
        .arg("--dump")
        .arg(&dump)
        .status()
        .unwrap()
        .success());
    let data = std::fs::read(dump).unwrap();
    let mut offset = 0;
    fn word(data: &[u8], offset: &mut usize) -> u64 {
        let n = u64::from_ne_bytes(data[*offset..*offset + 8].try_into().unwrap());
        *offset += 8;
        n
    }
    let state = word(&data, &mut offset) as usize;
    let n = word(&data, &mut offset);
    let mut regions = Vec::new();
    for _ in 0..n {
        let address = word(&data, &mut offset) as usize;
        let len = word(&data, &mut offset) as usize;
        regions.push((address, data[offset..offset + len].to_vec()));
        offset += len;
    }
    if heap_layout.shared_age.is_none() {
        // Real allocator bytes in a portable ELF wrapper exercise the full core
        // pipeline on macOS too. Linux acceptance separately uses GDB-generated cores.
        let input = dir.path().join("allocator.core");
        let core_regions: Vec<_> = regions
            .iter()
            .map(|(address, data)| (*address as u64, data.clone(), data.len() as u64, 6))
            .collect();
        core::fixture(&input, &core_regions, &[]);
        let mut core_snapshot = core::read(
            &input,
            &core::Options {
                lua54: true,
                ..Default::default()
            },
        )
        .unwrap();
        analyze::classify(&mut core_snapshot);
        assert!(!core_snapshot.incomplete, "{:?}", core_snapshot.diagnostics);
        let report = html::report(&core_snapshot, 100, false);
        assert!(report.contains("player.cache") && report.contains("› sessions"));
        assert!(report.contains("函数通过捕获变量持有"));
    }
    let r = reader(Regions(regions));
    let heaps = capture::discover(&r, 123, None, &[], Some(state)).unwrap();
    let mut s = snapshot();
    capture::collect(&r, heaps, &mut s, 100_000, 100_000, heap_layout);
    analyze::classify(&mut s);
    assert!(!s.incomplete, "{:?}", s.diagnostics);
    let report = analyze::report(&s, 100, None);
    assert!(report.contains("player.cache"), "{report}");
    assert!(report.contains("sessions"), "{report}");
    assert!(s.heaps[0]
        .objects
        .iter()
        .any(|o| o.kind == "userdata" && o.shallow_bytes >= 4096));
    assert!(s.heaps[0]
        .objects
        .iter()
        .any(|o| o.kind == "Lua closure" && o.summary.contains("heap_fixture.lua")));
    let weak = s.heaps[0]
        .objects
        .iter()
        .find(|o| o.edges.iter().any(|e| e.label == "[\"lost\"]"))
        .unwrap();
    assert_eq!(
        weak.edges
            .iter()
            .find(|e| e.label == "[\"lost\"]")
            .unwrap()
            .strength,
        Strength::Weak
    );
}

/// Manual report preview from the two real snlua services' cooperative exports.
/// This never substitutes for external all-thread capture acceptance on Linux.
#[test]
#[ignore = "run tests/preview-skynet-mem.sh to export real service heaps first"]
fn write_cooperative_skynet_preview_snapshot() {
    struct Regions(BTreeMap<usize, Vec<u8>>);
    impl ProcessMemoryReader for Regions {
        fn read_memory(&self, address: usize, len: usize) -> Result<Vec<u8>> {
            if let Some((&base, bytes)) = self.0.range(..=address).next_back() {
                let offset = address - base;
                if offset.saturating_add(len) <= bytes.len() {
                    return Ok(bytes[offset..offset + len].to_vec());
                }
            }
            Err(TracemoonError::Other(format!(
                "missing exported region 0x{address:x}"
            )))
        }
    }
    fn word(bytes: &[u8], offset: &mut usize) -> u64 {
        let value = u64::from_ne_bytes(bytes[*offset..*offset + 8].try_into().unwrap());
        *offset += 8;
        value
    }
    let output = PathBuf::from(std::env::var("TRACEMOON_EXPORT_DIR").expect("export directory"));
    let mut snapshot = snapshot();
    snapshot.lua_abi = "skynet-lua54".into();
    snapshot.capture_method = Some("cooperative test export from real snlua services; captured separately, not a simultaneous process snapshot; no external pause (pause_ms=0)".into());
    snapshot.pause_ms = 0;
    snapshot.limits = snapshot::CaptureLimits {
        pause_ms: 10_000,
        objects: 100_000,
        edges: 100_000,
        read_bytes: 32 * 1024 * 1024,
    };
    for name in ["alpha", "beta"] {
        let bytes = std::fs::read(output.join(format!("{name}.bin"))).unwrap();
        let mut offset = 0;
        let state = word(&bytes, &mut offset) as usize;
        let count = word(&bytes, &mut offset);
        let mut regions = BTreeMap::new();
        for _ in 0..count {
            let address = word(&bytes, &mut offset) as usize;
            let len = word(&bytes, &mut offset) as usize;
            assert!(regions
                .insert(address, bytes[offset..offset + len].to_vec())
                .is_none());
            offset += len;
        }
        let pid = word(&bytes, &mut offset) as i32;
        let time = word(&bytes, &mut offset);
        let export_ns = word(&bytes, &mut offset);
        let handle = word(&bytes, &mut offset);
        assert_eq!(offset, bytes.len());
        if snapshot.pid != 123 {
            assert_eq!(snapshot.pid, pid);
        }
        snapshot.pid = pid;
        snapshot.captured_unix_secs = time;
        let r = reader(Regions(regions));
        let mut heaps = capture::discover(&r, pid, None, &[], Some(state)).unwrap();
        assert_eq!(heaps.len(), 1);
        let discovered = heaps.values_mut().next().unwrap();
        discovered.services = vec![format!("trace_{name} ({handle:08x})")];
        discovered.active_threads.push(state);
        capture::collect(
            &r,
            heaps,
            &mut snapshot,
            100_000,
            100_000,
            layout::HeapLayout::skynet54(),
        );
        snapshot.read_bytes += r.bytes.load(std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "trace_{name}: cooperative copy took {:.3} ms",
            export_ns as f64 / 1_000_000.0
        );
    }
    analyze::classify(&mut snapshot);
    assert!(!snapshot.incomplete, "{:?}", snapshot.diagnostics);
    assert_eq!(snapshot.heaps.len(), 2);
    let report = analyze::report(&snapshot, 100, None);
    assert!(report.contains("sessions") && report.contains("tracemoon.fixture"));
    snapshot.write(&output.join("heap.tmoon")).unwrap();
    let mut focused = String::new();
    for heap in &snapshot.heaps {
        let module = heap
            .objects
            .iter()
            .flat_map(|o| &o.edges)
            .find(|edge| edge.label == "[\"tracemoon.fixture\"]")
            .unwrap()
            .target;
        let sessions = heap
            .objects
            .iter()
            .find(|o| o.address == module)
            .unwrap()
            .edges
            .iter()
            .find(|edge| edge.label == "[\"sessions\"]")
            .unwrap()
            .target;
        let object = heap.objects.iter().find(|o| o.address == sessions).unwrap();
        assert_eq!(object.entries, 200);
        focused.push_str(&format!(
            "{} 0x{sessions:x}\n",
            heap.services[0].split(' ').next().unwrap()
        ));
    }
    std::fs::write(output.join("sessions.index"), focused).unwrap();
}
