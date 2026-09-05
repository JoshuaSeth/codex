use super::preserve_completion_binding_after_submit_error;
use super::release_rejected_completion_binding;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_state::CompletionBindingState;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[test]
fn internal_agent_died_preserves_registered_completion_binding_for_exact_retry() {
    assert!(preserve_completion_binding_after_submit_error(
        &CodexErr::InternalAgentDied
    ));
}

#[test]
fn ordinary_rejected_submission_releases_registered_completion_binding() {
    assert!(!preserve_completion_binding_after_submit_error(
        &CodexErr::InvalidRequest("rejected".to_string())
    ));
}

#[tokio::test]
async fn internal_agent_died_keeps_persisted_registered_completion_binding() {
    let temp_dir = TempDir::new().expect("temporary state directory should exist");
    let runtime = StateRuntime::init(temp_dir.path().to_path_buf(), "test-provider".to_string())
        .await
        .expect("state runtime should initialize");
    let completions = runtime.completions();
    let completion_work_id = "19240c9f-7038-47bc-9b54-5d7e52854db2";
    let thread_id = ThreadId::from_string("20000000-0000-0000-0000-000000000007")
        .expect("thread id should be valid");
    completions
        .bind_turn(completion_work_id, thread_id, completion_work_id)
        .await
        .expect("completion binding should persist");

    release_rejected_completion_binding(
        completions,
        completion_work_id,
        &CodexErr::InternalAgentDied,
    )
    .await
    .expect("dead-handle recovery should preserve the binding");

    assert_eq!(
        completions
            .existing_turn_binding(completion_work_id, thread_id)
            .await
            .expect("completion binding should remain readable"),
        Some((
            completion_work_id.to_string(),
            CompletionBindingState::Registered
        ))
    );
}
