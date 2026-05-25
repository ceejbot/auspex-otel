//! Export pipeline components (Task 6.1).
//!
//! The `Exporter` trait is the seam between the batch worker and concrete
//! wire-format exporters. The worker itself lives in `batch.rs`.

use std::error::Error;
use std::fmt;

use async_trait::async_trait;

use crate::span::FinishedSpan;

pub mod batch;
mod http;
mod otlp_http;
mod proto;
mod retry;
#[cfg(test)]
mod test_exporter;
mod zipkin;

/// Error returned by an `Exporter`.
///
/// In v0.1 this is intentionally minimal. Real exporters (Task 6.2+) may
/// enrich it with more structured information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportError {
    pub message: String,
}

impl ExportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "export error: {}", self.message)
    }
}

impl Error for ExportError {}

/// The pluggable export backend.
///
/// Implementations are responsible for their own retry/backoff/timeout policy.
/// The batch worker only guarantees delivery of non-empty batches when either
/// the size or schedule-delay threshold is reached.
#[async_trait]
pub trait Exporter: Send + Sync + 'static {
    async fn export(&self, batch: Vec<FinishedSpan>) -> Result<(), ExportError>;
}

// Re-exports for convenience inside the crate (and for `crate::exporter::XXX`
// paths used by tests and pipeline wiring). `pub(crate)` is redundant because
// the parent `mod exporter` is itself private, but the names are the intended
// public-in-crate API.
#[allow(unused_imports, clippy::redundant_pub_crate)]
pub(crate) use self::batch::BatchWorker;
#[allow(unused_imports, clippy::redundant_pub_crate)]
pub(crate) use self::otlp_http::OtlpHttpExporter;
#[cfg(test)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) use self::test_exporter::TestExporter;
#[allow(unused_imports, clippy::redundant_pub_crate)]
pub(crate) use self::zipkin::ZipkinExporter;
