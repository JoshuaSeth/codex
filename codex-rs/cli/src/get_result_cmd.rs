use anyhow::Context;
use clap::Parser;
use codex_common::CliConfigOverrides;
use codex_core::config::find_codex_home;
use codex_core::live_status::LiveStatusRecordV1;
use codex_exec::exec_events::CommandExecutionItem;
use codex_exec::exec_events::CommandExecutionStatus;
use codex_exec::exec_events::FileChangeItem;
use codex_exec::exec_events::PatchApplyStatus;
use codex_exec::exec_events::ThreadEvent;
use codex_exec::exec_events::ThreadItem;
use codex_exec::exec_events::ThreadItemDetails;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

/// Print a non-interactive summary of a thread: running/finished + last turn result.
///
/// This reads:
/// - `$CODEX_HOME/live/<thread_id>.json` for liveness/status
/// - an exec JSONL event stream (either explicitly `--events-file` or resolved via
///   `$CODEX_HOME/live/<thread_id>.events.jsonl.path`)
#[derive(Debug, Parser)]
pub struct GetResultCli {
    /// Conversation/thread id.
    #[arg(value_name = "THREAD_ID")]
    pub thread_id: String,

    /// Read events from this file instead of resolving via `$CODEX_HOME/live/<thread_id>.events.jsonl.path`.
    #[arg(long = "events-file", value_name = "FILE")]
    pub events_file: Option<PathBuf>,

    /// How many most-recent turns to include.
    #[arg(long, default_value_t = 1)]
    pub turns: usize,

