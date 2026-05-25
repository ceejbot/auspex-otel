//! Dev-only generator for auspex's vendored OTLP protobuf Rust modules.
//!
//! Run via `just gen-proto`. Compiles the narrow set of vendored `.proto`
//! files (pure-Rust, via `protox` — no system `protoc`) and emits one
//! `prost`-generated `.rs` per proto package into `src/exporter/proto/`.
//!
//! The committed output is wired together by the hand-written
//! `src/exporter/proto/mod.rs` (whose nested module tree mirrors the proto
//! package hierarchy so prost's cross-package `super::` references resolve).

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Resolve paths from this crate's manifest dir (tools/proto-gen) so the
    // generator works regardless of the caller's CWD.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest
        .parent()
        .and_then(|p| p.parent())
        .ok_or("could not locate repo root")?
        .to_path_buf();

    let proto_root = repo.join("proto");
    let out_dir = repo.join("src/exporter/proto");

    let protos = [
        "opentelemetry/proto/common/v1/common.proto",
        "opentelemetry/proto/resource/v1/resource.proto",
        "opentelemetry/proto/trace/v1/trace.proto",
        "opentelemetry/proto/collector/trace/v1/trace_service.proto",
    ]
    .map(|p| proto_root.join(p));

    // Pure-Rust .proto -> FileDescriptorSet (no protoc needed).
    let fds = protox::compile(protos, [&proto_root])?;

    std::fs::create_dir_all(&out_dir)?;
    let mut cfg = prost_build::Config::new();
    cfg.out_dir(&out_dir);
    // Strip the proto doc-comments: they contain space-indented examples that
    // rustdoc would otherwise try to compile as doctests (and fail on). The
    // generated module is internal, so the lost comments cost us nothing.
    // "." matches every proto path.
    cfg.disable_comments(["."]);
    cfg.compile_fds(fds)?;

    println!("proto-gen: wrote generated modules to {}", out_dir.display());
    Ok(())
}
