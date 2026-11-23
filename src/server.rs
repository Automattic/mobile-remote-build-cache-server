//! Listeners, connection limits, timeouts and graceful shutdown.

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, Sleep};

use crate::body::SetOnDrop;
use crate::config::Config;
use crate::{App, evict, log, routes};

/// Connection phases, shared between the socket wrapper and the request handler.
/// Waiting for a request head whose deadline is already running.
const PHASE_HEAD: u8 = 0;
/// Keep-alive: waiting for the first byte of the next request.
const PHASE_IDLE: u8 = 1;
/// A request head has been parsed and its response is not finished yet.
const PHASE_BUSY: u8 = 2;

/// Size of the per-connection read buffer. It bounds the request head: the
/// HTTP library refuses a head once the buffer holds this much without a
/// complete one, so a head can reach at most twice this size.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Connection cap of the admin listener, which only serves metrics.
const ADMIN_MAX_CONNECTIONS: usize = 64;

/// A socket with two timeouts that the HTTP library does not provide in the
/// shape needed here:
///
/// - **Header timeout** (slowloris defence): a request head must be complete
///   within `header_timeout` of the connection opening or, on a kept-alive
///   connection, of its first byte arriving. hyper's own header timeout also
///   counts keep-alive idle time, which would close idle connections the load
///   balancer still considers usable.
/// - **Idle timeout**: whenever the server is waiting on the peer (to send
///   body bytes, to accept response bytes, or to start the next request),
///   some byte must move in either direction within `idle_timeout`.
struct TimedIo {
    inner: TcpStream,
    phase: Arc<AtomicU8>,
    header_timeout: Duration,
    idle_timeout: Duration,
    head_deadline: Instant,
    last_activity: Instant,
    timer: Pin<Box<Sleep>>,
}

impl TimedIo {
    fn new(inner: TcpStream, phase: Arc<AtomicU8>, cfg: &Config) -> TimedIo {
        let now = Instant::now();
        TimedIo {
            inner,
            phase,
            header_timeout: cfg.header_timeout,
            idle_timeout: cfg.idle_timeout,
            head_deadline: now + cfg.header_timeout,
            last_activity: now,
            timer: Box::pin(tokio::time::sleep_until(now + cfg.header_timeout)),
        }
    }

    /// Called when the socket is not ready: fails once `deadline` has passed,
    /// otherwise arranges a wake-up for it.
    fn poll_deadline<T>(&mut self, cx: &mut Context<'_>, deadline: Instant) -> Poll<io::Result<T>> {
        if self.timer.deadline() != deadline {
            self.timer.as_mut().reset(deadline);
        }
        match self.timer.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::ErrorKind::TimedOut.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncRead for TimedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let now = Instant::now();
                this.last_activity = now;
                if buf.filled().len() > before
                    && this
                        .phase
                        .compare_exchange(PHASE_IDLE, PHASE_HEAD, Relaxed, Relaxed)
                        .is_ok()
                {
                    this.head_deadline = now + this.header_timeout;
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => {
                let deadline = if this.phase.load(Relaxed) == PHASE_HEAD {
                    this.head_deadline
                } else {
                    this.last_activity + this.idle_timeout
                };
                this.poll_deadline(cx, deadline)
            }
        }
    }
}

