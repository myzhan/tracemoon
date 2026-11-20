//! Real-process acceptance. Run on Linux after `bash tests/build-vendor.sh`:
//! cargo test --test integration -- --ignored --test-threads=1
//! Explicitly ignored in the fast unit suite; CI runs every test here as a required job.
#![cfg(target_os = "linux")]

use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

struct Vendors {
    lua: PathBuf,
    skynet: PathBuf,
}

fn vendors() -> &'static Vendors {
    static VENDORS: OnceLock<Vendors> = OnceLock::new();
    VENDORS.get_or_init(|| {
        let built = |name: &str| {
            let manifest = root().join(format!("target/test-vendor/{name}.path"));
            let directory = PathBuf::from(
                fs::read_to_string(&manifest)
                    .unwrap_or_else(|e| {
                        panic!(
                            "{}: {e}; run bash tests/build-vendor.sh",
                            manifest.display()
                        )
                    })
                    .trim(),
            );
            let revision = Command::new("git")
                .arg("-C")
                .arg(root().join(format!("tests/vendor/{name}")))
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            assert!(revision.status.success(), "Missing {name} submodule");
            let revision = String::from_utf8(revision.stdout).unwrap();
            assert!(
                directory
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(revision.trim()),
                "Stale {name} build; run bash tests/build-vendor.sh"
            );
            directory
        };
        let vendors = Vendors {
            lua: built("lua"),
            skynet: built("skynet"),
        };
        assert!(vendors.lua.join("lua").is_file());
        assert!(vendors.skynet.join("skynet").is_file());
        vendors
    })
}

/// File-backed output cannot deadlock on a full pipe. A private process group lets a
/// failing/timed-out test clean up both tracemoon and any target it spawned.
fn run(mut command: Command) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let stdout_path = dir.path().join("stdout");
    let stderr_path = dir.path().join("stderr");
    command
        .stdout(Stdio::from(File::create(&stdout_path).unwrap()))
        .stderr(Stdio::from(File::create(&stderr_path).unwrap()));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {command:?}: {e}"));
    let pid = child.id() as i32;
    let deadline = Instant::now() + Duration::from_secs(60);
    let (status, timeout) = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break (status, false);
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            break (child.wait().unwrap(), true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Clean up any orphaned target even if tracemoon unexpectedly failed before Drop.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let output = Output {
        status,
        stdout: fs::read(stdout_path).unwrap(),
        stderr: fs::read(stderr_path).unwrap(),
    };
    assert!(
        !timeout && output.status.success(),
        "{command:?}\nstatus: {} (timeout={timeout})\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn tracer(skynet: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tracemoon"));
    command.current_dir(if skynet { &vendors().skynet } else { root() });
    command.env(
        "TRACEMOON_FIXTURE_DIR",
        root().join("tests/fixtures/skynet"),
    );
    command
}

fn target(command: &mut Command, skynet: bool) {
    command.arg("--");
    if skynet {
        command
            .arg(vendors().skynet.join("skynet"))
            .arg(root().join("tests/fixtures/skynet/config"));
    } else {
        command
            .arg(vendors().lua.join("lua"))
            .arg(root().join("tests/fixtures/plain.lua"));
    }
}

