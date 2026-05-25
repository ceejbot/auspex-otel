//! `tracestate` header preservation (Task 1.3).
//!
//! Design goal:
//! - Preserve inbound `tracestate` as an **opaque validated string**.
//! - Do **not** interpret individual vendors in v0.1.
//! - Invalid or over-budget values are dropped (with a future counter/debug
//!   event).
//! - Absence must not cause allocation on `SpanContext` or `OtelSpan`.

const MAX_TRACESTATE_LEN: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TraceState {
    /// The raw value is stored as `Box<str>` so there is only one heap
    /// allocation when a value is actually present.
    value: Box<str>,
}

impl TraceState {
    /// Create a `TraceState` from a raw `tracestate` HTTP header value.
    ///
    /// Returns `None` (i.e. the header is dropped) when:
    /// - The value is empty after trimming whitespace, or
    /// - The value exceeds `MAX_TRACESTATE_LEN` (currently 512 bytes).
    ///
    /// For v0.1 we treat almost any non-empty string under the budget as
    /// valid. We do not parse the `key[=value]` list or enforce vendor
    /// format rules — the value is truly opaque.
    pub fn from_header(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.len() > MAX_TRACESTATE_LEN {
            return None;
        }
        Some(Self { value: trimmed.into() })
    }

    /// Returns the original (trimmed) header value.
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

// =============================================================================
// Test Skeletons
// These tests document the exact behavior we want to build.
// Many will be enabled as we implement the type and wire it into
// SpanContext / OtelSpan.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Construction & validation (can run immediately) ---

    #[test]
    fn valid_tracestate_is_preserved() {
        let input = "vendor1=value1,vendor2=value2";
        let ts = TraceState::from_header(input).expect("should be valid");
        assert_eq!(ts.as_str(), input);
    }

    #[test]
    fn whitespace_is_trimmed_on_construction() {
        let ts = TraceState::from_header("  foo=bar,baz=quux  ").expect("should be valid");
        assert_eq!(ts.as_str(), "foo=bar,baz=quux");
    }

    #[test]
    fn empty_after_trim_is_rejected() {
        assert!(TraceState::from_header("").is_none());
        assert!(TraceState::from_header("   \t\n  ").is_none());
    }

    #[test]
    fn over_budget_is_rejected() {
        let too_long = "a".repeat(MAX_TRACESTATE_LEN + 1);
        assert!(TraceState::from_header(&too_long).is_none());

        // Exactly at the limit is still accepted
        let at_limit = "a".repeat(MAX_TRACESTATE_LEN);
        assert!(TraceState::from_header(&at_limit).is_some());
    }

    // --- Future integration tests (kept as ignored skeletons for now) ---
    // These will become real once we update SpanContext and OtelSpan.

    #[test]
    fn can_attach_to_span_context() {
        use crate::context::{SpanContext, SpanId, TraceId};

        let ts = TraceState::from_header("rojo=val1,verde=val2").expect("should get tracestate");
        let tid = TraceId::from_bytes([1; 16]);
        let sid = SpanId::from_bytes([2; 8]);

        let ctx = SpanContext::new(tid, sid, true).with_trace_state(ts);

        assert_eq!(
            ctx.trace_state.as_ref().map(TraceState::as_str),
            Some("rojo=val1,verde=val2")
        );
    }

    #[test]
    fn can_be_attached_to_otel_span() {
        use crate::context::{SpanId, TraceId};
        use crate::span::OtelSpan;

        let ts = TraceState::from_header("rojo=val1").expect("valid tracestate");
        let tid = TraceId::from_bytes([1; 16]);
        let sid = SpanId::from_bytes([2; 8]);

        let mut span = OtelSpan::new(tid, sid, None, "test");
        span.trace_state = Some(ts);

        assert_eq!(span.trace_state.as_ref().map(TraceState::as_str), Some("rojo=val1"));
    }

    #[test]
    fn absence_does_not_allocate_on_containers() {
        use crate::context::SpanContext;

        // Constructing a SpanContext without a TraceState should not
        // allocate for the trace_state field (thanks to Option).
        let _ctx: SpanContext = SpanContext::new(
            crate::context::TraceId::from_bytes([0; 16]),
            crate::context::SpanId::from_bytes([0; 8]),
            false,
        );
    }

    #[test]
    #[ignore = "needs FinishedSpan / exporter work"]
    fn is_carried_through_to_export() {
        // When a span is finished, any preserved TraceState must be
        // available on the FinishedSpan so the exporter can write it
        // into the `trace_state` field of the OTLP Span.
        todo!("TraceState must survive until export time");
    }
}
