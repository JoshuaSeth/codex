use crate::client_common::tools::ToolSpec;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::function_tool::FunctionCallError;
use crate::sandboxing::SandboxPermissions;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::hooks::TimeoutBehavior;
use crate::tools::hooks::ToolCallSnapshot;
use crate::tools::hooks::ToolHookDirective;
use crate::tools::hooks::ToolHookEvent;
use crate::tools::registry::ConfiguredToolSpec;
use crate::tools::registry::ToolRegistry;
use crate::tools::spec::ToolsConfig;
use crate::tools::spec::build_specs;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::ShellToolCallParams;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;
use tracing::instrument;
use tracing::warn;

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub tool_name: String,
    pub call_id: String,
    pub payload: ToolPayload,
}

pub struct ToolRouter {
    registry: ToolRegistry,
    specs: Vec<ConfiguredToolSpec>,
}

impl ToolRouter {
    pub fn from_config(
        config: &ToolsConfig,
        mcp_tools: Option<HashMap<String, mcp_types::Tool>>,
    ) -> Self {
        let builder = build_specs(config, mcp_tools);
        let (specs, registry) = builder.build();

        Self { registry, specs }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs
            .iter()
            .map(|config| config.spec.clone())
            .collect()
    }

    pub fn tool_supports_parallel(&self, tool_name: &str) -> bool {
        self.specs
            .iter()
            .filter(|config| config.supports_parallel_tool_calls)
            .any(|config| config.spec.name() == tool_name)
    }

    #[instrument(level = "trace", skip_all, err)]
    pub async fn build_tool_call(
        session: &Session,
        item: ResponseItem,
    ) -> Result<Option<ToolCall>, FunctionCallError> {
        match item {
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                if let Some((server, tool)) = session.parse_mcp_tool_name(&name).await {
                    Ok(Some(ToolCall {
                        tool_name: name,
                        call_id,
                        payload: ToolPayload::Mcp {
                            server,
                            tool,
                            raw_arguments: arguments,
                        },
                    }))
                } else {
                    Ok(Some(ToolCall {
                        tool_name: name,
                        call_id,
                        payload: ToolPayload::Function { arguments },
                    }))
                }
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => Ok(Some(ToolCall {
                tool_name: name,
                call_id,
                payload: ToolPayload::Custom { input },
            })),
            ResponseItem::LocalShellCall {
                id,
                call_id,
                action,
                ..
            } => {
                let call_id = call_id
                    .or(id)
                    .ok_or(FunctionCallError::MissingLocalShellCallId)?;

                match action {
                    LocalShellAction::Exec(exec) => {
                        let params = ShellToolCallParams {
                            command: exec.command,
                            workdir: exec.working_directory,
                            timeout_ms: exec.timeout_ms,
                            sandbox_permissions: Some(SandboxPermissions::UseDefault),
                            justification: None,
                        };
                        Ok(Some(ToolCall {
                            tool_name: "local_shell".to_string(),
                            call_id,
                            payload: ToolPayload::LocalShell { params },
                        }))
                    }
                }
            }
            _ => Ok(None),
        }
    }

    #[instrument(level = "trace", skip_all, err)]
    pub async fn dispatch_tool_call(
        &self,
        session: Arc<Session>,
        turn: Arc<TurnContext>,
        tracker: SharedTurnDiffTracker,
        call: ToolCall,
    ) -> Result<ResponseInputItem, FunctionCallError> {
        let hook = turn.tool_hook.clone();
        let mut call = call;
        let hook_snapshot = hook.as_ref().map(|_| ToolCallSnapshot::from_call(&call));
        let directive =
            if let (Some(hook), Some(snapshot)) = (hook.as_ref(), hook_snapshot.as_ref()) {
                hook.emit(ToolHookEvent::before(snapshot.clone())).await
            } else {
                None
            };
        if let Some(directive) = directive {
            Self::apply_tool_hook_directive(&mut call, directive);
        }

        let ToolCall {
            tool_name,
            call_id,
            payload,
        } = call;
        let payload_outputs_custom = matches!(payload, ToolPayload::Custom { .. });
        let failure_call_id = call_id.clone();

        let invocation = ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
        };

