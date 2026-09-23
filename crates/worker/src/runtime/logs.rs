//! Per-invocation stdout/stderr sinks. Overflow is discarded, never backpressure
//! or a guest I/O error. Keep ownership outside the execution timeout future.
use hibana_shared::application_logs::{ApplicationLogs, MAX_LOG_BYTES};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::io::{self, AsyncWrite};
use wasmtime_wasi::{
    cli::{IsTerminal, StdoutStream},
    p2::{OutputStream, Pollable, StreamError},
};

#[derive(Default)]
struct Buffer {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    dropped: u64,
}

#[derive(Clone, Default)]
pub(crate) struct Capture(Arc<Mutex<Buffer>>);

impl Capture {
    pub(crate) fn stream(&self, stderr: bool) -> LogStream {
        LogStream {
            capture: self.clone(),
            stderr,
        }
    }

    pub(crate) fn snapshot(&self) -> ApplicationLogs {
        let buffer = self.0.lock().unwrap();
        ApplicationLogs {
            stdout: String::from_utf8_lossy(&buffer.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&buffer.stderr).into_owned(),
            truncated: buffer.dropped > 0,
        }
        .bounded()
    }

    pub(crate) fn dropped_bytes(&self) -> u64 {
        self.0.lock().unwrap().dropped
    }
}

#[derive(Clone)]
pub(crate) struct LogStream {
    capture: Capture,
    stderr: bool,
}

impl LogStream {
    fn append(&self, bytes: &[u8]) {
        let mut buffer = self.capture.0.lock().unwrap();
        let available = MAX_LOG_BYTES - buffer.stdout.len() - buffer.stderr.len();
        let take = bytes.len().min(available);
        buffer.dropped = buffer.dropped.saturating_add((bytes.len() - take) as u64);
        let target = if self.stderr {
            &mut buffer.stderr
        } else {
            &mut buffer.stdout
        };
        target.extend_from_slice(&bytes[..take]);
    }
}

impl IsTerminal for LogStream {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for LogStream {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl Pollable for LogStream {
    async fn ready(&mut self) {}
}

#[async_trait::async_trait]
impl OutputStream for LogStream {
    fn write(&mut self, bytes: bytes::Bytes) -> Result<(), StreamError> {
        self.append(&bytes);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), StreamError> {
        Ok(())
    }
    fn check_write(&mut self) -> Result<usize, StreamError> {
        Ok(4096)
    }
}

impl AsyncWrite for LogStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.append(bytes);
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_survives_cancellation_and_overflow_does_not_fail_the_guest() {
        let capture = Capture::default();
        let writer = capture.clone();
        let task = tokio::spawn(async move {
            let mut stdout = writer.stream(false);
            let mut stderr = writer.stream(true);
            // UTF-8 can be split across writes.
            stdout.write(bytes::Bytes::from_static(&[0xe9])).unwrap();
            stdout
                .write(bytes::Bytes::from_static(&[0x9b, 0xaa]))
                .unwrap();
            stderr.write(bytes::Bytes::from_static(b"error\n")).unwrap();
            for _ in 0..10 {
                stdout.write(bytes::Bytes::from(vec![b'x'; 4096])).unwrap();
            }
            assert!(stdout.check_write().unwrap() > 0);
            stdout.flush().unwrap();
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        let logs = capture.snapshot();
        assert!(logs.stdout.starts_with("雪"));
        assert_eq!(logs.stderr, "error\n");
        assert_eq!(logs.stdout.len() + logs.stderr.len(), MAX_LOG_BYTES);
        assert!(logs.truncated);
        assert!(capture.dropped_bytes() > 0);
        assert_eq!(Capture::default().snapshot(), ApplicationLogs::default());
    }
}
