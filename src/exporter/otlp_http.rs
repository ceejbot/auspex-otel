//! OTLP/HTTP protobuf exporter.
//!
//! Maps `FinishedSpan` to the OTLP trace protobuf, encodes with `prost`, and
//! POSTs it as `application/x-protobuf`. The message types are vendored under
//! `src/exporter/proto/` (regenerate with `just gen-proto`); see that
//! directory's README for why we don't use the `opentelemetry-proto` crate.
//!
//! Transport (retry, fail-fast on 4xx) is the shared
//! [`super::http::HttpPoster`].

#![allow(clippy::redundant_pub_crate)]

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use prost::Message as _;
use url::Url;

use super::http::HttpPoster;
use super::proto::collector_trace_v1::ExportTraceServiceRequest;
use super::proto::common_v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use super::proto::resource_v1::Resource;
use super::proto::trace_v1;
use super::{ExportError, Exporter};
use crate::propagation::TraceFlags;
use crate::span::{AttributeValue, Event, FinishedSpan, Link, Status};

/// OTLP/HTTP requires the protobuf payload to be sent with this content type.
const PROTOBUF_CONTENT_TYPE: &str = "application/x-protobuf";

/// OTEL spec default when no `service.name` is configured.
const DEFAULT_SERVICE_NAME: &str = "unknown_service";

/// The OTLP/HTTP implementation of the `Exporter` trait.
pub(crate) struct OtlpHttpExporter {
    poster: HttpPoster,
    /// `service.name` resource attribute value.
    service_name: String,
    /// Extra Resource attributes (`service.version`, `deployment.environment`,
    /// …); `service.name` is never among these.
    resource_attributes: Vec<(String, String)>,
}

impl OtlpHttpExporter {
    /// Build an exporter for the resolved endpoint, applying the configured
    /// OTLP headers. Fails if the HTTP client or any header is invalid.
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
            poster: HttpPoster::try_new(endpoint, PROTOBUF_CONTENT_TYPE, headers)?,
            service_name: service_name.unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_owned()),
            resource_attributes,
        })
    }
}

#[async_trait]
impl Exporter for OtlpHttpExporter {
    async fn export(&self, batch: Vec<FinishedSpan>) -> Result<(), ExportError> {
        if batch.is_empty() {
            return Ok(());
        }
        let body = build_export_request(&batch, &self.service_name, &self.resource_attributes).encode_to_vec();
        self.poster.post_with_retry(body).await
    }
}

#[inline]
fn system_time_to_nanos(t: SystemTime) -> u64 {
    // Saturate rather than wrap: nanoseconds-since-epoch overflows u64 in the
    // year 2554, which is well past any realistic span timestamp.
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

fn attribute_value_to_any_value(v: &AttributeValue) -> AnyValue {
    match v {
        AttributeValue::String(s) => AnyValue {
            value: Some(any_value::Value::StringValue(s.to_string())),
        },
        AttributeValue::Int(i) => AnyValue {
            value: Some(any_value::Value::IntValue(*i)),
        },
        AttributeValue::Float(f) => AnyValue {
            value: Some(any_value::Value::DoubleValue(*f)),
        },
        AttributeValue::Bool(b) => AnyValue {
            value: Some(any_value::Value::BoolValue(*b)),
        },
    }
}

fn to_key_value(key: &str, value: &AttributeValue) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(attribute_value_to_any_value(value)),
        ..Default::default()
    }
}

fn attributes_to_proto<'a>(
    attrs: impl IntoIterator<Item = &'a (std::borrow::Cow<'static, str>, AttributeValue)>,
) -> Vec<KeyValue> {
    attrs.into_iter().map(|(k, v)| to_key_value(k, v)).collect()
}

fn event_to_proto(e: &Event) -> trace_v1::span::Event {
    trace_v1::span::Event {
        time_unix_nano: system_time_to_nanos(e.timestamp),
        name: e.name.to_string(),
        attributes: attributes_to_proto(e.attributes.iter()),
        // We enforce attribute limits before this point, so nothing is dropped here.
        dropped_attributes_count: 0,
    }
}

fn link_to_proto(l: &Link) -> trace_v1::span::Link {
    trace_v1::span::Link {
        trace_id: l.trace_id.as_bytes().to_vec(),
        span_id: l.span_id.as_bytes().to_vec(),
        // Our internal Link model carries neither a per-link trace_state nor flags.
        trace_state: String::new(),
        attributes: attributes_to_proto(l.attributes.iter()),
        dropped_attributes_count: 0,
        flags: 0,
    }
}

fn status_to_proto(s: &Status) -> trace_v1::Status {
    use trace_v1::status::StatusCode;
    match s {
        Status::Unset => trace_v1::Status {
            code: StatusCode::Unset as i32,
            message: String::new(),
        },
        Status::Ok => trace_v1::Status {
            code: StatusCode::Ok as i32,
            message: String::new(),
        },
        Status::Error { message } => trace_v1::Status {
            code: StatusCode::Error as i32,
            message: message.to_string(),
        },
    }
}