        match self.registry.dispatch(invocation).await {
            Ok(response) => {
                if let (Some(hook), Some(snapshot)) = (hook.as_ref(), hook_snapshot.as_ref()) {
                    let _ = hook
                        .emit(ToolHookEvent::after_success(
                            snapshot.clone(),
                            response.clone(),
                        ))
                        .await;
                }
                Ok(response)
            }
            Err(FunctionCallError::Fatal(message)) => {
                if let (Some(hook), Some(snapshot)) = (hook.as_ref(), hook_snapshot.as_ref()) {
                    let _ = hook
                        .emit(ToolHookEvent::after_error(
                            snapshot.clone(),
                            message.clone(),
                        ))
                        .await;
                }
                Err(FunctionCallError::Fatal(message))
            }
            Err(err) => {
                if let (Some(hook), Some(snapshot)) = (hook.as_ref(), hook_snapshot.as_ref()) {
                    let _ = hook
                        .emit(ToolHookEvent::after_error(
                            snapshot.clone(),
                            err.to_string(),
                        ))
                        .await;
                }
                Ok(Self::failure_response(
                    failure_call_id,
                    payload_outputs_custom,
                    err,
                ))
            }
        }
    }

    fn failure_response(
        call_id: String,
        payload_outputs_custom: bool,
        err: FunctionCallError,
    ) -> ResponseInputItem {
        let message = err.to_string();
        if payload_outputs_custom {
            ResponseInputItem::CustomToolCallOutput {
                call_id,
                output: message,
            }
        } else {
            ResponseInputItem::FunctionCallOutput {
                call_id,
                output: codex_protocol::models::FunctionCallOutputPayload {
                    content: message,
                    success: Some(false),
                    ..Default::default()
                },
            }
        }
    }

    fn apply_tool_hook_directive(call: &mut ToolCall, directive: ToolHookDirective) {
        let Some(local_shell) = directive.local_shell else {
            return;
        };

        let Some(behavior) = local_shell.timeout_behavior() else {
            return;
        };

        match (&mut call.payload, call.tool_name.as_str()) {
            (ToolPayload::LocalShell { params }, _) => {
                Self::apply_timeout_behavior(&mut params.timeout_ms, behavior);
                debug!("tool_hook_timeout_override" = "local_shell", command = ?params.command, timeout = ?params.timeout_ms);
            }
            (ToolPayload::Function { arguments }, "shell_command") => {
                match serde_json::from_str::<Value>(arguments) {
                    Ok(mut params) => {
                        if let Some(obj) = params.as_object_mut() {
                            let value = match behavior {
                                TimeoutBehavior::Millis(ms) => Value::from(ms),
                                TimeoutBehavior::Infinite => Value::from(0u64),
                            };
                            obj.insert("timeout_ms".to_string(), value);
                            match serde_json::to_string(&params) {
                                Ok(updated) => {
                                    *arguments = updated;
                                    debug!("tool_hook_timeout_override" = "shell_command", timeout_behavior = ?behavior);
                                }
                                Err(err) => {
                                    warn!("shell_command_hook_serialize_error" = %err,
                                        "failed to serialize shell_command arguments after applying timeout override");
                                }
                            }
                        } else {
                            warn!(
                                "shell_command_hook_parse_error: shell_command arguments were not an object"
                            );
                        }
                    }
                    Err(err) => {
                        warn!("shell_command_hook_parse_error" = %err,
                            "failed to parse shell_command arguments for timeout override");
                    }
                }
            }
            _ => {}
        }
    }

    fn apply_timeout_behavior(target: &mut Option<u64>, behavior: TimeoutBehavior) {
        match behavior {
            TimeoutBehavior::Millis(ms) => *target = Some(ms),
            TimeoutBehavior::Infinite => *target = Some(0),
        }
    }
}
