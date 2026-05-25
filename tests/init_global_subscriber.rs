//! `init()` installs a process-global `tracing` subscriber, which is set-once
//! for the life of the process. This test lives in its own integration-test
//! binary so it runs in a dedicated process, isolated from the `OTEL_*`-env-
//! mutating unit tests whose process-global state would otherwise race `init()`
//! under a shared-process runner like `cargo test`. (`cargo nextest`, used by
//! `just ci`, isolates every test into its own process and is immune anyway.)

use auspex::{InitError, init};

#[test]
fn init_installs_once_then_reports_already_set() {
    // Pin a clean, valid environment so `Tracer::try_new()` inside `init()`
    // always succeeds (as a disabled tracer), regardless of the caller's shell.
    // This isolates the behavior under test — global-subscriber detection —
    // from ambient OTEL_* configuration.
    temp_env::with_vars(
        [
            ("OTEL_SERVICE_NAME", Some("auspex-init-test")),
            ("OTEL_SINK_URI", None),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", None),
            ("OTEL_TRACES_EXPORTER", None),
        ],
        || {
            // The first call installs the global subscriber and hands back the tracer.
            let first = init();
            assert!(
                first.is_ok(),
                "first init() should install the global subscriber, got {first:?}"
            );

            // The second call must detect the installed subscriber and say so precisely.
            let second = init();
            assert!(
                matches!(second, Err(InitError::GlobalSubscriberAlreadySet)),
                "second init() should return GlobalSubscriberAlreadySet, got {second:?}"
            );
        },
    );
}
