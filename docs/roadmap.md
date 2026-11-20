# Roadmap

Current direction and implementation status as of 2026-11-20.

[Implementation notes](lua-profiler-ptrace.md) describe the existing profiler.
[Memory snapshot design](memory-snapshot-design.md) describes the heap analyzer and its remaining work.
This roadmap records priorities and scope; proposed commands are not available until
implemented and validated. The original item IDs are retained for continuity.

## Current capabilities

The CPU profiling foundation includes Lua 5.4/5.5 stack walking, native C merge on supported
platforms in single-VM mode, skynet multi-VM discovery, symbol resolution, and
`record` / `dump` / `top`. Samples describe observed stacks; they do not directly measure
CPU execution time or individual message latency.

The latest changes implement three improvements:

| Item | Implemented behavior | Remaining scope |
|---|---|---|
| T1-3 Metadata | `record` writes `<output>.meta.json` with target context and sampling statistics | No checkpoint or timeline implied |
| T1-4 Error-path output | Attach/spawn recording attempts output even when sampling returns an error; the original sampling error takes precedence | Periodic checkpoints deferred |
| T2-1 Service filtering | Repeatable `--service` in record/dump/top, matching exact labels or `:xxxxxxxx` handles before stack walking | Split files deferred |
| T3-1 Heap snapshot | Linux Lua 5.4 capture, portable offline report/inspect, large-object candidates and holding paths | Real-workload validation, Lua 5.5, richer GC semantics |

Metadata includes a schema version, tool version, PID, spawn argv (null for attach),
service selectors, Lua layout and confirmation status, requested sample rate, start time
in Unix seconds, elapsed sampling time, ticks, failed ticks, sampled stacks, and success
rate. Elapsed sampling time excludes child cleanup and output generation.

Service filters use OR semantics. Non-skynet targets reject them. The success rate applies
to the selected services; absent or unwalkable selections produce no usable stacks.
A synthetic service root alone must not count as a successful sample.

**Validation status:** formatting, unit tests (including a real Lua 5.4.7 allocator dump),
host Clippy, and Linux x86_64 cross-target Clippy pass locally. The heap ABI is independently
checked against the pinned Lua and skynet C headers. Both real allocator-dump decoder tests
pass locally, including skynet's patched string/GC offsets. Upstreams are test submodules;
CI explicitly runs `tests/integration.rs` and that job passes on Linux for real Lua and
skynet CPU/heap capture, service filters and offline reports. Local macOS validation is not
a substitute for it. See [test setup](../tests/README.md).

## Immediate priorities

1. Complete Linux runtime acceptance for pause/resume and heap capture, then real skynet
   acceptance for report usefulness, capture consistency, service identity and pause cost.
2. Validate report clarity on representative large service heaps before expanding scope.
   Confirm weak/conditional references and partial captures remain clearly labeled.
3. Choose further CPU output improvements from actual usage. Speedscope is a useful
   candidate; checkpoints depend on demand for long-running sessions.

There is no requirement to finish every CPU backlog item before memory work, and no
fixed day-count or line-count estimate for the heap analyzer.

## Main feature in progress: single heap snapshot (T3-1)

**Status: initial Lua 5.4 implementation; Linux runtime and skynet acceptance pass in CI,
validation on real workloads remains.**

Available commands are `mem capture`, `mem report`, and `mem inspect`. Capture supports
attach or spawn on 64-bit Linux; offline reporting also works on macOS. The saved JSON
graph includes roots, objects, typed reference edges, budgets, and completeness diagnostics.
Offline reports support text and standalone HTML (`--format text|html`, with `.html`/`.htm`
auto-detection). HTML presents a compact sortable object table with search/service filters
and expandable rows; it uses the same bounded graph analysis and preserves capture provenance.
`mem report core.dump --exe ./skynet -o report.html` also accepts Linux ELF64 cores directly,
without an import step; `mem inspect` accepts them too. Little-endian x86_64/aarch64 cores
are read offline on Linux/macOS, with AT_PHDR/NT_FILE relocation, explicit executable
read-only recovery, and incomplete diagnostics for missing writable mappings/objects.
Skynet requires a matching unstripped ELF binary with `H`. Lua 5.5, compressed/Mach-O cores,
and shared-library file recovery remain unsupported. Extraction uses fixed 60-second,
1 GiB-read, 1-million-object and 2-million-edge budgets. Portable ELF/allocator tests,
Linux cross-Clippy, and the real GDB Lua/skynet core cases pass in the Linux CI job; they
remain the acceptance gate for any change in this area.
Default capture budgets are 2 seconds, 1 million objects, 2 million edges, and 256 MiB of
remote reads. Capture limits return a saved incomplete result and a nonzero exit status.

