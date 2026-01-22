// - In the default output mode, it is paramount that the only thing written to
//   stdout is the final message (if any).
// - In --json mode, stdout must be valid JSONL, one event per line.
// For both modes, any other output must be written to stderr.
#![deny(clippy::print_stdout)]

mod cli;
mod event_processor;
mod event_processor_with_human_output;
pub mod event_processor_with_jsonl_output;
pub mod exec_events;
mod pending_tool_ipc;
mod prompt_sequence;

use anyhow::Context;
pub use cli::Cli;
pub use cli::Command;
pub use cli::ReviewArgs;
use codex_common::oss::ensure_oss_provider_ready;
use codex_common::oss::get_default_model_for_oss_provider;
use codex_core::AuthManager;
use codex_core::LMSTUDIO_OSS_PROVIDER_ID;
use codex_core::NewThread;
use codex_core::OLLAMA_OSS_PROVIDER_ID;
use codex_core::ThreadManager;
use codex_core::auth::enforce_login_restrictions;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::config::find_codex_home;
use codex_core::config::load_config_as_toml_with_cli_overrides;
use codex_core::config::resolve_oss_provider;
use codex_core::git_info::get_git_repo_root;
use codex_core::live_status::LiveFrontend;
use codex_core::live_status::LiveIpc;
use codex_core::live_status::LiveSessionStatus;
use codex_core::live_status::LiveStatusDetail;
use codex_core::live_status::LiveStatusRecordV1;
use codex_core::live_status::LiveStatusWriter;
use codex_core::live_status::LiveStatusWriterConfig;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::Event;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::ReviewRequest;
use codex_core::protocol::ReviewTarget;
use codex_core::protocol::SessionSource;
use codex_protocol::approvals::ElicitationAction;
use codex_protocol::config_types::SandboxMode;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use event_processor_with_human_output::EventProcessorWithHumanOutput;
use event_processor_with_jsonl_output::EventProcessorWithJsonOutput;
use serde_json::Value;
use std::io::IsTerminal;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use supports_color::Stream;
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

use crate::cli::Command as ExecCommand;
use crate::cli::DeliverPendingArgs;
use crate::event_processor::CodexStatus;
use crate::event_processor::EventProcessor;
use crate::pending_tool_ipc::PendingToolServer;
use crate::pending_tool_ipc::addr_from_metadata;
use crate::pending_tool_ipc::load_metadata;
use crate::pending_tool_ipc::send_pending_result;
use crate::prompt_sequence::PromptSequenceRunner;
use codex_core::default_client::set_default_originator;
use codex_core::find_conversation_path_by_selector_str;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;

enum InitialOperation {
    UserTurn {
        items: Vec<UserInput>,
        output_schema: Option<Value>,
    },
    Review {
        review_request: ReviewRequest,
    },
}

