use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use anyhow::Context;
use app_test_support::ChatGptAuthFixture;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use app_test_support::write_chatgpt_auth;
use codex_app_server_protocol::ExperimentalFeatureEnablementSetParams;
use codex_app_server_protocol::ExperimentalFeatureEnablementSetResponse;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::PitchAiSkillPrincipal;
use codex_app_server_protocol::ConfigBatchWriteParams;
use codex_app_server_protocol::ConfigEdit;
use codex_app_server_protocol::ConfigWriteResponse;
use codex_app_server_protocol::ExperimentalFeatureEnablementSetParams;
use codex_app_server_protocol::ExperimentalFeatureEnablementSetResponse;
use codex_app_server_protocol::MergeStrategy;
use codex_app_server_protocol::PluginListParams;
use codex_app_server_protocol::PluginListResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SkillScope;
use codex_app_server_protocol::SkillsChangedNotification;
use codex_app_server_protocol::SkillsExtraRootsSetParams;
use codex_app_server_protocol::SkillsExtraRootsSetResponse;
use codex_app_server_protocol::SkillsListParams;
use codex_app_server_protocol::SkillsListResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_config::types::AuthCredentialsStoreMode;
use codex_exec_server::CODEX_EXEC_SERVER_URL_ENV_VAR;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::skip_if_remote;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const WATCHER_TIMEOUT: Duration = Duration::from_secs(20);

fn write_skill(root: &TempDir, name: &str) -> Result<()> {
    let skill_dir = root.path().join("skills").join(name);
    std::fs::create_dir_all(&skill_dir)?;
    let content = format!("---\nname: {name}\ndescription: {name} description\n---\n\n# Body\n");
    std::fs::write(skill_dir.join("SKILL.md"), content)?;
    Ok(())
}

