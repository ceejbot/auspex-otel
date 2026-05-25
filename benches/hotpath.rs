//! Request-path benchmark for auspex (Task 7.2).
//!
//! Measures the latency and heap allocations auspex adds to a request, against
//! the v0.1 targets: **< 50 µs p99 added latency** and **< 2
//! request-path allocations** for the enabled, root-span-only scenario.
//!
//! ## Method
//!
//! The request path is runtime-independent: `TracerService::call` builds the
//! root span and the `ResponseFuture` finalizes it and `try_send`s to the
//! export channel — none of which needs a Tokio runtime (only the background
//! export worker does, and that is deliberately *off* the request path). So we
//! drive the full request synchronously with a no-op waker, which isolates the
//! request-path cost from the worker/export thread entirely.
//!
//! Latency is reported as **added** latency: `auspex_path − bare_baseline` at
//! each percentile. Baseline subtraction cancels the per-iteration timer
//! overhead, which matters at this sub-microsecond scale. Allocations are
//! counted with a process-global counting allocator, snapshotted around a
//! single warmed call (excluding request construction, which is identical for
//! both).
//!
//! Run with: `cargo bench --bench hotpath`. This is a plain binary
//! (`harness = false`); CI only checks it compiles (`cargo bench --no-run`).
//!
//! This file is dev/measurement tooling, not shipped code — relax the crate
//! lints. A counting `GlobalAlloc` inherently needs `unsafe`.
#![allow(
    unsafe_code,
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use tower::Service;
use tower::layer::Layer;

// --- Counting global allocator ---

struct Counting;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static COUNTING_ON: AtomicBool = AtomicBool::new(false);

// SAFETY: delegates every operation to the system allocator unchanged; we only
// add an atomic increment on the allocating paths when counting is enabled.
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
        // A realloc is a heap operation on the request path; count it too.
        if COUNTING_ON.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// --- A trivial inner "handler" service ---

#[derive(Clone)]
struct Handler {
    /// Number of `#[instrument]`-style child spans to create per request.
    children: usize,
}

impl Service<http::Request<()>> for Handler {
    type Response = http::Response<()>;
    type Error = Infallible;
    type Future = HandlerFut;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: http::Request<()>) -> Self::Future {
        HandlerFut {
            children: self.children,
        }
    }
}

/// Creates the child spans during `poll`, so they nest under the root span that
/// `ResponseFuture` enters around this future.
struct HandlerFut {
    children: usize,
}

impl Future for HandlerFut {
    type Output = Result<http::Response<()>, Infallible>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        for i in 0..self.children {
            let span = tracing::info_span!("child_work", i = i);
            let _enter = span.enter();
            // Dropping `span` closes it -> on_close -> finalize + try_send.
        }
        Poll::Ready(Ok(http::Response::new(())))
    }
}

fn make_request() -> http::Request<()> {
    http::Request::builder()
        .method("GET")
        .uri("/api/users/42")
        .header("user-agent", "auspex-bench/1.0")
        .header("host", "localhost:3000")
        .body(())
        .expect("request builds")
}

/// Drive one request to completion synchronously (inner future is `Ready`).
fn drive<S>(svc: &mut S, req: http::Request<()>)
where
    S: Service<http::Request<()>, Response = http::Response<()>>,
{
    let fut = svc.call(req);
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if fut.as_mut().poll(&mut cx).is_ready() {
            break;
        }
        std::hint::spin_loop();
    }
}

// --- Measurement ---

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

