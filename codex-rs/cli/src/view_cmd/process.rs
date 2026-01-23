use anyhow::Context;
use clap::Parser;
use clap::ValueEnum;
use codex_exec::Cli as ExecCli;
use std::ffi::OsString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use crate::exec_view_meta::ExecViewMetaV1;

pub(super) fn terminate_process(pid: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let pid_i32 = i32::try_from(pid).unwrap_or(i32::MAX);
        // The child uses `setsid()`, so it is a session leader and its PID is also its process group id.
        // Kill the entire group so we don't leave tool subprocesses around.
        let rc = unsafe { libc::kill(-pid_i32, libc::SIGTERM) };
        if rc == -1 {
            let err = std::io::Error::last_os_error();
            // ESRCH: already exited; treat as success.
            if err.raw_os_error() != Some(libc::ESRCH) {
                return Err(err).context(format!("kill process group {pid}"));
            }
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        Ok(())
    }
}

pub(super) fn process_is_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let pid_i32 = i32::try_from(pid).unwrap_or(i32::MAX);
        let mut status: i32 = 0;
        let rc = unsafe { libc::waitpid(pid_i32, &mut status as *mut i32, libc::WNOHANG) };
        if rc == 0 {
            return true;
        }
        if rc == pid_i32 {
            return false;
        }
        if rc == -1 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ECHILD) {
                // If we're not the parent process, fall back to a basic liveness check.
                let rc = unsafe { libc::kill(pid_i32, 0) };
                if rc == 0 {
                    return true;
                }
                let err = std::io::Error::last_os_error();
                return err.raw_os_error() != Some(libc::ESRCH);
            }
            return err.raw_os_error() != Some(libc::ESRCH);
        }

        true
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn open_append_file_create(path: &Path) -> anyhow::Result<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(Into::into)
}

pub(super) fn touch_events_file(events_file: &Path) -> anyhow::Result<()> {
    let mut file = open_append_file_create(events_file)
        .with_context(|| format!("open {}", events_file.display()))?;
    writeln!(&mut file)?;
    let _ = file.flush();
    Ok(())
}

