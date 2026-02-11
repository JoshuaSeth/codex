use codex_exec::exec_events::CollabToolCallStatus;
use codex_exec::exec_events::CommandExecutionStatus;
use codex_exec::exec_events::McpToolCallStatus;
use codex_exec::exec_events::PatchApplyStatus;
use codex_exec::exec_events::PatchChangeKind;
use codex_exec::exec_events::ThreadItemDetails;
use codex_tui::render_markdown_text;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::List;
use ratatui::widgets::ListItem;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use serde_json::Value;

use super::app::ItemPhase;
use super::app::ItemRecord;
use super::app::LiveTextKind;
use super::app::ViewApp;
use super::app::ViewLayout;
use super::util::clip_one_line;
use super::util::pretty_json;
use super::util::strip_shell_launcher_prefix;
use super::util::trim_one_line;

pub(super) fn draw_view(f: &mut ratatui::Frame, app: &mut ViewApp) {
    let header_height = 4;
    let composer_height = composer_panel_height(app);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Length(header_height),
                Constraint::Min(0),
                Constraint::Length(composer_height),
            ]
            .as_ref(),
        )
        .split(f.area());

    let header = header_lines(app);
    let header = Paragraph::new(header).wrap(Wrap { trim: true }).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .title("codex view".bold()),
    );
    f.render_widget(header, chunks[0]);

    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)].as_ref())
        .split(chunks[1]);

    let prompt_lines = latest_prompt_lines(app, body_chunks[0].width.saturating_sub(2));
    let prompt_panel_height = prompt_lines.len().saturating_add(2) as u16;

    let queue_panel_height = if app.queued_user_prompts.is_empty() {
        0
    } else {
        // Border adds 2 lines; clamp to keep the events list usable.
        let visible = app.queued_user_prompts.len().min(4) as u16;
        (visible + 2).min(8)
    };

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Length(1),
                Constraint::Length(prompt_panel_height),
                Constraint::Min(0),
                Constraint::Length(queue_panel_height),
            ]
            .as_ref(),
        )
        .split(body_chunks[0]);

    app.layout = Some(ViewLayout {
        follow_bar: left_chunks[0],
        events_list: left_chunks[2],
        details: body_chunks[1],
    });

    let follow = Paragraph::new(vec![follow_bar_line(app)]).block(Block::default());
    f.render_widget(follow, left_chunks[0]);

    let prompt = Paragraph::new(prompt_lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Prompt".bold()),
        );
    f.render_widget(prompt, left_chunks[1]);

    let items = app
        .items
        .iter()
        .map(|record| ListItem::new(item_summary_line(record)))
        .collect::<Vec<_>>();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Events".bold()),
        )
        .highlight_style(Style::new().bold())
        .highlight_symbol("▌ ");

    f.render_stateful_widget(list, left_chunks[2], &mut app.list_state);

    if queue_panel_height > 0 {
        let queue_items = app
            .queued_user_prompts
            .iter()
            .take(queue_panel_height.saturating_sub(2) as usize)
            .enumerate()
            .map(|(idx, prompt)| {
                let prefix: Span<'static> = if idx == 0 {
                    "▶ ".green().bold()
                } else {
                    "  ".dim()
                };
                let num: Span<'static> = format!("{:>2}. ", idx + 1).dim();
                let text = clip_one_line(prompt, 240);
                let line: Line<'static> = vec![prefix, num, text.into()].into();
                ListItem::new(line)
            })
            .collect::<Vec<_>>();

        let title = format!("Queue ({})", app.queued_user_prompts.len()).bold();
        let list =
            List::new(queue_items).block(Block::default().borders(Borders::ALL).title(title));
        f.render_widget(list, left_chunks[3]);
    }

    let detail_lines = selected_item_details(app);
    let viewport_h = body_chunks[1].height.saturating_sub(2) as usize;
    let max_scroll = detail_lines.len().saturating_sub(viewport_h);
    let clamped = max_scroll.min(u16::MAX as usize) as u16;
    if app.detail_follow || (app.detail_scroll as usize) > max_scroll {
        app.detail_scroll = clamped;
    }

    let detail = Paragraph::new(detail_lines)
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Details".bold()),
        );
    f.render_widget(detail, body_chunks[1]);

    if app.show_quit_prompt && app.process_pid.is_some() {
        let area = centered_rect(70, 20, f.area());
        f.render_widget(Clear, area);
        let block = Block::default().borders(Borders::ALL).title("Exit".bold());
        let text: Vec<Line<'static>> = vec![
            "Close viewer only or also terminate the running process?".into(),
            Line::default(),
            "Close process too? (y/N)".bold().into(),
            "Enter = close only viewer, Esc = cancel".dim().into(),
        ];
        let p = Paragraph::new(text).block(block).wrap(Wrap { trim: true });
        f.render_widget(p, area);
    }

    if app.show_stop_prompt && app.process_pid.is_some() {
        let area = centered_rect(72, 22, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Stop Process".bold());
        let text: Vec<Line<'static>> = vec![
            "Stop the running `codex exec` process?".into(),
            Line::default(),
            "Enter / y = stop".bold().into(),
            "r = stop and retry last prompt".bold().into(),
            "Esc = cancel".dim().into(),
        ];
        let p = Paragraph::new(text).block(block).wrap(Wrap { trim: true });
        f.render_widget(p, area);
    }

    draw_composer(f, app, chunks[2]);
}

