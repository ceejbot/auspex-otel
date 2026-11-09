//! Read auspex's trace context back out of a `tracing` span, and write it
//! into outbound requests.

use http::header::{HeaderMap, HeaderName, HeaderValue};
use tracing_subscriber::registry::LookupSpan as _;

use crate::SpanContext;

const TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");
const TRACESTATE: HeaderName = HeaderName::from_static("tracestate");

/// Extension trait exposing the [`SpanContext`] auspex recorded on a
/// [`tracing::Span`].
///
/// The main use is log correlation: a JSON log formatter can stamp every
/// record with the current `trace_id` and `span_id` so logs and traces join
/// in the backend.
pub trait AuspexSpanExt {
    /// Returns the [`SpanContext`] auspex recorded on this span, if any.
    ///
    /// `None` when the span is disabled, has already closed, or lives in a
    /// subscriber that does not include auspex's layer
    /// ([`Tracer::subscriber_layer`](crate::Tracer::subscriber_layer)).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use auspex::AuspexSpanExt;
    /// use tracing::Span;
    ///
    /// // The trace id of the current span, or `None` when auspex is not
    /// // tracking it.
    /// let trace_id = Span::current().span_context().map(|ctx| ctx.trace_id());
    /// ```
    fn span_context(&self) -> Option<SpanContext>;

    /// Writes this span's trace context into outbound request headers so the
    /// downstream service's spans join this trace.
    ///
    /// Sets `traceparent` naming this span as the parent, replacing any value
    /// already present. `tracestate` is set when the inbound request carried
    /// one and removed otherwise, so the pair always describes this span.
    /// Does nothing when auspex is not tracking the span, which keeps the
    /// disabled tracer free here too.
    ///
    /// [`http::HeaderMap`] is the type reqwest and hyper use, so the map goes
    /// straight onto the outbound request.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use auspex::AuspexSpanExt;
    ///
    /// let mut headers = reqwest::header::HeaderMap::new();
    /// tracing::Span::current().inject_headers(&mut headers);
    /// client.get(url).headers(headers).send().await?;
    /// ```
    fn inject_headers(&self, headers: &mut HeaderMap);
}

impl AuspexSpanExt for tracing::Span {
    fn span_context(&self) -> Option<SpanContext> {
        self.with_subscriber(|(id, dispatch)| {
            // `downcast_ref` sees through `Layered` wrappers, so this works for
            // any subscriber built on `tracing_subscriber::registry()`.
            let registry = dispatch.downcast_ref::<tracing_subscriber::Registry>()?;
            let span = registry.span(id)?;
            span.extensions().get::<SpanContext>().cloned()
        })
        .flatten()
    }