const fn span_kind_to_proto(k: crate::span::SpanKind) -> trace_v1::span::SpanKind {
    use trace_v1::span::SpanKind as P;
    match k {
        crate::span::SpanKind::Server => P::Server,
        crate::span::SpanKind::Client => P::Client,
        // v0.1 only models Server/Client distinctly; everything else is Internal.
        crate::span::SpanKind::Internal | crate::span::SpanKind::Producer | crate::span::SpanKind::Consumer => {
            P::Internal
        }
    }
}

fn finished_span_to_proto(s: &FinishedSpan) -> trace_v1::Span {
    trace_v1::Span {
        trace_id: s.trace_id.as_bytes().to_vec(),
        span_id: s.span_id.as_bytes().to_vec(),
        parent_span_id: s
            .parent_span_id
            .as_ref()
            .map_or_else(Vec::new, |p| p.as_bytes().to_vec()),
        name: s.name.to_string(),
        kind: span_kind_to_proto(s.kind) as i32,
        start_time_unix_nano: system_time_to_nanos(s.start_time),
        end_time_unix_nano: system_time_to_nanos(s.end_time),
        attributes: attributes_to_proto(s.attributes.iter()),
        dropped_attributes_count: s.dropped_attributes_count,
        events: s.events.iter().map(event_to_proto).collect(),
        dropped_events_count: s.dropped_events_count,
        links: s.links.iter().map(link_to_proto).collect(),
        dropped_links_count: s.dropped_links_count,
        status: Some(status_to_proto(&s.status)),
        trace_state: s
            .trace_state
            .as_ref()
            .map_or_else(String::new, |ts| ts.as_str().to_owned()),
        // W3C trace-flags occupy the low 8 bits of OTLP `Span.flags`; bit 0 is
        // the sampled flag.
        flags: if s.is_sampled {
            u32::from(TraceFlags::SAMPLED)
        } else {
            0
        },
    }
}

