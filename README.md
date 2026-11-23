# apps-infra-mobile-build-cache-server

A small Gradle remote build cache server. It replaces `gradle/build-cache-node` as a drop-in: same URL layout, same on-disk format, same usernames and passwords.

- Stores each entry as one file on local disk and streams bodies in both directions.
- HTTP Basic authentication with `read` and `readwrite` roles, argon2id password hashes.
- Evicts by age since last use, by total size, and by free disk space.
- `/health`, `/ping`, and Prometheus metrics on a separate listener.
- No TLS, no UI, no clustering. A load balancer terminates TLS.

## Quick start

```sh
docker build -t apps-infra-mobile-build-cache-server .

# One line per user: username:role:argon2id-hash. The password is read from stdin.
printf '%s' "$CI_PASSWORD"  | docker run --rm -i apps-infra-mobile-build-cache-server hash-password ci-user readwrite >  users
printf '%s' "$DEV_PASSWORD" | docker run --rm -i apps-infra-mobile-build-cache-server hash-password developer read     >> users

docker run -d --name build-cache --restart always \
  --read-only --ulimit nofile=65536:65536 \
  -p 5071:5071 -p 9090:9090 \
  -v /opt/build-cache-node/system/cache/artifacts-v2:/data \
  -v "$PWD/users:/users:ro" \
  -e CACHE_USERS_FILE=/users \
  -e CACHE_MAX_SIZE=90GiB -e CACHE_MIN_FREE_SPACE=8GiB \
  -e CACHE_MAX_ARTIFACT_SIZE=1000MiB \
  apps-infra-mobile-build-cache-server
```

The image is `scratch` plus one static binary (about 1 MB) and runs as `5000:5000`. The data directory must be writable by that user. `--ulimit nofile` covers connections plus open files; the default cap is 1024 connections.

Clients need no change. For reference:

```kotlin
// settings.gradle.kts
buildCache {
    remote<HttpBuildCache> {
        url = uri("https://cache.example.internal/cache/")
        isPush = System.getenv("CI") != null
        credentials {
            username = providers.gradleProperty("cacheUser").orNull
            password = providers.gradleProperty("cachePassword").orNull
        }
    }
}
```

## Users and roles

| Role | May |
|---|---|
| `read` | `GET`, `HEAD` |
| `readwrite` | `GET`, `HEAD`, `PUT` |

- **Write access is the trust boundary.** Anyone who can write can replace build outputs for everyone who reads. Give `readwrite` to CI only and `read` to developer machines.
- **A `read` user must not push.** A `PUT` from a `read` user gets `403`, and Gradle then disables the remote cache for the rest of that build. Set `isPush = false` for read-only users.
- The users file is read at startup and again on `SIGHUP` (`docker kill -s HUP build-cache`). A reload drops every remembered login, so removed users and changed passwords stop working at once. A file that fails to parse is refused and the current users stay.
- `CACHE_ANONYMOUS_ACCESS` (`none`, `read`, `readwrite`) sets what a request without credentials may do. A request that does send credentials must send correct ones, even if anonymous access would have been enough.
- Failed logins answer `401` with `WWW-Authenticate: Basic realm="cache"`.

How the password check is protected, since the server is internet-facing:

- A correct login is verified with argon2 once, then remembered for `CACHE_AUTH_CACHE_TTL` under a keyed hash whose key exists only in memory. Remembered logins skip everything below.
- At most `CACHE_AUTH_CONCURRENCY` argon2 computations run at a time. Others wait up to `CACHE_AUTH_WAIT_TIMEOUT`, then get `503`.
- After `CACHE_AUTH_FAILURE_LIMIT` failures from one client address, further attempts from it get `429` without running argon2 until the window ends. This also applies to a correct password from that address if it is not yet remembered.
- The client address is the TCP peer, unless the peer is in `CACHE_TRUSTED_PROXIES`; then it is the last `X-Forwarded-For` entry. Set this to the load balancer's subnets, otherwise every internet client shares the load balancer's address for throttling.
- An unknown username costs one argon2 computation, the same as a wrong password.

