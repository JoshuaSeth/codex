//! End-to-end compaction flow tests.
//!
//! Phases:
//! 1) Arrange: mock responses/compact endpoints + config.
//! 2) Act: start a thread and submit multiple turns to trigger auto-compaction.
//! 3) Assert: verify item/started + item/completed notifications for context compaction.

use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use app_test_support::write_chatgpt_auth;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadCompactStartParams;
use codex_app_server_protocol::ThreadCompactStartResponse;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_config::types::AuthCredentialsStoreMode;
use codex_features::Feature;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use tempfile::TempDir;
use tokio::time::timeout;

// macOS and Windows Bazel CI can spend tens of seconds starting app-server
// subprocesses or processing test RPCs under load.
#[cfg(any(target_os = "macos", windows))]
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(not(any(target_os = "macos", windows)))]
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const AUTO_COMPACT_LIMIT: i64 = 1_000;
const COMPACT_PROMPT: &str = "Summarize the conversation.";
const INVALID_REQUEST_ERROR_CODE: i64 = -32600;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_compaction_local_emits_started_and_completed_items() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let sse1 = responses::sse(vec![
        responses::ev_assistant_message("m1", "FIRST_REPLY"),
        responses::ev_completed_with_tokens("r1", /*total_tokens*/ 70_000),
    ]);
    let sse2 = responses::sse(vec![
        responses::ev_assistant_message("m2", "SECOND_REPLY"),
        responses::ev_completed_with_tokens("r2", /*total_tokens*/ 330_000),
    ]);
    let sse3 = responses::sse(vec![
        responses::ev_assistant_message("m3", "LOCAL_SUMMARY"),
        responses::ev_completed_with_tokens("r3", /*total_tokens*/ 200),
    ]);
    let sse4 = responses::sse(vec![
        responses::ev_assistant_message("m4", "FINAL_REPLY"),
        responses::ev_completed_with_tokens("r4", /*total_tokens*/ 120),
    ]);
    responses::mount_sse_sequence(&server, vec![sse1, sse2, sse3, sse4]).await;

    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &BTreeMap::default(),
        AUTO_COMPACT_LIMIT,
        /*requires_openai_auth*/ None,
        "mock_provider",
        COMPACT_PROMPT,
    )?;

    let mut mcp = TestAppServer::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_id = start_thread(&mut mcp).await?;
    for message in ["first", "second", "third"] {
        send_turn_and_wait(&mut mcp, &thread_id, message).await?;
    }

    let started = wait_for_context_compaction_started(&mut mcp).await?;
    let completed = wait_for_context_compaction_completed(&mut mcp).await?;

    let ThreadItem::ContextCompaction { id: started_id } = started.item else {
        unreachable!("started item should be context compaction");
    };
    let ThreadItem::ContextCompaction { id: completed_id } = completed.item else {
        unreachable!("completed item should be context compaction");
    };

    assert_eq!(started.thread_id, thread_id);
    assert_eq!(completed.thread_id, thread_id);
    assert_eq!(started_id, completed_id);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_compaction_remote_emits_started_and_completed_items() -> Result<()> {
    skip_if_no_network!(Ok(()));
    const REMOTE_AUTO_COMPACT_LIMIT: i64 = 200_000;

    let server = responses::start_mock_server().await;
    let sse1 = responses::sse(vec![
        responses::ev_assistant_message("m1", "FIRST_REPLY"),
        responses::ev_completed_with_tokens("r1", /*total_tokens*/ 70_000),
    ]);
    let sse2 = responses::sse(vec![
        responses::ev_assistant_message("m2", "SECOND_REPLY"),
        responses::ev_completed_with_tokens("r2", /*total_tokens*/ 330_000),
    ]);
    let sse3 = responses::sse(vec![
        responses::ev_assistant_message("m3", "FINAL_REPLY"),
        responses::ev_completed_with_tokens("r3", /*total_tokens*/ 120),
    ]);
    let responses_log = responses::mount_sse_sequence(&server, vec![sse1, sse2, sse3]).await;

    let compacted_history = vec![
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "REMOTE_COMPACT_SUMMARY".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "ENCRYPTED_COMPACTION_SUMMARY".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let compact_mock = responses::mount_compact_json_once(
        &server,
        serde_json::json!({ "output": compacted_history }),
    )
    .await;

    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &BTreeMap::from([(Feature::RemoteCompactionV2, false)]),
        REMOTE_AUTO_COMPACT_LIMIT,
        Some(true),
        "mock_provider",
        COMPACT_PROMPT,
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("access-chatgpt").plan_type("pro"),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp =
        TestAppServer::new_with_env(codex_home.path(), &[("OPENAI_API_KEY", None)]).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_id = start_thread(&mut mcp).await?;
    for message in ["first", "second", "third"] {
        send_turn_and_wait(&mut mcp, &thread_id, message).await?;
    }

    let started = wait_for_context_compaction_started(&mut mcp).await?;
    let completed = wait_for_context_compaction_completed(&mut mcp).await?;

    let ThreadItem::ContextCompaction { id: started_id } = started.item else {
        unreachable!("started item should be context compaction");
    };
    let ThreadItem::ContextCompaction { id: completed_id } = completed.item else {
        unreachable!("completed item should be context compaction");
    };

    assert_eq!(started.thread_id, thread_id);
    assert_eq!(completed.thread_id, thread_id);
    assert_eq!(started_id, completed_id);

    let compact_requests = compact_mock.requests();
    assert_eq!(compact_requests.len(), 1);
    assert_eq!(compact_requests[0].path(), "/v1/responses/compact");

    let response_requests = responses_log.requests();
    assert_eq!(response_requests.len(), 3);
    let turn_metadata = response_requests
        .iter()
        .map(|request| {
            request
                .header("x-codex-turn-metadata")
                .as_deref()
                .map(parse_json_header)
                .expect("turn request should include turn metadata")
        })
        .collect::<Vec<_>>();
    for (request, metadata) in response_requests.iter().zip(&turn_metadata) {
        assert_eq!(metadata["request_kind"].as_str(), Some("turn"));
        assert!(
            metadata["turn_id"]
                .as_str()
                .is_some_and(|turn_id| !turn_id.is_empty()),
            "turn request should carry a non-empty turn id"
        );
        assert_eq!(
            metadata["window_id"].as_str(),
            request.header("x-codex-window-id").as_deref()
        );
        assert!(metadata.get("compaction").is_none());
    }

    let compact_metadata = compact_requests[0]
        .header("x-codex-turn-metadata")
        .as_deref()
        .map(parse_json_header)
        .expect("compact request should include turn metadata");
    assert_eq!(
        compact_metadata["request_kind"].as_str(),
        Some("compaction")
    );
    assert_eq!(
        compact_metadata["compaction"],
        serde_json::json!({
            "trigger": "auto",
            "reason": "context_limit",
            "implementation": "responses_compact",
            "phase": "pre_turn",
            "strategy": "memento",
        })
    );
    assert_eq!(
        compact_metadata["turn_id"], turn_metadata[2]["turn_id"],
        "pre-turn compaction should carry the current turn id"
    );
    assert_eq!(
        compact_metadata["window_id"].as_str(),
        compact_requests[0].header("x-codex-window-id").as_deref()
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_mid_turn_compaction_preserves_current_goal_after_stale_user_instruction()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    run_goal_compaction_preservation_test(GoalCompactionImplementation::Local).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_legacy_mid_turn_compaction_preserves_current_goal_after_stale_user_instruction()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    run_goal_compaction_preservation_test(GoalCompactionImplementation::RemoteLegacy).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_v2_mid_turn_compaction_preserves_current_goal_after_stale_user_instruction()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    run_goal_compaction_preservation_test(GoalCompactionImplementation::RemoteV2).await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GoalCompactionImplementation {
    Local,
    RemoteLegacy,
    RemoteV2,
}

async fn run_goal_compaction_preservation_test(
    implementation: GoalCompactionImplementation,
) -> Result<()> {
    const CURRENT_GOAL: &str = "RESUME_THE_CAMPAIGN_CURRENT";
    const STALE_USER_INSTRUCTION: &str = "PAUSE_THE_CAMPAIGN_OBSOLETE";

    let server = responses::start_mock_server().await;
    let shell_arguments = serde_json::to_string(&serde_json::json!({
        "cmd": "printf goal-compaction-test",
        "yield_time_ms": 500,
    }))?;
    let complete_goal_arguments = serde_json::to_string(&serde_json::json!({
        "status": "complete",
    }))?;
    let mut response_bodies = vec![
        responses::sse(vec![
            responses::ev_assistant_message("stale-turn-message", "Paused."),
            responses::ev_completed_with_tokens("stale-turn-response", 50),
        ]),
        responses::sse(vec![
            responses::ev_function_call("goal-tool-call", "exec_command", &shell_arguments),
            responses::ev_completed_with_tokens("goal-tool-response", 500),
        ]),
    ];
    match implementation {
        GoalCompactionImplementation::Local => response_bodies.push(responses::sse(vec![
            responses::ev_assistant_message(
                "local-goal-summary-message",
                "LOCAL_GOAL_COMPACTION_SUMMARY",
            ),
            responses::ev_completed_with_tokens("local-goal-summary-response", 20),
        ])),
        GoalCompactionImplementation::RemoteLegacy => {
            responses::mount_compact_user_history_with_summary_once(
                &server,
                "LEGACY_GOAL_COMPACTION_CHECKPOINT",
            )
            .await;
        }
        GoalCompactionImplementation::RemoteV2 => response_bodies.push(responses::sse(vec![
            serde_json::json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "compaction",
                    "encrypted_content": "GOAL_COMPACTION_CHECKPOINT",
                },
            }),
            responses::ev_completed("goal-compaction-response"),
        ])),
    }
    response_bodies.extend([
        responses::sse(vec![
            responses::ev_function_call(
                "goal-complete-call",
                "update_goal",
                &complete_goal_arguments,
            ),
            responses::ev_completed_with_tokens("goal-complete-response", 80),
        ]),
        responses::sse(vec![
            responses::ev_assistant_message("goal-final-message", "Goal complete."),
            responses::ev_completed_with_tokens("goal-final-response", 80),
        ]),
    ]);
    let responses_log = responses::mount_sse_sequence(&server, response_bodies).await;

    let codex_home = TempDir::new()?;
    let mut feature_flags = BTreeMap::from([(Feature::Goals, true)]);
    let requires_openai_auth = match implementation {
        GoalCompactionImplementation::Local => None,
        GoalCompactionImplementation::RemoteLegacy => {
            feature_flags.insert(Feature::RemoteCompactionV2, false);
            Some(true)
        }
        GoalCompactionImplementation::RemoteV2 => {
            feature_flags.insert(Feature::RemoteCompactionV2, true);
            Some(true)
        }
    };
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &feature_flags,
        /*auto_compact_limit*/ 200,
        requires_openai_auth,
        "mock_provider",
        COMPACT_PROMPT,
    )?;
    if requires_openai_auth.is_some() {
        write_chatgpt_auth(
            codex_home.path(),
            ChatGptAuthFixture::new("access-chatgpt").plan_type("pro"),
            AuthCredentialsStoreMode::File,
        )?;
    }

    let mut mcp = if requires_openai_auth.is_some() {
        TestAppServer::new_with_env(codex_home.path(), &[("OPENAI_API_KEY", None)]).await?
    } else {
        TestAppServer::new(codex_home.path()).await?
    };
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_id = start_thread(&mut mcp).await?;
    send_turn_and_wait(&mut mcp, &thread_id, STALE_USER_INSTRUCTION).await?;

    let goal_request_id = mcp
        .send_raw_request(
            "thread/goal/set",
            Some(serde_json::json!({
                "threadId": thread_id,
                "objective": CURRENT_GOAL,
            })),
        )
        .await?;
    let goal_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(goal_request_id)),
    )
    .await??;
    let goal: ThreadGoalSetResponse = to_response(goal_response)?;
    assert_eq!(goal.goal.status, ThreadGoalStatus::Active);
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let requests = responses_log.requests();
    let (expected_request_count, post_compaction_request_index) = match implementation {
        GoalCompactionImplementation::Local | GoalCompactionImplementation::RemoteV2 => (5, 3),
        GoalCompactionImplementation::RemoteLegacy => (4, 2),
    };
    assert_eq!(requests.len(), expected_request_count);
    let post_compaction_body = requests[post_compaction_request_index].body_json();
    let post_compaction_input = post_compaction_body
        .get("input")
        .and_then(serde_json::Value::as_array)
        .expect("post-compaction request should contain input");
    let stale_index = input_item_text_index(post_compaction_input, STALE_USER_INSTRUCTION)
        .expect("stale user instruction should remain as historical context");
    let current_goal_indices = input_item_text_indices(post_compaction_input, CURRENT_GOAL);
    let compaction_index = compaction_boundary_index(post_compaction_input, implementation)
        .expect("post-compaction request should contain the compaction checkpoint");

    assert_eq!(current_goal_indices.len(), 1);
    assert!(
        stale_index < current_goal_indices[0],
        "current goal must supersede the stale user instruction by appearing later"
    );
    assert!(
        current_goal_indices[0] < compaction_index,
        "current goal must be retained immediately before the compaction checkpoint"
    );
    assert_eq!(current_goal_indices[0] + 1, compaction_index);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_compact_start_triggers_compaction_and_returns_empty_response() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let sse = responses::sse(vec![
        responses::ev_assistant_message("m1", "MANUAL_COMPACT_SUMMARY"),
        responses::ev_completed_with_tokens("r1", /*total_tokens*/ 200),
    ]);
    responses::mount_sse_sequence(&server, vec![sse]).await;

    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &BTreeMap::default(),
        AUTO_COMPACT_LIMIT,
        /*requires_openai_auth*/ None,
        "mock_provider",
        COMPACT_PROMPT,
    )?;

    let mut mcp = TestAppServer::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_id = start_thread(&mut mcp).await?;
    let compact_id = mcp
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id: thread_id.clone(),
        })
        .await?;
    let compact_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(compact_id)),
    )
    .await??;
    let _compact: ThreadCompactStartResponse =
        to_response::<ThreadCompactStartResponse>(compact_resp)?;

    let started = wait_for_context_compaction_started(&mut mcp).await?;
    let completed = wait_for_context_compaction_completed(&mut mcp).await?;

    let ThreadItem::ContextCompaction { id: started_id } = started.item else {
        unreachable!("started item should be context compaction");
    };
    let ThreadItem::ContextCompaction { id: completed_id } = completed.item else {
        unreachable!("completed item should be context compaction");
    };

    assert_eq!(started.thread_id, thread_id);
    assert_eq!(completed.thread_id, thread_id);
    assert_eq!(started_id, completed_id);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_compact_start_rejects_invalid_thread_id() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &BTreeMap::default(),
        AUTO_COMPACT_LIMIT,
        /*requires_openai_auth*/ None,
        "mock_provider",
        COMPACT_PROMPT,
    )?;

    let mut mcp = TestAppServer::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id: "not-a-thread-id".to_string(),
        })
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(error.error.code, INVALID_REQUEST_ERROR_CODE);
    assert!(error.error.message.contains("invalid thread id"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_compact_start_rejects_unknown_thread_id() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &BTreeMap::default(),
        AUTO_COMPACT_LIMIT,
        /*requires_openai_auth*/ None,
        "mock_provider",
        COMPACT_PROMPT,
    )?;

    let mut mcp = TestAppServer::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id: "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string(),
        })
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(error.error.code, INVALID_REQUEST_ERROR_CODE);
    assert!(error.error.message.contains("thread not found"));

    Ok(())
}

