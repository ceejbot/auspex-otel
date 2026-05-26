# auspex

[![Tests](https://github.com/ceejbot/auspex-otel/actions/workflows/test.yaml/badge.svg)](https://github.com/ceejbot/auspex-otel/actions/workflows/test.yaml)

Extremely opinionated, lean OTEL tracing middleware for [axum](https://lib.rs/crates/axum).

`auspex` offers almost none of the knobs the [tracing-opentelemetry](https://lib.rs/crates/tracing-opentelemetry)
ecosystem does. In return it gives you **trivial setup** (env vars + one
`.layer()`), it stays out of your logging and metrics, and it does none of the
work it can avoid on the request path. The goal is to make OTEL tracing in an
`axum` stack easy and cheap, instead of a research project.

It is tracing-only and built on the [`tracing`](https://lib.rs/crates/tracing)
facade. You instrument with `#[tracing::instrument]`, `info_span!`,
`Span::record`, events, and `follows_from` exactly as you already do; auspex
turns that into exported OTEL spans.

**TL;DR:** if you want OTEL-compliant traces out of an `axum` service without the
baroque complexity of the full OpenTelemetry SDK, this is for you.

If you need flexibility — custom samplers, metrics, log bridging, exotic
exporters — this is **not** the library for you; use `tracing-opentelemetry`
directly. If you need different choices, fork it and hack in yours (it's
dual-licensed MIT/Apache-2.0; sharing your work back is appreciated but not
required).

## Why it's lean

Measured against its own targets and against the mainstream stack
(`opentelemetry_sdk` + `tracing-opentelemetry`) on the same workload — full
numbers and method in [`docs/benchmarks/v0.1.md`](docs/benchmarks/v0.1.md):

- **~2.7 µs p99 added latency** for an enabled HTTP root span (target was
  < 50 µs).
- **~4× fewer heap allocations per span** than the OpenTelemetry SDK stack
  (auspex 5 vs 19 on the comparison bench). The few allocations auspex does make
  are all genuinely per-request client data; fixed-vocabulary fields (method,
  scheme, …) are interned to zero.
- **Zero added latency and zero allocations when disabled** — the middleware
  takes a no-span fast path.
- No `opentelemetry`, `opentelemetry_sdk`, or `tonic` in your dependency tree.
  The OTLP protobuf types are vendored; the runtime deps are small.

(Numbers are from one machine; re-run with `cargo bench --bench hotpath` and
`just compare-otel`.)

## Install

```sh
cargo add auspex
# enable axum route-name extraction (recommended for axum apps):
cargo add auspex --features axum
```

The `axum` feature lets auspex read axum's `MatchedPath`, so HTTP span names are
the low-cardinality route (`GET /users/{id}`) rather than just the method.

## Quick start

**Ergonomic path** — auspex installs a global subscriber containing just its
layer. This gives you both HTTP root spans _and_ child spans (`#[instrument]`,
etc.):

```rust
#[tokio::main]
async fn main() {
    let tracer = auspex::init().expect("auspex init");

    let app = axum::Router::new()
        .route("/", axum::routing::get(handler))
        .layer(tracer);

    // serve `app` ...
}
```

**Explicit path** — if your app already owns its `tracing_subscriber` setup,
compose auspex's layer yourself:

```rust
let tracer = auspex::Tracer::new();

tracing_subscriber::registry()
    .with(tracer.subscriber_layer())   // auspex exports spans
    .with(tracing_subscriber::fmt::layer())  // your console logging
    .init();

let app = axum::Router::new()
    .route("/", axum::routing::get(handler))
    .layer(tracer);
```

> **Runtime note:** construct the tracer inside a Tokio runtime (the normal
> `#[tokio::main]` case). The background export worker is spawned at
> construction; with no runtime present, auspex still runs but export is
> disabled.

`Tracer` is a Tower `Layer`. Layer it over the routes you want traced and keep
untraced endpoints (health checks) outside it, per normal Tower ordering:

```rust
let app = axum::Router::new()
    .nest("/api", traced_routes)
    .layer(tracer)
    .route("/healthz", axum::routing::get(health));  // not traced
```

For builder-based setup (overrides win over env vars), use
`auspex::Tracer::builder()`. Resource attributes can be set in code with
`.with_resource_attributes([...])`, which overrides matching
`OTEL_RESOURCE_ATTRIBUTES` keys while leaving the rest in place — handy for
values the app knows at startup, e.g. `("service.version", env!("CARGO_PKG_VERSION"))`.

## Configuration

auspex reads standard OTEL environment variables (plus a couple of `AUSPEX_`
extras). It is intentionally narrow:

| Variable                             | Purpose                                                                                                           |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------------------- |
| `OTEL_SERVICE_NAME`                  | `service.name` on exported spans.                                                                                 |
| `OTEL_RESOURCE_ATTRIBUTES`           | Comma-separated `k=v` Resource attributes (`service.version`, `deployment.environment`, …) added to every span.   |
| `OTEL_SINK_URI`                      | auspex shorthand for the export endpoint (OTLP or Zipkin — see below).                                            |
| `OTEL_EXPORTER_OTLP_ENDPOINT`        | Standard OTLP/HTTP base (auspex appends `/v1/traces`).                                                            |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Standard per-signal OTLP endpoint (used as-is).                                                                   |
| `OTEL_EXPORTER_OTLP_HEADERS`         | Comma-separated `k=v` headers sent on every export request.                                                       |
| `OTEL_TRACES_EXPORTER=none`          | Force disabled mode.                                                                                              |
| `OTEL_BSP_MAX_EXPORT_BATCH_SIZE`     | Max spans per export batch (default 512).                                                                         |
| `OTEL_BSP_SCHEDULE_DELAY`            | Batch flush interval, in **milliseconds** (default 5000).                                                         |
| `AUSPEX_CAPTURE_HEADERS_PREFIX`      | Comma-separated response-header name prefixes to capture as attributes (a sensitive-header denylist always wins). |

If exporting is disabled, misconfigured, or set to `none`, the middleware passes
requests through without creating spans (the zero-cost path).

### Exporters and endpoint schemes

The endpoint scheme selects the exporter and the default path:

| Sink URI                                                         | Exporter             | Default path    |
| ---------------------------------------------------------------- | -------------------- | --------------- |
| `http://host:4318` / `https://…`                                 | OTLP/HTTP (protobuf) | `/v1/traces`    |
| `zipkin+http://host:9411` / `zipkin+https://…` / `zipkin://host` | Zipkin v2 (JSON)     | `/api/v2/spans` |

Both exporters POST in the background with jittered retry on transient failures
(5xx / 429 / transport errors) and fail fast on 4xx; the request path never
blocks on export.

## What gets captured

Each HTTP request becomes a root span with OTEL-semantic attributes:

- `http.request.method`, `url.path`, `url.scheme`, `url.query` (when present),
  `network.protocol.name`
- `http.route` — only with the `axum` feature and a matched route (raw paths are
  never used as span names)
- `http.response.status_code`
- `user_agent.original` and `server.address` — when those headers are present
- `error.type` + OTEL `Error` status — on 5xx responses and service errors
- opt-in response headers via `AUSPEX_CAPTURE_HEADERS_PREFIX`

The exported span name is `{method} {route}` when a low-cardinality route is
available, otherwise `{method}`.

W3C `traceparent` / `tracestate` are extracted from inbound requests so traces
join across services, and the sampled flag is preserved.

Add your own context with the normal `tracing` facade — `#[instrument]`,
`info_span!`, `Span::record`, events, and `follows_from` (exported as OTEL span
links).

## Try it locally

A runnable axum example and a one-command local Jaeger sink are included:

```sh
just jaeger-up      # start Jaeger all-in-one (OTLP :4318, Zipkin :9411, UI :16686)
just example        # run examples/basic.rs, exporting via OTLP/HTTP
# or: just example-zipkin   # same app, Zipkin exporter
# generate traffic, then open the UI:
curl localhost:3000/ ; curl localhost:3000/hello/world ; curl localhost:3000/boom
open http://localhost:16686
just jaeger-down
```

See [`examples/README.md`](examples/README.md) for the full walkthrough.

## Status & compatibility

- MSRV **1.90** (Rust edition 2024).
- v0.1: OTLP/HTTP and Zipkin v2 exporters, W3C context propagation, events,
  links, and HTTP error status. OTLP/gRPC, metrics, and logs are out of scope.

## License

Dual-licensed under MIT and Apache-2.0.