fn core_report(skynet: bool) {
    let dir = tempfile::tempdir().unwrap();
    let core = dir.path().join("core.dump");
    let executable = if skynet {
        vendors().skynet.join("skynet")
    } else {
        vendors().lua.join("lua")
    };
    let mut gdb = Command::new("gdb");
    gdb.current_dir(if skynet { &vendors().skynet } else { root() })
        .env(
            "TRACEMOON_FIXTURE_DIR",
            root().join("tests/fixtures/skynet"),
        )
        .env(
            "TRACEMOON_CORE_TRIGGER",
            root().join("target/test-vendor/core-trigger.so"),
        )
        .args([
            "--batch",
            "--nx",
            "-ex",
            "set pagination off",
            "-ex",
            "set confirm off",
            "-ex",
            "run",
            "-ex",
        ])
        .arg(format!("generate-core-file {}", core.display()))
        .args(["-ex", "kill", "--args"])
        .arg(&executable)
        .arg(root().join(if skynet {
            "tests/fixtures/skynet/config"
        } else {
            "tests/fixtures/plain.lua"
        }));
    let generated = run(gdb);
    assert!(String::from_utf8_lossy(&generated.stdout).contains("SIGABRT"));
    assert!(core.is_file());
    // GDB has killed/reaped its target. Report/inspect must rely only on the core
    // and explicitly supplied binary, even when the original process is gone.
    for format in ["txt", "html"] {
        let output = dir.path().join(format!("report.{format}"));
        let mut command = tracer(false);
        command
            .args(["mem", "report"])
            .arg(&core)
            .arg("--exe")
            .arg(&executable)
            .args(["--top", "100", "-o"])
            .arg(&output);
        if format == "txt" {
            command.arg("--show-addresses");
        }
        run(command);
        let report = fs::read_to_string(output).unwrap();
        assert!(report.contains("tracemoon.fixture"), "{report}");
        assert!(report.contains("sessions"), "{report}");
        if format == "html" {
            assert!(report.contains("tracemoon.fixture › sessions"));
            assert!(report.contains("flush → 捕获变量 sessions"));
        } else {
            // Exercise the same address hand-off a user makes from report to inspect.
            let address = report
                .lines()
                .filter(|line| line.trim_start().starts_with("Object #"))
                .find_map(|line| line.rsplit_once(" / @").map(|(_, addr)| addr))
                .expect("detailed core report should expose an inspectable object");
            let mut inspect = tracer(false);
            inspect
                .args(["mem", "inspect"])
                .arg(&core)
                .arg("--exe")
                .arg(&executable)
                .args(["--show-addresses", "--object", address]);
            let inspected = String::from_utf8(run(inspect).stdout).unwrap();
            assert!(inspected
                .lines()
                .any(|line| line.trim_start().starts_with("Object #")
                    && line.ends_with(&format!(" / @{address}"))));
        }
        assert!(
            !report.contains("INCOMPLETE") && !report.contains("快照不完整"),
            "{report}"
        );
        if skynet {
            assert!(
                report.contains("trace_alpha") && report.contains("trace_beta"),
                "{report}"
            );
            assert!(report.contains("skynet-lua54"), "{report}");
        }
    }
    if skynet {
        let output = dir.path().join("selected.txt");
        let mut command = tracer(false);
        command
            .args(["mem", "report"])
            .arg(&core)
            .arg("--exe")
            .arg(&executable)
            .args(["--service", "trace_alpha", "-o"])
            .arg(&output);
        run(command);
        let report = fs::read_to_string(output).unwrap();
        assert!(
            report.contains("trace_alpha") && !report.contains("trace_beta"),
            "{report}"
        );
    }
}

#[test]
#[ignore = "requires built vendors and GDB; mandatory in Linux integration CI"]
fn plain_lua_real_core_has_business_objects_and_reports() {
    core_report(false);
}

#[test]
#[ignore = "requires built vendors and GDB; mandatory in Linux integration CI"]
fn skynet_real_core_has_both_service_heaps_and_filtering() {
    core_report(true);
}

fn json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn record(skynet: bool, service: Option<&str>) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stacks.txt");
    let mut command = tracer(skynet);
    command
        .args(["record", "-d", "3", "-r", "100", "-o"])
        .arg(&path);
    if let Some(service) = service {
        command.args(["--service", service]);
    }
    target(&mut command, skynet);
    run(command);
    let stacks = fs::read_to_string(&path).unwrap();
    let metadata = json(&PathBuf::from(format!("{}.meta.json", path.display())));
    assert!(
        metadata["sampled_stacks"].as_u64().unwrap() > 50,
        "{metadata}"
    );
    assert!(
        metadata["success_rate"].as_f64().unwrap() >= 90.0,
        "{metadata}"
    );
    assert_eq!(metadata["lua_version"], "5.4");
    assert!(
        !stacks.contains("getenv") && !stacks.contains("gcov"),
        "{stacks}"
    );
    assert!(
        stacks.contains(if skynet {
            "tracemoon_worker.lua:"
        } else {
            "plain.lua:"
        }),
        "{stacks}"
    );
    stacks
}

#[test]
#[ignore = "requires built vendors and Linux ptrace; mandatory in integration CI"]
fn plain_lua_cpu_samples_have_lua_and_native_symbols() {
    let stacks = record(false, None);
    assert!(stacks.contains(":main"), "{stacks}");
    #[cfg(target_arch = "x86_64")]
    assert!(
        stacks.contains("[C]:luaV_execute") || stacks.contains("[C]:math_sin"),
        "{stacks}"
    );
}

#[test]
#[ignore = "requires built vendors and Linux ptrace; mandatory in integration CI"]
fn skynet_cpu_samples_have_two_real_services_and_filtering() {
    let stacks = record(true, None);
    assert!(stacks.contains("[service:trace_alpha]"), "{stacks}");
    assert!(stacks.contains("[service:trace_beta]"), "{stacks}");
    let selected = record(true, Some("trace_alpha"));
    assert!(selected.contains("[service:trace_alpha]"), "{selected}");
    assert!(!selected.contains("[service:trace_beta]"), "{selected}");
}

