//! Internal representation of an OpenTelemetry span.
//!
//! Designed for low allocation in the hot path. Uses `Cow<'static, str>` for
//! attribute keys (most keys are static from semantic conventions) and a
//! small, closed `AttributeValue` enum.

use std::borrow::Cow;
use std::time::{Duration, SystemTime};

use smallvec::SmallVec;

use crate::context::{SpanId, TraceId};
use crate::propagation::TraceState;

/// Returns true for attribute keys that are known to be recorded late by the
/// HTTP middleware and must not be dropped (e.g. response status code).
#[inline]
fn is_protected_http_attribute(key: &str) -> bool {
    key == "http.response.status_code"
        || key == "http.response.body.size"
        || key == "http.route"
        || key == "error.type"
        || key.starts_with("http.response.header.")
}

/// The value of a span attribute.
///
/// Kept deliberately small and non-recursive for performance and to avoid
/// the complexity of the full OTEL attribute value model in v1.
#[derive(Clone, PartialEq, Debug)]
pub enum AttributeValue {
    /// UTF-8 string. Stored as `Cow` so static strings are zero-cost.
    String(Cow<'static, str>),
    /// Signed 64-bit integer (covers most counter/measurement use cases).
    Int(i64),
    /// 64-bit floating point.
    Float(f64),
    /// Boolean.
    Bool(bool),
    // NOTE: We deliberately do *not* support arrays or kv-lists in v1.
    // Adding them later is a non-breaking change if we make this enum
    // non-exhaustive or version the wire format.
}

impl From<&'static str> for AttributeValue {
    fn from(s: &'static str) -> Self {
        Self::String(Cow::Borrowed(s))
    }
}

impl From<String> for AttributeValue {
    fn from(s: String) -> Self {
        Self::String(Cow::Owned(s))
    }
}

