//! Configuration for the tracer and exporter pipeline.
//!
//! Resolved once at construction time with the following precedence:
//!
//! 1. Explicit builder / `from_config` values
//! 2. `OTEL_SINK_URI` (auspex shortcut)
//! 3. Standard `OTEL_*` variables
//! 4. Sensible defaults (disabled export until a valid sink is configured)
//!
//! Convenience constructors (`new`, `from_config`) turn any unusable exporter
//! config into a disabled `Tracer`. Fallible constructors (`try_new`,
//! `try_from_config`, `init`) return `ConfigError` *only* for partial
//! misconfiguration (sink configured but service name missing). A completely
//! unconfigured environment (or explicit `none`) yields a disabled tracer
//! successfully from the fallible paths as well.

use std::env;
use std::time::Duration;

use url::Url;

use crate::ConfigError;

/// High-level configuration for auspex tracing/export.
/// Endpoint / exporter kind are the typed form of the resolved sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Service name attached to the OTEL Resource (required for export).
    pub service_name: Option<String>,

    /// Preferred sink (`OTEL_SINK_URI` or derived). Examples:
    /// - `<http://localhost:4318>` (OTLP/HTTP)
    /// - `<zipkin+http://localhost:9411>` (Zipkin for local Jaeger)
    ///
    /// Legacy string form — prefer the typed `endpoint` / `exporter` fields
    /// after resolution.
    pub sink_uri: Option<String>,

    /// Typed endpoint (populated by resolver when possible).
    pub endpoint: Option<Endpoint>,

    /// Selected exporter kind after resolution.
    pub exporter: ExporterKind,

    /// OTLP exporter headers (from `OTEL_EXPORTER_OTLP_HEADERS` etc.).
    pub headers: Vec<(String, String)>,

    /// Whether exporting is explicitly disabled (via
    /// `OTEL_TRACES_EXPORTER=none` or absence of usable endpoint + service
    /// name in fallible paths).
    pub disabled: bool,

    /// Max batch size for the span exporter (default 512).
    pub max_export_batch_size: usize,

    /// Schedule delay between batch exports (default 5s).
    pub schedule_delay: Duration,

    /// Comma-separated header prefixes to capture as attributes
    /// (via `AUSPEX_CAPTURE_HEADERS_PREFIX`).
    pub capture_header_prefixes: Vec<String>,

    /// Extra OTEL Resource attributes attached to every exported span
    /// (from `OTEL_RESOURCE_ATTRIBUTES` and/or the builder). These are
    /// per-process identity — `service.version`, `deployment.environment`,
    /// `service.commit_hash`, etc. `service.name` is never stored here; it
    /// lives in the dedicated `service_name` field.
    pub resource_attributes: Vec<(String, String)>,
}

/// Typed representation of a resolved exporter endpoint.
///
/// Produced by precedence resolution + scheme dispatch in `Config::from_env`
/// (and the builder paths). You normally do not construct these directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// OTLP/HTTP endpoint (protobuf traces).
    OtlpHttp(Url),
    /// Zipkin v2 JSON endpoint.
    Zipkin(Url),
}

/// High-level kind of exporter selected after resolution.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ExporterKind {
    /// OTLP/HTTP protobuf exporter.
    Otlp,
    /// Zipkin v2 JSON exporter.
    Zipkin,
    /// No exporter configured (disabled mode).
    #[default]
    None,
}

