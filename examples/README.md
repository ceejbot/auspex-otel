# auspex examples

## `basic.rs` — axum app exporting to Jaeger

A minimal axum service wired with auspex, exporting traces over OTLP/HTTP.
Doubles as the manual smoke test for the OTLP/HTTP exporter (Task 6.2).

### Quick start

```sh
# 1. Start a local Jaeger all-in-one (OTLP/HTTP :4318, UI :16686).
just jaeger-up

# 2. Run the example wired to Jaeger.
just example
#   ...which is just:
#   OTEL_SERVICE_NAME=auspex-example \
#   OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
#       cargo run --example basic --features axum

# 3. Generate some traffic.
curl localhost:3000/
curl localhost:3000/hello/world
curl localhost:3000/boom        # 500 -> span recorded with Error status

# 4. View traces in the Jaeger UI:
open http://localhost:16686     # pick service "auspex-example", Find Traces

# 5. Tear down Jaeger when done.
just jaeger-down
```

### What it demonstrates

- **HTTP root spans** — one span per request, created by the `Tracer` Tower
  layer, with W3C trace-context propagation.
- **Child spans** — `do_work()` is `#[tracing::instrument]`-annotated; it
  appears as a child of the request's root span (via the subscriber layer).
- **Low-cardinality route names** — `/hello/world` is reported as the span name
  `GET /hello/{name}` (requires the `axum` feature for `MatchedPath`).
- **Error status** — `/boom` returns 500, so its span carries OTEL `Error`
  status.

### Configuration

auspex reads standard OTEL environment variables. The most relevant:

| Variable | Example | Meaning |
|---|---|---|
| `OTEL_SERVICE_NAME` | `auspex-example` | `service.name` resource attribute |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `http://localhost:4318` | OTLP/HTTP base (auspex appends `/v1/traces`) |
| `OTEL_SINK_URI` | `http://localhost:4318` | auspex shorthand for the endpoint |
| `OTEL_EXPORTER_OTLP_HEADERS` | `x-api-key=secret` | headers sent on every export |

If no endpoint is configured the app still runs, but export is disabled and it
prints a notice.

> **Note:** the exporter worker is spawned only inside a Tokio runtime (the
> normal `#[tokio::main]` case). That is satisfied here.
