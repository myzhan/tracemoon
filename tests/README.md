# Test vendors and real-process acceptance

The upstream sources are Git submodules, pinned by tracemoon's gitlinks:

| Path | Upstream | Pin | Tested ABI |
|---|---|---|---|
| `tests/vendor/lua` | https://github.com/lua/lua | `v5.4.7` / `1ab3208a1fceb12fca8f24ba57d6e13c5bff15e3` | Standard Lua 5.4 |
| `tests/vendor/skynet` | https://github.com/cloudwu/skynet | `v1.8.0` / `ba64be6f9fa044933c77de1317f466afbade8eaa` | Skynet's patched Lua 5.4.7 |

Skynet's version string does not identify its ABI changes: `TString.id` moves string
length/data fields, and removing `global_State.seed` moves `gcstate`. We check both header
sets independently and run the heap decoder against allocator dumps built with both Lua
libraries. Do not replace the embedded Lua with stock Lua to make these checks pass.

## Setup and build

```bash
git submodule update --init tests/vendor/lua tests/vendor/skynet
bash tests/build-vendor.sh
```

The build script requires Git, make, a C compiler, and standard platform development tools.
It archives each checked-out commit into `target/test-vendor/<name>-<commit>-<platform>` and
builds there. Vendor working trees remain clean. Re-running it reuses the build for that
commit. Run it again after changing a submodule pin; integration tests reject stale builds.
Do not run multiple instances of this build script concurrently in one checkout.

The skynet build disables jemalloc and its malloc hook; Lua heaps and real service scheduling
are unchanged. Its nested jemalloc submodule is not needed. No runtime downloads, installed
system Lua interpreter, external skynet checkout, sockets, or third-party services are needed.

## Portable decoder tests

After building, enable the two real-allocator fixtures on Linux or macOS:

```bash
TRACEMOON_HEAP_FIXTURE="$PWD/target/test-vendor/heap-fixture" \
TRACEMOON_SKYNET_HEAP_FIXTURE="$PWD/target/test-vendor/heap-fixture-skynet" \
cargo test --all-targets
```

These fixtures export their own allocated regions for an offline reader. They verify real
objects and paths without requiring ptrace or macOS `task_for_pid`. Without these variables,
the ordinary fast unit suite remains usable without fetching/building the vendors. CI always
sets both variables; a missing or broken fixture is a failure there.

## Real-process integration tests (Linux)

Install `liblzma-dev` and `libunwind-dev` on Linux x86_64, then run:

```bash
bash tests/build-vendor.sh
cargo test --release --test integration -- --ignored --test-threads=1
```

The integration suite invokes the actual tracemoon CLI. Tracemoon spawns the targets itself,
so these tests need no root under the usual Yama `ptrace_scope=1` parent/child policy.
Stricter host policies can still deny tracing. Each invocation has a timeout and private
process group, with cleanup for the profiler and spawned target on failure.

| Process | CPU assertions | Heap assertions |
|---|---|---|
| Vendored Lua | Real fixture source lines, sample counts, success rate, native symbols on x86_64 | One complete heap, expected business container and allocation sizes, closure source, offline report and inspect |
| Vendored skynet | Both named worker VMs, real Lua source lines, exact service filtering | Two distinct business heaps, patched ABI, per-service name and handle filtering, offline holding paths |

The CPU workload stays busy so success-rate checks do not depend on catching timer wakeups.
The fixture business data stays stable and GC is stopped after setup, allowing heap tests
to verify object enumeration independently of live GC transition tests in the unit suite.
The bootstrap starts the real launcher and two real `snlua` services; no fake `H` or process
memory is used in this integration suite.

The tests are explicitly ignored in the default fast suite and selected by a dedicated,
required Linux CI job. Running `cargo test` alone is not real-process acceptance. macOS
builds both upstreams and validates decoding; heap process capture remains Linux-only.

Core tests include portable ELF64 images covering cross-segment reads, omitted pages,
truncation, format validation, Linux notes, PIE/non-PIE relocation, executable mismatch,
and immutable-only recovery. The stock Lua real-allocator export is also wrapped in ELF
to exercise automatic VM discovery and ownership reports on macOS (this wrapper is not
a kernel/GDB-generated core).

Linux integration additionally requires `gdb`. A test-only module raises SIGABRT after
the plain Lua fixture or both real skynet services initialize. GDB, as the target's parent,
saves a real core and kills/reaps the target; the tests then run `mem report` directly on
that core, checking text/HTML business paths and skynet service filtering. These tests
are included in the same mandatory `--ignored` CI invocation. The trigger module is never
linked into tracemoon or upstream vendors.

## Preview a real skynet memory report on macOS

```bash
bash tests/preview-skynet-mem.sh
```

This starts the real vendored skynet with the two test services and loads a **test-only**
C module that cooperatively exports each service's heap at a Lua C-call boundary. The
ordinary tracemoon decoder and offline CLI generate `report.html`, `report.txt`, a focused
`business-report.txt`, and `heap.tmoon` under the printed `target/reports/skynet-mem.*`
directory. The process is terminated after export; the analysis uses only saved bytes.

These are real service heaps, captured separately, with explicit provenance in the
snapshot/report. They are not a simultaneous process snapshot, do not measure external
pause time, and do not replace Linux ptrace acceptance. The helper is never linked into
tracemoon or the upstream source trees.

Shared Proto/string objects in skynet's code cache can sit outside the service GC chains.
The capture follows supported references marked `G_SHARED`; reports separate private
allocation from shared code/strings and count shared addresses once across selected services.

Reports default to readable object numbers, type/entry summaries and module holding paths.
Use `mem report heap.tmoon --show-addresses` or add `--show-addresses` to `mem inspect`
when exact pointers are needed. Display numbers are local to a service; the inspection
command still takes a hex address.

HTML rendering is covered by unit tests for format inference/overrides, offline output,
graph-derived container summaries, escaping untrusted snapshot strings, and visible
incomplete/provenance information. Linux process tests also regenerate HTML from each
captured plain-Lua and skynet snapshot.

## Updating pins

Check out the intended commit inside each submodule, rebuild, then stage the gitlinks:

```bash
git add tests/vendor/lua tests/vendor/skynet
```

Run both the portable decoder tests and Linux integration suite before accepting a new pin.
Keep release/commit details above in sync. Do not follow upstream HEAD automatically: an
upstream upgrade can change the memory layout even when its Lua release string is unchanged.
