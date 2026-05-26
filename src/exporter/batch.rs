//! The background batch worker (Task 6.1).
//!
//! Pulls `FinishedSpan`s from the channel, accumulates them, and hands
//! non-empty batches to an `Exporter` on size or schedule-delay boundaries.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, Instant};

use super::Exporter;
use crate::span::FinishedSpan;

pub struct BatchWorker {
    rx: mpsc::Receiver<FinishedSpan>,
    exporter: std::sync::Arc<dyn Exporter>,
    max_batch: usize,
    schedule_delay: Duration,
    last_error_log: Option<Instant>,
    /// Resolves when an explicit shutdown is requested (via the signal) or the
    /// signalling half is dropped (full pipeline drop). Either way, drain.
    shutdown_rx: oneshot::Receiver<()>,
}

impl BatchWorker {
    pub fn new(
        rx: mpsc::Receiver<FinishedSpan>,
        exporter: std::sync::Arc<dyn Exporter>,
        max_batch: usize,
        schedule_delay: Duration,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> Self {
        Self {
            rx,
            exporter,
            max_batch,
            schedule_delay,
            last_error_log: None,
            shutdown_rx,
        }
    }

    /// Main loop. Runs until the sender side is dropped.
    ///
    /// Uses an interval that is reset on activity. This plays nicely with
    /// `tokio::time::pause()` + `advance()` for deterministic schedule-delay
    /// tests.
    pub async fn run(mut self) {
        let mut batch: Vec<FinishedSpan> = Vec::with_capacity(self.max_batch);

        // Start with a dummy interval; we'll reset it when we have data.
        let mut ticker = time::interval(self.schedule_delay);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        let mut has_pending_batch = false;

        loop {
            let tick = if has_pending_batch { Some(ticker.tick()) } else { None };

            tokio::select! {
                biased;

                // Explicit shutdown: drain everything buffered, then exit.
                // Checked first so a shutdown request is honoured promptly.
                _ = &mut self.shutdown_rx => {
                    self.drain_and_flush(std::mem::take(&mut batch)).await;
                    return;
                }

                maybe_span = self.rx.recv() => {
                    if let Some(span) = maybe_span {
                        let was_empty = batch.is_empty();
                        batch.push(span);

                        if was_empty {
                            has_pending_batch = true;
                            ticker.reset(); // next tick in exactly schedule_delay from now
                        }

                        if batch.len() >= self.max_batch {
                            self.flush(std::mem::take(&mut batch)).await;
                            has_pending_batch = false;
                        }
                    } else {
                        // All senders dropped → drain (close is then a no-op).
                        self.drain_and_flush(std::mem::take(&mut batch)).await;
                        return;
                    }
                }

                () = async {
                    match tick {
                        Some(t) => { t.await; }
                        None => { std::future::pending::<()>().await; }
                    }
                } , if tick.is_some() => {
                    if !batch.is_empty() {
                        self.flush(std::mem::take(&mut batch)).await;
                    }
                    has_pending_batch = false;
                }
            }
        }
    }

    /// Final drain on shutdown: stop accepting new spans, then export every
    /// span already buffered in the channel.
    ///
    /// `Receiver::close()` rejects further sends (late spans are dropped, which
    /// is honest under shutdown) while leaving buffered spans receivable to
    /// completion — so this is race-free without spinning on `try_recv`.
    async fn drain_and_flush(&mut self, mut batch: Vec<FinishedSpan>) {
        self.rx.close();
        while let Some(span) = self.rx.recv().await {
            batch.push(span);
            if batch.len() >= self.max_batch {
                self.flush(std::mem::take(&mut batch)).await;
            }
        }
        self.flush(batch).await; // no-op if empty
    }

    async fn flush(&mut self, batch: Vec<FinishedSpan>) {
        if batch.is_empty() {
            return;
        }

        if let Err(err) = self.exporter.export(batch).await {
            let now = Instant::now();
            let should_log = self
                .last_error_log
                .is_none_or(|last| now.duration_since(last) > Duration::from_secs(30));

            if should_log {
                tracing::warn!(
                    target: "auspex::exporter",
                    error = %err,
                    "exporter failed to export batch (will continue)"
                );
                self.last_error_log = Some(now);
            }
        }
    }
}
