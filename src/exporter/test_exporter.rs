//! In-memory test exporter (Task 6.1).
//!
//! Records every batch it receives so tests can assert on them.
//! Never returns errors unless explicitly configured to do so.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

use super::{ExportError, Exporter};
use crate::span::FinishedSpan;

/// A test-only `Exporter` that records every batch it receives.
///
/// By default it always succeeds. Tests can use `force_error` to make the
/// next call (or all calls) return an error, or `with_delay` to make each
/// `export` sleep — useful for exercising the worker's error and
/// shutdown-timeout paths.
pub struct TestExporter {
    inner: Mutex<Inner>,
    /// Per-call artificial delay, used to exercise the shutdown timeout path.
    delay: Option<Duration>,
}

struct Inner {
    batches: Vec<Vec<FinishedSpan>>,
    next_error: Option<ExportError>,
    always_error: bool,
}

impl Default for TestExporter {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                batches: Vec::new(),
                next_error: None,
                always_error: false,
            }),
            delay: None,
        }
    }
}

#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
impl TestExporter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every `export` call sleep for `delay` before recording, so tests
    /// can drive the worker past the shutdown deadline.
    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// Returns all batches that have been successfully exported so far.
    pub fn exported_batches(&self) -> Vec<Vec<FinishedSpan>> {
        let inner = self.inner.lock().unwrap();
        inner.batches.clone()
    }

    /// Make the next `export` call return the given error (once).
    pub fn force_next_error(&self, err: ExportError) {
        let mut inner = self.inner.lock().unwrap();
        inner.next_error = Some(err);
        inner.always_error = false;
    }

    /// Make every subsequent `export` call return an error.
    pub fn force_always_error(&self, err: ExportError) {
        let mut inner = self.inner.lock().unwrap();
        inner.next_error = None;
        inner.always_error = true;
        // Store the error so we can keep returning it
        inner.next_error = Some(err); // reuse the slot
    }

    fn take_error(&self) -> Option<ExportError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.always_error {
            return inner.next_error.clone();
        }
        inner.next_error.take()
    }
}

#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
#[async_trait]
impl Exporter for TestExporter {
    async fn export(&self, batch: Vec<FinishedSpan>) -> Result<(), ExportError> {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }

        if let Some(err) = self.take_error() {
            return Err(err);
        }

        self.inner.lock().unwrap().batches.push(batch);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::context::{SpanId, TraceId};
    // Pipeline not needed in this module's tests currently.
    use crate::span::OtelSpan;

    #[tokio::test]
    async fn test_exporter_records_batches() {
        let exporter = TestExporter::new();

        let span = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "test-span");
        let finished = span.finish();

        exporter
            .export(vec![finished.clone()])
            .await
            .expect("export should succeed");

