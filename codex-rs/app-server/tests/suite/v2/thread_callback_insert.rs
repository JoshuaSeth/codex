use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadCallbackInsertOutcome;
use codex_app_server_protocol::ThreadCallbackInsertParams;
use codex_app_server_protocol::ThreadCallbackInsertResponse;
use codex_app_server_protocol::ThreadCallbackInsertState;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput as V2UserInput;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::timeout;

const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn callback_insert_reports_when_target_thread_needs_rehydration() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::new(codex_home.path()).await?;
    timeout(STARTUP_TIMEOUT, mcp.initialize()).await??;
    let params = callback_params("30000000-0000-0000-0000-000000000004".to_string());
    let insert_request = mcp.send_thread_callback_insert_request(params).await?;
    let insert_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(insert_request)),
    )
    .await??;
    let accepted = to_response::<ThreadCallbackInsertResponse>(insert_response)?;

    assert_eq!(ThreadCallbackInsertOutcome::Accepted, accepted.outcome);
    assert_eq!(ThreadCallbackInsertState::Pending, accepted.state);
    assert!(accepted.needs_rehydrate);
    Ok(())
}

#[tokio::test]
async fn callback_insert_wakes_idle_thread_and_deduplicates_after_model_visibility() -> Result<()> {
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_assistant_message("msg-1", "Callback reported"),
        responses::ev_completed("resp-1"),
    ]);
    let response_mock = responses::mount_sse_once(&server, body).await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::new(codex_home.path()).await?;
    timeout(STARTUP_TIMEOUT, mcp.initialize()).await??;
    let thread_request = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_request)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_response)?;

    let params = callback_params(thread.id.clone());
    let insert_request = mcp
        .send_thread_callback_insert_request(params.clone())
        .await?;
    let insert_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(insert_request)),
    )
    .await??;
    let accepted = to_response::<ThreadCallbackInsertResponse>(insert_response)?;
    assert_eq!(ThreadCallbackInsertOutcome::Accepted, accepted.outcome);
    assert_eq!(ThreadCallbackInsertState::Pending, accepted.state);
    assert!(!accepted.needs_rehydrate);
    assert_eq!(
        "pitchai_callback_30000000000000000000000000000001",
        accepted.call_id
    );

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let duplicate_request = mcp.send_thread_callback_insert_request(params).await?;
    let duplicate_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(duplicate_request)),
    )
    .await??;
    let duplicate = to_response::<ThreadCallbackInsertResponse>(duplicate_response)?;
    assert_eq!(ThreadCallbackInsertOutcome::Duplicate, duplicate.outcome);
    assert_eq!(ThreadCallbackInsertState::Delivered, duplicate.state);
    assert!(!duplicate.needs_rehydrate);

    let model_input = response_mock.single_request().input();
    let callback_items = model_input
        .iter()
        .filter(|item| response_call_id(item) == Some(accepted.call_id.as_str()));
    let callback_item_types = callback_items
        .filter_map(|item| item.get("type").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        vec!["function_call", "function_call_output"],
        callback_item_types,
        "the model should receive one callback call and its matching output"
    );
    assert!(
        model_input.iter().any(|item| {
            item.get("output")
                .and_then(Value::as_str)
                .is_some_and(|output| {
                    output.contains("Tell the requester this callback arrived.")
                        && output.contains("The delegated task finished.")
                        && output.contains("not a correction or new assignment")
                })
        }),
        "the model should receive the callback note, result, and non-steering instruction"
    );
    Ok(())
}

