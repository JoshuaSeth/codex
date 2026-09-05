//! Linux has no platform proxy resolver, so exercise its environment fallback
//! through a real session, including the separately constructed compact client.

use anyhow::Result;
use codex_login::CodexAuth;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::time::Duration;
use tokio::process::Command;

const CHILD_ENV: &str = "CODEX_RESPONSES_PROXY_TEST_CHILD";
const TEST_NAME: &str =
    "responses_api_system_proxy::responses_and_compaction_use_enabled_proxy_fallback";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_and_compaction_use_enabled_proxy_fallback() -> Result<()> {
    skip_if_no_network!(Ok(()));

    if std::env::var_os(CHILD_ENV).is_none() {
        let proxy = responses::start_mock_server().await;
        let stream_mock = responses::mount_sse_once(
            &proxy,
            responses::sse(vec![
                responses::ev_assistant_message("proxy-message", "proxy reply"),
                responses::ev_completed("proxy-response"),
            ]),
        )
        .await;
        let compact_mock = responses::mount_compact_user_history_with_summary_once(
            &proxy,
            "proxy compaction summary",
        )
        .await;

        // Isolate environment-dependent client initialization from parallel
        // tests. Only this child receives the mock proxy configuration.
        let mut child = Command::new(std::env::current_exe()?);
        child
            .arg("--exact")
            .arg(TEST_NAME)
            .arg("--nocapture")
            .env(CHILD_ENV, "1")
            .kill_on_drop(true);
        for &key in codex_network_proxy::PROXY_ENV_KEYS {
            child.env_remove(key);
        }
        child.env("HTTP_PROXY", proxy.uri());
        let output = tokio::time::timeout(Duration::from_secs(60), child.output()).await??;
        assert!(
            output.status.success(),
            "proxy session failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        let stream = stream_mock.single_request();
        let compact = compact_mock.single_request();
        assert_eq!(
            (stream.path(), compact.path()),
            (
                "/v1/responses".to_string(),
                "/v1/responses/compact".to_string()
            )
        );
        assert!(stream.body_contains_text("hello through proxy"));
        assert!(compact.body_contains_text("proxy reply"));
        assert_eq!(
            (
                stream.header("chatgpt-account-id"),
                compact.header("chatgpt-account-id"),
            ),
            (
                Some("account_id".to_string()),
                Some("account_id".to_string())
            )
        );
        return Ok(());
    }

    let unused_origin = responses::start_mock_server().await;
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_pre_build_hook(|home| {
            std::fs::write(
                home.join("config.toml"),
                "[features]\nrespect_system_proxy = true\n",
            )
            .expect("write proxy feature configuration");
        })
        .with_config(|config| {
            // This reserved name cannot resolve directly. Both requests must
            // reach the parent's proxy to complete the session operations.
            config.model_provider.base_url =
                Some("http://responses-api-proxy.invalid/v1".to_string());
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        });
    let test = builder.build_with_auto_env(&unused_origin).await?;
    assert!(test.config.respect_system_proxy);
    test.submit_turn("hello through proxy").await?;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(unused_origin.received_requests().await.unwrap().is_empty());
    Ok(())
}