async fn start_thread(mcp: &mut TestAppServer) -> Result<String> {
    let thread_id = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;
    Ok(thread.id)
}

async fn send_turn_and_wait(
    mcp: &mut TestAppServer,
    thread_id: &str,
    text: &str,
) -> Result<String> {
    let turn_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.to_string(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_id)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response::<TurnStartResponse>(turn_resp)?;
    wait_for_turn_completed(mcp, &turn.id).await?;
    Ok(turn.id)
}

async fn wait_for_turn_completed(mcp: &mut TestAppServer, turn_id: &str) -> Result<()> {
    loop {
        let notification: JSONRPCNotification = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_notification_message("turn/completed"),
        )
        .await??;
        let completed: TurnCompletedNotification =
            serde_json::from_value(notification.params.clone().expect("turn/completed params"))?;
        if completed.turn.id == turn_id {
            return Ok(());
        }
    }
}

async fn wait_for_context_compaction_started(
    mcp: &mut TestAppServer,
) -> Result<ItemStartedNotification> {
    loop {
        let notification: JSONRPCNotification = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_notification_message("item/started"),
        )
        .await??;
        let started: ItemStartedNotification =
            serde_json::from_value(notification.params.clone().expect("item/started params"))?;
        if let ThreadItem::ContextCompaction { .. } = started.item {
            return Ok(started);
        }
    }
}

