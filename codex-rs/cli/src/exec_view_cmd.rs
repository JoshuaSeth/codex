use anyhow::Context;
use clap::Parser;
use codex_core::config::find_codex_home;
use codex_exec::Cli as ExecCli;
use codex_exec::exec_events::AgentMessageItem;
use codex_exec::exec_events::ItemCompletedEvent;
use codex_exec::exec_events::ReasoningItem;
use codex_exec::exec_events::ThreadEvent;
use codex_exec::exec_events::ThreadItem;
use codex_exec::exec_events::ThreadItemDetails;
use codex_exec::exec_events::ThreadStartedEvent;
use codex_utils_cli::CliConfigOverrides;
use serde_json::Value;
use std::ffi::OsString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crate::exec_view_meta::ExecViewMetaV1;
use crate::exec_view_meta::RootOverrides;

/// Run `codex exec --json` detached, write events to a file, then attach a read-only viewer.
#[derive(Debug, Parser)]
pub struct ExecViewCli {
    /// Write JSONL events to this file (created if missing).
    #[arg(long = "events-file", value_name = "FILE")]
    pub events_file: Option<PathBuf>,

    /// Write stderr logs to this file (created if missing).
    #[arg(long = "stderr-file", value_name = "FILE")]
    pub stderr_file: Option<PathBuf>,

    /// Overwrite existing output files.
    #[arg(long = "overwrite", default_value_t = false)]
    pub overwrite: bool,

    /// Don't start the exec process; only create the events/meta files and launch the viewer.
    ///
    /// Useful for a viewer-first workflow where prompts are entered inside `codex view`.
    #[arg(long = "no-exec", default_value_t = false)]
    pub no_exec: bool,

    /// Don't launch the viewer; only start the detached exec process.
    #[arg(long = "no-view", default_value_t = false)]
    pub no_view: bool,

    /// When launching the viewer, start at the end of the file.
    #[arg(long, default_value_t = false)]
    pub tail: bool,

    /// Poll interval passed to the viewer when following.
    #[arg(long, default_value_t = 200)]
    pub poll_ms: u64,

    /// How long to wait for the thread id when it isn't known upfront (new session).
    #[arg(long = "wait-ms", default_value_t = 5_000)]
    pub wait_ms: u64,

    /// Poll interval when waiting for the thread.started event to appear in the file.
    #[arg(long = "wait-poll-ms", default_value_t = 50)]
    pub wait_poll_ms: u64,

    /// Disable mouse interactions in the viewer.
    #[arg(long = "no-mouse", default_value_t = false)]
    pub no_mouse: bool,

    /// Arguments forwarded to `codex exec` (same syntax as `codex exec`).
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "EXEC_ARGS"
    )]
    pub exec_args: Vec<OsString>,
}

pub fn run_exec_view(cli: ExecViewCli, root_overrides: &CliConfigOverrides) -> anyhow::Result<()> {
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

    if let Some(thread_id) = parse_resume_thread_id(&cli.exec_args) {
        seed_events_file_from_rollout(&events_path, root_overrides, &thread_id).with_context(
            || {
                format!(
                    "seed events file {} for resume {thread_id}",
                    events_path.display()
                )
            },
        )?;
    }

    if cli.no_exec {
        let mut meta_exec_args: Vec<String> = Vec::new();
        let json_missing = !cli
            .exec_args
            .iter()
            .any(|a| a == "--json" || a == "--experimental-json");
        if json_missing {
            meta_exec_args.push("--json".to_string());
        }
        let mut exec_args = cli.exec_args.clone();
        if !exec_args.iter().any(|a| a == "--skip-git-repo-check") {
            exec_args.insert(0, "--skip-git-repo-check".into());
        }
        meta_exec_args.extend(exec_args.iter().map(|a| a.to_string_lossy().into_owned()));

        let mut meta = ExecViewMetaV1::new(
            RootOverrides {
                raw_overrides: root_overrides.raw_overrides.clone(),
                config_home: None,
                config_file: None,
            },
            meta_exec_args,
            stderr_path,
        );
        meta.current_prompt = extract_effective_prompt(&meta.exec_args);
        meta.process_pid = None;
        meta.save_for_events(&events_path)?;

        if cli.no_view {
            return Ok(());
        }
        return crate::view_cmd::run_view(
            crate::view_cmd::ViewCli {
                file: Some(events_path),
                init: true,
                resume: None,
                follow: true,
                tail: cli.tail,
                poll_ms: cli.poll_ms,
                process_pid: None,
                no_mouse: cli.no_mouse,
            },
            root_overrides,
        );
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

    detach_child_process(&mut cmd);

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
    meta.current_prompt = extract_effective_prompt(&meta.exec_args);
    meta.process_pid = Some(pid);
    meta.save_for_events(&events_path)?;

    if let Some(thread_id) = parse_resume_thread_id(&cli.exec_args).or_else(|| {
        wait_for_thread_id(
            &events_path,
            Duration::from_millis(cli.wait_ms),
            Duration::from_millis(cli.wait_poll_ms),
        )
        .ok()
    }) {
        let _ = write_thread_events_pointer(root_overrides, &thread_id, &events_path);
    }

    eprintln!(
        "Started `codex exec` (pid={pid:?}); events: {}; stderr: {}",
        events_path.display(),
        stderr_path.display()
    );
    let inv = invocation_name();
    eprintln!(
        "Attach later with: {inv} view {}",
        shell_quote(events_path.as_os_str().to_string_lossy().as_ref())
    );

    if cli.no_view {
        return Ok(());
    }

    crate::view_cmd::run_view(
        crate::view_cmd::ViewCli {
            file: Some(events_path),
            init: true,
            resume: None,
            follow: true,
            tail: cli.tail,
            poll_ms: cli.poll_ms,
            process_pid: Some(pid),
            no_mouse: cli.no_mouse,
        },
        root_overrides,
    )
}

pub(crate) fn wait_for_thread_id(
    events_path: &Path,
    timeout: Duration,
    poll: Duration,
) -> anyhow::Result<String> {
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            anyhow::bail!(
                "timed out waiting for thread.started in {}",
                events_path.display()
            );
        }

        let file = OpenOptions::new()
            .read(true)
            .open(events_path)
            .with_context(|| format!("open {}", events_path.display()))?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Ok(ThreadEvent::ThreadStarted(ThreadStartedEvent { thread_id })) =
                serde_json::from_value::<ThreadEvent>(value)
            {
                return Ok(thread_id);
            }
        }

        std::thread::sleep(poll);
    }
}

