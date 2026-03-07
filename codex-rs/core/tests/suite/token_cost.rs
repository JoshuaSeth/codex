#![allow(clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn cost_file_path(test: &TestCodex) -> PathBuf {
    test.codex_home_path()
        .join("sessions")
        .join(format!("cost_{}.json", test.session_configured.session_id))
}

fn ev_completed_with_usage(id: &str, input_tokens: i64, output_tokens: i64) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": {
                "input_tokens": input_tokens,
                "input_tokens_details": null,
                "output_tokens": output_tokens,
                "output_tokens_details": null,
                "total_tokens": input_tokens + output_tokens,
            }
        }
    })
}

fn load_cost_file(test: &TestCodex) -> Result<Value> {
    let path = cost_file_path(test);
    let file = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&file)?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_cost_file_records_second_turn_multi_request_breakdown() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "call-token-cost";
    let shell_args = serde_json::to_string(&json!({
        "command": "echo token-cost",
        "timeout_ms": 2_000,
        "login": false,
    }))?;

    let _responses_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-turn-1"),
                ev_assistant_message("msg-turn-1", "seeded history"),
                ev_completed_with_usage("resp-turn-1", 20, 5),
            ]),
            sse(vec![
                ev_response_created("resp-turn-2a"),
                ev_function_call(call_id, "shell_command", &shell_args),
                ev_completed_with_usage("resp-turn-2a", 120, 7),
            ]),
            sse(vec![
                ev_response_created("resp-turn-2b"),
                ev_assistant_message("msg-turn-2", "done"),
                ev_completed_with_usage("resp-turn-2b", 80, 15),
            ]),
        ],
    )
    .await;

    let test = test_codex().with_model("gpt-5.1").build(&server).await?;

    test.submit_turn("seed the session").await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    test.submit_turn("run the command and summarize").await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;

    let cost = load_cost_file(&test)?;

    assert_eq!(cost["sessionId"], json!(test.session_configured.session_id));
    assert_eq!(cost["turns"].as_array().expect("turns array").len(), 2);
    assert_eq!(cost["totals"]["turnCount"], json!(2));
    assert_eq!(cost["totals"]["requestCount"], json!(3));
    assert_eq!(cost["totals"]["reportedUsage"]["totalTokens"], json!(247));

    let second_turn = &cost["turns"][1];
    assert_eq!(second_turn["requestCount"], json!(2));
    assert!(
        second_turn["estimatedInputTokens"]["alreadyInSessionTokens"]
            .as_u64()
            .expect("alreadyInSessionTokens should be positive")
            > 0
    );
    assert!(
        second_turn["estimatedInputTokens"]["newLocalTokens"]
            .as_u64()
            .expect("newLocalTokens should be positive")
            > 0
    );
    assert!(
        second_turn["estimatedInputTokens"]["replayedModelOutputTokens"]
            .as_u64()
            .expect("replayedModelOutputTokens should be positive")
            > 0
    );
    assert_eq!(second_turn["reportedUsage"]["inputTokens"], json!(200));
    assert_eq!(second_turn["reportedUsage"]["outputTokens"], json!(22));
    assert_eq!(second_turn["reportedUsage"]["totalTokens"], json!(222));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_turn_records_estimates_and_explicit_usage_gap() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let _responses_mock = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_response_created("resp-turn-1"),
            ev_assistant_message("msg-turn-1", "history for compact"),
            ev_completed_with_usage("resp-turn-1", 18, 6),
        ])],
    )
    .await;
    let compact_mock =
        responses::mount_compact_user_history_with_summary_once(&server, "compacted summary").await;

    let test = test_codex().with_model("gpt-5.1").build(&server).await?;

    test.submit_turn("make history before compact").await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    assert_eq!(compact_mock.requests().len(), 1);

    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;

    let cost = load_cost_file(&test)?;
    let compact_turn = &cost["turns"][1];
    let errors = compact_turn["errors"]
        .as_array()
        .expect("errors array")
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<Vec<_>>();

    assert_eq!(compact_turn["requestCount"], json!(1));
    assert!(
        compact_turn["estimatedInputTokens"]["totalTokens"]
            .as_u64()
            .expect("compact turn should estimate tokens")
            > 0
    );
    assert_eq!(compact_turn["reportedUsage"]["totalTokens"], json!(0));
    assert!(
        errors
            .iter()
            .any(|error| error.contains("responses/compact does not return token usage"))
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_turn_records_cost_entry_before_shutdown() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let shell_args = serde_json::to_string(&json!({
        "command": "sleep 60",
        "timeout_ms": 60_000,
        "login": false,
    }))?;
    let _responses_mock = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-interrupt"),
            ev_function_call("call-interrupt", "shell_command", &shell_args),
            ev_completed_with_usage("resp-interrupt", 42, 8),
        ]),
    )
    .await;

    let test = test_codex().with_model("gpt-5.1").build(&server).await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "start a long-running command".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| {
        matches!(ev, EventMsg::ExecCommandBegin(_))
    })
    .await;

    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnAborted(_))).await;

    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;

    let cost = load_cost_file(&test)?;
    let interrupted_turn = &cost["turns"][0];
    let errors = interrupted_turn["errors"]
        .as_array()
        .expect("errors array")
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<Vec<_>>();

    assert_eq!(cost["totals"]["turnCount"], json!(1));
    assert_eq!(interrupted_turn["requestCount"], json!(1));
    assert_eq!(interrupted_turn["reportedUsage"]["totalTokens"], json!(50));
    assert!(
        interrupted_turn["estimatedInputTokens"]["totalTokens"]
            .as_u64()
            .expect("interrupted turn should estimate tokens")
            > 0
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("turn aborted: interrupted"))
    );

    Ok(())
}
