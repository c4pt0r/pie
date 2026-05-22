//! RFC 1 (issue #20) dedup window + cycle suppression engine.
//!
//! Pure logic, no IO. `AgentHarness::handle_trigger` (follow-up sub-PR) wraps this engine
//! into the agent loop entrypoint, but the dedup / cycle decisions live here so they are
//! independently testable.
//!
//! Behaviour matches RFC 1 §5:
//! - **Dedup window**: same `idempotency_key` seen twice within
//!   [`TriggerRuntimeConfig::dedup_window`] (default 5 minutes) → outcome depends on the
//!   *first* trigger's [`ReplacementPolicy`] (per RFC 1 §11 fixed decision #4 — sources
//!   declare per-event; the runtime trusts the first arrival's declaration to set the
//!   window's collapse semantics).
//! - **Cycle suppression**: when the same `trace_id` exceeds
//!   [`TriggerRuntimeConfig::cycle_hop_limit`] (default 5) → forced
//!   [`EvaluationOutcome::CycleSuppressed`]. Each accepted trigger bumps the per-trace hop
//!   counter; the runtime calls [`TriggerRuntime::record_follow_up_hop`] before spawning
//!   sub-triggers that share the parent's trace.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use super::trigger::{ReplacementPolicy, Trigger};

/// Tunable knobs for [`TriggerRuntime`]. The runtime never mutates these; callers can swap
/// them via [`TriggerRuntime::new`] or [`TriggerRuntime::with_config`].
#[derive(Clone, Copy, Debug)]
pub struct TriggerRuntimeConfig {
    /// How long after a successful admission the same `idempotency_key` is considered a
    /// duplicate. RFC 1 §5 default: 5 minutes. Capped at 24h to bound memory.
    pub dedup_window: Duration,
    /// Maximum `trace_id` chain depth before the runtime forces
    /// [`EvaluationOutcome::CycleSuppressed`]. RFC 1 §5 default: 5.
    pub cycle_hop_limit: u32,
}

impl TriggerRuntimeConfig {
    pub const DEFAULT_DEDUP_WINDOW: Duration = Duration::from_secs(5 * 60);
    pub const DEFAULT_CYCLE_HOP_LIMIT: u32 = 5;
    /// Upper bound enforced by [`TriggerRuntime::with_config`]; anything larger is clamped
    /// down because the dedup registry is in-memory and would otherwise grow unbounded.
    pub const MAX_DEDUP_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
}

impl Default for TriggerRuntimeConfig {
    fn default() -> Self {
        Self {
            dedup_window: Self::DEFAULT_DEDUP_WINDOW,
            cycle_hop_limit: Self::DEFAULT_CYCLE_HOP_LIMIT,
        }
    }
}

/// Result of running a [`Trigger`] through [`TriggerRuntime::evaluate`]. Subsequent runtime
/// state (state machine transitions, session audit, permission evaluator) consumes this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvaluationOutcome {
    /// First admission of this `idempotency_key` in the current dedup window AND the
    /// `trace_id` is still within `cycle_hop_limit`. The runtime should advance the state
    /// machine to `Accepted` (subject to subsequent permission evaluation).
    Accept,
    /// The same `idempotency_key` has been seen before in the dedup window. The first
    /// trigger's `ReplacementPolicy` decides what happens; the runtime's audit record
    /// captures the previous `trace_id` so the user can correlate which event "won".
    Deduped {
        replacement_policy: ReplacementPolicy,
        previous_trace_id: String,
    },
    /// Cycle suppression fired: the `trace_id` has already passed through this runtime
    /// `hop_count` times, exceeding `cycle_hop_limit`.
    CycleSuppressed { hop_count: u32 },
}

/// In-memory dedup + cycle registry shared across all `NotificationHook` sources for a
/// single agent / daemon. Cloning is cheap; the actual state lives behind an `Arc<Mutex>`.
#[derive(Clone, Debug)]
pub struct TriggerRuntime {
    inner: std::sync::Arc<Mutex<Inner>>,
    config: TriggerRuntimeConfig,
}

impl Default for TriggerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct Inner {
    /// `idempotency_key` → first-arrival entry. Pruned lazily on every [`evaluate`].
    dedup: HashMap<String, DedupEntry>,
    /// `trace_id` → hop count. Lazy pruning is not safe here (we cannot tell when a trace
    /// is "done"), so we cap each entry's lifetime to one cycle window (= `dedup_window`,
    /// reused for simplicity) and prune the same way as the dedup map.
    cycle: HashMap<String, CycleEntry>,
    /// Monotonic counters surfaced through [`TriggerRuntime::snapshot`] for TUI / `/triggers`
    /// observability. These never decrement and survive entry pruning.
    deduped_total: u64,
    cycle_suppressed_total: u64,
    accepted_total: u64,
}