The goal is a clear report from one capture: which service consumes memory, what its large
objects contain, and which roots, module fields, or function upvalues hold those objects.
Snapshot comparison is deferred.

The earlier live-memory-sampling proposal is replaced by:

1. Pause all target threads and confirm they are stopped.
2. Capture the Lua object graph, roots, and diagnostics within explicit resource budgets.
3. Resume the target, including on partial capture or errors.
4. Analyze the saved graph offline and produce a readable report.

The first report should show:

- Capture time, actual pause duration, selected services, and coverage/limitations.
- Per-service and per-object-type memory totals.
- Large tables and other important containers, with shallow size and bounded content summaries.
- Readable root paths, such as
  `registry → _LOADED["player.cache"] → ["sessions"] → table`.
- Alternative holding paths and available function source/upvalue names.
- Strongly reachable bytes separately from objects whose reachability is unconfirmed.

### Boundaries that define the design

- Tables and strings do not record allocation source locations. A function holding a table
  can supply a source clue, not proof of where the table was allocated.
- `allgc` alone is insufficient. Enumerate the version-specific ownership lists, including
  finalization and fixed-object lists, and deduplicate heaps by `global_State`.
- GC enumeration links are not business references or roots. Unreachable coroutines must
  not become artificial roots merely because their thread objects were enumerated.
- Stopping all threads removes concurrent writes but can catch a structure mid-update.
  Validate consistency and explicitly report incomplete captures.
- Pausing does not collect garbage. Until weak references, ephemerons, finalizers, and roots
  are modeled correctly, report “unconfirmed reachability,” not “garbage.”
- Shallow bytes and bytes reachable through references are different measurements.
  Reachable totals may overlap and cannot be added across report entries.
- Precise retained size, allocation history, automatic leak verdicts, and native allocations
  behind arbitrary userdata pointers are outside the first version.
- Lua object bytes are not process RSS. Allocator metadata and fragmentation are excluded.

### Delivery gates and remaining work

1. Freeze/resume implemented with RAII cleanup and explicit resume error reporting;
   Linux tests cover all-thread stops, success/error cleanup, expired budgets and existing
   job-control stops. Runtime execution, clone/exit races and tracer-death behavior still
   need acceptance.
2. Lua 5.4 object enumeration and size validation implemented; C ABI checks and allocator
   fixtures cover real tables, strings, closures, userdata, coroutines and finalizable tables.
3. Root/reference decoding, bounded ownership paths, reports and offline inspection are
   implemented. Validate clarity and scaling on representative workloads.
4. Weak values are distinguished from strong holders. Weak-key values remain conditional
   without ephemeron convergence; finalizer semantics remain conservative. Missing references
   and capture failures mark the result incomplete. No garbage or leak verdict is emitted.
5. Real skynet acceptance: report clarity, heap coverage, pause cost, and analyzer memory use.
6. Lua 5.5 layout verification and the same fixture coverage before enabling support.

Reuse process readers, symbols, and VM discovery, but keep heap capture/analysis separate
from the CPU `Aggregator`. Per-service CPU buckets are not a prerequisite for memory reports.

See [the full design](memory-snapshot-design.md) for example output, module boundaries,
resource limits, and acceptance cases.

## CPU profiling backlog

### T1-2 Speedscope output — candidate

**Why:** preserve the order of observations so users can inspect changes over a run rather
than only aggregate flamegraph counts.

**Design constraints:**