/// Time `iters` request drives (ns each), returning a sorted sample vector.
fn measure_latency<S, F>(iters: usize, warmup: usize, mut make_svc: F) -> Vec<u64>
where
    S: Service<http::Request<()>, Response = http::Response<()>>,
    F: FnMut() -> S,
{
    let mut svc = make_svc();
    for _ in 0..warmup {
        drive(&mut svc, make_request());
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let req = make_request();
        let start = Instant::now();
        drive(&mut svc, req);
        samples.push(start.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    samples
}

/// Minimum allocations observed for a single warmed request drive (the
/// steady-state floor, excluding request construction).
fn measure_min_allocs<S, F>(samples: usize, warmup: usize, mut make_svc: F) -> usize
where
    S: Service<http::Request<()>, Response = http::Response<()>>,
    F: FnMut() -> S,
{
    let mut svc = make_svc();
    for _ in 0..warmup {
        drive(&mut svc, make_request());
    }
    let mut min = usize::MAX;
    for _ in 0..samples {
        let req = make_request(); // built outside the counted window
        let before = ALLOC_COUNT.load(Ordering::Relaxed);
        COUNTING_ON.store(true, Ordering::Relaxed);
        drive(&mut svc, req);
        COUNTING_ON.store(false, Ordering::Relaxed);
        let delta = ALLOC_COUNT.load(Ordering::Relaxed) - before;
        min = min.min(delta);
    }
    min
}

fn enabled_tracer() -> auspex::Tracer {
    // Any http/zipkin sink enables the pipeline; no worker is spawned here
    // (no runtime), so nothing leaves the request path.
    auspex::Tracer::builder()
        .with_service_name("auspex-bench")
        .with_sink_uri("http://localhost:4318")
        .build()
}

fn disabled_tracer() -> auspex::Tracer {
    auspex::Tracer::builder().with_service_name("auspex-bench").build()
}

const LAT_ITERS: usize = 200_000;
const LAT_WARMUP: usize = 20_000;
const ALLOC_SAMPLES: usize = 5_000;
const ALLOC_WARMUP: usize = 5_000;

fn main() {
    println!("auspex request-path benchmark");
    println!("targets: p99 added latency < 50us, request-path allocations < 2 (root-span-only)\n");

    // Baseline: bare handler, no tracing layer, no subscriber.
    let baseline = measure_latency(LAT_ITERS, LAT_WARMUP, || Handler { children: 0 });
    let baseline_allocs = measure_min_allocs(ALLOC_SAMPLES, ALLOC_WARMUP, || Handler { children: 0 });

    // The three auspex scenarios, each under its own (thread-local) subscriber.
    run_scenario("disabled middleware", &baseline, baseline_allocs, &disabled_tracer(), 0);
    run_scenario(
        "enabled, root-span-only",
        &baseline,
        baseline_allocs,
        &enabled_tracer(),
        0,
    );
    run_scenario(
        "enabled, root + 3 child spans",
        &baseline,
        baseline_allocs,
        &enabled_tracer(),
        3,
    );

    println!(
        "\nbaseline (bare service): p50={}ns p95={}ns p99={}ns, allocs/req={}",
        percentile(&baseline, 0.50),
        percentile(&baseline, 0.95),
        percentile(&baseline, 0.99),
        baseline_allocs,
    );
}

fn run_scenario(name: &str, baseline: &[u64], baseline_allocs: usize, tracer: &auspex::Tracer, children: usize) {
    let subscriber = {
        use tracing_subscriber::layer::SubscriberExt as _;
        tracing_subscriber::registry().with(tracer.subscriber_layer())
    };

    let (lat, allocs) = tracing::subscriber::with_default(subscriber, || {
        let lat = measure_latency(LAT_ITERS, LAT_WARMUP, || tracer.clone().layer(Handler { children }));
        let allocs = measure_min_allocs(ALLOC_SAMPLES, ALLOC_WARMUP, || {
            tracer.clone().layer(Handler { children })
        });
        (lat, allocs)
    });

    let added = |p: f64| percentile(&lat, p).saturating_sub(percentile(baseline, p));
    let us = |ns: u64| ns as f64 / 1000.0;

    println!("── {name} ──");
    println!(
        "  latency:  p50={}ns  p95={}ns  p99={}ns",
        percentile(&lat, 0.50),
        percentile(&lat, 0.95),
        percentile(&lat, 0.99),
    );
    println!(
        "  ADDED:    p50={:.2}us  p95={:.2}us  p99={:.2}us   {}",
        us(added(0.50)),
        us(added(0.95)),
        us(added(0.99)),
        if us(added(0.99)) < 50.0 {
            "✓ < 50us"
        } else {
            "✗ OVER 50us"
        },
    );
    let added_allocs = allocs.saturating_sub(baseline_allocs);
    println!(
        "  allocs/req: {allocs} (added {added_allocs})   {}\n",
        if added_allocs < 2 { "✓ < 2" } else { "✗ >= 2" },
    );
}
