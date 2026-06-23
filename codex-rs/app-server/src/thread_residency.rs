use codex_protocol::ThreadId;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex;

const DEFAULT_SOFT_CAP_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const DEFAULT_HARD_CAP_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const DEFAULT_IDLE_MIN_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const DEFAULT_EVICTION_POLL: Duration = Duration::from_secs(60);

const SOFT_CAP_ENV: &str = "CODEX_APP_SERVER_RESIDENCY_SOFT_BYTES";
const HARD_CAP_ENV: &str = "CODEX_APP_SERVER_RESIDENCY_HARD_BYTES";
const IDLE_MIN_TTL_ENV: &str = "CODEX_APP_SERVER_RESIDENCY_IDLE_MIN_TTL_SECS";
const EVICTION_POLL_ENV: &str = "CODEX_APP_SERVER_RESIDENCY_EVICTION_POLL_SECS";

#[derive(Clone)]
pub(crate) struct ThreadResidencyManager {
    state: Arc<Mutex<ThreadResidencyState>>,
    policy: ThreadResidencyPolicy,
}

#[derive(Clone)]
struct ThreadResidencyPolicy {
    soft_cap_bytes: u64,
    hard_cap_bytes: u64,
    idle_min_ttl: Duration,
    eviction_poll: Duration,
}

#[derive(Default)]
struct ThreadResidencyState {
    records: HashMap<ThreadId, ThreadResidencyRecord>,
}

#[derive(Clone)]
struct ThreadResidencyRecord {
    loaded_at: Instant,
    last_accessed_at: Instant,
    estimated_bytes: Option<u64>,
    resume_source: Option<String>,
    resume_duration: Option<Duration>,
    has_subscribers: bool,
    is_active: bool,
}

