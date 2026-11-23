//! The response body type: nothing, a small in-memory buffer, or a file
//! streamed in fixed-size chunks so entries are never buffered whole.

use std::fs::File;
use std::io::{self, Read};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::task::{Context, Poll};

use hyper::body::{Bytes, Frame, SizeHint};
use tokio::task::JoinHandle;

use crate::metrics::Metrics;

/// Bytes read from disk per blocking call when streaming an entry.
pub const CHUNK: usize = 256 * 1024;

/// Stores a value into an atomic when dropped. The connection uses it to learn
/// that a response has been handed over completely (see `server.rs`).
pub struct SetOnDrop(pub Arc<AtomicU8>, pub u8);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(self.1, Relaxed);
    }
}

enum FileState {
    Idle(File),
    Reading(JoinHandle<io::Result<(File, Vec<u8>)>>),
    Done,
}

pub struct FileBody {
    state: FileState,
    /// Bytes already read by the call that opened the file.
    first: Option<Bytes>,
    /// Bytes still to be read from the file.
    remaining: u64,
    metrics: Arc<Metrics>,
}

enum Kind {
    Empty,
    Full(Option<Bytes>),
    File(FileBody),
}

pub struct Body {
    kind: Kind,
    guard: Option<SetOnDrop>,
}

impl Body {
    pub fn empty() -> Body {
        Body {
            kind: Kind::Empty,
            guard: None,
        }
    }

    pub fn full(data: impl Into<Bytes>) -> Body {
        let data: Bytes = data.into();
        Body {
            kind: if data.is_empty() {
                Kind::Empty
            } else {
                Kind::Full(Some(data))
            },
            guard: None,
        }
    }

    /// Streams `size` bytes: `first` (already read) followed by the rest of `file`.
    pub fn file(file: File, first: Vec<u8>, size: u64, metrics: Arc<Metrics>) -> Body {
        let remaining = size.saturating_sub(first.len() as u64);
        Body {
            kind: Kind::File(FileBody {
                state: FileState::Idle(file),
                first: (!first.is_empty()).then(|| Bytes::from(first)),
                remaining,
                metrics,
            }),
            guard: None,
        }
    }

    pub fn set_guard(&mut self, guard: SetOnDrop) {
        self.guard = Some(guard);
    }
}

impl hyper::body::Body for Body {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        match &mut self.get_mut().kind {
            Kind::Empty => Poll::Ready(None),
            Kind::Full(data) => Poll::Ready(data.take().map(|d| Ok(Frame::data(d)))),
            Kind::File(f) => f.poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.kind {
            Kind::Empty => true,
            Kind::Full(data) => data.is_none(),
            Kind::File(f) => f.first.is_none() && f.remaining == 0,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.kind {
            Kind::Empty => SizeHint::with_exact(0),
            Kind::Full(data) => SizeHint::with_exact(data.as_ref().map_or(0, |d| d.len() as u64)),
            Kind::File(f) => {
                SizeHint::with_exact(f.first.as_ref().map_or(0, |d| d.len() as u64) + f.remaining)
            }
        }
    }
}

impl FileBody {
    fn poll_frame(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        if let Some(first) = self.first.take() {
            self.metrics
                .bytes_sent
                .fetch_add(first.len() as u64, Relaxed);
            return Poll::Ready(Some(Ok(Frame::data(first))));
        }
        loop {
            match std::mem::replace(&mut self.state, FileState::Done) {
                FileState::Done => return Poll::Ready(None),
                FileState::Idle(mut file) => {
                    if self.remaining == 0 {
                        return Poll::Ready(None);
                    }
                    let len = self.remaining.min(CHUNK as u64) as usize;
                    self.state = FileState::Reading(tokio::task::spawn_blocking(move || {
                        let mut buf = vec![0; len];
                        file.read_exact(&mut buf)?;
                        Ok((file, buf))
                    }));
                }
                FileState::Reading(mut handle) => {
                    let result = match Pin::new(&mut handle).poll(cx) {
                        Poll::Pending => {
                            self.state = FileState::Reading(handle);
                            return Poll::Pending;
                        }
                        Poll::Ready(r) => r,
                    };
                    return Poll::Ready(Some(match result {
                        Ok(Ok((file, buf))) => {
                            self.remaining -= buf.len() as u64;
                            self.metrics.bytes_sent.fetch_add(buf.len() as u64, Relaxed);
                            self.state = FileState::Idle(file);
                            Ok(Frame::data(Bytes::from(buf)))
                        }
                        // The state stays `Done`: hyper aborts the connection,
                        // so the client sees a short body, never wrong bytes.
                        Ok(Err(e)) => Err(e),
                        Err(e) => Err(io::Error::other(e.to_string())),
                    }));
                }
            }
        }
    }
}
