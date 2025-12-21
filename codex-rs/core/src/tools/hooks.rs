use crate::tools::context::ToolPayload;
use crate::tools::router::ToolCall;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use serde::Deserialize;
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

    pub async fn emit(&self, event: ToolHookEvent) -> Option<ToolHookDirective> {
        match self.spawn_and_send(event).await {
            Ok(result) => result,
            Err(err) => {
                warn!("tool_hook_error" = %err, "failed to run tool hook command");
                None
            }
        }
    }

    async fn spawn_and_send(
        &self,
        event: ToolHookEvent,
    ) -> std::io::Result<Option<ToolHookDirective>> {
        let capture_response = matches!(event.phase, ToolHookPhase::BeforeExecution);
        let mut cmd = Command::new(&self.command[0]);
        if self.command.len() > 1 {
            cmd.args(&self.command[1..]);
        }
        cmd.stdin(Stdio::piped());
        if capture_response {
            cmd.stdout(Stdio::piped());
        } else {
            cmd.stdout(Stdio::inherit());
        }
        cmd.stderr(Stdio::inherit());

        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            let payload = serde_json::to_vec(&event).map_err(|err| {
                std::io::Error::other(format!("failed to serialize hook event: {err}"))
            })?;
            stdin.write_all(&payload).await?;
        }
        if capture_response {
            let output = child.wait_with_output().await?;
            if !output.status.success() {
                return Err(std::io::Error::other(format!(
                    "hook exited with status {}",
                    output.status
                )));
            }
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                return Ok(None);
            }
            match serde_json::from_str::<ToolHookDirective>(&stdout) {
                Ok(directive) => Ok(Some(directive)),
                Err(err) => {
                    warn!(
                        "tool_hook_parse_error" = %err,
                        "stdout" = %stdout,
                        "failed to parse tool hook output"
                    );
                    Ok(None)
                }
            }
        } else {
            let status = child.wait().await?;
            if !status.success() {
                return Err(std::io::Error::other(format!(
                    "hook exited with status {status}"
                )));
            }
            Ok(None)
        }
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

#[derive(Debug, Clone, Deserialize)]
pub struct ToolHookDirective {
    #[serde(default)]
    pub local_shell: Option<HookLocalShellDirective>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookLocalShellDirective {
    #[serde(default)]
    timeout_ms: Option<ToolHookTimeoutOverride>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ToolHookTimeoutOverride {
    Millis(u64),
    Keyword(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutBehavior {
    Millis(u64),
    Infinite,
}

impl HookLocalShellDirective {
    pub fn timeout_behavior(&self) -> Option<TimeoutBehavior> {
        self.timeout_ms
            .as_ref()
            .and_then(ToolHookTimeoutOverride::behavior)
    }
}

impl ToolHookTimeoutOverride {
    fn behavior(&self) -> Option<TimeoutBehavior> {
        match self {
            Self::Millis(ms) => Some(TimeoutBehavior::Millis(*ms)),
            Self::Keyword(keyword) => {
                let normalized = keyword.trim().to_ascii_lowercase();
                if matches!(
                    normalized.as_str(),
                    "infinite" | "no_timeout" | "none" | "unlimited"
                ) {
                    Some(TimeoutBehavior::Infinite)
                } else {
                    None
                }
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_infinite_timeout_directive() {
        let directive: ToolHookDirective =
            serde_json::from_str(r#"{"local_shell":{"timeout_ms":"infinite"}}"#).unwrap();
        let behavior = directive
            .local_shell
            .as_ref()
            .and_then(HookLocalShellDirective::timeout_behavior)
            .unwrap();
        assert!(matches!(behavior, TimeoutBehavior::Infinite));
    }

    #[test]
    fn parses_numeric_timeout_directive() {
        let directive: ToolHookDirective =
            serde_json::from_str(r#"{"local_shell":{"timeout_ms":60000}}"#).unwrap();
        let behavior = directive
            .local_shell
            .as_ref()
            .and_then(HookLocalShellDirective::timeout_behavior)
            .unwrap();
        assert_eq!(behavior, TimeoutBehavior::Millis(60_000));
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