/// Point-in-time view of the runtime's dedup + cycle bookkeeping. Cheap to copy; used by
/// [`super::agent_harness::AgentHarness::notification_status_snapshot`] for status banners
/// and `/triggers` rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TriggerRuntimeSnapshot {
    /// Number of distinct `idempotency_key` entries currently inside the dedup window.
    pub dedup_entries: usize,
    /// Number of distinct `trace_id` chains currently inside the cycle window.
    pub active_traces: usize,
    /// Lifetime count of triggers that admitted (advanced the dedup map + cycle counter).
    pub accepted_total: u64,
    /// Lifetime count of triggers that were dropped because their `idempotency_key`
    /// matched an entry still inside the dedup window.
    pub deduped_total: u64,
    /// Lifetime count of triggers that were dropped because their `trace_id` exceeded
    /// `cycle_hop_limit`.
    pub cycle_suppressed_total: u64,
}

#[derive(Clone, Debug)]
struct DedupEntry {
    received_at: DateTime<Utc>,
    replacement_policy: ReplacementPolicy,
    trace_id: String,
}

#[derive(Clone, Debug)]
struct CycleEntry {
    last_seen_at: DateTime<Utc>,
    hop_count: u32,
}

impl TriggerRuntime {
    /// Construct a runtime with [`TriggerRuntimeConfig::default`].
    pub fn new() -> Self {
        Self::with_config(TriggerRuntimeConfig::default())
    }

    /// Construct a runtime with a custom config. `dedup_window` is clamped to
    /// [`TriggerRuntimeConfig::MAX_DEDUP_WINDOW`].
    pub fn with_config(mut config: TriggerRuntimeConfig) -> Self {
        if config.dedup_window > TriggerRuntimeConfig::MAX_DEDUP_WINDOW {
            config.dedup_window = TriggerRuntimeConfig::MAX_DEDUP_WINDOW;
        }
        Self {
            inner: std::sync::Arc::new(Mutex::new(Inner {
                dedup: HashMap::new(),
                cycle: HashMap::new(),
                deduped_total: 0,
                cycle_suppressed_total: 0,
                accepted_total: 0,
            })),
            config,
        }
    }

    /// Point-in-time view of the dedup / cycle bookkeeping plus lifetime counters. Intended
    /// for status banners; cheap (one mutex lock + struct copy). Lifetime counters never
    /// decrement so consumers can build delta UIs without missing intermediate events.
    pub fn snapshot(&self) -> TriggerRuntimeSnapshot {
        let inner = self.inner.lock();
        TriggerRuntimeSnapshot {
            dedup_entries: inner.dedup.len(),
            active_traces: inner.cycle.len(),
            accepted_total: inner.accepted_total,
            deduped_total: inner.deduped_total,
            cycle_suppressed_total: inner.cycle_suppressed_total,
        }
    }

    /// Convenience getter for the active configuration. Useful in tests and status output.
    pub fn config(&self) -> TriggerRuntimeConfig {
        self.config
    }

