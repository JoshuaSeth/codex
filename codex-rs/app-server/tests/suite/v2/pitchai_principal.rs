use anyhow::Context;
use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::TestAppServer;
use app_test_support::create_fake_rollout;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::rollout_path;
use app_test_support::to_response;
use app_test_support::write_chatgpt_auth;
use app_test_support::write_mock_responses_config_toml_with_chatgpt_base_url;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::PitchAiSkillPrincipal as AppServerPitchAiSkillPrincipal;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SkillsListParams;
use codex_app_server_protocol::SkillsListResponse;
use codex_app_server_protocol::ThreadForkHistoryMode;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_config::types::AuthCredentialsStoreMode;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::PitchAiSkillPrincipal;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::TurnContextItem;
use codex_rollout::read_session_meta_line;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::MockServer;

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
    let (_server, codex_home, catalog) = create_managed_fixture().await?;
    let mut app_server = start_managed_server(&codex_home, &catalog).await?;
    let thread_id = start_materialized_thread(&mut app_server, THOMAS_ID, "materialize").await?;

    let mismatched_resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
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
    assert_sanitized(&error.error.message);

    let same_resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id,
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

#[tokio::test]
async fn cold_restart_rejects_cross_principal_resume_and_accepts_same_principal() -> Result<()> {
    let (_server, codex_home, catalog) = create_managed_fixture().await?;
    let mut app_server = start_managed_server(&codex_home, &catalog).await?;
    let thread_id = start_materialized_thread(&mut app_server, THOMAS_ID, "materialize").await?;
    drop(app_server);

    let mut app_server = start_managed_server(&codex_home, &catalog).await?;

    let mismatched_resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
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
            thread_id: thread_id.clone(),
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

    drop(app_server);
    let mut unbound_app_server = TestAppServer::new_with_env(
        codex_home.path(),
        &[("PITCHAI_SKILL_CATALOG_RELEASE", None)],
    )
    .await?;
    timeout(DEFAULT_TIMEOUT, unbound_app_server.initialize()).await??;
    let unbound_resume_id = unbound_app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            ..Default::default()
        })
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_TIMEOUT,
        unbound_app_server.read_stream_until_error_message(RequestId::Integer(unbound_resume_id)),
    )
    .await??;
    assert_eq!(
        error.error.message,
        "Resuming an identity-bound PitchAI thread requires an authoritative tenant/user principal."
    );
    assert_sanitized(&error.error.message);

    let explicit_resume_id = unbound_app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id,
            config: Some(principal_config(THOMAS_ID)),
            ..Default::default()
        })
        .await?;
    let explicit_error: JSONRPCError = timeout(
        DEFAULT_TIMEOUT,
        unbound_app_server.read_stream_until_error_message(RequestId::Integer(explicit_resume_id)),
    )
    .await??;
    assert_eq!(
        explicit_error.error.message,
        "PitchAI skill principal was supplied without PITCHAI_SKILL_CATALOG_RELEASE."
    );
    assert_sanitized(&explicit_error.error.message);
    Ok(())
}

#[tokio::test]
async fn cold_restart_rejects_cross_principal_fork_and_persists_same_principal() -> Result<()> {
    let (_server, codex_home, catalog) = create_managed_fixture().await?;
    let mut app_server = start_managed_server(&codex_home, &catalog).await?;
    let source_thread_id =
        start_materialized_thread(&mut app_server, THOMAS_ID, "materialize before fork").await?;
    drop(app_server);

    let mut app_server = start_managed_server(&codex_home, &catalog).await?;

    let mismatched_fork_id = app_server
        .send_thread_fork_request(ThreadForkParams {
            thread_id: source_thread_id.clone(),
            config: Some(principal_config(JEF_ID)),
            ..Default::default()
        })
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_error_message(RequestId::Integer(mismatched_fork_id)),
    )
    .await??;
    assert_eq!(
        error.error.message,
        "PitchAI skill principal does not match the identity already bound to this thread."
    );
    assert_sanitized(&error.error.message);

    let same_fork_id = app_server
        .send_thread_fork_request(ThreadForkParams {
            thread_id: source_thread_id.clone(),
            config: Some(principal_config(THOMAS_ID)),
            history_mode: ThreadForkHistoryMode::Compact,
            ..Default::default()
        })
        .await?;
    let same_response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(same_fork_id)),
    )
    .await??;
    let ThreadForkResponse { thread: forked, .. } = to_response(same_response)?;
    assert_ne!(forked.id, source_thread_id);
    let forked_path = forked
        .path
        .context("forked thread must have a rollout path")?;
    let meta = read_session_meta_line(forked_path.as_path()).await?;
    let principal = meta
        .meta
        .pitchai_principal
        .context("forked thread must persist its managed principal")?;
    assert_eq!(principal, persisted_principal(THOMAS_ID));
    Ok(())
}

