//! Routing, request validation and status mapping.
//!
//! The rule behind every status chosen here: for a Gradle client the only
//! harmless answers are `2xx` and `404`. Anything else switches off the remote
//! cache for the rest of that build, so error statuses are reserved for
//! requests that are wrong or hostile.

use std::future::poll_fn;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Instant, SystemTime};

use hyper::body::{Body as _, Bytes, Incoming};
use hyper::header::{
    ALLOW, AUTHORIZATION, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, EXPECT, HeaderValue,
    RETRY_AFTER, WWW_AUTHENTICATE,
};
use hyper::http::request::Parts;
use hyper::{Method, Request, Response, StatusCode};

use crate::App;
use crate::auth::{Decision, Role, client_ip};
use crate::body::{Body, CHUNK};
use crate::index::Touch;
use crate::log;
use crate::metrics::RejectReason;
use crate::store::{is_no_space, is_valid_key, now_secs};

/// Largest unread request body that is drained to keep a connection alive
/// after an early response; anything bigger gets `Connection: close`.
const DRAIN_UNREAD_MAX: u64 = 64 * 1024;

const MAX_ENTRY_SIZE_HEADER: &str = "x-max-entry-size-bytes";

fn text(status: StatusCode, body: &'static str) -> Response<Body> {
    let mut resp = Response::new(Body::full(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

fn status_only(status: StatusCode) -> Response<Body> {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = status;
    resp
}

fn method_not_allowed(allow: &'static str) -> Response<Body> {
    let mut resp = text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
    resp.headers_mut()
        .insert(ALLOW, HeaderValue::from_static(allow));
    resp
}

fn close(mut resp: Response<Body>) -> Response<Body> {
    resp.headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    resp
}

fn expects_continue(parts: &Parts) -> bool {
    parts
        .headers
        .get(EXPECT)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"))
}

/// Next chunk of the request body. Reading the first one is what makes the
/// HTTP library send `100 Continue` to a client that asked for it.
async fn next_data(body: &mut Incoming) -> Option<Result<Bytes, hyper::Error>> {
    loop {
        match poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await? {
            Ok(frame) => {
                if let Ok(data) = frame.into_data() {
                    return Some(Ok(data));
                }
            }
            Err(e) => return Some(Err(e)),
        }
    }
}

/// Handles one request on the cache listener.
pub async fn handle(app: &Arc<App>, peer: IpAddr, req: Request<Incoming>) -> Response<Body> {
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    let mut body = Some(body);
    let mut user = None;
    let mut resp = route(app, peer, &parts, &mut body, &mut user).await;

    // A response decided without reading the request body (every check
    // runs before the body is polled). A small body that is already
    // on its way is read and dropped so the connection stays usable; otherwise
    // the client is told the connection is closing rather than finding out on
    // its next request.
    if let Some(mut unread) = body
        && !unread.is_end_stream()
    {
        let small = unread
            .size_hint()
            .exact()
            .is_some_and(|n| n <= DRAIN_UNREAD_MAX);
        let mut drained = false;
        if small && !expects_continue(&parts) {
            drained = true;
            while let Some(chunk) = next_data(&mut unread).await {
                if chunk.is_err() {
                    drained = false;
                    break;
                }
            }
        }
        if !drained {
            resp = close(resp);
        }
    }

    let elapsed = started.elapsed();
    app.metrics
        .request(parts.method.as_str(), resp.status().as_u16(), elapsed);
    if app.cfg.access_log {
        let path: String = parts.uri.path().chars().take(200).collect();
        log::info(
            "request",
            &[
                ("method", parts.method.as_str().into()),
                ("path", (&path).into()),
                ("status", resp.status().as_u16().into()),
                ("duration_us", (elapsed.as_micros() as u64).into()),
                ("client", (&peer.to_string()).into()),
                ("user", user.as_deref().unwrap_or("").into()),
            ],
        );
    }
    resp
}

async fn route(
    app: &Arc<App>,
    peer: IpAddr,
    parts: &Parts,
    body: &mut Option<Incoming>,
    user: &mut Option<Arc<str>>,
) -> Response<Body> {
    let read_only = parts.method == Method::GET || parts.method == Method::HEAD;
    match parts.uri.path() {
        "/ping" if read_only => text(StatusCode::OK, "SUCCESS"),
        "/health" | "/cache-node-info/health" if read_only => health(app),
        "/ping" | "/health" | "/cache-node-info/health" => method_not_allowed("GET, HEAD"),
        path => match path.strip_prefix("/cache/") {
            Some(key) => {
                let mut resp = cache(app, peer, parts, key, body, user).await;
                if let Ok(v) = HeaderValue::from_str(&app.cfg.max_artifact_size.to_string()) {
                    resp.headers_mut().insert(MAX_ENTRY_SIZE_HEADER, v);
                }
                resp
            }
            None => text(StatusCode::NOT_FOUND, "not found\n"),
        },
    }
}

fn health(app: &App) -> Response<Body> {
    if !app.index.is_ready() {
        text(
            StatusCode::SERVICE_UNAVAILABLE,
            "UNHEALTHY: index is still loading\n",
        )
    } else if !app.writable.load(Relaxed) {
        text(
            StatusCode::SERVICE_UNAVAILABLE,
            "UNHEALTHY: data directory is not writable\n",
        )
    } else {
        text(StatusCode::OK, "HEALTHY\n")
    }
}

async fn cache(
    app: &Arc<App>,
    peer: IpAddr,
    parts: &Parts,
    key: &str,
    body: &mut Option<Incoming>,
    user: &mut Option<Arc<str>>,
) -> Response<Body> {
    // 1. Method.
    let needed = match parts.method {
        Method::GET | Method::HEAD => Role::Read,
        Method::PUT => Role::ReadWrite,
        _ => return method_not_allowed("GET, HEAD, PUT"),
    };
    // 2. Key. From here on `key` is safe to use in a filesystem path.
    if !is_valid_key(key) {
        return text(StatusCode::BAD_REQUEST, "invalid cache key\n");
    }
    // 3. Authentication and authorization.
    let forwarded = parts
        .headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()
        .map(|v| v.as_bytes());
    let client = client_ip(peer, forwarded, &app.cfg.trusted_proxies);
    let credentials = parts.headers.get(AUTHORIZATION).map(|v| v.as_bytes());
    match app
        .auth
        .check(credentials, client, needed, &app.metrics)
        .await
    {
        Decision::Allow(name) => *user = name,
        Decision::Unauthorized => {
            let mut resp = text(StatusCode::UNAUTHORIZED, "authentication required\n");
            resp.headers_mut().insert(
                WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"cache\""),
            );
            return resp;
        }
        Decision::Forbidden => return text(StatusCode::FORBIDDEN, "write access required\n"),
        Decision::Throttled(retry_after) => {
            let mut resp = text(StatusCode::TOO_MANY_REQUESTS, "too many failed attempts\n");
            resp.headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(retry_after));
            return resp;
        }
        Decision::Busy => {
            let mut resp = text(StatusCode::SERVICE_UNAVAILABLE, "authentication is busy\n");
            resp.headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(1u64));
            return resp;
        }
    }
    match parts.method {
        Method::PUT => put(app, parts, key, body).await,
        _ => get(app, key, parts.method == Method::HEAD).await,
    }
}

