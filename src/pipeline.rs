//! The shared export pipeline (configuration, batching, exporter worker).
//!
//! `Tracer` and `OtelLayer` both hold an `Arc<Pipeline>` so that HTTP root
//! spans created by the Tower middleware and child spans observed by the
//! subscriber layer participate in the same trace and share exporter state.
//!
//! Production send is non-blocking (bounded tokio mpsc + `try_send`); excess
//! spans are dropped and counted when the channel is full. The receiver side
//! will be drained by a background worker task in later phases.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::mpsc::{self as mpsc, Receiver, Sender};

use tokio::sync::mpsc as tokio_mpsc;

use crate::config::{Config, Endpoint, ExporterKind};
use crate::exporter::{BatchWorker, Exporter, OtlpHttpExporter, ZipkinExporter};
// Re-export the real type from span.rs now that it is defined (Task 2.1).
pub use crate::span::FinishedSpan;

/// The shared pipeline state.
///
/// Cloning the handle is cheap (Arc). The production path uses a bounded
/// tokio mpsc channel with non-blocking `try_send`; on full we drop the span
/// and increment `drop_count` (never blocks the hot tracing path).
#[derive(Debug)]
pub struct Pipeline {
    pub config: Config,
    drop_count: AtomicUsize,

    /// Production bounded sender (Some when enabled). The matching receiver is
    /// owned by the spawned `BatchWorker`; when this sender drops, the worker
    /// drains any buffered spans and exits (flush-on-drop).
    sender: Option<tokio_mpsc::Sender<FinishedSpan>>,

    // For Task 3.2 test support (separate std mpsc so plain #[test] can recv easily)
    #[cfg(test)]
    test_sender: Option<Sender<FinishedSpan>>,
}

