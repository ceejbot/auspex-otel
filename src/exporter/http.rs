//! Shared HTTP POST-with-retry transport for the wire-format exporters.
//!
//! Both the OTLP/HTTP (protobuf) and Zipkin (JSON) exporters do the same thing
//! once they have an encoded body: POST it to a configured endpoint with a set
//! of headers, retrying transient failures with jittered backoff. That common
//! machinery lives here so each exporter only has to build its own body and
//! pick a content type.

#![allow(clippy::redundant_pub_crate)] // private module; pub(crate) is the intent

use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use url::Url;

use super::ExportError;
use super::retry::{Backoff, jitter_fraction};

/// Request timeout for a single export POST. Fixed for v0.1 (no env knob).
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a single POST attempt tells us to do next.
enum Disposition {
    Success,
    /// Worth another attempt (5xx, 429, or a transport error). Carries a
    /// human-readable reason for the eventual give-up message.
    Retryable(String),
    /// Definitive failure (4xx other than 429). Do not retry.
    Permanent(String),
}

/// A configured HTTP client + endpoint + headers + retry policy. Cheap to build
/// once per exporter; `reqwest::Client` is internally reference-counted.
pub(crate) struct HttpPoster {
    client: reqwest::Client,
    endpoint: Url,
    headers: HeaderMap,
    backoff: Backoff,
}

impl HttpPoster {
    /// Build a poster for `endpoint`, seeding the header map with
    /// `content_type` plus the caller-configured `headers`.
    ///
    /// # Errors
    /// Returns [`ExportError`] if the `reqwest` client cannot be built or a
    /// configured header name/value is not valid HTTP.
    pub(crate) fn try_new(
        endpoint: Url,
        content_type: &'static str,
        headers: &[(String, String)],
    ) -> Result<Self, ExportError> {
        // We compile rustls with only the `ring` provider, but a consuming app
        // may enable `aws-lc-rs` elsewhere in its tree; with both features
        // present rustls cannot infer a default and reqwest's builder panics.
        // Installing ring as the process default (a no-op if the app already
        // installed one) keeps client construction infallible either way.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let client = reqwest::Client::builder()
            .timeout(EXPORT_TIMEOUT)
            .build()
            .map_err(|e| ExportError::new(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            endpoint,
            headers: build_header_map(content_type, headers)?,
            backoff: Backoff::default(),
        })
    }

    /// The configured request headers (test-only introspection).
    #[cfg(test)]
    pub(crate) const fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// POST `body`, retrying transient failures per the backoff policy. Returns
    /// `Err` on a permanent rejection or once the retry budget is exhausted;
    /// the `BatchWorker` then drops the batch with a rate-limited warning.
    pub(crate) async fn post_with_retry(&self, body: Vec<u8>) -> Result<(), ExportError> {
        let mut attempt: u32 = 0;
        loop {
            let reason = match self.send_once(body.clone()).await {
                Disposition::Success => return Ok(()),
                Disposition::Permanent(reason) => {
                    return Err(ExportError::new(format!("export rejected: {reason}")));
                }
                Disposition::Retryable(reason) => reason,
            };

            attempt += 1;
            match self.backoff.delay_for(attempt, jitter_fraction()) {
                Some(delay) => tokio::time::sleep(delay).await,
                None => {
                    return Err(ExportError::new(format!(
                        "export failed after {} attempts: {reason}",
                        self.backoff.max_retries() + 1
                    )));
                }
            }
        }
    }

    /// Send the body once and classify the result.
    async fn send_once(&self, body: Vec<u8>) -> Disposition {
        let response = self
            .client
            .post(self.endpoint.clone())
            .headers(self.headers.clone())
            .body(body)
            .send()
            .await;

        match response {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    Disposition::Success
                } else if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    Disposition::Retryable(format!("server returned {status}"))
                } else {
                    Disposition::Permanent(format!("server returned {status}"))
                }
            }
            // Transport-level errors (connect, timeout, ...) are transient.
            Err(e) => Disposition::Retryable(format!("transport error: {e}")),
        }
    }
}

/// Parse configured headers into a `HeaderMap`, seeded with the content type.
fn build_header_map(content_type: &'static str, headers: &[(String, String)]) -> Result<HeaderMap, ExportError> {
    let mut map = HeaderMap::with_capacity(headers.len() + 1);
    map.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));

    for (name, value) in headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| ExportError::new(format!("invalid header name {name:?}: {e}")))?;
        let header_value = HeaderValue::from_str(value)
            .map_err(|e| ExportError::new(format!("invalid header value for {name:?}: {e}")))?;
        map.insert(header_name, header_value);
    }
    Ok(map)
}