impl AsyncWrite for TimedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(cx, data) {
            Poll::Ready(Ok(n)) => {
                this.last_activity = Instant::now();
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => {
                let deadline = this.last_activity + this.idle_timeout;
                this.poll_deadline(cx, deadline)
            }
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write_vectored(cx, bufs) {
            Poll::Ready(Ok(n)) => {
                this.last_activity = Instant::now();
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => {
                let deadline = this.last_activity + this.idle_timeout;
                this.poll_deadline(cx, deadline)
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Cache,
    Admin,
}

/// Decrements the active-connection gauge when a connection task ends.
struct ConnectionGauge(Arc<App>);

impl Drop for ConnectionGauge {
    fn drop(&mut self) {
        self.0.metrics.connections_active.fetch_sub(1, Relaxed);
    }
}

async fn serve_connection(
    app: Arc<App>,
    kind: Kind,
    stream: TcpStream,
    peer: IpAddr,
    mut shutdown: watch::Receiver<bool>,
) {
    let _gauge = (kind == Kind::Cache).then(|| {
        app.metrics.connections_active.fetch_add(1, Relaxed);
        ConnectionGauge(app.clone())
    });
    let _ = stream.set_nodelay(true);
    let phase = Arc::new(AtomicU8::new(PHASE_HEAD));
    let io = TokioIo::new(TimedIo::new(stream, phase.clone(), &app.cfg));
    let service = service_fn(move |req| {
        let app = app.clone();
        phase.store(PHASE_BUSY, Relaxed);
        // Dropped when the response body has been handed over completely (or
        // the request is abandoned): the connection is then idle again.
        let done = SetOnDrop(phase.clone(), PHASE_IDLE);
        async move {
            let mut resp = match kind {
                Kind::Cache => routes::handle(&app, peer, req).await,
                Kind::Admin => routes::handle_admin(&app, &req),
            };
            resp.body_mut().set_guard(done);
            Ok::<_, Infallible>(resp)
        }
    });
    let mut builder = http1::Builder::new();
    builder.keep_alive(true).max_buf_size(MAX_HEAD_BYTES);
    let conn = builder.serve_connection(io, service);
    tokio::pin!(conn);
    // Errors here are the peer's doing (resets, timeouts, garbage): there is
    // nothing to report beyond what the metrics already count.
    tokio::select! {
        _ = conn.as_mut() => {}
        _ = shutdown.changed() => {
            // Finish the request in flight, then close instead of keeping alive.
            conn.as_mut().graceful_shutdown();
            let _ = conn.await;
        }
    }
}

async fn accept_loop(
    app: Arc<App>,
    kind: Kind,
    listener: TcpListener,
    permits: Arc<Semaphore>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        // Take a slot before accepting: at the cap, further connections wait
        // in the kernel's accept backlog instead of consuming memory here.
        let permit = tokio::select! {
            p = permits.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => return,
            },
            _ = shutdown.changed() => return,
        };
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = shutdown.changed() => return,
        };
        match accepted {
            Ok((stream, peer)) => {
                let conn = serve_connection(app.clone(), kind, stream, peer.ip(), shutdown.clone());
                tokio::spawn(async move {
                    conn.await;
                    drop(permit);
                });
            }
            Err(e) => {
                // Typically file descriptor exhaustion; back off briefly
                // rather than spin.
                log::error("accept failed", &[("error", (&e.to_string()).into())]);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// A running server. Dropping it without calling [`Server::shutdown`] leaves
/// the tasks running until the runtime ends.
pub struct Server {
    pub app: Arc<App>,
    pub cache_addr: SocketAddr,
    pub admin_addr: Option<SocketAddr>,
    shutdown: watch::Sender<bool>,
    cache_permits: Arc<Semaphore>,
    tasks: Vec<JoinHandle<()>>,
}

impl Server {
    /// Prepares the data directory, binds both listeners and starts serving.
    /// The index scan runs in the background: requests are served while it runs.
    pub async fn start(cfg: Config) -> Result<Server, String> {
        cfg.validate()?;
        let app = Arc::new(App::new(cfg)?);
        let cfg = &app.cfg;
        let prepare_app = app.clone();
        tokio::task::spawn_blocking(move || prepare_app.store.prepare())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| {
                format!(
                    "cannot prepare data directory {}: {e}",
                    cfg.data_dir.display()
                )
            })?;

        let cache_listener = TcpListener::bind(cfg.listen)
            .await
            .map_err(|e| format!("cannot listen on {}: {e}", cfg.listen))?;
        let cache_addr = cache_listener.local_addr().map_err(|e| e.to_string())?;
        let admin_listener = match cfg.admin_listen {
            Some(addr) => Some(
                TcpListener::bind(addr)
                    .await
                    .map_err(|e| format!("cannot listen on {addr}: {e}"))?,
            ),
            None => None,
        };
        let admin_addr = match &admin_listener {
            Some(l) => Some(l.local_addr().map_err(|e| e.to_string())?),
            None => None,
        };

        let (shutdown, shutdown_rx) = watch::channel(false);
        let mut tasks = Vec::new();

        let scan_app = app.clone();
        tasks.push(tokio::spawn(async move {
            let app = scan_app.clone();
            if tokio::task::spawn_blocking(move || app.scan())
                .await
                .is_err()
            {
                log::error("startup scan failed", &[]);
            }
            // Eviction was held back until now.
            scan_app.evict_wake.notify_one();
        }));
        tasks.push(tokio::spawn(evict::run(app.clone(), shutdown_rx.clone())));

        let cache_permits = Arc::new(Semaphore::new(cfg.max_connections));
        tasks.push(tokio::spawn(accept_loop(
            app.clone(),
            Kind::Cache,
            cache_listener,
            cache_permits.clone(),
            shutdown_rx.clone(),
        )));
        if let Some(listener) = admin_listener {
            tasks.push(tokio::spawn(accept_loop(
                app.clone(),
                Kind::Admin,
                listener,
                Arc::new(Semaphore::new(ADMIN_MAX_CONNECTIONS)),
                shutdown_rx,
            )));
        }
        Ok(Server {
            app,
            cache_addr,
            admin_addr,
            shutdown,
            cache_permits,
            tasks,
        })
    }

    /// Stops accepting, lets requests in flight finish for up to
    /// `CACHE_SHUTDOWN_TIMEOUT`, then returns. Returns false if the deadline
    /// passed with connections still open; their unfinished uploads are
    /// discarded as temp files.
    pub async fn shutdown(self) -> bool {
        let _ = self.shutdown.send(true);
        let all = self.app.cfg.max_connections as u32;
        let drained = tokio::time::timeout(
            self.app.cfg.shutdown_timeout,
            self.cache_permits.acquire_many(all),
        )
        .await
        .is_ok();
        for task in &self.tasks {
            task.abort();
        }
        drained
    }
}