pub(crate) struct ThreadResidencyDecision {
    pub(crate) should_unload: bool,
    pub(crate) reason: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ThreadResidencySnapshot {
    pub(crate) thread_id: String,
    pub(crate) loaded_for_secs: u64,
    pub(crate) idle_for_secs: u64,
    pub(crate) estimated_bytes: Option<u64>,
    pub(crate) resume_source: Option<String>,
    pub(crate) resume_duration_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct ThreadResidencyDiagnostics {
    pub(crate) soft_cap_bytes: u64,
    pub(crate) hard_cap_bytes: u64,
    pub(crate) idle_min_ttl_secs: u64,
    pub(crate) eviction_poll_secs: u64,
    pub(crate) rss_bytes: Option<u64>,
    pub(crate) loaded_threads: usize,
    pub(crate) resident_estimated_bytes: u64,
    pub(crate) records: Vec<ThreadResidencySnapshot>,
}

impl Default for ThreadResidencyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadResidencyManager {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ThreadResidencyState::default())),
            policy: ThreadResidencyPolicy::from_env(),
        }
    }

    #[cfg(test)]
    fn with_policy(soft_cap_bytes: u64, hard_cap_bytes: u64, idle_min_ttl: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(ThreadResidencyState::default())),
            policy: ThreadResidencyPolicy {
                soft_cap_bytes,
                hard_cap_bytes,
                idle_min_ttl,
                eviction_poll: DEFAULT_EVICTION_POLL,
            },
        }
    }

    pub(crate) fn eviction_poll(&self) -> Duration {
        self.policy.eviction_poll
    }

    pub(crate) async fn note_loaded(&self, thread_id: ThreadId, rollout_path: Option<&Path>) {
        let estimated_bytes = rollout_path.and_then(file_size);
        let mut state = self.state.lock().await;
        let now = Instant::now();
        state
            .records
            .entry(thread_id)
            .or_insert(ThreadResidencyRecord {
                loaded_at: now,
                last_accessed_at: now,
                estimated_bytes,
                resume_source: None,
                resume_duration: None,
                has_subscribers: false,
                is_active: false,
            });
        if let Some(record) = state.records.get_mut(&thread_id) {
            record.last_accessed_at = now;
            if estimated_bytes.is_some() {
                record.estimated_bytes = estimated_bytes;
            }
        }
    }

    pub(crate) async fn note_accessed(&self, thread_id: ThreadId) {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        state
            .records
            .entry(thread_id)
            .and_modify(|record| record.last_accessed_at = now)
            .or_insert(ThreadResidencyRecord {
                loaded_at: now,
                last_accessed_at: now,
                estimated_bytes: None,
                resume_source: None,
                resume_duration: None,
                has_subscribers: false,
                is_active: false,
            });
    }

    pub(crate) async fn note_resume(
        &self,
        thread_id: ThreadId,
        rollout_path: Option<&Path>,
        resume_source: &'static str,
        duration: Duration,
    ) {
        self.note_loaded(thread_id, rollout_path).await;
        let mut state = self.state.lock().await;
        if let Some(record) = state.records.get_mut(&thread_id) {
            record.resume_source = Some(resume_source.to_string());
            record.resume_duration = Some(duration);
        }
    }

    pub(crate) async fn note_observed(
        &self,
        thread_id: ThreadId,
        has_subscribers: bool,
        is_active: bool,
    ) {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let record = state
            .records
            .entry(thread_id)
            .or_insert(ThreadResidencyRecord {
                loaded_at: now,
                last_accessed_at: now,
                estimated_bytes: None,
                resume_source: None,
                resume_duration: None,
                has_subscribers,
                is_active,
            });
        record.has_subscribers = has_subscribers;
        record.is_active = is_active;
    }

    pub(crate) async fn note_removed(&self, thread_id: ThreadId) {
        self.state.lock().await.records.remove(&thread_id);
    }

    pub(crate) async fn unload_decision(
        &self,
        thread_id: ThreadId,
        has_subscribers: bool,
        is_active: bool,
    ) -> ThreadResidencyDecision {
        if has_subscribers {
            return keep("thread has subscribers");
        }
        if is_active {
            return keep("thread is active");
        }

        let rss_bytes = current_rss_bytes();
        let Some(rss_bytes) = rss_bytes else {
            return keep("process RSS is unavailable");
        };
        if rss_bytes <= self.policy.soft_cap_bytes {
            return keep("process RSS is below the residency soft cap");
        }

        self.note_observed(thread_id, has_subscribers, is_active)
            .await;
        let idle_cutoff = if rss_bytes > self.policy.hard_cap_bytes {
            Duration::ZERO
        } else {
            self.policy.idle_min_ttl
        };
        let mut state = self.state.lock().await;
        let Some(record) = state.records.get_mut(&thread_id) else {
            return keep("thread has no residency record yet");
        };
        record.has_subscribers = has_subscribers;
        record.is_active = is_active;
        let idle_for = record.last_accessed_at.elapsed();
        if rss_bytes <= self.policy.hard_cap_bytes && idle_for < self.policy.idle_min_ttl {
            return keep("process RSS is above the soft cap but thread is inside minimum idle TTL");
        }
        let Some(lru_thread_id) = oldest_unprotected_idle_thread(&state.records, idle_cutoff)
        else {
            return keep("no idle unsubscribed resident is eligible for eviction");
        };
        if lru_thread_id != thread_id {
            return keep("a less-recently-used idle resident should be evicted first");
        }

        ThreadResidencyDecision {
            should_unload: true,
            reason: if rss_bytes > self.policy.hard_cap_bytes {
                "process RSS is above the residency hard cap".to_string()
            } else {
                "process RSS is above the residency soft cap and thread is idle past minimum TTL"
                    .to_string()
            },
        }
    }

    pub(crate) async fn diagnostics(&self) -> ThreadResidencyDiagnostics {
        let state = self.state.lock().await;
        let now = Instant::now();
        let records = state
            .records
            .iter()
            .map(|(thread_id, record)| ThreadResidencySnapshot {
                thread_id: thread_id.to_string(),
                loaded_for_secs: duration_secs(now.saturating_duration_since(record.loaded_at)),
                idle_for_secs: duration_secs(
                    now.saturating_duration_since(record.last_accessed_at),
                ),
                estimated_bytes: record.estimated_bytes,
                resume_source: record.resume_source.clone(),
                resume_duration_ms: record.resume_duration.map(duration_millis),
            })
            .collect::<Vec<_>>();
        let resident_estimated_bytes = records
            .iter()
            .filter_map(|record| record.estimated_bytes)
            .sum();
        ThreadResidencyDiagnostics {
            soft_cap_bytes: self.policy.soft_cap_bytes,
            hard_cap_bytes: self.policy.hard_cap_bytes,
            idle_min_ttl_secs: duration_secs(self.policy.idle_min_ttl),
            eviction_poll_secs: duration_secs(self.policy.eviction_poll),
            rss_bytes: current_rss_bytes(),
            loaded_threads: records.len(),
            resident_estimated_bytes,
            records,
        }
    }
}