#[tokio::test]
async fn legacy_resume_migrates_identity_before_writer_and_survives_restart() -> Result<()> {
    let (_server, codex_home, catalog) = create_managed_fixture().await?;
    let thread_id = create_fake_rollout(
        codex_home.path(),
        "2026-08-09T09-00-00",
        "2026-08-09T09:00:00Z",
        "legacy history must survive identity migration",
        Some("mock_provider"),
        None,
    )?;
    let path = rollout_path(codex_home.path(), "2026-08-09T09-00-00", &thread_id);
    let before = std::fs::read_to_string(&path)?;
    assert!(!before.contains("pitchai_principal"));

    let mut app_server = start_managed_server(&codex_home, &catalog).await?;

    let resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            config: Some(principal_config(THOMAS_ID)),
            ..Default::default()
        })
        .await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(resume_id)),
    )
    .await??;
    let _: ThreadResumeResponse = to_response(response)?;
    drop(app_server);

    let after = std::fs::read_to_string(&path)?;
    assert_eq!(after.lines().count(), before.lines().count());
    assert!(after.contains("legacy history must survive identity migration"));
    assert!(!after.contains("chatgpt-token"));
    let meta = read_session_meta_line(&path).await?;
    let principal = meta
        .meta
        .pitchai_principal
        .context("legacy resume must persist its principal in canonical metadata")?;
    assert_eq!(principal, persisted_principal(THOMAS_ID));

    let mut app_server = start_managed_server(&codex_home, &catalog).await?;
    let mismatched_resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id,
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
    assert_sanitized(&error.error.message);
    Ok(())
}