#[tokio::test]
async fn skills_list_resolves_pitchai_tenant_and_user_profiles_without_root_leakage() -> Result<()>
{
    const SETH_ID: &str = "3a41af79-9fab-5ea0-bb83-a121ee0498a0";
    const THOMAS_ID: &str = "baef2f0b-181b-571d-b66b-7a52d79eb963";
    const JEF_ID: &str = "340fb61c-ed7b-57c4-b2f2-c1cd8e7986fd";
    const TENANT_ID: &str = "9bc52e7e-79df-5a9b-a4c7-d4eb29d24f12";

    let codex_home = TempDir::new()?;
    let catalog = TempDir::new()?;
    let repo = TempDir::new()?;
    let wrong_cwd = TempDir::new()?;
    std::fs::write(
        catalog.path().join(".pitchai-principal-catalog-v1"),
        "pitchai-codex-home-skills/principal-v1\n",
    )?;
    let system_root = catalog.path().join(".system");
    let tenant_root = catalog
        .path()
        .join("profiles/tenants")
        .join(TENANT_ID)
        .join("skills");
    let seth_root = catalog
        .path()
        .join("profiles/users")
        .join(SETH_ID)
        .join("skills");
    let thomas_root = catalog
        .path()
        .join("profiles/users")
        .join(THOMAS_ID)
        .join("skills");
    let jef_root = catalog
        .path()
        .join("profiles/users")
        .join(JEF_ID)
        .join("skills");
    for root in [
        &system_root,
        &tenant_root,
        &seth_root,
        &thomas_root,
        &jef_root,
    ] {
        std::fs::create_dir_all(root)?;
    }

    write_skill_at(&system_root, "system-shared", "system-shared", "system")?;
    write_skill_at(&system_root, "duplicate", "duplicate", "system duplicate")?;
    write_skill_at(&tenant_root, "tenant-shared", "tenant-shared", "tenant")?;
    write_skill_at(&tenant_root, "duplicate", "duplicate", "tenant duplicate")?;
    write_skill_at(&seth_root, "m365", "m365", "Seth private")?;
    write_skill_at(
        &seth_root,
        "saulo-mailbox",
        "saulo-mailbox",
        "explicitly Seth-bound mailbox code",
    )?;
    write_skill_at(
        &thomas_root,
        "pitchai-thomas-m365",
        "pitchai-thomas-m365",
        "Thomas private",
    )?;
    write_skill_at(&thomas_root, "duplicate", "duplicate", "Thomas duplicate")?;
    write_skill_at(
        &jef_root,
        "pitchai-jeff-m365-azure",
        "pitchai-jeff-m365-azure",
        "Jef private",
    )?;
    write_skill_at(
        &codex_home.path().join("skills"),
        "root-private",
        "root-private",
        "must never load",
    )?;
    write_skill_at(
        &codex_home.path().join("skills"),
        "seth-tracking-data-query",
        "seth-tracking-data-query",
        "root-only and never inferred",
    )?;
    write_skill_at(
        catalog.path(),
        "catalog-backed",
        "catalog-backed",
        "approved immutable link",
    )?;
    std::os::unix::fs::symlink(
        catalog.path().join("catalog-backed"),
        thomas_root.join("catalog-backed"),
    )?;
    std::fs::create_dir_all(repo.path().join(".git"))?;
    let repo_root = repo.path().join(".agents/skills");
    write_skill_at(&repo_root, "duplicate", "duplicate", "repo duplicate")?;
    std::os::unix::fs::symlink(
        codex_home.path().join("skills/root-private"),
        repo_root.join("forbidden-link"),
    )?;

    let catalog_path = catalog
        .path()
        .to_str()
        .context("catalog path must be UTF-8")?;
    let mut mcp = TestAppServer::new_with_env(
        codex_home.path(),
        &[("PITCHAI_SKILL_CATALOG_RELEASE", Some(catalog_path))],
    )
    .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    let project_response = request_skills(&mut mcp, repo.path(), false, THOMAS_ID).await?;
    let project_entry = &project_response.data[0];
    assert_eq!(project_entry.errors.len(), 1);
    assert!(
        project_entry.errors[0]
            .path
            .ends_with(".agents/skills/forbidden-link")
    );
    assert!(
        project_entry.errors[0]
            .message
            .contains("outside its approved source boundary")
    );
    assert!(
        !project_entry.errors[0]
            .message
            .contains(codex_home.path().to_string_lossy().as_ref())
    );
    let duplicate = project_entry
        .skills
        .iter()
        .find(|skill| skill.name == "duplicate")
        .context("repo duplicate should win")?;
    assert_eq!(duplicate.description, "repo duplicate");
    assert_eq!(duplicate.scope, SkillScope::Repo);
    let project_names = project_entry
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        project_names
            .iter()
            .filter(|name| **name == "duplicate")
            .count(),
        1
    );
    assert!(project_names.contains(&"pitchai-thomas-m365"));
    assert!(project_names.contains(&"catalog-backed"));
    assert!(project_names.contains(&"tenant-shared"));
    assert!(project_names.contains(&"system-shared"));
    assert!(!project_names.contains(&"m365"));
    assert!(!project_names.contains(&"pitchai-jeff-m365-azure"));
    assert!(!project_names.contains(&"saulo-mailbox"));
    assert!(!project_names.contains(&"seth-tracking-data-query"));
    assert!(!project_names.contains(&"root-private"));

    let wrong_response = request_skills(&mut mcp, wrong_cwd.path(), false, THOMAS_ID).await?;
    let wrong_entry = &wrong_response.data[0];
    let duplicate = wrong_entry
        .skills
        .iter()
        .find(|skill| skill.name == "duplicate")
        .context("user duplicate should win outside a repo")?;
    assert_eq!(duplicate.description, "Thomas duplicate");
    assert_eq!(duplicate.scope, SkillScope::User);

    let seth_response = request_skills(&mut mcp, wrong_cwd.path(), false, SETH_ID).await?;
    let seth_names = seth_response.data[0]
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>();
    assert!(seth_names.contains(&"m365"));
    assert!(seth_names.contains(&"saulo-mailbox"));
    assert!(!seth_names.contains(&"seth-tracking-data-query"));
    assert!(!seth_names.contains(&"pitchai-thomas-m365"));
    assert!(!seth_names.contains(&"pitchai-jeff-m365-azure"));

    let jef_response = request_skills(&mut mcp, wrong_cwd.path(), false, JEF_ID).await?;
    let jef_names = jef_response.data[0]
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>();
    assert!(jef_names.contains(&"pitchai-jeff-m365-azure"));
    assert!(!jef_names.contains(&"m365"));
    assert!(!jef_names.contains(&"saulo-mailbox"));
    assert!(!jef_names.contains(&"seth-tracking-data-query"));
    assert!(!jef_names.contains(&"pitchai-thomas-m365"));

    write_skill_at(&tenant_root, "late-tenant", "late-tenant", "force reloaded")?;
    let cached_response = request_skills(&mut mcp, wrong_cwd.path(), false, THOMAS_ID).await?;
    assert!(
        cached_response.data[0]
            .skills
            .iter()
            .all(|skill| skill.name != "late-tenant")
    );
    let reloaded_response = request_skills(&mut mcp, wrong_cwd.path(), true, THOMAS_ID).await?;
    assert!(
        reloaded_response.data[0]
            .skills
            .iter()
            .any(|skill| skill.name == "late-tenant")
    );
    Ok(())
}