fn composer_panel_height(app: &ViewApp) -> u16 {
    if app.composer_active { 7 } else { 3 }
}

fn draw_composer(f: &mut ratatui::Frame, app: &mut ViewApp, area: ratatui::layout::Rect) {
    let title = if app.composer_active {
        "Compose prompt (Enter send, Shift+Enter newline, Esc close)".bold()
    } else {
        "Compose prompt (press i)".dim()
    };

    let block = Block::default().borders(Borders::ALL).title(title);

    if !app.composer_active && app.composer_text.trim().is_empty() {
        let hint: Vec<Line<'static>> = vec![
            "Viewer is read-only for events, but can enqueue prompts.".into(),
            "Press i to write a multi-line prompt.".dim().into(),
        ];
        let p = Paragraph::new(hint).block(block).wrap(Wrap { trim: true });
        f.render_widget(p, area);
        return;
    }

    let inner_width = area.width.saturating_sub(2);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let width = inner_width.saturating_sub(1).max(10) as usize;
    for raw_line in app.composer_text.lines() {
        if raw_line.trim().is_empty() {
            lines.push(Line::default());
            continue;
        }
        for wrapped in textwrap::wrap(raw_line, width) {
            lines.push(Line::from(wrapped.to_string()));
        }
    }
    if app.composer_text.ends_with('\n') {
        lines.push(Line::default());
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }

    let p = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((0, 0));
    f.render_widget(p, area);
}

fn centered_rect(
    percent_x: u16,
    percent_y: u16,
    r: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Percentage((100 - percent_y) / 2),
                Constraint::Percentage(percent_y),
                Constraint::Percentage((100 - percent_y) / 2),
            ]
            .as_ref(),
        )
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints(
            [
                Constraint::Percentage((100 - percent_x) / 2),
                Constraint::Percentage(percent_x),
                Constraint::Percentage((100 - percent_x) / 2),
            ]
            .as_ref(),
        )
        .split(popup_layout[1])[1]
}

fn header_lines(app: &ViewApp) -> Vec<Line<'static>> {
    let thread = app.thread_id.as_deref().unwrap_or("<pending thread id>");
    let file = app.file.display().to_string();

    let mut line1: Line<'static> = vec!["Thread: ".dim(), thread.to_string().bold()].into();

    if let Some(err) = app.last_error.as_deref() {
        line1.spans.push("  ".into());
        line1.spans.push("Error: ".red().bold());
        line1.spans.push(err.to_string().red());
    }

    let line2: Line<'static> = vec!["File: ".dim(), file.into()].into();

    let usage = app.last_usage.clone().unwrap_or_default();
    let quit_hint = if app.process_pid.is_some() {
        "Ctrl+C exit (prompt)"
    } else {
        "Ctrl+C quit"
    };
    let line3: Line<'static> = vec![
        "Turns: ".dim(),
        format!("{} started", app.turns_started).into(),
        "  ".into(),
        format!("{} completed", app.turns_completed).into(),
        "  ".into(),
        "Usage (last): ".dim(),
        format!(
            "in={} cached={} out={}",
            usage.input_tokens, usage.cached_input_tokens, usage.output_tokens
        )
        .into(),
        "  ".into(),
        "Unknown: ".dim(),
        format!("{}", app.unknown_events).into(),
        "  ".into(),
        "Invalid lines: ".dim(),
        format!("{}", app.invalid_lines).into(),
        "  ".into(),
        "Queue: ".dim(),
        format!("{}", app.queued_user_prompts.len()).into(),
        "  ".into(),
        "Keys: ".dim(),
        format!(
            "click select  f follow latest  s stop  n/i compose prompt  j/k ↑/↓ move  g/G home/end  PgUp/PgDn or Ctrl+u/d scroll details  {quit_hint}"
        )
        .dim(),
    ]
    .into();

    vec![line1, line2, line3]
}

