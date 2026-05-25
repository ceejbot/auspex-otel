//! Minimal axum application wired with auspex, exporting traces over OTLP/HTTP
//! to a local Jaeger all-in-one (or any OTLP/HTTP collector).
//!
//! ## Run it
//!
//! ```text
//! just jaeger-up                 # start Jaeger (OTLP/HTTP :4318, UI :16686)
//! just example                   # run this app wired to Jaeger
//! # ...or manually:
//! OTEL_SERVICE_NAME=auspex-example \
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
//!     cargo run --example basic --features axum
//! ```
//!
//! Then exercise the routes and view the traces at <http://localhost:16686>:
//!
//! ```text
//! curl localhost:3000/
//! curl localhost:3000/hello/world
//! curl localhost:3000/boom        # 500 -> span status Error
//! ```
//!
//! This uses the **explicit subscriber path** (auspex's layer for export plus a
//! `fmt` layer for console output) so you can see what is happening locally.
//! The one-liner `auspex::init()` path works too — see the crate docs.

use std::time::Duration;

use axum::Router;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::get;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Build the tracer from the environment (OTEL_* / OTEL_SINK_URI).
    let tracer = auspex::Tracer::new();

    // Explicit subscriber: auspex exports spans; the fmt layer prints to stdout.
    tracing_subscriber::registry()
        .with(tracer.subscriber_layer())
        .with(tracing_subscriber::fmt::layer())
        .init();

    if tracer.is_enabled() {
        println!("auspex export is ENABLED");
    } else {
        eprintln!(
            "auspex export is DISABLED — set OTEL_EXPORTER_OTLP_ENDPOINT \
             (e.g. http://localhost:4318) or OTEL_SINK_URI to enable it."
        );
    }

    let app = Router::new()
        .route("/", get(root))
        .route("/hello/{name}", get(hello))
        .route("/boom", get(boom))
        // The Tracer is a Tower layer: it creates the per-request root span and
        // handles W3C trace-context propagation.
        .layer(tracer);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    println!("listening on http://localhost:3000  (traces -> http://localhost:16686)");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Root handler: does a little instrumented child work so the trace has depth.
async fn root() -> &'static str {
    do_work().await;
    "hello from auspex\n"
}

/// A child span, exported via auspex's subscriber layer. Shows that
/// `#[tracing::instrument]` spans become children of the HTTP root span.
#[tracing::instrument]
async fn do_work() {
    tracing::info!("doing some work");
    tokio::time::sleep(Duration::from_millis(25)).await;
}

/// Demonstrates low-cardinality route naming: the span is named
/// `GET /hello/{name}` (not the concrete path) thanks to axum `MatchedPath`.
async fn hello(Path(name): Path<String>) -> String {
    format!("hello, {name}\n")
}

/// Returns 500 so you can see the span recorded with OTEL `Error` status.
async fn boom() -> StatusCode {
    tracing::error!("simulated failure in /boom");
    StatusCode::INTERNAL_SERVER_ERROR
}
