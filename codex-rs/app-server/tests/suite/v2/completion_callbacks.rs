use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_state::CompletionOutboxEvent;
use codex_state::StateRuntime;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::timeout;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);
const OUTBOX_LEASE_MS: i64 = 60_000;
const CENTRAL_URL_ENV: &str = "PITCHAI_PLATFORM_CENTRAL_URL";
const CELL_TOKEN_ENV: &str = "PITCHAI_PLATFORM_CELL_TOKEN";
const NORMAL_WORK_ID: &str = "10000000-0000-0000-0000-000000000101";
const GOAL_WORK_ID: &str = "10000000-0000-0000-0000-000000000102";
const CALLBACK_PROTOCOL_VERSION: &str = "pitchai-completion-callback/v1";
const CALLBACK_TEXT: &str = "Report this completion to the Events inbox.";

#[tokio::test]
async fn turn_completion_persists_central_and_webhook_events() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Normal callback evidence").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let mut app_server = app_server_without_completion_sender(codex_home.path()).await?;
    timeout(STARTUP_TIMEOUT, app_server.initialize()).await??;
    let thread_id = start_thread(&mut app_server).await?;

    let turn_request = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.clone(),
            completion_work_id: Some(NORMAL_WORK_ID.to_string()),
            completion_callback_metadata: Some(callback_metadata()),
            input: vec![UserInput::Text {
                text: "Finish this normal callback test.".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(turn_request)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response::<TurnStartResponse>(turn_response)?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let outbox = claim_outbox(codex_home.path()).await?;
    assert_eq!(1, outbox.len());
    let event = &outbox[0];
    assert_eq!(NORMAL_WORK_ID, event.event_id);
    assert_eq!(NORMAL_WORK_ID, event.completion_work_id);
    assert_eq!(thread_id, event.thread_id);
    assert_eq!("normal", event.execution_kind);
    assert_eq!(turn.id, event.execution_id);
    assert_eq!("completed", event.terminal_status);
    assert_eq!("Normal callback evidence", event.final_text);

    let webhook_outbox = claim_webhook_outbox(codex_home.path()).await?;
    assert_eq!(1, webhook_outbox.len());
    let webhook_event = &webhook_outbox[0];
    assert_eq!(NORMAL_WORK_ID, webhook_event.event_id);
    assert_eq!(thread_id, webhook_event.thread_id);
    assert_eq!("normal", webhook_event.execution_kind);
    assert_eq!(turn.id, webhook_event.execution_id);
    assert_eq!(
        callback_metadata(),
        serde_json::from_str::<serde_json::Value>(&webhook_event.callback_metadata_json)?
    );
    Ok(())
}

#[tokio::test]
async fn exact_turn_start_retry_returns_same_turn_without_duplicate_model_work() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("One execution only").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let mut app_server = app_server_without_completion_sender(codex_home.path()).await?;
    timeout(STARTUP_TIMEOUT, app_server.initialize()).await??;
    let thread_id = start_thread(&mut app_server).await?;
    let params = TurnStartParams {
        thread_id,
        completion_work_id: Some(NORMAL_WORK_ID.to_string()),
        input: vec![UserInput::Text {
            text: "Execute this callback-bound turn once.".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    };

    let first_request = app_server.send_turn_start_request(params.clone()).await?;
    let first_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(first_request)),
    )
    .await??;
    let first_turn = to_response::<TurnStartResponse>(first_response)?.turn;

    let retry_request = app_server.send_turn_start_request(params).await?;
    let retry_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(retry_request)),
    )
    .await??;
    let retry_turn = to_response::<TurnStartResponse>(retry_response)?.turn;
    assert_eq!(first_turn.id, retry_turn.id);
    assert_eq!(NORMAL_WORK_ID, first_turn.id);

    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        1,
        requests.len(),
        "an exact retry must not run the model twice"
    );
    assert_eq!(1, claim_outbox(codex_home.path()).await?.len());
    assert!(
        claim_webhook_outbox(codex_home.path()).await?.is_empty(),
        "work without callback metadata must not emit a webhook"
    );
    Ok(())
}

