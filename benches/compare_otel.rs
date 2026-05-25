//! Allocation comparison: auspex vs the full OpenTelemetry SDK stack.
//!
//! Answers "is 3 allocations/span good?" by measuring the *identical* span
//! workload — an HTTP-style span with the standard attributes, created and
//! closed — under two `tracing` subscriber layers:
//!
//! 1. **auspex** (`Tracer::subscriber_layer`), and
//! 2. **`tracing-opentelemetry` + `opentelemetry_sdk`** (the mainstream stack).
//!
//! Both are measured with the *same* counting allocator and the *same*
//! workload. Neither pays for serialization/transport: auspex `try_send`s to
//! its channel (no worker spawned here) and the OTEL stack uses a no-op
//! `SpanProcessor` that drops the `SpanData` on end — so this isolates the
//! per-span data-model cost.
//!
//! Run: `just compare-otel` (i.e. `cargo bench --bench compare_otel --features
//! compare-otel`). Feature-gated so the OTEL dependency tree is only compiled
//! when explicitly requested — it never enters the shipped crate or normal CI.
#![allow(unsafe_code, clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use opentelemetry::Context;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SdkTracerProvider, Span as SdkSpan, SpanData, SpanProcessor};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt as _;

// --- Counting global allocator ---

struct Counting;
static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static COUNTING_ON: AtomicBool = AtomicBool::new(false);

// SAFETY: delegates to the system allocator unchanged; only adds an atomic
// increment on allocation when counting is enabled.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING_ON.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING_ON.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// --- A no-op SpanProcessor: drops SpanData on end (no export, no runtime) ---

#[derive(Debug)]
struct NoopProcessor;

impl SpanProcessor for NoopProcessor {
    fn on_start(&self, _span: &mut SdkSpan, _cx: &Context) {}
    fn on_end(&self, _span: SpanData) {}
    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }
    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

/// The shared workload: one HTTP-style span with the standard attributes,
/// created and closed. Identical bytes for both subscribers.
#[inline(never)]
fn workload() {
    let span = tracing::info_span!(
        "HTTP request",
        "http.request.method" = "GET",
        "url.path" = "/api/users/42",
        "url.scheme" = "http",
        "network.protocol.name" = "http",
        "user_agent.original" = "auspex-bench/1.0",
        "server.address" = "localhost:3000",
    );
    let _enter = span.enter();
    // span closes on drop -> subscriber finalizes/exports it.
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    #[allow(clippy::cast_sign_loss)]
    sorted[((sorted.len() - 1) as f64 * p).round().abs() as usize]
}

/// Run `workload` under the already-installed (thread-local default)
/// subscriber, returning (min allocations/span, sorted latency samples in ns).
fn measure(iters: usize, warmup: usize) -> (usize, Vec<u64>) {
    for _ in 0..warmup {
        workload();
    }
    let mut min_allocs = usize::MAX;
    let mut lat = Vec::with_capacity(iters);
    for _ in 0..iters {
        let before = ALLOC_COUNT.load(Ordering::Relaxed);
        COUNTING_ON.store(true, Ordering::Relaxed);
        let start = Instant::now();
        workload();
        let ns = start.elapsed().as_nanos() as u64;
        COUNTING_ON.store(false, Ordering::Relaxed);
        min_allocs = min_allocs.min(ALLOC_COUNT.load(Ordering::Relaxed) - before);
        lat.push(ns);
    }
    lat.sort_unstable();
    (min_allocs, lat)
}

const ITERS: usize = 100_000;
const WARMUP: usize = 10_000;

fn main() {
    println!("allocation comparison: auspex vs opentelemetry_sdk + tracing-opentelemetry");
    println!("identical workload: one HTTP span (6 attributes), create + close\n");

    // auspex.
    let auspex_tracer = auspex::Tracer::builder()
        .with_service_name("auspex-compare")
        .with_sink_uri("http://localhost:4318")
        .build();
    let (auspex_allocs, auspex_lat) =
        tracing::subscriber::with_default(Registry::default().with(auspex_tracer.subscriber_layer()), || {
            measure(ITERS, WARMUP)
        });

    // opentelemetry_sdk + tracing-opentelemetry (no-op processor).
    let provider = SdkTracerProvider::builder().with_span_processor(NoopProcessor).build();
    let otel_tracer = provider.tracer("auspex-compare");
    let (otel_allocs, otel_lat) = tracing::subscriber::with_default(
        Registry::default().with(tracing_opentelemetry::layer().with_tracer(otel_tracer)),
        || measure(ITERS, WARMUP),
    );

    let row = |name: &str, allocs: usize, lat: &[u64]| {
        println!(
            "  {name:<26} allocs/span = {allocs:<3}  latency p50={}ns p99={}ns",
            percentile(lat, 0.50),
            percentile(lat, 0.99),
        );
    };
    row("auspex", auspex_allocs, &auspex_lat);
    row("opentelemetry stack", otel_allocs, &otel_lat);

    if auspex_allocs > 0 {
        println!(
            "\n  → the OpenTelemetry stack does {:.1}x the allocations of auspex per span",
            otel_allocs as f64 / auspex_allocs as f64,
        );
    }
}