fn write_skill_at(
    root: &std::path::Path,
    directory: &str,
    name: &str,
    description: &str,
) -> Result<()> {
    let skill_dir = root.join(directory);
    std::fs::create_dir_all(&skill_dir)?;
    let content = format!("---\nname: {name}\ndescription: {description}\n---\n\n# Body\n");
    std::fs::write(skill_dir.join("SKILL.md"), content)?;
    Ok(())
}

async fn request_skills(
    mcp: &mut TestAppServer,
    cwd: &std::path::Path,
    force_reload: bool,
    user_id: &str,
) -> Result<SkillsListResponse> {
    let request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd.to_path_buf()],
            force_reload,
            pitchai_principal: Some(PitchAiSkillPrincipal {
                schema_version: 1,
                tenant_id: "9bc52e7e-79df-5a9b-a4c7-d4eb29d24f12".to_string(),
                user_id: user_id.to_string(),
            }),
        })
        .await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    to_response(response)
}

async fn expect_skills_changed_notification(
    mcp: &mut TestAppServer,
    timeout_duration: Duration,
) -> Result<()> {
    let notification: SkillsChangedNotification =
        timeout(timeout_duration, mcp.read_notification("skills/changed")).await??;
    assert_eq!(notification, SkillsChangedNotification {});
    Ok(())
}

fn write_plugins_enabled_config_with_base_url(
    codex_home: &std::path::Path,
    base_url: &str,
) -> std::io::Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"chatgpt_base_url = "{base_url}"

[features]
plugins = true
"#,
        ),
    )
}

fn write_plugin_with_skill(
    repo_root: &std::path::Path,
    plugin_name: &str,
    skill_name: &str,
) -> Result<()> {
    std::fs::create_dir_all(repo_root.join(".git"))?;
    std::fs::create_dir_all(repo_root.join(".agents/plugins"))?;
    std::fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        format!(
            r#"{{
  "name": "local-marketplace",
  "plugins": [
    {{
      "name": "{plugin_name}",
      "source": {{
        "source": "local",
        "path": "./{plugin_name}"
      }}
    }}
  ]
}}"#
        ),
    )?;

    let plugin_root = repo_root.join(plugin_name);
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        format!(r#"{{"name":"{plugin_name}"}}"#),
    )?;

    let skill_dir = plugin_root.join("skills").join(skill_name);
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {skill_name}\ndescription: {skill_name} description\n---\n\n# Body\n"),
    )?;
    Ok(())
}

