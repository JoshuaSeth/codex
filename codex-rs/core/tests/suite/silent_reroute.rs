use anyhow::Result;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::SandboxPolicy;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created_with_model;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_reroute_forces_fallback_model_and_effort() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let resp_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created_with_model("resp-1", "gpt-5.2-2025-12-11"),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created_with_model("resp-2", "gpt-5.2-2025-12-11"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let mut builder = test_codex()
        .with_model("gpt-5.2-codex")
        .with_config(|config| {
            // Ensure `reasoning.effort` is sent so we can assert against it.
            config.model_supports_reasoning_summaries = Some(true);
        });
    let test = builder.build(&server).await?;

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::ReadOnly,
            model: test.session_configured.model.clone(),
            effort: None,
            summary: ReasoningSummary::Auto,
            collaboration_mode: None,
            personality: None,
        })
        .await?;

    wait_for_event(
        &test.codex,
        |ev| matches!(ev, EventMsg::Warning(warn) if warn.message.contains("silently rerouted")),
    )
    .await;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    // Simulate a client that keeps trying to use the original model: core should still force
    // the fallback model once reroute has been detected.
    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "second turn".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::ReadOnly,
            model: test.session_configured.model.clone(),
            effort: None,
            summary: ReasoningSummary::Auto,
            collaboration_mode: None,
            personality: None,
        })
        .await?;

    wait_for_event(
        &test.codex,
        |ev| matches!(ev, EventMsg::Warning(warn) if warn.message.contains("silently rerouted")),
    )
    .await;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let requests = resp_mock.requests();
    assert_eq!(requests.len(), 2);

    let first = requests[0].body_json();
    assert_eq!(first["model"], "gpt-5.2-codex");

    let second = requests[1].body_json();
    assert_eq!(second["model"], "gpt-5.2");
    assert_eq!(second["reasoning"]["effort"], "xhigh");

    Ok(())
}
