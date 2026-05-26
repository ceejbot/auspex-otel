//! Zipkin v2 JSON exporter (Task 6.3).
//!
//! Serializes `FinishedSpan`s to the [Zipkin v2 JSON span format][zipkin] and
//! POSTs them (as a JSON array) to the configured endpoint — typically a local
//! Jaeger all-in-one on `:9411/api/v2/spans` for the "see your traces locally"
//! path. Transport (retry, headers, timeout) is the shared
//! [`super::http::HttpPoster`].
//!
//! Lossiness worth knowing: Zipkin tag values are strings, so non-string
//! attributes are coerced via `to_string`. Zipkin v2 has no link concept, so
//! links are emitted as annotations (timestamped strings). The W3C sampled bit
//! has no native Zipkin field, so it is preserved as an `otel.sampled` tag.
//!
//! [zipkin]: https://zipkin.io/zipkin-api/#/default/post_spans

#![allow(clippy::redundant_pub_crate)] // private module; pub(crate) is the intent

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Serialize;
use url::Url;

use super::http::HttpPoster;
use super::{ExportError, Exporter};
use crate::span::{AttributeValue, Event, FinishedSpan, Link, SpanKind, Status};

/// Zipkin ingest expects JSON.
const JSON_CONTENT_TYPE: &str = "application/json";

/// Default `localEndpoint.serviceName` when none is configured.
const DEFAULT_SERVICE_NAME: &str = "unknown_service";

/// The Zipkin v2 JSON implementation of the `Exporter` trait.
pub(crate) struct ZipkinExporter {
    poster: HttpPoster,
    service_name: String,
    /// Extra Resource attributes, emitted as Zipkin tags. `service.name` is
    /// never among these (it maps to `localEndpoint.serviceName`).
    resource_attributes: Vec<(String, String)>,
}

impl ZipkinExporter {
    /// Build an exporter for the resolved Zipkin endpoint.
    ///
    /// # Errors
    /// Returns [`ExportError`] if the `reqwest` client cannot be built or a
    /// configured header name/value is not valid HTTP.
    pub(crate) fn try_new(
        endpoint: Url,
        headers: &[(String, String)],
        service_name: Option<String>,
        resource_attributes: Vec<(String, String)>,
    ) -> Result<Self, ExportError> {
        Ok(Self {
            poster: HttpPoster::try_new(endpoint, JSON_CONTENT_TYPE, headers)?,
            service_name: service_name.unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_owned()),
            resource_attributes,
        })
    }
}

#[async_trait]
impl Exporter for ZipkinExporter {
    async fn export(&self, batch: Vec<FinishedSpan>) -> Result<(), ExportError> {
        if batch.is_empty() {
            return Ok(());
        }
        let spans: Vec<ZipkinSpan> = batch
            .iter()
            .map(|s| ZipkinSpan::from_finished(s, &self.service_name, &self.resource_attributes))
            .collect();
        let body =
            serde_json::to_vec(&spans).map_err(|e| ExportError::new(format!("zipkin JSON encode failed: {e}")))?;
        self.poster.post_with_retry(body).await
    }
}

#[derive(Serialize)]
struct ZipkinSpan {
    #[serde(rename = "traceId")]
    trace_id: String,
    id: String,
    #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
    name: String,
    /// `SERVER`/`CLIENT`/`PRODUCER`/`CONSUMER`; omitted for internal spans.
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    /// Microseconds since the Unix epoch.
    timestamp: u64,
    /// Span duration in microseconds.
    duration: u64,
    #[serde(rename = "localEndpoint")]
    local_endpoint: LocalEndpoint,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    tags: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    annotations: Vec<Annotation>,
}

#[derive(Serialize)]
struct LocalEndpoint {
    #[serde(rename = "serviceName")]
    service_name: String,
}

#[derive(Serialize)]
struct Annotation {
    timestamp: u64,
    value: String,
}

