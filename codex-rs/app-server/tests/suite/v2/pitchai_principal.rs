use anyhow::Context;
use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use app_test_support::write_chatgpt_auth;
use app_test_support::write_mock_responses_config_toml_with_chatgpt_base_url;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_config::types::AuthCredentialsStoreMode;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

const TENANT_ID: &str = "9bc52e7e-79df-5a9b-a4c7-d4eb29d24f12";
const THOMAS_ID: &str = "baef2f0b-181b-571d-b66b-7a52d79eb963";
const JEF_ID: &str = "340fb61c-ed7b-57c4-b2f2-c1cd8e7986fd";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn managed_thread_start_requires_principal_without_echoing_config() -> Result<()> {
    let codex_home = TempDir::new()?;
    let catalog = create_catalog()?;
    let catalog_path = catalog
        .path()
        .to_str()
        .context("catalog path must be UTF-8")?;
    let mut app_server = TestAppServer::new_with_env(
        codex_home.path(),
        &[("PITCHAI_SKILL_CATALOG_RELEASE", Some(catalog_path))],
    )
    .await?;
    timeout(DEFAULT_TIMEOUT, app_server.initialize()).await??;

    let request_id = app_server
        .send_thread_start_request(ThreadStartParams::default())
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(
        error.error.message,
        "Managed PitchAI skill resolution requires an authoritative tenant/user principal."
    );
    assert!(!error.error.message.contains(TENANT_ID));
    assert!(!error.error.message.contains(THOMAS_ID));
    assert!(!error.error.message.contains(JEF_ID));
    Ok(())
}

#[tokio::test]
async fn loaded_thread_rejects_cross_principal_resume_and_accepts_same_principal() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("done").await;
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml_with_chatgpt_base_url(
        codex_home.path(),
        &server.uri(),
        &server.uri(),
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("chatgpt-token"),
        AuthCredentialsStoreMode::File,
    )?;
    let catalog = create_catalog()?;
    let catalog_path = catalog
        .path()
        .to_str()
        .context("catalog path must be UTF-8")?;
    let mut app_server = TestAppServer::new_with_env(
        codex_home.path(),
        &[("PITCHAI_SKILL_CATALOG_RELEASE", Some(catalog_path))],
    )
    .await?;
    timeout(DEFAULT_TIMEOUT, app_server.initialize()).await??;

    let start_id = app_server
        .send_thread_start_request(ThreadStartParams {
            config: Some(principal_config(THOMAS_ID)),
            ..Default::default()
        })
        .await?;
    let start_response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response(start_response)?;

    let turn_id = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "materialize".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(turn_id)),
    )
    .await??;
    timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let mismatched_resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id.clone(),
            config: Some(principal_config(JEF_ID)),
            ..Default::default()
        })
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_error_message(RequestId::Integer(mismatched_resume_id)),
    )
    .await??;
    assert_eq!(
        error.error.message,
        "PitchAI skill principal does not match the identity already bound to this thread."
    );
    assert!(!error.error.message.contains(TENANT_ID));
    assert!(!error.error.message.contains(THOMAS_ID));
    assert!(!error.error.message.contains(JEF_ID));

    let same_resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id,
            config: Some(principal_config(THOMAS_ID)),
            ..Default::default()
        })
        .await?;
    let same_response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(same_resume_id)),
    )
    .await??;
    let _: ThreadResumeResponse = to_response(same_response)?;
    Ok(())
}

fn principal_config(user_id: &str) -> HashMap<String, Value> {
    HashMap::from([(
        "skills".to_string(),
        json!({
            "pitchai_principal": {
                "schema_version": 1,
                "tenant_id": TENANT_ID,
                "user_id": user_id,
            }
        }),
    )])
}

fn create_catalog() -> Result<TempDir> {
    let catalog = TempDir::new()?;
    std::fs::write(
        catalog.path().join(".pitchai-principal-catalog-v1"),
        "pitchai-codex-home-skills/principal-v1\n",
    )?;
    for root in [
        catalog.path().join(".system"),
        catalog
            .path()
            .join("profiles/tenants")
            .join(TENANT_ID)
            .join("skills"),
        catalog
            .path()
            .join("profiles/users")
            .join(THOMAS_ID)
            .join("skills"),
        catalog
            .path()
            .join("profiles/users")
            .join(JEF_ID)
            .join("skills"),
    ] {
        std::fs::create_dir_all(root)?;
    }
    Ok(catalog)
}
