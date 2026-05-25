//! Records `tracing` span fields into an `OtelSpan` (the attribute visitor).

use std::borrow::Cow;
use std::fmt;

use tracing::field::{Field, Visit};

use crate::span::{AttributeValue, OtelSpan, SpanKind, Status};

/// Converts a `u64` to an `AttributeValue`.
/// Stores as `i64` when it fits safely; otherwise falls back to a debug string.
/// Shared lightly between `FieldVisitor` and `EventVisitor`.
fn u64_to_attribute_value(v: u64) -> AttributeValue {
    i64::try_from(v).map_or_else(
        |_| AttributeValue::String(Cow::Owned(format!("{v}"))),
        AttributeValue::Int,
    )
}

/// Borrow well-known fixed-vocabulary values (HTTP methods, URL schemes, the
/// `http` protocol name, the empty string) instead of owning them, saving a
/// per-request allocation on the hot path. Everything else is owned. The stored
/// content is identical either way.
pub fn intern_wellknown(value: &str) -> Cow<'static, str> {
    match value {
        "" => Cow::Borrowed(""),
        // HTTP methods (RFC 9110 + PATCH).
        "GET" => Cow::Borrowed("GET"),
        "HEAD" => Cow::Borrowed("HEAD"),
        "POST" => Cow::Borrowed("POST"),
        "PUT" => Cow::Borrowed("PUT"),
        "DELETE" => Cow::Borrowed("DELETE"),
        "CONNECT" => Cow::Borrowed("CONNECT"),
        "OPTIONS" => Cow::Borrowed("OPTIONS"),
        "TRACE" => Cow::Borrowed("TRACE"),
        "PATCH" => Cow::Borrowed("PATCH"),
        // URL schemes + protocol name.
        "http" => Cow::Borrowed("http"),
        "https" => Cow::Borrowed("https"),
        other => Cow::Owned(other.to_owned()),
    }
}

/// Visitor that records `tracing` span fields into an `OtelSpan`.
///
/// Special control fields starting with `otel.` are handled specially:
/// - `otel.name` → updates the span name (not exported as attribute)
/// - `otel.kind` → sets the span kind (not exported)
/// - `otel.status_code` / `otel.status_message` → sets OTEL status (not
///   exported)
pub struct FieldVisitor<'a> {
    pub(crate) span: &'a mut OtelSpan,
}

impl Visit for FieldVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        // Borrow well-known fixed-vocabulary values (methods, schemes, ...) to
        // avoid a per-request allocation; own everything else.
        self.handle_special_field(field, AttributeValue::String(intern_wellknown(value)));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.handle_special_field(field, value.into());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.handle_special_field(field, value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.handle_special_field(field, u64_to_attribute_value(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.handle_special_field(field, value.into());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let msg = value.to_string();
        self.handle_special_field(field, msg.into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.handle_special_field(field, format!("{value:?}").into());
    }
}

impl FieldVisitor<'_> {
    fn handle_special_field(&mut self, field: &Field, value: AttributeValue) {
        match field.name() {
            "otel.name" => {
                if let AttributeValue::String(s) = value {
                    self.span.name = s;
                }
            }
            "otel.kind" => {
                if let AttributeValue::String(s) = &value {
                    self.span.kind = match s.as_ref() {
                        "server" => SpanKind::Server,
                        "client" => SpanKind::Client,
                        "producer" => SpanKind::Producer,
                        "consumer" => SpanKind::Consumer,
                        _ => SpanKind::Internal,
                    };
                }
            }
            "otel.status_code" => {
                if let AttributeValue::String(s) = &value {
                    // Look for a pending message (may have been recorded before or after
                    // this status_code field). We steal it so the otel.* key never leaks
                    // into exported attributes.
                    let message = self
                        .span
                        .attributes
                        .iter()
                        .find(|(k, _)| k == "otel.status_message")
                        .and_then(|(_, v)| match v {
                            AttributeValue::String(m) => Some(m.clone()),
                            _ => None,
                        });

                    // Remove the control field from attributes if it was present.
                    if let Some(pos) = self
                        .span
                        .attributes
                        .iter()
                        .position(|(k, _)| k == "otel.status_message")
                    {
                        self.span.attributes.remove(pos);
                    }

                    self.span.status = match s.as_ref() {
                        "ok" | "OK" => Status::Ok,
                        "error" | "ERROR" => Status::Error {
                            message: message.unwrap_or_else(|| "".into()),
                        },
                        _ => Status::Unset,
                    };
                }
            }
            "otel.status_message" => {
                if matches!(self.span.status, Status::Error { .. })
                    && let AttributeValue::String(s) = value
                {
                    self.span.status = Status::Error { message: s };
                }
                // If not yet Error, we intentionally drop the message here
                // (it will be picked up when/if
                // status_code=error arrives later). Never store
                // otel.* control fields as normal attributes.
                // Do nothing in the non-Error case: control field is never
                // exported.
            }
            // Dynamic response-header keys arrive funneled through one declared
            // field as `name=value` (header names never contain '='). Split and
            // store under the real OTEL `http.response.header.<name>` key.
            "http.response.header" => {
                if let AttributeValue::String(s) = value
                    && let Some((name, val)) = s.split_once('=')
                {
                    self.span.record(format!("http.response.header.{name}"), val.to_owned());
                }
            }
            // `remote.*` are internal control fields: the layer reads them in
            // `on_new_span` to adopt a remote parent. They must never leak into
            // the exported attribute set.
            "remote.trace_id" | "remote.parent_span_id" | "remote.sampled" | "remote.trace_state" => {}
            _ => self.record_normal_field(field, value),
        }
    }

    fn record_normal_field(&mut self, field: &Field, value: AttributeValue) {
        self.span.record(field.name(), value);
    }
}

/// Visitor that turns a `tracing::Event` into an `Event` for OTEL.
///
/// This is deliberately separate from `FieldVisitor`:
/// - It does not participate in the span's attribute budget or protected-key
///   logic.
/// - `otel.*` control fields are ignored (they only affect the containing
///   span).
/// - The "message" field (if present) is used as the event name; everything
///   else becomes attributes on the event.
pub struct EventVisitor {
    pub name: Cow<'static, str>,
    pub attributes: smallvec::SmallVec<[(Cow<'static, str>, AttributeValue); 4]>,
}

impl Default for EventVisitor {
    fn default() -> Self {
        Self {
            name: Cow::Borrowed("event"),
            attributes: smallvec::SmallVec::new(),
        }
    }
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.name = Cow::Owned(value.to_owned());
        } else {
            self.attributes
                .push((field.name().to_string().into(), value.to_owned().into()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() != "message" {
            self.attributes
                .push((field.name().to_string().into(), format!("{value:?}").into()));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() != "message" {
            self.attributes.push((field.name().to_string().into(), value.into()));
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() != "message" {
            self.attributes
                .push((field.name().to_string().into(), u64_to_attribute_value(value)));
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() != "message" {
            self.attributes.push((field.name().to_string().into(), value.into()));
        }
    }
}