pub(crate) fn write_thread_events_pointer(
    root_overrides: &CliConfigOverrides,
    thread_id: &str,
    events_file: &Path,
) -> anyhow::Result<PathBuf> {
    let codex_home = codex_home_from_root_overrides(root_overrides);
    let dir = codex_home.join("live");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{thread_id}.events.jsonl.path"));
    std::fs::write(&path, format!("{}\n", events_file.display()))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

pub(crate) fn codex_home_from_root_overrides(_root_overrides: &CliConfigOverrides) -> PathBuf {
    find_codex_home().unwrap_or_else(|_| default_codex_home())
}

pub(crate) fn extract_effective_prompt(exec_args: &[String]) -> Option<String> {
    let mut argv: Vec<OsString> = Vec::with_capacity(exec_args.len() + 1);
    argv.push(OsString::from("codex exec"));
    argv.extend(exec_args.iter().map(OsString::from));

    let parsed = ExecCli::try_parse_from(argv).ok()?;
    match parsed.command {
        Some(codex_exec::Command::Resume(args)) => {
            if args.no_prompt {
                return None;
            }
            args.prompt.or(parsed.prompt)
        }
        _ => parsed.prompt,
    }
}

fn default_events_path(root_overrides: &CliConfigOverrides) -> anyhow::Result<PathBuf> {
    // If a custom config-home is specified, keep the events file alongside it so it’s easy to find.
    let codex_home = codex_home_from_root_overrides(root_overrides);

    let dir = codex_home.join("live").join("exec-view");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let pid = std::process::id();
    Ok(dir.join(format!("exec-view-{now}-{pid}.events.jsonl")))
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

pub(crate) fn seed_events_file_from_rollout(
    events_path: &Path,
    root_overrides: &CliConfigOverrides,
    thread_id: &str,
) -> anyhow::Result<()> {
    let Some(rollout_path) = find_rollout_file_by_thread_id(root_overrides, thread_id)? else {
        return Ok(());
    };

    let mut out = OpenOptions::new()
        .append(true)
        .open(events_path)
        .with_context(|| format!("open {}", events_path.display()))?;

    let started = ThreadEvent::ThreadStarted(ThreadStartedEvent {
        thread_id: thread_id.to_string(),
    });
    writeln!(out, "{}", serde_json::to_string(&started)?)?;

    let file =
        File::open(&rollout_path).with_context(|| format!("open {}", rollout_path.display()))?;
    let mut reader = BufReader::new(file);
    let mut buf = String::new();

    let mut next_id: u64 = 0;
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    while reader.read_line(&mut buf)? != 0 {
        let line = buf.trim().to_string();
        buf.clear();
        if line.is_empty() {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(line_type) = value.get("type").and_then(Value::as_str) else {
            continue;
        };

        // Prefer persisted event messages for a clean "step view" (this mirrors what the TUI
        // renders). If those aren't present for some reason, response_item can be added later.
        if line_type != "event_msg" {
            continue;
        }

        let Some(payload) = value.get("payload") else {
            continue;
        };
        let Some(payload_type) = payload.get("type").and_then(Value::as_str) else {
            continue;
        };

        let (kind, text) = match payload_type {
            "agent_message" => payload
                .get("message")
                .and_then(Value::as_str)
                .map(|t| ("agent_message", t.to_string())),
            "agent_reasoning" => payload
                .get("text")
                .and_then(Value::as_str)
                .map(|t| ("reasoning", t.to_string())),
            _ => None,
        }
        .unwrap_or_else(|| ("", String::new()));

        if kind.is_empty() || text.trim().is_empty() {
            continue;
        }

        if !seen.insert((kind.to_string(), text.clone())) {
            continue;
        }

        let item_id = format!("history_{next_id}");
        next_id += 1;

        let details = match kind {
            "agent_message" => ThreadItemDetails::AgentMessage(AgentMessageItem { text }),
            "reasoning" => ThreadItemDetails::Reasoning(ReasoningItem { text }),
            _ => continue,
        };

        let item = ThreadItem {
            id: item_id,
            details,
        };
        let ev = ThreadEvent::ItemCompleted(ItemCompletedEvent { item });
        writeln!(out, "{}", serde_json::to_string(&ev)?)?;
    }

    out.flush()?;
    Ok(())
}

fn find_rollout_file_by_thread_id(
    _root_overrides: &CliConfigOverrides,
    thread_id: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let codex_home = find_codex_home().unwrap_or_else(|_| default_codex_home());
    let sessions_dir = codex_home.join("sessions");
    let mut matches: Vec<(SystemTime, PathBuf)> = Vec::new();
    find_rollout_files_recursive(&sessions_dir, thread_id, &mut matches)?;
    matches.sort_by(|(a, _), (b, _)| b.cmp(a));
    Ok(matches.into_iter().next().map(|(_, p)| p))
}

fn find_rollout_files_recursive(
    dir: &Path,
    thread_id: &str,
    out: &mut Vec<(SystemTime, PathBuf)>,
) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context(format!("read {}", dir.display())),
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if ft.is_dir() {
            find_rollout_files_recursive(&path, thread_id, out)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }

        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("rollout-") || !name.ends_with(".jsonl") || !name.contains(thread_id) {
            continue;
        }

        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        out.push((modified, path));
    }
    Ok(())
}

pub(crate) fn detach_child_process(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
}

pub(crate) fn invocation_name() -> String {
    let Some(arg0) = std::env::args_os().next() else {
        return "codex".to_string();
    };
    let arg0 = arg0.to_string_lossy();
    let path = PathBuf::from(arg0.as_ref());
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "codex".to_string())
}