fn follow_bar_line(app: &ViewApp) -> Line<'static> {
    if app.follow_latest_item {
        vec![
            "Following latest".green().bold(),
            "  ".into(),
            "(click an event to pin)".dim(),
        ]
        .into()
    } else {
        vec![
            "Pinned".magenta().bold(),
            "  ".into(),
            "[follow latest]".cyan().bold(),
            "  ".into(),
            "(click or press f)".dim(),
        ]
        .into()
    }
}

fn item_summary_line(record: &ItemRecord) -> Line<'static> {
    let (status, status_style) = item_status(record);
    let (kind, summary) = item_kind_and_summary(&record.item.details);
    let id = record.item.id.clone();

    let mut spans: Vec<Span<'static>> = Vec::new();
    if !status.is_empty() {
        spans.push(Span::styled(status, status_style));
        spans.push("  ".into());
    }
    spans.push(kind.bold());
    spans.push(" ".dim());
    spans.push(summary.into());
    spans.push("  ".into());
    spans.push(id.dim());
    spans.into()
}

fn item_status(record: &ItemRecord) -> (&'static str, Style) {
    if record.phase == ItemPhase::InProgress {
        return ("RUN", Style::new().cyan().bold());
    }

    match &record.item.details {
        ThreadItemDetails::CommandExecution(cmd) => match cmd.status {
            CommandExecutionStatus::Completed => ("", Style::new()),
            CommandExecutionStatus::Failed => ("ERR", Style::new().red().bold()),
            CommandExecutionStatus::Declined => ("NO ", Style::new().cyan().bold()),
            CommandExecutionStatus::InProgress => ("RUN", Style::new().cyan().bold()),
        },
        ThreadItemDetails::FileChange(fc) => match fc.status {
            PatchApplyStatus::Completed => ("", Style::new()),
            PatchApplyStatus::Failed => ("ERR", Style::new().red().bold()),
            PatchApplyStatus::InProgress => ("RUN", Style::new().cyan().bold()),
        },
        ThreadItemDetails::McpToolCall(tc) => match tc.status {
            McpToolCallStatus::Completed => ("", Style::new()),
            McpToolCallStatus::Failed => ("ERR", Style::new().red().bold()),
            McpToolCallStatus::InProgress => ("RUN", Style::new().cyan().bold()),
        },
        ThreadItemDetails::CollabToolCall(call) => match call.status {
            CollabToolCallStatus::Completed => ("", Style::new()),
            CollabToolCallStatus::Failed => ("ERR", Style::new().red().bold()),
            CollabToolCallStatus::InProgress => ("RUN", Style::new().cyan().bold()),
        },
        ThreadItemDetails::Error(_) => ("ERR", Style::new().red().bold()),
        _ => ("", Style::new()),
    }
}

fn item_kind_and_summary(details: &ThreadItemDetails) -> (String, String) {
    match details {
        ThreadItemDetails::AgentMessage(msg) => ("Agent".to_string(), trim_one_line(&msg.text)),
        ThreadItemDetails::Reasoning(r) => ("Reasoning".to_string(), trim_one_line(&r.text)),
        ThreadItemDetails::CommandExecution(cmd) => (
            "Cmd".to_string(),
            trim_one_line(strip_shell_launcher_prefix(&cmd.command)),
        ),
        ThreadItemDetails::FileChange(fc) => {
            ("Patch".to_string(), format!("{} file(s)", fc.changes.len()))
        }
        ThreadItemDetails::McpToolCall(tc) => {
            ("MCP".to_string(), format!("{}::{}", tc.server, tc.tool))
        }
        ThreadItemDetails::CollabToolCall(call) => {
            let tool = format!("{:?}", call.tool);
            let summary = match call.receiver_thread_ids.as_slice() {
                [receiver] => format!("{tool} -> {receiver}"),
                receivers if receivers.is_empty() => tool,
                receivers => format!("{tool} -> {} threads", receivers.len()),
            };
            ("Collab".to_string(), summary)
        }
        ThreadItemDetails::WebSearch(ws) => ("Search".to_string(), trim_one_line(&ws.query)),
        ThreadItemDetails::TodoList(todos) => {
            ("Plan".to_string(), format!("{} step(s)", todos.items.len()))
        }
        ThreadItemDetails::Error(err) => ("Error".to_string(), trim_one_line(&err.message)),
    }
}

