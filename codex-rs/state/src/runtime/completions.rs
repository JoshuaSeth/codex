use super::*;
use sqlx::SqliteConnection;
use sqlx::sqlite::SqliteRow;
use std::borrow::Cow;
use std::collections::HashSet;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

const MAX_COMPLETION_FINAL_TEXT_CHARS: usize = 1_048_576;
const COMPLETION_FINAL_TEXT_TRUNCATION_NOTICE: &str =
    "\n\n[PitchAI callback final output truncated at the durable transport limit.]";
const TERMINAL_GOAL_FINAL_CAPTURE_GRACE_MS: i64 = 60_000;

#[derive(Clone)]
pub struct CompletionStore {
    pool: Arc<SqlitePool>,
    notify: Arc<Notify>,
    turn_admission_lock: Arc<Mutex<()>>,
    goal_admission_lock: Arc<Mutex<()>>,
    submitted_turns: Arc<Mutex<HashSet<String>>>,
    tracked_turns: Arc<Mutex<HashSet<(String, String)>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionOutboxEvent {
    pub event_id: String,
    pub completion_work_id: String,
    pub thread_id: String,
    pub execution_kind: String,
    pub execution_id: String,
    pub terminal_turn_id: Option<String>,
    pub callback_metadata_json: String,
    pub terminal_status: String,
    pub final_text: String,
    pub terminal_at_ms: i64,
    pub attempt: i64,
    pub lease_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionOutboxStats {
    pub pending_count: i64,
    pub sending_count: i64,
    pub sent_count: i64,
    pub total_attempts: i64,
    pub oldest_unsent_at_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionBindingState {
    Registered,
    Active,
    Terminal,
}

struct ExecutionBinding<'a> {
    completion_work_id: &'a str,
    thread_id: ThreadId,
    execution_kind: &'static str,
    execution_id: &'a str,
    callback_metadata_json: &'a str,
    initial_state: &'static str,
    now_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionCallback {
    pub delivery_id: String,
    pub event_id: String,
    pub completion_work_id: String,
    pub target_thread_id: String,
    pub source_agent_display_id: String,
    pub execution_kind: String,
    pub execution_id: String,
    pub terminal_status: String,
    pub callback_text: String,
    pub final_text: String,
    pub terminal_at_ms: i64,
    pub payload_digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionCallbackState {
    Pending,
    Injected,
    Delivered,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionCallbackRecord {
    pub callback: CompletionCallback,
    pub call_id: String,
    pub state: CompletionCallbackState,
    pub injected_boot_id: Option<String>,
    pub attempt_count: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionCallbackAcceptance {
    pub inserted: bool,
    pub record: CompletionCallbackRecord,
}

impl CompletionStore {
    pub(crate) async fn new(pool: Arc<SqlitePool>) -> anyhow::Result<Self> {
        let tracked_turns = outstanding_normal_turns(pool.as_ref()).await?;
        Ok(Self {
            pool,
            notify: Arc::new(Notify::new()),
            turn_admission_lock: Arc::new(Mutex::new(())),
            goal_admission_lock: Arc::new(Mutex::new(())),
            submitted_turns: Arc::new(Mutex::new(HashSet::new())),
            tracked_turns: Arc::new(Mutex::new(tracked_turns)),
        })
    }

    pub fn wakeup(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    pub(crate) fn notify_sender(&self) {
        self.notify.notify_waiters();
    }

    pub async fn lock_turn_admission(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.turn_admission_lock).lock_owned().await
    }

    pub async fn lock_goal_admission(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.goal_admission_lock).lock_owned().await
    }

    pub async fn turn_was_submitted_in_process(&self, completion_work_id: &str) -> bool {
        self.submitted_turns
            .lock()
            .await
            .contains(completion_work_id)
    }

    pub async fn note_turn_submitted(
        &self,
        completion_work_id: &str,
        thread_id: ThreadId,
        turn_id: &str,
    ) {
        self.submitted_turns
            .lock()
            .await
            .insert(completion_work_id.to_string());
        self.tracked_turns
            .lock()
            .await
            .insert((thread_id.to_string(), turn_id.to_string()));
    }

    pub async fn turn_is_tracked_in_process(&self, thread_id: ThreadId, turn_id: &str) -> bool {
        self.tracked_turns
            .lock()
            .await
            .contains(&(thread_id.to_string(), turn_id.to_string()))
    }

    pub async fn bind_turn(
        &self,
        completion_work_id: &str,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> anyhow::Result<CompletionBindingState> {
        self.bind_turn_with_callback_metadata(completion_work_id, thread_id, turn_id, "")
            .await
    }

    pub async fn bind_turn_with_callback_metadata(
        &self,
        completion_work_id: &str,
        thread_id: ThreadId,
        turn_id: &str,
        callback_metadata_json: &str,
    ) -> anyhow::Result<CompletionBindingState> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let mut transaction = self.pool.begin().await?;
        let state = bind_execution(
            &mut transaction,
            ExecutionBinding {
                completion_work_id,
                thread_id,
                execution_kind: "normal",
                execution_id: turn_id,
                callback_metadata_json,
                initial_state: "registered",
                now_ms,
            },
        )
        .await?;
        transaction.commit().await?;
        completion_binding_state(&state)
    }

    pub async fn existing_turn_binding(
        &self,
        completion_work_id: &str,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<(String, CompletionBindingState)>> {
        self.existing_turn_binding_with_callback_metadata(completion_work_id, thread_id, "")
            .await
    }

    pub async fn existing_turn_binding_with_callback_metadata(
        &self,
        completion_work_id: &str,
        thread_id: ThreadId,
        callback_metadata_json: &str,
    ) -> anyhow::Result<Option<(String, CompletionBindingState)>> {
        let row = sqlx::query(
            r#"
SELECT thread_id, execution_kind, execution_id, callback_metadata_json, state
FROM completion_bindings
WHERE completion_work_id = ?
            "#,
        )
        .bind(completion_work_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let stored_thread_id: String = row.try_get("thread_id")?;
        let execution_kind: String = row.try_get("execution_kind")?;
        let stored_callback_metadata_json: String = row.try_get("callback_metadata_json")?;
        anyhow::ensure!(
            stored_thread_id == thread_id.to_string()
                && execution_kind == "normal"
                && stored_callback_metadata_json == callback_metadata_json,
            "completion work id is already bound to different execution intent"
        );
        let execution_id = row.try_get("execution_id")?;
        let state: String = row.try_get("state")?;
        Ok(Some((execution_id, completion_binding_state(&state)?)))
    }

    pub async fn mark_turn_submitted(&self, completion_work_id: &str) -> anyhow::Result<()> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let result = sqlx::query(
            r#"
UPDATE completion_bindings
SET state = 'active', updated_at_ms = ?
WHERE completion_work_id = ? AND execution_kind = 'normal' AND state = 'registered'
            "#,
        )
        .bind(now_ms)
        .bind(completion_work_id)
        .execute(self.pool.as_ref())
        .await?;
        if result.rows_affected() == 0 {
            let state: Option<String> = sqlx::query_scalar(
                r#"
SELECT state
FROM completion_bindings
WHERE completion_work_id = ? AND execution_kind = 'normal'
                "#,
            )
            .bind(completion_work_id)
            .fetch_optional(self.pool.as_ref())
            .await?;
            anyhow::ensure!(
                matches!(state.as_deref(), Some("active" | "terminal")),
                "turn completion binding disappeared before submission was recorded"
            );
        }
        Ok(())
    }

    pub async fn release_registered_turn_binding(
        &self,
        completion_work_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"
DELETE FROM completion_bindings
WHERE completion_work_id = ? AND execution_kind = 'normal' AND state = 'registered'
            "#,
        )
        .bind(completion_work_id)
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    pub async fn complete_turn(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
        final_text: &str,
        terminal_at_ms: i64,
    ) -> anyhow::Result<u64> {
        let final_text = bounded_completion_final_text(final_text);
        let mut transaction = self.pool.begin().await?;
        let terminal_work_ids: HashSet<String> = sqlx::query_scalar(
            r#"
SELECT completion_work_id
FROM completion_bindings
WHERE thread_id = ?
  AND execution_kind = 'normal'
  AND execution_id = ?
  AND state IN ('registered', 'active')
            "#,
        )
        .bind(thread_id.to_string())
        .bind(turn_id)
        .fetch_all(&mut *transaction)
        .await?
        .into_iter()
        .collect();
        let inserted = sqlx::query(
            r#"
INSERT OR IGNORE INTO completion_outbox (
    event_id,
    completion_work_id,
    thread_id,
    execution_kind,
    execution_id,
    terminal_turn_id,
    callback_metadata_json,
    terminal_status,
    final_text,
    terminal_at_ms,
    state,
    available_at_ms,
    created_at_ms,
    updated_at_ms
)
SELECT
    binding.completion_work_id,
    binding.completion_work_id,
    binding.thread_id,
    binding.execution_kind,
    binding.execution_id,
    binding.execution_id,
    binding.callback_metadata_json,
    'completed',
    ?,
    ?,
    'pending',
    ?,
    ?,
    ?
FROM completion_bindings AS binding
WHERE binding.thread_id = ?
  AND binding.execution_kind = 'normal'
  AND binding.execution_id = ?
  AND binding.state IN ('registered', 'active')
            "#,
        )
        .bind(final_text.as_ref())
        .bind(terminal_at_ms)
        .bind(terminal_at_ms)
        .bind(terminal_at_ms)
        .bind(terminal_at_ms)
        .bind(thread_id.to_string())
        .bind(turn_id)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        sqlx::query(
            r#"
UPDATE completion_bindings
SET state = 'terminal', updated_at_ms = ?
WHERE thread_id = ?
  AND execution_kind = 'normal'
  AND execution_id = ?
  AND state IN ('registered', 'active')
            "#,
        )
        .bind(terminal_at_ms)
        .bind(thread_id.to_string())
        .bind(turn_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        self.submitted_turns
            .lock()
            .await
            .retain(|completion_work_id| !terminal_work_ids.contains(completion_work_id));
        self.tracked_turns
            .lock()
            .await
            .remove(&(thread_id.to_string(), turn_id.to_string()));
        if inserted > 0 {
            self.notify_sender();
        }
        Ok(inserted)
    }

    pub async fn associate_terminal_goal_turn(
        &self,
        thread_id: ThreadId,
        terminal_status: &str,
        terminal_updated_at_seconds: i64,
        turn_id: &str,
    ) -> anyhow::Result<u64> {
        anyhow::ensure!(
            matches!(
                terminal_status,
                "complete" | "blocked" | "usageLimited" | "budgetLimited"
            ),
            "unsupported terminal goal completion status"
        );
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let terminal_second_start_ms = terminal_updated_at_seconds.saturating_mul(1_000);
        let terminal_second_end_ms = terminal_second_start_ms.saturating_add(1_000);
        let mut transaction = self.pool.begin().await?;
        let associated = sqlx::query(
            r#"
UPDATE completion_outbox
SET terminal_turn_id = ?, updated_at_ms = ?
WHERE event_id = (
    SELECT event_id
    FROM completion_outbox
    WHERE thread_id = ?
      AND execution_kind = 'goal'
      AND state = 'pending'
      AND terminal_turn_id IS NULL
      AND terminal_status = ?
      AND terminal_at_ms >= ?
      AND terminal_at_ms < ?
      AND NOT EXISTS (
          SELECT 1
          FROM completion_outbox AS already_associated
          WHERE already_associated.thread_id = ?
            AND already_associated.execution_kind = 'goal'
            AND already_associated.terminal_turn_id = ?
      )
    ORDER BY terminal_at_ms, created_at_ms, event_id
    LIMIT 1
)
            "#,
        )
        .bind(turn_id)
        .bind(now_ms)
        .bind(thread_id.to_string())
        .bind(terminal_status)
        .bind(terminal_second_start_ms)
        .bind(terminal_second_end_ms)
        .bind(thread_id.to_string())
        .bind(turn_id)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        sqlx::query(
            r#"
UPDATE completion_webhook_outbox
SET terminal_turn_id = ?, updated_at_ms = ?
WHERE event_id IN (
    SELECT event_id
    FROM completion_outbox
    WHERE thread_id = ?
      AND execution_kind = 'goal'
      AND terminal_turn_id = ?
)
            "#,
        )
        .bind(turn_id)
        .bind(now_ms)
        .bind(thread_id.to_string())
        .bind(turn_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(associated)
    }

    pub async fn complete_terminal_goal_turn(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
        final_text: &str,
        completed_at_ms: i64,
    ) -> anyhow::Result<u64> {
        if final_text.trim().is_empty() {
            return Ok(0);
        }
        let final_text = bounded_completion_final_text(final_text);
        let mut transaction = self.pool.begin().await?;
        let completed = sqlx::query(
            r#"
UPDATE completion_outbox
SET final_text = ?, available_at_ms = ?, updated_at_ms = ?
WHERE thread_id = ?
  AND execution_kind = 'goal'
  AND state = 'pending'
  AND terminal_turn_id = ?
  AND final_text = ''
            "#,
        )
        .bind(final_text.as_ref())
        .bind(completed_at_ms)
        .bind(completed_at_ms)
        .bind(thread_id.to_string())
        .bind(turn_id)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        sqlx::query(
            r#"
UPDATE completion_webhook_outbox
SET final_text = ?, available_at_ms = ?, updated_at_ms = ?
WHERE thread_id = ?
  AND execution_kind = 'goal'
  AND state = 'pending'
  AND terminal_turn_id = ?
  AND final_text = ''
            "#,
        )
        .bind(final_text.as_ref())
        .bind(completed_at_ms)
        .bind(completed_at_ms)
        .bind(thread_id.to_string())
        .bind(turn_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        if completed > 0 {
            self.notify_sender();
        }
        Ok(completed)
    }

    pub async fn claim_outbox(
        &self,
        limit: i64,
        lease_duration_ms: i64,
    ) -> anyhow::Result<Vec<CompletionOutboxEvent>> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let lease_expires_at_ms = now_ms.saturating_add(lease_duration_ms);
        let mut transaction = self.pool.begin().await?;
        let rows = sqlx::query(
            r#"
SELECT event_id
FROM completion_outbox
WHERE available_at_ms <= ?
  AND (
      state = 'pending'
      OR (state = 'sending' AND lease_expires_at_ms <= ?)
  )
ORDER BY created_at_ms, event_id
LIMIT ?
            "#,
        )
        .bind(now_ms)
        .bind(now_ms)
        .bind(limit)
        .fetch_all(&mut *transaction)
        .await?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let event_id: String = row.try_get("event_id")?;
            let lease_id = Uuid::new_v4().to_string();
            let claimed = sqlx::query(
                r#"
UPDATE completion_outbox
SET
    state = 'sending',
    attempt_count = attempt_count + 1,
    lease_id = ?,
    lease_expires_at_ms = ?,
    updated_at_ms = ?
WHERE event_id = ?
  AND available_at_ms <= ?
  AND (
      state = 'pending'
      OR (state = 'sending' AND lease_expires_at_ms <= ?)
  )
RETURNING
    event_id,
    completion_work_id,
    thread_id,
    execution_kind,
    execution_id,
    terminal_turn_id,
    callback_metadata_json,
    terminal_status,
    final_text,
    terminal_at_ms,
    attempt_count
                "#,
            )
            .bind(&lease_id)
            .bind(lease_expires_at_ms)
            .bind(now_ms)
            .bind(event_id)
            .bind(now_ms)
            .bind(now_ms)
            .fetch_optional(&mut *transaction)
            .await?;
            if let Some(row) = claimed {
                events.push(CompletionOutboxEvent {
                    event_id: row.try_get("event_id")?,
                    completion_work_id: row.try_get("completion_work_id")?,
                    thread_id: row.try_get("thread_id")?,
                    execution_kind: row.try_get("execution_kind")?,
                    execution_id: row.try_get("execution_id")?,
                    terminal_turn_id: row.try_get("terminal_turn_id")?,
                    callback_metadata_json: row.try_get("callback_metadata_json")?,
                    terminal_status: row.try_get("terminal_status")?,
                    final_text: row.try_get("final_text")?,
                    terminal_at_ms: row.try_get("terminal_at_ms")?,
                    attempt: row.try_get("attempt_count")?,
                    lease_id,
                });
            }
        }
        transaction.commit().await?;
        Ok(events)
    }

    pub async fn next_outbox_wakeup_delay_ms(&self, maximum_delay_ms: i64) -> anyhow::Result<i64> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let row = sqlx::query(
            r#"
SELECT MIN(
    CASE state
        WHEN 'pending' THEN available_at_ms
        WHEN 'sending' THEN lease_expires_at_ms
    END
) AS due_at_ms
FROM completion_outbox
WHERE state IN ('pending', 'sending')
            "#,
        )
        .fetch_one(self.pool.as_ref())
        .await?;
        let due_at_ms: Option<i64> = row.try_get("due_at_ms")?;
        let delay_ms = due_at_ms
            .map(|due_at_ms| due_at_ms.saturating_sub(now_ms).max(0))
            .unwrap_or(maximum_delay_ms);
        Ok(delay_ms.min(maximum_delay_ms).max(0))
    }

    pub async fn outbox_stats(&self) -> anyhow::Result<CompletionOutboxStats> {
        let row = sqlx::query(
            r#"
SELECT
    SUM(CASE WHEN state = 'pending' THEN 1 ELSE 0 END) AS pending_count,
    SUM(CASE WHEN state = 'sending' THEN 1 ELSE 0 END) AS sending_count,
    SUM(CASE WHEN state = 'sent' THEN 1 ELSE 0 END) AS sent_count,
    COALESCE(SUM(attempt_count), 0) AS total_attempts,
    MIN(CASE WHEN state IN ('pending', 'sending') THEN created_at_ms END) AS oldest_unsent_at_ms
FROM completion_outbox
            "#,
        )
        .fetch_one(self.pool.as_ref())
        .await?;
        Ok(CompletionOutboxStats {
            pending_count: row.try_get::<Option<i64>, _>("pending_count")?.unwrap_or(0),
            sending_count: row.try_get::<Option<i64>, _>("sending_count")?.unwrap_or(0),
            sent_count: row.try_get::<Option<i64>, _>("sent_count")?.unwrap_or(0),
            total_attempts: row.try_get("total_attempts")?,
            oldest_unsent_at_ms: row.try_get("oldest_unsent_at_ms")?,
        })
    }

    pub async fn claim_webhook_outbox(
        &self,
        limit: i64,
        lease_duration_ms: i64,
    ) -> anyhow::Result<Vec<CompletionOutboxEvent>> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let lease_expires_at_ms = now_ms.saturating_add(lease_duration_ms);
        let mut transaction = self.pool.begin().await?;
        let rows = sqlx::query(
            r#"
SELECT event_id
FROM completion_webhook_outbox
WHERE available_at_ms <= ?
  AND (
      state = 'pending'
      OR (state = 'sending' AND lease_expires_at_ms <= ?)
  )
ORDER BY created_at_ms, event_id
LIMIT ?
            "#,
        )
        .bind(now_ms)
        .bind(now_ms)
        .bind(limit)
        .fetch_all(&mut *transaction)
        .await?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let event_id: String = row.try_get("event_id")?;
            let lease_id = Uuid::new_v4().to_string();
            let claimed = sqlx::query(
                r#"
UPDATE completion_webhook_outbox
SET
    state = 'sending',
    attempt_count = attempt_count + 1,
    lease_id = ?,
    lease_expires_at_ms = ?,
    updated_at_ms = ?
WHERE event_id = ?
  AND available_at_ms <= ?
  AND (
      state = 'pending'
      OR (state = 'sending' AND lease_expires_at_ms <= ?)
  )
RETURNING
    event_id,
    completion_work_id,
    thread_id,
    execution_kind,
    execution_id,
    terminal_turn_id,
    callback_metadata_json,
    terminal_status,
    final_text,
    terminal_at_ms,
    attempt_count
                "#,
            )
            .bind(&lease_id)
            .bind(lease_expires_at_ms)
            .bind(now_ms)
            .bind(event_id)
            .bind(now_ms)
            .bind(now_ms)
            .fetch_optional(&mut *transaction)
            .await?;
            if let Some(row) = claimed {
                events.push(CompletionOutboxEvent {
                    event_id: row.try_get("event_id")?,
                    completion_work_id: row.try_get("completion_work_id")?,
                    thread_id: row.try_get("thread_id")?,
                    execution_kind: row.try_get("execution_kind")?,
                    execution_id: row.try_get("execution_id")?,
                    terminal_turn_id: row.try_get("terminal_turn_id")?,
                    callback_metadata_json: row.try_get("callback_metadata_json")?,
                    terminal_status: row.try_get("terminal_status")?,
                    final_text: row.try_get("final_text")?,
                    terminal_at_ms: row.try_get("terminal_at_ms")?,
                    attempt: row.try_get("attempt_count")?,
                    lease_id,
                });
            }
        }
        transaction.commit().await?;
        Ok(events)
    }

    pub async fn next_webhook_outbox_wakeup_delay_ms(
        &self,
        maximum_delay_ms: i64,
    ) -> anyhow::Result<i64> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let due_at_ms: Option<i64> = sqlx::query_scalar(
            r#"
SELECT MIN(
    CASE state
        WHEN 'pending' THEN available_at_ms
        WHEN 'sending' THEN lease_expires_at_ms
    END
)
FROM completion_webhook_outbox
WHERE state IN ('pending', 'sending')
            "#,
        )
        .fetch_one(self.pool.as_ref())
        .await?;
        let delay_ms = due_at_ms
            .map(|due_at_ms| due_at_ms.saturating_sub(now_ms).max(0))
            .unwrap_or(maximum_delay_ms);
        Ok(delay_ms.min(maximum_delay_ms).max(0))
    }

    pub async fn webhook_outbox_stats(&self) -> anyhow::Result<CompletionOutboxStats> {
        let row = sqlx::query(
            r#"
SELECT
    SUM(CASE WHEN state = 'pending' THEN 1 ELSE 0 END) AS pending_count,
    SUM(CASE WHEN state = 'sending' THEN 1 ELSE 0 END) AS sending_count,
    SUM(CASE WHEN state = 'sent' THEN 1 ELSE 0 END) AS sent_count,
    COALESCE(SUM(attempt_count), 0) AS total_attempts,
    MIN(CASE WHEN state IN ('pending', 'sending') THEN created_at_ms END) AS oldest_unsent_at_ms
FROM completion_webhook_outbox
            "#,
        )
        .fetch_one(self.pool.as_ref())
        .await?;
        Ok(CompletionOutboxStats {
            pending_count: row.try_get::<Option<i64>, _>("pending_count")?.unwrap_or(0),
            sending_count: row.try_get::<Option<i64>, _>("sending_count")?.unwrap_or(0),
            sent_count: row.try_get::<Option<i64>, _>("sent_count")?.unwrap_or(0),
            total_attempts: row.try_get("total_attempts")?,
            oldest_unsent_at_ms: row.try_get("oldest_unsent_at_ms")?,
        })
    }

    pub async fn accept_callback(
        &self,
        callback: CompletionCallback,
    ) -> anyhow::Result<CompletionCallbackAcceptance> {
        validate_callback(&callback)?;
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let call_id = callback_call_id(&callback.delivery_id);
        let inserted = sqlx::query(
            r#"
INSERT OR IGNORE INTO completion_callback_inbox (
    delivery_id,
    event_id,
    completion_work_id,
    target_thread_id,
    source_agent_display_id,
    execution_kind,
    execution_id,
    terminal_status,
    callback_text,
    final_text,
    terminal_at_ms,
    payload_digest,
    call_id,
    state,
    created_at_ms,
    updated_at_ms
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?)
            "#,
        )
        .bind(&callback.delivery_id)
        .bind(&callback.event_id)
        .bind(&callback.completion_work_id)
        .bind(&callback.target_thread_id)
        .bind(&callback.source_agent_display_id)
        .bind(&callback.execution_kind)
        .bind(&callback.execution_id)
        .bind(&callback.terminal_status)
        .bind(&callback.callback_text)
        .bind(&callback.final_text)
        .bind(callback.terminal_at_ms)
        .bind(&callback.payload_digest)
        .bind(&call_id)
        .bind(now_ms)
        .bind(now_ms)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected()
            == 1;
        let record = self
            .callback_record(&callback.delivery_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("accepted completion callback row disappeared"))?;
        anyhow::ensure!(
            record.callback == callback,
            "completion callback delivery id is already bound to different immutable payload"
        );
        if inserted {
            self.notify_sender();
        }
        Ok(CompletionCallbackAcceptance { inserted, record })
    }

    pub async fn callback_record(
        &self,
        delivery_id: &str,
    ) -> anyhow::Result<Option<CompletionCallbackRecord>> {
        let row = sqlx::query(
            r#"
SELECT
    delivery_id,
    event_id,
    completion_work_id,
    target_thread_id,
    source_agent_display_id,
    execution_kind,
    execution_id,
    terminal_status,
    callback_text,
    final_text,
    terminal_at_ms,
    payload_digest,
    call_id,
    state,
    injected_boot_id,
    attempt_count
FROM completion_callback_inbox
WHERE delivery_id = ?
            "#,
        )
        .bind(delivery_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(callback_record_from_row).transpose()
    }

    pub async fn begin_callback_injection(
        &self,
        delivery_id: &str,
        boot_id: &str,
    ) -> anyhow::Result<bool> {
        validate_canonical_uuid(boot_id, "callback insertion boot id")?;
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let updated = sqlx::query(
            r#"
UPDATE completion_callback_inbox
SET
    state = 'injected',
    injected_boot_id = ?,
    attempt_count = attempt_count + 1,
    last_error = '',
    updated_at_ms = ?
WHERE delivery_id = ?
  AND state != 'delivered'
  AND (state = 'pending' OR injected_boot_id IS NULL OR injected_boot_id != ?)
            "#,
        )
        .bind(boot_id)
        .bind(now_ms)
        .bind(delivery_id)
        .bind(boot_id)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    pub async fn retry_callback_injection(
        &self,
        delivery_id: &str,
        boot_id: &str,
        error: &str,
    ) -> anyhow::Result<bool> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let updated = sqlx::query(
            r#"
UPDATE completion_callback_inbox
SET
    state = 'pending',
    injected_boot_id = NULL,
    last_error = ?,
    updated_at_ms = ?
WHERE delivery_id = ?
  AND state = 'injected'
  AND injected_boot_id = ?
            "#,
        )
        .bind(error)
        .bind(now_ms)
        .bind(delivery_id)
        .bind(boot_id)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();
        if updated == 1 {
            self.notify_sender();
        }
        Ok(updated == 1)
    }

    pub async fn mark_callback_delivered(&self, delivery_id: &str) -> anyhow::Result<bool> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let updated = sqlx::query(
            r#"
UPDATE completion_callback_inbox
SET
    state = 'delivered',
    injected_boot_id = NULL,
    last_error = '',
    updated_at_ms = ?
WHERE delivery_id = ? AND state != 'delivered'
            "#,
        )
        .bind(now_ms)
        .bind(delivery_id)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    pub async fn mark_sent(&self, event_id: &str, lease_id: &str) -> anyhow::Result<bool> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let updated = sqlx::query(
            r#"
UPDATE completion_outbox
SET
    state = 'sent',
    lease_id = NULL,
    lease_expires_at_ms = NULL,
    last_error = '',
    updated_at_ms = ?
WHERE event_id = ? AND state = 'sending' AND lease_id = ?
            "#,
        )
        .bind(now_ms)
        .bind(event_id)
        .bind(lease_id)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    pub async fn mark_undeliverable(
        &self,
        event_id: &str,
        lease_id: &str,
        error: &str,
    ) -> anyhow::Result<bool> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let updated = sqlx::query(
            r#"
UPDATE completion_outbox
SET
    state = 'sent',
    lease_id = NULL,
    lease_expires_at_ms = NULL,
    last_error = ?,
    updated_at_ms = ?
WHERE event_id = ? AND state = 'sending' AND lease_id = ?
            "#,
        )
        .bind(error)
        .bind(now_ms)
        .bind(event_id)
        .bind(lease_id)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    pub async fn mark_webhook_attempted(
        &self,
        event_id: &str,
        lease_id: &str,
        error: &str,
    ) -> anyhow::Result<bool> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let updated = sqlx::query(
            r#"
UPDATE completion_webhook_outbox
SET
    state = 'sent',
    lease_id = NULL,
    lease_expires_at_ms = NULL,
    last_error = ?,
    updated_at_ms = ?
WHERE event_id = ? AND state = 'sending' AND lease_id = ?
            "#,
        )
        .bind(error)
        .bind(now_ms)
        .bind(event_id)
        .bind(lease_id)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    pub async fn retry_webhook_later(
        &self,
        event_id: &str,
        lease_id: &str,
        error: &str,
        delay_ms: i64,
    ) -> anyhow::Result<bool> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let available_at_ms = now_ms.saturating_add(delay_ms.max(0));
        let updated = sqlx::query(
            r#"
UPDATE completion_webhook_outbox
SET
    state = 'pending',
    available_at_ms = ?,
    lease_id = NULL,
    lease_expires_at_ms = NULL,
    last_error = ?,
    updated_at_ms = ?
WHERE event_id = ? AND state = 'sending' AND lease_id = ?
            "#,
        )
        .bind(available_at_ms)
        .bind(error)
        .bind(now_ms)
        .bind(event_id)
        .bind(lease_id)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();
        if updated == 1 {
            self.notify_sender();
        }
        Ok(updated == 1)
    }

    pub async fn retry_later(
        &self,
        event_id: &str,
        lease_id: &str,
        error: &str,
        delay_ms: i64,
    ) -> anyhow::Result<bool> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let available_at_ms = now_ms.saturating_add(delay_ms.max(0));
        let updated = sqlx::query(
            r#"
UPDATE completion_outbox
SET
    state = 'pending',
    available_at_ms = ?,
    lease_id = NULL,
    lease_expires_at_ms = NULL,
    last_error = ?,
    updated_at_ms = ?
WHERE event_id = ? AND state = 'sending' AND lease_id = ?
            "#,
        )
        .bind(available_at_ms)
        .bind(error)
        .bind(now_ms)
        .bind(event_id)
        .bind(lease_id)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();
        if updated == 1 {
            self.notify_sender();
        }
        Ok(updated == 1)
    }

    pub(crate) async fn bind_goal_in_transaction(
        &self,
        connection: &mut SqliteConnection,
        completion_work_id: &str,
        thread_id: ThreadId,
        goal_id: &str,
        callback_metadata_json: &str,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        bind_execution(
            &mut *connection,
            ExecutionBinding {
                completion_work_id,
                thread_id,
                execution_kind: "goal",
                execution_id: goal_id,
                callback_metadata_json,
                initial_state: "active",
                now_ms,
            },
        )
        .await?;
        enqueue_terminal_goal_if_needed(connection, completion_work_id, now_ms).await
    }

    pub async fn existing_goal_binding(
        &self,
        completion_work_id: &str,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<(String, CompletionBindingState)>> {
        self.existing_goal_binding_with_callback_metadata(completion_work_id, thread_id, "")
            .await
    }

    pub async fn existing_goal_binding_with_callback_metadata(
        &self,
        completion_work_id: &str,
        thread_id: ThreadId,
        callback_metadata_json: &str,
    ) -> anyhow::Result<Option<(String, CompletionBindingState)>> {
        let row = sqlx::query(
            r#"
SELECT thread_id, execution_kind, execution_id, callback_metadata_json, state
FROM completion_bindings
WHERE completion_work_id = ?
            "#,
        )
        .bind(completion_work_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let stored_thread_id: String = row.try_get("thread_id")?;
        let execution_kind: String = row.try_get("execution_kind")?;
        let stored_callback_metadata_json: String = row.try_get("callback_metadata_json")?;
        anyhow::ensure!(
            stored_thread_id == thread_id.to_string()
                && execution_kind == "goal"
                && stored_callback_metadata_json == callback_metadata_json,
            "completion work id is already bound to different execution intent"
        );
        let execution_id = row.try_get("execution_id")?;
        let state: String = row.try_get("state")?;
        Ok(Some((execution_id, completion_binding_state(&state)?)))
    }
}

fn callback_record_from_row(row: SqliteRow) -> anyhow::Result<CompletionCallbackRecord> {
    let state: String = row.try_get("state")?;
    let state = match state.as_str() {
        "pending" => CompletionCallbackState::Pending,
        "injected" => CompletionCallbackState::Injected,
        "delivered" => CompletionCallbackState::Delivered,
        unexpected => anyhow::bail!("unexpected completion callback state: {unexpected}"),
    };
    Ok(CompletionCallbackRecord {
        callback: CompletionCallback {
            delivery_id: row.try_get("delivery_id")?,
            event_id: row.try_get("event_id")?,
            completion_work_id: row.try_get("completion_work_id")?,
            target_thread_id: row.try_get("target_thread_id")?,
            source_agent_display_id: row.try_get("source_agent_display_id")?,
            execution_kind: row.try_get("execution_kind")?,
            execution_id: row.try_get("execution_id")?,
            terminal_status: row.try_get("terminal_status")?,
            callback_text: row.try_get("callback_text")?,
            final_text: row.try_get("final_text")?,
            terminal_at_ms: row.try_get("terminal_at_ms")?,
            payload_digest: row.try_get("payload_digest")?,
        },
        call_id: row.try_get("call_id")?,
        state,
        injected_boot_id: row.try_get("injected_boot_id")?,
        attempt_count: row.try_get("attempt_count")?,
    })
}

fn validate_callback(callback: &CompletionCallback) -> anyhow::Result<()> {
    validate_canonical_uuid(&callback.delivery_id, "callback delivery id")?;
    validate_canonical_uuid(&callback.event_id, "callback event id")?;
    validate_canonical_uuid(&callback.completion_work_id, "callback completion work id")?;
    let thread_id = ThreadId::from_string(&callback.target_thread_id)
        .map_err(|err| anyhow::anyhow!("callback target thread id is invalid: {err}"))?;
    anyhow::ensure!(
        thread_id.to_string() == callback.target_thread_id,
        "callback target thread id must use canonical lowercase UUID form"
    );
    anyhow::ensure!(
        matches!(callback.execution_kind.as_str(), "normal" | "goal"),
        "callback execution kind is invalid"
    );
    let valid_terminal_status = match callback.execution_kind.as_str() {
        "normal" => callback.terminal_status == "completed",
        "goal" => matches!(
            callback.terminal_status.as_str(),
            "complete" | "blocked" | "usageLimited" | "budgetLimited"
        ),
        _ => false,
    };
    anyhow::ensure!(
        valid_terminal_status,
        "callback terminal status does not match its execution kind"
    );
    anyhow::ensure!(
        !callback.source_agent_display_id.is_empty(),
        "callback source agent display id must not be empty"
    );
    anyhow::ensure!(
        !callback.execution_id.is_empty(),
        "callback execution id must not be empty"
    );
    anyhow::ensure!(
        !callback.callback_text.is_empty(),
        "callback text must not be empty"
    );
    anyhow::ensure!(
        callback.payload_digest.len() == 64
            && callback
                .payload_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "callback payload digest must be a lowercase SHA-256 hex digest"
    );
    Ok(())
}

fn validate_canonical_uuid(value: &str, label: &str) -> anyhow::Result<()> {
    let parsed =
        Uuid::parse_str(value).map_err(|err| anyhow::anyhow!("{label} must be a UUID: {err}"))?;
    anyhow::ensure!(
        parsed.to_string() == value,
        "{label} must use canonical lowercase UUID form"
    );
    Ok(())
}

fn callback_call_id(delivery_id: &str) -> String {
    format!("pitchai_callback_{}", delivery_id.replace('-', ""))
}

async fn outstanding_normal_turns(pool: &SqlitePool) -> anyhow::Result<HashSet<(String, String)>> {
    let rows = sqlx::query(
        r#"
SELECT thread_id, execution_id
FROM completion_bindings
WHERE execution_kind = 'normal'
  AND state IN ('registered', 'active')
        "#,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| Ok((row.try_get("thread_id")?, row.try_get("execution_id")?)))
        .collect()
}

fn bounded_completion_final_text(final_text: &str) -> Cow<'_, str> {
    if final_text.chars().count() <= MAX_COMPLETION_FINAL_TEXT_CHARS {
        return Cow::Borrowed(final_text);
    }
    let notice_chars = COMPLETION_FINAL_TEXT_TRUNCATION_NOTICE.chars().count();
    let retained_chars = MAX_COMPLETION_FINAL_TEXT_CHARS.saturating_sub(notice_chars);
    let mut bounded = final_text.chars().take(retained_chars).collect::<String>();
    bounded.push_str(COMPLETION_FINAL_TEXT_TRUNCATION_NOTICE);
    Cow::Owned(bounded)
}

async fn bind_execution(
    connection: &mut SqliteConnection,
    binding: ExecutionBinding<'_>,
) -> anyhow::Result<String> {
    let ExecutionBinding {
        completion_work_id,
        thread_id,
        execution_kind,
        execution_id,
        callback_metadata_json,
        initial_state,
        now_ms,
    } = binding;
    let parsed_work_id = Uuid::parse_str(completion_work_id)
        .map_err(|err| anyhow::anyhow!("completion work id must be a UUID: {err}"))?;
    anyhow::ensure!(
        parsed_work_id.to_string() == completion_work_id,
        "completion work id must use canonical lowercase UUID form"
    );
    sqlx::query(
        r#"
INSERT OR IGNORE INTO completion_bindings (
    completion_work_id,
    thread_id,
    execution_kind,
    execution_id,
    callback_metadata_json,
    state,
    created_at_ms,
    updated_at_ms
    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(completion_work_id)
    .bind(thread_id.to_string())
    .bind(execution_kind)
    .bind(execution_id)
    .bind(callback_metadata_json)
    .bind(initial_state)
    .bind(now_ms)
    .bind(now_ms)
    .execute(&mut *connection)
    .await?;
    let row = sqlx::query(
        r#"
SELECT thread_id, execution_kind, execution_id, callback_metadata_json, state
FROM completion_bindings
WHERE completion_work_id = ?
        "#,
    )
    .bind(completion_work_id)
    .fetch_one(&mut *connection)
    .await?;
    let stored_thread_id: String = row.try_get("thread_id")?;
    let stored_execution_kind: String = row.try_get("execution_kind")?;
    let stored_execution_id: String = row.try_get("execution_id")?;
    let stored_callback_metadata_json: String = row.try_get("callback_metadata_json")?;
    let stored_state: String = row.try_get("state")?;
    if stored_thread_id != thread_id.to_string()
        || stored_execution_kind != execution_kind
        || stored_execution_id != execution_id
        || stored_callback_metadata_json != callback_metadata_json
    {
        anyhow::bail!("completion work id is already bound to different execution intent");
    }
    Ok(stored_state)
}

fn completion_binding_state(state: &str) -> anyhow::Result<CompletionBindingState> {
    match state {
        "registered" => Ok(CompletionBindingState::Registered),
        "active" => Ok(CompletionBindingState::Active),
        "terminal" => Ok(CompletionBindingState::Terminal),
        unexpected => anyhow::bail!("unexpected completion binding state: {unexpected}"),
    }
}

async fn enqueue_terminal_goal_if_needed(
    connection: &mut SqliteConnection,
    completion_work_id: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
INSERT OR IGNORE INTO completion_outbox (
    event_id,
    completion_work_id,
    thread_id,
    execution_kind,
    execution_id,
    callback_metadata_json,
    terminal_status,
    final_text,
    terminal_at_ms,
    state,
    available_at_ms,
    terminal_turn_id,
    created_at_ms,
    updated_at_ms
)
SELECT
    binding.completion_work_id,
    binding.completion_work_id,
    binding.thread_id,
    binding.execution_kind,
    binding.execution_id,
    binding.callback_metadata_json,
    CASE goal.status
        WHEN 'usage_limited' THEN 'usageLimited'
        WHEN 'budget_limited' THEN 'budgetLimited'
        ELSE goal.status
    END,
    '',
    goal.updated_at_ms,
    'pending',
    goal.updated_at_ms + ?,
    NULL,
    ?,
    ?
FROM completion_bindings AS binding
JOIN thread_goals AS goal
  ON goal.thread_id = binding.thread_id
 AND goal.goal_id = binding.execution_id
WHERE binding.completion_work_id = ?
  AND binding.execution_kind = 'goal'
  AND binding.state = 'active'
  AND goal.status IN ('complete', 'blocked', 'usage_limited', 'budget_limited')
        "#,
    )
    .bind(TERMINAL_GOAL_FINAL_CAPTURE_GRACE_MS)
    .bind(now_ms)
    .bind(now_ms)
    .bind(completion_work_id)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        r#"
UPDATE completion_bindings
SET state = 'terminal', updated_at_ms = ?
WHERE completion_work_id = ?
  AND state = 'active'
  AND EXISTS (
      SELECT 1
      FROM thread_goals AS goal
      WHERE goal.thread_id = completion_bindings.thread_id
        AND goal.goal_id = completion_bindings.execution_id
        AND goal.status IN ('complete', 'blocked', 'usage_limited', 'budget_limited')
  )
        "#,
    )
    .bind(now_ms)
    .bind(completion_work_id)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::GoalUpdate;
    use crate::runtime::test_support::test_thread_metadata;
    use crate::runtime::test_support::unique_temp_dir;
    use pretty_assertions::assert_eq;

    async fn test_runtime() -> Arc<StateRuntime> {
        StateRuntime::init(unique_temp_dir(), "test-provider".to_string())
            .await
            .expect("state db should initialize")
    }

    fn test_thread_id() -> ThreadId {
        ThreadId::from_string("00000000-0000-0000-0000-000000000456").expect("valid thread id")
    }

    async fn upsert_test_thread(runtime: &StateRuntime, thread_id: ThreadId) {
        let metadata = test_thread_metadata(
            runtime.codex_home(),
            thread_id,
            runtime.codex_home().join("workspace"),
        );
        runtime
            .upsert_thread(&metadata)
            .await
            .expect("test thread should be upserted");
    }

    #[tokio::test]
    async fn normal_completion_is_deduplicated_and_retried_by_lease() {
        let runtime = test_runtime().await;
        let thread_id = test_thread_id();
        let completion_work_id = "10000000-0000-0000-0000-000000000001";
        let callback_metadata_json =
            r#"{"protocol_version":"pitchai-completion-callback/v1","text":"Publish the result."}"#;
        let binding_state = runtime
            .completions()
            .bind_turn_with_callback_metadata(
                completion_work_id,
                thread_id,
                "turn-1",
                callback_metadata_json,
            )
            .await
            .expect("turn binding should persist");
        assert_eq!(CompletionBindingState::Registered, binding_state);

        let inserted = runtime
            .completions()
            .complete_turn(thread_id, "turn-1", "finished", 1_000)
            .await
            .expect("turn completion should persist");
        let duplicate = runtime
            .completions()
            .complete_turn(thread_id, "turn-1", "duplicate", 2_000)
            .await
            .expect("duplicate completion should be harmless");
        assert_eq!(1, inserted);
        assert_eq!(0, duplicate);

        let first_claim = runtime
            .completions()
            .claim_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
            .await
            .expect("completion should be claimable");
        assert_eq!(1, first_claim.len());
        let first_event = &first_claim[0];
        assert_eq!(completion_work_id, first_event.event_id);
        assert_eq!("normal", first_event.execution_kind);
        assert_eq!("turn-1", first_event.execution_id);
        assert_eq!(Some("turn-1"), first_event.terminal_turn_id.as_deref());
        assert_eq!("completed", first_event.terminal_status);
        assert_eq!("finished", first_event.final_text);
        assert_eq!(callback_metadata_json, first_event.callback_metadata_json);
        assert_eq!(1, first_event.attempt);

        let concurrent_claim = runtime
            .completions()
            .claim_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
            .await
            .expect("leased completion should not fail another claim");
        assert_eq!(Vec::<CompletionOutboxEvent>::new(), concurrent_claim);

        let retry_scheduled = runtime
            .completions()
            .retry_later(
                &first_event.event_id,
                &first_event.lease_id,
                "temporary failure",
                /*delay_ms*/ 0,
            )
            .await
            .expect("retry should be persisted");
        assert!(retry_scheduled);

        let second_claim = runtime
            .completions()
            .claim_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
            .await
            .expect("retried completion should be claimable");
        assert_eq!(1, second_claim.len());
        let second_event = &second_claim[0];
        assert_eq!(2, second_event.attempt);
        assert_ne!(first_event.lease_id, second_event.lease_id);

        let marked_sent = runtime
            .completions()
            .mark_sent(&second_event.event_id, &second_event.lease_id)
            .await
            .expect("sent state should persist");
        assert!(marked_sent);
        assert_eq!(
            Vec::<CompletionOutboxEvent>::new(),
            runtime
                .completions()
                .claim_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
                .await
                .expect("sent completion should not be claimable")
        );

        let webhook_claim = runtime
            .completions()
            .claim_webhook_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
            .await
            .expect("callback completion should have an independent webhook event");
        assert_eq!(1, webhook_claim.len());
        assert_eq!(Some("turn-1"), webhook_claim[0].terminal_turn_id.as_deref());
        assert_eq!(
            callback_metadata_json,
            webhook_claim[0].callback_metadata_json
        );
        let webhook_retry_scheduled = runtime
            .completions()
            .retry_webhook_later(
                &webhook_claim[0].event_id,
                &webhook_claim[0].lease_id,
                "receiver unavailable",
                /*delay_ms*/ 0,
            )
            .await
            .expect("webhook retry should be persisted");
        assert!(webhook_retry_scheduled);
        let webhook_retry = runtime
            .completions()
            .claim_webhook_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
            .await
            .expect("failed webhook should remain claimable");
        assert_eq!(1, webhook_retry.len());
        assert_eq!(2, webhook_retry[0].attempt);
        assert_ne!(webhook_claim[0].lease_id, webhook_retry[0].lease_id);
        let webhook_finalized = runtime
            .completions()
            .mark_webhook_attempted(&webhook_retry[0].event_id, &webhook_retry[0].lease_id, "")
            .await
            .expect("accepted webhook should be finalized");
        assert!(webhook_finalized);
    }

    #[tokio::test]
    async fn turn_binding_retry_distinguishes_registered_from_accepted() {
        let runtime = test_runtime().await;
        let thread_id = test_thread_id();
        let completion_work_id = "10000000-0000-0000-0000-000000000004";
        let completions = runtime.completions();

        let initial = completions
            .bind_turn(completion_work_id, thread_id, completion_work_id)
            .await
            .expect("initial binding should persist");
        let retry_before_submission = completions
            .bind_turn(completion_work_id, thread_id, completion_work_id)
            .await
            .expect("a retry before submission should remain recoverable");
        assert_eq!(CompletionBindingState::Registered, initial);
        assert_eq!(CompletionBindingState::Registered, retry_before_submission);

        completions
            .mark_turn_submitted(completion_work_id)
            .await
            .expect("accepted submission should be recorded");
        let retry_after_submission = completions
            .bind_turn(completion_work_id, thread_id, completion_work_id)
            .await
            .expect("an exact retry after submission should be idempotent");
        assert_eq!(CompletionBindingState::Active, retry_after_submission);

        let conflicting = completions
            .bind_turn(completion_work_id, thread_id, "different-turn")
            .await
            .expect_err("one work id must not bind to a different execution");
        assert!(
            conflicting
                .to_string()
                .contains("already bound to different execution intent")
        );
    }

    #[tokio::test]
    async fn runtime_restart_tracks_only_outstanding_normal_callback_turns() {
        let sqlite_home = unique_temp_dir();
        let thread_id = test_thread_id();
        let completion_work_id = "10000000-0000-0000-0000-000000000005";
        let runtime = StateRuntime::init(sqlite_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        runtime
            .completions()
            .bind_turn(completion_work_id, thread_id, "turn-before-restart")
            .await
            .expect("turn binding should persist");
        runtime
            .completions()
            .mark_turn_submitted(completion_work_id)
            .await
            .expect("accepted submission should be recorded");
        drop(runtime);

        let restarted = StateRuntime::init(sqlite_home, "test-provider".to_string())
            .await
            .expect("state db should reopen");
        assert!(
            restarted
                .completions()
                .turn_is_tracked_in_process(thread_id, "turn-before-restart")
                .await
        );
        let existing = restarted
            .completions()
            .existing_turn_binding(completion_work_id, thread_id)
            .await
            .expect("existing binding should be readable");
        assert_eq!(
            Some((
                "turn-before-restart".to_string(),
                CompletionBindingState::Active
            )),
            existing
        );
        restarted
            .completions()
            .complete_turn(thread_id, "turn-before-restart", "recovered", 1_000)
            .await
            .expect("recovered completion should persist");
        assert!(
            !restarted
                .completions()
                .turn_is_tracked_in_process(thread_id, "turn-before-restart")
                .await
        );
    }

    #[tokio::test]
    async fn normal_completion_bounds_final_text_for_durable_transport() {
        let runtime = test_runtime().await;
        let thread_id = test_thread_id();
        let completion_work_id = "10000000-0000-0000-0000-000000000006";
        runtime
            .completions()
            .bind_turn(completion_work_id, thread_id, "turn-large-final")
            .await
            .expect("turn binding should persist");
        let oversized = "x".repeat(MAX_COMPLETION_FINAL_TEXT_CHARS + 100);
        runtime
            .completions()
            .complete_turn(thread_id, "turn-large-final", &oversized, 1_000)
            .await
            .expect("bounded completion should persist");

        let claimed = runtime
            .completions()
            .claim_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
            .await
            .expect("bounded completion should be claimable");
        assert_eq!(1, claimed.len());
        assert_eq!(
            MAX_COMPLETION_FINAL_TEXT_CHARS,
            claimed[0].final_text.chars().count()
        );
        assert!(
            claimed[0]
                .final_text
                .ends_with(COMPLETION_FINAL_TEXT_TRUNCATION_NOTICE)
        );
    }

    #[tokio::test]
    async fn tracked_goal_terminal_transition_creates_one_completion_event() {
        let runtime = test_runtime().await;
        let thread_id = test_thread_id();
        upsert_test_thread(&runtime, thread_id).await;
        let completion_work_id = "10000000-0000-0000-0000-000000000002";
        let callback_metadata_json = r#"{"protocol_version":"pitchai-completion-callback/v1","text":"Publish goal completion."}"#;
        let goal = runtime
            .thread_goals()
            .replace_thread_goal_with_completion_metadata(
                thread_id,
                "finish the callback test",
                crate::ThreadGoalStatus::Active,
                /*token_budget*/ None,
                completion_work_id,
                callback_metadata_json,
            )
            .await
            .expect("tracked goal should persist atomically");
        assert_eq!(
            Vec::<CompletionOutboxEvent>::new(),
            runtime
                .completions()
                .claim_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
                .await
                .expect("active goal should not emit completion")
        );

        let completed_goal = runtime
            .thread_goals()
            .update_thread_goal(
                thread_id,
                GoalUpdate {
                    objective: None,
                    status: Some(crate::ThreadGoalStatus::Complete),
                    token_budget: None,
                    expected_goal_id: Some(goal.goal_id.clone()),
                },
            )
            .await
            .expect("goal completion should persist")
            .expect("goal should still exist");

        assert_eq!(
            Vec::<CompletionOutboxEvent>::new(),
            runtime
                .completions()
                .claim_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
                .await
                .expect("terminal goal should wait for the assistant final")
        );
        assert_eq!(
            0,
            runtime
                .completions()
                .associate_terminal_goal_turn(
                    thread_id,
                    "blocked",
                    completed_goal.updated_at.timestamp(),
                    "turn-goal-final",
                )
                .await
                .expect("an unrelated terminal transition should not bind")
        );
        runtime
            .completions()
            .associate_terminal_goal_turn(
                thread_id,
                "complete",
                completed_goal.updated_at.timestamp(),
                "turn-goal-final",
            )
            .await
            .expect("terminal goal should bind to its emitting turn");
        assert_eq!(
            0,
            runtime
                .completions()
                .associate_terminal_goal_turn(
                    thread_id,
                    "complete",
                    completed_goal.updated_at.timestamp(),
                    "turn-goal-final",
                )
                .await
                .expect("an exact goal update replay should be idempotent")
        );
        runtime
            .completions()
            .complete_terminal_goal_turn(
                thread_id,
                "turn-goal-final",
                "The tracked goal is complete.",
                datetime_to_epoch_millis(Utc::now()),
            )
            .await
            .expect("assistant final should complete the goal event");

        let claimed = runtime
            .completions()
            .claim_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
            .await
            .expect("terminal goal should emit completion");
        assert_eq!(1, claimed.len());
        assert_eq!(completion_work_id, claimed[0].event_id);
        assert_eq!("goal", claimed[0].execution_kind);
        assert_eq!(goal.goal_id, claimed[0].execution_id);
        assert_eq!(
            Some("turn-goal-final"),
            claimed[0].terminal_turn_id.as_deref()
        );
        assert_eq!("complete", claimed[0].terminal_status);
        assert_eq!("The tracked goal is complete.", claimed[0].final_text);
        assert_eq!(callback_metadata_json, claimed[0].callback_metadata_json);
        let webhook_claim = runtime
            .completions()
            .claim_webhook_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
            .await
            .expect("terminal goal should emit a webhook event");
        assert_eq!(1, webhook_claim.len());
        assert_eq!("goal", webhook_claim[0].execution_kind);
        assert_eq!(
            Some("turn-goal-final"),
            webhook_claim[0].terminal_turn_id.as_deref()
        );
        assert_eq!(
            callback_metadata_json,
            webhook_claim[0].callback_metadata_json
        );
    }

    #[tokio::test]
    async fn every_terminal_goal_status_emits_one_webhook_event() {
        let terminal_statuses = [
            (crate::ThreadGoalStatus::Complete, "complete"),
            (crate::ThreadGoalStatus::Blocked, "blocked"),
            (crate::ThreadGoalStatus::UsageLimited, "usageLimited"),
            (crate::ThreadGoalStatus::BudgetLimited, "budgetLimited"),
        ];
        let callback_metadata_json = r#"{"protocol_version":"pitchai-completion-callback/v1","text":"Publish terminal goal state."}"#;

        for (index, (status, expected_status)) in terminal_statuses.into_iter().enumerate() {
            let runtime = test_runtime().await;
            let thread_id = test_thread_id();
            upsert_test_thread(&runtime, thread_id).await;
            let completion_work_id = format!("10000000-0000-0000-0000-00000000020{index}");
            let goal = runtime
                .thread_goals()
                .replace_thread_goal_with_completion_metadata(
                    thread_id,
                    "reach one supported terminal state",
                    crate::ThreadGoalStatus::Active,
                    /*token_budget*/ None,
                    &completion_work_id,
                    callback_metadata_json,
                )
                .await
                .expect("tracked goal should persist");

            let terminal_goal = runtime
                .thread_goals()
                .update_thread_goal(
                    thread_id,
                    GoalUpdate {
                        objective: None,
                        status: Some(status),
                        token_budget: None,
                        expected_goal_id: Some(goal.goal_id.clone()),
                    },
                )
                .await
                .expect("terminal goal status should persist")
                .expect("goal should still exist");

            let turn_id = format!("turn-terminal-{index}");
            runtime
                .completions()
                .associate_terminal_goal_turn(
                    thread_id,
                    expected_status,
                    terminal_goal.updated_at.timestamp(),
                    &turn_id,
                )
                .await
                .expect("terminal goal should bind to its emitting turn");
            runtime
                .completions()
                .complete_terminal_goal_turn(
                    thread_id,
                    &turn_id,
                    "Terminal goal final.",
                    datetime_to_epoch_millis(Utc::now()),
                )
                .await
                .expect("terminal goal final should become deliverable");

            let webhook_events = runtime
                .completions()
                .claim_webhook_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
                .await
                .expect("terminal goal should emit a webhook event");
            assert_eq!(1, webhook_events.len());
            assert_eq!(expected_status, webhook_events[0].terminal_status);
            assert_eq!(
                Vec::<CompletionOutboxEvent>::new(),
                runtime
                    .completions()
                    .claim_webhook_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
                    .await
                    .expect("terminal goal webhook should emit exactly once")
            );
        }
    }

    #[tokio::test]
    async fn immediately_limited_goal_is_bound_and_emitted_in_one_transaction() {
        let runtime = test_runtime().await;
        let thread_id = test_thread_id();
        upsert_test_thread(&runtime, thread_id).await;
        let completion_work_id = "10000000-0000-0000-0000-000000000003";

        let goal = runtime
            .thread_goals()
            .replace_thread_goal_with_completion(
                thread_id,
                "respect the zero token budget",
                crate::ThreadGoalStatus::Active,
                /*token_budget*/ Some(0),
                completion_work_id,
            )
            .await
            .expect("immediately terminal goal and binding should commit together");
        assert_eq!(crate::ThreadGoalStatus::BudgetLimited, goal.status);

        assert_eq!(
            Vec::<CompletionOutboxEvent>::new(),
            runtime
                .completions()
                .claim_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
                .await
                .expect("immediately terminal goal should honor final capture grace")
        );
        sqlx::query("UPDATE completion_outbox SET available_at_ms = 0 WHERE event_id = ?")
            .bind(completion_work_id)
            .execute(runtime.completions().pool.as_ref())
            .await
            .expect("test should advance the fallback delivery clock");

        let claimed = runtime
            .completions()
            .claim_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
            .await
            .expect("immediately terminal goal should emit completion");
        assert_eq!(1, claimed.len());
        assert_eq!("budgetLimited", claimed[0].terminal_status);
        assert_eq!(goal.goal_id, claimed[0].execution_id);
        assert_eq!(
            Vec::<CompletionOutboxEvent>::new(),
            runtime
                .completions()
                .claim_webhook_outbox(/*limit*/ 10, /*lease_duration_ms*/ 60_000)
                .await
                .expect("work without callback metadata must not emit a webhook")
        );
    }

    #[tokio::test]
    async fn callback_inbox_deduplicates_payload_and_fences_injection_by_boot() {
        let runtime = test_runtime().await;
        let callback = CompletionCallback {
            delivery_id: "30000000-0000-0000-0000-000000000001".to_string(),
            event_id: "30000000-0000-0000-0000-000000000002".to_string(),
            completion_work_id: "30000000-0000-0000-0000-000000000003".to_string(),
            target_thread_id: test_thread_id().to_string(),
            source_agent_display_id: "source-agent".to_string(),
            execution_kind: "normal".to_string(),
            execution_id: "turn-1".to_string(),
            terminal_status: "completed".to_string(),
            callback_text: "Tell me whether the task passed.".to_string(),
            final_text: "It passed.".to_string(),
            terminal_at_ms: 1_000,
            payload_digest: "a".repeat(64),
        };
        let first_acceptance = runtime
            .completions()
            .accept_callback(callback.clone())
            .await
            .expect("callback should be accepted");
        assert!(first_acceptance.inserted);
        assert_eq!(
            CompletionCallbackState::Pending,
            first_acceptance.record.state
        );
        let duplicate = runtime
            .completions()
            .accept_callback(callback.clone())
            .await
            .expect("exact callback replay should be accepted");
        assert!(!duplicate.inserted);
        assert_eq!(first_acceptance.record, duplicate.record);

        let first_boot = "40000000-0000-0000-0000-000000000001";
        assert!(
            runtime
                .completions()
                .begin_callback_injection(&callback.delivery_id, first_boot)
                .await
                .expect("first boot should claim callback injection")
        );
        assert!(
            !runtime
                .completions()
                .begin_callback_injection(&callback.delivery_id, first_boot)
                .await
                .expect("same boot should not inject callback twice")
        );
        let second_boot = "40000000-0000-0000-0000-000000000002";
        assert!(
            runtime
                .completions()
                .begin_callback_injection(&callback.delivery_id, second_boot)
                .await
                .expect("new boot should recover an unconfirmed injection")
        );
        assert!(
            runtime
                .completions()
                .mark_callback_delivered(&callback.delivery_id)
                .await
                .expect("visible callback should become delivered")
        );
        assert!(
            !runtime
                .completions()
                .begin_callback_injection(&callback.delivery_id, second_boot)
                .await
                .expect("delivered callback should never inject again")
        );

        let changed_callback = CompletionCallback {
            final_text: "different terminal evidence".to_string(),
            ..callback
        };
        assert!(
            runtime
                .completions()
                .accept_callback(changed_callback)
                .await
                .is_err()
        );
    }
}
