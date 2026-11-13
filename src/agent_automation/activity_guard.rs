//! Visible counters and a circuit breaker for automatic browser activity.
//!
//! Every dispatched browser action is counted. Automatic work (goal recovery,
//! replacement conversations, turn renewal and worker turns) also feeds a
//! rolling window; when it exceeds a limit the breaker opens and that
//! automatic work stays queued until the breaker closes. Owner messages,
//! pairing and new-chat setup are counted but never held.
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Mutex;

pub(crate) const WINDOW_MS: u64 = 10 * 60 * 1000;
pub(crate) const OPEN_MS: u64 = 15 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivityClass {
    /// Owner-driven or setup work. Counted, never held.
    Owner,
    /// Goal recovery and replacement-conversation creation.
    Recovery,
    Renewal,
    WorkerTurn,
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    loads: usize,
    recoveries: usize,
    renewals: usize,
    worker_turns: usize,
}

const LIMITS: Limits = Limits {
    loads: 180,
    recoveries: 12,
    renewals: 12,
    worker_turns: 60,
};

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActivityCounts {
    /// Dispatched browser actions. Each opens (or reuses) its conversation,
    /// so this bounds conversation loads from above.
    pub loads: u64,
    pub sends: u64,
    pub recoveries: u64,
    pub renewals: u64,
    pub worker_turns: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActivitySnapshot {
    pub totals: ActivityCounts,
    pub window: ActivityCounts,
    pub window_ms: u64,
    pub limits: ActivityCounts,
    pub breaker_open: bool,
    pub breaker_open_until_ms: Option<u64>,
    pub breaker_reason: Option<String>,
    pub breaker_trips: u64,
    pub held_actions: u64,
}

#[derive(Default)]
struct State {
    totals: ActivityCounts,
    loads: VecDeque<u64>,
    sends: VecDeque<u64>,
    recoveries: VecDeque<u64>,
    renewals: VecDeque<u64>,
    worker_turns: VecDeque<u64>,
    open_until_ms: Option<u64>,
    reason: Option<String>,
    trips: u64,
    held: u64,
}

impl State {
    fn prune(&mut self, now_ms: u64) {
        let floor = now_ms.saturating_sub(WINDOW_MS);
        for queue in [
            &mut self.loads,
            &mut self.sends,
            &mut self.recoveries,
            &mut self.renewals,
            &mut self.worker_turns,
        ] {
            while queue.front().is_some_and(|at| *at < floor) {
                queue.pop_front();
            }
        }
        if self.open_until_ms.is_some_and(|until| now_ms >= until) {
            self.open_until_ms = None;
            self.reason = None;
        }
    }

    fn trip_if_exceeded(&mut self, now_ms: u64) {
        if self.open_until_ms.is_some() {
            return;
        }
        let exceeded = [
            ("conversation loads", self.loads.len(), LIMITS.loads),
            ("recoveries", self.recoveries.len(), LIMITS.recoveries),
            ("turn renewals", self.renewals.len(), LIMITS.renewals),
            ("worker turns", self.worker_turns.len(), LIMITS.worker_turns),
        ]
        .into_iter()
        .find(|(_, count, limit)| count >= limit);
        if let Some((name, count, limit)) = exceeded {
            self.open_until_ms = Some(now_ms.saturating_add(OPEN_MS));
            self.reason = Some(format!(
                "{count} {name} in {} minutes reached the limit of {limit}",
                WINDOW_MS / 60_000
            ));
            self.trips += 1;
            tracing::warn!(
                reason = self.reason.as_deref().unwrap_or_default(),
                open_ms = OPEN_MS,
                "automatic browser activity circuit breaker opened"
            );
        }
    }
}

#[derive(Default)]
pub struct ActivityGuard {
    state: Mutex<State>,
}

impl ActivityGuard {
    /// Returns the time automatic work may resume, or `None` when this class
    /// may run now. Owner work is never held.
    pub(crate) fn hold_until(&self, class: ActivityClass, now_ms: u64) -> Option<u64> {
        if class == ActivityClass::Owner {
            return None;
        }
        let mut state = self.state.lock().unwrap();
        state.prune(now_ms);
        let until = state.open_until_ms?;
        state.held += 1;
        Some(until)
    }

    /// Records one dispatched browser action.
    pub(crate) fn record_dispatch(&self, class: ActivityClass, now_ms: u64) {
        let mut state = self.state.lock().unwrap();
        state.prune(now_ms);
        state.totals.loads += 1;
        state.loads.push_back(now_ms);
        match class {
            ActivityClass::Owner => {}
            ActivityClass::Recovery => {
                state.totals.recoveries += 1;
                state.recoveries.push_back(now_ms);
            }
            ActivityClass::Renewal => {
                state.totals.renewals += 1;
                state.renewals.push_back(now_ms);
            }
            ActivityClass::WorkerTurn => {
                state.totals.worker_turns += 1;
                state.worker_turns.push_back(now_ms);
            }
        }
        state.trip_if_exceeded(now_ms);
    }

    /// Records a Send the browser confirmed as accepted.
    pub(crate) fn record_send(&self, now_ms: u64) {
        let mut state = self.state.lock().unwrap();
        state.prune(now_ms);
        state.totals.sends += 1;
        state.sends.push_back(now_ms);
    }

    pub fn snapshot(&self, now_ms: u64) -> ActivitySnapshot {
        let mut state = self.state.lock().unwrap();
        state.prune(now_ms);
        ActivitySnapshot {
            totals: state.totals,
            window: ActivityCounts {
                loads: state.loads.len() as u64,
                sends: state.sends.len() as u64,
                recoveries: state.recoveries.len() as u64,
                renewals: state.renewals.len() as u64,
                worker_turns: state.worker_turns.len() as u64,
            },
            window_ms: WINDOW_MS,
            limits: ActivityCounts {
                loads: LIMITS.loads as u64,
                sends: 0,
                recoveries: LIMITS.recoveries as u64,
                renewals: LIMITS.renewals as u64,
                worker_turns: LIMITS.worker_turns as u64,
            },
            breaker_open: state.open_until_ms.is_some(),
            breaker_open_until_ms: state.open_until_ms,
            breaker_reason: state.reason.clone(),
            breaker_trips: state.trips,
            held_actions: state.held,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_work_is_counted_but_never_held() {
        let guard = ActivityGuard::default();
        for at in 0..LIMITS.loads as u64 {
            guard.record_dispatch(ActivityClass::Owner, at);
        }
        let snapshot = guard.snapshot(LIMITS.loads as u64);
        assert!(snapshot.breaker_open, "load volume alone opens the breaker");
        assert_eq!(snapshot.totals.loads, LIMITS.loads as u64);
        assert_eq!(guard.hold_until(ActivityClass::Owner, 1_000), None);
        assert!(guard.hold_until(ActivityClass::WorkerTurn, 1_000).is_some());
    }

    #[test]
    fn repeated_recoveries_open_the_breaker_until_it_expires() {
        let guard = ActivityGuard::default();
        let start = 1_000_000;
        for index in 0..LIMITS.recoveries as u64 - 1 {
            guard.record_dispatch(ActivityClass::Recovery, start + index);
        }
        assert_eq!(guard.hold_until(ActivityClass::Recovery, start + 100), None);
        guard.record_dispatch(ActivityClass::Recovery, start + 200);
        let until = guard
            .hold_until(ActivityClass::Renewal, start + 300)
            .expect("breaker open");
        assert_eq!(until, start + 200 + OPEN_MS);
        let snapshot = guard.snapshot(start + 300);
        assert_eq!(snapshot.breaker_trips, 1);
        assert_eq!(snapshot.held_actions, 1);
        assert!(snapshot.breaker_reason.unwrap().contains("recoveries"));
        assert_eq!(snapshot.window.recoveries, LIMITS.recoveries as u64);

        // After the open period the old events have also left the window.
        assert_eq!(guard.hold_until(ActivityClass::Recovery, until), None);
        let closed = guard.snapshot(until);
        assert!(!closed.breaker_open);
        assert_eq!(closed.window.recoveries, 0);
        assert_eq!(closed.totals.recoveries, LIMITS.recoveries as u64);
    }

    #[test]
    fn spread_out_activity_never_trips() {
        let guard = ActivityGuard::default();
        for index in 0..100u64 {
            let at = index * (WINDOW_MS / 4);
            guard.record_dispatch(ActivityClass::Renewal, at);
            guard.record_send(at);
            assert_eq!(guard.hold_until(ActivityClass::Renewal, at), None);
        }
        let snapshot = guard.snapshot(100 * (WINDOW_MS / 4));
        assert_eq!(snapshot.breaker_trips, 0);
        assert_eq!(snapshot.totals.renewals, 100);
        assert_eq!(snapshot.totals.sends, 100);
    }
}