impl ZipkinSpan {
    fn from_finished(s: &FinishedSpan, service_name: &str, resource_attributes: &[(String, String)]) -> Self {
        // Resource attributes seed the tags; span attributes are layered on top
        // so a per-span value always wins over a process-identity tag.
        let mut tags: BTreeMap<String, String> = resource_attributes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        tags.extend(
            s.attributes
                .iter()
                .map(|(k, v)| (k.to_string(), attribute_value_to_string(v))),
        );

        // Zipkin's conventional error signal (Jaeger renders these red).
        if let Status::Error { message } = &s.status {
            let value = if message.is_empty() {
                "true".to_owned()
            } else {
                message.to_string()
            };
            tags.insert("error".to_owned(), value);
        }
        // No native Zipkin field for the W3C sampled bit; preserve it as a tag.
        tags.insert("otel.sampled".to_owned(), s.is_sampled.to_string());

        // Events -> annotations; links -> annotations (no native Zipkin concept).
        let mut annotations: Vec<Annotation> = s.events.iter().map(Annotation::from_event).collect();
        annotations.extend(s.links.iter().map(Annotation::from_link));

        Self {
            trace_id: s.trace_id.to_string(),
            id: s.span_id.to_string(),
            parent_id: s.parent_span_id.as_ref().map(ToString::to_string),
            name: s.name.to_string(),
            kind: zipkin_kind(s.kind),
            timestamp: micros_since_epoch(s.start_time),
            duration: duration_micros(s.start_time, s.end_time),
            local_endpoint: LocalEndpoint {
                service_name: service_name.to_owned(),
            },
            tags,
            annotations,
        }
    }
}

impl Annotation {
    fn from_event(e: &Event) -> Self {
        Self {
            timestamp: micros_since_epoch(e.timestamp),
            value: e.name.to_string(),
        }
    }

    fn from_link(l: &Link) -> Self {
        // Zipkin has no link concept; record the causal reference as an
        // annotation valued at the linked trace/span.
        Self {
            timestamp: 0,
            value: format!("link:{}/{}", l.trace_id, l.span_id),
        }
    }
}

/// Map our `SpanKind` to Zipkin's string enum. Internal spans omit `kind`.
const fn zipkin_kind(kind: SpanKind) -> Option<&'static str> {
    match kind {
        SpanKind::Server => Some("SERVER"),
        SpanKind::Client => Some("CLIENT"),
        SpanKind::Producer => Some("PRODUCER"),
        SpanKind::Consumer => Some("CONSUMER"),
        SpanKind::Internal => None,
    }
}

/// Coerce an attribute value to a string (Zipkin tags are string-valued).
fn attribute_value_to_string(v: &AttributeValue) -> String {
    match v {
        AttributeValue::String(s) => s.to_string(),
        AttributeValue::Int(i) => i.to_string(),
        AttributeValue::Float(f) => f.to_string(),
        AttributeValue::Bool(b) => b.to_string(),
    }
}

