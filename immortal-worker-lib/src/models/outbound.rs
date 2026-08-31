use immortal_lib::immortal::{
    ActivityHeartbeatV1, ImmortalServerActionV1, Log, Metrics, immortal_server_action_v1,
};
use prost::Message;
use std::collections::{HashMap, VecDeque};
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug)]
pub(crate) struct OutboundLimits {
    pub log_records: usize,
    pub log_bytes: usize,
    pub max_record_bytes: usize,
    pub activity_records: usize,
    pub activity_fallback_bytes: usize,
    pub control_records: usize,
    pub log_batch_records: usize,
}

impl OutboundLimits {
    pub(crate) fn validate(self) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.log_records > 0,
            "worker log queue record limit must be greater than zero"
        );
        anyhow::ensure!(
            self.log_bytes > 0,
            "worker log queue byte limit must be greater than zero"
        );
        anyhow::ensure!(
            self.max_record_bytes > 0,
            "worker maximum log record size must be greater than zero"
        );
        anyhow::ensure!(
            self.activity_records > 0,
            "worker activity lane capacity must be greater than zero"
        );
        anyhow::ensure!(
            self.activity_fallback_bytes > 0,
            "worker activity fallback byte limit must be greater than zero"
        );
        anyhow::ensure!(
            self.control_records > 0,
            "worker control lane capacity must be greater than zero"
        );
        anyhow::ensure!(
            self.log_batch_records > 0,
            "worker log batch size must be greater than zero"
        );
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutboundStatsSnapshot {
    pub log_records: usize,
    pub log_bytes: usize,
    pub heartbeat_slots: usize,
    pub activity_fallback_records: usize,
    pub activity_fallback_bytes: usize,
    pub metrics_pending: bool,
    pub dropped_logs: u64,
    pub truncated_activity_logs: u64,
    pub activity_fallback_replacements: u64,
    pub dropped_activity_fallbacks: u64,
    pub dropped_heartbeats: u64,
    pub stripped_heartbeat_details: u64,
    pub dropped_control: u64,
}

#[derive(Default)]
struct OutboundStats {
    dropped_logs: AtomicU64,
    truncated_activity_logs: AtomicU64,
    activity_fallback_replacements: AtomicU64,
    dropped_activity_fallbacks: AtomicU64,
    dropped_heartbeats: AtomicU64,
    stripped_heartbeat_details: AtomicU64,
    dropped_control: AtomicU64,
}

impl OutboundStats {
    fn snapshot(&self, state: &OutboundState) -> OutboundStatsSnapshot {
        OutboundStatsSnapshot {
            log_records: state.logs.len(),
            log_bytes: state.log_bytes,
            heartbeat_slots: state.heartbeats.values.len(),
            activity_fallback_records: state.activity_logs.values.len(),
            activity_fallback_bytes: state.activity_log_bytes,
            metrics_pending: state.metrics.is_some(),
            dropped_logs: self.dropped_logs.load(Ordering::Relaxed),
            truncated_activity_logs: self.truncated_activity_logs.load(Ordering::Relaxed),
            activity_fallback_replacements: self
                .activity_fallback_replacements
                .load(Ordering::Relaxed),
            dropped_activity_fallbacks: self.dropped_activity_fallbacks.load(Ordering::Relaxed),
            dropped_heartbeats: self.dropped_heartbeats.load(Ordering::Relaxed),
            stripped_heartbeat_details: self.stripped_heartbeat_details.load(Ordering::Relaxed),
            dropped_control: self.dropped_control.load(Ordering::Relaxed),
        }
    }
}

struct LatestByActivity<T> {
    order: VecDeque<String>,
    values: HashMap<String, T>,
}

impl<T> Default for LatestByActivity<T> {
    fn default() -> Self {
        Self {
            order: VecDeque::new(),
            values: HashMap::new(),
        }
    }
}

enum LatestInsert<T> {
    Inserted,
    Replaced(T),
    Full(T),
}

impl<T> LatestByActivity<T> {
    fn insert(&mut self, key: String, value: T, capacity: usize) -> LatestInsert<T> {
        if let Some(previous) = self.values.insert(key.clone(), value) {
            return LatestInsert::Replaced(previous);
        }
        if self.values.len() > capacity {
            let value = self.values.remove(&key).expect("value was just inserted");
            return LatestInsert::Full(value);
        }
        self.order.push_back(key);
        LatestInsert::Inserted
    }

