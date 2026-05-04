use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::backend::shard::{BackendOutcome, BackendShard, LifecycleEvent};
use crate::backend::state::BackendState;
use crate::config::Config;
use crate::dispatch::DispatchEvent;
use crate::metrics::Metrics;
use crate::types::BackendId;

use super::connect::{backoff_for_attempt, connect_once};

/// Per-backend lifecycle task. Single-writer to the shard's circuit, scoring,
/// state, and slot. Owns the receiver end of the shard's lifecycle channel.
pub struct BackendLifecycle {
    shard: Arc<BackendShard>,
    rx: mpsc::Receiver<LifecycleEvent>,
    dispatch_tx: mpsc::Sender<DispatchEvent>,
    metrics: Arc<Metrics>,
    connect_timeout: Duration,
}

impl BackendLifecycle {
    pub fn new(
        shard: Arc<BackendShard>,
        rx: mpsc::Receiver<LifecycleEvent>,
        dispatch_tx: mpsc::Sender<DispatchEvent>,
        metrics: Arc<Metrics>,
        config: &Config,
    ) -> Self {
        Self {
            shard,
            rx,
            dispatch_tx,
            metrics,
            connect_timeout: config.connect_timeout,
        }
    }

    /// Run the lifecycle event loop. Performs an initial connect, then
    /// services lifecycle events forever (until the channel closes).
    pub async fn run(mut self) {
        // Initial connect on startup. The reconnect_attempt counter starts
        // at 0 so this attempt is immediate (no backoff).
        self.attempt_connect_with_backoff().await;

        while let Some(event) = self.rx.recv().await {
            match event {
                LifecycleEvent::ForwardDone(outcome) => {
                    self.apply_outcome(outcome).await;
                }
                LifecycleEvent::CooldownExpired => {
                    // Recovery probe: the cooldown has elapsed and we should
                    // attempt a connect. Treat this connect as the "half-open
                    // probe" — failure bumps the cooldown index.
                    self.attempt_connect_with_backoff().await;
                }
                LifecycleEvent::Shutdown => break,
            }
        }

        tracing::debug!(backend = %self.shard.id, "lifecycle task exiting");
    }

    async fn apply_outcome(&self, outcome: BackendOutcome) {
        // Drop any held slot first — a forward task only sends ForwardDone
        // after the WS has been consumed/cleaned up, so the slot is None
        // already, but be defensive.
        self.shard.drop_slot();
        self.shard.set_state(BackendState::Disconnected);
        self.metrics
            .set_backend_state(&self.shard.addr_str, BackendState::Disconnected.name());

        match outcome {
            BackendOutcome::Success { ttfc_ms } => {
                self.shard.record_ttfc_ms(ttfc_ms);
                self.shard.record_result(true);
                self.shard.record_circuit_success();
                // Healthy completion → reconnect immediately.
                self.attempt_connect_with_backoff().await;
            }
            BackendOutcome::Failure => {
                self.shard.record_result(false);
                let (just_opened, cooldown) = self.shard.record_circuit_failure();
                if just_opened {
                    tracing::info!(
                        backend = %self.shard.id,
                        cooldown = ?cooldown,
                        "circuit opened; scheduling recovery timer"
                    );
                    self.spawn_cooldown_timer(cooldown);
                } else {
                    self.attempt_connect_with_backoff().await;
                }
            }
            BackendOutcome::Busy => {
                // No scoring change, no circuit hit. Just reconnect.
                self.attempt_connect_with_backoff().await;
            }
            BackendOutcome::ClientGoneOrCancel => {
                // No scoring change.
                self.attempt_connect_with_backoff().await;
            }
        }
    }