async fn get(app: &Arc<App>, key: &str, head_only: bool) -> Response<Body> {
    let miss = || {
        app.metrics.misses.fetch_add(1, Relaxed);
        status_only(StatusCode::NOT_FOUND)
    };
    let now = now_secs();
    let touch = app.index.touch(key, now, app.cfg.touch_interval.as_secs());
    // Once the startup scan is complete the index is authoritative and a miss
    // costs no disk access. During the scan, reads go straight to disk.
    if touch == Touch::Missing && app.index.is_ready() {
        return miss();
    }
    let set_mtime = (touch != Touch::Fresh).then(SystemTime::now);
    let first_len = if head_only { 0 } else { CHUNK };
    let store = app.store.clone();
    let owned_key = key.to_string();
    let opened =
        tokio::task::spawn_blocking(move || store.open_read(&owned_key, first_len, set_mtime))
            .await;
    match opened {
        Ok(Ok(opened)) => {
            if touch == Touch::Missing {
                app.index.insert_if_absent(key, opened.size, opened.mtime);
                app.index.touch(key, now, app.cfg.touch_interval.as_secs());
            }
            app.metrics.hits.fetch_add(1, Relaxed);
            let body = if head_only {
                Body::empty()
            } else {
                Body::file(opened.file, opened.first, opened.size, app.metrics.clone())
            };
            let mut resp = Response::new(body);
            let headers = resp.headers_mut();
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            headers.insert(CONTENT_LENGTH, HeaderValue::from(opened.size));
            resp
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            // The file is gone (evicted, or removed by hand): drop the stale record.
            if touch != Touch::Missing {
                app.index.remove(key);
            }
            miss()
        }
        Ok(Err(e)) => {
            log::error(
                "cannot read entry",
                &[("key", key.into()), ("error", (&e.to_string()).into())],
            );
            status_only(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(e) => {
            log::error(
                "read task failed",
                &[("key", key.into()), ("error", (&e.to_string()).into())],
            );
            status_only(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

fn note_reject(app: &App, key: &str, size: u64, reason: RejectReason) {
    app.metrics.rejected_upload(reason);
    // The client is told `200`, so this log line and the counter are the only
    // trace that the entry was not stored.
    log::warn(
        "upload not stored",
        &[
            ("key", key.into()),
            ("size", size.into()),
            ("reason", reason.as_str().into()),
        ],
    );
    if reason == RejectReason::NoSpace {
        app.evict_wake.notify_one();
    }
}

fn too_large() -> Response<Body> {
    close(text(
        StatusCode::PAYLOAD_TOO_LARGE,
        "upload far exceeds the size limit\n",
    ))
}

async fn put(
    app: &Arc<App>,
    parts: &Parts,
    key: &str,
    body_slot: &mut Option<Incoming>,
) -> Response<Body> {
    let cfg = &app.cfg;
    let declared = body_slot.as_ref().and_then(|b| b.size_hint().exact());

    // 4. Acceptance check, still before the body is read. An upload the server
    // will not keep is answered `200` and discarded (soft reject).
    if let Some(len) = declared {
        let reason = if len > cfg.max_artifact_size {
            Some(RejectReason::TooLarge)
        } else if !app.can_fit(len) {
            Some(RejectReason::NoSpace)
        } else {
            None
        };
        if let Some(reason) = reason {
            note_reject(app, key, len, reason);
            if expects_continue(parts) {
                // Final answer instead of `100 Continue`: the client never
                // sends the body. The body stays in `body_slot`, unread, and
                // the caller marks the connection as closing.
                return status_only(StatusCode::OK);
            }
            if len > cfg.discard_ceiling {
                return too_large();
            }
        }
        if reason.is_some() {
            let Some(mut body) = body_slot.take() else {
                return status_only(StatusCode::OK);
            };
            return discard(app, &mut body, 0).await;
        }
    }

    let Some(mut body) = body_slot.take() else {
        return status_only(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let mut upload = match app.store.begin_upload() {
        Ok(upload) => upload,
        Err(e) => return storage_error(app, key, &e, &mut body, 0).await,
    };
    let mut received = 0u64;
    while let Some(chunk) = next_data(&mut body).await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            // The client went away or sent a malformed body. Dropping `upload`
            // removes the temp file. Nobody is likely to read this response.
            Err(_) => return close(status_only(StatusCode::BAD_REQUEST)),
        };
        received += chunk.len() as u64;
        app.metrics
            .bytes_received
            .fetch_add(chunk.len() as u64, Relaxed);
        if received > cfg.max_artifact_size {
            // A chunked or length-less upload crossed the limit mid-stream.
            drop(upload);
            note_reject(app, key, received, RejectReason::TooLarge);
            return discard(app, &mut body, received).await;
        }
        if let Err(e) = upload.write(&chunk).await {
            drop(upload);
            return storage_error(app, key, &e, &mut body, received).await;
        }
    }
    if declared.is_some_and(|len| len != received) {
        return close(status_only(StatusCode::BAD_REQUEST));
    }

    let index_app = app.clone();
    let index_key = key.to_string();
    let committed = upload
        .commit(key, move |size| {
            index_app.index.insert(&index_key, size, now_secs());
        })
        .await;
    match committed {
        Ok(_) => {
            if cfg
                .max_size
                .is_some_and(|max| app.index.total_bytes() > max)
            {
                app.evict_wake.notify_one();
            }
            status_only(StatusCode::OK)
        }
        Err(e) => storage_error(app, key, &e, &mut body, received).await,
    }
}

/// Maps a storage failure during upload to a response. A full disk is a soft
/// reject: the build keeps its cache and eviction is woken. Anything else is a
/// real server fault and says so.
async fn storage_error(
    app: &Arc<App>,
    key: &str,
    e: &std::io::Error,
    body: &mut Incoming,
    received: u64,
) -> Response<Body> {
    if is_no_space(e) {
        note_reject(app, key, received, RejectReason::NoSpace);
        return discard(app, body, received).await;
    }
    log::error(
        "cannot store entry",
        &[("key", key.into()), ("error", (&e.to_string()).into())],
    );
    let resp = status_only(StatusCode::INTERNAL_SERVER_ERROR);
    if body.is_end_stream() {
        resp
    } else {
        close(resp)
    }
}

/// Reads and drops the rest of a rejected upload, then answers `200`. Closing
/// the connection instead would look like a transmission failure to Gradle,
/// which retries and then disables the cache. Past the hard ceiling the server
/// stops being polite: `413` and close.
async fn discard(app: &Arc<App>, body: &mut Incoming, mut received: u64) -> Response<Body> {
    while let Some(chunk) = next_data(body).await {
        match chunk {
            Ok(chunk) => {
                received += chunk.len() as u64;
                app.metrics
                    .bytes_received
                    .fetch_add(chunk.len() as u64, Relaxed);
                if received > app.cfg.discard_ceiling {
                    return too_large();
                }
            }
            Err(_) => return close(status_only(StatusCode::BAD_REQUEST)),
        }
    }
    status_only(StatusCode::OK)
}

/// Handles one request on the admin listener: metrics only.
pub fn handle_admin(app: &App, req: &Request<Incoming>) -> Response<Body> {
    match req.uri().path() {
        "/metrics" if req.method() == Method::GET || req.method() == Method::HEAD => {
            let text = app.metrics.render(
                app.index.len(),
                app.index.total_bytes(),
                app.cfg.max_size,
                app.is_healthy(),
            );
            let mut resp = Response::new(Body::full(text));
            resp.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
            );
            resp
        }
        "/metrics" => method_not_allowed("GET, HEAD"),
        _ => text(StatusCode::NOT_FOUND, "not found\n"),
    }
}
