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
        Self::current()
            .with_subscriber(|(id, dispatch)| {
                let registry = dispatch.downcast_ref::<tracing_subscriber::Registry>()?;
                let span = registry.span(id)?;
                span.extensions().get::<SpanContext>().cloned()
            })
            .flatten()
    }
}