- Record an opt-in, timestamped sample sequence with interned frames and bounded memory.
- Keep each service's timeline separate in multi-VM mode; stacks collected in one tick
  must not be serialized as successive time intervals of one combined profile.
- Preserve gaps, failed observations, and actual elapsed time honestly. Nominal `1/rate`
  weights alone do not explain delayed ticks.
- Emit root-to-leaf samples exactly once. The walker yields deepest-first; existing
  collapsed keys already reverse that order.
- At 100 Hz and depth 100, one hour of single-VM observations is 36 million frame indices
  (about 144 MB at four bytes each), before container overhead. Multiple services multiply
  the cost, so explicit caps and truncation diagnostics are required.

Choose this when temporal inspection becomes a recurring need. It is not required for
the memory snapshot feature.

### T1-4 Periodic checkpoints — deferred

Error-path output is implemented; mid-run durability remains separate work.

For long recording sessions, consider interval-based checkpoints using a temporary file
and atomic rename. Never write on every sampling tick. Define which output and metadata
are checkpointed, how failures are surfaced, and how repeated serialization affects sampling.

Current output does not protect against SIGKILL, OOM termination, or reboot.

### T2-1 Per-service split output — deferred

Filtering is implemented; existing synthetic service roots already provide grouping.
Do not add a redundant `--group-by service` flag without a behavioral need.

If operators need one file per service, add `--split-by-service` with stable handle-based
identity, safe filenames, shared tick accounting, and consistent metadata. Avoid wrapping
whole aggregators per service unless the output model actually requires it.

### T1-1 Consecutive stack observations — needs measurement design

This could highlight stacks observed repeatedly over a time window. It must not be named
or presented as measured message latency or a proven stall.

Identical samples may come from a hot loop, repeated short requests, blocking, or a suspended
coroutine; a long-running message can change stacks and break the sequence.

If pursued, track service identity, stack identity, actual timestamps, and failure/gap
semantics. Test simultaneous services and sampling jitter. Streaming streak bookkeeping
does not require storing a full timeline.

### T1-5 GC-related sample share — deferred

Useful only when observation coverage and symbol availability support it.

Do not classify every `luaE_` function as GC. A GC-related symbol on a sampled stack is
not a precise measurement of time spent collecting, and skynet lacks a merged native chain.
Define the function set and denominator; unavailable or ambiguous coverage must remain
unknown rather than being reported as zero.

### T2-2 Idle / blocking classification — needs redesign

The original skynet `epoll_wait` proposal does not fit the current architecture:
skynet native stacks are intentionally not merged, and a worker's native snapshot cannot
be attributed directly to a particular service.

For single-VM targets, known blocking symbols may provide clues, but a `read` or `recv`
frame does not prove the call was blocked. Do not exclude such observations by default
before defining and validating the semantics; waiting may be precisely what users
are investigating.

### T2-3 `dump --locals` — deferred pending a concrete debugging need

Decode active locals from Proto debug information and frame registers, with vararg and
version-specific layout handling behind existing abstractions.

This introduces value decoding and additional remote reads. A running-process dump is
not a consistent value snapshot; invalid or torn values must display unknown.
Missing debug information must not be replaced with invented names.
Share value-formatting helpers with heap work only where their responsibilities match.

## LuaJIT (T3-2) — demand-driven, separate investigation

LuaJIT support should follow concrete target-user demand. Do not treat it as merely another
PUC Lua offset-table constructor.

First verify LuaJIT's own frame representation, GC64/build configurations, bytecode/debug
encoding, and JIT trace attribution. Its stacks cannot be designed from PUC Lua
`CallInfo` assumptions. Interpreter-only coverage and compiled-trace coverage must be
explicitly distinguished in output.

Choose an abstraction after a prototype demonstrates the boundary between the two
implementations. No one-to-two-week estimate or release commitment is retained.

## Non-goals for this roadmap

- Windows support.
- PUC Lua 5.1–5.3 compatibility work.
- DWARF source-line lookup for native C frames.
- Exact message latency from repeated stack samples.
- Heap allocation-site history reconstructed from an ordinary snapshot.
- Snapshot comparison before a useful, trustworthy single-snapshot report exists.