fn write_cached_remote_plugin_with_skill(
    codex_home: &std::path::Path,
) -> Result<std::path::PathBuf> {
    let plugin_root = codex_home.join("plugins/cache/openai-curated-remote/linear/local");
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"linear"}"#,
    )?;

    let skill_dir = plugin_root.join("skills/triage-issues");
    std::fs::create_dir_all(&skill_dir)?;
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_path,
        "---\nname: triage-issues\ndescription: Triage Linear issues\n---\n\n# Body\n",
    )?;
    Ok(skill_path)
}

fn write_cached_local_curated_plugin_with_skill(codex_home: &std::path::Path) -> Result<()> {
    let plugin_root = codex_home.join("plugins/cache/openai-curated/google-calendar/local");
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"google-calendar"}"#,
    )?;

    let skill_dir = plugin_root.join("skills/meeting-prep");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: meeting-prep\ndescription: Prepare for meetings\n---\n\n# Body\n",
    )?;
    Ok(())
}

#[tokio::test]
async fn runtime_remote_plugin_toggle_updates_local_curated_plugin_skills() -> Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let server = MockServer::start().await;
    write_cached_local_curated_plugin_with_skill(codex_home.path())?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            r#"chatgpt_base_url = "{}/backend-api/"

[features]
plugins = true

[plugins."google-calendar@openai-curated"]
enabled = true
"#,
            server.uri()
        ),
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("chatgpt-token")
            .account_id("account-123")
            .chatgpt_user_id("user-123")
            .chatgpt_account_id("account-123"),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("PITCHAI_SKILL_CATALOG_RELEASE", None)])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let disablement_request_id = mcp
        .send_experimental_feature_enablement_set_request(ExperimentalFeatureEnablementSetParams {
            enablement: BTreeMap::from([("remote_plugin".to_string(), false)]),
        })
        .await?;
    let _: ExperimentalFeatureEnablementSetResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(disablement_request_id)).await??;

    let initial_skills_list_request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd.path().to_path_buf()],
            force_reload: true,
            pitchai_principal: None,
        })
        .await?;
    let SkillsListResponse { data } = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_response(initial_skills_list_request_id),
    )
    .await??;
    assert!(data.iter().any(|entry| {
        entry
            .skills
            .iter()
            .any(|skill| skill.name == "google-calendar:meeting-prep")
    }));

    let enablement_request_id = mcp
        .send_experimental_feature_enablement_set_request(ExperimentalFeatureEnablementSetParams {
            enablement: BTreeMap::from([("remote_plugin".to_string(), true)]),
        })
        .await?;
    let _: ExperimentalFeatureEnablementSetResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(enablement_request_id)).await??;

    let skills_list_request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd.path().to_path_buf()],
            force_reload: true,
            pitchai_principal: None,
        })
        .await?;
    let SkillsListResponse { data } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(skills_list_request_id)).await??;

    assert!(data.iter().all(|entry| {
        entry
            .skills
            .iter()
            .all(|skill| skill.name != "google-calendar:meeting-prep")
    }));
    Ok(())
}