/// Endpoint resolver used by `from_env` and builder paths.
/// Applies scheme dispatch + documented path defaults.
/// Returns Err for unsupported schemes in v0.1 (e.g. grpc) so that
/// fallible constructors can surface a clear `ConfigError`.
pub fn resolve_endpoint(raw: &str) -> Result<Endpoint, ConfigError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::InvalidExporterEndpoint("empty endpoint".into()));
    }

    // grpc / otlp+grpc are explicitly unsupported in v0.1
    if trimmed.starts_with("grpc://") || trimmed.starts_with("otlp+grpc://") {
        return Err(ConfigError::InvalidExporterEndpoint(
            "gRPC endpoints are not supported in v0.1 (no grpc feature)".into(),
        ));
    }

    // Zipkin: accept `zipkin+http://`, `zipkin+https://`, or bare `zipkin://`
    // (defaults to http). We must normalize to a real http(s) URL — the stored
    // endpoint is handed to a `reqwest` client, which cannot POST to a custom
    // `zipkin+https` scheme.
    let into_zipkin = |mut u: Url| {
        if u.path() == "/" || u.path().is_empty() {
            u.set_path("/api/v2/spans");
        }
        Endpoint::Zipkin(u)
    };
    if let Some(rest) = trimmed.strip_prefix("zipkin+") {
        // `rest` is expected to already be an http(s) URL.
        if (rest.starts_with("http://") || rest.starts_with("https://"))
            && let Ok(u) = Url::parse(rest)
        {
            return Ok(into_zipkin(u));
        }
    } else if let Some(rest) = trimmed.strip_prefix("zipkin://") {
        // Bare `zipkin://host...` maps to plain http.
        if let Ok(u) = Url::parse(&format!("http://{rest}")) {
            return Ok(into_zipkin(u));
        }
    }

    // http / https → OTLP
    if (trimmed.starts_with("http://") || trimmed.starts_with("https://"))
        && let Ok(u) = Url::parse(trimmed)
    {
        let mut u = u;
        if u.path() == "/" || u.path().is_empty() {
            u.set_path("/v1/traces");
        }
        return Ok(Endpoint::OtlpHttp(u));
    }

    Err(ConfigError::InvalidExporterEndpoint(format!(
        "unsupported or unparseable endpoint: {trimmed}"
    )))
}

/// Parse `OTEL_EXPORTER_OTLP_HEADERS` style strings ("k1=v1,k2=v2").
/// Malformed individual pairs are skipped (with a debug log).
/// This matches the Validate requirement for Task 4.1.
pub fn parse_headers(raw: &str) -> Vec<(String, String)> {
    parse_kv_pairs(raw, "OTLP header")
}

/// Parse a `key=value,key=value` list, trimming whitespace and skipping any
/// malformed or empty entries. `what` labels the entry kind in debug logs
/// (shared by OTLP headers and `OTEL_RESOURCE_ATTRIBUTES`).
fn parse_kv_pairs(raw: &str, what: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in raw.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            if !k.is_empty() && !v.is_empty() {
                out.push((k.to_string(), v.to_string()));
            } else {
                tracing::debug!("skipping malformed {what} pair (empty key or value): {pair}");
            }
        } else {
            tracing::debug!("skipping malformed {what} pair (no '='): {pair}");
        }
    }
    out
}

impl Default for Config {
    fn default() -> Self {
        Self {
            service_name: None,
            sink_uri: None,
            endpoint: None,
            exporter: ExporterKind::None,
            headers: Vec::new(),
            disabled: true, // safe default until explicitly configured
            max_export_batch_size: 512,
            schedule_delay: Duration::from_secs(5),
            capture_header_prefixes: Vec::new(),
            resource_attributes: Vec::new(),
        }
    }
}