fn capture(skynet: bool, service: Option<&str>) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("heap.tmoon");
    let mut command = tracer(skynet);
    command
        .args([
            "mem",
            "capture",
            "--startup-ms",
            "1000",
            "--max-pause-ms",
            "5000",
            "-o",
        ])
        .arg(&path);
    if let Some(service) = service {
        command.args(["--service", service]);
    }
    target(&mut command, skynet);
    run(command);
    let snapshot = json(&path);
    assert_eq!(snapshot["incomplete"], false, "{}", snapshot["diagnostics"]);
    assert_eq!(
        snapshot["lua_abi"],
        if skynet { "skynet-lua54" } else { "lua54" }
    );
    let heaps = snapshot["heaps"].as_array().unwrap();
    assert!(!heaps.is_empty());
    let has_shared = heaps
        .iter()
        .flat_map(|heap| heap["objects"].as_array().unwrap())
        .any(|object| object["shared"] == true);
    assert_eq!(
        has_shared, skynet,
        "skynet shared code must be discovered outside per-service GC chains"
    );
    let business_heaps: Vec<_> = heaps
        .iter()
        .filter(|heap| {
            heap["objects"].as_array().unwrap().iter().any(|object| {
                object["edges"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| edge["label"] == "[\"tracemoon.fixture\"]")
            })
        })
        .collect();
    assert_eq!(
        business_heaps.len(),
        if skynet && service.is_none() { 2 } else { 1 },
        "{snapshot}"
    );
    for heap in business_heaps {
        let objects = heap["objects"].as_array().unwrap();
        assert!(objects.len() > 400);
        assert!(objects
            .iter()
            .any(|object| object["entries"].as_u64() == Some(200)
                && object["shallow_bytes"].as_u64().unwrap() > 200 * 16));
        assert!(objects.iter().any(|object| object["kind"] == "Lua closure"
            && object["summary"].as_str().unwrap().contains(if skynet {
                "tracemoon_worker.lua"
            } else {
                "plain.lua"
            })));
    }
    // The capture command has exited and terminated its spawned target. Offline reporting
    // must still recover business paths; it cannot accidentally fall back to live reads.
    let report_path = dir.path().join("report.txt");
    let mut report = tracer(false);
    report
        .args(["mem", "report"])
        .arg(&path)
        .args(["--top", "100", "-o"])
        .arg(&report_path);
    run(report);
    let report = fs::read_to_string(report_path).unwrap();
    assert!(
        report.contains("tracemoon.fixture") && report.contains("sessions"),
        "{report}"
    );
    assert!(!report.contains("INCOMPLETE"), "{report}");
    let html_path = dir.path().join("report.html");
    let mut html = tracer(false);
    html.args(["mem", "report"])
        .arg(&path)
        .arg("-o")
        .arg(&html_path);
    run(html);
    let html = fs::read_to_string(html_path).unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("tracemoon.fixture › sessions"));
    assert!(html.contains("id=\"service-filter\"") && html.contains("id=\"object-search\""));
    let address = heaps[0]["objects"][0]["address"].as_u64().unwrap();
    let mut inspect = tracer(false);
    inspect
        .args(["mem", "inspect"])
        .arg(&path)
        .arg("--show-addresses")
        .arg("--object")
        .arg(format!("0x{address:x}"));
    let inspect = run(inspect);
    assert!(String::from_utf8_lossy(&inspect.stdout).contains(&format!("@0x{address:x}")));
    snapshot
}

#[test]
#[ignore = "requires built vendors and Linux ptrace; mandatory in integration CI"]
fn plain_lua_heap_contains_business_objects_and_offline_holding_paths() {
    assert_eq!(capture(false, None)["heaps"].as_array().unwrap().len(), 1);
}

#[test]
#[ignore = "requires built vendors and Linux ptrace; mandatory in integration CI"]
fn skynet_heap_contains_two_service_heaps_and_supports_name_and_handle_filters() {
    let all = capture(true, None);
    let heaps = all["heaps"].as_array().unwrap();
    for name in ["trace_alpha", "trace_beta"] {
        assert!(
            heaps.iter().any(|h| h["services"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s.as_str().unwrap().starts_with(name))),
            "{all}"
        );
    }
    let selected = capture(true, Some("trace_alpha"));
    assert_eq!(selected["heaps"].as_array().unwrap().len(), 1);
    assert!(selected["heaps"][0]["services"][0]
        .as_str()
        .unwrap()
        .starts_with("trace_alpha"));
    // Isolated bootstrap makes service creation deterministic across spawned runs.
    let label = selected["heaps"][0]["services"][0].as_str().unwrap();
    let handle = label.split('(').nth(1).unwrap().trim_end_matches(')');
    let by_handle = capture(true, Some(&format!(":{handle}")));
    assert_eq!(
        by_handle["heaps"][0]["services"],
        selected["heaps"][0]["services"]
    );
}