#[tokio::test]
async fn callback_insert_joins_active_turn_without_becoming_a_user_steer() -> Result<()> {
    let (release_first_response, first_response_gate) = oneshot::channel();
    let first_response = vec![
        StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![responses::ev_response_created("resp-1")]),
        },
        StreamingSseChunk {
            gate: Some(first_response_gate),
            body: responses::sse(vec![
                responses::ev_assistant_message("msg-1", "Initial progress"),
                responses::ev_completed("resp-1"),
            ]),
        },
    ];
    let callback_response = vec![StreamingSseChunk {
        gate: None,
        body: responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_assistant_message("msg-2", "Callback acknowledged"),
            responses::ev_completed("resp-2"),
        ]),
    }];
    let (server, _) = start_streaming_sse_server(vec![first_response, callback_response]).await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), server.uri())?;

    let mut mcp = TestAppServer::new(codex_home.path()).await?;
    timeout(STARTUP_TIMEOUT, mcp.initialize()).await??;
    let thread_request = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_request)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_response)?;

    let turn_request = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "Keep working on the original task.".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_request)),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        server.wait_for_request_count(/* count */ 1),
    )
    .await?;

    let params = callback_params(thread.id.clone());
    let insert_request = mcp
        .send_thread_callback_insert_request(params.clone())
        .await?;
    let insert_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(insert_request)),
    )
    .await??;
    let accepted = to_response::<ThreadCallbackInsertResponse>(insert_response)?;
    assert_eq!(ThreadCallbackInsertOutcome::Accepted, accepted.outcome);
    assert_eq!(ThreadCallbackInsertState::Pending, accepted.state);
    assert!(!accepted.needs_rehydrate);

    release_first_response
        .send(())
        .expect("release active model response");
    timeout(
        DEFAULT_READ_TIMEOUT,
        server.wait_for_request_count(/* count */ 2),
    )
    .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let duplicate_request = mcp.send_thread_callback_insert_request(params).await?;
    let duplicate_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(duplicate_request)),
    )
    .await??;
    let duplicate = to_response::<ThreadCallbackInsertResponse>(duplicate_response)?;
    assert_eq!(ThreadCallbackInsertOutcome::Duplicate, duplicate.outcome);
    assert_eq!(ThreadCallbackInsertState::Delivered, duplicate.state);
    assert!(!duplicate.needs_rehydrate);

    let requests = server.requests().await;
    assert_eq!(2, requests.len());
    let callback_request: Value = serde_json::from_slice(&requests[1])?;
    let model_input = callback_request
        .get("input")
        .and_then(Value::as_array)
        .expect("second model request input");
    let callback_item_types = model_input
        .iter()
        .filter(|item| response_call_id(item) == Some(accepted.call_id.as_str()))
        .filter_map(|item| item.get("type").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        vec!["function_call", "function_call_output"],
        callback_item_types
    );
    assert!(
        !model_input.iter().any(|item| {
            item.get("role").and_then(Value::as_str) == Some("user")
                && response_item_contains(item, "Tell the requester this callback arrived.")
        }),
        "completion callbacks must not become user steering messages"
    );
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn callback_insert_survives_app_server_restart_without_duplicate_model_input() -> Result<()> {
    let (release_first_response, first_response_gate) = oneshot::channel();
    let interrupted_response = vec![
        StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![responses::ev_response_created("resp-1")]),
        },
        StreamingSseChunk {
            gate: Some(first_response_gate),
            body: responses::sse(vec![
                responses::ev_assistant_message("msg-1", "Callback reported"),
                responses::ev_completed("resp-1"),
            ]),
        },
    ];
    let (server, _) = start_streaming_sse_server(vec![interrupted_response]).await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), server.uri())?;

    let thread_id;
    let params;
    {
        let mut first_app_server = TestAppServer::new(codex_home.path()).await?;
        timeout(STARTUP_TIMEOUT, first_app_server.initialize()).await??;
        let thread_request = first_app_server
            .send_thread_start_request(ThreadStartParams {
                model: Some("mock-model".to_string()),
                ..Default::default()
            })
            .await?;
        let thread_response: JSONRPCResponse = timeout(
            DEFAULT_READ_TIMEOUT,
            first_app_server.read_stream_until_response_message(RequestId::Integer(thread_request)),
        )
        .await??;
        let ThreadStartResponse { thread, .. } =
            to_response::<ThreadStartResponse>(thread_response)?;
        thread_id = thread.id;
        params = callback_params(thread_id.clone());

        let insert_request = first_app_server
            .send_thread_callback_insert_request(params.clone())
            .await?;
        let insert_response: JSONRPCResponse = timeout(
            DEFAULT_READ_TIMEOUT,
            first_app_server.read_stream_until_response_message(RequestId::Integer(insert_request)),
        )
        .await??;
        let accepted = to_response::<ThreadCallbackInsertResponse>(insert_response)?;
        assert_eq!(ThreadCallbackInsertOutcome::Accepted, accepted.outcome);
        assert_eq!(ThreadCallbackInsertState::Pending, accepted.state);
        timeout(
            DEFAULT_READ_TIMEOUT,
            server.wait_for_request_count(/* count */ 1),
        )
        .await?;
    }
    let _ = release_first_response.send(());

    let mut restarted_app_server = TestAppServer::new(codex_home.path()).await?;
    timeout(STARTUP_TIMEOUT, restarted_app_server.initialize()).await??;
    let resume_request = restarted_app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id,
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        restarted_app_server.read_stream_until_response_message(RequestId::Integer(resume_request)),
    )
    .await??;

    let retry_request = restarted_app_server
        .send_thread_callback_insert_request(params)
        .await?;
    let retry_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        restarted_app_server.read_stream_until_response_message(RequestId::Integer(retry_request)),
    )
    .await??;
    let recovered = to_response::<ThreadCallbackInsertResponse>(retry_response)?;
    assert_eq!(ThreadCallbackInsertOutcome::Duplicate, recovered.outcome);
    assert_eq!(ThreadCallbackInsertState::Delivered, recovered.state);
    assert!(!recovered.needs_rehydrate);
    assert_eq!(1, server.requests().await.len());
    server.shutdown().await;
    Ok(())
}

fn callback_params(thread_id: String) -> ThreadCallbackInsertParams {
    ThreadCallbackInsertParams {
        delivery_id: "30000000-0000-0000-0000-000000000001".to_string(),
        event_id: "30000000-0000-0000-0000-000000000002".to_string(),
        completion_work_id: "30000000-0000-0000-0000-000000000003".to_string(),
        thread_id,
        source_agent_display_id: "source-agent".to_string(),
        execution_kind: "normal".to_string(),
        execution_id: "turn-1".to_string(),
        terminal_status: "completed".to_string(),
        callback_text: "Tell the requester this callback arrived.".to_string(),
        final_text: "The delegated task finished.".to_string(),
        terminal_at_ms: 1_000,
        payload_digest: "a".repeat(64),
    }
}

fn response_call_id(item: &Value) -> Option<&str> {
    item.get("call_id").and_then(Value::as_str)
}

fn response_item_contains(item: &Value, expected: &str) -> bool {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|content| content.get("text").and_then(Value::as_str))
        .any(|text| text.contains(expected))
}

fn create_config_toml(codex_home: &Path, server_uri: &str) -> std::io::Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )
}
