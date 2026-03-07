use anyhow::Context;
use clap::Parser;
use codex_core::config::find_codex_home;
use codex_utils_cli::CliConfigOverrides;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crate::exec_view_meta::ExecViewMetaV1;
use crate::exec_view_meta::RootOverrides;

/// Run `codex exec --json` detached and print the thread id immediately.
///
/// The detached process writes its JSONL stream to an events file (and stderr to a log file).
#[derive(Debug, Parser)]
pub struct ExecAsyncCli {
    /// Write JSONL events to this file (created if missing).
    #[arg(long = "events-file", value_name = "FILE")]
    pub events_file: Option<PathBuf>,

    /// Write stderr logs to this file (created if missing).
    #[arg(long = "stderr-file", value_name = "FILE")]
    pub stderr_file: Option<PathBuf>,

    /// Overwrite existing output files.
    #[arg(long = "overwrite", default_value_t = false)]
    pub overwrite: bool,

    /// How long to wait for the thread id when it isn't known upfront (new session or resume --last).
    #[arg(long = "wait-ms", default_value_t = 5_000)]
    pub wait_ms: u64,

    /// Poll interval when waiting for the thread.started event to appear in the file.
    #[arg(long = "poll-ms", default_value_t = 50)]
    pub poll_ms: u64,

    /// Arguments forwarded to `codex exec` (same syntax as `codex exec`).
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "EXEC_ARGS"
    )]
    pub exec_args: Vec<OsString>,
}

pub fn run_exec_async(
    cli: ExecAsyncCli,
    root_overrides: &CliConfigOverrides,
) -> anyhow::Result<()> {
    let events_path = match cli.events_file {
        Some(path) => path,
        None => default_events_path(root_overrides)?,
    };
    let stderr_path = cli
        .stderr_file
        .clone()
        .unwrap_or_else(|| events_path.with_extension("stderr.log"));

    create_output_file(&events_path, cli.overwrite)
        .with_context(|| format!("create events file {}", events_path.display()))?;
    create_output_file(&stderr_path, cli.overwrite)
        .with_context(|| format!("create stderr file {}", stderr_path.display()))?;

    // If this is an explicit resume, seed the events file with prior agent messages/reasoning so
    // `codex view` has immediate context.
    if let Some(thread_id) = parse_resume_thread_id(&cli.exec_args) {
        crate::exec_view_cmd::seed_events_file_from_rollout(
            &events_path,
            root_overrides,
            &thread_id,
        )
        .with_context(|| {
            format!(
                "seed events file {} for resume {thread_id}",
                events_path.display()
            )
        })?;
    }

    let events_file = open_append_file(&events_path)
        .with_context(|| format!("open events file {}", events_path.display()))?;
    let stderr_file = open_append_file(&stderr_path)
        .with_context(|| format!("open stderr file {}", stderr_path.display()))?;

    let exe = std::env::current_exe().context("resolve current executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("exec");

    // Propagate root-level config overrides.
    for raw in &root_overrides.raw_overrides {
        cmd.arg("-c").arg(raw);
    }

    let json_missing = !cli
        .exec_args
        .iter()
        .any(|a| a == "--json" || a == "--experimental-json");
    if json_missing {
        cmd.arg("--json");
    }
    let mut exec_args = cli.exec_args.clone();
    if !exec_args.iter().any(|a| a == "--skip-git-repo-check") {
        exec_args.insert(0, "--skip-git-repo-check".into());
    }
    cmd.args(&exec_args);

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(events_file));
    cmd.stderr(Stdio::from(stderr_file));

    crate::exec_view_cmd::detach_child_process(&mut cmd);
    let child = cmd.spawn().context("spawn detached `codex exec`")?;
    let pid = child.id();
    drop(child);

    let mut meta_exec_args: Vec<String> = Vec::new();
    if json_missing {
        meta_exec_args.push("--json".to_string());
    }
    meta_exec_args.extend(exec_args.iter().map(|a| a.to_string_lossy().into_owned()));
    let mut meta = ExecViewMetaV1::new(
        RootOverrides {
            raw_overrides: root_overrides.raw_overrides.clone(),
            config_home: None,
            config_file: None,
        },
        meta_exec_args,
        stderr_path.clone(),
    );
    meta.current_prompt = crate::exec_view_cmd::extract_effective_prompt(&meta.exec_args);
    meta.process_pid = Some(pid);
    meta.save_for_events(&events_path)?;

    let thread_id = match parse_resume_thread_id(&cli.exec_args) {
        Some(thread_id) => thread_id,
        None => crate::exec_view_cmd::wait_for_thread_id(
            &events_path,
            Duration::from_millis(cli.wait_ms),
            Duration::from_millis(cli.poll_ms),
        )
        .with_context(|| format!("wait for thread id in {}", events_path.display()))?,
    };

    let _ =
        crate::exec_view_cmd::write_thread_events_pointer(root_overrides, &thread_id, &events_path);

    eprintln!(
        "Started `codex exec` (pid={pid:?}); thread: {thread_id}; events: {}; stderr: {}",
        events_path.display(),
        stderr_path.display()
    );
    let inv = crate::exec_view_cmd::invocation_name();
    eprintln!(
        "Attach with: {inv} view {}",
        crate::exec_view_cmd::shell_quote(events_path.as_os_str().to_string_lossy().as_ref())
    );

    println!("{thread_id}");
    Ok(())
}