impl Config {
    /// Load configuration from the environment using the documented precedence.
    ///
    /// This is a *best-effort* load for convenience paths. It never panics and
    /// will produce a disabled config rather than erroring on bad values.
    pub fn from_env() -> Self {
        let mut c = Self::default();

        if let Ok(name) = env::var("OTEL_SERVICE_NAME")
            && !name.trim().is_empty()
        {
            c.service_name = Some(name.trim().to_string());
        }

        // Sink precedence: OTEL_SINK_URI, then the standard OTLP endpoint vars.
        let raw_sink = if let Ok(uri) = env::var("OTEL_SINK_URI") {
            if uri.trim().is_empty() {
                None
            } else {
                c.sink_uri = Some(uri.trim().to_string());
                c.disabled = false;
                Some(uri.trim().to_string())
            }
        } else if let Ok(ep) = env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") {
            if ep.trim().is_empty() {
                None
            } else {
                c.sink_uri = Some(ep.trim().to_string());
                c.disabled = false;
                Some(ep.trim().to_string())
            }
        } else if let Ok(ep) = env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
            if ep.trim().is_empty() {
                None
            } else {
                c.sink_uri = Some(ep.trim().to_string());
                c.disabled = false;
                Some(ep.trim().to_string())
            }
        } else {
            None
        };

        // Resolve into typed endpoint when possible.
        // On failure (invalid scheme, etc.) treat as unusable sink in the
        // convenience path: disable rather than leaving a broken config enabled.
        if let Some(raw) = &raw_sink {
            if let Ok(ep) = resolve_endpoint(raw) {
                c.endpoint = Some(ep.clone());
                match &ep {
                    Endpoint::OtlpHttp(_) => c.exporter = ExporterKind::Otlp,
                    Endpoint::Zipkin(_) => c.exporter = ExporterKind::Zipkin,
                }
            } else {
                c.sink_uri = None;
                c.disabled = true;
                c.endpoint = None;
                c.exporter = ExporterKind::None;
            }
        }

        // explicit disable wins (takes precedence)
        if let Ok(exporter) = env::var("OTEL_TRACES_EXPORTER")
            && exporter.trim().eq_ignore_ascii_case("none")
        {
            c.disabled = true;
            c.sink_uri = None;
            c.endpoint = None;
            c.exporter = ExporterKind::None;
        }

        if let Ok(h) = env::var("OTEL_EXPORTER_OTLP_HEADERS")
            && !h.trim().is_empty()
        {
            c.headers = parse_headers(&h);
        }
        if let Ok(h) = env::var("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
            && !h.trim().is_empty()
            && c.headers.is_empty()
        {
            c.headers = parse_headers(&h);
        }

        if let Ok(s) = env::var("OTEL_BSP_MAX_EXPORT_BATCH_SIZE")
            && let Ok(n) = s.trim().parse::<usize>()
            && n > 0
            && n <= 10_000
        {
            c.max_export_batch_size = n;
        }
        if let Ok(s) = env::var("OTEL_BSP_SCHEDULE_DELAY") {
            // Per the OTEL spec this value is in milliseconds (default 5000).
            // Bound to (0, 1h) to reject nonsense; out-of-range keeps the default.
            if let Ok(ms) = s.trim().parse::<u64>()
                && ms > 0
                && ms < 3_600_000
            {
                c.schedule_delay = Duration::from_millis(ms);
            }
        }

        if let Ok(pfx) = env::var("AUSPEX_CAPTURE_HEADERS_PREFIX")
            && !pfx.trim().is_empty()
        {
            c.capture_header_prefixes = pfx
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }

        // Parsed after OTEL_SERVICE_NAME so an explicit service name wins over a
        // `service.name` carried in OTEL_RESOURCE_ATTRIBUTES (see the setter).
        if let Ok(attrs) = env::var("OTEL_RESOURCE_ATTRIBUTES")
            && !attrs.trim().is_empty()
        {
            c = c.with_resource_attributes(parse_kv_pairs(&attrs, "resource attribute"));
        }

        // Whether this is usable for export is decided by the Tracer
        // constructors (real export also requires a service_name).
        c
    }

    /// Returns whether this config will result in an enabled exporter pipeline.
    ///
    /// Note: for a pipeline to actually export useful data we also require
    /// a service name. This is enforced at `Tracer` construction time.
    #[inline]
    pub const fn is_enabled(&self) -> bool {
        !self.disabled && (self.endpoint.is_some() || self.sink_uri.is_some())
    }

    /// Returns true only when the configuration has both a usable endpoint
    /// (typed or legacy) and a service name.
    #[inline]
    pub const fn can_export(&self) -> bool {
        (self.endpoint.is_some() || self.sink_uri.is_some()) && self.service_name.is_some()
    }

    /// Builder-style setter (used by `TracerBuilder` in skeletons).
    pub fn with_service_name(mut self, name: impl Into<String>) -> Self {
        let n = name.into();
        if !n.trim().is_empty() {
            self.service_name = Some(n.trim().to_string());
        }
        self
    }

    /// Builder-style setter.
    /// When possible, also resolves the URI into the typed `endpoint` and
    /// `exporter` fields for consistency with the resolved Config shape.
    pub fn with_sink_uri(mut self, uri: impl Into<String>) -> Self {
        let u = uri.into();
        if !u.trim().is_empty() {
            self.sink_uri = Some(u.trim().to_string());
            self.disabled = false;

            if let Ok(ep) = resolve_endpoint(&u) {
                self.endpoint = Some(ep.clone());
                match &ep {
                    Endpoint::OtlpHttp(_) => self.exporter = ExporterKind::Otlp,
                    Endpoint::Zipkin(_) => self.exporter = ExporterKind::Zipkin,
                }
            }
        }
        self
    }

    /// Force disabled mode (useful for tests and explicit "off").
    pub fn disabled(mut self) -> Self {
        self.disabled = true;
        self.sink_uri = None;
        self.endpoint = None;
        self.exporter = ExporterKind::None;
        self
    }

    /// Builder-style setter for capture header prefixes
    /// (`AUSPEX_CAPTURE_HEADERS_PREFIX`).
    pub fn with_capture_header_prefixes(mut self, prefixes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.capture_header_prefixes = prefixes.into_iter().map(Into::into).collect();
        self
    }

    /// Builder-style setter for OTLP exporter headers.
    pub fn with_headers(mut self, headers: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>) -> Self {
        self.headers = headers.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        self
    }

    /// Builder-style setter for extra OTEL Resource attributes.
    ///
    /// Merges the given attributes into the existing set: an entry whose key
    /// already exists is overwritten, others are appended. This lets builder
    /// values win over `OTEL_RESOURCE_ATTRIBUTES` while leaving non-conflicting
    /// env entries in place.
    ///
    /// A `service.name` entry is special-cased: it is never stored as a
    /// resource attribute (it has its own field). It promotes to `service_name`
    /// only when that field is unset, so an explicit service name (e.g.
    /// `OTEL_SERVICE_NAME`) always wins.
    pub fn with_resource_attributes(
        mut self,
        attributes: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        for (k, v) in attributes {
            let k = k.into();
            let v = v.into();
            if k == "service.name" {
                if self.service_name.is_none() && !v.trim().is_empty() {
                    self.service_name = Some(v.trim().to_string());
                }
                continue;
            }
            if let Some(existing) = self.resource_attributes.iter_mut().find(|(ek, _)| *ek == k) {
                existing.1 = v;
            } else {
                self.resource_attributes.push((k, v));
            }
        }
        self
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::match_wildcard_for_single_variants)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled() {
        let c = Config::default();
        assert!(c.disabled);
        assert!(!c.is_enabled());
        assert!(c.service_name.is_none());
    }

    #[test]
    fn withers_update_fields() {
        let c = Config::default()
            .with_service_name("test-svc")
            .with_sink_uri("http://localhost:4318");
        assert_eq!(c.service_name.as_deref(), Some("test-svc"));
        assert_eq!(c.sink_uri.as_deref(), Some("http://localhost:4318"));
        assert!(!c.disabled);
        assert!(c.is_enabled());
    }

    #[test]
    fn disabled_wins() {
        let c = Config::default().with_sink_uri("http://example.com").disabled();
        assert!(c.disabled);
        assert!(!c.is_enabled());
    }

    #[test]
    fn from_env_basic_parsing_does_not_panic() {
        // Just exercise the parser in the current env (whatever it is).
        let _c = Config::from_env();
    }

    // TDD step 1 — first Validate bullet: "Builder overrides env".
    // Demonstrates the override contract using the existing with_* builder-style
    // methods (the foundation for the real TracerBuilder in 4.2). No global
    // env mutation for this bullet (env-interaction tests will introduce a
    // module-level Mutex or temp-env later per the Gotchas).
    #[test]
    fn builder_value_overrides_env() {
        // Simulate a "from env" base config (service name came from OTEL_*).
        let base = Config::default().with_service_name("from-env-value");

        // Explicit builder / with_ call (or future from_config with explicit
        // fields) must win.
        let overridden = base.with_service_name("explicit-builder-value");

        assert_eq!(overridden.service_name.as_deref(), Some("explicit-builder-value"));
    }

    // TDD Bullet 2: "OTEL_SINK_URI overrides standard endpoint variables."
    // Uses temp-env for safe, parallel-test-friendly isolation.
    #[test]
    fn otel_sink_uri_overrides_standard_otlp_endpoint() {
        temp_env::with_vars(
            [
                ("OTEL_SINK_URI", Some("http://auspex-sink.example.com:4318")),
                (
                    "OTEL_EXPORTER_OTLP_ENDPOINT",
                    Some("http://standard-otlp.example.com:4318"),
                ),
            ],
            || {
                let c = Config::from_env();
                // SINK_URI must win per the documented precedence.
                assert_eq!(c.sink_uri.as_deref(), Some("http://auspex-sink.example.com:4318"));
                assert!(!c.disabled);
            },
        );
    }

    // TDD Bullet 3: "OTEL_TRACES_EXPORTER=none disables, even if a sink is set."
    // Critical for the disabled-mode contract.
    #[test]
    fn otel_traces_exporter_none_disables_even_with_sink() {
        temp_env::with_vars(
            [
                ("OTEL_SINK_URI", Some("http://should-be-ignored.example.com")),
                ("OTEL_TRACES_EXPORTER", Some("none")),
            ],
            || {
                let c = Config::from_env();
                assert!(c.disabled);
                assert!(c.sink_uri.is_none());
                assert!(!c.is_enabled());
            },
        );
    }

    // TDD endpoint resolution bullets (path defaults + scheme dispatch).
    // Pure unit tests on the new helper (no env mutation needed).
    #[test]
    fn otlp_endpoint_path_default_is_v1_traces() {
        let ep = resolve_endpoint("http://collector.example.com").expect("valid OTLP endpoint");
        match ep {
            Endpoint::OtlpHttp(u) => {
                assert_eq!(u.path(), "/v1/traces");
                assert_eq!(u.scheme(), "http");
            }
            _ => panic!("expected OtlpHttp with default path"),
        }
    }

    #[test]
    fn zipkin_endpoint_path_default_is_api_v2_spans() {
        // Zipkin is selected via the zipkin+ or zipkin:// scheme prefix. The
        // `zipkin+` prefix is stripped so the stored URL is a real http(s) URL the HTTP
        // client can POST to.
        let ep = resolve_endpoint("zipkin+https://zipkin.example.com").expect("valid Zipkin endpoint");
        match ep {
            Endpoint::Zipkin(u) => {
                assert_eq!(u.scheme(), "https", "the zipkin+ prefix must be stripped");
                assert_eq!(u.path(), "/api/v2/spans");
            }
            _ => panic!("expected Zipkin with default path"),
        }
    }

    // zipkin+https routes to Zipkin kind.
    #[test]
    fn zipkin_plus_https_scheme_routes_to_zipkin() {
        let ep = resolve_endpoint("zipkin+https://jaeger.example.com:9411").expect("valid Zipkin endpoint");
        assert!(matches!(ep, Endpoint::Zipkin(_)));
    }

    /// Regression: the resolved Zipkin URL must use a real http(s) scheme (not
    /// `zipkin+http`), otherwise `reqwest` cannot POST to it. Covers both the
    /// `zipkin+http://` and bare `zipkin://` forms.
    #[test]
    fn zipkin_endpoint_uses_usable_http_scheme() {
        match resolve_endpoint("zipkin+http://localhost:9411").expect("valid") {
            Endpoint::Zipkin(u) => {
                assert_eq!(u.scheme(), "http");
                assert_eq!(u.host_str(), Some("localhost"));
                assert_eq!(u.port(), Some(9411));
                assert_eq!(u.path(), "/api/v2/spans");
            }
            _ => panic!("expected Zipkin endpoint"),
        }
        match resolve_endpoint("zipkin://localhost:9411").expect("valid") {
            Endpoint::Zipkin(u) => {
                assert_eq!(u.scheme(), "http", "bare zipkin:// maps to http");
                assert_eq!(u.port(), Some(9411));
            }
            _ => panic!("expected Zipkin endpoint"),
        }
    }

    // Named test case: grpc:// and otlp+grpc:// are rejected in v0.1
    #[test]
    fn grpc_scheme_rejected_in_v0_1() {
        let err = resolve_endpoint("grpc://collector:4317").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidExporterEndpoint(_)));
        let msg = err.to_string();
        assert!(msg.contains("gRPC"));
        assert!(msg.contains("v0.1"));
    }

    // Codex review finding: invalid configured endpoints in from_env()
    // (convenience path) must result in a disabled config, not an enabled
    // one with a broken sink.
    #[test]
    fn from_env_disables_on_invalid_endpoint() {
        temp_env::with_vars(
            [
                ("OTEL_SINK_URI", Some("grpc://collector:4317")),
                ("OTEL_SERVICE_NAME", Some("test-service")),
            ],
            || {
                let c = Config::from_env();
                assert!(c.disabled);
                assert!(c.sink_uri.is_none());
                assert!(c.endpoint.is_none());
                assert_eq!(c.exporter, ExporterKind::None);
            },
        );
    }

    // Codex review finding: with_sink_uri on the builder should populate
    // the typed endpoint/exporter fields when resolution succeeds.
    #[test]
    fn with_sink_uri_populates_typed_fields() {
        let c = Config::default().with_sink_uri("zipkin+https://zipkin.example.com");

        assert_eq!(c.sink_uri.as_deref(), Some("zipkin+https://zipkin.example.com"));
        assert!(c.endpoint.is_some());
        assert_eq!(c.exporter, ExporterKind::Zipkin);
    }

    // Codex review finding: OTEL_EXPORTER_OTLP_TRACES_ENDPOINT should take
    // precedence over the generic OTEL_EXPORTER_OTLP_ENDPOINT.
    #[test]
    fn traces_specific_otlp_endpoint_takes_precedence() {
        temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://generic.example.com:4318")),
                (
                    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                    Some("http://traces.example.com:4318"),
                ),
            ],
            || {
                let c = Config::from_env();
                assert_eq!(c.sink_uri.as_deref(), Some("http://traces.example.com:4318"));
            },
        );
    }

    // Critical contract: service name required when a sink is present.
    // Fallible paths must error; convenience paths must silently disable.
    #[test]
    fn try_new_returns_error_on_sink_without_service_name() {
        // We test the underlying Config state that the try_ constructors rely on.
        // (Full Tracer::try_new integration tested via the public API in later
        // bullets.)
        temp_env::with_vars(
            [
                ("OTEL_SINK_URI", Some("http://example.com")),
                // deliberately no OTEL_SERVICE_NAME
            ],
            || {
                let c = Config::from_env();
                assert!(c.sink_uri.is_some());
                assert!(c.service_name.is_none());
                assert!(!c.can_export());
            },
        );
    }

    #[test]
    fn new_silently_disables_on_sink_without_service_name() {
        temp_env::with_vars(
            [
                ("OTEL_SINK_URI", Some("http://example.com")),
                // no service name
            ],
            || {
                let c = Config::from_env();
                // The convenience path will turn this into a disabled config
                // (the actual disabling logic lives in Tracer::new / from_config).
                assert!(c.sink_uri.is_some());
                assert!(c.service_name.is_none());
            },
        );
    }

    // Small end-to-end assertion that the deeper wiring actually populates
    // the typed fields from from_env (using a Zipkin-style SINK_URI).
    #[test]
    fn from_env_populates_typed_endpoint_and_exporter() {
        temp_env::with_var("OTEL_SINK_URI", Some("zipkin+https://jaeger.local:9411"), || {
            let c = Config::from_env();
            assert!(c.endpoint.is_some());
            assert_eq!(c.exporter, ExporterKind::Zipkin);
            if let Some(Endpoint::Zipkin(u)) = &c.endpoint {
                assert!(u.as_str().contains("jaeger.local"));
            } else {
                panic!("expected Zipkin endpoint");
            }
        });
    }

    // BSP override tests (with bad-value fallback behavior)
    #[test]
    fn bsp_schedule_delay_is_parsed_as_milliseconds() {
        // Per the OTEL spec, OTEL_BSP_SCHEDULE_DELAY is in milliseconds.
        temp_env::with_var("OTEL_BSP_SCHEDULE_DELAY", Some("500"), || {
            let c = Config::from_env();
            assert_eq!(c.schedule_delay, Duration::from_millis(500));
        });
        temp_env::with_var("OTEL_BSP_SCHEDULE_DELAY", Some("5000"), || {
            let c = Config::from_env();
            assert_eq!(c.schedule_delay, Duration::from_secs(5));
        });
    }

    #[test]
    fn bsp_schedule_delay_bad_value_falls_back_to_default() {
        temp_env::with_var("OTEL_BSP_SCHEDULE_DELAY", Some("not-a-number"), || {
            let c = Config::from_env();
            // Should fall back to the documented default (5s)
            assert_eq!(c.schedule_delay, Duration::from_secs(5));
        });
    }

    #[test]
    fn otlp_headers_parses_comma_separated_pairs() {
        let headers = parse_headers("a=b,c=d, malformed, x= , =y");
        assert_eq!(
            headers,
            vec![("a".to_string(), "b".to_string()), ("c".to_string(), "d".to_string()),]
        );
        // malformed entries were skipped (we don't assert the logs here)
    }

    #[test]
    fn capture_headers_prefix_empty_means_no_capture() {
        temp_env::with_var("AUSPEX_CAPTURE_HEADERS_PREFIX", Some(""), || {
            let c = Config::from_env();
            assert!(c.capture_header_prefixes.is_empty());
        });

        temp_env::with_var("AUSPEX_CAPTURE_HEADERS_PREFIX", Some("x-request-id, x-trace"), || {
            let c = Config::from_env();
            assert!(!c.capture_header_prefixes.is_empty());
        });
    }

    #[test]
    fn with_resource_attributes_merges_and_overrides_by_key() {
        let c = Config::default()
            .with_resource_attributes([("service.version", "1.0.0"), ("deployment.environment", "dev")])
            .with_resource_attributes([("deployment.environment", "prod"), ("region", "us-east-1")]);
        // Overridden key keeps a single entry with the new value; others survive.
        assert_eq!(
            c.resource_attributes,
            vec![
                ("service.version".to_string(), "1.0.0".to_string()),
                ("deployment.environment".to_string(), "prod".to_string()),
                ("region".to_string(), "us-east-1".to_string()),
            ]
        );
    }

    #[test]
    fn resource_attributes_never_store_service_name() {
        // service.name promotes to the dedicated field (when unset) and is never
        // kept among the resource attributes.
        let c = Config::default().with_resource_attributes([("service.name", "promoted"), ("service.version", "9")]);
        assert_eq!(c.service_name.as_deref(), Some("promoted"));
        assert_eq!(
            c.resource_attributes,
            vec![("service.version".to_string(), "9".to_string())]
        );
    }

    #[test]
    fn explicit_service_name_wins_over_resource_attribute() {
        let c = Config::default()
            .with_service_name("explicit")
            .with_resource_attributes([("service.name", "from-attrs")]);
        assert_eq!(c.service_name.as_deref(), Some("explicit"));
        assert!(c.resource_attributes.is_empty());
    }

    #[test]
    fn from_env_parses_resource_attributes() {
        temp_env::with_var(
            "OTEL_RESOURCE_ATTRIBUTES",
            Some("service.version=0.10.7, deployment.environment=prod , bogus, =noKey, noValue="),
            || {
                let c = Config::from_env();
                // Well-formed pairs kept (trimmed); malformed/empty entries skipped.
                assert_eq!(
                    c.resource_attributes,
                    vec![
                        ("service.version".to_string(), "0.10.7".to_string()),
                        ("deployment.environment".to_string(), "prod".to_string()),
                    ]
                );
            },
        );
    }

    #[test]
    fn from_env_resource_attributes_unset_means_empty() {
        temp_env::with_var("OTEL_RESOURCE_ATTRIBUTES", None::<&str>, || {
            let c = Config::from_env();
            assert!(c.resource_attributes.is_empty());
        });
    }

    #[test]
    fn otel_service_name_wins_over_service_name_in_resource_attributes() {
        temp_env::with_vars(
            [
                ("OTEL_SERVICE_NAME", Some("explicit-svc")),
                (
                    "OTEL_RESOURCE_ATTRIBUTES",
                    Some("service.name=from-attrs,service.version=2"),
                ),
            ],
            || {
                let c = Config::from_env();
                assert_eq!(c.service_name.as_deref(), Some("explicit-svc"));
                assert_eq!(
                    c.resource_attributes,
                    vec![("service.version".to_string(), "2".to_string())]
                );
            },
        );
    }

    #[test]
    fn service_name_promoted_from_resource_attributes_when_otel_service_name_absent() {
        temp_env::with_vars(
            [
                ("OTEL_SERVICE_NAME", None),
                ("OTEL_RESOURCE_ATTRIBUTES", Some("service.name=promoted-svc")),
            ],
            || {
                let c = Config::from_env();
                assert_eq!(c.service_name.as_deref(), Some("promoted-svc"));
                assert!(c.resource_attributes.is_empty());
            },
        );
    }
}
