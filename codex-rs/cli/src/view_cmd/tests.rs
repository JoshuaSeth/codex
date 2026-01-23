use super::app::ViewApp;
use super::process::build_follow_up_exec_args;
use super::process::build_new_exec_args;
use super::util::strip_shell_launcher_prefix;
use super::util::trim_one_line;
use crate::exec_view_meta::ExecViewMetaV1;
use crate::exec_view_meta::RootOverrides;
use codex_exec::exec_events::AgentMessageItem;
use codex_exec::exec_events::ItemCompletedEvent;
use codex_exec::exec_events::ReasoningItem;
use codex_exec::exec_events::ThreadEvent;
use codex_exec::exec_events::ThreadItem;
use codex_exec::exec_events::ThreadItemDetails;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use tempfile::tempdir;

#[test]
fn strips_bin_zsh_lc_prefix() {
    assert_eq!(
        strip_shell_launcher_prefix("/bin/zsh -lc 'echo hi'"),
        "'echo hi'"
    );
}

#[test]
fn strips_usr_bin_zsh_lc_prefix() {
    assert_eq!(
        strip_shell_launcher_prefix("/usr/bin/zsh -lc \"echo hi\""),
        "\"echo hi\""
    );
}

#[test]
fn strips_plain_zsh_lc_prefix() {
    assert_eq!(strip_shell_launcher_prefix("zsh -lc ls"), "ls");
}

#[test]
fn leaves_other_commands_unchanged() {
    assert_eq!(strip_shell_launcher_prefix("git status"), "git status");
    assert_eq!(
        strip_shell_launcher_prefix("/bin/bash -lc 'ls'"),
        "/bin/bash -lc 'ls'"
    );
    assert_eq!(
        strip_shell_launcher_prefix("/bin/zsh -c ls"),
        "/bin/zsh -c ls"
    );
}

#[test]
fn view_does_not_jump_to_latest_when_user_moved_selection() {
    let mut app = ViewApp::new(PathBuf::from("test.events.jsonl"), None, None);
    app.items.clear();
    app.list_state.select(None);

    app.handle_input(super::app::ViewInput::ThreadEvent(Box::new(
        ThreadEvent::ThreadStarted(codex_exec::exec_events::ThreadStartedEvent {
            thread_id: "thread-1".to_string(),
        }),
    )));
    app.handle_input(super::app::ViewInput::ThreadEvent(Box::new(
        ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::AgentMessage(AgentMessageItem {
                    text: "one".to_string(),
                }),
            },
        }),
    )));
    app.handle_input(super::app::ViewInput::ThreadEvent(Box::new(
        ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_1".to_string(),
                details: ThreadItemDetails::AgentMessage(AgentMessageItem {
                    text: "two".to_string(),
                }),
            },
        }),
    )));

    app.follow_latest_item = false;
    app.list_state.select(Some(0));

    app.handle_input(super::app::ViewInput::ThreadEvent(Box::new(
        ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_2".to_string(),
                details: ThreadItemDetails::Reasoning(ReasoningItem {
                    text: "three".to_string(),
                }),
            },
        }),
    )));

    assert_eq!(app.list_state.selected(), Some(0));
}

#[test]
fn follow_up_runs_can_reuse_item_ids_without_overwriting_previous_items() {
    let mut app = ViewApp::new(PathBuf::from("test.events.jsonl"), None, None);
    app.items.clear();
    app.list_state.select(None);

    app.handle_input(super::app::ViewInput::ThreadEvent(Box::new(
        ThreadEvent::ThreadStarted(codex_exec::exec_events::ThreadStartedEvent {
            thread_id: "thread-1".to_string(),
        }),
    )));
    app.handle_input(super::app::ViewInput::ThreadEvent(Box::new(
        ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::AgentMessage(AgentMessageItem {
                    text: "first run".to_string(),
                }),
            },
        }),
    )));

    app.handle_input(super::app::ViewInput::ThreadEvent(Box::new(
        ThreadEvent::ThreadStarted(codex_exec::exec_events::ThreadStartedEvent {
            thread_id: "thread-1".to_string(),
        }),
    )));
    app.handle_input(super::app::ViewInput::ThreadEvent(Box::new(
        ThreadEvent::ItemStarted(build_item_started("item_0", "second run")),
    )));

    assert_eq!(app.items.len(), 2);
    assert_eq!(
        app.items[0].item.details,
        ThreadItemDetails::AgentMessage(AgentMessageItem {
            text: "first run".to_string(),
        })
    );
    assert_eq!(
        app.items[1].item.details,
        ThreadItemDetails::AgentMessage(AgentMessageItem {
            text: "second run".to_string(),
        })
    );
}

