#![cfg(not(target_os = "windows"))]

use anyhow::Result;
use core_test_support::assert_regex_match;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodexHarness;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::fs;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_and_stop_hooks_run_and_tool_hook_can_override_timeout() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const TOOL_LOG: &str = "tool_hook_events.jsonl";
    const STOP_LOG: &str = "stop_hook_event.json";
    const SCRIPT: &str = "hook_recorder.py";

    let harness = TestCodexHarness::with_config(|config| {
        let script_path = config.cwd.join(SCRIPT);
        let tool_log_path = config.cwd.join(TOOL_LOG);
        let stop_log_path = config.cwd.join(STOP_LOG);

        fs::write(
            &script_path,
            r#"
import json
import sys

mode = sys.argv[1]
log_path = sys.argv[2]

event = json.load(sys.stdin)

if mode == "tool":
    with open(log_path, "a", encoding="utf-8") as f:
        f.write(json.dumps(event) + "\n")
    if event.get("phase") == "before_execution":
        print(json.dumps({"local_shell": {"timeout_ms": "infinite"}}))
else:
    with open(log_path, "w", encoding="utf-8") as f:
        json.dump(event, f)
"#,
        )
        .expect("write hook script");

        config.tool_hook_command = Some(vec![
            "python3".to_string(),
            script_path.to_string_lossy().into_owned(),
            "tool".to_string(),
            tool_log_path.to_string_lossy().into_owned(),
        ]);
        config.stop_hook_command = Some(vec![
            "python3".to_string(),
            script_path.to_string_lossy().into_owned(),
            "stop".to_string(),
            stop_log_path.to_string_lossy().into_owned(),
        ]);
    })
    .await?;

    let call_id = "hooked-shell-command";
    let command = r#"python3 -c "import time; time.sleep(0.3); print('hooked')""#;
    let args = json!({
        "command": command,
        "timeout_ms": 50,
        "login": false,
    });
    let arguments = serde_json::to_string(&args)?;

    mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(call_id, "shell_command", &arguments),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    harness.submit("run a command with a short timeout").await?;

    let output = harness.function_call_stdout(call_id).await;
    assert!(
        output.contains("Exit code: 0"),
        "expected success output: {output}"
    );
    assert_regex_match("hooked", &output);

    let tool_log_raw = fs::read_to_string(harness.path(TOOL_LOG))?;
    let tool_events: Vec<Value> = tool_log_raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;

    assert_eq!(tool_events.len(), 2);
    assert_eq!(tool_events[0]["phase"], "before_execution");
    assert_eq!(tool_events[0]["call"]["tool_name"], "shell_command");
    assert_eq!(tool_events[0]["call"]["call_id"], call_id);
    assert_eq!(tool_events[0]["call"]["payload"]["kind"], "function");
    assert_eq!(
        tool_events[0]["call"]["payload"]["parsed_arguments"]["timeout_ms"],
        50
    );
    assert_eq!(tool_events[0].get("outcome"), None);

    assert_eq!(tool_events[1]["phase"], "after_execution");
    assert_eq!(tool_events[1]["call"]["tool_name"], "shell_command");
    assert_eq!(tool_events[1]["call"]["call_id"], call_id);
    assert_eq!(tool_events[1]["call"]["payload"]["kind"], "function");
    assert!(tool_events[1]["outcome"].get("success").is_some());

    let stop_event: Value = serde_json::from_str(&fs::read_to_string(harness.path(STOP_LOG))?)?;
    let expected_cwd = harness.cwd().display().to_string();
    assert_eq!(
        stop_event.get("cwd").and_then(Value::as_str),
        Some(expected_cwd.as_str())
    );
    assert_eq!(
        stop_event.get("final_message").and_then(Value::as_str),
        Some("done")
    );
    assert!(
        stop_event["conversation_id"].as_str().is_some(),
        "expected conversation_id in stop hook payload: {stop_event}"
    );
    assert!(
        stop_event["turn_id"].as_str().is_some(),
        "expected turn_id in stop hook payload: {stop_event}"
    );
    assert!(
        stop_event["response_items"].as_array().is_some(),
        "expected response_items in stop hook payload: {stop_event}"
    );

    Ok(())
}
