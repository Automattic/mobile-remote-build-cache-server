#!/bin/bash
# Runs examples/loadgen against one server in Docker: the old Gradle node or
# this server. Both run as linux/amd64 containers (the old node ships only for
# amd64) on a fresh volume, with the load generator sharing the server's
# network namespace.
#
# Once, beforehand:
#   docker build --platform linux/amd64 -t apps-infra-mobile-build-cache-server:amd64 .
#   docker run --rm -v "$PWD":/src:ro -v bcs-target:/target -e CARGO_TARGET_DIR=/target -w /src \
#       rust:1.99.1-alpine cargo build --release --locked --example loadgen
#
# Then:
#   examples/compare-old-node.sh old-node 32 30 -- x
#   examples/compare-old-node.sh new 32 30 -- -e CACHE_MAX_SIZE=2GiB -e CACHE_MAX_ARTIFACT_SIZE=1000MiB \
#       -e CACHE_ANONYMOUS_ACCESS=readwrite apps-infra-mobile-build-cache-server:amd64
#
# Set PLATFORM=linux/arm64 to run this server natively on an ARM host.
#
# Arguments: <label> <connections> <seconds> -- <docker run arguments for the server, image last>
label="$1"; conns="$2"; secs="$3"; shift 4
docker rm -f cmp-server >/dev/null 2>&1; docker volume rm cmp-data >/dev/null 2>&1; docker volume create cmp-data >/dev/null
OG='gradle/build-cache-node:20.2@sha256:c4c111df759bfd6944f4b8064f5e275b8cfb382e03b5edfef218b79659e94576'
if [ "$label" = "old-node" ]; then
  docker run --rm -v cmp-data:/data alpine sh -c 'mkdir -p /data/conf && printf "version: 5\ncache:\n  accessControl:\n    anonymousLevel: \"readwrite\"\n  targetSize:\n    type: fixed\n    size: 2048\n  maxArtifactSize: 1000\n  freeSpaceBufferSize: 1024\nuiAccess:\n  type: \"disabled\"\n" > /data/conf/config.yaml'
  docker run -d --name cmp-server --platform linux/amd64 -v cmp-data:/data "$OG" start >/dev/null
else
  docker run --rm -v cmp-data:/data alpine chown 5000:5000 /data
  docker run -d --name cmp-server --platform "${PLATFORM:-linux/amd64}" -v cmp-data:/data "$@" >/dev/null
fi
for i in $(seq 120); do docker run --rm --network container:cmp-server alpine wget -q -O- http://127.0.0.1:5071/ping 2>/dev/null | grep -q SUCCESS && break; sleep 0.5; done
echo "### $label, $conns connections"
docker run --rm --network container:cmp-server -v bcs-target:/target:ro alpine /target/release/examples/loadgen 127.0.0.1:5071 --connections "$conns" --seconds "$secs"
docker run --rm --pid container:cmp-server alpine sh -c '
hwm=0; cpu=0
for s in /proc/[0-9]*/status; do p=${s%/status}; [ "$p" = "/proc/$$" ] && continue
  n=$(sed -n "s/^Name:[[:space:]]*//p" $s); case "$n" in sh|sed|awk) continue;; esac
  h=$(awk "/VmHWM/{print \$2}" $s); [ -n "$h" ] && hwm=$((hwm + h))
  c=$(awk "{print \$14 + \$15}" $p/stat 2>/dev/null); [ -n "$c" ] && cpu=$((cpu + c))
done
echo "server: peak RSS $((hwm / 1024)) MiB, CPU time $((cpu / 100)) s"'
docker run --rm -v cmp-data:/data alpine sh -c 'echo "data dir: $(du -sm /data | cut -f1) MiB"'
docker rm -f cmp-server >/dev/null 2>&1; docker volume rm cmp-data >/dev/null 2>&1