    /// Output as JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

pub fn run_get_result(
    cli: GetResultCli,
    root_overrides: &CliConfigOverrides,
) -> anyhow::Result<()> {
    let codex_home = match &root_overrides.config_home {
        Some(home) => home.clone(),
        None => find_codex_home().unwrap_or_else(|_| default_codex_home()),
    };

    let thread_id =
        codex_protocol::ThreadId::from_string(&cli.thread_id).context("invalid thread id")?;
    let live_status_path = LiveStatusRecordV1::path_for(&codex_home, &thread_id);

    let events_file = match cli.events_file {
        Some(path) => path,
        None => resolve_events_file_for_thread(&codex_home, &cli.thread_id)?,
    };

    let meta = crate::exec_view_meta::ExecViewMetaV1::load_for_events(&events_file)
        .with_context(|| format!("load meta for {}", events_file.display()))?;
    let current_prompt = meta.as_ref().and_then(|m| m.current_prompt.clone());

    let live = read_live_status_snapshot(&live_status_path)
        .with_context(|| format!("read {}", live_status_path.display()))?;

    let summary = summarize_events_file(&events_file, cli.turns)
        .with_context(|| format!("summarize {}", events_file.display()))?;

    let output = GetResultOutput {
        thread_id: cli.thread_id,
        status: live.status,
        alive: live.alive,
        stale: live.stale,
        current_prompt,
        events_file,
        last_turns: summary.turns,
    };

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    print_human(&output);
    Ok(())
}

fn default_codex_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

fn resolve_events_file_for_thread(codex_home: &Path, thread_id: &str) -> anyhow::Result<PathBuf> {
    let pointer = codex_home
        .join("live")
        .join(format!("{thread_id}.events.jsonl.path"));
    let raw =
        std::fs::read_to_string(&pointer).with_context(|| format!("read {}", pointer.display()))?;
    let path = raw.trim();
    if path.is_empty() {
        anyhow::bail!("empty events pointer file: {}", pointer.display());
    }
    Ok(PathBuf::from(path))
}

#[derive(Debug, Deserialize)]
struct LiveStatusSnapshot {
    #[serde(default)]
    alive: Option<bool>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    last_heartbeat_at: Option<String>,
}

#[derive(Debug)]
struct LiveStatusSummary {
    alive: bool,
    status: String,
    stale: bool,
}

fn read_live_status_snapshot(path: &Path) -> anyhow::Result<LiveStatusSummary> {
    let raw = std::fs::read_to_string(path)?;
    let snapshot: LiveStatusSnapshot = serde_json::from_str(&raw).context("parse live status")?;
    let alive = snapshot.alive.unwrap_or(false);
    let status = snapshot.status.unwrap_or_else(|| "unknown".to_string());

    let stale = snapshot
        .last_heartbeat_at
        .as_deref()
        .and_then(heartbeat_age_seconds)
        .is_some_and(|age| age > 30.0);

    Ok(LiveStatusSummary {
        alive,
        status,
        stale,
    })
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

#[derive(Debug, Clone, Serialize)]
struct GetResultOutput {
    thread_id: String,
    status: String,
    alive: bool,
    stale: bool,
    current_prompt: Option<String>,
    events_file: PathBuf,
    last_turns: Vec<TurnSummary>,
}

#[derive(Debug, Clone, Serialize)]
struct TurnSummary {
    index: usize,
    agent_message: Option<String>,
    reasoning: String,
    commands: Vec<CommandSummary>,
    file_changes: Vec<FileChangeSummary>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CommandSummary {
    command: String,
    status: CommandExecutionStatus,
    exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
struct FileChangeSummary {
    path: String,
    kind: String,
    move_to: Option<String>,
    added_lines: Option<u32>,
    deleted_lines: Option<u32>,
    status: PatchApplyStatus,
}

#[derive(Debug)]
struct ParsedSummary {
    turns: Vec<TurnSummary>,
}

fn summarize_events_file(events_file: &Path, turns: usize) -> anyhow::Result<ParsedSummary> {
    use std::io::BufRead;

    let f = std::fs::File::open(events_file)?;
    let reader = std::io::BufReader::new(f);

    let mut all_turns: Vec<TurnSummary> = Vec::new();
    let mut current_turn_index = 0usize;
    let mut current = TurnSummary {
        index: current_turn_index,
        agent_message: None,
        reasoning: String::new(),
        commands: Vec::new(),
        file_changes: Vec::new(),
        errors: Vec::new(),
    };

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<ThreadEvent>(&line) else {
            continue;
        };

        match ev {
            ThreadEvent::TurnStarted(_) => {
                if has_any_activity(&current) {
                    all_turns.push(current);
                }
                current_turn_index += 1;
                current = TurnSummary {
                    index: current_turn_index,
                    agent_message: None,
                    reasoning: String::new(),
                    commands: Vec::new(),
                    file_changes: Vec::new(),
                    errors: Vec::new(),
                };
            }
            ThreadEvent::TurnFailed(failed) => {
                current.errors.push(failed.error.message);
            }
            ThreadEvent::Error(err) => {
                current.errors.push(err.message);
            }
            ThreadEvent::ItemCompleted(item) => {
                merge_item_completed(&mut current, item.item);
            }
            ThreadEvent::ItemUpdated(item) => {
                merge_item_updated(&mut current, item.item);
            }
            _ => {}
        }
    }
    if has_any_activity(&current) {
        all_turns.push(current);
    }

    let keep = turns.max(1).min(all_turns.len());
    Ok(ParsedSummary {
        turns: all_turns
            .into_iter()
            .rev()
            .take(keep)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect(),
    })
}

fn has_any_activity(turn: &TurnSummary) -> bool {
    turn.agent_message.is_some()
        || !turn.reasoning.is_empty()
        || !turn.commands.is_empty()
        || !turn.file_changes.is_empty()
        || !turn.errors.is_empty()
}

fn merge_item_completed(turn: &mut TurnSummary, item: ThreadItem) {
    match item.details {
        ThreadItemDetails::AgentMessage(msg) => {
            turn.agent_message = Some(msg.text);
        }
        ThreadItemDetails::Reasoning(reasoning) => {
            append_reasoning(turn, &reasoning.text);
        }
        ThreadItemDetails::CommandExecution(cmd) => {
            turn.commands.push(CommandSummary {
                command: cmd.command,
                status: cmd.status,
                exit_code: cmd.exit_code,
            });
        }
        ThreadItemDetails::FileChange(change) => {
            merge_file_change(turn, &change);
        }
        ThreadItemDetails::Error(err) => {
            turn.errors.push(err.message);
        }
        _ => {}
    }
}

fn merge_item_updated(turn: &mut TurnSummary, item: ThreadItem) {
    match item.details {
        ThreadItemDetails::Reasoning(reasoning) => append_reasoning(turn, &reasoning.text),
        ThreadItemDetails::CommandExecution(CommandExecutionItem {
            command,
            exit_code,
            status,
            ..
        }) => {
            if matches!(
                status,
                CommandExecutionStatus::Completed | CommandExecutionStatus::Failed
            ) {
                turn.commands.push(CommandSummary {
                    command,
                    status,
                    exit_code,
                });
            }
        }
        _ => {}
    }
}

fn append_reasoning(turn: &mut TurnSummary, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    if !turn.reasoning.is_empty() {
        turn.reasoning.push_str("\n\n");
    }
    turn.reasoning.push_str(trimmed);
}

fn merge_file_change(turn: &mut TurnSummary, change: &FileChangeItem) {
    for file in &change.changes {
        let kind = match file.kind {
            codex_exec::exec_events::PatchChangeKind::Add => "add",
            codex_exec::exec_events::PatchChangeKind::Delete => "delete",
            codex_exec::exec_events::PatchChangeKind::Update => "update",
        };
        turn.file_changes.push(FileChangeSummary {
            path: file.path.clone(),
            kind: kind.to_string(),
            move_to: file.move_to.clone(),
            added_lines: file.added_lines,
            deleted_lines: file.deleted_lines,
            status: change.status.clone(),
        });
    }
}

fn print_human(out: &GetResultOutput) {
    println!("Thread: {}", out.thread_id);
    println!(
        "Status: {} (alive={}, stale={})",
        out.status, out.alive, out.stale
    );
    println!("Events: {}", out.events_file.display());
    if let Some(prompt) = out.current_prompt.as_deref() {
        println!("Prompt: {}", prompt.trim());
    }

    for turn in &out.last_turns {
        println!();
        println!("Turn {}", turn.index);
        if !turn.errors.is_empty() {
            for err in &turn.errors {
                println!("- error: {err}");
            }
        }
        if !turn.reasoning.is_empty() {
            println!("- reasoning:");
            println!("{}", turn.reasoning);
        }
        if let Some(msg) = turn.agent_message.as_deref() {
            println!("- agent:");
            println!("{}", msg.trim());
        }
        if !turn.file_changes.is_empty() {
            println!("- files:");
            let mut grouped: BTreeMap<&str, Vec<&FileChangeSummary>> = BTreeMap::new();
            for ch in &turn.file_changes {
                grouped.entry(ch.path.as_str()).or_default().push(ch);
            }
            for (path, changes) in grouped {
                let mut adds = 0u32;
                let mut dels = 0u32;
                let mut has_stats = true;
                let mut kinds: Vec<String> = Vec::new();
                let mut move_to: Option<String> = None;
                let mut status = PatchApplyStatus::Completed;
                for ch in changes {
                    kinds.push(ch.kind.clone());
                    move_to = move_to.or_else(|| ch.move_to.clone());
                    status = ch.status.clone();
                    match (ch.added_lines, ch.deleted_lines) {
                        (Some(a), Some(d)) => {
                            adds = adds.saturating_add(a);
                            dels = dels.saturating_add(d);
                        }
                        _ => has_stats = false,
                    }
                }

                let move_suffix = move_to
                    .as_deref()
                    .map(|p| format!(" -> {p}"))
                    .unwrap_or_default();
                if has_stats {
                    println!(
                        "  - {path}{move_suffix} (+{adds}/-{dels}) {status:?} [{}]",
                        kinds.join(",")
                    );
                } else {
                    println!("  - {path}{move_suffix} {status:?} [{}]", kinds.join(","));
                }
            }
        }
        if !turn.commands.is_empty() {
            println!("- commands:");
            for cmd in &turn.commands {
                println!(
                    "  - {:?} (exit={:?}) {}",
                    cmd.status, cmd.exit_code, cmd.command
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_events_file_for_thread;
    use super::summarize_events_file;
    use codex_exec::exec_events::FileChangeItem;
    use codex_exec::exec_events::FileUpdateChange;
    use codex_exec::exec_events::ItemCompletedEvent;
    use codex_exec::exec_events::PatchApplyStatus;
    use codex_exec::exec_events::PatchChangeKind;
    use codex_exec::exec_events::ThreadEvent;
    use codex_exec::exec_events::ThreadItem;
    use codex_exec::exec_events::ThreadItemDetails;
    use codex_exec::exec_events::ThreadStartedEvent;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn resolves_events_pointer_file() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let codex_home = dir.path();
        std::fs::create_dir_all(codex_home.join("live"))?;
        std::fs::write(
            codex_home.join("live").join("thread-1.events.jsonl.path"),
            "/tmp/x.events.jsonl\n",
        )?;
        let resolved = resolve_events_file_for_thread(codex_home, "thread-1")?;
        assert_eq!(resolved, PathBuf::from("/tmp/x.events.jsonl"));
        Ok(())
    }

    #[test]
    fn summarizes_last_turn() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let events = dir.path().join("run.events.jsonl");

        let lines: Vec<String> = vec![
            serde_json::to_string(&ThreadEvent::ThreadStarted(ThreadStartedEvent {
                thread_id: "thread-1".to_string(),
            }))?,
            serde_json::to_string(&ThreadEvent::TurnStarted(Default::default()))?,
            serde_json::to_string(&ThreadEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: "item_1".to_string(),
                    details: ThreadItemDetails::FileChange(FileChangeItem {
                        changes: vec![FileUpdateChange {
                            path: "a.txt".to_string(),
                            kind: PatchChangeKind::Update,
                            move_to: None,
                            added_lines: Some(3),
                            deleted_lines: Some(1),
                        }],
                        status: PatchApplyStatus::Completed,
                    }),
                },
            }))?,
            serde_json::to_string(&ThreadEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: "item_2".to_string(),
                    details: ThreadItemDetails::AgentMessage(
                        codex_exec::exec_events::AgentMessageItem {
                            text: "done".to_string(),
                        },
                    ),
                },
            }))?,
        ];

        std::fs::write(&events, lines.join("\n") + "\n")?;

        let summary = summarize_events_file(&events, 1)?;
        assert_eq!(summary.turns.len(), 1);
        assert_eq!(summary.turns[0].agent_message.as_deref(), Some("done"));
        assert_eq!(summary.turns[0].file_changes.len(), 1);
        assert_eq!(summary.turns[0].file_changes[0].path, "a.txt");
        assert_eq!(summary.turns[0].file_changes[0].added_lines, Some(3));
        assert_eq!(summary.turns[0].file_changes[0].deleted_lines, Some(1));
        Ok(())
    }
}
