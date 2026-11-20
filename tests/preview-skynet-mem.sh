#!/usr/bin/env bash
# A real-service, cooperative report preview on macOS; not external-capture acceptance.
set -euo pipefail
project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"
bash tests/build-vendor.sh
skynet_build="$(cat target/test-vendor/skynet.path)"
mkdir -p target/reports
export TRACEMOON_EXPORT_DIR
TRACEMOON_EXPORT_DIR="$(mktemp -d "$project_root/target/reports/skynet-mem.XXXXXX")"
export TRACEMOON_EXPORT_MODULE="$project_root/target/test-vendor/skynet-heap-export.so"
export TRACEMOON_FIXTURE_DIR="$project_root/tests/fixtures/skynet"
case "$(uname -s)" in
  Darwin) shared=(-dynamiclib -Wl,-undefined,dynamic_lookup) ;;
  Linux) shared=(-shared) ;;
  *) exit 1 ;;
esac
cc -std=gnu11 -O2 -g -fPIC "${shared[@]}" -I"$skynet_build/3rd/lua" \
  tests/c/skynet_heap_export.c -o "$TRACEMOON_EXPORT_MODULE"

target_pid=
cleanup() {
  if test -n "$target_pid"; then
    kill -KILL "$target_pid" 2>/dev/null || true
    wait "$target_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT
(
  cd "$skynet_build"
  exec ./skynet "$TRACEMOON_FIXTURE_DIR/config"
) > "$TRACEMOON_EXPORT_DIR/skynet.log" 2>&1 &
target_pid=$!
ready=false
for attempt in {1..100}; do
  if grep -q 'TRACEMOON_EXPORTED:alpha' "$TRACEMOON_EXPORT_DIR/skynet.log" && \
     grep -q 'TRACEMOON_EXPORTED:beta' "$TRACEMOON_EXPORT_DIR/skynet.log"; then
    ready=true
    break
  fi
  kill -0 "$target_pid" 2>/dev/null || break
  sleep 0.1
done
if test "$ready" != true; then
  cat "$TRACEMOON_EXPORT_DIR/skynet.log" >&2
  exit 1
fi
cleanup
target_pid=
cargo test --bin tracemoon memory::tests::write_cooperative_skynet_preview_snapshot -- --ignored --exact --nocapture
cargo run --quiet -- mem report "$TRACEMOON_EXPORT_DIR/heap.tmoon" --top 20 -o "$TRACEMOON_EXPORT_DIR/report.txt"
cargo run --quiet -- mem report "$TRACEMOON_EXPORT_DIR/heap.tmoon" -o "$TRACEMOON_EXPORT_DIR/report.html"
awk '/^Service\(s\):/ { exit } { print }' "$TRACEMOON_EXPORT_DIR/report.txt" > "$TRACEMOON_EXPORT_DIR/business-report.txt"
while read -r service address; do
  cargo run --quiet -- mem inspect "$TRACEMOON_EXPORT_DIR/heap.tmoon" --object "$address" > "$TRACEMOON_EXPORT_DIR/$service-sessions.txt"
  awk -v service="$service" '/^Service\(s\):/ { show = index($0, "Service(s): " service " ") == 1 } show { print }' \
    "$TRACEMOON_EXPORT_DIR/$service-sessions.txt" >> "$TRACEMOON_EXPORT_DIR/business-report.txt"
done < "$TRACEMOON_EXPORT_DIR/sessions.index"
printf 'Report: %s/report.txt\nSnapshot: %s/heap.tmoon\n' "$TRACEMOON_EXPORT_DIR" "$TRACEMOON_EXPORT_DIR"
printf 'Business objects: %s/business-report.txt\n' "$TRACEMOON_EXPORT_DIR"
printf 'HTML: %s/report.html\n' "$TRACEMOON_EXPORT_DIR"