fn selected_item_details(app: &ViewApp) -> Vec<Line<'static>> {
    let Some(selected) = app.list_state.selected() else {
        return vec!["No events yet.".dim().into()];
    };
    let Some(record) = app.items.get(selected) else {
        return vec!["No events yet.".dim().into()];
    };

    let show_live_text = selected + 1 == app.items.len()
        && matches!(
            record.item.details,
            ThreadItemDetails::AgentMessage(_) | ThreadItemDetails::Reasoning(_)
        )
        && !app.live_text_blocks.is_empty();

    let mut out = Vec::new();
    out.push(
        vec![
            "Item: ".dim(),
            record.item.id.clone().bold(),
            "  ".into(),
            "Phase: ".dim(),
            format!("{:?}", record.phase).into(),
        ]
        .into(),
    );
    out.push(Line::default());

    if show_live_text {
        let (selected_kind, selected_text) = match &record.item.details {
            ThreadItemDetails::AgentMessage(msg) => (LiveTextKind::Agent, msg.text.as_str()),
            ThreadItemDetails::Reasoning(r) => (LiveTextKind::Reasoning, r.text.as_str()),
            _ => (LiveTextKind::Agent, ""),
        };

        for (idx, block) in app.live_text_blocks.iter().enumerate() {
            if idx > 0 {
                out.push(Line::default());
            }
            match block.kind {
                LiveTextKind::Agent => out.push("Agent:".bold().into()),
                LiveTextKind::Reasoning => out.push("Reasoning:".bold().into()),
            }
            out.push(Line::default());
            push_markdown(&mut out, &block.text);
        }

        if record.phase == ItemPhase::InProgress && !selected_text.trim().is_empty() {
            out.push(Line::default());
            match selected_kind {
                LiveTextKind::Agent => out.push("Agent:".bold().into()),
                LiveTextKind::Reasoning => out.push("Reasoning:".bold().into()),
            }
            out.push(Line::default());
            push_markdown(&mut out, selected_text);
        }
        return out;
    }

    match &record.item.details {
        ThreadItemDetails::AgentMessage(msg) => push_markdown(&mut out, &msg.text),
        ThreadItemDetails::Reasoning(r) => push_markdown(&mut out, &r.text),
        ThreadItemDetails::CommandExecution(cmd) => {
            out.push(
                vec![
                    "Command: ".dim(),
                    strip_shell_launcher_prefix(&cmd.command).to_string().into(),
                ]
                .into(),
            );
            out.push(vec!["Status: ".dim(), format!("{:?}", cmd.status).into()].into());
            if let Some(code) = cmd.exit_code {
                out.push(vec!["Exit: ".dim(), format!("{code}").into()].into());
            }
            if !cmd.aggregated_output.trim().is_empty() {
                out.push(Line::default());
                out.push("Output:".bold().into());
                out.push(Line::default());
                push_wrapped(&mut out, &cmd.aggregated_output);
            }
        }
        ThreadItemDetails::FileChange(fc) => {
            out.push(vec!["Status: ".dim(), format!("{:?}", fc.status).into()].into());
            out.push(Line::default());
            out.push("Changes:".bold().into());
            out.push(Line::default());
            for change in &fc.changes {
                let kind_span: Span<'static> = match change.kind {
                    PatchChangeKind::Add => "A".green().bold(),
                    PatchChangeKind::Delete => "D".red().bold(),
                    PatchChangeKind::Update => "M".cyan().bold(),
                };
                out.push(
                    vec![
                        "  ".into(),
                        kind_span,
                        " ".dim(),
                        change.path.clone().into(),
                    ]
                    .into(),
                );
            }
        }
        ThreadItemDetails::McpToolCall(tc) => {
            out.push(vec!["Server: ".dim(), tc.server.clone().into()].into());
            out.push(vec!["Tool: ".dim(), tc.tool.clone().into()].into());
            out.push(vec!["Status: ".dim(), format!("{:?}", tc.status).into()].into());

            if tc.arguments != Value::Null {
                out.push(Line::default());
                out.push("Arguments:".bold().into());
                out.push(Line::default());
                push_wrapped(&mut out, &pretty_json(&tc.arguments));
            }

            if let Some(result) = &tc.result {
                out.push(Line::default());
                out.push("Result:".bold().into());
                out.push(Line::default());
                if let Some(structured) = &result.structured_content {
                    push_wrapped(&mut out, &pretty_json(structured));
                } else {
                    push_wrapped(
                        &mut out,
                        &format!("{} content block(s)", result.content.len()),
                    );
                }
            }

            if let Some(err) = &tc.error {
                out.push(Line::default());
                out.push("Error:".red().bold().into());
                out.push(Line::default());
                push_wrapped(&mut out, &err.message);
            }
        }
        ThreadItemDetails::CollabToolCall(call) => {
            out.push(vec!["Tool: ".dim(), format!("{:?}", call.tool).into()].into());
            out.push(vec!["Sender: ".dim(), call.sender_thread_id.clone().into()].into());
            out.push(vec!["Status: ".dim(), format!("{:?}", call.status).into()].into());

            if !call.receiver_thread_ids.is_empty() {
                out.push(Line::default());
                out.push("Receivers:".bold().into());
                out.push(Line::default());
                for receiver in &call.receiver_thread_ids {
                    out.push(vec!["  ".into(), receiver.clone().into()].into());
                }
            }

            if let Some(prompt) = call
                .prompt
                .as_deref()
                .filter(|prompt| !prompt.trim().is_empty())
            {
                out.push(Line::default());
                out.push("Prompt:".bold().into());
                out.push(Line::default());
                push_markdown(&mut out, prompt);
            }

            if !call.agents_states.is_empty() {
                out.push(Line::default());
                out.push("Agents:".bold().into());
                out.push(Line::default());
                let mut agents = call
                    .agents_states
                    .iter()
                    .map(|(thread_id, state)| (thread_id.clone(), state.clone()))
                    .collect::<Vec<_>>();
                agents.sort_by(|(a, _), (b, _)| a.cmp(b));
                for (thread_id, state) in agents {
                    let status = format!("{:?}", state.status);
                    let mut spans: Vec<Span<'static>> =
                        vec!["  ".into(), thread_id.dim(), "  ".into(), status.into()];
                    if let Some(message) = state.message.as_deref().filter(|m| !m.trim().is_empty())
                    {
                        spans.push("  ".into());
                        spans.push(trim_one_line(message).dim());
                    }
                    out.push(spans.into());
                }
            }
        }
        ThreadItemDetails::WebSearch(ws) => {
            out.push(vec!["Query: ".dim(), ws.query.clone().into()].into());
        }
        ThreadItemDetails::TodoList(list) => {
            out.push("Plan:".bold().into());
            out.push(Line::default());
            for item in &list.items {
                let checkbox: Span<'static> = if item.completed {
                    "[x]".green().bold()
                } else {
                    "[ ]".dim()
                };
                out.push(vec![checkbox, " ".into(), item.text.clone().into()].into());
            }
        }
        ThreadItemDetails::Error(err) => push_wrapped(&mut out, &err.message),
    }

    out
}

