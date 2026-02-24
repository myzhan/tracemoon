# tracemoon

A sampling profiler for Lua 5.4, inspired by [py-spy](https://github.com/benfred/py-spy). Attach to a running Lua process, capture mixed Lua + C call stacks, and generate interactive flamegraph SVGs.

## Features

- **Zero instrumentation** - attaches to running Lua processes without modification
- **Mixed stacks** - captures both Lua and native C call stacks, merged together
- **Symbol resolution** - resolves C function names from Mach-O/ELF symbol tables
- **Lua debug info** - extracts source files, line numbers, and function names
- **Flamegraph SVG** - generates interactive flamegraphs directly (via [inferno](https://github.com/jonhoo/inferno))
- **Cross-platform** - supports Linux and macOS (x86_64 / ARM64)

## Quick Start

```bash
cargo build --release

# Start a Lua process
lua tests/lua/multi_func.lua &
PID=$!

# Profile for 10 seconds, generate flamegraph
sudo ./target/release/tracemoon record -p $PID -d 10

# Open flamegraph.svg in a browser
open flamegraph.svg
```

## Usage

```
tracemoon record [OPTIONS]

OPTIONS:
  -p, --pid <PID>          PID of a running Lua process (required)
  -r, --rate <HZ>          Sample rate in Hz (default: 100)
  -d, --duration <SECS>    Duration in seconds (0 = until Ctrl+C, default: 0)
  -o, --output <FILE>      Output file (default: flamegraph.svg)
```

Output format is determined by file extension:
- `.svg` - interactive flamegraph (default)
- `.txt` - collapsed stacks, compatible with [flamegraph.pl](https://github.com/brendangregg/FlameGraph)

### Examples

```bash
# Default: flamegraph SVG
sudo tracemoon record -p 12345 -d 5

# Collapsed stacks for external tools
sudo tracemoon record -p 12345 -d 5 -o profile.txt

# Higher sample rate, custom output
sudo tracemoon record -p 12345 -r 200 -d 10 -o flame.svg
```

## Sample Output

```
Sampling statistics:
  Duration: 5.0s
  Samples collected: 288
  Successful samples: 288
  Unique stacks: 4
  Success rate: 100.0%

  Top stacks:
    [33.1%] (95 samples)
      ├─ [C]:pmain
      ├─ tests/lua/multi_func.lua:27:main
      ├─ tests/lua/multi_func.lua:22:process_data
      ├─ tests/lua/multi_func.lua:15:compute_heavy
      ├─ tests/lua/multi_func.lua:7
      ├─ [C]:math_sin
      ├─ [C]:luaV_execute
      ├─ ...
      └─ [C]:main
```

## Requirements

- Rust 1.70+
- Target: Lua 5.4.x processes
- **Linux:** root or `CAP_SYS_PTRACE` capability
- **macOS:** root (`sudo`) required for `task_for_pid`

## How It Works

1. **Attach** to a running Lua process via platform APIs (mach ports on macOS, ptrace on Linux)
2. **Discover** the `lua_State` address by scanning process memory for valid patterns
3. **Sample** at configurable frequency (default 100 Hz):
   - Walk the Lua `CallInfo` chain for Lua-level stacks
   - Walk native frame pointers for C-level stacks
   - Merge both into a unified call stack
4. **Resolve** symbols:
   - Lua: read `Proto.source` (TString) for filenames, `lineinfo` for line numbers, upvalue names for function names
   - C: parse Mach-O/ELF symbol tables from mapped binaries
5. **Output** aggregated stacks as flamegraph SVG or collapsed text

## Debug Logging

```bash
sudo RUST_LOG=debug ./target/debug/tracemoon record -p 12345 -d 2
```

## Building

```bash
cargo build           # debug
cargo build --release # optimized
cargo test            # run tests
```

## License

MIT
