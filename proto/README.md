# Vendored OTLP protobuf definitions

These are a narrow subset of the [OpenTelemetry protocol][otlp] `.proto`
definitions — only the messages auspex needs to build an
`ExportTraceServiceRequest` for the OTLP/HTTP trace exporter.

## Provenance

- **Source:** <https://github.com/open-telemetry/opentelemetry-proto>
- **Version:** OTLP proto **v1.10.0** (2026-03-09)
- **Obtained via:** the `opentelemetry-proto` 0.32.0 crate's vendored copy.

Only these files are vendored (with their original Apache-2.0 license headers):

```
opentelemetry/proto/common/v1/common.proto
opentelemetry/proto/resource/v1/resource.proto
opentelemetry/proto/trace/v1/trace.proto
opentelemetry/proto/collector/trace/v1/trace_service.proto
```

## Why vendored (not the `opentelemetry-proto` crate)

`opentelemetry-proto` has **non-optional** dependencies on `opentelemetry` and
`opentelemetry_sdk` — there is no feature combination that excludes them. Pulling
it in would put the full OTEL SDK in the dependency tree, which directly violates
auspex's core constraint ("none of the full OpenTelemetry SDK horror"). The
serialization path is identical either way (both are `prost`-generated), so there
is no runtime cost to vendoring — only a small, self-controlled maintenance cost.

## Regenerating the Rust

The generated Rust lives at `src/exporter/proto/generated.rs` and is committed.
Regenerate it with:

```
just gen-proto
```

This runs the isolated generator in `tools/proto-gen/` (pure-Rust `protox` +
`prost-build`, so no system `protoc` is required). Regeneration is only needed
when bumping the vendored proto version or the `prost` major version.

[otlp]: https://github.com/open-telemetry/opentelemetry-proto