fn build_item_started(id: &str, text: &str) -> codex_exec::exec_events::ItemStartedEvent {
    codex_exec::exec_events::ItemStartedEvent {
        item: ThreadItem {
            id: id.to_string(),
            details: ThreadItemDetails::AgentMessage(AgentMessageItem {
                text: text.to_string(),
            }),
        },
    }
}

#[test]
fn consecutive_reasoning_items_accumulate_but_agent_replaces_them() {
    let mut app = ViewApp::new(PathBuf::from("test.events.jsonl"), None, None);
    app.items.clear();
    app.list_state.select(None);

    app.handle_input(super::app::ViewInput::ThreadEvent(Box::new(
        ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::Reasoning(ReasoningItem {
                    text: "r1".to_string(),
                }),
            },
        }),
    )));
    app.handle_input(super::app::ViewInput::ThreadEvent(Box::new(
        ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_1".to_string(),
                details: ThreadItemDetails::Reasoning(ReasoningItem {
                    text: "r2".to_string(),
                }),
            },
        }),
    )));

    assert_eq!(app.live_text_blocks.len(), 2);
    assert_eq!(app.live_text_blocks[0].text, "r1");
    assert_eq!(app.live_text_blocks[1].text, "r2");

    app.handle_input(super::app::ViewInput::ThreadEvent(Box::new(
        ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_2".to_string(),
                details: ThreadItemDetails::AgentMessage(AgentMessageItem {
                    text: "a1".to_string(),
                }),
            },
        }),
    )));

    assert_eq!(app.live_text_blocks.len(), 1);
    assert_eq!(app.live_text_blocks[0].text, "a1");
}

#[test]
fn quit_prompt_default_is_view_only() {
    let mut app = ViewApp::new(PathBuf::from("test.events.jsonl"), Some(123), None);
    app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(app.show_quit_prompt, true);
    assert_eq!(app.should_quit, false);

    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.should_quit, true);
    assert_eq!(app.kill_on_exit, false);
}

#[test]
fn quit_prompt_y_kills_process() {
    let mut app = ViewApp::new(PathBuf::from("test.events.jsonl"), Some(123), None);
    app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(app.show_quit_prompt, true);

    app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(app.should_quit, true);
    assert_eq!(app.kill_on_exit, true);
}

#[test]
fn q_does_not_quit_viewer() {
    let mut app = ViewApp::new(PathBuf::from("test.events.jsonl"), Some(123), None);
    app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
    assert_eq!(app.show_quit_prompt, false);
    assert_eq!(app.should_quit, false);
}

#[test]
fn trim_one_line_does_not_panic_on_unicode() {
    let input = "é".repeat(200);
    let out = trim_one_line(&input);
    assert_eq!(out.chars().count(), 80);
    assert_eq!(out.chars().last(), Some('…'));
}

#[test]
fn queue_prompt_requires_exec_view_meta() {
    let mut app = ViewApp::new(PathBuf::from("test.events.jsonl"), None, None);
    app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.composer_active, true);

    app.composer_text = "next".to_string();
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.last_error.is_some());
}