pub async fn run_main(cli: Cli, codex_linux_sandbox_exe: Option<PathBuf>) -> anyhow::Result<()> {
    if let Err(err) = set_default_originator("codex_exec".to_string()) {
        tracing::warn!(?err, "Failed to set codex exec originator override {err:?}");
    }

    let Cli {
        command,
        images,
        model: model_cli_arg,
        oss,
        oss_provider,
        config_profile,
        full_auto,
        dangerously_bypass_approvals_and_sandbox,
        cwd,
        skip_git_repo_check,
        prompt_sequence,
        add_dir,
        color,
        last_message_file,
        json: json_mode,
        sandbox_mode: sandbox_mode_cli_arg,
        prompt,
        output_schema: output_schema_path,
        config_overrides,
    } = cli;

    let command = match command {
        Some(Command::DeliverPending(args)) => {
            run_deliver_pending(args).await?;
            return Ok(());
        }
        other => other,
    };

    if prompt_sequence.is_some() {
        if command.is_some() {
            eprintln!(
                "--prompt-sequence cannot be combined with exec subcommands like review or resume."
            );
            std::process::exit(1);
        }
        if prompt.is_some() {
            eprintln!("PROMPT arguments cannot be provided when using --prompt-sequence.");
            std::process::exit(1);
        }
        if !images.is_empty() {
            eprintln!(
                "--image is not supported together with --prompt-sequence. Attachments should be listed in the sequence file."
            );
            std::process::exit(1);
        }
    }

    let (stdout_with_ansi, stderr_with_ansi) = match color {
        cli::Color::Always => (true, true),
        cli::Color::Never => (false, false),
        cli::Color::Auto => (
            supports_color::on_cached(Stream::Stdout).is_some(),
            supports_color::on_cached(Stream::Stderr).is_some(),
        ),
    };

    // Build fmt layer (existing logging) to compose with OTEL layer.
    let default_level = "error";

    // Build env_filter separately and attach via with_filter.
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(default_level))
        .unwrap_or_else(|_| EnvFilter::new(default_level));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(stderr_with_ansi)
        .with_writer(std::io::stderr)
        .with_filter(env_filter);

    let sandbox_mode = if full_auto {
        Some(SandboxMode::WorkspaceWrite)
    } else if dangerously_bypass_approvals_and_sandbox {
        Some(SandboxMode::DangerFullAccess)
    } else {
        sandbox_mode_cli_arg.map(Into::<SandboxMode>::into)
    };

    // Parse `-c` overrides from the CLI.
    let cli_kv_overrides = match config_overrides.parse_overrides() {
        Ok(v) => v,
        #[allow(clippy::print_stderr)]
        Err(e) => {
            eprintln!("Error parsing -c overrides: {e}");
            std::process::exit(1);
        }
    };

    let resolved_cwd = cwd.clone();
    let config_cwd = match resolved_cwd.as_deref() {
        Some(path) => AbsolutePathBuf::from_absolute_path(path.canonicalize()?)?,
        None => AbsolutePathBuf::current_dir()?,
    };

    // we load config.toml here to determine project state.
    #[allow(clippy::print_stderr)]
    let config_toml = {
        let codex_home = match find_codex_home() {
            Ok(codex_home) => codex_home,
            Err(err) => {
                eprintln!("Error finding codex home: {err}");
                std::process::exit(1);
            }
        };

        match load_config_as_toml_with_cli_overrides(
            &codex_home,
            &config_cwd,
            cli_kv_overrides.clone(),
        )
        .await
        {
            Ok(config_toml) => config_toml,
            Err(err) => {
                eprintln!("Error loading config.toml: {err}");
                std::process::exit(1);
            }
        }
    };

    let model_provider = if oss {
        let resolved = resolve_oss_provider(
            oss_provider.as_deref(),
            &config_toml,
            config_profile.clone(),
        );

        if let Some(provider) = resolved {
            Some(provider)
        } else {
            return Err(anyhow::anyhow!(
                "No default OSS provider configured. Use --local-provider=provider or set oss_provider to either {LMSTUDIO_OSS_PROVIDER_ID} or {OLLAMA_OSS_PROVIDER_ID} in config.toml"
            ));
        }
    } else {
        None // No OSS mode enabled
    };

    // When using `--oss`, let the bootstrapper pick the model based on selected provider
    let model = if let Some(model) = model_cli_arg {
        Some(model)
    } else if oss {
        model_provider
            .as_ref()
            .and_then(|provider_id| get_default_model_for_oss_provider(provider_id))
            .map(std::borrow::ToOwned::to_owned)
    } else {
        None // No model specified, will use the default.
    };

    // Load configuration and determine approval policy
    let overrides = ConfigOverrides {
        model,
        review_model: None,
        config_profile,
        // Default to never ask for approvals in headless mode. Feature flags can override.
        approval_policy: Some(AskForApproval::Never),
        sandbox_mode,
        cwd: resolved_cwd,
        model_provider: model_provider.clone(),
        codex_linux_sandbox_exe,
        base_instructions: None,
        developer_instructions: None,
        compact_prompt: None,
        include_apply_patch_tool: None,
        show_raw_agent_reasoning: oss.then_some(true),
        tools_web_search_request: None,
        additional_writable_roots: add_dir,
    };

    let config =
        Config::load_with_cli_overrides_and_harness_overrides(cli_kv_overrides, overrides).await?;

    if let Err(err) = enforce_login_restrictions(&config) {
        eprintln!("{err}");
        std::process::exit(1);
    }

    let otel =
        codex_core::otel_init::build_provider(&config, env!("CARGO_PKG_VERSION"), None, true);

    #[allow(clippy::print_stderr)]
    let otel = match otel {
        Ok(otel) => otel,
        Err(e) => {
            eprintln!("Could not create otel exporter: {e}");
            std::process::exit(1);
        }
    };

    let otel_logger_layer = otel.as_ref().and_then(|o| o.logger_layer());

    let otel_tracing_layer = otel.as_ref().and_then(|o| o.tracing_layer());

    let _ = tracing_subscriber::registry()
        .with(fmt_layer)
        .with(otel_tracing_layer)
        .with(otel_logger_layer)
        .try_init();

    let mut event_processor: Box<dyn EventProcessor> = match json_mode {
        true => Box::new(EventProcessorWithJsonOutput::new(last_message_file.clone())),
        _ => Box::new(EventProcessorWithHumanOutput::create_with_ansi(
            stdout_with_ansi,
            &config,
            last_message_file.clone(),
        )),
    };

    if oss {
        // We're in the oss section, so provider_id should be Some
        // Let's handle None case gracefully though just in case
        let provider_id = match model_provider.as_ref() {
            Some(id) => id,
            None => {
                error!("OSS provider unexpectedly not set when oss flag is used");
                return Err(anyhow::anyhow!(
                    "OSS provider not set but oss flag was used"
                ));
            }
        };
        ensure_oss_provider_ready(provider_id, &config)
            .await
            .map_err(|e| anyhow::anyhow!("OSS setup failed: {e}"))?;
    }

    let default_cwd = config.cwd.to_path_buf();
    let default_approval_policy = config.approval_policy.value();
    let default_sandbox_policy = config.sandbox_policy.get();
    let default_effort = config.model_reasoning_effort;
    let default_summary = config.model_reasoning_summary;

    if !skip_git_repo_check && get_git_repo_root(&default_cwd).is_none() {
        eprintln!("Not inside a trusted directory and --skip-git-repo-check was not specified.");
        std::process::exit(1);
    }

    let auth_manager = AuthManager::shared(
        config.codex_home.clone(),
        true,
        config.cli_auth_credentials_store_mode,
    );
    let thread_manager = ThreadManager::new(
        config.codex_home.clone(),
        auth_manager.clone(),
        SessionSource::Exec,
    );
    let default_model = thread_manager
        .get_models_manager()
        .get_model(&config.model, &config)
        .await;

    // Handle resume subcommand by resolving a rollout path and using explicit resume API.
    let NewThread {
        thread_id: conversation_id,
        thread: conversation,
        session_configured,
    } = if let Some(ExecCommand::Resume(args)) = command.as_ref() {
        let mut resume_path = resolve_resume_path(&config, args).await?;

        if let Some(path) = resume_path.as_ref() {
            let thread_id = resolve_thread_id_for_resume(args, path).await?;
            wait_for_thread_to_finish_if_running(&config.codex_home, &thread_id).await?;
        }

        if let Some(path) = resume_path.take() {
            thread_manager
                .resume_thread_from_rollout(config.clone(), path, auth_manager.clone())
                .await?
        } else {
            thread_manager.start_thread(config.clone()).await?
        }
    } else {
        thread_manager.start_thread(config.clone()).await?
    };

    let pending_tool_server = match PendingToolServer::start(conversation.clone()).await {
        Ok(server) => Some(server),
        Err(err) => {
            warn!(?err, "failed to start pending tool IPC server");
            None
        }
    };

    let cli_version = Some(env!("CARGO_PKG_VERSION").to_string());
    let mut live_status = match LiveStatusWriter::spawn(LiveStatusWriterConfig {
        codex_home: config.codex_home.clone(),
        thread_id: conversation_id,
        frontend: LiveFrontend::Exec,
        status: LiveSessionStatus::Running,
        detail: None,
        cwd: Some(config.cwd.clone()),
        cli_version,
        heartbeat_interval: None,
    }) {
        Ok(writer) => Some(writer),
        Err(err) => {
            warn!(?err, "failed to start live status writer");
            None
        }
    };

    if let (Some(writer), Some(server)) = (live_status.as_ref(), pending_tool_server.as_ref()) {
        let addr = server.addr();
        writer.set_ipc(LiveIpc {
            host: addr.ip().to_string(),
            port: addr.port(),
        });
    }

    let mut prompt_sequence_runner = match prompt_sequence {
        Some(path) => Some(PromptSequenceRunner::load(&path)?),
        None => None,
    };

    let output_schema = load_output_schema(output_schema_path);

    let mut initial_sequence_entry = None;
    if let Some(runner) = prompt_sequence_runner.as_mut() {
        initial_sequence_entry = Some(runner.next_entry().ok_or_else(|| {
            anyhow::anyhow!(
                "prompt-sequence {} did not contain any steps",
                runner.source().display()
            )
        })?);
    }

    let (initial_operation, prompt_summary) = if let Some(entry) = initial_sequence_entry {
        let description = format!(
            "{} ({}/{})",
            entry.description,
            entry.index + 1,
            entry.total
        );
        (
            InitialOperation::UserTurn {
                items: entry.items,
                output_schema: output_schema.clone(),
            },
            description,
        )
    } else {
        match (command, prompt, images) {
            (Some(ExecCommand::Review(review_cli)), _, _) => {
                let review_request = build_review_request(review_cli)?;
                let summary = codex_core::review_prompts::user_facing_hint(&review_request.target);
                (InitialOperation::Review { review_request }, summary)
            }
            (Some(ExecCommand::Resume(args)), root_prompt, imgs) => {
                if args.no_prompt && root_prompt.is_some() {
                    eprintln!(
                        "--no-prompt cannot be combined with a PROMPT argument or piped stdin input."
                    );
                    std::process::exit(1);
                }

                if args.no_prompt {
                    let items: Vec<UserInput> = imgs
                        .into_iter()
                        .chain(args.images.into_iter())
                        .map(|path| UserInput::LocalImage { path })
                        .collect();
                    (
                        InitialOperation::UserTurn {
                            items,
                            output_schema: output_schema.clone(),
                        },
                        "(resume without prompt)".to_string(),
                    )
                } else {
                    let prompt_arg = args
                        .prompt
                        .clone()
                        .or_else(|| {
                            if args.last {
                                args.session_id.clone()
                            } else {
                                None
                            }
                        })
                        .or(root_prompt);
                    let prompt_text = resolve_prompt(prompt_arg);
                    let mut items: Vec<UserInput> = imgs
                        .into_iter()
                        .chain(args.images.into_iter())
                        .map(|path| UserInput::LocalImage { path })
                        .collect();
                    items.push(UserInput::Text {
                        text: prompt_text.clone(),
                    });
                    (
                        InitialOperation::UserTurn {
                            items,
                            output_schema: output_schema.clone(),
                        },
                        prompt_text,
                    )
                }
            }
            (None, root_prompt, imgs) => {
                let prompt_text = resolve_prompt(root_prompt);
                let mut items: Vec<UserInput> = imgs
                    .into_iter()
                    .map(|path| UserInput::LocalImage { path })
                    .collect();
                items.push(UserInput::Text {
                    text: prompt_text.clone(),
                });
                (
                    InitialOperation::UserTurn {
                        items,
                        output_schema: output_schema.clone(),
                    },
                    prompt_text,
                )
            }
            (Some(ExecCommand::DeliverPending(_)), _, _) => {
                unreachable!("deliver command handled earlier")
            }
        }
    };

    // Print the effective configuration and initial request so users can see what Codex
    // is using.
    event_processor.print_config_summary(&config, &prompt_summary, &session_configured);

    info!("Codex initialized with event: {session_configured:?}");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    {
        let conversation = conversation.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        tracing::debug!("Keyboard interrupt");
                        // Immediately notify Codex to abort any in‑flight task.
                        conversation.submit(Op::Interrupt).await.ok();

                        // Exit the inner loop and return to the main input prompt. The codex
                        // will emit a `TurnInterrupted` (Error) event which is drained later.
                        break;
                    }
                    res = conversation.next_event() => match res {
                        Ok(event) => {
                            debug!("Received event: {event:?}");

                            let is_shutdown_complete = matches!(event.msg, EventMsg::ShutdownComplete);
                            if let Err(e) = tx.send(event) {
                                error!("Error sending event: {e:?}");
                                break;
                            }
                            if is_shutdown_complete {
                                info!("Received shutdown event, exiting event loop.");
                                break;
                            }
                        },
                        Err(e) => {
                            error!("Error receiving event: {e:?}");
                            break;
                        }
                    }
                }
            }
        });
    }

    match initial_operation {
        InitialOperation::UserTurn {
            items,
            output_schema,
        } => {
            let task_id = conversation
                .submit(Op::UserTurn {
                    items,
                    cwd: default_cwd.clone(),
                    approval_policy: default_approval_policy,
                    sandbox_policy: default_sandbox_policy.clone(),
                    model: default_model.clone(),
                    effort: default_effort,
                    summary: default_summary,
                    final_output_json_schema: output_schema,
                })
                .await?;
            info!("Sent prompt with event ID: {task_id}");
            task_id
        }
        InitialOperation::Review { review_request } => {
            let task_id = conversation.submit(Op::Review { review_request }).await?;
            info!("Sent review request with event ID: {task_id}");
            task_id
        }
    };

    // Run the loop until the task is complete.
    // Track whether a fatal error was reported by the server so we can
    // exit with a non-zero status for automation-friendly signaling.
    let mut error_seen = false;
    while let Some(event) = rx.recv().await {
        let mut queued_sequence_step = None;
        if let Some(runner) = prompt_sequence_runner.as_mut()
            && matches!(&event.msg, EventMsg::TurnComplete(_))
            && runner.has_remaining()
        {
            queued_sequence_step = runner.next_entry();
        }

        if let EventMsg::ElicitationRequest(ev) = &event.msg {
            // Automatically cancel elicitation requests in exec mode.
            conversation
                .submit(Op::ResolveElicitation {
                    server_name: ev.server_name.clone(),
                    request_id: ev.id.clone(),
                    decision: ElicitationAction::Cancel,
                })
                .await?;
        }
        if let (Some(writer), EventMsg::PendingToolState(ev)) = (live_status.as_ref(), &event.msg) {
            match ev.status {
                codex_core::protocol::PendingToolStatus::Waiting => {
                    writer.set_status(
                        LiveSessionStatus::WaitingPendingTool,
                        Some(LiveStatusDetail {
                            call_id: Some(ev.call_id.clone()),
                            tool_name: Some(ev.tool_name.clone()),
                            turn_id: Some(ev.turn_id.clone()),
                            note: ev.note.clone(),
                            ..Default::default()
                        }),
                    );
                }
                codex_core::protocol::PendingToolStatus::Resolved
                | codex_core::protocol::PendingToolStatus::Cancelled => {
                    writer.set_status(LiveSessionStatus::Running, None);
                }
            }
        }
        if matches!(&event.msg, EventMsg::Error(_)) {
            error_seen = true;
        }
        let mut shutdown: CodexStatus = event_processor.process_event(event);

        if let Some(entry) = queued_sequence_step {
            info!(
                "Prompt sequence: launching step {}/{} ({})",
                entry.index + 1,
                entry.total,
                entry.description
            );
            conversation
                .submit(Op::UserTurn {
                    items: entry.items,
                    cwd: default_cwd.clone(),
                    approval_policy: default_approval_policy,
                    sandbox_policy: default_sandbox_policy.clone(),
                    model: default_model.clone(),
                    effort: default_effort,
                    summary: default_summary,
                    final_output_json_schema: output_schema.clone(),
                })
                .await?;
            shutdown = CodexStatus::Running;
        }

        match shutdown {
            CodexStatus::Running => continue,
            CodexStatus::InitiateShutdown => {
                conversation.submit(Op::Shutdown).await?;
            }
            CodexStatus::Shutdown => {
                break;
            }
        }
    }
    event_processor.print_final_output();
    if let Some(writer) = live_status.take() {
        let status = if error_seen {
            LiveSessionStatus::Errored
        } else {
            LiveSessionStatus::Completed
        };
        writer
            .shutdown(
                status,
                Some(if error_seen {
                    "fatal error seen".to_string()
                } else {
                    "finished".to_string()
                }),
            )
            .await;
    }
    drop(pending_tool_server);

    if error_seen {
        anyhow::bail!("fatal error reported by server");
    }

    Ok(())
}