    /// Decide whether a fresh trigger should be admitted, deduped, or cycle-suppressed.
    /// Pure (modulo wall-clock pruning); does NOT advance the trigger state machine —
    /// that's the harness's job after it sees the outcome.
    ///
    /// Side effects (when the outcome is [`EvaluationOutcome::Accept`]):
    /// - inserts the `idempotency_key` → first-arrival entry into the dedup map
    /// - bumps the `trace_id` hop counter
    ///
    /// On [`EvaluationOutcome::Deduped`] or [`EvaluationOutcome::CycleSuppressed`] the
    /// internal maps are *not* mutated for that trigger (the prior entry stands; the cycle
    /// counter does not advance on a suppressed trigger).
    pub fn evaluate(&self, trigger: &Trigger) -> EvaluationOutcome {
        let mut inner = self.inner.lock();
        let now = trigger.received_at;

        prune_expired(&mut inner.dedup, now, self.config.dedup_window);
        prune_expired_cycle(&mut inner.cycle, now, self.config.dedup_window);

        // Dedup check runs first because a duplicate event is never "real" for cycle
        // counting — we do not want a deduped event to consume hop budget.
        if let Some(prev) = inner.dedup.get(&trigger.idempotency_key) {
            let outcome = EvaluationOutcome::Deduped {
                replacement_policy: prev.replacement_policy,
                previous_trace_id: prev.trace_id.clone(),
            };
            inner.deduped_total = inner.deduped_total.saturating_add(1);
            return outcome;
        }

        // Cycle check runs against the trace counter as it stands BEFORE this trigger; if
        // we are already at the limit, suppress without advancing.
        if let Some(existing) = inner.cycle.get(&trigger.trace_id) {
            if existing.hop_count >= self.config.cycle_hop_limit {
                let outcome = EvaluationOutcome::CycleSuppressed {
                    hop_count: existing.hop_count,
                };
                inner.cycle_suppressed_total = inner.cycle_suppressed_total.saturating_add(1);
                return outcome;
            }
        }

        // Admit. Record both the dedup entry and the hop bump in one atomic critical section.
        inner.dedup.insert(
            trigger.idempotency_key.clone(),
            DedupEntry {
                received_at: now,
                replacement_policy: trigger.replacement_policy,
                trace_id: trigger.trace_id.clone(),
            },
        );
        inner
            .cycle
            .entry(trigger.trace_id.clone())
            .and_modify(|e| {
                e.hop_count = e.hop_count.saturating_add(1);
                e.last_seen_at = now;
            })
            .or_insert(CycleEntry {
                hop_count: 1,
                last_seen_at: now,
            });
        inner.accepted_total = inner.accepted_total.saturating_add(1);

        EvaluationOutcome::Accept
    }

    /// Record an additional hop on `trace_id` without going through dedup. Called by the
    /// harness immediately before spawning a follow-up trigger that inherits the parent's
    /// trace (e.g. an `AgentDelegate` trigger emitted by a tool call).
    ///
    /// `now` is wall-clock time at the moment the follow-up is queued; used both to bump
    /// the entry's `last_seen_at` and to drive lazy pruning of stale trace entries.
    pub fn record_follow_up_hop(&self, trace_id: &str, now: DateTime<Utc>) {
        let mut inner = self.inner.lock();
        prune_expired_cycle(&mut inner.cycle, now, self.config.dedup_window);
        inner
            .cycle
            .entry(trace_id.to_string())
            .and_modify(|e| {
                e.hop_count = e.hop_count.saturating_add(1);
                e.last_seen_at = now;
            })
            .or_insert(CycleEntry {
                hop_count: 1,
                last_seen_at: now,
            });
    }

    /// Test helper: snapshot the current dedup map size. Public for white-box tests; not
    /// part of the public surface users build against.
    #[cfg(test)]
    pub(crate) fn dedup_entry_count(&self) -> usize {
        self.inner.lock().dedup.len()
    }

    /// Test helper: snapshot the current trace map size.
    #[cfg(test)]
    pub(crate) fn cycle_entry_count(&self) -> usize {
        self.inner.lock().cycle.len()
    }
}

fn prune_expired(map: &mut HashMap<String, DedupEntry>, now: DateTime<Utc>, window: Duration) {
    let cutoff =
        now - chrono::Duration::from_std(window).expect("dedup_window fits in chrono::Duration");
    map.retain(|_, entry| entry.received_at >= cutoff);
}

