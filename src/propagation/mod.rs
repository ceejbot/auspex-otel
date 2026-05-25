//! Internal W3C trace context propagation support (Phase 1).
//!
//! Currently only the `traceparent` header (Task 1.2). `tracestate` comes in
//! Task 1.3. Everything is `pub(crate)` — users interact only via the `tracing`
//! facade and the `auspex::Tracer` middleware.

mod traceparent;
mod tracestate;

// Re-exports for use by the HTTP middleware (future Phase 3 work).
// We use `pub(crate) use` + allow because the parent `mod propagation` is
// itself declared `pub(crate)` in lib.rs; the lint is overly noisy for this
// pattern.
#[allow(unused_imports, clippy::redundant_pub_crate)]
pub(crate) use traceparent::{ParseError as TraceParentParseError, TraceFlags, TraceParent, parse_traceparent};
#[allow(clippy::redundant_pub_crate)]
pub(crate) use tracestate::TraceState;