async fn run_deliver_pending(args: DeliverPendingArgs) -> anyhow::Result<()> {
    let codex_home = find_codex_home().context("failed to locate codex home")?;
    let thread_id = codex_protocol::ThreadId::from_string(&args.session_id)?;
    let metadata_path = LiveStatusRecordV1::path_for(&codex_home, &thread_id);
    let bytes = fs::read(&metadata_path)
        .await
        .with_context(|| format!("failed to read {}", metadata_path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)?;
    let meta = load_metadata(value.clone())?;
    if let Ok(record) = serde_json::from_value::<LiveStatusRecordV1>(value) {
        if !record.alive {
            anyhow::bail!("session {} is not alive", record.thread_id);
        }
        if let Some(age_s) = heartbeat_age_seconds(&record.last_heartbeat_at)
            && age_s > 30.0
        {
            anyhow::bail!(
                "session {} live record is stale (last heartbeat {:.1}s ago)",
                record.thread_id,
                age_s
            );
        }
    }
    let addr = addr_from_metadata(meta)?;
    let payload = FunctionCallOutputPayload {
        content: args.output,
        success: Some(args.success),
        ..Default::default()
    };
    send_pending_result(addr, args.call_id, payload).await?;
    eprintln!(
        "Delivered pending tool result for session {}.",
        args.session_id
    );
    Ok(())
}

fn heartbeat_age_seconds(last_heartbeat_at: &str) -> Option<f64> {
    let parsed = time::OffsetDateTime::parse(
        last_heartbeat_at,
        &time::format_description::well_known::Rfc3339,
    )
    .ok()?;
    let now = time::OffsetDateTime::now_utc();
    let diff = now - parsed;
    Some(diff.whole_milliseconds() as f64 / 1000.0)
}

async fn resolve_thread_id_for_resume(
    args: &crate::cli::ResumeArgs,
    rollout_path: &Path,
) -> anyhow::Result<codex_protocol::ThreadId> {
    if let Some(selector) = args.session_id.as_deref()
        && let Ok(id) = codex_protocol::ThreadId::from_string(selector)
    {
        return Ok(id);
    }
    read_thread_id_from_rollout_head(rollout_path).await
}

async fn read_thread_id_from_rollout_head(path: &Path) -> anyhow::Result<codex_protocol::ThreadId> {
    use tokio::io::AsyncBufReadExt;

    const HEAD_RECORD_LIMIT: usize = 25;

    let file = fs::File::open(path)
        .await
        .with_context(|| format!("failed to open rollout {}", path.display()))?;
    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();
    let mut non_empty = 0usize;
    while non_empty < HEAD_RECORD_LIMIT {
        let line_opt = lines.next_line().await?;
        let Some(line) = line_opt else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        non_empty += 1;

        let Ok(rollout_line) = serde_json::from_str::<RolloutLine>(trimmed) else {
            continue;
        };
        if let RolloutItem::SessionMeta(meta) = rollout_line.item {
            return Ok(meta.meta.id);
        }
    }

    anyhow::bail!(
        "failed to resolve thread id from rollout {}",
        path.display()
    )
}

async fn wait_for_thread_to_finish_if_running(
    codex_home: &Path,
    thread_id: &codex_protocol::ThreadId,
) -> anyhow::Result<()> {
    let status_path = LiveStatusRecordV1::path_for(codex_home, thread_id);

    let mut printed_wait_message = false;
    loop {
        let bytes = match fs::read(&status_path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("failed reading live status {}", status_path.display())
                });
            }
        };
        let value: Value = serde_json::from_slice(&bytes).with_context(|| {
            format!("failed parsing live status json {}", status_path.display())
        })?;

        let alive = value
            .get("alive")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !alive {
            return Ok(());
        }

        let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if matches!(status, "completed" | "errored") {
            return Ok(());
        }

        let last_heartbeat_at = value
            .get("last_heartbeat_at")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let age_s = heartbeat_age_seconds(last_heartbeat_at).unwrap_or(f64::INFINITY);
        if age_s > 30.0 {
            // Best-effort: treat stale records as not-running so users can recover after a crash.
            return Ok(());
        }

        if !printed_wait_message {
            printed_wait_message = true;
            eprintln!(
                "Session {thread_id} appears to be running; waiting for it to finish before resuming (Ctrl+C to cancel)."
            );
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                anyhow::bail!("interrupted while waiting for session {thread_id} to finish");
            }
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
}