fn oldest_unprotected_idle_thread(
    records: &HashMap<ThreadId, ThreadResidencyRecord>,
    idle_cutoff: Duration,
) -> Option<ThreadId> {
    records
        .iter()
        .filter(|(_, record)| {
            !record.has_subscribers
                && !record.is_active
                && record.last_accessed_at.elapsed() >= idle_cutoff
        })
        .min_by_key(|(_, record)| record.last_accessed_at)
        .map(|(thread_id, _)| *thread_id)
}

impl ThreadResidencyPolicy {
    fn from_env() -> Self {
        let soft_cap_bytes = read_u64_env(SOFT_CAP_ENV).unwrap_or(DEFAULT_SOFT_CAP_BYTES);
        let hard_cap_bytes =
            read_u64_env(HARD_CAP_ENV).unwrap_or(DEFAULT_HARD_CAP_BYTES.max(soft_cap_bytes));
        let idle_min_ttl = read_duration_env(IDLE_MIN_TTL_ENV).unwrap_or(DEFAULT_IDLE_MIN_TTL);
        let eviction_poll = read_duration_env(EVICTION_POLL_ENV).unwrap_or(DEFAULT_EVICTION_POLL);
        Self {
            soft_cap_bytes,
            hard_cap_bytes: hard_cap_bytes.max(soft_cap_bytes),
            idle_min_ttl,
            eviction_poll,
        }
    }
}

fn keep(reason: &str) -> ThreadResidencyDecision {
    ThreadResidencyDecision {
        should_unload: false,
        reason: reason.to_string(),
    }
}

fn file_size(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|metadata| metadata.len())
}

fn read_u64_env(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse::<u64>().ok()
}

fn read_duration_env(name: &str) -> Option<Duration> {
    read_u64_env(name).map(Duration::from_secs)
}

fn current_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string(PathBuf::from("/proc/self/status")).ok()?;
    let rss_line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let rss_kib = rss_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())?;
    rss_kib.checked_mul(1024)
}

fn duration_secs(duration: Duration) -> u64 {
    duration.as_secs()
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn diagnostics_reports_residency_records() {
        let manager = ThreadResidencyManager::new();
        let thread_id = ThreadId::new();

        manager.note_loaded(thread_id, None).await;
        manager
            .note_resume(thread_id, None, "rollout", Duration::from_millis(12))
            .await;

        let diagnostics = manager.diagnostics().await;

        assert_eq!(diagnostics.loaded_threads, 1);
        assert_eq!(diagnostics.records[0].thread_id, thread_id.to_string());
        assert_eq!(
            diagnostics.records[0].resume_source.as_deref(),
            Some("rollout")
        );
        assert_eq!(diagnostics.records[0].resume_duration_ms, Some(12));
    }

    #[tokio::test]
    async fn unload_decision_protects_active_and_subscribed_threads() {
        let manager = ThreadResidencyManager::with_policy(0, 0, Duration::ZERO);
        let thread_id = ThreadId::new();

        assert!(
            !manager
                .unload_decision(thread_id, true, false)
                .await
                .should_unload
        );
        assert!(
            !manager
                .unload_decision(thread_id, false, true)
                .await
                .should_unload
        );
    }

    #[tokio::test]
    async fn unload_decision_keeps_idle_threads_when_under_soft_cap() {
        let manager = ThreadResidencyManager::with_policy(u64::MAX, u64::MAX, Duration::ZERO);
        let thread_id = ThreadId::new();

        manager.note_loaded(thread_id, None).await;

        let decision = manager.unload_decision(thread_id, false, false).await;

        assert!(!decision.should_unload, "{}", decision.reason);
    }

    #[tokio::test]
    async fn unload_decision_evicts_lru_idle_thread_under_pressure() {
        let manager = ThreadResidencyManager::with_policy(0, u64::MAX, Duration::ZERO);
        let older_thread_id = ThreadId::new();
        let newer_thread_id = ThreadId::new();

        manager.note_loaded(older_thread_id, None).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        manager.note_loaded(newer_thread_id, None).await;

        let newer_decision = manager.unload_decision(newer_thread_id, false, false).await;
        let older_decision = manager.unload_decision(older_thread_id, false, false).await;

        assert!(!newer_decision.should_unload, "{}", newer_decision.reason);
        assert!(older_decision.should_unload, "{}", older_decision.reason);
    }
}