fn default_events_path(_root_overrides: &CliConfigOverrides) -> anyhow::Result<PathBuf> {
    let codex_home = find_codex_home().unwrap_or_else(|_| default_codex_home());

    let dir = codex_home.join("live").join("exec-async");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let pid = std::process::id();
    Ok(dir.join(format!("exec-async-{now}-{pid}.events.jsonl")))
}

fn default_codex_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

fn open_output_file(path: &PathBuf, overwrite: bool) -> anyhow::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let mut opts = OpenOptions::new();
    opts.write(true).create(true);
    if overwrite {
        opts.truncate(true);
    } else {
        opts.create_new(true);
    }
    opts.open(path).map_err(Into::into)
}

fn create_output_file(path: &PathBuf, overwrite: bool) -> anyhow::Result<()> {
    let _ = open_output_file(path, overwrite)?;
    Ok(())
}

fn open_append_file(path: &PathBuf) -> anyhow::Result<std::fs::File> {
    OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(Into::into)
}

fn parse_resume_thread_id(exec_args: &[OsString]) -> Option<String> {
    let mut it = exec_args.iter().peekable();
    while let Some(arg) = it.next() {
        let arg = arg.to_string_lossy();
        if arg != "resume" {
            continue;
        }
        let next = it.peek()?;
        let next = next.to_string_lossy();
        if next.starts_with('-') {
            return None;
        }
        return Some(next.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_resume_thread_id;
    use crate::exec_view_cmd::wait_for_thread_id;
    use codex_exec::exec_events::ThreadEvent;
    use codex_exec::exec_events::ThreadStartedEvent;
    use pretty_assertions::assert_eq;
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn parse_resume_thread_id_finds_explicit_id() {
        let args = vec![
            OsString::from("prompt"),
            OsString::from("resume"),
            OsString::from("thread-123"),
            OsString::from("--yolo"),
        ];
        assert_eq!(
            parse_resume_thread_id(&args),
            Some("thread-123".to_string())
        );
    }

    #[test]
    fn parse_resume_thread_id_rejects_flag_as_id() {
        let args = vec![OsString::from("resume"), OsString::from("--last")];
        assert_eq!(parse_resume_thread_id(&args), None);
    }

    #[test]
    fn wait_for_thread_id_reads_first_thread_started() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let events = dir.path().join("run.events.jsonl");
        std::fs::write(&events, "")?;

        let events_for_writer = events.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            let ev = ThreadEvent::ThreadStarted(ThreadStartedEvent {
                thread_id: "thread-abc".to_string(),
            });
            let line = serde_json::to_string(&ev).expect("serialize thread.started");
            let mut f = OpenOptions::new()
                .append(true)
                .open(&events_for_writer)
                .expect("open events");
            writeln!(f, "{line}").expect("write line");
            f.flush().expect("flush");
        });

        let got = wait_for_thread_id(
            &events,
            Duration::from_millis(500),
            Duration::from_millis(10),
        )?;
        assert_eq!(got, "thread-abc");
        Ok(())
    }
}
