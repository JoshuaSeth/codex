use std::io;
use std::path::Path;

use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;

/// Describes which type of tool output was patched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolResultKind {
    Function,
    Custom,
}

/// Details about the patched tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchedToolCall {
    pub call_id: String,
    pub kind: ToolResultKind,
}

/// Replace the payload of the most recent tool call output within the rollout at `path`.
///
/// This is primarily used to swap in the real result for sessions that shut down immediately
/// after a `shutdown_after_call` tool execution.
pub async fn replace_last_tool_result(
    path: &Path,
    new_output: &str,
) -> io::Result<PatchedToolCall> {
    let contents = tokio::fs::read_to_string(path).await?;
    if contents.trim().is_empty() {
        return Err(io::Error::other("rollout file is empty"));
    }

    let mut lines: Vec<RolloutLine> = Vec::new();
    for raw in contents.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        let parsed: RolloutLine = serde_json::from_str(raw).map_err(|err| {
            io::Error::other(format!(
                "failed to parse rollout line as JSON: {err}; offending line: {raw}"
            ))
        })?;
        lines.push(parsed);
    }

    let mut patched: Option<PatchedToolCall> = None;
    for entry in lines.iter_mut().rev() {
        if let RolloutItem::ResponseItem(response) = &mut entry.item {
            match response {
                ResponseItem::FunctionCallOutput { call_id, output } => {
                    overwrite_function_output(output, new_output);
                    patched = Some(PatchedToolCall {
                        call_id: call_id.clone(),
                        kind: ToolResultKind::Function,
                    });
                    break;
                }
                ResponseItem::CustomToolCallOutput { call_id, output } => {
                    *output = new_output.to_string();
                    patched = Some(PatchedToolCall {
                        call_id: call_id.clone(),
                        kind: ToolResultKind::Custom,
                    });
                    break;
                }
                _ => {}
            }
        }
    }

    let patched = patched.ok_or_else(|| {
        io::Error::other("no tool call output found in rollout; nothing to replace")
    })?;

    let mut buffer = String::new();
    for line in &lines {
        let encoded = serde_json::to_string(line)
            .map_err(|err| io::Error::other(format!("failed to encode rollout line: {err}")))?;
        buffer.push_str(&encoded);
        buffer.push('\n');
    }

    tokio::fs::write(path, buffer).await?;
    Ok(patched)
}

fn overwrite_function_output(output: &mut FunctionCallOutputPayload, new_output: &str) {
    output.content = new_output.to_string();
    output.content_items = None;
}