fn prune_expired_cycle(
    map: &mut HashMap<String, CycleEntry>,
    now: DateTime<Utc>,
    window: Duration,
) {
    let cutoff =
        now - chrono::Duration::from_std(window).expect("dedup_window fits in chrono::Duration");
    map.retain(|_, entry| entry.last_seen_at >= cutoff);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::trigger::{
        CredentialScope, PayloadVisibility, SourceKind, TriggerAuthority, TriggerSource,
    };

    fn make_trigger(idempotency: &str, trace: &str, policy: ReplacementPolicy) -> Trigger {
        make_trigger_at(idempotency, trace, policy, fixed_now())
    }

    fn make_trigger_at(
        idempotency: &str,
        trace: &str,
        policy: ReplacementPolicy,
        received_at: DateTime<Utc>,
    ) -> Trigger {
        Trigger {
            source: TriggerSource::Local {
                subkind: "test".into(),
            },
            source_kind: SourceKind::Local,
            source_label: "test".into(),
            event_label: "fire".into(),
            payload_visibility: PayloadVisibility::Local,
            payload_summary: None,
            payload: None,
            idempotency_key: idempotency.into(),
            replacement_policy: policy,
            trace_id: trace.into(),
            authority: TriggerAuthority {
                principal_id: "test:principal".into(),
                principal_label: "test".into(),
                credential_scope: CredentialScope::Project,
                allowed_source_actions: vec![],
                expires_at: None,
            },
            received_at,
        }
    }

    fn fixed_now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    #[test]
    fn first_admission_accepts() {
        let runtime = TriggerRuntime::new();
        let outcome = runtime.evaluate(&make_trigger("k1", "t1", ReplacementPolicy::Drop));
        assert_eq!(outcome, EvaluationOutcome::Accept);
        assert_eq!(runtime.dedup_entry_count(), 1);
        assert_eq!(runtime.cycle_entry_count(), 1);
    }

    #[test]
    fn duplicate_within_window_returns_deduped_with_previous_trace_id() {
        let runtime = TriggerRuntime::new();
        runtime.evaluate(&make_trigger(
            "k1",
            "trace-original",
            ReplacementPolicy::Drop,
        ));
        let outcome = runtime.evaluate(&make_trigger(
            "k1",
            "trace-duplicate",
            ReplacementPolicy::Drop,
        ));
        assert_eq!(
            outcome,
            EvaluationOutcome::Deduped {
                replacement_policy: ReplacementPolicy::Drop,
                previous_trace_id: "trace-original".into()
            }
        );
        // Deduped event does not mutate the registry: cycle counter for the duplicate's
        // trace must NOT have been bumped, because dedup short-circuits before cycle.
        assert_eq!(
            runtime.cycle_entry_count(),
            1,
            "duplicate trigger must not allocate a cycle entry for its trace_id"
        );
    }

    #[test]
    fn deduped_outcome_carries_first_arrivals_replacement_policy() {
        // RFC 1 §5 + §11 #4: the first arrival's policy decides the window's collapse
        // semantics. Verify the engine reports back the FIRST policy even when subsequent
        // duplicates declare a different one.
        let runtime = TriggerRuntime::new();
        runtime.evaluate(&make_trigger("k1", "t1", ReplacementPolicy::LatestReplaces));
        let outcome = runtime.evaluate(&make_trigger("k1", "t2", ReplacementPolicy::Drop));
        match outcome {
            EvaluationOutcome::Deduped {
                replacement_policy, ..
            } => assert_eq!(
                replacement_policy,
                ReplacementPolicy::LatestReplaces,
                "first arrival's policy MUST win in the dedup window per RFC 1 §5"
            ),
            other => panic!("expected Deduped, got {other:?}"),
        }
    }

    #[test]
    fn dedup_window_expiry_re_admits_same_key() {
        let runtime = TriggerRuntime::with_config(TriggerRuntimeConfig {
            dedup_window: Duration::from_secs(60),
            cycle_hop_limit: 10,
        });
        let t0 = fixed_now();
        runtime.evaluate(&make_trigger_at("k1", "t1", ReplacementPolicy::Drop, t0));
        // Within window → dedup.
        let just_under = t0 + chrono::Duration::seconds(59);
        let still_dup = runtime.evaluate(&make_trigger_at(
            "k1",
            "t2",
            ReplacementPolicy::Drop,
            just_under,
        ));
        assert!(matches!(still_dup, EvaluationOutcome::Deduped { .. }));
        // Past window → fresh accept; the prior entry has been pruned.
        let past_window = t0 + chrono::Duration::seconds(61);
        let outcome = runtime.evaluate(&make_trigger_at(
            "k1",
            "t3",
            ReplacementPolicy::Drop,
            past_window,
        ));
        assert_eq!(outcome, EvaluationOutcome::Accept);
        assert_eq!(
            runtime.dedup_entry_count(),
            1,
            "only the freshest entry remains after window expiry + re-admit"
        );
    }

    #[test]
    fn cycle_limit_suppresses_when_trace_exceeds_hop_count() {
        let runtime = TriggerRuntime::with_config(TriggerRuntimeConfig {
            dedup_window: Duration::from_secs(300),
            cycle_hop_limit: 3,
        });
        let trace = "trace-loop";
        // Three accepts get us to hop_count = 3 == limit.
        for i in 0..3 {
            let outcome = runtime.evaluate(&make_trigger(
                &format!("k{i}"),
                trace,
                ReplacementPolicy::Drop,
            ));
            assert_eq!(outcome, EvaluationOutcome::Accept, "iteration {i}");
        }
        // Fourth trigger on same trace: must be suppressed, reporting the current hop
        // count BEFORE the suppression (since we do not advance the counter on suppress).
        let suppressed = runtime.evaluate(&make_trigger("k4", trace, ReplacementPolicy::Drop));
        assert_eq!(
            suppressed,
            EvaluationOutcome::CycleSuppressed { hop_count: 3 },
            "suppression reports the pre-block hop count so the audit shows where the chain stopped"
        );
    }

    #[test]
    fn record_follow_up_hop_does_not_require_a_trigger() {
        // The harness may bump the hop counter before spawning a sub-trigger (e.g. a tool
        // call that will produce an AgentDelegate event); make sure the helper exists and
        // contributes to cycle suppression even without a Trigger envelope on hand.
        let runtime = TriggerRuntime::with_config(TriggerRuntimeConfig {
            dedup_window: Duration::from_secs(300),
            cycle_hop_limit: 2,
        });
        let trace = "trace-followup";
        // One real trigger → hop_count = 1.
        let outcome = runtime.evaluate(&make_trigger("k1", trace, ReplacementPolicy::Drop));
        assert_eq!(outcome, EvaluationOutcome::Accept);
        // A follow-up hop recorded by the harness → hop_count = 2 (= limit).
        runtime.record_follow_up_hop(trace, fixed_now());
        // Next real trigger on the same trace must already be suppressed.
        let suppressed = runtime.evaluate(&make_trigger("k2", trace, ReplacementPolicy::Drop));
        assert_eq!(
            suppressed,
            EvaluationOutcome::CycleSuppressed { hop_count: 2 }
        );
    }

    #[test]
    fn dedup_window_clamped_to_max_24h() {
        let runtime = TriggerRuntime::with_config(TriggerRuntimeConfig {
            dedup_window: Duration::from_secs(48 * 60 * 60),
            cycle_hop_limit: 5,
        });
        assert_eq!(
            runtime.config().dedup_window,
            TriggerRuntimeConfig::MAX_DEDUP_WINDOW,
            "dedup_window MUST be clamped to MAX_DEDUP_WINDOW to bound memory"
        );
    }

    #[test]
    fn cycle_entries_for_unrelated_traces_are_independent() {
        let runtime = TriggerRuntime::with_config(TriggerRuntimeConfig {
            dedup_window: Duration::from_secs(300),
            cycle_hop_limit: 2,
        });
        // trace-a hits the limit
        runtime.evaluate(&make_trigger("k-a-1", "trace-a", ReplacementPolicy::Drop));
        runtime.evaluate(&make_trigger("k-a-2", "trace-a", ReplacementPolicy::Drop));
        assert!(matches!(
            runtime.evaluate(&make_trigger("k-a-3", "trace-a", ReplacementPolicy::Drop)),
            EvaluationOutcome::CycleSuppressed { .. }
        ));
        // trace-b is unaffected
        assert_eq!(
            runtime.evaluate(&make_trigger("k-b-1", "trace-b", ReplacementPolicy::Drop)),
            EvaluationOutcome::Accept
        );
    }

    #[test]
    fn snapshot_counters_track_each_outcome() {
        let runtime = TriggerRuntime::with_config(TriggerRuntimeConfig {
            dedup_window: Duration::from_secs(300),
            cycle_hop_limit: 2,
        });
        // accepted: 2 (two distinct keys / trace)
        runtime.evaluate(&make_trigger("k1", "ta", ReplacementPolicy::Drop));
        runtime.evaluate(&make_trigger("k2", "tb", ReplacementPolicy::Drop));
        // deduped: 1 (duplicate of k1)
        runtime.evaluate(&make_trigger("k1", "tc", ReplacementPolicy::Drop));
        // cycle-suppress: take trace-a to limit + 1 over
        runtime.evaluate(&make_trigger("k3", "ta", ReplacementPolicy::Drop));
        // ta is now at hop 2 (limit). next evaluate hits CycleSuppressed.
        runtime.evaluate(&make_trigger("k4", "ta", ReplacementPolicy::Drop));

        let snap = runtime.snapshot();
        assert_eq!(snap.accepted_total, 3, "snapshot: {snap:?}");
        assert_eq!(snap.deduped_total, 1, "snapshot: {snap:?}");
        assert_eq!(snap.cycle_suppressed_total, 1, "snapshot: {snap:?}");
        assert!(
            snap.dedup_entries >= 1,
            "dedup map must hold at least one live entry"
        );
        assert!(
            snap.active_traces >= 1,
            "cycle map must hold at least one live trace"
        );
    }
}
