//! The shared export pipeline (configuration, batching, exporter worker).
//!
//! `Tracer` and `OtelLayer` both hold an `Arc<Pipeline>` so that HTTP root
//! spans created by the Tower middleware and child spans observed by the
//! subscriber layer participate in the same trace and share exporter state.
//!
//! Production send is non-blocking (bounded tokio mpsc + `try_send`); excess
//! spans are dropped and counted when the channel is full. The receiver side
//! will be drained by a background worker task in later phases.

use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::mpsc::{self as mpsc, Receiver, Sender};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config::{Config, Endpoint, ExporterKind};
use crate::exporter::{BatchWorker, Exporter, OtlpHttpExporter, ZipkinExporter};
// Re-export the real type from span.rs now that it is defined (Task 2.1).
pub use crate::span::FinishedSpan;

/// Shutdown coordination, populated only when an exporter worker was spawned.
///
/// Held behind a `Mutex` that is touched *only* by [`Pipeline::shutdown`], so
/// the hot-path `sender` stays lock-free. `take`-ing the state makes shutdown
/// idempotent.
#[derive(Debug)]
struct ShutdownState {
    /// Signals the worker to stop accepting spans and drain. Dropping it (full
    /// pipeline drop) triggers the same drain via the receiver erroring.
    signal: oneshot::Sender<()>,
    /// The spawned worker task; awaited (with a timeout) so callers can wait
    /// for the final export to complete.
    worker: JoinHandle<()>,
}

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

    /// Shutdown coordination (`Some` only when a worker was actually spawned).
    /// Touched only by `shutdown()`, never on the hot path.
    shutdown: Mutex<Option<ShutdownState>>,

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
    /// worker is spawned and produced spans are dropped-and-counted.
    ///
    /// **Shutdown:** prefer the deterministic [`Pipeline::shutdown`] (exposed
    /// as `Tracer::shutdown`), which signals the worker to drain and awaits
    /// the final export. Dropping the pipeline remains a best-effort
    /// fallback: the channel closes and the detached worker drains, but
    /// nothing waits for it.
    pub(crate) fn from_config(config: Config) -> Arc<Self> {
        if !config.is_enabled() {
            return Arc::new(Self {
                config,
                drop_count: AtomicUsize::new(0),
                sender: None,
                shutdown: Mutex::new(None),
                #[cfg(test)]
                test_sender: None,
            });
        }

        // Bounded channel: hot path never blocks. Capacity allows reasonable
        // buffering under burst without unbounded memory growth.
        let (tx, rx) = tokio_mpsc::channel::<FinishedSpan>(2048);

        let shutdown = if let Some(exporter) = build_exporter(&config) {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let (signal, shutdown_rx) = oneshot::channel();
                let worker = BatchWorker::new(
                    rx,
                    exporter,
                    config.max_export_batch_size,
                    config.schedule_delay,
                    shutdown_rx,
                );
                // The worker drains on an explicit `shutdown()` signal, or when the
                // channel closes on full drop. Keep its handle so `shutdown()` can
                // await the final export.
                let worker = handle.spawn(worker.run());
                Some(ShutdownState { signal, worker })
            } else {
                // `rx` drops at function end, closing the channel; sends will count
                // as drops. Surfaced once so misconfiguration is visible.
                tracing::debug!(
                    target: "auspex::exporter",
                    "no Tokio runtime at pipeline construction; exporter worker not started (spans will be dropped)"
                );
                None
            }
        } else {
            tracing::warn!(
                target: "auspex::exporter",
                "no usable exporter for the resolved config; export disabled (spans will be dropped)"
            );
            None
        };

        Arc::new(Self {
            config,
            drop_count: AtomicUsize::new(0),
            sender: Some(tx),
            shutdown: Mutex::new(shutdown),
            #[cfg(test)]
            test_sender: None,
        })
    }

    /// Drain and export buffered spans, then wait for the worker to finish.
    ///
    /// Returns `true` if everything flushed within `config.shutdown_timeout`;
    /// `false` if the deadline elapsed or the worker had panicked. Idempotent
    /// (the shutdown state is taken on first call); a no-op returning `true`
    /// when no worker was spawned (disabled, or no runtime at construction).
    pub(crate) async fn shutdown(&self) -> bool {
        let state = self
            .shutdown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(ShutdownState { signal, worker }) = state else {
            return true;
        };

        // Err means the worker already exited; the drop path will have drained.
        let _ = signal.send(());

        match tokio::time::timeout(self.config.shutdown_timeout, worker).await {
            Ok(Ok(())) => true,
            Ok(Err(_join_err)) => {
                tracing::warn!(target: "auspex::exporter", "export worker panicked during shutdown");
                false
            }
            Err(_elapsed) => {
                tracing::warn!(
                    target: "auspex::exporter",
                    "shutdown drain exceeded shutdown_timeout; some spans may not have been exported"
                );
                false
            }
        }
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
            shutdown: Mutex::new(None),
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
            shutdown: Mutex::new(None),
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
    /// a `BatchWorker` using the provided exporter, with the shutdown state
    /// wired up so tests can drive `pipeline.shutdown()` directly.
    #[cfg(test)]
    pub(crate) fn new_for_test_with_exporter(config: Config, exporter: Arc<dyn Exporter>) -> Arc<Self> {
        if !config.is_enabled() {
            return Arc::new(Self {
                config,
                drop_count: AtomicUsize::new(0),
                sender: None,
                shutdown: Mutex::new(None),
                test_sender: None,
            });
        }

        let (tx, rx) = tokio_mpsc::channel::<FinishedSpan>(2048);
        let (signal, shutdown_rx) = oneshot::channel();
        let worker = BatchWorker::new(
            rx,
            exporter,
            config.max_export_batch_size,
            config.schedule_delay,
            shutdown_rx,
        );
        let worker = tokio::spawn(worker.run());

        Arc::new(Self {
            config,
            drop_count: AtomicUsize::new(0),
            sender: Some(tx),
            shutdown: Mutex::new(Some(ShutdownState { signal, worker })),
            test_sender: None,
        })
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
    async fn end_to_end_finished_span_reaches_test_exporter() {
        use crate::context::{SpanId, TraceId};
        // TestExporter is qualified below to avoid unused import warnings in non-test builds.
        use crate::span::OtelSpan;

        let config = Config::default()
            .with_service_name("e2e-test")
            .with_sink_uri("http://example.com"); // enabled

        let test_exporter = Arc::new(crate::exporter::TestExporter::new());

        let pipeline = Pipeline::new_for_test_with_exporter(config, test_exporter.clone());

        // Create and "finish" a real span the normal way
        let live = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "e2e-span");
        let finished = live.finish();

        // Send it through the normal hot path
        pipeline.send(finished);

        // Explicit shutdown drains and exports synchronously, then returns.
        assert!(
            pipeline.shutdown().await,
            "shutdown should drain cleanly within the timeout"
        );

        let batches = test_exporter.exported_batches();
        assert!(
            !batches.is_empty(),
            "expected at least one batch to have reached the TestExporter via shutdown drain"
        );
        assert!(!batches[0].is_empty());
    }

    /// `shutdown()` is idempotent: a second call is a harmless no-op that still
    /// reports a clean result.
    #[tokio::test]
    async fn shutdown_is_idempotent() {
        use crate::context::{SpanId, TraceId};
        use crate::span::OtelSpan;

        let config = Config::default()
            .with_service_name("idem")
            .with_sink_uri("http://example.com");
        let test_exporter = Arc::new(crate::exporter::TestExporter::new());
        let pipeline = Pipeline::new_for_test_with_exporter(config, test_exporter);

        pipeline.send(OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s").finish());

        assert!(pipeline.shutdown().await, "first shutdown drains cleanly");
        assert!(pipeline.shutdown().await, "second shutdown is a no-op returning true");
    }

    /// `shutdown()` on a disabled pipeline returns immediately (no worker to
    /// await, no hang).
    #[tokio::test]
    async fn shutdown_on_disabled_pipeline_returns_true() {
        let pipeline = Pipeline::from_config(Config::default());
        assert!(!pipeline.is_enabled());
        assert!(
            pipeline.shutdown().await,
            "disabled pipeline shutdown is an immediate no-op"
        );
    }

    /// When the worker cannot finish exporting within `shutdown_timeout`,
    /// `shutdown()` reports `false` (deadline hit) rather than blocking
    /// forever.
    #[tokio::test]
    async fn shutdown_times_out_when_export_is_too_slow() {
        use std::time::Duration;

        use crate::context::{SpanId, TraceId};
        use crate::span::OtelSpan;

        let mut config = Config::default()
            .with_service_name("slow")
            .with_sink_uri("http://example.com");
        config.shutdown_timeout = Duration::from_millis(50);

        // Exporter that sleeps far longer than the shutdown deadline.
        let test_exporter = Arc::new(crate::exporter::TestExporter::new().with_delay(Duration::from_secs(30)));
        let pipeline = Pipeline::new_for_test_with_exporter(config, test_exporter);

        pipeline.send(OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s").finish());

        assert!(
            !pipeline.shutdown().await,
            "shutdown should report false when the drain exceeds shutdown_timeout"
        );
    }

    /// The production `from_config` path spawns a worker that POSTs spans to
    /// the resolved OTLP endpoint; `shutdown()` drains it deterministically.
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

        // shutdown() closes the channel, drains the buffered span, awaits the
        // POST, and returns — no polling needed.
        assert!(
            pipeline.shutdown().await,
            "shutdown should drain and export within the timeout"
        );

        let reqs = server.received_requests().await.expect("mock server records requests");
        assert!(
            !reqs.is_empty(),
            "the worker should have POSTed the span to the endpoint"
        );
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

        assert!(
            pipeline.shutdown().await,
            "zipkin shutdown should drain and export within the timeout"
        );

        let reqs = server.received_requests().await.expect("mock server records requests");
        assert!(
            !reqs.is_empty(),
            "the zipkin worker should have POSTed to /api/v2/spans"
        );
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