#[tokio::test]
async fn skills_list_loads_remote_installed_plugin_skills_from_cache() -> Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let server = MockServer::start().await;
    let expected_skill_path =
        std::fs::canonicalize(write_cached_remote_plugin_with_skill(codex_home.path())?)?;
    write_plugins_enabled_config_with_base_url(
        codex_home.path(),
        &format!("{}/backend-api/", server.uri()),
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("chatgpt-token")
            .account_id("account-123")
            .chatgpt_user_id("user-123")
            .chatgpt_account_id("account-123"),
        AuthCredentialsStoreMode::File,
    )?;

    let global_directory_body = r#"{
  "plugins": [
    {
      "id": "plugins~Plugin_linear",
      "name": "linear",
      "scope": "GLOBAL",
      "installation_policy": "AVAILABLE",
      "authentication_policy": "ON_USE",
      "release": {
        "display_name": "Linear",
        "description": "Track work in Linear",
        "app_ids": [],
        "interface": {},
        "skills": []
      }
    }
  ],
  "pagination": {
    "limit": 50,
    "next_page_token": null
  }
}"#;
    let global_installed_body = r#"{
  "plugins": [
    {
      "id": "plugins~Plugin_linear",
      "name": "linear",
      "scope": "GLOBAL",
      "installation_policy": "AVAILABLE",
      "authentication_policy": "ON_USE",
      "release": {
        "display_name": "Linear",
        "description": "Track work in Linear",
        "app_ids": [],
        "interface": {},
        "skills": []
      },
      "enabled": true,
      "disabled_skill_names": []
    }
  ],
  "pagination": {
    "limit": 50,
    "next_page_token": null
  }
}"#;
    let empty_page_body = r#"{
  "plugins": [],
  "pagination": {
    "limit": 50,
    "next_page_token": null
  }
}"#;

    for (scope, body) in [
        ("GLOBAL", global_directory_body),
        ("WORKSPACE", empty_page_body),
    ] {
        Mock::given(method("GET"))
            .and(path("/backend-api/ps/plugins/list"))
            .and(query_param("scope", scope))
            .and(query_param("limit", "200"))
            .and(header("authorization", "Bearer chatgpt-token"))
            .and(header("chatgpt-account-id", "account-123"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    }
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("PITCHAI_SKILL_CATALOG_RELEASE", None)])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let stale_skills_list_request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd.path().to_path_buf()],
            force_reload: true,
            pitchai_principal: None,
        })
        .await?;
    let SkillsListResponse { data } = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_response(stale_skills_list_request_id),
    )
    .await??;
    assert_eq!(data.len(), 1);
    assert!(
        data[0]
            .skills
            .iter()
            .all(|skill| skill.name != "linear:triage-issues"),
        "remote installed plugin cache has not been refreshed yet"
    );

    for (scope, body) in [
        ("GLOBAL", global_installed_body),
        ("USER", empty_page_body),
        ("WORKSPACE", empty_page_body),
    ] {
        Mock::given(method("GET"))
            .and(path("/backend-api/ps/plugins/installed"))
            .and(query_param("scope", scope))
            .and(header("authorization", "Bearer chatgpt-token"))
            .and(header("chatgpt-account-id", "account-123"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    }

    let plugin_list_request_id = mcp
        .send_plugin_list_request(PluginListParams {
            cwds: None,
            marketplace_kinds: None,
            force_refetch: false,
        })
        .await?;
    let _: PluginListResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(plugin_list_request_id)).await??;

    let SkillsListResponse { data } = timeout(DEFAULT_TIMEOUT, async {
        loop {
            let skills_list_request_id = mcp
                .send_skills_list_request(SkillsListParams {
                    cwds: vec![cwd.path().to_path_buf()],
                    force_reload: false,
                    pitchai_principal: None,
                })
                .await?;
            let response: SkillsListResponse =
                timeout(DEFAULT_TIMEOUT, mcp.read_response(skills_list_request_id)).await??;
            if response.data.iter().any(|entry| {
                entry
                    .skills
                    .iter()
                    .any(|skill| skill.name == "linear:triage-issues")
            }) {
                break Ok::<SkillsListResponse, anyhow::Error>(response);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;

    assert_eq!(data.len(), 1);
    assert_eq!(data[0].errors, Vec::new());
    let skill = data[0]
        .skills
        .iter()
        .find(|skill| skill.name == "linear:triage-issues")
        .expect("expected skill from cached remote plugin");
    assert_eq!(
        std::fs::canonicalize(skill.path.as_path())?,
        expected_skill_path
    );
    assert_eq!(skill.enabled, true);
    Ok(())
}

#[tokio::test]
async fn skills_list_excludes_plugin_skills_when_workspace_codex_plugins_disabled() -> Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    let server = MockServer::start().await;
    write_skill(&codex_home, "home-skill")?;
    write_plugin_with_skill(repo_root.path(), "demo-plugin", "plugin-skill")?;
    write_plugins_enabled_config_with_base_url(
        codex_home.path(),
        &format!("{}/backend-api/", server.uri()),
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("chatgpt-token")
            .account_id("account-123")
            .chatgpt_user_id("user-123")
            .chatgpt_account_id("account-123")
            .plan_type("team"),
        AuthCredentialsStoreMode::File,
    )?;
    Mock::given(method("GET"))
        .and(path("/backend-api/accounts/account-123/settings"))
        .and(header("authorization", "Bearer chatgpt-token"))
        .and(header("chatgpt-account-id", "account-123"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"beta_settings":{"enable_plugins":false}}"#),
        )
        .mount(&server)
        .await;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .without_managed_config()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![repo_root.path().to_path_buf()],
            force_reload: true,
            pitchai_principal: None,
        })
        .await?;

    let SkillsListResponse { data } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;
    assert_eq!(data.len(), 1);
    assert!(
        data[0]
            .skills
            .iter()
            .any(|skill| skill.name == "home-skill"),
        "non-plugin skills should remain available"
    );
    assert!(
        data[0]
            .skills
            .iter()
            .all(|skill| skill.name != "demo-plugin:plugin-skill"),
        "plugin skills should be hidden when workspace Codex plugins are disabled"
    );
    Ok(())
}

#[tokio::test]
async fn skills_list_skips_cwd_roots_when_environment_disabled() -> Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    write_skill(&codex_home, "home-skill")?;
    let repo_skill_dir = cwd.path().join(".codex/skills/repo-skill");
    std::fs::create_dir_all(&repo_skill_dir)?;
    std::fs::write(
        repo_skill_dir.join("SKILL.md"),
        "---\nname: repo-skill\ndescription: from repo root\n---\n\n# Body\n",
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[(CODEX_EXEC_SERVER_URL_ENV_VAR, Some("none"))])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd.path().to_path_buf()],
            force_reload: true,
            pitchai_principal: None,
        })
        .await?;

    let SkillsListResponse { data } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].cwd, cwd.path().to_path_buf());
    assert_eq!(data[0].errors, Vec::new());
    assert!(
        data[0]
            .skills
            .iter()
            .any(|skill| skill.name == "home-skill")
    );
    assert!(
        data[0]
            .skills
            .iter()
            .all(|skill| skill.name != "repo-skill")
    );
    Ok(())
}

