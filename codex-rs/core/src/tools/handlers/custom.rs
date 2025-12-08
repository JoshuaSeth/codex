use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;

use crate::exec::ExecParams;
use crate::exec_env::create_env;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::shell::ShellHandler;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
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

        let mut env = create_env(&turn.shell_environment_policy);
        env.extend(tool.env.clone());
        env.insert("CODEX_TOOL_ARGS_JSON".to_string(), serialized_args.clone());
        env.insert("CODEX_TOOL_NAME".to_string(), tool.name.clone());
        env.insert("CODEX_TOOL_CALL_ID".to_string(), call_id.clone());
        env.insert(
            "CODEX_CONVERSATION_ID".to_string(),
            session.conversation_id().to_string(),
        );
        env.insert("CODEX_TURN_ID".to_string(), turn.sub_id.clone());
        env.insert(
            "CODEX_TURN_CWD".to_string(),
            turn.cwd.to_string_lossy().into_owned(),
        );

        let exec_params = ExecParams {
            command: tool.command.clone(),
            cwd: turn.resolve_path(tool.cwd.clone()),
            expiration: tool.timeout_ms.into(),
            env,
            with_escalated_permissions: tool.with_escalated_permissions,
            justification: None,
            arg0: None,
        };

        let output = ShellHandler::run_exec_like(
            tool_name.as_str(),
            exec_params,
            session,
            turn,
            tracker,
            call_id,
            false,
        )
        .await?;

        if tool.shutdown_after_call {
            if let ToolOutput::Function {
                content,
                content_items,
                success,
            } = output
            {
                return Ok(ToolOutput::Pending {
                    content,
                    content_items,
                    success,
                    shutdown: true,
                });
            }
        }

        Ok(output)
    }
}