impl Pipeline {
    /// Primary constructor used by `Tracer` and `TracerBuilder`. It is the
    /// official integration point with the fully resolved `Config` from
    /// Task 4.1, and (Task 6.2) the point where the background exporter worker
    /// is started.
    ///
    /// **Runtime requirement:** to actually export, this must be called from
    /// within a Tokio runtime (the normal `#[tokio::main]` axum case). When no
    /// runtime is present at construction the pipeline still builds, but no
    /// worker is spawned and produced spans are dropped-and-counted. The worker
    /// drains and exits when this pipeline (and thus the sender) is dropped.
    pub(crate) fn from_config(config: Config) -> Arc<Self> {
        if !config.is_enabled() {
            return Arc::new(Self {
                config,
                drop_count: AtomicUsize::new(0),
                sender: None,
                #[cfg(test)]
                test_sender: None,
            });
        }

        // Bounded channel: hot path never blocks. Capacity allows reasonable
        // buffering under burst without unbounded memory growth.
        let (tx, rx) = tokio_mpsc::channel::<FinishedSpan>(2048);

        match build_exporter(&config) {
            Some(exporter) => match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let worker = BatchWorker::new(rx, exporter, config.max_export_batch_size, config.schedule_delay);
                    // Detached: dropping the JoinHandle does not cancel the task;
                    // it runs until the channel closes, then drains and exits.
                    handle.spawn(worker.run());
                }
                Err(_) => {
                    // `rx` drops here, closing the channel; sends will count as
                    // drops. Surfaced once so misconfiguration is visible.
                    tracing::debug!(
                        target: "auspex::exporter",
                        "no Tokio runtime at pipeline construction; exporter worker not started (spans will be dropped)"
                    );
                }
            },
            None => {
                tracing::warn!(
                    target: "auspex::exporter",
                    "no usable exporter for the resolved config; export disabled (spans will be dropped)"
                );
            }
        }

        Arc::new(Self {
            config,
            drop_count: AtomicUsize::new(0),
            sender: Some(tx),
            #[cfg(test)]
            test_sender: None,
        })
    }

    /// Test helper: create a pipeline + receiver for inspecting sent
    /// `FinishedSpan`s. Uses the separate test (std mpsc) path so
    /// synchronous tests remain simple.
    #[cfg(test)]
    pub(crate) fn new_for_test(config: Config) -> (Arc<Self>, Receiver<FinishedSpan>) {
        let (tx, rx) = mpsc::channel();
        let pipeline = Arc::new(Self {
            config,
            drop_count: AtomicUsize::new(0),
            sender: None,
            test_sender: Some(tx),
        });
        (pipeline, rx)
    }

    /// Test-only accessor so precedence and other builder tests can
    /// observe the exact Config that was passed to `from_config`.
    #[cfg(test)]
    pub(crate) const fn config_for_test(&self) -> &Config {
        &self.config
    }

    /// Test helper for exercising the *production* bounded channel +
    /// drop-on-full behavior. Returns the tokio receiver so the test can
    /// observe accepted spans.
    #[cfg(test)]
    pub(crate) fn new_for_production_test(
        config: Config,
        capacity: usize,
    ) -> (Arc<Self>, tokio_mpsc::Receiver<FinishedSpan>) {
        let (tx, rx) = tokio_mpsc::channel::<FinishedSpan>(capacity);
        let pipeline = Arc::new(Self {
            config,
            drop_count: AtomicUsize::new(0),
            sender: Some(tx),
            #[cfg(test)]
            test_sender: None,
        });
        (pipeline, rx)
    }

    /// Returns whether exporting is enabled according to the resolved config.
    #[inline]
    pub(crate) const fn is_enabled(&self) -> bool {
        self.config.is_enabled()
    }

    /// Number of spans dropped because the export channel was full.
    /// Incremented under backpressure (never blocks callers).
    #[inline]
    #[allow(dead_code)] // exercised in tests today; will be used by exporter stats later
    pub(crate) fn dropped_count(&self) -> usize {
        self.drop_count.load(Ordering::Relaxed)
    }

    /// Attempt to send a finished span to the export pipeline.
    ///
    /// Production: non-blocking `try_send` on bounded tokio channel. On full,
    /// increment drop counter and drop the span (hot path must never block).
    /// Test path uses a separate std channel for ergonomic synchronous tests.
    pub(crate) fn send(&self, span: FinishedSpan) {
        if !self.is_enabled() {
            return;
        }

        #[cfg(test)]
        if let Some(sender) = &self.test_sender {
            // Test path: blocking send is acceptable (dedicated test thread).
            let _ = sender.send(span);
            return;
        }

        // Production path — the only one that matters for real usage.
        if let Some(tx) = &self.sender {
            if tx.try_send(span).is_err() {
                self.drop_count.fetch_add(1, Ordering::Relaxed);
                // Intentionally no logging on every drop (would be too noisy
                // under load). Counter is the observable; a
                // future sampling debug log can be added.
            }
        } else {
            // Should not happen when enabled, but be defensive.
            self.drop_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Test helper: create a production-style pipeline that immediately spawns
    /// a `BatchWorker` using the provided exporter. This is the foundation for
    /// the real end-to-end named tests in Task 6.1.
    #[cfg(test)]
    pub(crate) fn new_for_test_with_exporter(
        config: Config,
        exporter: Arc<dyn Exporter>,
    ) -> (Arc<Self>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = if config.is_enabled() {
            tokio::sync::mpsc::channel::<FinishedSpan>(2048)
        } else {
            // For disabled case we still return a no-op pipeline + a dummy task
            let (_tx, _rx) = tokio::sync::mpsc::channel::<FinishedSpan>(1);
            let pipeline = Arc::new(Self {
                config,
                drop_count: AtomicUsize::new(0),
                sender: None,
                #[cfg(test)]
                test_sender: None,
            });
            let handle = tokio::spawn(async {});
            return (pipeline, handle);
        };

        let pipeline = Arc::new(Self {
            config: config.clone(),
            drop_count: AtomicUsize::new(0),
            sender: Some(tx),
            #[cfg(test)]
            test_sender: None,
        });

        let worker = BatchWorker::new(rx, exporter, config.max_export_batch_size, config.schedule_delay);

        let handle = tokio::spawn(worker.run());

        (pipeline, handle)
    }
}

/// Construct the concrete exporter for the resolved config, if one is
/// available.
///
/// Returns `None` (export disabled) when the configured exporter has no
/// implementation yet (Zipkin — Task 6.3) or when building it fails (e.g. an
/// invalid configured header). Construction failure is logged, not fatal: the
/// application keeps running with export disabled.
fn build_exporter(config: &Config) -> Option<Arc<dyn Exporter>> {
    match (&config.exporter, config.endpoint.as_ref()) {
        (ExporterKind::Otlp, Some(Endpoint::OtlpHttp(url))) => {
            match OtlpHttpExporter::try_new(
                url.clone(),
                &config.headers,
                config.service_name.clone(),
                config.resource_attributes.clone(),
            ) {
                Ok(exporter) => Some(Arc::new(exporter)),
                Err(err) => {
                    tracing::warn!(
                        target: "auspex::exporter",
                        %err,
                        "failed to build OTLP/HTTP exporter; export disabled"
                    );
                    None
                }
            }
        }
        (ExporterKind::Zipkin, Some(Endpoint::Zipkin(url))) => {
            match ZipkinExporter::try_new(
                url.clone(),
                &config.headers,
                config.service_name.clone(),
                config.resource_attributes.clone(),
            ) {
                Ok(exporter) => Some(Arc::new(exporter)),
                Err(err) => {
                    tracing::warn!(
                        target: "auspex::exporter",
                        %err,
                        "failed to build Zipkin exporter; export disabled"
                    );
                    None
                }
            }
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::used_underscore_binding)]
mod tests {
    use super::*;

    #[tokio::test]
    #[allow(clippy::used_underscore_binding)]
    async fn end_to_end_finished_span_reaches_test_exporter() {
        use crate::context::{SpanId, TraceId};
        // TestExporter is qualified below to avoid unused import warnings in non-test builds.
        use crate::span::OtelSpan;

        let config = Config::default()
            .with_service_name("e2e-test")
            .with_sink_uri("http://example.com"); // enabled

        let test_exporter = Arc::new(crate::exporter::TestExporter::new());

        let (pipeline, _worker_handle) = Pipeline::new_for_test_with_exporter(config, test_exporter.clone());

        // Create and "finish" a real span the normal way
        let live = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "e2e-span");
        let finished = live.finish();

        // Send it through the normal hot path
        pipeline.send(finished);

        // Drop the pipeline (and thus the sender). This triggers the worker's
        // shutdown drain path, which should deliver the span to the exporter.
        drop(pipeline);

        // Wait for the worker to finish draining and exit.
        _worker_handle.await.expect("worker task should not panic");

        let batches = test_exporter.exported_batches();
        assert!(
            !batches.is_empty(),
            "expected at least one batch to have reached the TestExporter via shutdown drain"
        );
        assert!(!batches[0].is_empty());
    }

    /// The production `from_config` path spawns a worker that POSTs spans to
    /// the resolved OTLP endpoint, and flushes them when the pipeline drops.
    #[tokio::test]
    async fn from_config_spawns_worker_and_exports_to_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use crate::context::{SpanId, TraceId};
        use crate::span::OtelSpan;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/traces"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let config = Config::default()
            .with_service_name("e2e")
            .with_sink_uri(format!("{}/v1/traces", server.uri()));

        let pipeline = Pipeline::from_config(config);
        assert!(pipeline.is_enabled(), "OTLP sink should enable the pipeline");

        let finished = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "prod-span").finish();
        pipeline.send(finished);

        // Dropping closes the channel; the worker drains the buffered span and
        // POSTs it. The worker runs detached, so poll the server briefly.
        drop(pipeline);

        let mut exported = false;
        for _ in 0..50 {
            if let Some(reqs) = server.received_requests().await
                && !reqs.is_empty()
            {
                exported = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(exported, "the spawned worker should POST the span to the endpoint");
    }

    /// The production path also works for a Zipkin sink: `from_config` builds a
    /// `ZipkinExporter` and the worker POSTs JSON to `/api/v2/spans`.
    #[tokio::test]
    async fn from_config_spawns_zipkin_worker_and_exports() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use crate::context::{SpanId, TraceId};
        use crate::span::OtelSpan;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/spans"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        // `server.uri()` is http://127.0.0.1:PORT; the zipkin+ prefix selects the
        // Zipkin exporter and must be stripped to a usable http scheme.
        let config = Config::default()
            .with_service_name("zk-e2e")
            .with_sink_uri(format!("zipkin+{}", server.uri()));

        let pipeline = Pipeline::from_config(config);
        assert!(pipeline.is_enabled(), "zipkin sink should enable the pipeline");

        let finished = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "zk-span").finish();
        pipeline.send(finished);
        drop(pipeline);

        let mut exported = false;
        for _ in 0..50 {
            if let Some(reqs) = server.received_requests().await
                && !reqs.is_empty()
            {
                exported = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(exported, "the spawned zipkin worker should POST to /api/v2/spans");
    }

    #[test]
    fn pipeline_from_config_respects_enabled() {
        let disabled = Config::default();
        let p = Pipeline::from_config(disabled);
        assert!(!p.is_enabled());

        let enabled = Config::default()
            .with_service_name("svc")
            .with_sink_uri("http://localhost:4318");
        let p2 = Pipeline::from_config(enabled);
        assert!(p2.is_enabled());
    }

    #[test]
    fn send_is_noop_when_disabled() {
        use crate::context::{SpanId, TraceId};
        use crate::span::OtelSpan;

        let p = Pipeline::from_config(Config::default());

        // Create a minimal real FinishedSpan via OtelSpan (Task 2.1)
        let live = OtelSpan::new(TraceId::ZERO, SpanId::ZERO, None, "test");
        let finished = live.finish();

        p.send(finished); // does not panic or allocate (early return)
    }

    #[test]
    fn test_pipeline_receives_finished_spans() {
        use crate::context::{SpanId, TraceId};
        use crate::span::OtelSpan;

        let config = Config::default()
            .with_service_name("test-svc")
            .with_sink_uri("http://localhost:4318");

        let (pipeline, receiver) = Pipeline::new_for_test(config);

        // Simulate finishing a span
        let live = OtelSpan::new(TraceId::ZERO, SpanId::ZERO, None, "test-span");
        let finished = live.finish();

        pipeline.send(finished);

        // In Task 3.2 we can now observe the sent span
        let received = receiver.recv().expect("should have received a FinishedSpan");
        assert_eq!(received.name, "test-span");
    }

    #[test]
    fn production_channel_drops_and_counts_on_full() {
        use crate::context::{SpanId, TraceId};
        use crate::span::OtelSpan;

        let config = Config::default()
            .with_service_name("bp-test")
            .with_sink_uri("http://localhost:4318");

        // Tiny bounded production channel (exercises the real tokio try_send path)
        let (pipeline, mut rx) = Pipeline::new_for_production_test(config, 1);

        // Helper to make distinct minimal finished spans.
        // Use to_string() so the &str (even if literal) satisfies Cow<'static, str> in
        // OtelSpan.
        let make_span = |name: &str| {
            let live = OtelSpan::new(TraceId::ZERO, SpanId::ZERO, None, name.to_string());
            live.finish()
        };

        // First send should be accepted (buffer of 1)
        pipeline.send(make_span("span-1"));
        assert_eq!(pipeline.dropped_count(), 0);

        // Second send: channel full → drop + counter
        pipeline.send(make_span("span-2"));
        assert_eq!(pipeline.dropped_count(), 1);

        // Third also dropped (still at capacity)
        pipeline.send(make_span("span-3"));
        assert_eq!(pipeline.dropped_count(), 2);

        // The accepted one can be observed via the returned receiver
        let first = rx.try_recv().expect("one span should have been buffered");
        assert_eq!(first.name, "span-1");

        // Channel is now empty (space freed)
        assert!(rx.try_recv().is_err(), "channel should now be empty after drain");

        // A new send now succeeds because we drained
        pipeline.send(make_span("span-4"));
        assert_eq!(pipeline.dropped_count(), 2, "drain allowed 4th to be accepted");

        // If we fill it again without draining, drops resume
        pipeline.send(make_span("span-5"));
        assert_eq!(pipeline.dropped_count(), 3);
    }
}