    fn pop_front(&mut self) -> Option<T> {
        while let Some(key) = self.order.pop_front() {
            if let Some(value) = self.values.remove(&key) {
                return Some(value);
            }
        }
        None
    }

    fn remove(&mut self, key: &str) -> Option<T> {
        self.values.remove(key)
    }
}

#[derive(Default)]
struct OutboundState {
    active_runs: HashMap<String, ActiveRun>,
    control: VecDeque<ImmortalServerActionV1>,
    heartbeats: LatestByActivity<ActivityHeartbeatV1>,
    activity_logs: LatestByActivity<Log>,
    activity_log_bytes: usize,
    logs: VecDeque<Log>,
    log_bytes: usize,
    metrics: Option<Metrics>,
    logs_since_metrics: usize,
}

struct ActiveRun {
    run_id: String,
    workflow_id: String,
    workflow_epoch: u64,
}

struct OutboundInner {
    limits: OutboundLimits,
    state: Mutex<OutboundState>,
    notify: Notify,
    stats: OutboundStats,
}

/// Cloneable, synchronous producer half of the worker's bounded outbound lanes.
///
/// Producers never wait for queue capacity. Logs degrade into the bounded per-activity fallback
/// or are dropped, while heartbeats and metrics keep only their latest value.
#[derive(Clone)]
pub struct WorkerOutboundSender {
    inner: Arc<OutboundInner>,
}

/// Single-consumer scheduler half. It is cloneable solely so a reconnect can construct a fresh
/// request stream without moving queued state out of the worker.
#[derive(Clone)]
pub struct WorkerOutboundReceiver {
    inner: Arc<OutboundInner>,
}

#[derive(Debug)]
pub struct OutboundSendError;

impl Display for OutboundSendError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("worker outbound control lane is full")
    }
}

impl std::error::Error for OutboundSendError {}

pub(crate) fn outbound_channel(
    limits: OutboundLimits,
) -> (WorkerOutboundSender, WorkerOutboundReceiver) {
    let inner = Arc::new(OutboundInner {
        limits,
        state: Mutex::new(OutboundState::default()),
        notify: Notify::new(),
        stats: OutboundStats::default(),
    });
    (
        WorkerOutboundSender {
            inner: Arc::clone(&inner),
        },
        WorkerOutboundReceiver { inner },
    )
}

impl WorkerOutboundSender {
    /// Compatibility-shaped entry point for existing worker producers. A successful return means
    /// the action was accepted, coalesced, moved to the activity fallback, or deliberately
    /// dropped according to the bounded overload policy.
    pub fn send(&self, action: ImmortalServerActionV1) -> Result<usize, OutboundSendError> {
        match action.action {
            Some(immortal_server_action_v1::Action::LogEvent(log)) => {
                self.send_log(log);
                Ok(1)
            }
            Some(immortal_server_action_v1::Action::ActivityHeartbeatV1(heartbeat)) => {
                self.send_heartbeat(heartbeat);
                Ok(1)
            }
            Some(immortal_server_action_v1::Action::Metrics(metrics)) => {
                self.send_metrics(metrics);
                Ok(1)
            }
            action => {
                let action = ImmortalServerActionV1 { action };
                if action.encoded_len() > self.inner.limits.max_record_bytes {
                    self.inner
                        .stats
                        .dropped_control
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(OutboundSendError);
                }
                let mut state = lock(&self.inner.state);
                if state.control.len() >= self.inner.limits.control_records {
                    self.inner
                        .stats
                        .dropped_control
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(OutboundSendError);
                }
                state.control.push_back(action);
                drop(state);
                self.inner.notify.notify_one();
                Ok(1)
            }
        }
    }

