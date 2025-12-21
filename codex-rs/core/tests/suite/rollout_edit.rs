use std::path::Path;

use codex_core::PatchedToolCall;
use codex_core::ToolResultKind;
use codex_core::replace_last_tool_result;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use tempfile::tempdir;
use tokio::fs;

#[tokio::test]
async fn replace_last_function_call_output() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("rollout.jsonl");

    let lines = vec![
        session_meta_line(),
        RolloutLine {
            timestamp: ts(1),
            item: RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput {
                call_id: "call_func".into(),
                output: FunctionCallOutputPayload {
                    content: "pending".into(),
                    content_items: None,
                    success: Some(false),
                },
            }),
        },
    ];
    write_lines(&path, &lines).await?;

    let patched = replace_last_tool_result(&path, "final output").await?;
    assert_eq!(
        patched,
        PatchedToolCall {
            call_id: "call_func".into(),
            kind: ToolResultKind::Function,
        }
    );

    let rewritten = read_lines(&path).await?;
    match &rewritten[1].item {
        RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput { output, .. }) => {
            assert_eq!(output.content, "final output");
            assert_eq!(output.content_items, None);
        }
        other => anyhow::bail!("unexpected item: {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn replace_last_custom_tool_output() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("rollout.jsonl");

    let lines = vec![
        session_meta_line(),
        RolloutLine {
            timestamp: ts(1),
            item: RolloutItem::ResponseItem(ResponseItem::CustomToolCallOutput {
                call_id: "call_custom".into(),
                output: "pending".into(),
            }),
        },
    ];
    write_lines(&path, &lines).await?;

    let patched = replace_last_tool_result(&path, "delivered").await?;
    assert_eq!(
        patched,
        PatchedToolCall {
            call_id: "call_custom".into(),
            kind: ToolResultKind::Custom,
        }
    );

    let rewritten = read_lines(&path).await?;
    match &rewritten[1].item {
        RolloutItem::ResponseItem(ResponseItem::CustomToolCallOutput { output, .. }) => {
            assert_eq!(output, "delivered");
        }
        other => anyhow::bail!("unexpected item: {other:?}"),
    }

    Ok(())
}

fn ts(n: u8) -> String {
    format!("2025-12-07T00:00:{n:02}Z")
}

fn session_meta_line() -> RolloutLine {
    RolloutLine {
        timestamp: ts(0),
        item: RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                id: codex_protocol::ConversationId::new(),
                timestamp: ts(0),
                cwd: "/tmp".into(),
                originator: "test".into(),
                cli_version: "0.0.0".into(),
                instructions: None,
                source: codex_protocol::protocol::SessionSource::Cli,
                model_provider: Some("openai".into()),
            },
            git: None,
        }),
    }
}

async fn write_lines(path: &Path, lines: &[RolloutLine]) -> anyhow::Result<()> {
    let mut buf = String::new();
    for line in lines {
        buf.push_str(&serde_json::to_string(line)?);
        buf.push('\n');
    }
    fs::write(path, buf).await?;
    Ok(())
}

async fn read_lines(path: &Path) -> anyhow::Result<Vec<RolloutLine>> {
    let text = fs::read_to_string(path).await?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}
