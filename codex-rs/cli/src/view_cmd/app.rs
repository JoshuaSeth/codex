use codex_exec::exec_events::ThreadEvent;
use codex_exec::exec_events::ThreadItem;
use codex_exec::exec_events::ThreadItemDetails;
use codex_exec::exec_events::Usage;
use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use ratatui::layout::Position;
use ratatui::widgets::ListState;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;

use crate::exec_view_meta::ExecViewMetaV1;

use super::process;
use super::util::extract_effective_prompt;

pub(super) enum ViewInput {
    ThreadEvent(Box<ThreadEvent>),
    AtEof,
    UnknownJson,
    InvalidJson,
    IoError { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ItemPhase {
    InProgress,
    Completed,
}

#[derive(Debug, Clone)]
pub(super) struct ItemRecord {
    pub(super) item: ThreadItem,
    pub(super) phase: ItemPhase,
}

#[derive(Debug, Clone)]
pub(super) enum LiveTextKind {
    Agent,
    Reasoning,
}

#[derive(Debug, Clone)]
pub(super) struct LiveTextBlock {
    pub(super) kind: LiveTextKind,
    pub(super) text: String,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ViewLayout {
    pub(super) follow_bar: ratatui::layout::Rect,
    pub(super) events_list: ratatui::layout::Rect,
    pub(super) details: ratatui::layout::Rect,
}

pub(super) struct ViewApp {
    pub(super) file: PathBuf,
    pub(super) meta: Option<ExecViewMetaV1>,
    pub(super) current_prompt: Option<String>,
    pub(super) thread_id: Option<String>,
    pub(super) event_stream_epoch: u64,
    pub(super) seen_thread_started: bool,
    pub(super) turns_started: u64,
    pub(super) turns_completed: u64,
    pub(super) last_usage: Option<Usage>,
    pub(super) last_error: Option<String>,
    pub(super) unknown_events: u64,
    pub(super) invalid_lines: u64,
    pub(super) items: Vec<ItemRecord>,
    pub(super) items_by_id: HashMap<String, usize>,
    pub(super) list_state: ListState,
    pub(super) should_quit: bool,
    pub(super) follow_latest_item: bool,
    pub(super) detail_scroll: u16,
    pub(super) detail_follow: bool,
    pub(super) live_text_blocks: Vec<LiveTextBlock>,
    pub(super) process_pid: Option<u32>,
    pub(super) show_quit_prompt: bool,
    pub(super) kill_on_exit: bool,
    pub(super) show_stop_prompt: bool,
    pub(super) stop_retry_on_confirm: bool,
    pub(super) queued_user_prompts: VecDeque<String>,
    pub(super) at_eof: bool,
    pub(super) last_turn_terminal: bool,
    pub(super) layout: Option<ViewLayout>,
    pub(super) composer_active: bool,
    pub(super) composer_text: String,
    pub(super) last_sent_prompt: Option<String>,
}

impl ViewApp {
    pub(super) fn new(
        file: PathBuf,
        process_pid: Option<u32>,
        meta: Option<ExecViewMetaV1>,
    ) -> Self {
        let current_prompt = meta
            .as_ref()
            .and_then(|meta| meta.current_prompt.clone())
            .or_else(|| {
                meta.as_ref()
                    .and_then(|meta| extract_effective_prompt(&meta.exec_args))
            });
        let queued_user_prompts = meta
            .as_ref()
            .map(|meta| {
                meta.queued_user_prompts
                    .iter()
                    .cloned()
                    .collect::<VecDeque<_>>()
            })
            .unwrap_or_default();

        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            file,
            meta,
            current_prompt,
            thread_id: None,
            event_stream_epoch: 0,
            seen_thread_started: false,
            turns_started: 0,
            turns_completed: 0,
            last_usage: None,
            last_error: None,
            unknown_events: 0,
            invalid_lines: 0,
            items: Vec::new(),
            items_by_id: HashMap::new(),
            list_state,
            should_quit: false,
            follow_latest_item: true,
            detail_scroll: 0,
            detail_follow: true,
            live_text_blocks: Vec::new(),
            process_pid,
            show_quit_prompt: false,
            kill_on_exit: false,
            show_stop_prompt: false,
            stop_retry_on_confirm: false,
            queued_user_prompts,
            at_eof: false,
            last_turn_terminal: false,
            layout: None,
            composer_active: false,
            composer_text: String::new(),
            last_sent_prompt: None,
        }
    }

    pub(super) fn handle_input(&mut self, input: ViewInput) {
        match input {
            ViewInput::ThreadEvent(ev) => {
                self.at_eof = false;
                self.handle_thread_event(*ev);
            }
            ViewInput::AtEof => self.at_eof = true,
            ViewInput::UnknownJson => self.unknown_events += 1,
            ViewInput::InvalidJson => self.invalid_lines += 1,
            ViewInput::IoError { message } => {
                self.at_eof = false;
                self.last_error = Some(message);
            }
        }
    }

    fn handle_thread_event(&mut self, event: ThreadEvent) {
        match event {
            ThreadEvent::ThreadStarted(ev) => {
                self.thread_id = Some(ev.thread_id);
                if self.seen_thread_started {
                    self.event_stream_epoch = self.event_stream_epoch.saturating_add(1);
                } else {
                    self.seen_thread_started = true;
                }
            }
            ThreadEvent::TurnStarted(_) => {
                self.turns_started += 1;
                self.last_turn_terminal = false;
            }
            ThreadEvent::TurnCompleted(ev) => {
                self.turns_completed += 1;
                self.last_usage = Some(ev.usage);
                self.last_turn_terminal = true;
            }
            ThreadEvent::TurnFailed(ev) => {
                self.last_error = Some(ev.error.message);
                self.last_turn_terminal = true;
            }
            ThreadEvent::Error(ev) => self.last_error = Some(ev.message),
            ThreadEvent::ItemStarted(ev) => {
                self.on_item_started(&ev.item);
                self.upsert_item(ev.item, ItemPhase::InProgress);
            }
            ThreadEvent::ItemUpdated(ev) => self.upsert_item(ev.item, ItemPhase::InProgress),
            ThreadEvent::ItemCompleted(ev) => {
                self.on_item_completed(&ev.item);
                self.upsert_item(ev.item, ItemPhase::Completed);
            }
        }
    }

    fn on_item_started(&mut self, item: &ThreadItem) {
        match item.details {
            ThreadItemDetails::AgentMessage(_) => {
                // The agent message should replace any prior reasoning in the details pane.
                self.live_text_blocks.clear();
            }
            ThreadItemDetails::Reasoning(_) => {
                // Consecutive reasoning items should accumulate, but reasoning should not bleed
                // across non-reasoning items (including agent messages).
                let is_continuation = self
                    .live_text_blocks
                    .last()
                    .is_some_and(|block| matches!(block.kind, LiveTextKind::Reasoning));
                if !is_continuation {
                    self.live_text_blocks.clear();
                }
            }
            _ => {
                self.live_text_blocks.clear();
            }
        }
    }

    fn on_item_completed(&mut self, item: &ThreadItem) {
        match &item.details {
            ThreadItemDetails::AgentMessage(msg) => {
                // Ensure the final agent output doesn't include any previous reasoning blocks.
                self.live_text_blocks.clear();
                self.append_live_text(LiveTextKind::Agent, msg.text.clone());
            }
            ThreadItemDetails::Reasoning(r) => {
                let is_continuation = self
                    .live_text_blocks
                    .last()
                    .is_some_and(|block| matches!(block.kind, LiveTextKind::Reasoning));
                if !is_continuation {
                    self.live_text_blocks.clear();
                }
                self.append_live_text(LiveTextKind::Reasoning, r.text.clone());
            }
            _ => {
                self.live_text_blocks.clear();
            }
        }
    }

    fn append_live_text(&mut self, kind: LiveTextKind, text: String) {
        self.live_text_blocks.push(LiveTextBlock { kind, text });
    }

    fn upsert_item(&mut self, item: ThreadItem, phase: ItemPhase) {
        let key = self.item_key(&item.id);
        let idx = if let Some(idx) = self.items_by_id.get(&key).copied() {
            idx
        } else {
            let should_select_new = match self.list_state.selected() {
                None => true,
                Some(selected) => selected + 1 == self.items.len(),
            };

            let idx = self.items.len();
            self.items_by_id.insert(key, idx);
            self.items.push(ItemRecord {
                item: item.clone(),
                phase,
            });
            if self.follow_latest_item && should_select_new {
                self.list_state.select(Some(idx));
                self.detail_follow = true;
                self.detail_scroll = 0;
            }
            idx
        };

        if let Some(existing) = self.items.get_mut(idx) {
            existing.item = item;
            existing.phase = phase;
        }
    }

    fn item_key(&self, item_id: &str) -> String {
        format!("{}:{item_id}", self.event_stream_epoch)
    }

    pub(super) fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.handle_quit_prompt_key(key) {
            return;
        }
        if self.handle_stop_prompt_key(key) {
            return;
        }
        if self.handle_composer_key(key) {
            return;
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if self.process_pid.is_some() {
                    self.show_quit_prompt = true;
                } else {
                    self.should_quit = true;
                }
            }
            (_, KeyCode::Char('s')) | (_, KeyCode::Char('S')) => {
                if self.process_pid.is_some() {
                    self.show_stop_prompt = true;
                    self.stop_retry_on_confirm = false;
                } else {
                    self.last_error = Some("No running process to stop.".to_string());
                }
            }
            (_, KeyCode::Up) | (_, KeyCode::Char('k')) => {
                self.follow_latest_item = false;
                self.select_prev();
                self.reset_detail_scroll_to_top();
            }
            (_, KeyCode::Down) | (_, KeyCode::Char('j')) => {
                self.follow_latest_item = false;
                self.select_next();
                self.reset_detail_scroll_to_top();
            }
            (_, KeyCode::Home) | (_, KeyCode::Char('g')) => {
                self.follow_latest_item = false;
                self.select_first();
                self.reset_detail_scroll_to_top();
            }
            (_, KeyCode::End) | (_, KeyCode::Char('G')) => {
                self.follow_latest_item = true;
                self.select_last();
                self.reset_detail_scroll_follow_latest();
            }
            (_, KeyCode::Char('f')) | (_, KeyCode::Char('F')) => {
                self.follow_latest_item = true;
                self.select_last();
                self.reset_detail_scroll_follow_latest();
            }
            (_, KeyCode::PageUp) | (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                self.follow_latest_item = false;
                self.detail_follow = false;
                self.detail_scroll = self.detail_scroll.saturating_sub(5);
            }
            (_, KeyCode::PageDown) | (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.follow_latest_item = false;
                self.detail_follow = false;
                self.detail_scroll = self.detail_scroll.saturating_add(5);
            }
            (_, KeyCode::Char('n')) | (_, KeyCode::Char('N')) => self.composer_active = true,
            (_, KeyCode::Char('i')) | (_, KeyCode::Char('I')) => {
                self.composer_active = true;
            }
            _ => {}
        }
    }