async fn resolve_resume_path(
    config: &Config,
    args: &crate::cli::ResumeArgs,
) -> anyhow::Result<Option<PathBuf>> {
    let resolved = if args.last {
        let default_provider_filter = vec![config.model_provider_id.clone()];
        match codex_core::RolloutRecorder::list_threads(
            &config.codex_home,
            1,
            None,
            &[],
            Some(default_provider_filter.as_slice()),
            &config.model_provider_id,
        )
        .await
        {
            Ok(page) => page.items.first().map(|it| it.path.clone()),
            Err(e) => {
                error!("Error listing threads: {e}");
                None
            }
        }
    } else if let Some(id_str) = args.session_id.as_deref() {
        find_conversation_path_by_selector_str(&config.codex_home, id_str).await?
    } else {
        None
    };

    if args.fork {
        let Some(path) = resolved.as_ref() else {
            return Ok(None);
        };
        return Ok(Some(fork_rollout_file(&config.codex_home, path).await?));
    }

    Ok(resolved)
}

async fn fork_rollout_file(codex_home: &Path, source_path: &Path) -> anyhow::Result<PathBuf> {
    let content = fs::read_to_string(source_path)
        .await
        .with_context(|| format!("failed to read {}", source_path.display()))?;
    let mut lines = content.lines();
    let first = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("rollout file missing session meta line"))?;
    let mut first_json: Value = serde_json::from_str(first)
        .with_context(|| format!("invalid session meta json in {}", source_path.display()))?;

    let new_id = codex_protocol::ThreadId::default();

    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let format: &[FormatItem] =
        format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]");
    let ts_str = now
        .format(format)
        .with_context(|| "failed to format fork timestamp")?;

    let payload = first_json
        .get_mut("payload")
        .ok_or_else(|| anyhow::anyhow!("session meta line missing payload"))?;
    payload["id"] = Value::String(new_id.to_string());
    payload["timestamp"] = Value::String(ts_str.clone());
    first_json["timestamp"] = Value::String(ts_str.clone());

    let mut dir = codex_home.to_path_buf();
    dir.push("sessions");
    dir.push(format!("{:04}", now.year()));
    dir.push(format!("{:02}", u8::from(now.month())));
    dir.push(format!("{:02}", now.day()));
    fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("failed to create {}", dir.display()))?;

    let filename = format!("rollout-{ts_str}-{new_id}.jsonl");
    let dest_path = dir.join(filename);
    let mut out = fs::File::create(&dest_path)
        .await
        .with_context(|| format!("failed to create {}", dest_path.display()))?;
    out.write_all(serde_json::to_string(&first_json)?.as_bytes())
        .await?;
    out.write_all(b"\n").await?;

    for line in lines {
        out.write_all(line.as_bytes()).await?;
        out.write_all(b"\n").await?;
    }
    out.flush().await?;
    Ok(dest_path)
}

