//! W3C trace context propagation: `traceparent` parsing and formatting plus
//! opaque `tracestate` preservation. Everything is `pub(crate)` — users
//! interact only via the `tracing` facade and the `auspex::Tracer` middleware.

mod traceparent;
mod tracestate;

// We use `pub(crate) use` + allow because the parent `mod propagation` is
// itself declared `pub(crate)` in lib.rs; the lint is overly noisy for this
// pattern.
#[allow(clippy::redundant_pub_crate)]
pub(crate) use traceparent::{TraceFlags, TraceParent, parse_traceparent};
#[allow(clippy::redundant_pub_crate)]
pub(crate) use tracestate::TraceState;
