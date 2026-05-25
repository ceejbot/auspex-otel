//! # auspex
//!
//! Extremely opinionated, lean, high-performance OTEL tracing middleware for
//! [axum](https://docs.rs/axum).
//!
//! The goal is trivial setup with excellent performance and none of the
//! complexity of the full OpenTelemetry SDK.
//!
//! ## Two supported setup patterns
//!
//! Ergonomic path (applications that do not already own their subscriber):
//!
//! ```rust,ignore
//! let tracer = auspex::init()?;
//!
//! let app = axum::Router::new()
//!     .route("/", get(handler))
//!     .layer(tracer);
//! ```
//!
//! Explicit subscriber path (recommended when you control `tracing_subscriber`
//! initialization):
//!
//! ```rust,ignore
//! let tracer = auspex::Tracer::new();
//!
//! tracing_subscriber::registry()
//!     .with(tracer.subscriber_layer())
//!     .with(tracing_subscriber::fmt::layer())
//!     .init();
//!
//! let app = axum::Router::new()
//!     .route("/", get(handler))
//!     .layer(tracer);
//! ```

#![deny(unsafe_code)]
#![warn(missing_docs)]

// Internal modules first (required before pub use re-exports).
mod config;
mod error;
mod exporter;
mod pipeline;

pub(crate) mod context;
pub(crate) mod field;
pub(crate) mod span;

// Propagation (traceparent + future tracestate) is internal for now.
pub(crate) mod propagation;

// Public modules (re-exported below for a flat, ergonomic API).
pub mod layer; // OtelLayer lives here; prefer `tracer.subscriber_layer()`
pub mod middleware; // Tracer (+ builder methods) lives here

// Public API — flat and ergonomic at the crate root.
pub use config::Config;
pub use error::{ConfigError, InitError};
pub use layer::OtelLayer;
pub use middleware::{Tracer, TracerBuilder};
use tracing_subscriber::Registry;
// Used only by the ergonomic init() path.
use tracing_subscriber::layer::SubscriberExt as _;

/// Ergonomic entry point (the 90% path).
///
/// Creates a `Tracer` (from environment) and installs a global subscriber
/// containing only its `OtelLayer`. This is the shortest way to get both
/// HTTP root spans *and* child spans (`#[instrument]`, etc.) exported.
///
/// ```rust,ignore
/// let tracer = auspex::init()?;
/// let app = axum::Router::new().layer(tracer);
/// ```
///
/// If your application already initializes its own `tracing_subscriber`,
/// use the explicit path instead:
///
/// ```rust,ignore
/// let tracer = auspex::Tracer::new();
/// tracing_subscriber::registry()
///     .with(tracer.subscriber_layer())
///     .with(tracing_subscriber::fmt::layer())
///     .init();
/// ```
///
/// # Errors
///
/// Returns `Err(InitError::GlobalSubscriberAlreadySet)` when a global
/// subscriber was already installed. In that case the caller must use
/// the explicit `subscriber_layer()` composition path.
///
/// Also returns configuration errors from the underlying `Tracer::try_new()`.
pub fn init() -> Result<Tracer, InitError> {
    let tracer = Tracer::try_new()?;

    // Build a minimal subscriber that only contains our OtelLayer (no fmt layer,
    // no other noise). This is the "ergonomic path" promised by the design.
    let otel_layer = tracer.subscriber_layer();
    let subscriber = Registry::default().with(otel_layer);

    // If someone already called set_global_default (or another init), we must
    // not clobber it. The caller should use the explicit subscriber_layer path.
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        return Err(InitError::GlobalSubscriberAlreadySet);
    }

    Ok(tracer)
}

#[cfg(test)]
mod tests {
    /// Placeholder so `cargo test` on a fresh skeleton still works.
    #[test]
    fn crate_compiles() {
        // If this test runs, the module tree and strong types are healthy.
    }

    /// Compile-time proof that every item promised at the crate root by the
    /// public API docs is actually importable as `auspex::Item`.
    /// This is stronger than `rust,ignore` doctests.
    #[test]
    fn public_api_surface_is_available_at_root() {
        // These would fail to compile if any re-export is missing or private.
        use crate::{Config, ConfigError, InitError, OtelLayer, Tracer, TracerBuilder, init};

        // Also exercise the free function and the two main types users interact with.
        let _ = std::any::type_name::<Tracer>();
        let _ = std::any::type_name::<Config>();
        let _ = std::any::type_name::<OtelLayer>();
        let _ = std::any::type_name::<TracerBuilder>();
        let _ = std::any::type_name::<ConfigError>();
        let _ = std::any::type_name::<InitError>();

        // init() is callable in signature (we don't run it here to avoid side effects).
        let _: fn() -> Result<Tracer, InitError> = init;
    }
}