async fn wait_for_context_compaction_completed(
    mcp: &mut TestAppServer,
) -> Result<ItemCompletedNotification> {
    loop {
        let notification: JSONRPCNotification = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_notification_message("item/completed"),
        )
        .await??;
        let completed: ItemCompletedNotification =
            serde_json::from_value(notification.params.clone().expect("item/completed params"))?;
        if let ThreadItem::ContextCompaction { .. } = completed.item {
            return Ok(completed);
        }
    }
}

fn parse_json_header(value: &str) -> serde_json::Value {
    serde_json::from_str(value).expect("turn metadata should be JSON")
}

fn input_item_text_index(items: &[serde_json::Value], expected: &str) -> Option<usize> {
    input_item_text_indices(items, expected).into_iter().next()
}

fn input_item_text_indices(items: &[serde_json::Value], expected: &str) -> Vec<usize> {
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| input_item_contains_text(item, expected).then_some(index))
        .collect()
}

fn compaction_boundary_index(
    items: &[serde_json::Value],
    implementation: GoalCompactionImplementation,
) -> Option<usize> {
    items.iter().position(|item| match implementation {
        GoalCompactionImplementation::Local => {
            input_item_contains_text(item, "LOCAL_GOAL_COMPACTION_SUMMARY")
        }
        GoalCompactionImplementation::RemoteLegacy | GoalCompactionImplementation::RemoteV2 => {
            item.get("type").and_then(serde_json::Value::as_str) == Some("compaction")
        }
    })
}

fn input_item_contains_text(item: &serde_json::Value, expected: &str) -> bool {
    item.get("content")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|content| {
            content.iter().any(|part| {
                part.get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|text| text.contains(expected))
            })
        })
}
