use super::CompletionStore;
use codex_protocol::ThreadId;

impl CompletionStore {
    /// Read native successful execution evidence for one exact turn without
    /// loading its rollout or requiring a central completion-work registration.
    ///
    /// This is not quiescence, goal completion, or callback delivery evidence.
    /// Missing receipts (including pre-migration turns) remain unknown. The
    /// first committed terminal timestamp is retained across duplicate events.
    pub async fn successful_turn_completed_at(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> anyhow::Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT completed_at_ms FROM successful_turn_receipts \
             WHERE thread_id = ? AND turn_id = ?",
        )
        .bind(thread_id.to_string())
        .bind(turn_id)
        .fetch_optional(self.pool.as_ref())
        .await?)
    }
}

#[cfg(test)]
#[path = "terminal_receipts_tests.rs"]
mod tests;
