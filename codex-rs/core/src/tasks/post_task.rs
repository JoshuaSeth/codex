use std::sync::Arc;

use crate::session::session::Session;

/// Starts queued automatic work after a completed task, then emits idle lifecycle
/// hooks only when no queued work took ownership of the session.
///
/// This scheduler lives outside `tasks::mod` because starting queued work can
/// itself complete another task. The detached module boundary prevents that
/// cycle from becoming a recursive opaque task future.
pub(super) fn schedule_reconciliation(session: Arc<Session>) {
    drop(tokio::spawn(async move {
        session.maybe_start_turn_for_pending_work().await;
        session.emit_thread_idle_lifecycle_if_idle().await;
    }));
}
