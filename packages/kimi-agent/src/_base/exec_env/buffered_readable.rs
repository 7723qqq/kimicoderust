use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;
use parking_lot::Mutex;

const RESUME_LIVENESS_MS: u64 = 10;
const MAX_BUFFERED_BYTES: usize = 8 * 1024 * 1024;
const HIGH_WATER_MARK: usize = 128 * 1024;

pub trait ReadableSource: Stream<Item = Result<Vec<u8>, io::Error>> + Send {
    fn pause(&self);
    fn resume(&self);
    fn destroy(&self);
}

struct BufferState {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
}

pub struct BufferedReadable {
    source: Arc<Mutex<Pin<Box<dyn ReadableSource>>>>,
    buffer: Arc<Mutex<BufferState>>,
    ended: Arc<AtomicBool>,
    destroyed: Arc<AtomicBool>,
    consumer_arrived: AtomicBool,
    resume_timer: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    error: Mutex<Option<io::Error>>,
}

impl BufferedReadable {
    pub fn new(source: Pin<Box<dyn ReadableSource>>) -> Self {
        Self {
            source: Arc::new(Mutex::new(source)),
            buffer: Arc::new(Mutex::new(BufferState {
                chunks: VecDeque::new(),
                bytes: 0,
            })),
            ended: Arc::new(AtomicBool::new(false)),
            destroyed: Arc::new(AtomicBool::new(false)),
            consumer_arrived: AtomicBool::new(false),
            resume_timer: Arc::new(Mutex::new(None)),
            error: Mutex::new(None),
        }
    }

    pub fn read(&self) {
        self.consumer_arrived.store(true, Ordering::SeqCst);
        self.clear_resume();
        if !self.ended.load(Ordering::SeqCst) && !self.destroyed.load(Ordering::SeqCst) {
            self.source.lock().resume();
        }
    }

    pub fn on_data(&self, chunk: impl Into<Vec<u8>>) -> bool {
        let chunk = chunk.into();
        let accepted = {
            let mut buffer = self.buffer.lock();
            buffer.bytes += chunk.len();
            buffer.chunks.push_back(chunk);
            buffer.bytes < HIGH_WATER_MARK
        };
        if !accepted {
            self.source.lock().pause();
            if !self.consumer_arrived.load(Ordering::SeqCst) {
                self.arm_resume();
            }
        }
        accepted
    }

    pub fn on_end(&self) {
        self.clear_resume();
        self.ended.store(true, Ordering::SeqCst);
    }

    pub fn on_close(&self) {
        self.clear_resume();
        self.ended.store(true, Ordering::SeqCst);
    }

    pub fn on_error(&self, error: io::Error) {
        self.destroy(Some(error));
    }

    pub fn buffered_bytes(&self) -> usize {
        self.buffer.lock().bytes
    }

    pub fn destroy(&self, error: Option<io::Error>) {
        self.clear_resume();
        self.destroyed.store(true, Ordering::SeqCst);
        if let Some(error) = error {
            *self.error.lock() = Some(error);
        }
        self.source.lock().destroy();
    }

    fn arm_resume(&self) {
        {
            let mut timer = self.resume_timer.lock();
            if timer.as_ref().is_some_and(|handle| !handle.is_finished()) {
                return;
            }
            timer.take();
        }
        let source = self.source.clone();
        let ended = self.ended.clone();
        let destroyed = self.destroyed.clone();
        let buffer = self.buffer.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(RESUME_LIVENESS_MS)).await;
            if buffer.lock().bytes >= MAX_BUFFERED_BYTES {
                return;
            }
            if !ended.load(Ordering::SeqCst) && !destroyed.load(Ordering::SeqCst) {
                source.lock().resume();
            }
        });
        *self.resume_timer.lock() = Some(handle);
    }

    fn clear_resume(&self) {
        if let Some(timer) = self.resume_timer.lock().take() {
            timer.abort();
        }
    }
}

impl Stream for BufferedReadable {
    type Item = Result<Vec<u8>, io::Error>;

    fn poll_next(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.read();
        if let Some(error) = self.error.lock().take() {
            return Poll::Ready(Some(Err(error)));
        }
        if self.buffer.lock().bytes >= MAX_BUFFERED_BYTES {
            self.source.lock().pause();
        }
        if let Some(chunk) = {
            let mut buffer = self.buffer.lock();
            let chunk = buffer.chunks.pop_front();
            if let Some(chunk) = &chunk {
                buffer.bytes -= chunk.len();
            }
            chunk
        } {
            return Poll::Ready(Some(Ok(chunk)));
        }
        if self.ended.load(Ordering::SeqCst) || self.destroyed.load(Ordering::SeqCst) {
            return Poll::Ready(None);
        }
        self.source.lock().as_mut().poll_next(context)
    }
}

impl Drop for BufferedReadable {
    fn drop(&mut self) {
        self.clear_resume();
        self.source.lock().destroy();
    }
}
