#!/usr/bin/env bash
# Build pinned upstream sources out of tree. No downloads or submodule edits here.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
build_root="$project_root/target/test-vendor"
mkdir -p "$build_root"

case "$(uname -s)" in
  Linux) platform=linux; lua_cflags=-DLUA_USE_LINUX; lua_ldflags=-Wl,-E; lua_libs=-ldl ;;
  Darwin) platform=macosx; lua_cflags=-DLUA_USE_MACOSX; lua_ldflags=; lua_libs= ;;
  *) echo "Unsupported test vendor platform" >&2; exit 1 ;;
esac

# Lua ships `makefile` (lowercase), skynet ships `Makefile`. macOS is case-insensitive and
# Linux is not, so both names have to be tested explicitly.
has_makefile() {
  test -f "$1/Makefile" || test -f "$1/makefile"
}

archive_source() {
  local name="$1" vendor="$project_root/tests/vendor/$1" revision destination
  test -f "$vendor/lua.h" || has_makefile "$vendor" || {
    echo "Missing $name submodule. Run: git submodule update --init tests/vendor/lua tests/vendor/skynet" >&2
    exit 1
  }
  revision="$(git -C "$vendor" rev-parse HEAD)"
  destination="$build_root/$name-$revision-$platform"
  # A restored CI cache can leave an empty or half-populated directory behind, so only a
  # tree that still has its makefile counts as an extracted vendor.
  if ! has_makefile "$destination"; then
    local staging
    staging="$(mktemp -d "$build_root/$name.XXXXXX")"
    git -C "$vendor" archive HEAD | tar -xf - -C "$staging"
    has_makefile "$staging" || {
      echo "Empty $name vendor archive for $revision" >&2
      exit 1
    }
    rm -rf -- "$destination"
    mv "$staging" "$destination"
  fi
  printf '%s\n' "$destination"
}

lua_build="$(archive_source lua)"
skynet_build="$(archive_source skynet)"
jobs="${TRACEMOON_BUILD_JOBS:-2}"

# The GitHub Lua repository has a developer makefile, not the release tarball's layout.
# Override its readline/native-CPU defaults while retaining debug symbols for profiling.
make -C "$lua_build" -j"$jobs" lua CC=cc \
  "CFLAGS=-O2 -g -Wall $lua_cflags" "MYLDFLAGS=$lua_ldflags" "MYLIBS=$lua_libs"

# jemalloc is irrelevant to Lua object traversal. Disabling it also avoids a third
# recursive submodule and makes the fixture deterministic across Linux and macOS.
make -C "$skynet_build" -j"$jobs" "$platform" CC=cc \
  MALLOC_STATICLIB= SKYNET_DEFINES=-DNOUSE_JEMALLOC

cc -std=c11 -I"$lua_build" "$project_root/tests/c/lua54_heap_layout.c" -o "$build_root/heap-layout"
"$build_root/heap-layout"
cc -I"$lua_build" "$project_root/tests/c/heap_fixture.c" "$lua_build/liblua.a" -lm -o "$build_root/heap-fixture"

# Check the embedded Lua ABI independently as well; skynet can carry local patches.
cc -std=c11 -DTRACEMOON_SKYNET_LUA -I"$skynet_build/3rd/lua" "$project_root/tests/c/lua54_heap_layout.c" -o "$build_root/skynet-heap-layout"
"$build_root/skynet-heap-layout"
cc -I"$skynet_build/3rd/lua" "$project_root/tests/c/heap_fixture.c" "$skynet_build/3rd/lua/liblua.a" -lm -o "$build_root/heap-fixture-skynet"

cc -shared -fPIC "$project_root/tests/c/core_trigger.c" -o "$build_root/core-trigger.so"

printf '%s\n' "$lua_build" > "$build_root/lua.path"
printf '%s\n' "$skynet_build" > "$build_root/skynet.path"