#[tokio::test]
async fn unfinished_callback_turn_resumes_after_app_server_restart_and_emits_once() -> Result<()> {
    let (release_interrupted_response, interrupted_response_gate) = oneshot::channel();
    let interrupted_response = vec![
        StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![responses::ev_response_created("resp-interrupted")]),
        },
        StreamingSseChunk {
            gate: Some(interrupted_response_gate),
            body: responses::sse(vec![
                responses::ev_assistant_message("msg-interrupted", "Interrupted response"),
                responses::ev_completed("resp-interrupted"),
            ]),
        },
    ];
    let recovered_response = vec![StreamingSseChunk {
        gate: None,
        body: responses::sse(vec![
            responses::ev_response_created("resp-recovered"),
            responses::ev_assistant_message("msg-recovered", "Recovered callback evidence"),
            responses::ev_completed("resp-recovered"),
        ]),
    }];
    let (server, _) =
        start_streaming_sse_server(vec![interrupted_response, recovered_response]).await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), server.uri())?;
    let thread_id;
    let turn_params;
    {
        let mut first_app_server = app_server_without_completion_sender(codex_home.path()).await?;
        timeout(STARTUP_TIMEOUT, first_app_server.initialize()).await??;
        thread_id = start_thread(&mut first_app_server).await?;
        turn_params = TurnStartParams {
            thread_id: thread_id.clone(),
            completion_work_id: Some(NORMAL_WORK_ID.to_string()),
            input: vec![UserInput::Text {
                text: "Resume this exact callback work after a process restart.".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        };
        let turn_request = first_app_server
            .send_turn_start_request(turn_params.clone())
            .await?;
        let turn_response: JSONRPCResponse = timeout(
            DEFAULT_READ_TIMEOUT,
            first_app_server.read_stream_until_response_message(RequestId::Integer(turn_request)),
        )
        .await??;
        let first_turn = to_response::<TurnStartResponse>(turn_response)?.turn;
        assert_eq!(NORMAL_WORK_ID, first_turn.id);
        timeout(
            DEFAULT_READ_TIMEOUT,
            server.wait_for_request_count(/* count */ 1),
        )
        .await?;
    }
    let _ = release_interrupted_response.send(());

    let mut restarted_app_server = app_server_without_completion_sender(codex_home.path()).await?;
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
        .send_turn_start_request(turn_params)
        .await?;
    let retry_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        restarted_app_server.read_stream_until_response_message(RequestId::Integer(retry_request)),
    )
    .await??;
    let recovered_turn = to_response::<TurnStartResponse>(retry_response)?.turn;
    assert_eq!(NORMAL_WORK_ID, recovered_turn.id);
    timeout(
        DEFAULT_READ_TIMEOUT,
        restarted_app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let outbox = claim_outbox(codex_home.path()).await?;
    assert_eq!(1, outbox.len());
    assert_eq!("Recovered callback evidence", outbox[0].final_text);
    assert_eq!(2, server.requests().await.len());
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn goal_callback_waits_for_persisted_terminal_goal_state() -> Result<()> {
    let complete_goal_arguments = serde_json::to_string(&json!({"status": "complete"}))?;
    let (release_terminal_update, terminal_update_gate) = oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_assistant_message("goal-intermediate", "Intermediate goal turn"),
                responses::ev_completed("goal-intermediate-response"),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: Some(terminal_update_gate),
            body: responses::sse(vec![
                responses::ev_function_call(
                    "goal-complete-call",
                    "update_goal",
                    &complete_goal_arguments,
                ),
                responses::ev_completed("goal-complete-tool-response"),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_assistant_message(
                    "goal-terminal-final",
                    "Terminal goal final evidence",
                ),
                responses::ev_completed("goal-terminal-final-response"),
            ]),
        }],
    ])
    .await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), server.uri())?;
    let mut app_server = app_server_without_completion_sender(codex_home.path()).await?;
    timeout(STARTUP_TIMEOUT, app_server.initialize()).await??;
    let thread_id = start_thread(&mut app_server).await?;

    let goal_request = app_server
        .send_raw_request(
            "thread/goal/set",
            Some(json!({
                "threadId": thread_id,
                "completionWorkId": GOAL_WORK_ID,
                "completionCallbackMetadata": callback_metadata(),
                "objective": "Complete only after explicit persisted goal success.",
                "status": "active",
            })),
        )
        .await?;
    let goal_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(goal_request)),
    )
    .await??;
    let active_goal = to_response::<ThreadGoalSetResponse>(goal_response)?;
    assert_eq!(ThreadGoalStatus::Active, active_goal.goal.status);
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("thread/goal/updated"),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        server.wait_for_request_count(/* count */ 2),
    )
    .await?;

    let intermediate_outbox = claim_outbox(codex_home.path()).await?;
    assert!(
        intermediate_outbox.is_empty(),
        "an ordinary assistant final must not finish a goal callback"
    );
    assert!(
        claim_webhook_outbox(codex_home.path()).await?.is_empty(),
        "an ordinary assistant final must not emit a goal webhook"
    );

    release_terminal_update
        .send(())
        .expect("terminal goal response gate should still be open");
    let goal_updated = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("thread/goal/updated"),
    )
    .await??;
    let terminal_turn_id = goal_updated
        .params
        .as_ref()
        .and_then(|params| params.get("turnId"))
        .and_then(serde_json::Value::as_str)
        .expect("terminal goal update should identify its emitting turn")
        .to_string();
    assert!(!terminal_turn_id.is_empty());
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let terminal_outbox = claim_outbox(codex_home.path()).await?;
    assert_eq!(1, terminal_outbox.len());
    let event = &terminal_outbox[0];
    assert_eq!(GOAL_WORK_ID, event.event_id);
    assert_eq!(GOAL_WORK_ID, event.completion_work_id);
    assert_eq!(active_goal.goal.thread_id, event.thread_id);
    assert_eq!("goal", event.execution_kind);
    assert!(!event.execution_id.is_empty());
    assert_eq!("complete", event.terminal_status);
    assert_eq!("Terminal goal final evidence", event.final_text);
    assert_ne!(terminal_turn_id, event.execution_id);

    let webhook_outbox = claim_webhook_outbox(codex_home.path()).await?;
    assert_eq!(1, webhook_outbox.len());
    let webhook_event = &webhook_outbox[0];
    assert_eq!(GOAL_WORK_ID, webhook_event.event_id);
    assert_eq!("goal", webhook_event.execution_kind);
    assert_eq!("complete", webhook_event.terminal_status);
    assert_eq!("Terminal goal final evidence", webhook_event.final_text);
    assert_eq!(
        callback_metadata(),
        serde_json::from_str::<serde_json::Value>(&webhook_event.callback_metadata_json)?
    );

    let duplicate_outbox = claim_outbox(codex_home.path()).await?;
    assert!(
        duplicate_outbox.is_empty(),
        "the terminal goal transition must emit exactly once"
    );
    assert_eq!(3, server.requests().await.len());
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn exact_goal_set_retry_does_not_repeat_goal_effects() -> Result<()> {
    let server =
        create_mock_responses_server_repeating_assistant("Unexpected goal execution").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let mut app_server = app_server_without_completion_sender(codex_home.path()).await?;
    timeout(STARTUP_TIMEOUT, app_server.initialize()).await??;
    let thread_id = start_thread(&mut app_server).await?;
    let params = json!({
        "threadId": thread_id,
        "completionWorkId": GOAL_WORK_ID,
        "objective": "Execute this callback-bound goal once.",
        "status": "paused",
    });

    let first_request = app_server
        .send_raw_request("thread/goal/set", Some(params.clone()))
        .await?;
    let first_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(first_request)),
    )
    .await??;
    let first_goal = to_response::<ThreadGoalSetResponse>(first_response)?.goal;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("thread/goal/updated"),
    )
    .await??;

    let retry_request = app_server
        .send_raw_request("thread/goal/set", Some(params))
        .await?;
    let retry_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(retry_request)),
    )
    .await??;
    let retry_goal = to_response::<ThreadGoalSetResponse>(retry_response)?.goal;
    assert_eq!(first_goal.thread_id, retry_goal.thread_id);
    assert_eq!(first_goal.created_at, retry_goal.created_at);
    assert_eq!(first_goal.updated_at, retry_goal.updated_at);
    assert!(
        timeout(
            Duration::from_millis(250),
            app_server.read_stream_until_notification_message("thread/goal/updated"),
        )
        .await
        .is_err(),
        "an exact goal retry must not emit another goal update"
    );

    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        0,
        requests.len(),
        "a paused goal and its exact retry must not run the model"
    );

    let complete_request = app_server
        .send_raw_request(
            "thread/goal/set",
            Some(json!({
                "threadId": retry_goal.thread_id,
                "status": "complete",
            })),
        )
        .await?;
    let complete_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(complete_request)),
    )
    .await??;
    let complete_goal = to_response::<ThreadGoalSetResponse>(complete_response)?;
    assert_eq!(ThreadGoalStatus::Complete, complete_goal.goal.status);
    let state_db =
        StateRuntime::init(codex_home.path().to_path_buf(), "mock_provider".to_string()).await?;
    assert_eq!(
        1,
        state_db.completions().outbox_stats().await?.pending_count
    );
    assert!(
        claim_outbox(codex_home.path()).await?.is_empty(),
        "a goal completed outside an agent turn must retain its final-capture grace period"
    );
    Ok(())
}

