use crate::tools::context::ToolPayload;
use crate::tools::router::ToolCall;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use serde::Serialize;
use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::warn;

#[derive(Clone, Debug)]
pub struct ToolHook {
    command: Arc<Vec<String>>,
}

impl ToolHook {
    pub fn new(command: Vec<String>) -> Option<Self> {
        if command.is_empty() {
            return None;
        }
        Some(Self {
            command: Arc::new(command),
        })
    }

    pub async fn emit(&self, event: ToolHookEvent) {
        if let Err(err) = self.spawn_and_send(event).await {
            warn!("tool_hook_error" = %err, "failed to run tool hook command");
        }
    }

    async fn spawn_and_send(&self, event: ToolHookEvent) -> std::io::Result<()> {
        let mut cmd = Command::new(&self.command[0]);
        if self.command.len() > 1 {
            cmd.args(&self.command[1..]);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());

        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            let payload = serde_json::to_vec(&event).map_err(|err| {
                std::io::Error::other(format!("failed to serialize hook event: {err}"))
            })?;
            stdin.write_all(&payload).await?;
        }
        let status = child.wait().await?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "hook exited with status {status}"
            )));
        }
        Ok(())
    }
}

#[derive(Serialize, Clone)]
pub struct ToolCallSnapshot {
    tool_name: String,
    call_id: String,
    payload: ToolCallPayloadSnapshot,
}

impl ToolCallSnapshot {
    pub fn from_call(call: &ToolCall) -> Self {
        Self {
            tool_name: call.tool_name.clone(),
            call_id: call.call_id.clone(),
            payload: ToolCallPayloadSnapshot::from_payload(&call.payload),
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolCallPayloadSnapshot {
    Function {
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        parsed_arguments: Option<Value>,
    },
    Custom {
        input: String,
    },
    LocalShell {
        command: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        workdir: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    UnifiedExec {
        arguments: String,
    },
    Mcp {
        server: String,
        tool: String,
        raw_arguments: String,
    },
}

impl ToolCallPayloadSnapshot {
    fn from_payload(payload: &ToolPayload) -> Self {
        match payload {
            ToolPayload::Function { arguments } => {
                let parsed_arguments = serde_json::from_str(arguments).ok();
                Self::Function {
                    arguments: arguments.clone(),
                    parsed_arguments,
                }
            }
            ToolPayload::Custom { input } => Self::Custom {
                input: input.clone(),
            },
            ToolPayload::LocalShell { params } => Self::LocalShell {
                command: params.command.clone(),
                workdir: params.workdir.clone(),
                timeout_ms: params.timeout_ms,
            },
            ToolPayload::UnifiedExec { arguments } => Self::UnifiedExec {
                arguments: arguments.clone(),
            },
            ToolPayload::Mcp {
                server,
                tool,
                raw_arguments,
            } => Self::Mcp {
                server: server.clone(),
                tool: tool.clone(),
                raw_arguments: raw_arguments.clone(),
            },
        }
    }
}

#[derive(Serialize)]
pub struct ToolHookEvent {
    phase: ToolHookPhase,
    call: ToolCallSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<ToolHookOutcome>,
}

impl ToolHookEvent {
    pub fn before(call: ToolCallSnapshot) -> Self {
        Self {
            phase: ToolHookPhase::BeforeExecution,
            call,
            outcome: None,
        }
    }

    pub fn after_success(call: ToolCallSnapshot, response: ResponseInputItem) -> Self {
        Self {
            phase: ToolHookPhase::AfterExecution,
            call,
            outcome: Some(ToolHookOutcome::Success { response }),
        }
    }

    pub fn after_error(call: ToolCallSnapshot, message: String) -> Self {
        Self {
            phase: ToolHookPhase::AfterExecution,
            call,
            outcome: Some(ToolHookOutcome::Error { message }),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ToolHookPhase {
    BeforeExecution,
    AfterExecution,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ToolHookOutcome {
    Success { response: ResponseInputItem },
    Error { message: String },
}

#[derive(Clone, Debug)]
pub struct StopHook {
    command: Arc<Vec<String>>,
}

impl StopHook {
    pub fn new(command: Vec<String>) -> Option<Self> {
        if command.is_empty() {
            return None;
        }
        Some(Self {
            command: Arc::new(command),
        })
    }

    pub async fn emit(&self, event: StopHookEvent) {
        if let Err(err) = self.spawn_and_send(event).await {
            warn!("stop_hook_error" = %err, "failed to run stop hook command");
        }
    }

    async fn spawn_and_send(&self, event: StopHookEvent) -> std::io::Result<()> {
        let mut cmd = Command::new(&self.command[0]);
        if self.command.len() > 1 {
            cmd.args(&self.command[1..]);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());

        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            let payload = serde_json::to_vec(&event).map_err(|err| {
                std::io::Error::other(format!("failed to serialize stop hook event: {err}"))
            })?;
            stdin.write_all(&payload).await?;
        }
        let status = child.wait().await?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "hook exited with status {status}"
            )));
        }
        Ok(())
    }
}

#[derive(Serialize)]
pub struct StopHookEvent {
    conversation_id: String,
    turn_id: String,
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_message: Option<String>,
    response_items: Vec<ResponseItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_usage: Option<TokenUsage>,
}

impl StopHookEvent {
    pub fn new(
        conversation_id: String,
        turn_id: String,
        cwd: String,
        final_message: Option<String>,
        response_items: Vec<ResponseItem>,
        token_usage: Option<TokenUsage>,
    ) -> Self {
        Self {
            conversation_id,
            turn_id,
            cwd,
            final_message,
            response_items,
            token_usage,
        }
    }
}