`hash-password` uses argon2id with 19 MiB of memory, 2 iterations, 1 lane. One verification took 17 ms on an Apple M4 Pro. It has not been timed on the production `t3.medium` yet; see [Open items](#open-items).

## HTTP API

Cache listener (`CACHE_LISTEN`, default port 5071):

| Request | Access | Response |
|---|---|---|
| `GET /cache/{key}` | read | `200` with the entry, or `404` |
| `HEAD /cache/{key}` | read | Same, without the body |
| `PUT /cache/{key}` | readwrite | `200` when stored; also `200` when discarded (below) |
| `GET /ping` | none | `200`, body `SUCCESS` |
| `GET /health`, `GET /cache-node-info/health` | none | `200` when the index is loaded and the data directory is writable, else `503` |

Admin listener (`CACHE_ADMIN_LISTEN`, default port 9090): `GET /metrics` only. Keep it off the load balancer and restrict it with security groups.

Keys must match `^[0-9a-f]{32,128}$`; anything else is `400`. Every cache response carries `X-Max-Entry-Size-Bytes`.

**Uploads that are not kept still get `200`.** For Gradle, any status other than `2xx` or `404` disables the remote cache for the rest of the build. So an upload over `CACHE_MAX_ARTIFACT_SIZE`, or one that cannot fit on the volume, is read and dropped and answered `200`, as the old node does. With `Expect: 100-continue` the `200` is sent before the body. Each one is counted in `rejected_uploads_total` and logged with key and size, because the client cannot tell. Only an upload past `CACHE_DISCARD_CEILING` gets `413` and a closed connection.

A response sent without reading the request body (a failed login on a large `PUT`, for example) carries `Connection: close`.

## Configuration

Environment variables only. An invalid value or an unknown `CACHE_*` variable stops startup with a message naming it.

| Variable | Default | Meaning |
|---|---|---|
| `CACHE_LISTEN` | `0.0.0.0:5071` | Cache listener address |
| `CACHE_ADMIN_LISTEN` | `0.0.0.0:9090` | Metrics listener address; set to empty to disable |
| `CACHE_DATA_DIR` | `/data` | Storage root |
| `CACHE_MAX_SIZE` | none | Total size limit, for example `90GiB` |
| `CACHE_MAX_AGE` | none | Evict entries unused for this long, for example `30d` |
| `CACHE_MIN_FREE_SPACE` | `1GiB` | Free space to keep on the volume: a size, a percentage such as `10%`, or `none` |
| `CACHE_MAX_ARTIFACT_SIZE` | `100MiB` | Largest upload that is stored |
| `CACHE_DISCARD_CEILING` | 4 × artifact size | Largest rejected upload that is still read and answered `200` |
| `CACHE_EVICTION_INTERVAL` | `60s` | Time between background eviction passes |
| `CACHE_EVICTION_LOW_WATERMARK` | `98%` | Size eviction stops at this fraction of `CACHE_MAX_SIZE` |
| `CACHE_FSYNC` | `true` | Sync uploads to disk before publishing them |
| `CACHE_TOUCH_INTERVAL` | `1h` | Minimum time between writing an entry's last-use time to disk |
| `CACHE_USERS_FILE` | none | Path to the users file |
| `CACHE_ANONYMOUS_ACCESS` | `none` | `none`, `read`, or `readwrite` |
| `CACHE_AUTH_CONCURRENCY` | `2` | Concurrent argon2 verifications |
| `CACHE_AUTH_WAIT_TIMEOUT` | `5s` | Longest wait for a verification slot before `503` |
| `CACHE_AUTH_FAILURE_LIMIT` | `10/1m` | Failed logins per client address before `429` |
| `CACHE_AUTH_CACHE_TTL` | `1h` | How long a verified login is remembered |
| `CACHE_TRUSTED_PROXIES` | none | CIDRs (comma-separated) whose last `X-Forwarded-For` entry is taken as the client address |
| `CACHE_MAX_CONNECTIONS` | `1024` | Concurrent connections; more wait in the accept backlog |
| `CACHE_HEADER_TIMEOUT` | `10s` | Time allowed to receive a request head |
| `CACHE_IDLE_TIMEOUT` | `75s` | Longest pause in a transfer or between requests. Keep it above the load balancer's idle timeout (60 s in production), or clients see sporadic `502` |
| `CACHE_SHUTDOWN_TIMEOUT` | `30s` | Grace period for requests in flight on `SIGTERM` |
| `CACHE_ACCESS_LOG` | `false` | One log line per request |

Sizes take `B`, `KiB`, `MiB`, `GiB`, `TiB` (also `K`, `M`, `G`, `T`; `KB`/`MB`/`GB` are powers of 1000). Durations need a unit: `ms`, `s`, `m`, `h`, `d`. At least one of `CACHE_MAX_SIZE`, `CACHE_MAX_AGE`, `CACHE_MIN_FREE_SPACE` must be set.

Production values matching the old node:

```
CACHE_DATA_DIR=/opt/build-cache-node/system/cache/artifacts-v2   (or bind-mount it to /data)
CACHE_MAX_SIZE=90GiB
CACHE_MIN_FREE_SPACE=8GiB
CACHE_MAX_ARTIFACT_SIZE=1000MiB
CACHE_TRUSTED_PROXIES=<load balancer subnet CIDRs>
CACHE_USERS_FILE=<mounted secret with ci-user (readwrite) and developer (read)>
```

## Storage and eviction

```
{CACHE_DATA_DIR}/ab/abcdef….artifact   one file per entry: the uploaded bytes, nothing else
{CACHE_DATA_DIR}/.tmp/                 uploads in progress; emptied at startup
```

This is the old node's `artifacts-v2` layout, so the server can be pointed at the existing volume.

- **Uploads** go to a temp file, are synced, then renamed into place. A crash or a dropped connection never leaves a partial entry visible.
- **Last use** is kept in memory and written to the file's modification time at most once per `CACHE_TOUCH_INTERVAL`. At startup the index is rebuilt from file sizes and modification times. Entries from the old node carry their store time, so the first eviction passes after migration go by age rather than use.
- **Index memory** is roughly 100 bytes per entry. With 181,000 entries the whole process used 23 MiB on Linux.
- **During the startup scan** requests are served from disk and uploads are accepted; only eviction waits, and `/health` answers `503` until the scan ends. Scanning 181,000 files took 0.3 s on a warm page cache.
- **After the scan** a key that is not in the index is a miss without touching the disk. A file copied into the data directory by hand is not served until the next restart.
- **Stray files** (names that are not valid keys, files in the wrong shard directory) are never served and never deleted. They are counted in the `invalid_files` metric.
- **Eviction** runs every `CACHE_EVICTION_INTERVAL` and immediately when an upload takes the cache over `CACHE_MAX_SIZE`. Each pass removes entries unused for longer than `CACHE_MAX_AGE`, then least recently used entries down to the low watermark, then least recently used entries until `CACHE_MIN_FREE_SPACE` is restored.
- If uploads outrun eviction and the volume fills, the affected uploads are discarded with `200` (`rejected_uploads_total{reason="no_space"}`).

## Observability

Logs are one JSON object per line on stdout. Startup logs the effective configuration. Evictions, errors and discarded uploads are always logged; per-request lines need `CACHE_ACCESS_LOG=true`. Credentials are never logged.

Metrics, all prefixed `apps_infra_mobile_build_cache_`:

| Metric | Type | Labels |
|---|---|---|
| `requests_total` | counter | `method`, `status` |
| `request_duration_seconds` | histogram | `method` |
| `hits_total`, `misses_total` | counter | |
| `bytes_received_total`, `bytes_sent_total` | counter | |
| `entries`, `size_bytes`, `max_size_bytes` | gauge | |
| `disk_free_bytes`, `disk_total_bytes` | gauge | |
| `evictions_total`, `evicted_bytes_total` | counter | `reason` = `age`, `size`, `free_space` |
| `eviction_errors_total` | counter | |
| `eviction_duration_seconds` | gauge (last pass) | |
| `oldest_entry_last_used_seconds` | gauge (Unix time) | |
| `rejected_uploads_total` | counter | `reason` = `too_large`, `no_space` |
| `auth_failures_total`, `auth_verifications_total`, `auth_throttled_total`, `auth_busy_total` | counter | |
| `connections_active` | gauge | |
| `startup_scan_duration_seconds`, `invalid_files`, `healthy` | gauge | |
| `build_info` | gauge | `version` |

`request_duration_seconds` runs from the request head to the response head, so it includes the body for uploads but not for downloads. `disk_*` and `healthy` are refreshed by each eviction pass.

Scrape config:

```yaml
scrape_configs:
  - job_name: gradle-build-cache
    static_configs:
      - targets: ["10.0.2.214:9090"]
```

Alert rules:

```yaml
groups:
  - name: gradle-build-cache
    rules:
      - alert: BuildCacheDown
        expr: up{job="gradle-build-cache"} == 0 or apps_infra_mobile_build_cache_healthy == 0
        for: 5m
      - alert: BuildCacheDiskNearlyFull
        expr: apps_infra_mobile_build_cache_disk_free_bytes / apps_infra_mobile_build_cache_disk_total_bytes < 0.03
        for: 10m
      - alert: BuildCacheHitRateCollapsed
        # Normal is about 50 %.
        expr: |
          rate(apps_infra_mobile_build_cache_hits_total[1h])
            / (rate(apps_infra_mobile_build_cache_hits_total[1h]) + rate(apps_infra_mobile_build_cache_misses_total[1h])) < 0.2
          and rate(apps_infra_mobile_build_cache_misses_total[1h]) > 0.1
        for: 1h
      - alert: BuildCacheEvictionChurn
        # Entries are being evicted within a day of their last use: the cache is too small.
        expr: time() - apps_infra_mobile_build_cache_oldest_entry_last_used_seconds < 86400
          and apps_infra_mobile_build_cache_entries > 0
        for: 2h
      - alert: BuildCacheUploadsDiscarded
        expr: increase(apps_infra_mobile_build_cache_rejected_uploads_total[1h]) > 0
      - alert: BuildCacheDisablingClientBuilds
        # Any of these statuses switches off the remote cache for the build that receives it.
        expr: sum(rate(apps_infra_mobile_build_cache_requests_total{status=~"429|500|503"}[10m])) > 0
        for: 10m
      - alert: BuildCachePasswordGuessing
        expr: rate(apps_infra_mobile_build_cache_auth_failures_total[10m]) > 1
        for: 10m
      - alert: BuildCacheEvictionFailing
        expr: increase(apps_infra_mobile_build_cache_eviction_errors_total[1h]) > 0
```

## Performance

All figures are from 2026-11-23 on a laptop (Apple M4 Pro, Docker Linux VM), not on the production instance type. `gradle/build-cache-node:20.2` (the production digest) exists only for amd64, so it ran emulated; this server was measured both emulated (amd64) and native (arm64). Each figure is one run. Read them as rough ratios: the JVM pays more for emulation and warm-up than a native binary does, and whether the old node syncs uploads to disk is not known (its upload speeds suggest it does not).

### Mixed load

`examples/compare-old-node.sh` runs `examples/loadgen` for 30 seconds: 90 % `GET` (about half misses) and 10 % `PUT`, with entry sizes drawn from the production distribution (median 2 KB, p90 309 KB, p99 17.7 MB, up to 152.7 MB), against a 2 GiB cache so eviction runs throughout.

| 32 connections | Throughput | `GET` hit p50 / p99 | `GET` miss p50 / p99 | `PUT` p50 / p99 | Peak memory | Data dir at end |
|---|---|---|---|---|---|---|
| Old node | 2,119 req/s | 2.5 / 160 ms | 1.1 / 88 ms | 21 / 760 ms | 1,264 MiB | 5.9 GiB |
| This server, amd64, fsync on | 10,711 req/s | 0.95 / 61 ms | 0.44 / 2.3 ms | 7.6 / 188 ms | 23 MiB | 2.0 GiB |
| This server, amd64, fsync off | 9,000 req/s | 1.6 / 89 ms | 0.97 / 3.9 ms | 2.0 / 216 ms | 23 MiB | 2.0 GiB |
| This server, arm64, fsync on | 11,798 req/s | 0.61 / 114 ms | 0.17 / 1.6 ms | 6.2 / 141 ms | 19 MiB | 2.0 GiB |
| This server, arm64, fsync off | 9,035 req/s | 0.73 / 186 ms | 0.16 / 1.9 ms | 0.97 / 140 ms | 20 MiB | 2.0 GiB |

| 2 connections | Throughput | `GET` hit p50 / p99 | `GET` miss p50 / p99 | `PUT` p50 / p99 | Peak memory | Server CPU time |
|---|---|---|---|---|---|---|
| Old node | 3,011 req/s | 0.24 / 3.7 ms | 0.11 / 1.0 ms | 1.1 / 44 ms | 724 MiB | 75 s |
| This server, amd64, fsync on | 3,920 req/s | 0.09 / 7.6 ms | 0.03 / 0.14 ms | 2.0 / 29 ms | 23 MiB | 28 s |
| This server, amd64, fsync off | 7,064 req/s | 0.12 / 10 ms | 0.06 / 0.16 ms | 0.15 / 11 ms | 23 MiB | 50 s |
| This server, arm64, fsync on | 4,869 req/s | 0.07 / 6.8 ms | 0.03 / 0.11 ms | 1.4 / 23 ms | 20 MiB | 27 s |
| This server, arm64, fsync off | 8,413 req/s | 0.10 / 8.7 ms | 0.05 / 0.12 ms | 0.12 / 10 ms | 20 MiB | 46 s |

Under 32 connections the old node let its data directory grow to 5.9 GiB against a 2 GiB target; this server held the limit. Production peaks at about 35 requests per second, so all of this is headroom.

### One transfer at a time, by entry size

Median time for a single `PUT` and a single `GET` over loopback, file in the page cache:

| Entry size | Old node `PUT` / `GET` | This server (arm64, fsync on) | This server (arm64, fsync off) |
|---|---|---|---|
| 2 KB | 4.2 / 3.0 ms | 0.80 / 0.05 ms | 0.22 / 0.10 ms |
| 309 KB | 6.4 / 2.4 ms | 1.0 / 0.15 ms | 0.46 / 0.17 ms |
| 2 MB | 5.5 / 1.7 ms | 2.4 / 0.85 ms | 1.5 / 0.86 ms |
| 17.7 MB | 8.4 / 2.4 ms | 17 / 8.5 ms | 11 / 9.4 ms |
| 152.7 MB | 35 / 12 ms | 126 / 67 ms | 81 / 66 ms |

- Up to a few megabytes, which covers more than 90 % of production entries, this server is several times faster.
- For large entries the old node is faster on a single transfer over loopback: it downloads at 7–13 GB/s and uploads at 2–4 GB/s, against about 2.3 GB/s and 1.2–1.9 GB/s here. This is also why this server's `GET` hit p99 is higher than the old node's at 2 connections.
- That gap is not expected to show in production, because the hardware is slower than either server: a `t3.medium` network tops out near 600 MB/s and a default gp3 volume at 125 MB/s.
- A faster streaming path (read-ahead, reused buffers, 1 MiB chunks, vectored upload writes) was built and measured at about 7 GB/s for downloads. It was removed again as complexity the use case does not need.

## Migration from the old node

1. Snapshot the data volume. It is the rollback path: the old node's database goes stale once this server adds or evicts files.
2. Build the users file with `hash-password` from the existing passwords.
3. Stop the old container. Start this one as UID 5000 on port 5071 with the `artifacts-v2` directory as the data directory and the production values above.
4. The load balancer health check (`GET /cache-node-info/health`) needs no change.
5. Watch the hit rate, `invalid_files`, and the eviction metrics. Once settled, delete the old node's `db/`, `tmp/`, `upload/`, `conf/` and `logs/` directories.

## Development

```sh
cargo test --locked                      # unit and integration tests (84)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
tests/gradle-project/run.sh [gradle…]    # real Gradle: push, pull from a second checkout, oversized outputs
examples/loadtest.sh [true|false]        # load test, fsync on or off
cargo deny check && cargo audit          # supply chain; configuration in deny.toml
```

- The integration tests start the server in-process on a temp directory and talk to it over raw sockets. `tests/storage.rs` also runs the real binary, including killing it mid-upload.
- `tests/gradle-project/run.sh` takes one Gradle executable per version to test. It passed with Gradle 8.13 and 9.8.2. Run it with the versions production builds use.
- The Rust version is pinned in `rust-toolchain.toml` and in the `Dockerfile`; change both together.
- `unsafe` is forbidden crate-wide. Direct dependencies are listed with their reason in `Cargo.toml`.
- `cargo deny check` and `cargo audit` passed on 2026-11-23 (46 crates in `Cargo.lock`, no advisories).
- No CI pipeline is set up yet. It should run the commands above with `--locked`.

## Implementation notes

- **Header timeout.** hyper's own header timeout also counts keep-alive idle time, which would close the load balancer's pooled connections after 10 s. The timeouts are implemented on the socket instead: 10 s for a request head once its first byte arrives (or from connect, for the first request), 75 s of inactivity otherwise.
- **Extra variables:** `CACHE_DISCARD_CEILING`, `CACHE_EVICTION_LOW_WATERMARK`, `CACHE_TOUCH_INTERVAL`, `CACHE_AUTH_WAIT_TIMEOUT`, `CACHE_AUTH_CACHE_TTL`.
- **Extra metrics:** `auth_verifications_total`, `auth_throttled_total`, `auth_busy_total`, `eviction_errors_total`, `invalid_files`, `healthy`.
- **`SIGHUP` reload** of the users file is implemented.
- **Storage faults other than a full disk** answer `500`. Only "too large" and "no space" are soft-rejected.
- **Dependencies:** `blake2` (already required by `argon2`) provides the keyed hash, base64 decoding is hand-written, and `http-body-util` was not needed.

## Open items

These need the production environment and are not done:

- Time argon2 on a `t3.medium` and confirm the default parameters land in the tens of milliseconds.
- Run the load test on the production instance type and EBS volume, with and without fsync.
- Rehearse the migration on a volume restored from the snapshot, and note the startup scan time on cold disks.
- Run the Gradle regression with the exact Gradle versions production builds use (9.8.2 was the newest available here).
- Set up CI, including `cargo deny` and `cargo audit`.
