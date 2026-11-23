//! Startup, subcommands and signal handling.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

use apps_infra_mobile_build_cache_server::config::{Access, Config, FreeSpace};
use apps_infra_mobile_build_cache_server::server::Server;
use apps_infra_mobile_build_cache_server::{auth, log};
use tokio::signal::unix::{SignalKind, signal};

const USAGE: &str = "\
Gradle remote build cache server.

Usage:
  apps-infra-mobile-build-cache-server [serve]
      Run the server. Configuration comes from CACHE_* environment variables;
      see README.md.
  apps-infra-mobile-build-cache-server hash-password <username> <read|readwrite>
      Read a password from standard input and print the line for the users file.
  apps-infra-mobile-build-cache-server healthcheck
      Ask the local server for /ping; exit 0 if it answers (for Docker HEALTHCHECK).
  apps-infra-mobile-build-cache-server --version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        [] | ["serve"] => serve(),
        ["hash-password", username, role] => hash_password(username, role),
        ["healthcheck"] => healthcheck(),
        ["--version" | "-V"] => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        ["--help" | "-h" | "help"] => {
            print!("{USAGE}");
            Ok(())
        }
        _ => Err(format!("unrecognised arguments\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn hash_password(username: &str, role: &str) -> Result<(), String> {
    if username.is_empty() || username.contains(':') || username.chars().any(char::is_control) {
        return Err(
            "username must be non-empty and must not contain ':' or control characters".into(),
        );
    }
    if role != "read" && role != "readwrite" {
        return Err("role must be read or readwrite".into());
    }
    let mut password = Vec::new();
    std::io::stdin()
        .read_to_end(&mut password)
        .map_err(|e| format!("cannot read password from standard input: {e}"))?;
    // `echo password | …` and here-strings add a line ending that is not part
    // of the password.
    if password.ends_with(b"\n") {
        password.pop();
        if password.ends_with(b"\r") {
            password.pop();
        }
    }
    if password.is_empty() {
        return Err("empty password on standard input".into());
    }
    let hash = auth::hash_password(&password)?;
    println!("{username}:{role}:{hash}");
    Ok(())
}

fn healthcheck() -> Result<(), String> {
    let listen = Config::from_env()?.listen;
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let addr = SocketAddr::new(ip, listen.port());
    let timeout = Duration::from_secs(3);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("cannot connect to {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .map_err(|e| format!("cannot send request: {e}"))?;
    let mut response = Vec::new();
    stream
        .take(4096)
        .read_to_end(&mut response)
        .map_err(|e| format!("cannot read response: {e}"))?;
    let response = String::from_utf8_lossy(&response);
    if response.starts_with("HTTP/1.1 200") && response.ends_with("SUCCESS") {
        Ok(())
    } else {
        Err("unexpected response to /ping".into())
    }
}

fn serve() -> Result<(), String> {
    let cfg = Config::from_env()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start runtime: {e}"))?;
    let result = runtime.block_on(run(cfg));
    // Blocking tasks still running (a write to a dead disk, say) must not hold
    // the process open past shutdown.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

fn log_config(cfg: &Config, users: usize) {
    let opt_size = |v: Option<u64>| v.map_or("none".to_string(), |n| n.to_string());
    let admin = cfg
        .admin_listen
        .map_or("disabled".to_string(), |a| a.to_string());
    let min_free = match cfg.min_free_space {
        Some(FreeSpace::Bytes(b)) => b.to_string(),
        Some(FreeSpace::Percent(p)) => format!("{p}%"),
        None => "none".to_string(),
    };
    let anonymous = match cfg.anonymous_access {
        Access::None => "none",
        Access::Read => "read",
        Access::ReadWrite => "readwrite",
    };
    let proxies = cfg.trusted_proxies.len();
    log::info(
        "starting",
        &[
            ("version", env!("CARGO_PKG_VERSION").into()),
            ("listen", (&cfg.listen.to_string()).into()),
            ("admin_listen", (&admin).into()),
            ("data_dir", (&cfg.data_dir.display().to_string()).into()),
            ("max_size_bytes", (&opt_size(cfg.max_size)).into()),
            (
                "max_age_seconds",
                (&opt_size(cfg.max_age.map(|d| d.as_secs()))).into(),
            ),
            ("min_free_space", (&min_free).into()),
            ("max_artifact_size_bytes", cfg.max_artifact_size.into()),
            ("discard_ceiling_bytes", cfg.discard_ceiling.into()),
            (
                "eviction_interval_seconds",
                cfg.eviction_interval.as_secs().into(),
            ),
            ("eviction_low_watermark", cfg.eviction_low_watermark.into()),
            ("fsync", cfg.fsync.into()),
            (
                "touch_interval_seconds",
                cfg.touch_interval.as_secs().into(),
            ),
            ("users", users.into()),
            ("anonymous_access", anonymous.into()),
            ("auth_concurrency", cfg.auth_concurrency.into()),
            (
                "auth_wait_timeout_ms",
                (cfg.auth_wait_timeout.as_millis() as u64).into(),
            ),
            ("auth_failure_limit", cfg.auth_failure_limit.into()),
            (
                "auth_failure_window_seconds",
                cfg.auth_failure_window.as_secs().into(),
            ),
            (
                "auth_cache_ttl_seconds",
                cfg.auth_cache_ttl.as_secs().into(),
            ),
            ("trusted_proxies", proxies.into()),
            ("max_connections", cfg.max_connections.into()),
            (
                "header_timeout_ms",
                (cfg.header_timeout.as_millis() as u64).into(),
            ),
            (
                "idle_timeout_ms",
                (cfg.idle_timeout.as_millis() as u64).into(),
            ),
            (
                "shutdown_timeout_ms",
                (cfg.shutdown_timeout.as_millis() as u64).into(),
            ),
            ("access_log", cfg.access_log.into()),
        ],
    );
}

async fn run(cfg: Config) -> Result<(), String> {
    let server = Server::start(cfg).await?;
    let app = server.app.clone();
    log_config(&app.cfg, app.auth.user_count());
    if app.auth.user_count() == 0 && app.cfg.anonymous_access == Access::None {
        log::warn(
            "no users and no anonymous access: every cache request will be refused (set CACHE_USERS_FILE or CACHE_ANONYMOUS_ACCESS)",
            &[],
        );
    }
    if app.cfg.anonymous_access == Access::ReadWrite {
        log::warn("anonymous clients can write to the cache", &[]);
    }
    log::info(
        "listening",
        &[("address", (&server.cache_addr.to_string()).into())],
    );

    let signal_error = |e: std::io::Error| format!("cannot install signal handler: {e}");
    let mut sigterm = signal(SignalKind::terminate()).map_err(signal_error)?;
    let mut sigint = signal(SignalKind::interrupt()).map_err(signal_error)?;
    let mut sighup = signal(SignalKind::hangup()).map_err(signal_error)?;
    loop {
        tokio::select! {
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            _ = sighup.recv() => match app.auth.reload() {
                Ok(count) => log::info("users file reloaded", &[("users", count.into())]),
                Err(e) => log::error("users file reload failed; keeping current users", &[("error", (&e).into())]),
            },
        }
    }
    log::info("shutting down", &[]);
    let drained = server.shutdown().await;
    log::info("stopped", &[("drained", drained.into())]);
    Ok(())
}