fn micros_since_epoch(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

fn duration_micros(start: SystemTime, end: SystemTime) -> u64 {
    end.duration_since(start)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{SpanId, TraceId};
    use crate::span::OtelSpan;

    fn finished(name: &'static str) -> FinishedSpan {
        OtelSpan::new(TraceId::generate(), SpanId::generate(), None, name).finish()
    }

    /// The serialized payload is a JSON array of well-formed Zipkin v2 spans.
    #[test]
    fn serialized_payload_is_valid_zipkin_v2_json() {
        let span = ZipkinSpan::from_finished(&finished("GET /"), "svc", &[]);
        let json = serde_json::to_value([span]).expect("serialize");

        let arr = json.as_array().expect("top level is an array");
        assert_eq!(arr.len(), 1);
        let s = &arr[0];
        assert_eq!(
            s["traceId"].as_str().map(str::len),
            Some(32),
            "128-bit trace id, 32 hex chars"
        );
        assert_eq!(s["id"].as_str().map(str::len), Some(16), "64-bit span id, 16 hex chars");
        assert_eq!(s["name"], "GET /");
        assert_eq!(s["localEndpoint"]["serviceName"], "svc");
        assert!(s["timestamp"].is_u64(), "timestamp is microseconds (number)");
    }

    /// Tags include every attribute, with non-string values coerced to strings.
    #[test]
    fn tags_include_all_finished_span_attributes() {
        let mut live = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s");
        live.record("str", "v");
        live.record("int", 42i64);
        live.record("float", 1.5f64);
        live.record("bool", true);

        let span = ZipkinSpan::from_finished(&live.finish(), "svc", &[]);
        assert_eq!(span.tags.get("str").map(String::as_str), Some("v"));
        assert_eq!(span.tags.get("int").map(String::as_str), Some("42"));
        assert_eq!(span.tags.get("float").map(String::as_str), Some("1.5"));
        assert_eq!(span.tags.get("bool").map(String::as_str), Some("true"));
    }

    /// Resource attributes are emitted as tags; `service.name` stays in
    /// `localEndpoint.serviceName` and is not duplicated into tags.
    #[test]
    fn resource_attributes_appear_as_tags_and_service_name_stays_in_endpoint() {
        let attrs = vec![
            ("service.version".to_owned(), "0.10.7".to_owned()),
            ("deployment.environment".to_owned(), "prod".to_owned()),
        ];
        let span = ZipkinSpan::from_finished(&finished("GET /"), "checkout-api", &attrs);

        assert_eq!(span.tags.get("service.version").map(String::as_str), Some("0.10.7"));
        assert_eq!(
            span.tags.get("deployment.environment").map(String::as_str),
            Some("prod")
        );
        assert_eq!(span.local_endpoint.service_name, "checkout-api");
        assert!(
            !span.tags.contains_key("service.name"),
            "service.name must not be a tag"
        );
    }

    /// `SpanKind::Server` maps to Zipkin `SERVER`; internal omits `kind`.
    #[test]
    fn span_kind_server_is_mapped_to_zipkin_server_kind() {
        let mut live = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s");
        live.kind = SpanKind::Server;
        assert_eq!(
            ZipkinSpan::from_finished(&live.finish(), "svc", &[]).kind,
            Some("SERVER")
        );

        let internal = finished("s"); // default kind is Internal
        assert_eq!(ZipkinSpan::from_finished(&internal, "svc", &[]).kind, None);
    }

    /// The sampled flag is preserved (as an `otel.sampled` tag).
    #[test]
    fn sampled_flag_preserved_in_zipkin_payload() {
        for sampled in [true, false] {
            let mut live = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s");
            live.is_sampled = sampled;
            let span = ZipkinSpan::from_finished(&live.finish(), "svc", &[]);
            assert_eq!(
                span.tags.get("otel.sampled").map(String::as_str),
                Some(sampled.to_string().as_str())
            );
        }
    }

    /// Links are preserved as annotations referencing the linked trace/span.
    #[test]
    fn links_preserved_as_zipkin_annotations() {
        let mut live = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s");
        let linked_trace = TraceId::generate();
        let linked_span = SpanId::generate();
        live.links.push(Link {
            trace_id: linked_trace,
            span_id: linked_span,
            attributes: smallvec::SmallVec::new(),
        });

        let span = ZipkinSpan::from_finished(&live.finish(), "svc", &[]);
        let expected = format!("link:{linked_trace}/{linked_span}");
        assert!(
            span.annotations.iter().any(|a| a.value == expected),
            "expected an annotation referencing the link, got {:?}",
            span.annotations.iter().map(|a| &a.value).collect::<Vec<_>>()
        );
    }

    /// An OTEL Error status becomes the conventional Zipkin `error` tag.
    #[test]
    fn error_status_becomes_zipkin_error_tag() {
        let mut live = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s");
        live.status = Status::Error { message: "boom".into() };
        let span = ZipkinSpan::from_finished(&live.finish(), "svc", &[]);
        assert_eq!(span.tags.get("error").map(String::as_str), Some("boom"));
    }

    // --- Transport against a fake server ---

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A real POST reaches `/api/v2/spans` with `application/json`, and the
    /// body is a JSON array of spans.
    #[tokio::test]
    async fn zipkin_posts_json_array_to_server() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/spans"))
            .and(header("content-type", JSON_CONTENT_TYPE))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;

        let endpoint = Url::parse(&format!("{}/api/v2/spans", server.uri())).expect("url");
        let exporter = ZipkinExporter::try_new(endpoint, &[], Some("svc".to_owned()), Vec::new()).expect("build");

        let result = exporter.export(vec![finished("GET /")]).await;
        assert!(result.is_ok(), "zipkin export should succeed: {result:?}");

        // The single received request's body should be a JSON array.
        let requests = server.received_requests().await.expect("recording enabled");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
        assert!(body.is_array(), "zipkin body must be a JSON array of spans");
    }
}
