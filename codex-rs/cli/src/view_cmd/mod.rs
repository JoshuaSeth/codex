use anyhow::Context;
use clap::Parser;
use codex_common::CliConfigOverrides;
use crossterm::event::DisableMouseCapture;
use crossterm::event::EnableMouseCapture;
use crossterm::event::Event as CrosstermEvent;
use crossterm::execute;
use crossterm::terminal;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::exec_view_meta::ExecViewMetaV1;

mod app;
mod draw;
mod process;
mod resolve;
mod tail;
mod util;

#[cfg(test)]
mod tests;

/// View a `codex exec --json` JSONL event stream from a file (read-only).
#[derive(Debug, Parser)]
pub struct ViewCli {
    /// Path to a JSONL file produced by `codex exec --json`.
    #[arg(value_name = "EVENTS_FILE")]
    pub file: Option<PathBuf>,

    /// Initialize exec-view metadata next to the events file when missing.
    ///
    /// This enables prompting from the viewer (spawning detached `codex exec` processes).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub init: bool,

    /// Seed the events file from an existing thread id (writes `thread.started` + persisted items),
    /// enabling a viewer-first resume workflow.
    #[arg(long, value_name = "THREAD_ID")]
    pub resume: Option<String>,

    /// Follow the file as it grows.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub follow: bool,

    /// Start reading from the end of the file (implies --follow).
    #[arg(long, default_value_t = false)]
    pub tail: bool,

    /// Poll interval when following and the file is at EOF.
    #[arg(long, default_value_t = 200)]
    pub poll_ms: u64,

    /// PID of a `codex exec` process writing this events file.
    ///
    /// When set, pressing Ctrl+C prompts whether to close only the viewer or also terminate the process.
    #[arg(long = "process-pid", value_name = "PID")]
    pub process_pid: Option<u32>,

    /// Disable mouse interactions (click-to-select + follow-latest button).
    #[arg(long = "no-mouse", default_value_t = false)]
    pub no_mouse: bool,
}

struct TerminalGuard {
    mouse_enabled: bool,
}

impl TerminalGuard {
    fn new(mouse_enabled: bool) -> anyhow::Result<Self> {
        terminal::enable_raw_mode().context("enable raw mode")?;
        if mouse_enabled {
            execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture)
                .context("enter alternate screen")?;
        } else {
            execute!(std::io::stdout(), EnterAlternateScreen).context("enter alternate screen")?;
        }
        Ok(Self { mouse_enabled })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.mouse_enabled {
            let _ = execute!(std::io::stdout(), DisableMouseCapture);
        }
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

pub fn run_view(cli: ViewCli, root_overrides: &CliConfigOverrides) -> anyhow::Result<()> {
    let follow = cli.follow || cli.tail;
    let poll = Duration::from_millis(cli.poll_ms);

    let file = resolve::resolve_events_file(cli.file, root_overrides)?;
    if !follow && !file.exists() {
        anyhow::bail!("Events file does not exist: {}", file.display());
    }

    if let Some(thread_id) = cli.resume.as_deref() {
        std::fs::create_dir_all(file.parent().unwrap_or_else(|| std::path::Path::new(".")))
            .with_context(|| format!("create {}", file.display()))?;
        if !file.exists() {
            std::fs::write(&file, "").with_context(|| format!("create {}", file.display()))?;
        }
        crate::exec_view_cmd::seed_events_file_from_rollout(&file, root_overrides, thread_id)
            .with_context(|| format!("seed {} for resume {}", file.display(), thread_id))?;
    }

    let (tx, rx) = mpsc::channel::<app::ViewInput>();
    let stop = Arc::new(AtomicBool::new(false));

    {
        let path = file.clone();
        let tx = tx;
        let stop = Arc::clone(&stop);
        thread::spawn(move || tail::tail_events_file(&path, follow, cli.tail, poll, tx, stop));
    }

    let _guard = TerminalGuard::new(!cli.no_mouse)?;
    let stdout = std::io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let mut meta = ExecViewMetaV1::load_for_events(&file)?;
    if meta.is_none() && cli.init {
        let stderr_path = file.with_extension("stderr.log");
        let mut meta_new = ExecViewMetaV1::new(
            crate::exec_view_meta::RootOverrides {
                raw_overrides: root_overrides.raw_overrides.clone(),
                config_home: root_overrides.config_home.clone(),
                config_file: root_overrides.config_file.clone(),
            },
            vec!["--json".to_string(), "--skip-git-repo-check".to_string()],
            stderr_path,
        );
        meta_new.current_prompt = None;
        meta_new.process_pid = None;
        meta_new.save_for_events(&file)?;
        meta = Some(meta_new);
    }

    let mut process_pid = cli.process_pid;
    if process_pid.is_none() {
        process_pid = meta.as_ref().and_then(|m| m.process_pid);
    }
    if process_pid.is_some_and(|pid| !process::process_is_running(pid)) {
        process_pid = None;
        if let Some(meta) = meta.as_mut() {
            meta.process_pid = None;
            meta.save_for_events(&file)?;
        }
    }
    if cli.process_pid.is_some()
        && let Some(meta) = meta.as_mut()
    {
        meta.process_pid = process_pid;
        meta.save_for_events(&file)?;
    }

    let mut app = app::ViewApp::new(file, process_pid, meta);

    loop {
        while let Ok(input) = rx.try_recv() {
            app.handle_input(input);
        }

        app.poll_process_pid();
        app.maybe_spawn_next_queued_prompt();

        terminal.draw(|f| draw::draw_view(f, &mut app))?;

        if app.should_quit {
            break;
        }

        if crossterm::event::poll(Duration::from_millis(50)).unwrap_or(false) {
            match crossterm::event::read() {
                Ok(CrosstermEvent::Key(key)) => app.on_key(key),
                Ok(CrosstermEvent::Mouse(mouse)) => app.on_mouse(mouse, !cli.no_mouse),
                Ok(CrosstermEvent::Resize(_, _)) => {}
                Ok(_) => {}
                Err(err) => {
                    app.last_error = Some(format!("Input error: {err}"));
                }
            }
        }

        thread::sleep(Duration::from_millis(16));
    }

    stop.store(true, Ordering::Relaxed);
    if app.kill_on_exit
        && let Some(pid) = app.process_pid
    {
        let _ = process::terminate_process(pid);
    }
    Ok(())
}