pub(super) fn spawn_follow_up_exec(
    events_file: &Path,
    meta: &ExecViewMetaV1,
    thread_id: &str,
    prompt: &str,
) -> anyhow::Result<u32> {
    let exec_args = build_follow_up_exec_args(meta, thread_id, prompt)?;

    let stdout_file = open_append_file_create(events_file)
        .with_context(|| format!("open {}", events_file.display()))?;
    let stderr_file = open_append_file_create(&meta.stderr_file)
        .with_context(|| format!("open {}", meta.stderr_file.display()))?;

    let exe = std::env::current_exe().context("resolve current executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("exec");

    for raw in &meta.root_overrides.raw_overrides {
        cmd.arg("-c").arg(raw);
    }
    if let Some(home) = &meta.root_overrides.config_home {
        cmd.arg("--config-home").arg(home);
    }
    if let Some(file) = &meta.root_overrides.config_file {
        cmd.arg("--config-file").arg(file);
    }

    cmd.args(exec_args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(stdout_file));
    cmd.stderr(Stdio::from(stderr_file));

    crate::exec_view_cmd::detach_child_process(&mut cmd);
    let child = cmd
        .spawn()
        .context("spawn detached `codex exec` follow-up")?;
    let pid = child.id();
    drop(child);
    Ok(pid)
}

pub(super) fn spawn_new_exec(
    events_file: &Path,
    meta: &ExecViewMetaV1,
    prompt: &str,
) -> anyhow::Result<u32> {
    let exec_args = build_new_exec_args(meta, prompt)?;

    let stdout_file = open_append_file_create(events_file)
        .with_context(|| format!("open {}", events_file.display()))?;
    let stderr_file = open_append_file_create(&meta.stderr_file)
        .with_context(|| format!("open {}", meta.stderr_file.display()))?;

    let exe = std::env::current_exe().context("resolve current executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("exec");

    for raw in &meta.root_overrides.raw_overrides {
        cmd.arg("-c").arg(raw);
    }
    if let Some(home) = &meta.root_overrides.config_home {
        cmd.arg("--config-home").arg(home);
    }
    if let Some(file) = &meta.root_overrides.config_file {
        cmd.arg("--config-file").arg(file);
    }

    cmd.args(exec_args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(stdout_file));
    cmd.stderr(Stdio::from(stderr_file));

    crate::exec_view_cmd::detach_child_process(&mut cmd);
    let child = cmd.spawn().context("spawn detached `codex exec`")?;
    let pid = child.id();
    drop(child);
    Ok(pid)
}

pub(super) fn build_follow_up_exec_args(
    meta: &ExecViewMetaV1,
    thread_id: &str,
    prompt: &str,
) -> anyhow::Result<Vec<OsString>> {
    let mut argv: Vec<OsString> = Vec::with_capacity(meta.exec_args.len() + 1);
    argv.push(OsString::from("codex exec"));
    argv.extend(meta.exec_args.iter().map(OsString::from));

    let parsed = ExecCli::try_parse_from(argv).context("parse stored `codex exec` arguments")?;

    let mut args: Vec<OsString> = Vec::new();
    args.push("--json".into());
    args.push("--skip-git-repo-check".into());

    if let Some(model) = parsed.model {
        args.push("--model".into());
        args.push(model.into());
    }
    if parsed.oss {
        args.push("--oss".into());
    }
    if let Some(provider) = parsed.oss_provider {
        args.push("--local-provider".into());
        args.push(provider.into());
    }
    if let Some(mode) = parsed.sandbox_mode
        && let Some(value) = mode.to_possible_value()
    {
        args.push("--sandbox".into());
        args.push(value.get_name().into());
    }
    if let Some(profile) = parsed.config_profile {
        args.push("--profile".into());
        args.push(profile.into());
    }
    if parsed.full_auto {
        args.push("--full-auto".into());
    }
    if parsed.dangerously_bypass_approvals_and_sandbox {
        args.push("--yolo".into());
    }
    if let Some(cwd) = parsed.cwd {
        args.push("--cd".into());
        args.push(cwd.into_os_string());
    }
    for dir in parsed.add_dir {
        args.push("--add-dir".into());
        args.push(dir.into_os_string());
    }
    if let Some(schema) = parsed.output_schema {
        args.push("--output-schema".into());
        args.push(schema.into_os_string());
    }
    if let Some(last_message_file) = parsed.last_message_file {
        args.push("--output-last-message".into());
        args.push(last_message_file.into_os_string());
    }

    args.push(prompt.into());
    args.push("resume".into());
    args.push(thread_id.into());

    Ok(args)
}

pub(super) fn build_new_exec_args(
    meta: &ExecViewMetaV1,
    prompt: &str,
) -> anyhow::Result<Vec<OsString>> {
    let mut argv: Vec<OsString> = Vec::with_capacity(meta.exec_args.len() + 1);
    argv.push(OsString::from("codex exec"));
    argv.extend(meta.exec_args.iter().map(OsString::from));

    let parsed = ExecCli::try_parse_from(argv).context("parse stored `codex exec` arguments")?;

    let mut args: Vec<OsString> = Vec::new();
    args.push("--json".into());
    args.push("--skip-git-repo-check".into());

    if let Some(model) = parsed.model {
        args.push("--model".into());
        args.push(model.into());
    }
    if parsed.oss {
        args.push("--oss".into());
    }
    if let Some(provider) = parsed.oss_provider {
        args.push("--local-provider".into());
        args.push(provider.into());
    }
    if let Some(mode) = parsed.sandbox_mode
        && let Some(value) = mode.to_possible_value()
    {
        args.push("--sandbox".into());
        args.push(value.get_name().into());
    }
    if let Some(profile) = parsed.config_profile {
        args.push("--profile".into());
        args.push(profile.into());
    }
    if parsed.full_auto {
        args.push("--full-auto".into());
    }
    if parsed.dangerously_bypass_approvals_and_sandbox {
        args.push("--yolo".into());
    }
    if let Some(cwd) = parsed.cwd {
        args.push("--cd".into());
        args.push(cwd.into_os_string());
    }
    for dir in parsed.add_dir {
        args.push("--add-dir".into());
        args.push(dir.into_os_string());
    }
    if let Some(schema) = parsed.output_schema {
        args.push("--output-schema".into());
        args.push(schema.into_os_string());
    }
    if let Some(last_message_file) = parsed.last_message_file {
        args.push("--output-last-message".into());
        args.push(last_message_file.into_os_string());
    }

    args.push(prompt.into());
    Ok(args)
}
