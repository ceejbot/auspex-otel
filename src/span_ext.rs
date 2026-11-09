use tracing_subscriber::registry::LookupSpan as _;

use crate::SpanContext;

/// Utility to allow tracing [`Span`]s to return Auspex [`SpanContext`]s.
///
/// [`Span`]: tracing::Span
pub trait AuspexSpanExt {
    /// Extracts an Auspex [`SpanContext`] from `self`, if present.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use auspex::AuspexSpanExt;
    /// use tracing::Span;
    ///
    /// // Get the current trace_id context from the current tracing span,
    /// // or `None` if the span is not instrumented by Auspex
    /// let trace_id = Span::current().span_context().map(|ctx| ctx.trace_id());
    /// ```
    fn span_context(&self) -> Option<SpanContext>;
}

impl AuspexSpanExt for tracing::Span {
    fn span_context(&self) -> Option<SpanContext> {
        self.with_subscriber(|(id, dispatch)| {
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

    #[test]
    fn test_span_context_returns_none_when_not_instrumented() {
        let span_a = tracing::info_span!("a");
        assert!(span_a.span_context().is_none());
    }

    #[test]
    fn test_span_context_returns_correct_context() {
        let config = Config::default()
            .with_service_name("span-ext-test")
            .with_sink_uri("http://localhost:4318");
        let (pipeline, _rx) = Pipeline::new_for_test(config);
        let subscriber = tracing_subscriber::Registry::default().with(OtelLayer::from_pipeline(pipeline));

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
}
