_help:
    just -l

# Run all tests using nextest (the way CI does it).
# Note: `--all-targets` minus `--benches`. The `hotpath` bench is `harness =
# false` (a plain `fn main()`), so nextest can't enumerate it as a libtest
# binary. It is still compile-checked by `cargo clippy --all-targets` in `ci`.
# We run twice: once with default features, once with `axum` enabled, because
# the HTTP middleware's route/response/header tests are gated behind `axum`
# and would otherwise only be compile-checked, never executed.
test:
    cargo nextest run --workspace --lib --bins --tests --examples --future-incompat-report
    cargo nextest run --workspace --lib --tests --features axum --future-incompat-report

# cargo-deny (install via `cargo install cargo-deny`)
deny:
    cargo deny check

# Regenerate the vendored OTLP protobuf Rust modules from proto/*.proto.
# Runs the isolated, dev-only generator (pure-Rust protox + prost-build, so no
# system protoc is required). Only needed when bumping the vendored proto version
# or the prost major version. See proto/README.md.
gen-proto:
    cargo run --quiet --manifest-path tools/proto-gen/Cargo.toml
    cargo +nightly fmt

coverage:
    cargo llvm-cov --all-targets --summary-only

# Run the nightly formatter
fmt:
    cargo +nightly fmt

# Run the full local CI suite (matches GitHub Actions + extra strictness).
# Run this before any significant commit.
# Includes --features axum clippy so feature-gated code (e.g. MatchedPath paths)
# is also checked under the same -D warnings policy.
@ci: test
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy --workspace --all-targets --features axum -- -D warnings
    cargo test --doc
    cargo +nightly fmt --check --all
    cargo deny check 2>/dev/null || echo "(cargo-deny not installed or no deny.toml yet — skipping)"

# Start a local Jaeger all-in-one (OTLP/HTTP :4318, UI http://localhost:16686).
jaeger-up:
    docker compose up -d
    @echo "Jaeger UI: http://localhost:16686  | OTLP/HTTP: http://localhost:4318"

# Stop and remove the local Jaeger container.
jaeger-down:
    docker compose down

# Run the example axum app wired to local Jaeger via OTLP/HTTP (see examples/README.md).
example:
    OTEL_SERVICE_NAME=auspex-example \
    OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
        cargo run --example basic --features axum

# Run the example wired to local Jaeger via the Zipkin v2 JSON exporter.
example-zipkin:
    OTEL_SERVICE_NAME=auspex-example-zipkin \
    OTEL_SINK_URI=zipkin+http://localhost:9411 \
        cargo run --example basic --features axum

# Allocation comparison vs the full OpenTelemetry SDK (pulls the OTEL tree;
# gated behind the compare-otel feature so normal builds stay lean).
compare-otel:
    cargo bench --bench compare_otel --features compare-otel

# Quick development check (no tests)
check:
    cargo check --workspace --all-targets

# Run clippy with the same strictness as CI (both default and axum feature)
clippy:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy --workspace --all-targets --features axum -- -D warnings

# Install required tools
setup:
    brew tap ceejbot/tap
    brew install cargo-nextest tomato semver-bump
    rustup install nightly

# Tag a new version for release.
version BUMP:
    #!/usr/bin/env bash
    set -e
    current=$(tomato get package.version Cargo.toml)
    version=$(semver-bump {{ BUMP }} "$current")
    tomato set package.version "$version" Cargo.toml &> /dev/null
    cargo generate-lockfile
    git commit Cargo.toml -m "v${version}"
    git tag "v${version}"
    echo "Release tagged for version v${version}"

# publish to crates.io
release:
    cargo publish --dry-run