impl From<i64> for AttributeValue {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<i32> for AttributeValue {
    fn from(v: i32) -> Self {
        Self::Int(i64::from(v))
    }
}

impl From<f64> for AttributeValue {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}

impl From<bool> for AttributeValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

/// Span status as defined by the OTEL specification.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Status {
    /// Default. No known status.
    #[default]
    Unset,
    /// The operation completed successfully.
    Ok,
    /// The operation contains an error. May include a human-readable
    /// description.
    Error { message: Cow<'static, str> },
}

/// A link to another span (used for `follows_from` and causal relationships).
#[derive(Clone, PartialEq, Debug)]
pub struct Link {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub attributes: SmallVec<[(Cow<'static, str>, AttributeValue); 4]>,
}

/// An event recorded on a span (e.g. an exception or a log line turned into a
/// span event).
#[derive(Clone, PartialEq, Debug)]
pub struct Event {
    pub name: Cow<'static, str>,
    pub timestamp: SystemTime,
    pub attributes: SmallVec<[(Cow<'static, str>, AttributeValue); 4]>,
}

/// The internal representation of a span used by the tracing layer and
/// exporters.
///
/// This struct is **not** public API. Users interact with spans exclusively
/// through the `tracing` crate.
#[derive(Debug)]
pub struct OtelSpan {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent_span_id: Option<SpanId>,
    pub trace_state: Option<TraceState>,
    pub name: Cow<'static, str>,
    pub kind: SpanKind,
    pub start_time: SystemTime,
    pub end_time: Option<SystemTime>,

    /// Whether this span is sampled (the W3C sampled flag, inherited from the
    /// parent/remote context or defaulted for a locally-created root). Exported
    /// into the OTLP `Span.flags` trace-flags byte.
    pub is_sampled: bool,

    /// Inline storage sized for the 90% case (most HTTP spans fit in 16).
    pub attributes: SmallVec<[(Cow<'static, str>, AttributeValue); 16]>,
    pub dropped_attributes_count: u32,

    pub events: SmallVec<[Event; 8]>,
    pub dropped_events_count: u32,

    pub links: SmallVec<[Link; 4]>,
    pub dropped_links_count: u32,

    pub status: Status,
}

/// Immutable snapshot of a span, sent to the exporter worker.
///
/// This is intentionally distinct from the mutable `OtelSpan` that lives in
/// tracing extensions.
#[derive(Debug, Clone)]
pub struct FinishedSpan {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent_span_id: Option<SpanId>,
    pub trace_state: Option<TraceState>,

    pub name: Cow<'static, str>,
    pub kind: SpanKind,

    pub start_time: SystemTime,
    pub end_time: SystemTime, // always set when finished

    /// W3C sampled flag, exported into the OTLP `Span.flags` trace-flags byte.
    pub is_sampled: bool,

    pub attributes: SmallVec<[(Cow<'static, str>, AttributeValue); 16]>,
    pub dropped_attributes_count: u32,

    pub events: SmallVec<[Event; 8]>,
    pub dropped_events_count: u32,

    pub links: SmallVec<[Link; 4]>,
    pub dropped_links_count: u32,

    pub status: Status,
}

/// Span kind per OTEL spec. We only need Server + Client for v1.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SpanKind {
    #[default]
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

impl OtelSpan {
    /// Create a new span with the given identity and name.
    /// Timestamps are captured at construction for the start time.
    pub fn new(
        trace_id: TraceId,
        span_id: SpanId,
        parent_span_id: Option<SpanId>,
        name: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            trace_id,
            span_id,
            parent_span_id,
            trace_state: None,
            name: name.into(),
            kind: SpanKind::default(),
            start_time: SystemTime::now(),
            end_time: None,
            // A locally-created root is sampled by default; the layer overrides
            // this from the resolved parent/remote context when there is one.
            is_sampled: true,
            attributes: SmallVec::new(),
            dropped_attributes_count: 0,
            events: SmallVec::new(),
            dropped_events_count: 0,

            links: SmallVec::new(),
            dropped_links_count: 0,

            status: Status::default(),
        }
    }

    /// Record (or overwrite) an attribute on this live span.
    ///
    /// This is the hot path called from `tracing::Span::record`.
    /// Linear scan is intentional and fast for our chosen size.
    ///
    /// Critical late-bound HTTP attributes (status code, route, error.type,
    /// etc.) are protected from being dropped when the attribute budget is
    /// exhausted.
    pub fn record(&mut self, key: impl Into<Cow<'static, str>>, value: impl Into<AttributeValue>) {
        let key = key.into();
        let value = value.into();

        // Linear scan for existing key (very fast at N=16)
        for (k, v) in &mut self.attributes {
            if k == &key {
                *v = value;
                return;
            }
        }

        let is_protected = is_protected_http_attribute(&key);

        if self.attributes.len() < self.attributes.capacity() {
            self.attributes.push((key, value));
        } else if is_protected {
            // Evict the first non-protected attribute to make room for this critical one
            if let Some(pos) = self
                .attributes
                .iter()
                .position(|(k, _)| !is_protected_http_attribute(k))
            {
                self.attributes[pos] = (key, value);
            } else {
                // All existing attributes are protected — we have to drop this one (very rare)
                self.dropped_attributes_count = self.dropped_attributes_count.saturating_add(1);
            }
        } else {
            self.dropped_attributes_count = self.dropped_attributes_count.saturating_add(1);
        }
    }

    /// Record the end of the span.
    pub fn end(&mut self) {
        if self.end_time.is_none() {
            self.end_time = Some(SystemTime::now());
        }
    }

    /// Duration of the span, if it has been ended.
    // Convenience accessor; the export path derives nanos from start/end
    // directly, so this is currently only used in tests.
    #[allow(dead_code)]
    pub fn duration(&self) -> Option<Duration> {
        self.end_time.and_then(|end| end.duration_since(self.start_time).ok())
    }

    /// Consume the live span and produce an immutable `FinishedSpan` for
    /// the exporter pipeline.
    ///
    /// This enforces that end time is set exactly once (via the existing
    /// `end()` logic).
    pub fn finish(self) -> FinishedSpan {
        let end_time = self.end_time.unwrap_or_else(SystemTime::now);

        FinishedSpan {
            trace_id: self.trace_id,
            span_id: self.span_id,
            parent_span_id: self.parent_span_id,
            trace_state: self.trace_state,

            name: self.name,
            kind: self.kind,

            start_time: self.start_time,
            end_time,

            is_sampled: self.is_sampled,

            attributes: self.attributes,
            dropped_attributes_count: self.dropped_attributes_count,

            events: self.events,
            dropped_events_count: self.dropped_events_count,

            links: self.links,
            dropped_links_count: self.dropped_links_count,

            status: self.status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_value_from_conversions() {
        let v: AttributeValue = "static".into();
        assert!(matches!(v, AttributeValue::String(Cow::Borrowed(_))));

        let v: AttributeValue = "owned".to_string().into();
        assert!(matches!(v, AttributeValue::String(Cow::Owned(_))));

        let v: AttributeValue = 42i64.into();
        assert!(matches!(v, AttributeValue::Int(42)));

        let v: AttributeValue = true.into();
        assert!(matches!(v, AttributeValue::Bool(true)));
    }

    #[test]
    fn span_lifecycle() {
        let mut span = OtelSpan::new(TraceId::ZERO, SpanId::ZERO, None, "test-span");
        assert!(span.end_time.is_none());
        span.end();
        assert!(span.end_time.is_some());
        assert!(span.duration().is_some());
    }

    #[test]
    fn status_default_is_unset() {
        assert_eq!(Status::default(), Status::Unset);
    }

    // === Task 2.1: FinishedSpan and dropped counts (TDD skeletons) ===

    #[test]
    fn finished_span_is_immutable_snapshot() {
        let mut live = OtelSpan::new(TraceId::ZERO, SpanId::ZERO, None, "test-span");
        live.record("key1", "value1");
        live.end();

        let finished = live.finish();

        // The finished span should have captured the data.
        assert_eq!(finished.name, "test-span");
        // end_time >= start_time per the OTEL spec (a zero-duration span is
        // valid; new()->end() can land in the same clock tick on a fast host).
        assert!(finished.end_time >= finished.start_time);

        // Note: since we consumed `live` with `finish(self)`, there is no
        // way to mutate it afterward. This is the intended design.
    }

    #[test]
    fn finishing_span_sets_end_time_exactly_once() {
        let mut live = OtelSpan::new(TraceId::ZERO, SpanId::ZERO, None, "test");
        live.end();
        let finished = live.finish();

        // The existing `end()` + `finish()` logic ensures this.
        assert!(finished.end_time >= finished.start_time);
    }

    #[test]
    fn finished_span_preserves_dropped_counts() {
        let live = OtelSpan::new(TraceId::ZERO, SpanId::ZERO, None, "test");
        let finished = live.finish();

        assert_eq!(finished.dropped_attributes_count, 0);
        assert_eq!(finished.dropped_events_count, 0);
        assert_eq!(finished.dropped_links_count, 0);
    }

    // === Task 1.1 driving tests for the accepted SmallVec attribute model ===

    #[test]
    fn record_overwrites_existing_key() {
        let mut span = OtelSpan::new(TraceId::ZERO, SpanId::ZERO, None, "test");
        span.record("http.status_code", 200i32);
        span.record("http.status_code", 404i32);

        assert_eq!(span.attributes.len(), 1);
        assert!(matches!(&span.attributes[0], (k, AttributeValue::Int(404)) if k == "http.status_code"));
        assert_eq!(span.dropped_attributes_count, 0);
    }

    #[test]
    fn record_appends_new_keys() {
        let mut span = OtelSpan::new(TraceId::ZERO, SpanId::ZERO, None, "test");
        span.record("method", "GET");
        span.record("path", "/users");

        assert_eq!(span.attributes.len(), 2);
        assert_eq!(span.dropped_attributes_count, 0);
    }

    #[test]
    fn record_drops_on_overflow_and_tracks_count() {
        let mut span = OtelSpan::new(TraceId::ZERO, SpanId::ZERO, None, "overflow-test");

        // Fill up to capacity (16)
        for i in 0..16 {
            span.record(format!("key-{i}"), i64::from(i));
        }
        assert_eq!(span.attributes.len(), 16);
        assert_eq!(span.dropped_attributes_count, 0);

        // Now overflow with non-protected keys
        span.record("extra-1", 1i64);
        span.record("extra-2", 2i64);

        assert_eq!(span.attributes.len(), 16);
        assert_eq!(span.dropped_attributes_count, 2);
    }

    #[test]
    fn protected_http_attributes_are_never_dropped() {
        let mut span = OtelSpan::new(TraceId::ZERO, SpanId::ZERO, None, "http-test");

        // Fill with 16 regular attributes
        for i in 0..16 {
            span.record(format!("user.attr.{i}"), i64::from(i));
        }

        // Recording critical late-bound HTTP attributes should evict a
        // non-protected attribute instead of dropping the important one.
        span.record("http.response.status_code", 200i64);
        span.record("http.route", "/users/{id}");
        span.record("error.type", "io::Error");

        assert_eq!(span.dropped_attributes_count, 0);
        assert!(span.attributes.iter().any(|(k, _)| k == "http.response.status_code"));
        assert!(span.attributes.iter().any(|(k, _)| k == "http.route"));
        assert!(span.attributes.iter().any(|(k, _)| k == "error.type"));
    }
}
