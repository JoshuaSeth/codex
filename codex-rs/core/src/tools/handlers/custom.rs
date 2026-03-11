use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;

use crate::exec::ExecParams;
use crate::exec_env::create_env;
use crate::exec_policy::ExecApprovalRequest;
use crate::function_tool::FunctionCallError;
use crate::protocol::ExecCommandSource;
use crate::sandboxing::SandboxPermissions;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::apply_patch::intercept_apply_patch;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::tools::runtimes::shell::ShellRequest;
use crate::tools::runtimes::shell::ShellRuntime;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::spec::ConfigCustomTool;

pub struct CustomToolHandler {
    tools: HashMap<String, ConfigCustomTool>,
}

impl CustomToolHandler {
    pub fn new(tools: Vec<ConfigCustomTool>) -> Self {
        let map = tools
            .into_iter()
            .map(|tool| (tool.name.clone(), tool))
            .collect();
        Self { tools: map }
    }
}

#[async_trait]
impl ToolHandler for CustomToolHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
        } = invocation;

        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(format!(
                "tool {tool_name} expects function arguments"
            )));
        };

        let tool = match self.tools.get(tool_name.as_str()) {
            Some(tool) => tool,
            None => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "unsupported config-defined tool: {tool_name}"
                )));
            }
        };

        let args_json: Value = serde_json::from_str(&arguments).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to parse arguments for {tool_name}: {err}"
            ))
        })?;
        let serialized_args = serde_json::to_string(&args_json).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to serialize arguments for {tool_name}: {err}"
            ))
        })?;

        let mut env = create_env(
            &turn.shell_environment_policy,
            Some(session.conversation_id),
        );
        env.extend(tool.env.clone());
        env.insert("CODEX_TOOL_ARGS_JSON".to_string(), serialized_args.clone());
        env.insert("CODEX_TOOL_NAME".to_string(), tool.name.clone());
        env.insert("CODEX_TOOL_CALL_ID".to_string(), call_id.clone());
        env.insert(
            "CODEX_CONVERSATION_ID".to_string(),
            session.conversation_id.to_string(),
        );
        env.insert("CODEX_TURN_ID".to_string(), turn.sub_id.clone());
        env.insert(
            "CODEX_TURN_CWD".to_string(),
            turn.cwd.to_string_lossy().into_owned(),
        );

        let sandbox_permissions = match tool.with_escalated_permissions {
            Some(true) => SandboxPermissions::RequireEscalated,
            _ => SandboxPermissions::UseDefault,
        };

        let mut exec_params = ExecParams {
            command: tool.command.clone(),
            cwd: turn.resolve_path(tool.cwd.clone()),
            expiration: tool.timeout_ms.into(),
            env,
            network: turn.network.clone(),
            sandbox_permissions,
            windows_sandbox_level: turn.windows_sandbox_level,
            justification: None,
            arg0: None,
        };

        let dependency_env = session.dependency_env().await;
        if !dependency_env.is_empty() {
            exec_params.env.extend(dependency_env.clone());
        }

        let mut explicit_env_overrides = turn.shell_environment_policy.r#set.clone();
        for key in dependency_env.keys() {
            if let Some(value) = exec_params.env.get(key) {
                explicit_env_overrides.insert(key.clone(), value.clone());
            }
        }

        // Approval policy guard for explicit escalation in non-OnRequest modes.
        if exec_params
            .sandbox_permissions
            .requires_escalated_permissions()
            && !matches!(
                turn.approval_policy.value(),
                codex_protocol::protocol::AskForApproval::OnRequest
            )
        {
            let approval_policy = turn.approval_policy.value();
            return Err(FunctionCallError::RespondToModel(format!(
                "approval policy is {approval_policy:?}; reject command - you should not ask for escalated permissions if the approval policy is {approval_policy:?}"
            )));
        }

        // Intercept apply_patch if present.
        if let Some(output) = intercept_apply_patch(
            &exec_params.command,
            &exec_params.cwd,
            exec_params.expiration.timeout_ms(),
            session.clone(),
            turn.clone(),
            Some(&tracker),
            &call_id,
            tool_name.as_str(),
        )
        .await?
        {
            return Ok(output);
        }

        let source = ExecCommandSource::Agent;
        let emitter = ToolEmitter::shell(
            exec_params.command.clone(),
            exec_params.cwd.clone(),
            source,
            false,
        );
        let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, None);
        emitter.begin(event_ctx).await;

        let exec_approval_requirement = session
            .services
            .exec_policy
            .create_exec_approval_requirement_for_command(ExecApprovalRequest {
                command: &exec_params.command,
                approval_policy: turn.approval_policy.value(),
                sandbox_policy: turn.sandbox_policy.get(),
                sandbox_permissions: exec_params.sandbox_permissions,
                prefix_rule: None,
            })
            .await;

        let req = ShellRequest {
            command: exec_params.command.clone(),
            cwd: exec_params.cwd.clone(),
            timeout_ms: exec_params.expiration.timeout_ms(),
            env: exec_params.env.clone(),
            explicit_env_overrides,
            network: exec_params.network.clone(),
            sandbox_permissions: exec_params.sandbox_permissions,
            additional_permissions: None,
            justification: exec_params.justification.clone(),
            exec_approval_requirement,
        };
        let mut orchestrator = ToolOrchestrator::new();
        let mut runtime = ShellRuntime::new();
        let tool_ctx = ToolCtx {
            session: session.clone(),
            turn: turn.clone(),
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
        };
        let out = orchestrator
            .run(
                &mut runtime,
                &req,
                &tool_ctx,
                &turn,
                turn.approval_policy.value(),
            )
            .await
            .map(|result| result.output);
        let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, None);
        let content = emitter.finish(event_ctx, out).await?;

        Ok(ToolOutput::Function {
            body: codex_protocol::models::FunctionCallOutputBody::Text(content),
            success: Some(true),
        })
    }
}
