use std::sync::Arc;

use super::SessionTask;
use super::SessionTaskContext;
use crate::codex::TurnContext;
use crate::state::TaskKind;
use async_trait::async_trait;
use codex_protocol::user_input::UserInput;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Default)]
pub(crate) struct CompactTask;

#[async_trait]
impl SessionTask for CompactTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Compact
    }

    fn span_name(&self) -> &'static str {
        "session_task.compact"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<UserInput>,
        _cancellation_token: CancellationToken,
    ) -> Option<String> {
        let session = session.clone_session();
        let _ = if crate::compact::should_use_remote_compact_task(&ctx.provider) {
            let _ = session.services.otel_manager.counter(
                "codex.task.compact",
                1,
                &[("type", "remote")],
            );
            match crate::compact_remote::run_remote_compact_task(session.clone(), ctx.clone()).await
            {
                Ok(()) => Ok(()),
                Err(err) if crate::compact_remote::should_fallback_to_local_compact(&err) => {
                    let _ = session.services.otel_manager.counter(
                        "codex.task.compact",
                        1,
                        &[("type", "local_fallback")],
                    );
                    let fallback_reason =
                        if crate::compact_remote::is_remote_compact_payload_too_large(&err) {
                            "payload exceeded backend size limits"
                        } else {
                            "request failed"
                        };
                    session
                        .notify_background_event(
                            ctx.as_ref(),
                            format!(
                                "Remote compact {fallback_reason}; falling back to local compaction."
                            ),
                        )
                        .await;
                    crate::compact::run_compact_task(session.clone(), ctx, input).await
                }
                Err(err) => Err(err),
            }
        } else {
            let _ = session.services.otel_manager.counter(
                "codex.task.compact",
                1,
                &[("type", "local")],
            );
            crate::compact::run_compact_task(session.clone(), ctx, input).await
        };
        None
    }
}