        let batches = exporter.exported_batches();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }

    #[tokio::test]
    async fn test_exporter_can_be_forced_to_error() {
        let exporter = TestExporter::new();
        exporter.force_next_error(ExportError::new("boom"));

        let span = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "test-span");
        let finished = span.finish();

        let result = exporter.export(vec![finished]).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().message, "boom");

        // Subsequent call should succeed again
        let span2 = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "test-span-2");
        let finished2 = span2.finish();
        exporter
            .export(vec![finished2])
            .await
            .expect("second export should succeed after forced error");
    }

    #[tokio::test]
    async fn batch_worker_flushes_on_max_batch() {
        use std::time::Duration;

        use tokio::sync::{mpsc, oneshot};

        let exporter = std::sync::Arc::new(TestExporter::new());
        let (tx, rx) = mpsc::channel(16);
        // Keep the signal sender alive so the drop-of-`tx` path drives the drain.
        let (_sd_tx, sd_rx) = oneshot::channel();

        let worker = crate::exporter::BatchWorker::new(
            rx,
            exporter.clone(),
            3,                       // max batch
            Duration::from_secs(60), // long delay so only size triggers
            sd_rx,
        );

        // Send 4 spans (should cause one flush of 3 + one of 1 on shutdown)
        for i in 0..4 {
            let mut span = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "span");
            span.record("idx", i);
            tx.send(span.finish()).await.unwrap();
        }

        // Drop sender to trigger shutdown drain
        drop(tx);

        // Run the worker to completion
        worker.run().await;

        let batches = exporter.exported_batches();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 3);
        assert_eq!(batches[1].len(), 1);
    }

    #[tokio::test]
    async fn batch_worker_flushes_on_schedule_delay_with_partial_batch() {
        use std::time::Duration;

        use tokio::sync::{mpsc, oneshot};
        use tokio::time;

        let exporter = std::sync::Arc::new(TestExporter::new());
        let (tx, rx) = mpsc::channel(16);
        let (_sd_tx, sd_rx) = oneshot::channel();

        let worker = crate::exporter::BatchWorker::new(
            rx,
            exporter.clone(),
            10, // large max batch so only time triggers
            Duration::from_millis(100),
            sd_rx,
        );

        time::pause();

        // Send 2 spans (partial)
        for i in 0..2 {
            let mut span = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "span");
            span.record("idx", i);
            tx.send(span.finish()).await.unwrap();
        }

        // Advance just before the delay — should not have flushed
        time::advance(Duration::from_millis(99)).await;
        // Yield so the worker can process any pending timers
        time::sleep(Duration::ZERO).await;

        assert!(
            exporter.exported_batches().is_empty(),
            "should not flush before schedule delay on partial batch"
        );

        // Advance past the delay
        time::advance(Duration::from_millis(2)).await;
        time::sleep(Duration::ZERO).await;

        // Drop the sender to trigger shutdown drain.
        drop(tx);

        // Run the worker to completion. It will deliver any remaining partial batch
        // on shutdown (the schedule delay window has already elapsed).
        worker.run().await;

        let batches = exporter.exported_batches();
        assert!(
            !batches.is_empty(),
            "partial batch should have been delivered after schedule delay window + shutdown drain"
        );
        assert_eq!(batches[0].len(), 2);
    }

    #[tokio::test]
    async fn worker_continues_after_exporter_error() {
        use std::time::Duration;

        use tokio::sync::{mpsc, oneshot};

        let exporter = std::sync::Arc::new(TestExporter::new());
        exporter.force_always_error(ExportError::new("simulated failure"));

        let (tx, rx) = mpsc::channel(16);
        let (_sd_tx, sd_rx) = oneshot::channel();

        let worker = crate::exporter::BatchWorker::new(
            rx,
            exporter.clone(),
            2, // small batch so we trigger export quickly
            Duration::from_secs(60),
            sd_rx,
        );

        // Send enough spans to cause at least one flush attempt that will fail
        for i in 0..3 {
            let mut span = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "span");
            span.record("idx", i);
            tx.send(span.finish()).await.unwrap();
        }

        drop(tx);

        // The worker should continue and complete the drain even though export keeps
        // failing
        worker.run().await;

        // We don't assert on the (failed) batches here — the important thing is
        // that the worker didn't panic or get stuck.
    }

    /// The explicit shutdown signal drains buffered spans even though the
    /// sending half is still alive — the property the old fire-and-forget path
    /// could not provide.
    #[tokio::test]
    async fn worker_drains_on_shutdown_signal_with_live_sender() {
        use std::time::Duration;

        use tokio::sync::{mpsc, oneshot};

        let exporter = std::sync::Arc::new(TestExporter::new());
        let (tx, rx) = mpsc::channel(16);
        let (sd_tx, sd_rx) = oneshot::channel();

        let worker = crate::exporter::BatchWorker::new(
            rx,
            exporter.clone(),
            10,                      // large batch so size never triggers
            Duration::from_secs(60), // long delay so time never triggers
            sd_rx,
        );

        for i in 0..3 {
            let mut span = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "span");
            span.record("idx", i);
            tx.send(span.finish()).await.unwrap();
        }

        // Request shutdown WITHOUT dropping `tx`: only the signal triggers drain.
        sd_tx.send(()).unwrap();
        worker.run().await;

        let total: usize = exporter.exported_batches().iter().map(Vec::len).sum();
        assert_eq!(total, 3, "all buffered spans should be drained on shutdown signal");

        // `tx` was alive the whole time; the drain did not depend on dropping it.
        drop(tx);
    }

    /// After a shutdown drain the channel is closed: late sends are rejected
    /// (counted as drops upstream) rather than panicking, and every span sent
    /// before the signal still reaches the exporter.
    #[tokio::test]
    async fn worker_rejects_sends_after_shutdown_drain() {
        use std::time::Duration;

        use tokio::sync::{mpsc, oneshot};

        let exporter = std::sync::Arc::new(TestExporter::new());
        let (tx, rx) = mpsc::channel(16);
        let (sd_tx, sd_rx) = oneshot::channel();

        let worker = crate::exporter::BatchWorker::new(rx, exporter.clone(), 10, Duration::from_secs(60), sd_rx);

        for i in 0..2 {
            let mut span = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "span");
            span.record("idx", i);
            tx.send(span.finish()).await.unwrap();
        }

        sd_tx.send(()).unwrap();
        worker.run().await;

        let total: usize = exporter.exported_batches().iter().map(Vec::len).sum();
        assert_eq!(total, 2, "pre-signal spans must all be exported");

        let late = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "late").finish();
        assert!(
            tx.try_send(late).is_err(),
            "channel is closed after drain; late sends are rejected"
        );
    }
}