#[tokio::test]
async fn compacted_legacy_resume_replaces_stale_skills_context_before_model_request() -> Result<()>
{
    let (server, codex_home, catalog) = create_managed_fixture().await?;
    let thread_id = create_fake_rollout(
        codex_home.path(),
        "2026-08-09T09-15-00",
        "2026-08-09T09:15:00Z",
        "legacy user history must survive",
        Some("mock_provider"),
        None,
    )?;
    let path = rollout_path(codex_home.path(), "2026-08-09T09-15-00", &thread_id);
    let stale_skills = "<skills_instructions>\n## Skills\n### Available skills\n- pitchai-thomas-m365: stale Thomas capability\n- seth-private: stale Seth capability\n- stale-secret-sentinel: must not reach the model\n</skills_instructions>";
    let turn_context = TurnContextItem {
        turn_id: Some("legacy-turn".to_string()),
        cwd: PathBuf::from("/"),
        workspace_roots: None,
        current_date: Some("2026-08-09".to_string()),
        timezone: Some("UTC".to_string()),
        approval_policy: AskForApproval::Never,
        sandbox_policy: SandboxPolicy::new_read_only_policy(),
        permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: "gpt-5.3-codex".to_string(),
        comp_hash: None,
        personality: None,
        collaboration_mode: None,
        multi_agent_version: None,
        realtime_active: Some(false),
        effort: None,
        summary: ReasoningSummary::Auto,
    };
    let legacy_lines = [
        json!({
            "timestamp": "2026-08-09T09:15:01Z",
            "type": "compacted",
            "payload": {
                "message": "legacy compacted summary",
                "replacement_history": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "compacted replacement history must survive",
                        }],
                    },
                    {
                        "type": "message",
                        "role": "developer",
                        "content": [{"type": "input_text", "text": stale_skills}],
                    },
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": "legacy compacted assistant history",
                        }],
                    },
                ],
            },
        })
        .to_string(),
        json!({
            "timestamp": "2026-08-09T09:15:02Z",
            "type": "turn_context",
            "payload": serde_json::to_value(turn_context)?,
        })
        .to_string(),
    ];
    std::fs::write(
        &path,
        format!(
            "{}{}\n",
            std::fs::read_to_string(&path)?,
            legacy_lines.join("\n")
        ),
    )?;

    let mut app_server = start_managed_server(&codex_home, &catalog).await?;
    let resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            config: Some(principal_config(JEF_ID)),
            ..Default::default()
        })
        .await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(resume_id)),
    )
    .await??;
    let _: ThreadResumeResponse = to_response(response)?;
    let before_turn = std::fs::read_to_string(&path)?;
    assert!(before_turn.contains("stale-secret-sentinel"));

    let turn_id = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id,
            input: vec![UserInput::Text {
                text: "Report the currently available skills.".to_string(),
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

    let requests = server
        .received_requests()
        .await
        .context("mock server should retain requests")?;
    let model_requests = requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .collect::<Vec<_>>();
    assert_eq!(model_requests.len(), 1);
    let model_request = model_requests[0]
        .body_json::<Value>()
        .context("model request should be JSON")?
        .to_string();
    assert!(model_request.contains("compacted replacement history must survive"));
    assert_eq!(model_request.matches("<skills_instructions>").count(), 1);
    assert!(model_request.contains("- pitchai-jeff-m365-azure:"));
    for forbidden in [
        "pitchai-thomas-m365",
        "seth-private",
        "stale-secret-sentinel",
    ] {
        assert!(!model_request.contains(forbidden));
    }

    let persisted = std::fs::read_to_string(path)?;
    assert!(persisted.starts_with(&before_turn));
    assert!(persisted.contains("stale-secret-sentinel"));
    assert!(persisted.contains("pitchai-jeff-m365-azure"));
    Ok(())
}

#[tokio::test]
async fn force_reload_revokes_managed_skill_before_next_model_request() -> Result<()> {
    let (server, codex_home, catalog) = create_managed_fixture().await?;
    let mut app_server = start_managed_server(&codex_home, &catalog).await?;
    let cwd = codex_home.path().to_path_buf();
    let start_id = app_server
        .send_thread_start_request(ThreadStartParams {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            config: Some(principal_config(JEF_ID)),
            ..Default::default()
        })
        .await?;
    let start_response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response(start_response)?;
    let rollout_path = thread
        .path
        .context("managed thread should have a rollout path")?;

    let first_turn_id = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Report the initial managed skills.".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(first_turn_id)),
    )
    .await??;
    timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let before_revocation = std::fs::read_to_string(&rollout_path)?;
    assert!(before_revocation.contains("pitchai-jeff-m365-azure"));

    std::fs::remove_dir_all(
        catalog
            .path()
            .join("profiles/users")
            .join(JEF_ID)
            .join("skills/pitchai-jeff-m365-azure"),
    )?;
    let skills_id = app_server
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd],
            force_reload: true,
            pitchai_principal: Some(AppServerPitchAiSkillPrincipal {
                schema_version: 1,
                tenant_id: TENANT_ID.to_string(),
                user_id: JEF_ID.to_string(),
            }),
        })
        .await?;
    let skills_response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(skills_id)),
    )
    .await??;
    let SkillsListResponse { data } = to_response(skills_response)?;
    let refreshed = data
        .first()
        .context("force-reloaded skills response should include the requested CWD")?;
    assert!(
        refreshed
            .skills
            .iter()
            .any(|skill| skill.name == "system-shared")
    );
    assert!(
        refreshed
            .skills
            .iter()
            .all(|skill| skill.name != "pitchai-jeff-m365-azure")
    );

    let second_turn_id = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            input: vec![UserInput::Text {
                text: "Report the force-reloaded managed skills.".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(second_turn_id)),
    )
    .await??;
    timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let requests = server
        .received_requests()
        .await
        .context("mock server should retain requests")?;
    let model_requests = requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .collect::<Vec<_>>();
    assert_eq!(model_requests.len(), 2);
    let refreshed_request = model_requests[1]
        .body_json::<Value>()
        .context("force-reloaded model request should be JSON")?
        .to_string();
    assert_eq!(
        refreshed_request.matches("<skills_instructions>").count(),
        1
    );
    assert!(refreshed_request.contains("- system-shared:"));
    assert!(!refreshed_request.contains("pitchai-jeff-m365-azure"));
    assert!(!refreshed_request.contains("pitchai-thomas-m365"));

    let persisted = std::fs::read_to_string(rollout_path)?;
    assert!(persisted.starts_with(&before_revocation));
    assert_eq!(persisted.matches("<skills_instructions>").count(), 2);
    assert!(persisted.contains("pitchai-jeff-m365-azure"));
    assert!(persisted.contains("system-shared"));
    Ok(())
}

