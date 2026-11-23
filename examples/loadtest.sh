#!/usr/bin/env bash
# Runs the load test against a local release build, with eviction active
# throughout, and prints latency, throughput and the server's peak resident
# memory.
#
#   examples/loadtest.sh [fsync: true|false] [seconds] [connections]
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
fsync="${1:-true}"; seconds="${2:-30}"; connections="${3:-32}"
target="${CARGO_TARGET_DIR:-$repo/target}"
cargo build --release --locked --manifest-path "$repo/Cargo.toml" --example loadgen --bin apps-infra-mobile-build-cache-server
data="$(mktemp -d "${TMPDIR:-/tmp}/cache-load.XXXXXX")"
port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')"
CACHE_LISTEN="127.0.0.1:$port" CACHE_ADMIN_LISTEN="127.0.0.1:$((port + 1))" CACHE_DATA_DIR="$data" \
CACHE_MAX_SIZE=2GiB CACHE_MIN_FREE_SPACE=none CACHE_MAX_ARTIFACT_SIZE=1000MiB \
CACHE_ANONYMOUS_ACCESS=readwrite CACHE_FSYNC="$fsync" \
    "$target/release/apps-infra-mobile-build-cache-server" > "$data.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true; rm -rf "$data" "$data.log" "$data.rss"' EXIT
sleep 1
# Peak resident memory: the kernel's high-water mark on Linux, sampling elsewhere.
echo 0 > "$data.rss"
if [ ! -r "/proc/$pid/status" ]; then
    ( peak=0; while kill -0 $pid 2>/dev/null; do r=$(ps -o rss= -p $pid 2>/dev/null || echo 0); if [ "${r:-0}" -gt "$peak" ]; then peak=$r; echo "$peak" > "$data.rss"; fi; sleep 0.5; done ) &
fi
echo "fsync=$fsync"
"$target/release/examples/loadgen" "127.0.0.1:$port" --seconds "$seconds" --connections "$connections"
[ -r "/proc/$pid/status" ] && awk '/VmHWM/ { print $2 }' "/proc/$pid/status" > "$data.rss"
m() { curl -fsS "http://127.0.0.1:$((port + 1))/metrics" | awk -v m="apps_infra_mobile_build_cache_$1" '$1 == m { print $2 }'; }
echo "server: peak RSS $(( $(cat "$data.rss") / 1024 )) MiB, entries $(m entries), size $(( $(m size_bytes) / 1048576 )) MiB, evictions $(m 'evictions_total{reason="size"}'), last eviction pass $(m eviction_duration_seconds)s"