fn push_markdown(out: &mut Vec<Line<'static>>, text: &str) {
    let rendered = render_markdown_text(text);
    if rendered.lines.is_empty() {
        return;
    }
    out.extend(rendered.lines);
}

fn push_wrapped(out: &mut Vec<Line<'static>>, text: &str) {
    for wrapped in textwrap::wrap(text, 100) {
        out.push(Line::from(wrapped.to_string()));
    }
}

fn latest_prompt_lines(app: &ViewApp, inner_width: u16) -> Vec<Line<'static>> {
    let width = inner_width.saturating_sub(1).max(10) as usize;
    let Some(prompt) = app.current_prompt.as_deref() else {
        if app.meta.is_some() {
            return vec!["(no prompt)".dim().into()];
        }
        return vec!["(unknown; start with `exec-view`)".dim().into()];
    };

    let prompt = prompt.trim();
    if prompt.is_empty() {
        return vec!["(empty prompt)".dim().into()];
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    for raw_line in prompt.lines() {
        if raw_line.trim().is_empty() {
            out.push(Line::default());
            continue;
        }
        for wrapped in textwrap::wrap(raw_line, width) {
            out.push(Line::from(wrapped.to_string()));
        }
    }
    if out.is_empty() {
        out.push("(empty prompt)".dim().into());
    }
    out
}
