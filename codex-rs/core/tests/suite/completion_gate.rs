use anyhow::Result;
use codex_core::config::CompletionGateConfig;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

fn completion_gate_config(criteria: &str) -> CompletionGateConfig {
    CompletionGateConfig {
        criteria: criteria.to_string(),
        judge_model: None,
        judge_base_url: None,
        judge_api_key_env: None,
        timeout_ms: CompletionGateConfig::DEFAULT_TIMEOUT_MS,
        max_retries: CompletionGateConfig::DEFAULT_MAX_RETRIES,
        max_assistant_messages: CompletionGateConfig::DEFAULT_MAX_ASSISTANT_MESSAGES,
        max_user_messages: CompletionGateConfig::DEFAULT_MAX_USER_MESSAGES,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_gate_allows_candidate_stop_and_sends_judge_context() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let request_log = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-main-1"),
                ev_assistant_message("msg-main-1", "Implemented the change and verified it."),
                ev_completed("resp-main-1"),
            ]),
            sse(vec![
                ev_response_created("resp-judge-1"),
                ev_assistant_message(
                    "msg-judge-1",
                    &json!({
                        "allow_stop": true,
                        "reason": "The requested implementation and verification are present.",
                        "missing_requirements": [],
                        "continue_prompt": "",
                        "evidence": [
                            "The candidate final response says the change was implemented.",
                            "The candidate final response says it was verified."
                        ]
                    })
                    .to_string(),
                ),
                ev_completed("resp-judge-1"),
            ]),
        ],
    )
    .await;

    let criteria = "Stop only after the requested change is implemented and explicitly verified.";
    let test = test_codex()
        .with_config({
            let criteria = criteria.to_string();
            move |config| {
                config.completion_gate = Some(completion_gate_config(&criteria));
            }
        })
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "Ship the change and verify it.".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: test.session_configured.model.clone(),
            effort: test.config.model_reasoning_effort,
            summary: None,
            service_tier: None,
            collaboration_mode: None,
            personality: None,
        })
        .await?;

    let mut saw_started = false;
    let mut saw_allow = false;
    let turn_complete = loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::CompletionGateStarted(event) => {
                saw_started = true;
                assert_eq!(
                    event.thread_id,
                    test.session_configured.session_id.to_string()
                );
                assert_eq!(event.judge_model, test.session_configured.model);
            }
            EventMsg::CompletionGateDecision(event) => {
                assert!(event.allow_stop);
                assert_eq!(
                    event.reason,
                    "The requested implementation and verification are present."
                );
                saw_allow = true;
            }
            EventMsg::TurnComplete(event) => break event,
            _ => {}
        }
    };

    assert!(saw_started, "expected completion gate to start");
    assert!(saw_allow, "expected completion gate to allow stop");
    assert_eq!(
        turn_complete.last_agent_message.as_deref(),
        Some("Implemented the change and verified it.")
    );

    let requests = request_log.requests();
    assert_eq!(requests.len(), 2);
    let judge_request = &requests[1];
    assert!(
        judge_request
            .instructions_text()
            .contains("You are the completion gate")
    );
    assert!(judge_request.body_contains_text(criteria));
    assert!(judge_request.body_contains_text("Ship the change and verify it."));
    assert!(judge_request.body_contains_text("Implemented the change and verified it."));
    assert!(judge_request.body_contains_text("<candidate_final_response>"));
    assert_eq!(
        judge_request.body_json()["reasoning"]["effort"].as_str(),
        Some("low")
    );
    let judge_body = judge_request.body_json().to_string();
    assert!(judge_body.contains("\"allow_stop\""));
    assert!(judge_body.contains("\"missing_requirements\""));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_gate_denial_injects_follow_up_and_rejudges() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let request_log = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-main-1"),
                ev_assistant_message("msg-main-1", "I think this is done."),
                ev_completed("resp-main-1"),
            ]),
            sse(vec![
                ev_response_created("resp-judge-1"),
                ev_assistant_message(
                    "msg-judge-1",
                    &json!({
                        "allow_stop": false,
                        "reason": "The response does not mention verification.",
                        "missing_requirements": ["Explicit verification is still missing"],
                        "continue_prompt": "Run the requested verification and summarize the result before stopping.",
                        "evidence": ["The candidate response only says it is done."]
                    })
                    .to_string(),
                ),
                ev_completed("resp-judge-1"),
            ]),
            sse(vec![
                ev_response_created("resp-main-2"),
                ev_assistant_message("msg-main-2", "Verified the result and it passes."),
                ev_completed("resp-main-2"),
            ]),
            sse(vec![
                ev_response_created("resp-judge-2"),
                ev_assistant_message(
                    "msg-judge-2",
                    &json!({
                        "allow_stop": true,
                        "reason": "The response now includes the requested verification.",
                        "missing_requirements": [],
                        "continue_prompt": "",
                        "evidence": ["The candidate response now mentions verification."]
                    })
                    .to_string(),
                ),
                ev_completed("resp-judge-2"),
            ]),
        ],
    )
    .await;

    let criteria = "Stop only after the agent verifies the result.";
    let test = test_codex()
        .with_config({
            let criteria = criteria.to_string();
            move |config| {
                config.completion_gate = Some(completion_gate_config(&criteria));
            }
        })
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "Finish the task and verify the outcome.".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: test.session_configured.model.clone(),
            effort: test.config.model_reasoning_effort,
            summary: None,
            service_tier: None,
            collaboration_mode: None,
            personality: None,
        })
        .await?;

    let mut blocked_stop = None;
    let mut deny_reason = None;
    let mut allow_reason = None;
    let turn_complete = loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::CompletionGateDecision(event) if !event.allow_stop => {
                deny_reason = Some(event.reason);
            }
            EventMsg::CompletionGateBlockedStop(event) => {
                blocked_stop = Some(event);
            }
            EventMsg::CompletionGateDecision(event) if event.allow_stop => {
                allow_reason = Some(event.reason);
            }
            EventMsg::TurnComplete(event) => break event,
            _ => {}
        }
    };

    let blocked_stop = blocked_stop.expect("expected blocked-stop event");
    assert_eq!(
        deny_reason.as_deref(),
        Some("The response does not mention verification.")
    );
    assert!(
        blocked_stop
            .reason
            .contains("Explicit verification is still missing")
    );
    assert_eq!(
        blocked_stop.continue_prompt,
        "Run the requested verification and summarize the result before stopping."
    );
    assert_eq!(
        allow_reason.as_deref(),
        Some("The response now includes the requested verification.")
    );
    assert_eq!(
        turn_complete.last_agent_message.as_deref(),
        Some("Verified the result and it passes.")
    );

    let requests = request_log.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2].body_contains_text("<completion_gate_feedback>"),
        "expected denial continuation to be re-injected as contextual user input"
    );
    assert!(
        requests[2].body_contains_text(
            "Run the requested verification and summarize the result before stopping."
        ),
        "expected injected continuation prompt in follow-up request"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_gate_fail_closed_keeps_turn_running() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let request_log = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-main-1"),
                ev_assistant_message("msg-main-1", "Done."),
                ev_completed("resp-main-1"),
            ]),
            sse(vec![
                ev_response_created("resp-judge-1"),
                ev_assistant_message("msg-judge-1", "not valid json"),
                ev_completed("resp-judge-1"),
            ]),
            sse(vec![
                ev_response_created("resp-main-2"),
                ev_assistant_message("msg-main-2", "Done after retry and verification."),
                ev_completed("resp-main-2"),
            ]),
            sse(vec![
                ev_response_created("resp-judge-2"),
                ev_assistant_message(
                    "msg-judge-2",
                    &json!({
                        "allow_stop": true,
                        "reason": "The retry response now clearly satisfies the criterion.",
                        "missing_requirements": [],
                        "continue_prompt": "",
                        "evidence": ["The candidate retry response is explicit."]
                    })
                    .to_string(),
                ),
                ev_completed("resp-judge-2"),
            ]),
        ],
    )
    .await;

    let criteria = "Stop only when the answer explicitly states the verification result.";
    let test = test_codex()
        .with_config({
            let criteria = criteria.to_string();
            move |config| {
                config.completion_gate = Some(completion_gate_config(&criteria));
            }
        })
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "Finish this and verify it.".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: test.session_configured.model.clone(),
            effort: test.config.model_reasoning_effort,
            summary: None,
            service_tier: None,
            collaboration_mode: None,
            personality: None,
        })
        .await?;

    let mut gate_error = None;
    let turn_complete = loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::CompletionGateError(event) => gate_error = Some(event),
            EventMsg::TurnComplete(event) => break event,
            _ => {}
        }
    };

    let gate_error = gate_error.expect("expected completion gate error");
    assert!(
        gate_error
            .message
            .contains("completion gate judge returned invalid JSON"),
        "unexpected gate error message: {}",
        gate_error.message
    );
    assert_eq!(
        turn_complete.last_agent_message.as_deref(),
        Some("Done after retry and verification.")
    );

    let requests = request_log.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2].body_contains_text("completion gate failed closed"),
        "expected fail-closed continuation reason to be injected"
    );
    assert!(
        requests[2].body_contains_text(
            "Completion gate error: completion gate judge returned invalid JSON"
        ),
        "expected fail-closed continuation to surface the judge error"
    );

    Ok(())
}
