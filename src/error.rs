//! Error types for configuration and initialization.
//!
//! Two tiers:
//! - `ConfigError`: for fallible config validation (`try_new`,
//!   `try_from_config`).
//! - `InitError`: for the ergonomic `init()` path (may wrap config or report
//!   subscriber installation issues).
//!
//! Convenience paths (`new`, `from_config`) turn config problems into disabled
//! mode + a warning rather than returning errors.

use std::error::Error;
use std::fmt;

/// Errors that occur while building or validating a [`Config`](crate::Config).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A required value (service name) was missing while an exporter sink was
    /// explicitly configured. This is the *only* case in which fallible
    /// constructors (`try_new`, `init`) return an error; absence of any sink
    /// configuration produces a disabled tracer successfully.
    MissingServiceName,

    /// The exporter endpoint / sink URI was present but could not be
    /// interpreted (bad scheme, unsupported protocol in this build, etc.).
    InvalidExporterEndpoint(String),

    /// Other configuration problem (e.g. malformed header map, out-of-range
    /// batch size).
    InvalidConfig(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingServiceName => {
                write!(f, "OTEL_SERVICE_NAME is required for enabled export")
            }
            Self::InvalidExporterEndpoint(s) => {
                write!(f, "invalid exporter endpoint: {s}")
            }
            Self::InvalidConfig(s) => write!(f, "invalid configuration: {s}"),
        }
    }
}

impl Error for ConfigError {}

/// Errors returned by the ergonomic [`init`](crate::init) path and other
/// initialization routines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitError {
    /// The underlying configuration was invalid.
    Config(ConfigError),

    /// `init()` was called but a global `tracing` subscriber had already been
    /// installed. Use the explicit `Tracer::subscriber_layer()` path instead.
    GlobalSubscriberAlreadySet,

    /// Catch-all for other initialization failures (rare in v0.1).
    Other(String),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(e) => write!(f, "configuration error: {e}"),
            Self::GlobalSubscriberAlreadySet => {
                write!(
                    f,
                    "a global tracing subscriber is already set; use Tracer + subscriber_layer() instead of init()"
                )
            }
            Self::Other(s) => write!(f, "initialization error: {s}"),
        }
    }
}

impl Error for InitError {}

impl From<ConfigError> for InitError {
    fn from(e: ConfigError) -> Self {
        Self::Config(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_error_is_error_and_displayable() {
        let e = ConfigError::MissingServiceName;
        let s = e.to_string();
        assert!(s.contains("SERVICE_NAME"));
        // Implements std::error::Error
        let _: Box<dyn Error> = Box::new(e);
    }

    #[test]
    fn init_error_from_config_error() {
        let c = ConfigError::InvalidConfig("bad".into());
        let i: InitError = c.into();
        assert!(matches!(i, InitError::Config(_)));
        assert!(i.to_string().contains("configuration error"));
    }

    #[test]
    fn init_error_display_for_subscriber() {
        let e = InitError::GlobalSubscriberAlreadySet;
        let s = e.to_string();
        assert!(s.contains("subscriber_layer"));
    }
}