#[tokio::test]
async fn skills_list_accepts_relative_cwds() -> Result<()> {
    let codex_home = TempDir::new()?;
    let relative_cwd = std::path::PathBuf::from("relative-cwd");
    std::fs::create_dir_all(codex_home.path().join(&relative_cwd))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![relative_cwd.clone()],
            force_reload: true,
            pitchai_principal: None,
        })
        .await?;

    let SkillsListResponse { data } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].cwd, relative_cwd);
    assert_eq!(data[0].errors, Vec::new());
    Ok(())
}

#[tokio::test]
async fn skills_list_preserves_requested_cwd_order() -> Result<()> {
    let codex_home = TempDir::new()?;
    let first_cwd = TempDir::new()?;
    let second_cwd = TempDir::new()?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![
                first_cwd.path().to_path_buf(),
                second_cwd.path().to_path_buf(),
            ],
            force_reload: true,
            pitchai_principal: None,
        })
        .await?;

    let SkillsListResponse { data } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;
    assert_eq!(
        data.iter()
            .map(|entry| entry.cwd.clone())
            .collect::<Vec<_>>(),
        vec![
            first_cwd.path().to_path_buf(),
            second_cwd.path().to_path_buf(),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn skills_list_uses_cached_result_after_session_default_writes_until_force_reload()
-> Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    // Seed the cwd cache before the cwd-local skill exists.
    let first_request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd.path().to_path_buf()],
            force_reload: false,
            pitchai_principal: None,
        })
        .await?;
    let SkillsListResponse { data: first_data } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(first_request_id)).await??;
    assert_eq!(first_data.len(), 1);
    assert!(
        first_data[0]
            .skills
            .iter()
            .all(|skill| skill.name != "late-extra-skill")
    );

    let skill_dir = cwd.path().join(".codex/skills/late-extra-skill");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: late-extra-skill\ndescription: late skill\n---\n\n# Body\n",
    )?;

    for edits in [
        vec![ConfigEdit {
            key_path: "plan_mode_reasoning_effort".to_string(),
            value: serde_json::json!("high"),
            merge_strategy: MergeStrategy::Replace,
        }],
        vec![ConfigEdit {
            key_path: "service_tier".to_string(),
            value: serde_json::json!("fast"),
            merge_strategy: MergeStrategy::Replace,
        }],
        vec![ConfigEdit {
            key_path: "personality".to_string(),
            value: serde_json::json!("friendly"),
            merge_strategy: MergeStrategy::Replace,
        }],
        vec![
            ConfigEdit {
                key_path: "model".to_string(),
                value: serde_json::json!("gpt-5.4"),
                merge_strategy: MergeStrategy::Replace,
            },
            ConfigEdit {
                key_path: "model_reasoning_effort".to_string(),
                value: serde_json::json!("high"),
                merge_strategy: MergeStrategy::Replace,
            },
        ],
    ] {
        let write_id = mcp
            .send_config_batch_write_request(ConfigBatchWriteParams {
                edits,
                file_path: None,
                expected_version: None,
                reload_user_config: true,
            })
            .await?;
        let _: ConfigWriteResponse =
            timeout(DEFAULT_TIMEOUT, mcp.read_response(write_id)).await??;
    }

    let second_request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd.path().to_path_buf()],
            force_reload: false,
            pitchai_principal: None,
        })
        .await?;
    let SkillsListResponse { data: second_data } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(second_request_id)).await??;
    assert_eq!(second_data.len(), 1);
    assert!(
        second_data[0]
            .skills
            .iter()
            .all(|skill| skill.name != "late-extra-skill")
    );

    let third_request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd.path().to_path_buf()],
            force_reload: true,
            pitchai_principal: None,
        })
        .await?;
    let SkillsListResponse { data: third_data } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(third_request_id)).await??;
    assert_eq!(third_data.len(), 1);
    assert!(
        third_data[0]
            .skills
            .iter()
            .any(|skill| skill.name == "late-extra-skill")
    );
    Ok(())
}

