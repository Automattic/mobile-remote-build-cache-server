#!/usr/bin/env bash
# Real-Gradle regression test for the build cache server.
#
#   tests/gradle-project/run.sh [gradle-executable ...]
#
# With no arguments it uses `gradle` from PATH. Pass one executable per Gradle
# version that production builds use. For each one, against a fresh server:
#
#   1. A push build (CI credentials) stores four task outputs.
#   2. A build in a second checkout with a second Gradle home (read-only
#      developer credentials) must get all four FROM-CACHE.
#   3. A task output larger than the server's limit is pushed, without and
#      with useExpectContinue. The build must not report the remote cache as
#      disabled, and an output stored after the oversized one must be served.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
gradles=("$@")
[ ${#gradles[@]} -eq 0 ] && gradles=(gradle)

cargo build --release --locked --manifest-path "$repo/Cargo.toml"
server="${CARGO_TARGET_DIR:-$repo/target}/release/apps-infra-mobile-build-cache-server"

work="$(mktemp -d "${TMPDIR:-/tmp}/cache-gradle-test.XXXXXX")"
server_pid=""
cleanup() {
    [ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
metric() { curl -fsS "http://127.0.0.1:$admin_port/metrics" | awk -v m="apps_infra_mobile_build_cache_$1" '$1 == m { print $2 }'; }
fail() { echo "FAIL: $*" >&2; exit 1; }

DISABLED="The remote build cache was disabled during the build due to errors"

# gradle_run <checkout> <gradle home> <log> <task>... ; reads REMOTE_CACHE_* from the environment.
gradle_run() {
    local checkout="$1" home="$2" log="$3"; shift 3
    rm -rf "$checkout"; mkdir -p "$checkout"
    cp "$here/settings.gradle.kts" "$here/build.gradle.kts" "$checkout/"
    (cd "$checkout" && GRADLE_USER_HOME="$home" "$gradle" --no-daemon --build-cache --console=plain \
        -Pvariant="$variant" "$@") >"$log" 2>&1 || { cat "$log"; fail "gradle exited with an error ($log)"; }
    if grep -q "$DISABLED" "$log"; then cat "$log"; fail "remote cache was disabled ($log)"; fi
    if grep -qE "Could not (load|store) entry" "$log"; then cat "$log"; fail "cache request failed ($log)"; fi
}
expect_from_cache() { grep -qE "^> Task :$2 FROM-CACHE" "$1" || { cat "$1"; fail "$2 was not FROM-CACHE"; }; }
expect_executed() { grep -qE "^> Task :$2\$" "$1" || { cat "$1"; fail "$2 should have been executed"; }; }

for gradle in "${gradles[@]}"; do
    version="$("$gradle" --version | awk '/^Gradle /{print $2}')"
    echo "=== Gradle $version ($gradle)"
    run="$work/$version"; mkdir -p "$run/data"
    variant="$version-$$"

    # A fresh server per Gradle version, with users made by `hash-password`.
    printf 'ci-password' | "$server" hash-password ci-user readwrite > "$run/users"
    printf 'dev-password' | "$server" hash-password developer read >> "$run/users"
    port="$(free_port)"; admin_port="$(free_port)"
    CACHE_LISTEN="127.0.0.1:$port" CACHE_ADMIN_LISTEN="127.0.0.1:$admin_port" \
    CACHE_DATA_DIR="$run/data" CACHE_USERS_FILE="$run/users" \
    CACHE_MAX_SIZE=1GiB CACHE_MAX_ARTIFACT_SIZE=1MiB CACHE_ACCESS_LOG=true \
        "$server" > "$run/server.log" 2>&1 &
    server_pid=$!
    for _ in $(seq 100); do curl -fsS "http://127.0.0.1:$port/health" >/dev/null 2>&1 && break; sleep 0.1; done
    curl -fsS "http://127.0.0.1:$port/health" >/dev/null || { cat "$run/server.log"; fail "server did not start"; }
    export REMOTE_CACHE_URL="http://127.0.0.1:$port/cache/"

    # 1. Push build as CI.
    export REMOTE_CACHE_USER=ci-user REMOTE_CACHE_PASSWORD=ci-password REMOTE_CACHE_PUSH=true REMOTE_CACHE_EXPECT_CONTINUE=false
    gradle_run "$run/checkout-a" "$run/home-a" "$run/push.log" generateAll
    for t in generateA generateB generateC generateD; do expect_executed "$run/push.log" "$t"; done

    # 2. Pull build as a developer: other checkout, other Gradle home, read-only.
    export REMOTE_CACHE_USER=developer REMOTE_CACHE_PASSWORD=dev-password REMOTE_CACHE_PUSH=false
    gradle_run "$run/checkout-b" "$run/home-b" "$run/pull.log" generateAll
    for t in generateA generateB generateC generateD; do expect_from_cache "$run/pull.log" "$t"; done
    echo "    push then pull: 4 tasks FROM-CACHE"

    # 3. Oversized output, without and with Expect: 100-continue.
    for expect in false true; do
        variant="$version-$$-expect-$expect"
        before="$(metric 'rejected_uploads_total{reason="too_large"}')"
        export REMOTE_CACHE_USER=ci-user REMOTE_CACHE_PASSWORD=ci-password REMOTE_CACHE_PUSH=true REMOTE_CACHE_EXPECT_CONTINUE=$expect
        gradle_run "$run/checkout-a" "$run/home-a" "$run/big-$expect.log" generateBig generateAfterBig
        after="$(metric 'rejected_uploads_total{reason="too_large"}')"
        [ "$after" -eq $((before + 1)) ] || fail "expected one soft reject (expect=$expect), counter went $before -> $after"

        export REMOTE_CACHE_USER=developer REMOTE_CACHE_PASSWORD=dev-password REMOTE_CACHE_PUSH=false REMOTE_CACHE_EXPECT_CONTINUE=false
        gradle_run "$run/checkout-b" "$run/home-b" "$run/big-$expect-pull.log" generateBig generateAfterBig
        expect_executed "$run/big-$expect-pull.log" generateBig
        expect_from_cache "$run/big-$expect-pull.log" generateAfterBig
        echo "    oversized upload (useExpectContinue=$expect): soft-rejected, cache stayed enabled"
    done

    errors="$(grep -c '"level":"error"' "$run/server.log" || true)"
    [ "$errors" -eq 0 ] || { grep '"level":"error"' "$run/server.log"; fail "server logged errors"; }
    statuses="$(grep '"msg":"request"' "$run/server.log" | sed -E 's/.*"method":"([A-Z]+)".*"status":([0-9]+).*/\1 \2/' | sort | uniq -c | tr -s ' ' | tr '\n' ';')"
    echo "    server saw:$statuses"
    if grep '"msg":"request"' "$run/server.log" | grep -qvE '"status":(200|404)'; then
        fail "the server answered a Gradle request with something other than 200 or 404"
    fi
    kill "$server_pid"; wait "$server_pid" 2>/dev/null || true; server_pid=""
done
echo "PASS"