#[test]
fn composer_enter_enqueues_non_empty() {
    let dir = tempdir().expect("tempdir");
    let events_file = dir.path().join("test.events.jsonl");
    std::fs::write(&events_file, "").expect("create events file");
    let meta = ExecViewMetaV1::new(
        RootOverrides::default(),
        vec!["--json".to_string(), "--skip-git-repo-check".to_string()],
        PathBuf::from("stderr.log"),
    );
    let mut app = ViewApp::new(events_file, None, Some(meta));
    app.thread_id = Some("thread-123".to_string());
    app.on_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
    assert_eq!(app.composer_active, true);

    app.composer_text = "next".to_string();
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.composer_active, false);
    assert_eq!(app.queued_user_prompts.len(), 1);
    assert_eq!(app.queued_user_prompts.front().unwrap(), "next");
}

#[test]
fn stop_retry_requeues_last_prompt_for_follow_up() {
    let dir = tempdir().expect("tempdir");
    let events_file = dir.path().join("test.events.jsonl");
    std::fs::write(&events_file, "").expect("create events file");
    let meta = ExecViewMetaV1::new(
        RootOverrides::default(),
        vec!["--json".to_string(), "--skip-git-repo-check".to_string()],
        PathBuf::from("stderr.log"),
    );
    let mut app = ViewApp::new(events_file, Some(123), Some(meta));
    app.thread_id = Some("thread-123".to_string());
    app.last_sent_prompt = Some("do the thing".to_string());
    app.process_pid = None; // avoid actually killing anything in a unit test
    app.queued_user_prompts.clear();
    app.queued_user_prompts.push_back("later".to_string());

    // Simulate "stop+retry" logic by directly triggering the stop prompt handler path.
    app.show_stop_prompt = true;
    app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));

    assert_eq!(app.show_stop_prompt, false);
    assert_eq!(
        app.queued_user_prompts.front().map(String::as_str),
        Some("do the thing")
    );
}

#[test]
fn composer_enter_does_not_enqueue_empty() {
    let dir = tempdir().expect("tempdir");
    let events_file = dir.path().join("test.events.jsonl");
    std::fs::write(&events_file, "").expect("create events file");
    let meta = ExecViewMetaV1::new(
        RootOverrides::default(),
        vec!["--json".to_string(), "--skip-git-repo-check".to_string()],
        PathBuf::from("stderr.log"),
    );
    let mut app = ViewApp::new(events_file, None, Some(meta));
    app.on_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
    assert_eq!(app.composer_active, true);

    app.composer_text = "   ".to_string();
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.composer_active, true);
    assert_eq!(app.queued_user_prompts.len(), 0);
}

#[test]
fn follow_up_exec_args_preserve_key_flags() {
    let mut meta = ExecViewMetaV1::new(
        RootOverrides::default(),
        vec![
            "--json".to_string(),
            "--skip-git-repo-check".to_string(),
            "--model".to_string(),
            "gpt-5.2-codex".to_string(),
            "--yolo".to_string(),
            "--cd".to_string(),
            "/tmp".to_string(),
        ],
        PathBuf::from("stderr.log"),
    );
    meta.process_pid = Some(123);

    let args = build_follow_up_exec_args(&meta, "thread-123", "next").unwrap();
    let args: Vec<String> = args
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        vec![
            "--json",
            "--skip-git-repo-check",
            "--model",
            "gpt-5.2-codex",
            "--yolo",
            "--cd",
            "/tmp",
            "next",
            "resume",
            "thread-123"
        ]
    );
}

#[test]
fn new_exec_args_preserve_key_flags() -> anyhow::Result<()> {
    let meta = ExecViewMetaV1::new(
        RootOverrides::default(),
        vec![
            "--json".to_string(),
            "--skip-git-repo-check".to_string(),
            "--model".to_string(),
            "gpt-5.2-codex".to_string(),
            "--yolo".to_string(),
            "--cd".to_string(),
            "/tmp".to_string(),
        ],
        PathBuf::from("stderr.log"),
    );

    let argv = build_new_exec_args(&meta, "hello")?
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--model" && w[1] == "gpt-5.2-codex")
    );
    assert!(argv.iter().any(|a| a == "--yolo"));
    assert!(argv.windows(2).any(|w| w[0] == "--cd" && w[1] == "/tmp"));
    assert!(!argv.iter().any(|a| a == "resume"));
    assert_eq!(argv.last().map(String::as_str), Some("hello"));
    Ok(())
}