fn load_output_schema(path: Option<PathBuf>) -> Option<Value> {
    let path = path?;

    let schema_str = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) => {
            eprintln!(
                "Failed to read output schema file {}: {err}",
                path.display()
            );
            std::process::exit(1);
        }
    };

    match serde_json::from_str::<Value>(&schema_str) {
        Ok(value) => Some(value),
        Err(err) => {
            eprintln!(
                "Output schema file {} is not valid JSON: {err}",
                path.display()
            );
            std::process::exit(1);
        }
    }
}

fn resolve_prompt(prompt_arg: Option<String>) -> String {
    match prompt_arg {
        Some(p) if p != "-" => p,
        maybe_dash => {
            let force_stdin = matches!(maybe_dash.as_deref(), Some("-"));

            if std::io::stdin().is_terminal() && !force_stdin {
                eprintln!(
                    "No prompt provided. Either specify one as an argument or pipe the prompt into stdin."
                );
                std::process::exit(1);
            }

            if !force_stdin {
                eprintln!("Reading prompt from stdin...");
            }
            let mut buffer = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut buffer) {
                eprintln!("Failed to read prompt from stdin: {e}");
                std::process::exit(1);
            } else if buffer.trim().is_empty() {
                eprintln!("No prompt provided via stdin.");
                std::process::exit(1);
            }
            buffer
        }
    }
}

