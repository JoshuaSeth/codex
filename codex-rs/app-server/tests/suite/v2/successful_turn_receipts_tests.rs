use super::*;
use anyhow::Context;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn untracked_success_is_durable_before_completed_notification_without_publication()
-> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Local-only completion").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let mut app_server = app_server_without_completion_sender(codex_home.path()).await?;
    timeout(STARTUP_TIMEOUT, app_server.initialize()).await??;
    let thread_id = start_thread(&mut app_server).await?;
    let turn = start_untracked_turn(&mut app_server, &thread_id).await?;
    let completed = completed_turn(&mut app_server).await?;
    assert_eq!(
        (&thread_id, &turn.id, TurnStatus::Completed),
        (
            &completed.thread_id,
            &completed.turn.id,
            completed.turn.status
        )
    );
    let state =
        StateRuntime::init(codex_home.path().to_path_buf(), "mock_provider".to_string()).await?;
    let timestamp = state
        .completions()
        .successful_turn_completed_at(ThreadId::from_string(&thread_id)?, &turn.id)
        .await?;
    assert!(timestamp.is_some_and(|value| value > 0));
    assert!(claim_outbox(codex_home.path()).await?.is_empty());
    assert!(claim_webhook_outbox(codex_home.path()).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn failed_untracked_turn_has_no_successful_receipt() -> Result<()> {
    let server = app_test_support::create_mock_responses_server_sequence_unchecked(vec![
        responses::sse_failed("resp-failed", "invalid_prompt", "Synthetic rejected prompt"),
    ])
    .await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let mut app_server = app_server_without_completion_sender(codex_home.path()).await?;
    timeout(STARTUP_TIMEOUT, app_server.initialize()).await??;
    let thread_id = start_thread(&mut app_server).await?;
    let turn = start_untracked_turn(&mut app_server, &thread_id).await?;
    let completed = completed_turn(&mut app_server).await?;
    assert_eq!(TurnStatus::Failed, completed.turn.status);
    let state =
        StateRuntime::init(codex_home.path().to_path_buf(), "mock_provider".to_string()).await?;
    assert_eq!(
        None,
        state
            .completions()
            .successful_turn_completed_at(ThreadId::from_string(&thread_id)?, &turn.id)
            .await?
    );
    assert!(claim_outbox(codex_home.path()).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn running_then_interrupted_untracked_turn_has_no_successful_receipt() -> Result<()> {
    let (release, gate) = oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![vec![
        StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![responses::ev_response_created("resp-running")]),
        },
        StreamingSseChunk {
            gate: Some(gate),
            body: responses::sse(vec![responses::ev_completed("resp-running")]),
        },
    ]])
    .await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), server.uri())?;
    let mut app_server = app_server_without_completion_sender(codex_home.path()).await?;
    timeout(STARTUP_TIMEOUT, app_server.initialize()).await??;
    let thread_id = start_thread(&mut app_server).await?;
    let turn = start_untracked_turn(&mut app_server, &thread_id).await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        server.wait_for_request_count(/*count*/ 1),
    )
    .await?;
    let state =
        StateRuntime::init(codex_home.path().to_path_buf(), "mock_provider".to_string()).await?;
    let native_thread = ThreadId::from_string(&thread_id)?;
    assert_eq!(
        None,
        state
            .completions()
            .successful_turn_completed_at(native_thread, &turn.id)
            .await?
    );
    app_server
        .send_turn_interrupt_request(TurnInterruptParams {
            thread_id,
            turn_id: turn.id.clone(),
        })
        .await?;
    let completed = completed_turn(&mut app_server).await?;
    assert_eq!(TurnStatus::Interrupted, completed.turn.status);
    assert_eq!(
        None,
        state
            .completions()
            .successful_turn_completed_at(native_thread, &turn.id)
            .await?
    );
    assert!(claim_outbox(codex_home.path()).await?.is_empty());
    let _ = release.send(());
    server.shutdown().await;
    Ok(())
}

async fn start_untracked_turn(
    app_server: &mut TestAppServer,
    thread_id: &str,
) -> Result<codex_app_server_protocol::Turn> {
    let request = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text: "Finish this local turn".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(request)),
    )
    .await??;
    Ok(to_response::<TurnStartResponse>(response)?.turn)
}

async fn completed_turn(app_server: &mut TestAppServer) -> Result<TurnCompletedNotification> {
    let notification = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    Ok(serde_json::from_value(
        notification.params.context("turn/completed params")?,
    )?)
}