#[tokio::test]
async fn skills_extra_roots_set_updates_process_runtime_roots() -> Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let extra_root = TempDir::new()?;
    let extra_skills_root = extra_root.path().join("skills");
    let skill_dir = extra_skills_root.join("runtime-skill");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: runtime-skill\ndescription: runtime skill\n---\n\n# Body\n",
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let set_request_id = mcp
        .send_skills_extra_roots_set_request(SkillsExtraRootsSetParams {
            extra_roots: vec![AbsolutePathBuf::from_absolute_path(&extra_skills_root)?],
        })
        .await?;
    let _: SkillsExtraRootsSetResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(set_request_id)).await??;
    expect_skills_changed_notification(&mut mcp, DEFAULT_TIMEOUT).await?;

    let skills_request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd.path().to_path_buf()],
            force_reload: false,
            pitchai_principal: None,
        })
        .await?;
    let SkillsListResponse { data } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(skills_request_id)).await??;
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].errors, Vec::new());
    assert!(
        data[0]
            .skills
            .iter()
            .any(|skill| skill.name == "runtime-skill")
    );

    let missing_root = extra_root.path().join("missing-skills");
    let reset_request_id = mcp
        .send_skills_extra_roots_set_request(SkillsExtraRootsSetParams {
            extra_roots: vec![AbsolutePathBuf::from_absolute_path(&missing_root)?],
        })
        .await?;
    let _: SkillsExtraRootsSetResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(reset_request_id)).await??;
    expect_skills_changed_notification(&mut mcp, DEFAULT_TIMEOUT).await?;

    let skills_request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd.path().to_path_buf()],
            force_reload: false,
            pitchai_principal: None,
        })
        .await?;
    let SkillsListResponse { data } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(skills_request_id)).await??;
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].errors, Vec::new());
    assert!(
        data[0]
            .skills
            .iter()
            .all(|skill| skill.name != "runtime-skill")
    );

    let clear_request_id = mcp
        .send_skills_extra_roots_set_request(SkillsExtraRootsSetParams {
            extra_roots: Vec::new(),
        })
        .await?;
    let _: SkillsExtraRootsSetResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(clear_request_id)).await??;
    expect_skills_changed_notification(&mut mcp, DEFAULT_TIMEOUT).await?;
    let skills_request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd.path().to_path_buf()],
            force_reload: false,
            pitchai_principal: None,
        })
        .await?;
    let SkillsListResponse { data } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(skills_request_id)).await??;
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].errors, Vec::new());
    assert!(
        data[0]
            .skills
            .iter()
            .all(|skill| skill.name != "runtime-skill")
    );

    drop(mcp);
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let skills_request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![cwd.path().to_path_buf()],
            force_reload: false,
            pitchai_principal: None,
        })
        .await?;
    let SkillsListResponse { data } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(skills_request_id)).await??;
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].errors, Vec::new());
    assert!(
        data[0]
            .skills
            .iter()
            .all(|skill| skill.name != "runtime-skill")
    );
    Ok(())
}