    fn handle_stop_prompt_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if !self.show_stop_prompt {
            return false;
        }

        match (key.modifiers, key.code) {
            (_, KeyCode::Esc) => {
                self.show_stop_prompt = false;
                self.stop_retry_on_confirm = false;
                return true;
            }
            (_, KeyCode::Char('r')) | (_, KeyCode::Char('R')) => {
                self.stop_retry_on_confirm = true;
                self.show_stop_prompt = false;
                self.perform_stop(Some(true));
                return true;
            }
            (_, KeyCode::Enter) | (_, KeyCode::Char('y')) | (_, KeyCode::Char('Y')) => {
                let retry = self.stop_retry_on_confirm;
                self.show_stop_prompt = false;
                self.stop_retry_on_confirm = false;
                self.perform_stop(Some(retry));
                return true;
            }
            _ => {}
        }

        true
    }

    fn handle_composer_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if !self.composer_active {
            return false;
        }

        match (key.modifiers, key.code) {
            (_, KeyCode::Esc) => {
                self.composer_active = false;
                return true;
            }
            (KeyModifiers::SHIFT, KeyCode::Enter) => {
                self.composer_text.push('\n');
                return true;
            }
            (KeyModifiers::NONE, KeyCode::Enter) | (KeyModifiers::CONTROL, KeyCode::Enter) => {
                let text = self.composer_text.trim_end().to_string();
                if text.trim().is_empty() {
                    self.last_error = Some("Cannot send an empty prompt.".to_string());
                    return true;
                }
                self.last_sent_prompt = Some(text.clone());

                let Some(meta_for_spawn) = self.meta.clone() else {
                    self.last_error = Some(
                        "Cannot send a prompt: missing exec-view metadata (start with `exec-view`)"
                            .to_string(),
                    );
                    return true;
                };

                // If we know the thread id, enqueue a follow-up for that thread. If we don't,
                // start a new `codex exec` process and let it write thread.started into the file.
                if self.thread_id.is_some() {
                    self.queued_user_prompts.push_back(text.clone());
                    self.persist_queue_to_meta();
                    self.current_prompt = Some(text);
                    if let Err(err) = process::touch_events_file(&self.file) {
                        self.last_error = Some(format!("Failed to update events file: {err}"));
                    }
                    self.composer_text.clear();
                    self.composer_active = false;
                    return true;
                }

                match process::spawn_new_exec(&self.file, &meta_for_spawn, &text) {
                    Ok(pid) => {
                        self.current_prompt = Some(text);
                        self.last_turn_terminal = false;
                        self.process_pid = Some(pid);
                        if let Some(meta) = self.meta.as_mut() {
                            meta.process_pid = Some(pid);
                            meta.current_prompt = self.current_prompt.clone();
                            if let Err(err) = meta.save_for_events(&self.file) {
                                self.last_error = Some(format!("Failed to update meta: {err}"));
                            }
                        }
                        if let Err(err) = process::touch_events_file(&self.file) {
                            self.last_error = Some(format!("Failed to update events file: {err}"));
                        }
                        self.composer_text.clear();
                        self.composer_active = false;
                    }
                    Err(err) => {
                        self.last_error = Some(format!("Failed to start exec: {err}"));
                    }
                }
                return true;
            }
            (_, KeyCode::Backspace) => {
                self.composer_text.pop();
                return true;
            }
            (_, KeyCode::Tab) => {
                self.composer_text.push_str("  ");
                return true;
            }
            (_, KeyCode::Char(c)) => {
                self.composer_text.push(c);
                return true;
            }
            _ => {}
        }

        true
    }

    pub(super) fn on_mouse(&mut self, mouse: MouseEvent, mouse_enabled: bool) {
        if !mouse_enabled || self.show_quit_prompt || self.show_stop_prompt || self.composer_active
        {
            return;
        }
        let Some(layout) = self.layout else {
            return;
        };
        let pos = Position {
            x: mouse.column,
            y: mouse.row,
        };

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if layout.follow_bar.contains(pos) {
                    self.follow_latest_item = true;
                    self.select_last();
                    self.reset_detail_scroll_follow_latest();
                    return;
                }

                if layout.events_list.contains(pos) {
                    let inner_y0 = layout.events_list.y.saturating_add(1);
                    let inner_h = layout.events_list.height.saturating_sub(2);
                    if inner_h == 0 {
                        return;
                    }
                    if mouse.row < inner_y0 || mouse.row >= inner_y0.saturating_add(inner_h) {
                        return;
                    }

                    let offset = self.list_state.offset();
                    let rel = (mouse.row - inner_y0) as usize;
                    let idx = offset.saturating_add(rel);
                    if idx < self.items.len() {
                        self.follow_latest_item = false;
                        self.list_state.select(Some(idx));
                        self.reset_detail_scroll_to_top();
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                if layout.details.contains(pos) {
                    self.follow_latest_item = false;
                    self.detail_follow = false;
                    self.detail_scroll = self.detail_scroll.saturating_sub(3);
                } else if layout.events_list.contains(pos) {
                    self.follow_latest_item = false;
                    self.select_prev();
                    self.reset_detail_scroll_to_top();
                }
            }
            MouseEventKind::ScrollDown => {
                if layout.details.contains(pos) {
                    self.follow_latest_item = false;
                    self.detail_follow = false;
                    self.detail_scroll = self.detail_scroll.saturating_add(3);
                } else if layout.events_list.contains(pos) {
                    self.follow_latest_item = false;
                    self.select_next();
                    self.reset_detail_scroll_to_top();
                }
            }
            _ => {}
        }
    }

    fn handle_quit_prompt_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if !self.show_quit_prompt {
            return false;
        }

        match (key.modifiers, key.code) {
            (_, KeyCode::Esc) => {
                self.show_quit_prompt = false;
                return true;
            }
            (_, KeyCode::Enter) | (_, KeyCode::Char('n')) | (_, KeyCode::Char('N')) => {
                self.show_quit_prompt = false;
                self.should_quit = true;
                return true;
            }
            (_, KeyCode::Char('y')) | (_, KeyCode::Char('Y')) => {
                self.kill_on_exit = true;
                self.show_quit_prompt = false;
                self.should_quit = true;
                return true;
            }
            _ => {}
        }

        true
    }

    fn persist_queue_to_meta(&mut self) {
        let Some(meta) = self.meta.as_mut() else {
            return;
        };
        meta.queued_user_prompts = self.queued_user_prompts.iter().cloned().collect();
        if let Err(err) = meta.save_for_events(&self.file) {
            self.last_error = Some(format!("Failed to update meta: {err}"));
        }
    }

    pub(super) fn poll_process_pid(&mut self) {
        let Some(pid) = self.process_pid else {
            return;
        };
        if process::process_is_running(pid) {
            return;
        }

        self.process_pid = None;
        if let Some(meta) = self.meta.as_mut() {
            meta.process_pid = None;
            if let Err(err) = meta.save_for_events(&self.file) {
                self.last_error = Some(format!("Failed to update meta: {err}"));
            }
        }
    }

    pub(super) fn maybe_spawn_next_queued_prompt(&mut self) {
        if self.show_quit_prompt || self.composer_active {
            return;
        }
        let Some(meta_for_spawn) = self.meta.clone() else {
            return;
        };
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        if self.queued_user_prompts.is_empty() {
            return;
        }

        if let Some(pid) = self.process_pid
            && process::process_is_running(pid)
        {
            return;
        }

        let Some(next) = self.queued_user_prompts.front().cloned() else {
            return;
        };

        match process::spawn_follow_up_exec(&self.file, &meta_for_spawn, &thread_id, &next) {
            Ok(pid) => {
                self.queued_user_prompts.pop_front();
                self.current_prompt = Some(next);
                self.last_turn_terminal = false;
                self.process_pid = Some(pid);
                if let Some(meta) = self.meta.as_mut() {
                    meta.process_pid = Some(pid);
                    meta.current_prompt = self.current_prompt.clone();
                    meta.queued_user_prompts = self.queued_user_prompts.iter().cloned().collect();
                    if let Err(err) = meta.save_for_events(&self.file) {
                        self.last_error = Some(format!("Failed to update meta: {err}"));
                    }
                }
                if let Err(err) = process::touch_events_file(&self.file) {
                    self.last_error = Some(format!("Failed to update events file: {err}"));
                }
            }
            Err(err) => {
                self.last_error = Some(format!("Failed to start follow-up: {err}"));
            }
        }
    }

    fn perform_stop(&mut self, retry: Option<bool>) {
        let should_retry = retry.unwrap_or(false);

        if let Some(pid) = self.process_pid {
            let _ = process::terminate_process(pid);
            self.process_pid = None;
            if let Some(meta) = self.meta.as_mut() {
                meta.process_pid = None;
                if let Err(err) = meta.save_for_events(&self.file) {
                    self.last_error = Some(format!("Failed to update meta: {err}"));
                }
            }
        }

        if !should_retry {
            return;
        }

        let Some(prompt) = self
            .last_sent_prompt
            .clone()
            .or_else(|| self.current_prompt.clone())
        else {
            self.last_error = Some("No previous prompt to retry.".to_string());
            return;
        };

        if self.thread_id.is_some() {
            self.queued_user_prompts.push_front(prompt);
            self.persist_queue_to_meta();
            return;
        }

        let Some(meta_for_spawn) = self.meta.clone() else {
            self.last_error = Some("Missing exec-view metadata; cannot retry.".to_string());
            return;
        };
        match process::spawn_new_exec(&self.file, &meta_for_spawn, &prompt) {
            Ok(pid) => {
                self.process_pid = Some(pid);
                if let Some(meta) = self.meta.as_mut() {
                    meta.process_pid = Some(pid);
                    meta.current_prompt = Some(prompt);
                    if let Err(err) = meta.save_for_events(&self.file) {
                        self.last_error = Some(format!("Failed to update meta: {err}"));
                    }
                }
            }
            Err(err) => self.last_error = Some(format!("Failed to start exec: {err}")),
        }
    }

    fn reset_detail_scroll_follow_latest(&mut self) {
        self.detail_follow = true;
        self.detail_scroll = 0;
    }

    fn reset_detail_scroll_to_top(&mut self) {
        self.detail_follow = false;
        self.detail_scroll = 0;
    }

    fn select_prev(&mut self) {
        let Some(selected) = self.list_state.selected() else {
            self.list_state.select(Some(0));
            return;
        };
        self.list_state.select(Some(selected.saturating_sub(1)));
    }

    fn select_next(&mut self) {
        let Some(selected) = self.list_state.selected() else {
            self.list_state.select(Some(0));
            return;
        };
        if selected + 1 < self.items.len() {
            self.list_state.select(Some(selected + 1));
        }
    }

    fn select_first(&mut self) {
        if !self.items.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        if !self.items.is_empty() {
            self.list_state.select(Some(self.items.len() - 1));
        }
    }
}