    pub(crate) fn start_activity(
        &self,
        activity_id: &str,
        activity_run_id: &str,
        workflow_id: &str,
        workflow_epoch: u64,
    ) -> bool {
        let mut state = lock(&self.inner.state);
        if !state.active_runs.contains_key(activity_id)
            && state.active_runs.len() >= self.inner.limits.activity_records
        {
            return false;
        }

        let replacing_run = state
            .active_runs
            .insert(
                activity_id.to_string(),
                ActiveRun {
                    run_id: activity_run_id.to_string(),
                    workflow_id: workflow_id.to_string(),
                    workflow_epoch,
                },
            )
            .is_some_and(|previous| previous.run_id != activity_run_id);
        if replacing_run {
            state.heartbeats.remove(activity_id);
            if let Some(log) = state.activity_logs.remove(activity_id) {
                state.activity_log_bytes =
                    state.activity_log_bytes.saturating_sub(log.encoded_len());
            }
        }
        true
    }

    pub(crate) fn finish_activity(&self, activity_id: &str, activity_run_id: &str) {
        let mut state = lock(&self.inner.state);
        if state
            .active_runs
            .get(activity_id)
            .map(|run| run.run_id.as_str())
            != Some(activity_run_id)
        {
            return;
        }
        state.active_runs.remove(activity_id);
        state.heartbeats.remove(activity_id);
        if let Some(log) = state.activity_logs.remove(activity_id) {
            state.activity_log_bytes = state.activity_log_bytes.saturating_sub(log.encoded_len());
        }
    }

    pub fn stats(&self) -> OutboundStatsSnapshot {
        let state = lock(&self.inner.state);
        self.inner.stats.snapshot(&state)
    }