fn build_export_request(
    batch: &[FinishedSpan],
    service_name: &str,
    resource_attributes: &[(String, String)],
) -> ExportTraceServiceRequest {
    let spans: Vec<trace_v1::Span> = batch.iter().map(finished_span_to_proto).collect();

    // service.name first, then the configured Resource attributes (which never
    // include service.name — it is stripped upstream in Config).
    let mut attributes = Vec::with_capacity(1 + resource_attributes.len());
    attributes.push(KeyValue {
        key: "service.name".to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(service_name.to_owned())),
        }),
        ..Default::default()
    });
    for (k, v) in resource_attributes {
        attributes.push(to_key_value(k, &AttributeValue::String(v.clone().into())));
    }

    let resource = Resource {
        attributes,
        dropped_attributes_count: 0,
        ..Default::default()
    };

    let scope = InstrumentationScope {
        name: "auspex".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        attributes: vec![],
        dropped_attributes_count: 0,
    };

    let scope_spans = trace_v1::ScopeSpans {
        scope: Some(scope),
        spans,
        schema_url: String::new(),
    };

    let resource_spans = trace_v1::ResourceSpans {
        resource: Some(resource),
        scope_spans: vec![scope_spans],
        schema_url: String::new(),
    };

    ExportTraceServiceRequest {
        resource_spans: vec![resource_spans],
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{SpanId, TraceId};
    use crate::span::{Event, Link, OtelSpan};

    /// Required resource attributes (`service.name`) are present in the
    /// payload.
    #[test]
    fn payload_includes_required_resource_attributes() {
        let span = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s");
        let req = build_export_request(&[span.finish()], "checkout-api", &[]);
        let decoded = ExportTraceServiceRequest::decode(req.encode_to_vec().as_slice()).expect("roundtrip decode");

        let resource = decoded.resource_spans[0].resource.as_ref().expect("resource present");
        let service_name = resource
            .attributes
            .iter()
            .find(|kv| kv.key == "service.name")
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.as_ref());
        assert!(
            matches!(service_name, Some(any_value::Value::StringValue(s)) if s == "checkout-api"),
            "service.name should be the configured value"
        );
    }

    /// Configured Resource attributes ride alongside `service.name`, as typed
    /// `StringValue`s, with exactly one `service.name` key.
    #[test]
    fn payload_includes_configured_resource_attributes() {
        let span = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s");
        let attrs = vec![
            ("service.version".to_owned(), "0.10.7".to_owned()),
            ("deployment.environment".to_owned(), "prod".to_owned()),
        ];
        let req = build_export_request(&[span.finish()], "checkout-api", &attrs);
        let decoded = ExportTraceServiceRequest::decode(req.encode_to_vec().as_slice()).expect("roundtrip decode");
        let resource = decoded.resource_spans[0].resource.as_ref().expect("resource present");

        let string_attr = |key: &str| -> Option<String> {
            resource
                .attributes
                .iter()
                .find(|kv| kv.key == key)
                .and_then(|kv| kv.value.as_ref())
                .and_then(|v| v.value.as_ref())
                .and_then(|v| match v {
                    any_value::Value::StringValue(s) => Some(s.clone()),
                    _ => None,
                })
        };

        assert_eq!(string_attr("service.version").as_deref(), Some("0.10.7"));
        assert_eq!(string_attr("deployment.environment").as_deref(), Some("prod"));
        // Exactly one service.name key — no duplication from the attribute list.
        assert_eq!(
            resource.attributes.iter().filter(|kv| kv.key == "service.name").count(),
            1
        );
    }

    /// A configured OTLP header is applied (alongside the required
    /// content-type).
    #[test]
    fn otlp_headers_from_config_applied_to_request() {
        let exporter = OtlpHttpExporter::try_new(
            Url::parse("http://localhost:4318/v1/traces").expect("url"),
            &[("x-api-key".to_owned(), "secret".to_owned())],
            Some("svc".to_owned()),
            Vec::new(),
        )
        .expect("exporter builds");

        let headers = exporter.poster.headers();
        assert_eq!(headers.get("x-api-key").and_then(|v| v.to_str().ok()), Some("secret"));
        assert_eq!(
            headers.get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some(PROTOBUF_CONTENT_TYPE),
        );
    }

    /// An invalid configured header name is reported at construction.
    #[test]
    fn invalid_header_name_fails_construction() {
        let result = OtlpHttpExporter::try_new(
            Url::parse("http://localhost:4318/v1/traces").expect("url"),
            &[("bad header".to_owned(), "v".to_owned())],
            None,
            Vec::new(),
        );
        assert!(result.is_err(), "a header name with a space is not valid HTTP");
    }

    /// The full mapping (attributes, events, links) survives a prost
    /// encode -> decode roundtrip against the vendored generated types.
    #[test]
    fn mapping_roundtrips_through_prost() {
        let trace_id = TraceId::generate();
        let span_id = SpanId::generate();
        let mut span = OtelSpan::new(trace_id, span_id, None, "decode-test");
        span.record("foo", true);

        // Populate an event and a link (the part the earlier spike skipped).
        span.events.push(Event {
            name: "exception".into(),
            timestamp: SystemTime::now(),
            attributes: smallvec_of(&[("exception.type", AttributeValue::String("Boom".into()))]),
        });
        let linked_trace = TraceId::generate();
        let linked_span = SpanId::generate();
        span.links.push(Link {
            trace_id: linked_trace,
            span_id: linked_span,
            attributes: smallvec_of(&[("link.kind", AttributeValue::Int(1))]),
        });

        let finished = span.finish();
        let req = build_export_request(&[finished], "svc", &[]);
        let bytes = req.encode_to_vec();

        let decoded = ExportTraceServiceRequest::decode(bytes.as_slice())
            .expect("should roundtrip decode with the same prost types");

        assert_eq!(decoded.resource_spans.len(), 1);
        let spans = &decoded.resource_spans[0].scope_spans[0].spans;
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.name, "decode-test");
        assert_eq!(span.trace_id, trace_id.as_bytes().to_vec());
        assert_eq!(span.span_id, span_id.as_bytes().to_vec());

        assert_eq!(span.events.len(), 1, "the event should survive the roundtrip");
        assert_eq!(span.events[0].name, "exception");
        assert_eq!(span.events[0].attributes[0].key, "exception.type");

        assert_eq!(span.links.len(), 1, "the link should survive the roundtrip");
        assert_eq!(span.links[0].trace_id, linked_trace.as_bytes().to_vec());
        assert_eq!(span.links[0].span_id, linked_span.as_bytes().to_vec());
        assert_eq!(span.links[0].attributes[0].key, "link.kind");
    }

    /// The W3C sampled flag round-trips into the OTLP `Span.flags` byte.
    #[test]
    fn payload_preserves_sampled_flag_on_span_flags() {
        for sampled in [true, false] {
            let mut span = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s");
            span.is_sampled = sampled;
            let req = build_export_request(&[span.finish()], "svc", &[]);
            let decoded = ExportTraceServiceRequest::decode(req.encode_to_vec().as_slice()).expect("roundtrip decode");
            let flags = decoded.resource_spans[0].scope_spans[0].spans[0].flags;
            let sampled_bit = flags & u32::from(TraceFlags::SAMPLED) != 0;
            assert_eq!(sampled_bit, sampled, "sampled flag should map to span flags bit 0");
        }
    }

    /// A span's `tracestate` survives the mapping into the OTLP payload.
    #[test]
    fn payload_preserves_tracestate_on_span() {
        let mut span = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s");
        span.trace_state = crate::propagation::TraceState::from_header("vendor=abc123");
        assert!(span.trace_state.is_some(), "test fixture should be a valid tracestate");

        let req = build_export_request(&[span.finish()], "svc", &[]);
        let decoded = ExportTraceServiceRequest::decode(req.encode_to_vec().as_slice()).expect("roundtrip decode");
        assert_eq!(
            decoded.resource_spans[0].scope_spans[0].spans[0].trace_state,
            "vendor=abc123",
        );
    }

    /// Dropped attribute/event/link counts survive the mapping.
    #[test]
    fn payload_includes_dropped_counts() {
        let mut span = OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s");
        span.dropped_attributes_count = 3;
        span.dropped_events_count = 5;
        span.dropped_links_count = 7;

        let req = build_export_request(&[span.finish()], "svc", &[]);
        let decoded = ExportTraceServiceRequest::decode(req.encode_to_vec().as_slice()).expect("roundtrip decode");
        let span = &decoded.resource_spans[0].scope_spans[0].spans[0];
        assert_eq!(span.dropped_attributes_count, 3);
        assert_eq!(span.dropped_events_count, 5);
        assert_eq!(span.dropped_links_count, 7);
    }

    // --- Transport tests against a fake HTTP server (wiremock) ---

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn a_finished_span() -> FinishedSpan {
        OtelSpan::new(TraceId::generate(), SpanId::generate(), None, "s").finish()
    }

    fn exporter_for(server: &MockServer, headers: &[(String, String)]) -> OtlpHttpExporter {
        let endpoint = Url::parse(&format!("{}/v1/traces", server.uri())).expect("endpoint url");
        OtlpHttpExporter::try_new(endpoint, headers, Some("svc".to_owned()), Vec::new()).expect("exporter builds")
    }

    /// A transient 500 is retried, and the export ultimately succeeds.
    #[tokio::test]
    async fn transient_500_triggers_retry_then_succeeds() {
        let server = MockServer::start().await;

        // wiremock matches the first-mounted mock that still has budget. Mount
        // the single 500 first so it answers attempt #1, then the catch-all 200
        // answers the retry.
        Mock::given(method("POST"))
            .and(path("/v1/traces"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/traces"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let exporter = exporter_for(&server, &[]);
        let result = exporter.export(vec![a_finished_span()]).await;

        assert!(
            result.is_ok(),
            "export should succeed after the 500 is retried: {result:?}"
        );
        // MockServer verifies both `.expect(1)`s on drop.
    }

    /// When every attempt fails, the retry budget is exhausted (3 total
    /// attempts) and the batch is dropped (export returns Err). The
    /// `BatchWorker` is what rate-limits the warning; here we assert the
    /// exporter's give-up contract.
    #[tokio::test]
    async fn retry_budget_exhausted_drops_batch_and_logs_once() {
        let server = MockServer::start().await;

        // 1 initial attempt + 2 retries = 3 POSTs, all 500.
        Mock::given(method("POST"))
            .and(path("/v1/traces"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3)
            .mount(&server)
            .await;

        let exporter = exporter_for(&server, &[]);
        let result = exporter.export(vec![a_finished_span()]).await;

        assert!(result.is_err(), "export should fail after exhausting the retry budget");
        // MockServer verifies exactly 3 attempts were made (on drop).
    }

    /// A 4xx is permanent: fail fast with no retry (exactly one attempt).
    #[tokio::test]
    async fn permanent_4xx_fails_fast_without_retry() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/traces"))
            .respond_with(ResponseTemplate::new(400))
            .expect(1) // no retries
            .mount(&server)
            .await;

        let exporter = exporter_for(&server, &[]);
        let result = exporter.export(vec![a_finished_span()]).await;

        assert!(result.is_err(), "a 400 should fail");
    }

    /// Configured OTLP headers reach the server on the actual request.
    #[tokio::test]
    async fn otlp_headers_reach_the_server() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/traces"))
            .and(header("x-api-key", "secret"))
            .and(header("content-type", PROTOBUF_CONTENT_TYPE))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let exporter = exporter_for(&server, &[("x-api-key".to_owned(), "secret".to_owned())]);
        let result = exporter.export(vec![a_finished_span()]).await;

        assert!(
            result.is_ok(),
            "export with configured headers should succeed: {result:?}"
        );
    }

    fn smallvec_of(
        pairs: &[(&'static str, AttributeValue)],
    ) -> smallvec::SmallVec<[(std::borrow::Cow<'static, str>, AttributeValue); 4]> {
        pairs.iter().map(|(k, v)| ((*k).into(), v.clone())).collect()
    }
}