pub(crate) fn shell_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    if arg
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"@%_+=:,./-".contains(&b))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::extract_effective_prompt;
    use super::parse_resume_thread_id;
    use pretty_assertions::assert_eq;
    use std::ffi::OsString;

    #[test]
    fn parse_resume_thread_id_finds_resume_anywhere() {
        let args = vec![
            OsString::from("what did I ask you?"),
            OsString::from("resume"),
            OsString::from("019bd0f6-ade4-7862-b457-fbbfd47180d0"),
            OsString::from("--yolo"),
        ];
        assert_eq!(
            parse_resume_thread_id(&args),
            Some("019bd0f6-ade4-7862-b457-fbbfd47180d0".to_string())
        );
    }

    #[test]
    fn parse_resume_thread_id_rejects_flag_as_id() {
        let args = vec![OsString::from("resume"), OsString::from("--yolo")];
        assert_eq!(parse_resume_thread_id(&args), None);
    }

    #[test]
    fn extract_effective_prompt_prefers_resume_prompt() {
        let exec_args = vec![
            "--json".to_string(),
            "--skip-git-repo-check".to_string(),
            "root prompt".to_string(),
            "resume".to_string(),
            "thread-123".to_string(),
            "resume prompt".to_string(),
        ];
        assert_eq!(
            extract_effective_prompt(&exec_args),
            Some("resume prompt".to_string())
        );
    }

    #[test]
    fn extract_effective_prompt_uses_root_prompt_for_new_session() {
        let exec_args = vec![
            "--json".to_string(),
            "--skip-git-repo-check".to_string(),
            "hello".to_string(),
        ];
        assert_eq!(
            extract_effective_prompt(&exec_args),
            Some("hello".to_string())
        );
    }

    #[test]
    fn extract_effective_prompt_respects_resume_no_prompt() {
        let exec_args = vec![
            "--json".to_string(),
            "--skip-git-repo-check".to_string(),
            "resume".to_string(),
            "thread-123".to_string(),
            "--no-prompt".to_string(),
        ];
        assert_eq!(extract_effective_prompt(&exec_args), None);
    }
}
