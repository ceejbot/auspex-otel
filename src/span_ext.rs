//! Read auspex's trace context back out of a `tracing` span.

use tracing_subscriber::registry::LookupSpan as _;

use crate::SpanContext;

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