    /// Attempt one connect with the current backoff. Awaits inline; the
    /// per-backend lifecycle task is the only thing this blocks.
    async fn attempt_connect_with_backoff(&self) {
        // If the circuit is open and cooldown hasn't elapsed, don't connect —
        // wait for the CooldownExpired event.
        let circuit_open = matches!(
            self.shard.circuit_state(),
            crate::backend::circuit::CircuitState::Open
        );
        if circuit_open && !self.shard.cooldown_elapsed() {
            tracing::debug!(backend = %self.shard.id, "circuit open; deferring reconnect");
            return;
        }

        let attempt = self.shard.reconnect_attempt.load(Ordering::Relaxed);
        let backoff = backoff_for_attempt(attempt);

        self.shard.set_state(BackendState::Connecting);
        self.metrics
            .set_backend_state(&self.shard.addr_str, BackendState::Connecting.name());

        if !backoff.is_zero() {
            tokio::time::sleep(backoff).await;
        }

        let was_probe = self.shard.cooldown_elapsed();

        match connect_once(self.shard.addr, self.connect_timeout).await {
            Ok(ws) => {
                self.shard.install_warm_ws(ws);
                self.metrics
                    .set_backend_state(&self.shard.addr_str, BackendState::Ready.name());
                tracing::debug!(backend = %self.shard.id, "connection ready (warm slot)");
                let _ = self
                    .dispatch_tx
                    .send(DispatchEvent::BackendReady(self.shard.id))
                    .await;
            }
            Err(e) => {
                self.shard.set_state(BackendState::Disconnected);
                self.metrics
                    .set_backend_state(&self.shard.addr_str, BackendState::Disconnected.name());

                if was_probe {
                    let cooldown = self.shard.record_open_failure();
                    tracing::info!(
                        backend = %self.shard.id,
                        cooldown = ?cooldown,
                        error = %e,
                        "circuit probe failed; rescheduling recovery timer"
                    );
                    self.spawn_cooldown_timer(cooldown);
                } else {
                    self.shard.reconnect_attempt.fetch_add(1, Ordering::Relaxed);
                    let new_attempt = self.shard.reconnect_attempt.load(Ordering::Relaxed);
                    tracing::warn!(
                        backend = %self.shard.id,
                        attempt = new_attempt,
                        error = %e,
                        "connect failed",
                    );
                    // Schedule another attempt. We use a detached task to
                    // avoid holding the lifecycle loop in a tight retry
                    // hot-loop and to ensure any new ForwardDone events
                    // can be picked up between attempts.
                    self.queue_self_retry();
                }
            }
        }
    }

    /// Queue a CooldownExpired-like signal back into our own channel to
    /// re-trigger attempt_connect after the next retry. We reuse the
    /// CooldownExpired variant because the only difference is whether
    /// the failed attempt was a probe (which we'd already have detected
    /// and handled separately).
    ///
    /// Implementation: since we're inside `run()`, we cannot recursively
    /// recv. Spawning a tiny task that immediately sends `CooldownExpired`
    /// after the next backoff lets the main loop continue and pick the
    /// event up on its next `recv().await`.
    fn queue_self_retry(&self) {
        let tx = self.shard.lifecycle_sender();
        let attempt = self.shard.reconnect_attempt.load(Ordering::Relaxed);
        let backoff = backoff_for_attempt(attempt);
        tokio::spawn(async move {
            if !backoff.is_zero() {
                tokio::time::sleep(backoff).await;
            }
            let _ = tx.send(LifecycleEvent::CooldownExpired).await;
        });
    }

    fn spawn_cooldown_timer(&self, cooldown: Duration) {
        let tx = self.shard.lifecycle_sender();
        tokio::spawn(async move {
            tokio::time::sleep(cooldown).await;
            let _ = tx.send(LifecycleEvent::CooldownExpired).await;
        });
    }
}

/// Helper to spawn one lifecycle task per backend.
pub fn spawn_lifecycles(
    shards: &[Arc<BackendShard>],
    receivers: Vec<mpsc::Receiver<LifecycleEvent>>,
    dispatch_tx: mpsc::Sender<DispatchEvent>,
    metrics: Arc<Metrics>,
    config: Arc<Config>,
) -> Vec<BackendId> {
    assert_eq!(shards.len(), receivers.len(), "shard/rx mismatch");
    let mut ids = Vec::with_capacity(shards.len());
    for (shard, rx) in shards.iter().zip(receivers) {
        ids.push(shard.id);
        let lifecycle = BackendLifecycle::new(
            shard.clone(),
            rx,
            dispatch_tx.clone(),
            metrics.clone(),
            &config,
        );
        tokio::spawn(lifecycle.run());
    }
    ids
}