    fn inject_headers(&self, headers: &mut HeaderMap) {
        let Some(ctx) = self.span_context() else {
            return;
        };

        // The formatter emits only lowercase hex and dashes, so this cannot
        // fail; the guard keeps `unwrap_used` honest without a panic path.
        if let Ok(value) = HeaderValue::from_str(&ctx.traceparent_header()) {
            headers.insert(TRACEPARENT, value);
        }

        match ctx.trace_state().and_then(|ts| HeaderValue::from_str(ts).ok()) {
            Some(value) => {
                headers.insert(TRACESTATE, value);
            }
            None => {
                headers.remove(TRACESTATE);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use tracing_subscriber::layer::SubscriberExt;

    use crate::pipeline::Pipeline;
    use crate::{AuspexSpanExt, Config, OtelLayer};

    fn auspex_layer() -> OtelLayer {
        let config = Config::default()
            .with_service_name("span-ext-test")
            .with_sink_uri("http://localhost:4318");
        let (pipeline, _rx) = Pipeline::new_for_test(config);
        OtelLayer::from_pipeline(pipeline)
    }

    #[test]
    fn test_span_context_returns_none_when_not_instrumented() {
        let span_a = tracing::info_span!("a");
        assert!(span_a.span_context().is_none());
    }

    #[test]
    fn test_span_context_returns_none_without_auspex_layer() {
        assert!(tracing::Span::none().span_context().is_none());

        // A real subscriber that simply does not include auspex.
        let subscriber =
            tracing_subscriber::Registry::default().with(tracing_subscriber::fmt::layer().with_writer(std::io::sink));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("untracked");
            assert!(span.span_context().is_none());
        });
    }

    #[test]
    fn test_span_context_returns_correct_context() {
        let subscriber = tracing_subscriber::Registry::default().with(auspex_layer());

        tracing::subscriber::with_default(subscriber, || {
            // no span was entered, current has no trace_id
            assert!(
                tracing::Span::current().span_context().is_none(),
                "no span -> None context"
            );
            let span_a = tracing::info_span!("a");
            // no parent, so b is a root span with a new trace_id
            let span_b = tracing::info_span!(parent: None, "b");

            span_a.in_scope(|| {
                let a_ctx = span_a.span_context().expect("span a context");
                let b_ctx = span_b.span_context().expect("span b context");
                let current_ctx = tracing::Span::current().span_context().expect("current_ctx");

                assert_ne!(a_ctx.trace_id(), b_ctx.trace_id(), "b and a have different traces");
                assert!(!a_ctx.trace_id().is_zero());
                assert!(!a_ctx.span_id().is_zero());
                assert!(!current_ctx.span_id().is_zero());
                assert_eq!(a_ctx.span_id(), current_ctx.span_id(), "a is the current span");
                assert_eq!(a_ctx.trace_id(), current_ctx.trace_id(), "a is the current trace");
            });
        });
    }

    fn header_str<'a>(headers: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
        headers.get(name).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn test_inject_headers_names_this_span_as_parent() {
        let subscriber = tracing_subscriber::Registry::default().with(auspex_layer());

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("outbound");
            let ctx = span.span_context().expect("tracked span");

            let mut headers = http::HeaderMap::new();
            span.inject_headers(&mut headers);

            let traceparent = header_str(&headers, "traceparent").expect("traceparent set");
            assert_eq!(traceparent, ctx.traceparent_header());
            let parsed = crate::propagation::parse_traceparent(traceparent).expect("parses");
            assert_eq!(parsed.trace_id, ctx.trace_id());
            assert_eq!(parsed.parent_id, ctx.span_id(), "the injecting span is the parent");
            assert!(parsed.flags.is_sampled());
            assert!(
                headers.get("tracestate").is_none(),
                "no inbound tracestate, none emitted"
            );
        });
    }

    #[test]
    fn test_inject_headers_replaces_stale_values() {
        let subscriber = tracing_subscriber::Registry::default().with(auspex_layer());

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("outbound");
            let mut headers = http::HeaderMap::new();
            headers.insert(
                "traceparent",
                http::HeaderValue::from_static("00-11111111111111111111111111111111-2222222222222222-01"),
            );
            headers.insert("tracestate", http::HeaderValue::from_static("stale=1"));

            span.inject_headers(&mut headers);

            let ctx = span.span_context().expect("tracked span");
            assert_eq!(
                header_str(&headers, "traceparent"),
                Some(ctx.traceparent_header().as_str())
            );
            assert!(
                headers.get("tracestate").is_none(),
                "a tracestate that does not belong to this span is removed"
            );
        });
    }

    #[test]
    fn test_inject_headers_forwards_inbound_tracestate() {
        let subscriber = tracing_subscriber::Registry::default().with(auspex_layer());

        tracing::subscriber::with_default(subscriber, || {
            // Mirror what the HTTP middleware records for a request that
            // arrived with traceparent + tracestate.
            let span = tracing::info_span!(
                "root",
                "remote.trace_id" = "4bf92f3577b34da6a3ce929d0e0e4736",
                "remote.parent_span_id" = "00f067aa0ba902b7",
                "remote.sampled" = false,
                "remote.trace_state" = "congo=t61rcWkgMzE,rojo=00f067aa0ba902b7"
            );
            let ctx = span.span_context().expect("tracked span");

            let mut headers = http::HeaderMap::new();
            span.inject_headers(&mut headers);

            let parsed =
                crate::propagation::parse_traceparent(header_str(&headers, "traceparent").expect("traceparent set"))
                    .expect("parses");
            assert_eq!(parsed.trace_id.to_string(), "4bf92f3577b34da6a3ce929d0e0e4736");
            assert_eq!(parsed.parent_id, ctx.span_id(), "our span, not the remote parent");
            assert!(!parsed.flags.is_sampled(), "inbound unsampled stays unsampled");
            assert_eq!(
                header_str(&headers, "tracestate"),
                Some("congo=t61rcWkgMzE,rojo=00f067aa0ba902b7")
            );
        });
    }

    #[test]
    fn test_inject_headers_is_a_no_op_without_auspex() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-keep", http::HeaderValue::from_static("yes"));

        tracing::Span::none().inject_headers(&mut headers);
        tracing::info_span!("untracked").inject_headers(&mut headers);

        assert_eq!(headers.len(), 1);
        assert!(headers.get("traceparent").is_none());
    }

    #[test]
    fn test_span_context_sees_through_additional_layers() {
        // Consumers stack filters and formatters around auspex's layer; the
        // registry downcast must still succeed.
        let subscriber = tracing_subscriber::Registry::default()
            .with(auspex_layer())
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::sink));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("layered");
            let ctx = span
                .span_context()
                .expect("layered subscriber still exposes the context");
            assert!(!ctx.trace_id().is_zero());
            assert!(!ctx.span_id().is_zero());
            assert!(ctx.is_sampled());
            assert!(ctx.trace_state().is_none());
        });
    }
}