    fn send_heartbeat(&self, mut heartbeat: ActivityHeartbeatV1) {
        if heartbeat.encoded_len() > self.inner.limits.max_record_bytes {
            heartbeat.details = None;
            self.inner
                .stats
                .stripped_heartbeat_details
                .fetch_add(1, Ordering::Relaxed);
        }
        if heartbeat.encoded_len() > self.inner.limits.max_record_bytes {
            self.inner
                .stats
                .dropped_heartbeats
                .fetch_add(1, Ordering::Relaxed);
            return;
        }

        let key = heartbeat.activity_id.clone();
        let mut state = lock(&self.inner.state);
        if state.active_runs.get(&key).map(|run| run.run_id.as_str())
            != Some(heartbeat.activity_run_id.as_str())
        {
            self.inner
                .stats
                .dropped_heartbeats
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        if matches!(
            state
                .heartbeats
                .insert(key, heartbeat, self.inner.limits.activity_records),
            LatestInsert::Full(_)
        ) {
            self.inner
                .stats
                .dropped_heartbeats
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        drop(state);
        self.inner.notify.notify_one();
    }

    fn send_metrics(&self, metrics: Metrics) {
        let mut state = lock(&self.inner.state);
        state.metrics = Some(metrics);
        drop(state);
        self.inner.notify.notify_one();
    }

    fn send_log(&self, log: Log) {
        self.note_log_progress(&log);
        let log_size = log.encoded_len();
        if log_size <= self.inner.limits.max_record_bytes {
            let mut state = lock(&self.inner.state);
            let count_available = state.logs.len() < self.inner.limits.log_records;
            let bytes_available = state
                .log_bytes
                .checked_add(log_size)
                .is_some_and(|bytes| bytes <= self.inner.limits.log_bytes);
            if count_available && bytes_available {
                state.log_bytes += log_size;
                state.logs.push_back(log);
                drop(state);
                self.inner.notify.notify_one();
                return;
            }
        }

        self.inner
            .stats
            .dropped_logs
            .fetch_add(1, Ordering::Relaxed);
        self.send_activity_fallback(log, log_size > self.inner.limits.max_record_bytes);
    }

    fn note_log_progress(&self, log: &Log) {
        let Some((activity_id, activity_run_id)) = qualifying_activity(log) else {
            return;
        };
        let heartbeat = {
            let state = lock(&self.inner.state);
            let Some(active) = state.active_runs.get(activity_id) else {
                return;
            };
            if active.run_id != activity_run_id {
                return;
            }
            ActivityHeartbeatV1 {
                workflow_id: active.workflow_id.clone(),
                activity_id: activity_id.to_owned(),
                activity_run_id: activity_run_id.to_owned(),
                workflow_epoch: active.workflow_epoch,
                details: None,
            }
        };
        self.send_heartbeat(heartbeat);
    }

    fn send_activity_fallback(&self, log: Log, oversized: bool) {
        let Some((activity_id, activity_run_id)) =
            qualifying_activity(&log).map(|(activity_id, activity_run_id)| {
                (activity_id.to_owned(), activity_run_id.to_owned())
            })
        else {
            return;
        };
        {
            let state = lock(&self.inner.state);
            if state
                .active_runs
                .get(&activity_id)
                .map(|run| run.run_id.as_str())
                != Some(activity_run_id.as_str())
            {
                return;
            }
        }
        let Some((log, truncated)) = fit_log(log, self.inner.limits.max_record_bytes) else {
            self.inner
                .stats
                .dropped_activity_fallbacks
                .fetch_add(1, Ordering::Relaxed);
            return;
        };
        if oversized || truncated {
            self.inner
                .stats
                .truncated_activity_logs
                .fetch_add(1, Ordering::Relaxed);
        }

        let log_size = log.encoded_len();
        let mut state = lock(&self.inner.state);
        if state
            .active_runs
            .get(&activity_id)
            .map(|run| run.run_id.as_str())
            != Some(activity_run_id.as_str())
        {
            return;
        }
        let old_size = state
            .activity_logs
            .values
            .get(&activity_id)
            .map(Message::encoded_len)
            .unwrap_or(0);
        let next_bytes = state
            .activity_log_bytes
            .saturating_sub(old_size)
            .saturating_add(log_size);
        if next_bytes > self.inner.limits.activity_fallback_bytes {
            self.inner
                .stats
                .dropped_activity_fallbacks
                .fetch_add(1, Ordering::Relaxed);
            return;
        }

        match state
            .activity_logs
            .insert(activity_id, log, self.inner.limits.activity_records)
        {
            LatestInsert::Inserted => {
                state.activity_log_bytes = next_bytes;
            }
            LatestInsert::Replaced(_) => {
                state.activity_log_bytes = next_bytes;
                self.inner
                    .stats
                    .activity_fallback_replacements
                    .fetch_add(1, Ordering::Relaxed);
            }
            LatestInsert::Full(_) => {
                self.inner
                    .stats
                    .dropped_activity_fallbacks
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        drop(state);
        self.inner.notify.notify_one();
    }
}

impl WorkerOutboundReceiver {
    pub(crate) async fn recv(&self) -> ImmortalServerActionV1 {
        loop {
            let notified = self.inner.notify.notified();
            if let Some(action) = self.try_recv() {
                return action;
            }
            notified.await;
        }
    }

    pub(crate) fn try_recv(&self) -> Option<ImmortalServerActionV1> {
        let mut state = lock(&self.inner.state);

        if let Some(action) = state.control.pop_front() {
            return Some(action);
        }
        if let Some(heartbeat) = state.heartbeats.pop_front() {
            return Some(ImmortalServerActionV1 {
                action: Some(immortal_server_action_v1::Action::ActivityHeartbeatV1(
                    heartbeat,
                )),
            });
        }
        if let Some(log) = state.activity_logs.pop_front() {
            state.activity_log_bytes = state.activity_log_bytes.saturating_sub(log.encoded_len());
            return Some(log_action(log));
        }

        if !state.logs.is_empty()
            && (state.logs_since_metrics < self.inner.limits.log_batch_records
                || state.metrics.is_none())
        {
            state.logs_since_metrics += 1;
            return pop_general_log(&mut state);
        }
        if let Some(metrics) = state.metrics.take() {
            state.logs_since_metrics = 0;
            return Some(ImmortalServerActionV1 {
                action: Some(immortal_server_action_v1::Action::Metrics(metrics)),
            });
        }
        if !state.logs.is_empty() {
            state.logs_since_metrics = 1;
            return pop_general_log(&mut state);
        }
        state.logs_since_metrics = 0;
        None
    }
}

fn pop_general_log(state: &mut OutboundState) -> Option<ImmortalServerActionV1> {
    let log = state.logs.pop_front()?;
    state.log_bytes = state.log_bytes.saturating_sub(log.encoded_len());
    Some(log_action(log))
}

fn log_action(log: Log) -> ImmortalServerActionV1 {
    ImmortalServerActionV1 {
        action: Some(immortal_server_action_v1::Action::LogEvent(log)),
    }
}

fn qualifying_activity(log: &Log) -> Option<(&str, &str)> {
    match (log.activity_id.as_deref(), log.activity_run_id.as_deref()) {
        (Some(activity_id), Some(run_id)) if !activity_id.is_empty() && !run_id.is_empty() => {
            Some((activity_id, run_id))
        }
        _ => None,
    }
}

fn fit_log(mut log: Log, max_bytes: usize) -> Option<(Log, bool)> {
    if log.encoded_len() <= max_bytes {
        return Some((log, false));
    }

    log.metadata = None;
    let message = std::mem::take(&mut log.message);
    if log.encoded_len() > max_bytes {
        return None;
    }

    let base_size = log.encoded_len();
    let mut end = message.len().min(max_bytes.saturating_sub(base_size));
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    log.message.push_str(&message[..end]);
    while log.encoded_len() > max_bytes && !log.message.is_empty() {
        log.message.pop();
    }
    (log.encoded_len() <= max_bytes).then_some((log, true))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> OutboundLimits {
        OutboundLimits {
            log_records: 2,
            log_bytes: 1_024,
            max_record_bytes: 256,
            activity_records: 2,
            activity_fallback_bytes: 512,
            control_records: 2,
            log_batch_records: 2,
        }
    }

    fn log(message: &str, activity: Option<(&str, &str)>) -> Log {
        Log {
            event_id: None,
            when: 0,
            message: message.to_string(),
            workflow_id: "wf-1".to_string(),
            activity_id: activity.map(|(id, _)| id.to_string()),
            activity_run_id: activity.map(|(_, run)| run.to_string()),
            metadata: None,
            level: 0,
        }
    }

    fn heartbeat(activity_id: &str, details: Option<Vec<u8>>) -> ActivityHeartbeatV1 {
        ActivityHeartbeatV1 {
            activity_id: activity_id.to_string(),
            activity_run_id: format!("run-{activity_id}"),
            workflow_id: "wf-1".to_string(),
            workflow_epoch: 1,
            details: details.map(|data| immortal_lib::common::Payload {
                data,
                metadata: HashMap::new(),
            }),
        }
    }

    fn action_kind(action: ImmortalServerActionV1) -> &'static str {
        match action.action {
            Some(immortal_server_action_v1::Action::ActivityHeartbeatV1(_)) => "heartbeat",
            Some(immortal_server_action_v1::Action::LogEvent(_)) => "log",
            Some(immortal_server_action_v1::Action::Metrics(_)) => "metrics",
            _ => "control",
        }
    }

    #[test]
    fn priority_lanes_precede_general_logs_and_metrics() {
        let (tx, rx) = outbound_channel(limits());
        assert!(tx.start_activity("act-1", "run-act-1", "wf-1", 1));
        tx.send_metrics(Metrics {
            cput_pct: 1.0,
            mem_used: 2,
            mem_total: 3,
        });
        tx.send_log(log("general", None));
        tx.send_log(log("fills-general", None));
        tx.send_log(log("activity-fallback", Some(("act-1", "run-act-1"))));
        tx.send_heartbeat(heartbeat("act-1", None));

        assert_eq!(action_kind(rx.try_recv().unwrap()), "heartbeat");
        assert_eq!(action_kind(rx.try_recv().unwrap()), "log");
        assert_eq!(action_kind(rx.try_recv().unwrap()), "log");
        assert_eq!(action_kind(rx.try_recv().unwrap()), "log");
        assert_eq!(action_kind(rx.try_recv().unwrap()), "metrics");
    }

    #[test]
    fn heartbeat_and_metrics_lanes_keep_only_latest_values() {
        let (tx, rx) = outbound_channel(limits());
        assert!(tx.start_activity("act-1", "run-act-1", "wf-1", 1));
        tx.send_heartbeat(heartbeat("act-1", Some(vec![1])));
        tx.send_heartbeat(heartbeat("act-1", Some(vec![2])));
        tx.send_metrics(Metrics {
            cput_pct: 1.0,
            mem_used: 1,
            mem_total: 10,
        });
        tx.send_metrics(Metrics {
            cput_pct: 2.0,
            mem_used: 2,
            mem_total: 20,
        });

        match rx.try_recv().unwrap().action {
            Some(immortal_server_action_v1::Action::ActivityHeartbeatV1(heartbeat)) => {
                assert_eq!(heartbeat.details.unwrap().data, vec![2]);
            }
            other => panic!("expected heartbeat, got {other:?}"),
        }
        match rx.try_recv().unwrap().action {
            Some(immortal_server_action_v1::Action::Metrics(metrics)) => {
                assert_eq!(metrics.mem_used, 2);
            }
            other => panic!("expected metrics, got {other:?}"),
        }
        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn full_general_queue_keeps_latest_activity_log_in_bounded_fallback() {
        let (tx, rx) = outbound_channel(limits());
        assert!(tx.start_activity("act-1", "run-1", "wf-1", 1));
        tx.send_log(log("general-1", None));
        tx.send_log(log("general-2", None));
        tx.send_log(log("old", Some(("act-1", "run-1"))));
        tx.send_log(log("new", Some(("act-1", "run-1"))));

        match rx.try_recv().unwrap().action {
            Some(immortal_server_action_v1::Action::ActivityHeartbeatV1(heartbeat)) => {
                assert_eq!(heartbeat.activity_run_id, "run-1");
                assert_eq!(heartbeat.workflow_epoch, 1);
            }
            other => panic!("expected log-derived heartbeat, got {other:?}"),
        }
        match rx.try_recv().unwrap().action {
            Some(immortal_server_action_v1::Action::LogEvent(log)) => {
                assert_eq!(log.message, "new");
                assert_eq!(log.activity_run_id.as_deref(), Some("run-1"));
            }
            other => panic!("expected fallback log, got {other:?}"),
        }
        assert_eq!(tx.stats().activity_fallback_replacements, 1);
        assert_eq!(tx.stats().dropped_logs, 2);
    }

    #[test]
    fn oversized_activity_log_is_truncated_but_keeps_run_identity() {
        let mut small = limits();
        small.max_record_bytes = 80;
        let (tx, rx) = outbound_channel(small);
        assert!(tx.start_activity("act-1", "run-1", "wf-1", 1));
        tx.send_log(log(&"x".repeat(1_000), Some(("act-1", "run-1"))));

        match rx.try_recv().unwrap().action {
            Some(immortal_server_action_v1::Action::ActivityHeartbeatV1(heartbeat)) => {
                assert_eq!(heartbeat.activity_run_id, "run-1");
            }
            other => panic!("expected log-derived heartbeat, got {other:?}"),
        }
        match rx.try_recv().unwrap().action {
            Some(immortal_server_action_v1::Action::LogEvent(log)) => {
                assert!(log.encoded_len() <= small.max_record_bytes);
                assert_eq!(log.activity_id.as_deref(), Some("act-1"));
                assert_eq!(log.activity_run_id.as_deref(), Some("run-1"));
                assert!(log.message.len() < 1_000);
            }
            other => panic!("expected fallback log, got {other:?}"),
        }
        assert_eq!(tx.stats().truncated_activity_logs, 1);
    }

    #[test]
    fn byte_limit_triggers_fallback_before_record_limit() {
        let first = log("first", None);
        let mut byte_limited = limits();
        byte_limited.log_records = 10;
        byte_limited.log_bytes = first.encoded_len() + 1;
        let (tx, _rx) = outbound_channel(byte_limited);
        assert!(tx.start_activity("act-1", "run-1", "wf-1", 1));

        tx.send_log(first);
        tx.send_log(log("second", Some(("act-1", "run-1"))));

        let stats = tx.stats();
        assert_eq!(stats.log_records, 1);
        assert!(stats.log_bytes <= byte_limited.log_bytes);
        assert_eq!(stats.activity_fallback_records, 1);
        assert!(stats.activity_fallback_bytes <= byte_limited.activity_fallback_bytes);
    }

    #[test]
    fn activity_fallback_only_accepts_the_bounded_active_set() {
        let mut one_activity = limits();
        one_activity.log_records = 1;
        one_activity.activity_records = 1;
        let (tx, _rx) = outbound_channel(one_activity);
        assert!(tx.start_activity("act-1", "run-1", "wf-1", 1));
        assert!(!tx.start_activity("act-2", "run-2", "wf-1", 1));
        tx.send_log(log("general", None));
        tx.send_log(log("one", Some(("act-1", "run-1"))));
        tx.send_log(log("two", Some(("act-2", "run-2"))));

        let stats = tx.stats();
        assert_eq!(stats.activity_fallback_records, 1);
        assert_eq!(stats.dropped_activity_fallbacks, 0);
    }

    #[test]
    fn sustained_overload_cannot_grow_lane_state_past_limits() {
        let bounded = limits();
        let (tx, _rx) = outbound_channel(bounded);

        for index in 0..bounded.activity_records {
            let activity_id = format!("act-{index}");
            assert!(tx.start_activity(&activity_id, &format!("run-{activity_id}"), "wf-1", 1,));
        }

        for index in 0..10_000 {
            let activity_id = format!("act-{}", index % bounded.activity_records);
            let run_id = format!("run-{activity_id}");
            tx.send_log(log(
                &format!("record-{index}-{}", "x".repeat(100)),
                Some((&activity_id, &run_id)),
            ));
        }

        let stats = tx.stats();
        assert!(stats.log_records <= bounded.log_records);
        assert!(stats.log_bytes <= bounded.log_bytes);
        assert!(stats.activity_fallback_records <= bounded.activity_records);
        assert!(stats.activity_fallback_bytes <= bounded.activity_fallback_bytes);
    }

    #[test]
    fn oversized_heartbeat_drops_details_without_dropping_liveness() {
        let mut small = limits();
        small.max_record_bytes = 80;
        let (tx, rx) = outbound_channel(small);
        assert!(tx.start_activity("act-1", "run-act-1", "wf-1", 1));
        tx.send_heartbeat(heartbeat("act-1", Some(vec![7; 1_000])));

        match rx.try_recv().unwrap().action {
            Some(immortal_server_action_v1::Action::ActivityHeartbeatV1(heartbeat)) => {
                assert!(heartbeat.details.is_none());
                assert_eq!(heartbeat.activity_run_id, "run-act-1");
            }
            other => panic!("expected heartbeat, got {other:?}"),
        }
        assert_eq!(tx.stats().stripped_heartbeat_details, 1);
        assert_eq!(tx.stats().dropped_heartbeats, 0);
    }

    #[test]
    fn finishing_activity_removes_pending_priority_state() {
        let (tx, rx) = outbound_channel(limits());
        assert!(tx.start_activity("act-1", "run-1", "wf-1", 1));
        tx.send_heartbeat(ActivityHeartbeatV1 {
            activity_id: "act-1".to_string(),
            activity_run_id: "run-1".to_string(),
            workflow_id: "wf-1".to_string(),
            workflow_epoch: 1,
            details: None,
        });
        tx.send_log(log("general-1", None));
        tx.send_log(log("general-2", None));
        tx.send_log(log("fallback", Some(("act-1", "run-1"))));

        tx.finish_activity("act-1", "run-1");

        assert_eq!(action_kind(rx.try_recv().unwrap()), "log");
        assert_eq!(action_kind(rx.try_recv().unwrap()), "log");
        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn detached_or_superseded_run_cannot_reoccupy_priority_lanes() {
        let mut one_log = limits();
        one_log.log_records = 1;
        let (tx, rx) = outbound_channel(one_log);
        assert!(tx.start_activity("act-1", "run-1", "wf-1", 1));
        tx.send_log(log("general", None));

        tx.finish_activity("act-1", "run-1");
        tx.send_log(log("detached", Some(("act-1", "run-1"))));
        tx.send_heartbeat(heartbeat("act-1", None));
        assert_eq!(tx.stats().activity_fallback_records, 0);
        assert_eq!(tx.stats().heartbeat_slots, 0);

        assert!(tx.start_activity("act-1", "run-2", "wf-1", 1));
        tx.finish_activity("act-1", "run-1");
        tx.send_heartbeat(ActivityHeartbeatV1 {
            activity_id: "act-1".to_string(),
            activity_run_id: "run-2".to_string(),
            workflow_id: "wf-1".to_string(),
            workflow_epoch: 2,
            details: None,
        });

        assert_eq!(action_kind(rx.try_recv().unwrap()), "heartbeat");
        assert_eq!(action_kind(rx.try_recv().unwrap()), "log");
        assert!(rx.try_recv().is_none());
    }
}
