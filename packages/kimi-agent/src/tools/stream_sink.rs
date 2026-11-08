//! Sink-shaped background stream draining.
//!
//! v2 `process-task.ts:146-152` appends each chunk to a durable sink as it
//! arrives; `:119-132` can then bound the drain with a 250 ms grace without
//! losing output. This module ports the reader half only. The caller that wires
//! the grace comes next — the existing `read_bounded_stream` is left untouched
//! until then.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt};

use super::{MAX_RETAINED_OUTPUT_BYTES, MAX_TASK_OUTPUT_BYTES};

/// A bounded, durable tail of one stream. Chunks are appended as they arrive,
/// so an abandoned read future leaves its output behind.
#[derive(Debug, Clone, Default)]
pub struct StreamTail(Arc<Mutex<Vec<u8>>>);

impl StreamTail {
    pub fn append(&self, chunk: &[u8]) {
        let mut buf = self.0.lock().unwrap_or_else(|e| e.into_inner());
        buf.extend_from_slice(chunk);
        if buf.len() > MAX_RETAINED_OUTPUT_BYTES + 8192 {
            let excess = buf.len() - MAX_RETAINED_OUTPUT_BYTES;
            buf.drain(..excess);
        }
    }

    pub fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// Drain one stream into `sink`, keeping the same accounting as
/// `tools::read_bounded_stream`: one shared byte counter across both streams,
/// a stop as soon as the cap trips, and a bounded tail retained per stream.
///
/// `emit` drives the live progress card. Nothing is returned — the sink is the
/// output's owner.
pub async fn read_bounded_stream_to_sink<R>(
    pipe: &mut R,
    stream: &str,
    emit: &(dyn Fn(&str, &[u8]) + Sync),
    total: &AtomicU64,
    tripped: &AtomicBool,
    sink: &StreamTail,
) where
    R: AsyncRead + Unpin,
{
    let mut chunk = [0u8; 8192];
    loop {
        if tripped.load(Ordering::Relaxed) {
            break;
        }
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                emit(stream, &chunk[..n]);
                let seen = total.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
                if seen > MAX_TASK_OUTPUT_BYTES as u64 {
                    tripped.store(true, Ordering::Relaxed);
                    break;
                }
                sink.append(&chunk[..n]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_emit(_: &str, _: &[u8]) {}

    #[tokio::test]
    async fn every_chunk_reaches_the_sink_as_it_arrives() {
        let mut pipe: &[u8] = b"one\ntwo\nthree";
        let sink = StreamTail::default();
        let total = AtomicU64::new(0);
        let tripped = AtomicBool::new(false);
        read_bounded_stream_to_sink(&mut pipe, "stdout", &noop_emit, &total, &tripped, &sink).await;
        assert_eq!(sink.take(), b"one\ntwo\nthree");
        assert_eq!(total.load(Ordering::Relaxed), 13);
    }

    #[tokio::test]
    async fn output_survives_an_abandoned_read_future() {
        // A pipe that yields one chunk and then never closes. The read future is
        // dropped, as it would be when a 250 ms grace expires.
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        struct Pending {
            sent: bool,
            _rx: tokio::sync::oneshot::Receiver<()>,
        }
        impl AsyncRead for Pending {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                if !self.sent {
                    buf.put_slice(b"kept");
                    self.sent = true;
                    return std::task::Poll::Ready(Ok(()));
                }
                std::task::Poll::Pending
            }
        }
        let mut pipe = Pending {
            sent: false,
            _rx: rx,
        };
        let sink = StreamTail::default();
        let total = AtomicU64::new(0);
        let tripped = AtomicBool::new(false);
        let fut =
            read_bounded_stream_to_sink(&mut pipe, "stdout", &noop_emit, &total, &tripped, &sink);
        // Poll once so the first chunk is appended, then abandon the future.
        let mut fut = Box::pin(fut);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(10), &mut fut).await;
        drop(fut);
        drop(tx);
        assert_eq!(
            sink.take(),
            b"kept",
            "the grace must not discard read output"
        );
    }

    #[tokio::test]
    async fn the_two_streams_share_one_cap() {
        let a = StreamTail::default();
        let b = StreamTail::default();
        let total = AtomicU64::new(0);
        let tripped = AtomicBool::new(false);
        // One 8 KiB chunk on each stream stays under the 16 MiB cap; the test
        // pins that the counter is shared rather than per-stream.
        let mut out: &[u8] = &[b'x'; 8192];
        let mut err: &[u8] = &[b'y'; 8192];
        read_bounded_stream_to_sink(&mut out, "stdout", &noop_emit, &total, &tripped, &a).await;
        read_bounded_stream_to_sink(&mut err, "stderr", &noop_emit, &total, &tripped, &b).await;
        assert_eq!(total.load(Ordering::Relaxed), 16_384);
        assert!(!tripped.load(Ordering::Relaxed));
        assert_eq!(a.len(), 8192);
        assert_eq!(b.len(), 8192);
    }

    #[tokio::test]
    async fn a_tripped_cap_stops_the_reader_before_the_next_chunk() {
        let total = AtomicU64::new(MAX_TASK_OUTPUT_BYTES as u64);
        let tripped = AtomicBool::new(false);
        let sink = StreamTail::default();
        let mut pipe: &[u8] = b"first";
        read_bounded_stream_to_sink(&mut pipe, "stdout", &noop_emit, &total, &tripped, &sink).await;
        assert!(tripped.load(Ordering::Relaxed));
        assert!(sink.take().is_empty());

        let mut stopped: &[u8] = b"second";
        read_bounded_stream_to_sink(
            &mut stopped,
            "stderr",
            &noop_emit,
            &AtomicU64::new(0),
            &tripped,
            &sink,
        )
        .await;
        assert!(sink.take().is_empty(), "a tripped reader must not drain");
    }

    #[tokio::test]
    async fn the_tail_stays_bounded() {
        let sink = StreamTail::default();
        let payload = vec![b'z'; 64 * 1024];
        for _ in 0..(MAX_RETAINED_OUTPUT_BYTES / (64 * 1024) + 4) {
            sink.append(&payload);
        }
        assert!(sink.len() <= MAX_RETAINED_OUTPUT_BYTES + 8192);
    }
}