#[tokio::test]
async fn legacy_unbound_thread_cannot_be_forked_into_a_caller_selected_principal() -> Result<()> {
    let codex_home = TempDir::new()?;
    let catalog = create_catalog()?;
    let thread_id = create_fake_rollout(
        codex_home.path(),
        "2026-08-09T09-30-00",
        "2026-08-09T09:30:00Z",
        "legacy source must bind before fork",
        Some("mock_provider"),
        None,
    )?;
    let path = rollout_path(codex_home.path(), "2026-08-09T09-30-00", &thread_id);
    let before = std::fs::read_to_string(&path)?;
    let mut app_server = start_managed_server(&codex_home, &catalog).await?;

    let fork_id = app_server
        .send_thread_fork_request(ThreadForkParams {
            thread_id,
            config: Some(principal_config(THOMAS_ID)),
            ..Default::default()
        })
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_error_message(RequestId::Integer(fork_id)),
    )
    .await??;

    assert_eq!(
        error.error.message,
        "Legacy managed source thread must be resumed once with its canonical principal before it can be forked."
    );
    assert_sanitized(&error.error.message);
    assert_eq!(
        std::fs::read_to_string(path)?,
        before,
        "a rejected fork must not bind caller-selected identity to the source"
    );
    Ok(())
}

#[tokio::test]
async fn managed_resume_and_fork_reject_client_selected_identity_sources() -> Result<()> {
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

    let resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string(),
            history: Some(Vec::new()),
            config: Some(principal_config(THOMAS_ID)),
            ..Default::default()
        })
        .await?;
    let resume_error: JSONRPCError = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_error_message(RequestId::Integer(resume_id)),
    )
    .await??;
    assert_eq!(
        resume_error.error.message,
        "Managed PitchAI thread resume requires canonical stored history selected by thread id; client-supplied history and paths are not authoritative identity sources."
    );
    assert_sanitized(&resume_error.error.message);

    let fork_id = app_server
        .send_thread_fork_request(ThreadForkParams {
            thread_id: "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string(),
            path: Some(codex_home.path().join("forged-rollout.jsonl")),
            config: Some(principal_config(THOMAS_ID)),
            ..Default::default()
        })
        .await?;
    let fork_error: JSONRPCError = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_error_message(RequestId::Integer(fork_id)),
    )
    .await??;
    assert_eq!(
        fork_error.error.message,
        "Managed PitchAI thread fork requires canonical stored history selected by thread id; a client-supplied path is not an authoritative identity source."
    );
    assert_sanitized(&fork_error.error.message);
    Ok(())
}

async fn create_managed_fixture() -> Result<(MockServer, TempDir, TempDir)> {
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
    Ok((server, codex_home, create_catalog()?))
}

async fn start_managed_server(codex_home: &TempDir, catalog: &TempDir) -> Result<TestAppServer> {
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
    Ok(app_server)
}

async fn start_materialized_thread(
    app_server: &mut TestAppServer,
    user_id: &str,
    prompt: &str,
) -> Result<String> {
    let start_id = app_server
        .send_thread_start_request(ThreadStartParams {
            config: Some(principal_config(user_id)),
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
                text: prompt.to_string(),
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
    Ok(thread.id)
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

fn persisted_principal(user_id: &str) -> PitchAiSkillPrincipal {
    PitchAiSkillPrincipal {
        schema_version: 1,
        tenant_id: TENANT_ID.to_string(),
        user_id: user_id.to_string(),
    }
}

fn assert_sanitized(message: &str) {
    assert!(!message.contains(TENANT_ID));
    assert!(!message.contains(THOMAS_ID));
    assert!(!message.contains(JEF_ID));
    assert!(!message.contains("chatgpt-token"));
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
    write_skill(
        &catalog.path().join(".system"),
        "system-shared",
        "approved system capability",
    )?;
    write_skill(
        &catalog
            .path()
            .join("profiles/users")
            .join(THOMAS_ID)
            .join("skills"),
        "pitchai-thomas-m365",
        "Thomas-only capability",
    )?;
    write_skill(
        &catalog
            .path()
            .join("profiles/users")
            .join(JEF_ID)
            .join("skills"),
        "pitchai-jeff-m365-azure",
        "Jef-only capability",
    )?;
    Ok(catalog)
}

fn write_skill(root: &Path, name: &str, description: &str) -> Result<()> {
    let skill_dir = root.join(name);
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n"),
    )?;
    Ok(())
}