#[tokio::test]
async fn skills_changed_notification_is_emitted_after_skill_change() -> Result<()> {
    // TODO(anp): Remove after skill watching can bridge host-local storage into remote exec.
    skip_if_remote!(
        Ok(()),
        "host-local skill changes are not visible to remote executors"
    );

    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_root_config(&format!("chatgpt_base_url = \"{}\"", server.uri()))
        .write(codex_home.path())?;
    write_skill(&codex_home, "demo")?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let initial_skills_request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![codex_home.path().to_path_buf()],
            force_reload: true,
            pitchai_principal: None,
        })
        .await?;
    let SkillsListResponse { data } = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_response(initial_skills_request_id),
    )
    .await??;
    assert_eq!(data.len(), 1);
    assert!(
        data[0]
            .skills
            .iter()
            .any(|skill| { skill.name == "demo" && skill.description == "demo description" })
    );

    let thread_start_request_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: None,
            model_provider: None,
            allow_provider_model_fallback: false,
            service_tier: None,
            cwd: None,
            runtime_workspace_roots: None,
            approval_policy: None,
            approvals_reviewer: None,
            sandbox: None,
            permissions: None,
            config: None,
            service_name: None,
            base_instructions: None,
            developer_instructions: None,
            personality: None,
            multi_agent_mode: None,
            ephemeral: None,
            history_mode: None,
            session_start_source: None,
            thread_source: None,
            dynamic_tools: None,
            environments: None,
            selected_capability_roots: None,
            mock_experimental_field: None,
            experimental_raw_events: false,
        })
        .await?;
    let _: ThreadStartResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(thread_start_request_id)).await??;

    let skill_path = codex_home
        .path()
        .join("skills")
        .join("demo")
        .join("SKILL.md");
    std::fs::write(
        &skill_path,
        "---\nname: demo\ndescription: updated\n---\n\n# Updated\n",
    )?;

    expect_skills_changed_notification(&mut mcp, WATCHER_TIMEOUT).await?;
    let updated_skills_request_id = mcp
        .send_skills_list_request(SkillsListParams {
            cwds: vec![codex_home.path().to_path_buf()],
            force_reload: false,
            pitchai_principal: None,
        })
        .await?;
    let SkillsListResponse { data } = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_response(updated_skills_request_id),
    )
    .await??;
    assert_eq!(data.len(), 1);
    assert!(
        data[0]
            .skills
            .iter()
            .any(|skill| skill.name == "demo" && skill.description == "updated")
    );
    Ok(())
}