fn build_review_request(args: ReviewArgs) -> anyhow::Result<ReviewRequest> {
    let target = if args.uncommitted {
        ReviewTarget::UncommittedChanges
    } else if let Some(branch) = args.base {
        ReviewTarget::BaseBranch { branch }
    } else if let Some(sha) = args.commit {
        ReviewTarget::Commit {
            sha,
            title: args.commit_title,
        }
    } else if let Some(prompt_arg) = args.prompt {
        let prompt = resolve_prompt(Some(prompt_arg)).trim().to_string();
        if prompt.is_empty() {
            anyhow::bail!("Review prompt cannot be empty");
        }
        ReviewTarget::Custom {
            instructions: prompt,
        }
    } else {
        anyhow::bail!(
            "Specify --uncommitted, --base, --commit, or provide custom review instructions"
        );
    };

    Ok(ReviewRequest {
        target,
        user_facing_hint: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use time::format_description::well_known::Rfc3339;

    #[test]
    fn builds_uncommitted_review_request() {
        let request = build_review_request(ReviewArgs {
            uncommitted: true,
            base: None,
            commit: None,
            commit_title: None,
            prompt: None,
        })
        .expect("builds uncommitted review request");

        let expected = ReviewRequest {
            target: ReviewTarget::UncommittedChanges,
            user_facing_hint: None,
        };

        assert_eq!(request, expected);
    }

    #[test]
    fn builds_commit_review_request_with_title() {
        let request = build_review_request(ReviewArgs {
            uncommitted: false,
            base: None,
            commit: Some("123456789".to_string()),
            commit_title: Some("Add review command".to_string()),
            prompt: None,
        })
        .expect("builds commit review request");

        let expected = ReviewRequest {
            target: ReviewTarget::Commit {
                sha: "123456789".to_string(),
                title: Some("Add review command".to_string()),
            },
            user_facing_hint: None,
        };

        assert_eq!(request, expected);
    }

    #[test]
    fn builds_custom_review_request_trims_prompt() {
        let request = build_review_request(ReviewArgs {
            uncommitted: false,
            base: None,
            commit: None,
            commit_title: None,
            prompt: Some("  custom review instructions  ".to_string()),
        })
        .expect("builds custom review request");

        let expected = ReviewRequest {
            target: ReviewTarget::Custom {
                instructions: "custom review instructions".to_string(),
            },
            user_facing_hint: None,
        };

        assert_eq!(request, expected);
    }

    #[tokio::test]
    async fn resolves_thread_id_for_resume_from_rollout_head() {
        let tmp = TempDir::new().expect("creates tempdir");
        let rollout_path = tmp.path().join("rollout.jsonl");

        let thread_id = codex_protocol::ThreadId::default();
        let meta = codex_protocol::protocol::SessionMeta {
            id: thread_id,
            timestamp: "2026-01-01T00:00:00".to_string(),
            ..Default::default()
        };
        let line = RolloutLine {
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            item: RolloutItem::SessionMeta(codex_protocol::protocol::SessionMetaLine {
                meta,
                git: None,
            }),
        };
        let json = serde_json::to_string(&line).expect("serialize rollout line");
        tokio::fs::write(&rollout_path, format!("{json}\n"))
            .await
            .expect("write rollout file");

        let args = crate::cli::ResumeArgs {
            session_id: None,
            last: false,
            fork: false,
            images: Vec::new(),
            prompt: None,
            no_prompt: false,
        };

        let resolved = resolve_thread_id_for_resume(&args, &rollout_path)
            .await
            .expect("resolve thread id");
        assert_eq!(resolved, thread_id);
    }

    #[tokio::test]
    async fn waits_for_running_session_to_finish_before_resuming() {
        let tmp = TempDir::new().expect("creates tempdir");
        let codex_home = tmp.path();

        let thread_id = codex_protocol::ThreadId::default();
        let status_path = LiveStatusRecordV1::path_for(codex_home, &thread_id);
        tokio::fs::create_dir_all(status_path.parent().expect("live dir"))
            .await
            .expect("create live dir");

        let now = time::OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .expect("format rfc3339");
        let initial = serde_json::json!({
            "alive": true,
            "status": "running",
            "last_heartbeat_at": now,
        });
        tokio::fs::write(&status_path, serde_json::to_vec(&initial).expect("json"))
            .await
            .expect("write live status");

        let status_path_for_task = status_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let done = serde_json::json!({
                "alive": false,
                "status": "completed",
                "last_heartbeat_at": time::OffsetDateTime::now_utc()
                    .format(&Rfc3339)
                    .expect("format rfc3339"),
            });
            let _ = tokio::fs::write(
                &status_path_for_task,
                serde_json::to_vec(&done).expect("json"),
            )
            .await;
        });

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_thread_to_finish_if_running(codex_home, &thread_id),
        )
        .await
        .expect("wait returns")
        .expect("ok");
    }

    #[tokio::test]
    async fn does_not_block_on_stale_live_status() {
        let tmp = TempDir::new().expect("creates tempdir");
        let codex_home = tmp.path();

        let thread_id = codex_protocol::ThreadId::default();
        let status_path = LiveStatusRecordV1::path_for(codex_home, &thread_id);
        tokio::fs::create_dir_all(status_path.parent().expect("live dir"))
            .await
            .expect("create live dir");

        let stale = serde_json::json!({
            "alive": true,
            "status": "running",
            "last_heartbeat_at": "1970-01-01T00:00:00Z",
        });
        tokio::fs::write(&status_path, serde_json::to_vec(&stale).expect("json"))
            .await
            .expect("write live status");

        tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_thread_to_finish_if_running(codex_home, &thread_id),
        )
        .await
        .expect("returns without blocking")
        .expect("ok");
    }
}