async fn app_server_without_completion_sender(codex_home: &Path) -> Result<TestAppServer> {
    TestAppServer::new_without_managed_config_with_env(
        codex_home,
        &[(CENTRAL_URL_ENV, None), (CELL_TOKEN_ENV, None)],
    )
    .await
}

async fn start_thread(app_server: &mut TestAppServer) -> Result<String> {
    let request = app_server
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(request)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(response)?;
    Ok(thread.id)
}

async fn claim_outbox(codex_home: &Path) -> Result<Vec<CompletionOutboxEvent>> {
    let state_db =
        StateRuntime::init(codex_home.to_path_buf(), "mock_provider".to_string()).await?;
    let events = state_db
        .completions()
        .claim_outbox(/* limit */ 10, OUTBOX_LEASE_MS)
        .await?;
    Ok(events)
}

async fn claim_webhook_outbox(codex_home: &Path) -> Result<Vec<CompletionOutboxEvent>> {
    let state_db =
        StateRuntime::init(codex_home.to_path_buf(), "mock_provider".to_string()).await?;
    let events = state_db
        .completions()
        .claim_webhook_outbox(/* limit */ 10, OUTBOX_LEASE_MS)
        .await?;
    Ok(events)
}

fn callback_metadata() -> serde_json::Value {
    json!({
        "protocol_version": CALLBACK_PROTOCOL_VERSION,
        "text": CALLBACK_TEXT,
    })
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

[features]
goals = true

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
