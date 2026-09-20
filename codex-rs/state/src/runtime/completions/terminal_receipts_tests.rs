use crate::StateRuntime;
use crate::runtime::test_support::unique_temp_dir;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn unregistered_turn_receipt_survives_reopen_without_publication() -> anyhow::Result<()> {
    let home = unique_temp_dir();
    let thread_id = ThreadId::new();
    let runtime = StateRuntime::init(home.clone(), "test-provider".to_string()).await?;
    let store = runtime.completions();
    assert_eq!(
        None,
        store
            .successful_turn_completed_at(thread_id, "turn-a")
            .await?
    );
    assert_eq!(
        0,
        store
            .complete_turn(
                thread_id,
                "turn-a",
                "private text",
                /*terminal_at_ms*/ 1_000
            )
            .await?
    );
    assert_eq!(
        0,
        store
            .complete_turn(
                thread_id,
                "turn-a",
                "duplicate",
                /*terminal_at_ms*/ 2_000
            )
            .await?
    );
    assert_eq!(
        Some(1_000),
        store
            .successful_turn_completed_at(thread_id, "turn-a")
            .await?
    );
    assert_eq!(
        None,
        store
            .successful_turn_completed_at(thread_id, "turn-b")
            .await?
    );
    assert_eq!(
        None,
        store
            .successful_turn_completed_at(ThreadId::new(), "turn-a")
            .await?
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM completion_bindings) + \
                (SELECT COUNT(*) FROM completion_outbox) + \
                (SELECT COUNT(*) FROM completion_webhook_outbox)",
    )
    .fetch_one(store.pool.as_ref())
    .await?;
    assert_eq!(0, count);
    store.pool.close().await;
    drop(runtime);
    let reopened = StateRuntime::init(home, "test-provider".to_string()).await?;
    assert_eq!(
        Some(1_000),
        reopened
            .completions()
            .successful_turn_completed_at(thread_id, "turn-a")
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn registered_publication_failure_rolls_back_local_receipt() -> anyhow::Result<()> {
    let runtime = StateRuntime::init(unique_temp_dir(), "test-provider".to_string()).await?;
    let store = runtime.completions();
    let thread_id = ThreadId::new();
    store
        .bind_turn_with_callback_metadata(
            "10000000-0000-0000-0000-000000000001",
            thread_id,
            "turn-a",
            "",
        )
        .await?;
    sqlx::query(
        "CREATE TRIGGER reject_test_publication BEFORE INSERT ON completion_outbox \
                 BEGIN SELECT RAISE(ABORT, 'test publication failure'); END",
    )
    .execute(store.pool.as_ref())
    .await?;
    assert!(
        store
            .complete_turn(thread_id, "turn-a", "done", /*terminal_at_ms*/ 1_000)
            .await
            .is_err()
    );
    assert_eq!(
        None,
        store
            .successful_turn_completed_at(thread_id, "turn-a")
            .await?
    );
    sqlx::query("DROP TRIGGER reject_test_publication")
        .execute(store.pool.as_ref())
        .await?;
    assert_eq!(
        1,
        store
            .complete_turn(thread_id, "turn-a", "done", /*terminal_at_ms*/ 1_000)
            .await?
    );
    assert_eq!(
        Some(1_000),
        store
            .successful_turn_completed_at(thread_id, "turn-a")
            .await?
    );
    Ok(())
}
