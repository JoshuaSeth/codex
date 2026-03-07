use anyhow::Context;
use clap::Parser;
use codex_core::config::find_codex_home;
use codex_core::live_status::LiveStatusRecordV1;
use codex_protocol::ThreadId;
use codex_utils_cli::CliConfigOverrides;
use serde::Deserialize;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 2);

/// Block until any of the given conversations finishes.
///
/// This polls `$CODEX_HOME/live/<thread_id>.json` (written by `codex exec` / `codex exec-view` /
/// `codex exec-async`).
#[derive(Debug, Parser)]
pub struct AwaitAnyCli {
    /// One or more conversation/thread ids.
    #[arg(value_name = "THREAD_ID", required = true)]
    pub thread_ids: Vec<String>,

    /// Poll interval while waiting.
    #[arg(long, default_value_t = 200)]
    pub poll_ms: u64,

    /// Timeout in seconds; set to 0 to wait indefinitely.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT.as_secs())]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AwaitAnyResult {
    finished: String,
    still_running: Vec<String>,
}

pub fn run_await_any(cli: AwaitAnyCli, _root_overrides: &CliConfigOverrides) -> anyhow::Result<()> {
    let codex_home = find_codex_home().unwrap_or_else(|_| default_codex_home());

    let targets = cli
        .thread_ids
        .iter()
        .map(|raw| {
            let id =
                ThreadId::from_string(raw).with_context(|| format!("invalid thread id: {raw}"))?;
            Ok((raw.clone(), LiveStatusRecordV1::path_for(&codex_home, &id)))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    for (id, path) in &targets {
        if !path.exists() {
            anyhow::bail!(
                "live status file not found for {id}: {} (codex_home={})",
                path.display(),
                codex_home.display()
            );
        }
    }

    let poll = Duration::from_millis(cli.poll_ms);
    let timeout = if cli.timeout_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(cli.timeout_secs))
    };

    match await_any_finished(&targets, poll, timeout)? {
        AwaitAnyOutcome::Finished(result) => {
            let still_running = if result.still_running.is_empty() {
                "(none)".to_string()
            } else {
                result.still_running.join(" ")
            };
            println!(
                "\"{}\" finished, \"{}\" still running.",
                result.finished, still_running
            );
        }
        AwaitAnyOutcome::TimedOut(still_running) => {
            let still_running = if still_running.is_empty() {
                "(none)".to_string()
            } else {
                still_running.join(" ")
            };
            println!(
                "still working busy, see if you can prepare anything in the meantime and then await-any the colleagues again"
            );
            println!("\"{still_running}\" still running.");
            std::process::exit(2);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AwaitAnyOutcome {
    Finished(AwaitAnyResult),
    TimedOut(Vec<String>),
}

fn await_any_finished(
    targets: &[(String, PathBuf)],
    poll: Duration,
    timeout: Option<Duration>,
) -> anyhow::Result<AwaitAnyOutcome> {
    let start = Instant::now();
    loop {
        let snapshot = poll_live_snapshot(targets)?;
        if let Some(finished) = snapshot.finished {
            return Ok(AwaitAnyOutcome::Finished(AwaitAnyResult {
                finished,
                still_running: snapshot.still_running,
            }));
        }

        if let Some(timeout) = timeout
            && start.elapsed() > timeout
        {
            return Ok(AwaitAnyOutcome::TimedOut(snapshot.still_running));
        }

        std::thread::sleep(poll);
    }
}

#[derive(Debug)]
struct LiveSnapshot {
    finished: Option<String>,
    still_running: Vec<String>,
}

fn poll_live_snapshot(targets: &[(String, PathBuf)]) -> anyhow::Result<LiveSnapshot> {
    let mut finished: Option<String> = None;
    let mut still_running: Vec<String> = Vec::new();

    for (id, path) in targets {
        let record = read_live_status(path).with_context(|| format!("read {}", path.display()))?;
        if is_finished(&record) {
            if finished.is_none() {
                finished = Some(id.clone());
            }
        } else {
            still_running.push(id.clone());
        }
    }

    Ok(LiveSnapshot {
        finished,
        still_running,
    })
}

#[derive(Debug, Deserialize)]
struct LiveStatusSnapshot {
    #[serde(default)]
    alive: Option<bool>,
    #[serde(default)]
    status: Option<String>,
}

fn read_live_status(path: &Path) -> anyhow::Result<LiveStatusSnapshot> {
    let data = std::fs::read_to_string(path)?;
    if data.trim().is_empty() {
        // The file may be observed mid-write (truncate then write). Treat empty as transient.
        return Ok(LiveStatusSnapshot {
            alive: None,
            status: None,
        });
    }
    match serde_json::from_str(&data) {
        Ok(parsed) => Ok(parsed),
        Err(_err) => {
            // The file may be observed mid-write (truncate then write). Treat parse errors as transient.
            Ok(LiveStatusSnapshot {
                alive: None,
                status: None,
            })
        }
    }
}

fn is_finished(record: &LiveStatusSnapshot) -> bool {
    if record.alive == Some(false) {
        return true;
    }
    matches!(
        record.status.as_deref(),
        Some("completed") | Some("errored")
    )
}

fn default_codex_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

#[cfg(test)]
mod tests {
    use super::AwaitAnyOutcome;
    use super::await_any_finished;
    use super::default_codex_home;
    use codex_core::live_status::LiveFrontend;
    use codex_core::live_status::LiveSessionStatus;
    use codex_core::live_status::LiveStatusRecordV1;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::tempdir;

    fn write_record(path: &PathBuf, record: &LiveStatusRecordV1) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut bytes = serde_json::to_vec_pretty(record)?;
        bytes.push(b'\n');
        std::fs::write(path, bytes)?;
        Ok(())
    }

    fn mk_record(thread_id: &str, alive: bool, status: LiveSessionStatus) -> LiveStatusRecordV1 {
        LiveStatusRecordV1 {
            schema_version: 1,
            thread_id: thread_id.to_string(),
            instance_id: "inst".to_string(),
            frontend: LiveFrontend::Exec,
            status,
            detail: None,
            alive,
            pid: 123,
            ppid: None,
            hostname: None,
            device_id: None,
            tty: None,
            cwd: None,
            cli_version: None,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            last_heartbeat_at: "2026-01-01T00:00:00Z".to_string(),
            ended_at: if alive {
                None
            } else {
                Some("2026-01-01T00:00:01Z".to_string())
            },
            ipc: None,
            host: None,
            port: None,
        }
    }

    #[test]
    fn default_codex_home_is_non_empty() {
        assert!(!default_codex_home().as_os_str().is_empty());
    }

    #[test]
    fn await_any_returns_finished() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let codex_home = dir.path().join("codex_home");

        let a = "019bd2b2-09f5-7dc0-a7d1-1d8e74b0d103";
        let b = "019bd2b2-09f5-7dc0-a7d1-1d8e74b0d104";
        let a_path = codex_home.join("live").join(format!("{a}.json"));
        let b_path = codex_home.join("live").join(format!("{b}.json"));

        write_record(&a_path, &mk_record(a, true, LiveSessionStatus::Running))?;
        write_record(&b_path, &mk_record(b, true, LiveSessionStatus::Running))?;

        let a_path_for_writer = a_path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            let record = mk_record(a, false, LiveSessionStatus::Completed);
            let _ = write_record(&a_path_for_writer, &record);
        });

        let result = await_any_finished(
            &[(a.to_string(), a_path), (b.to_string(), b_path)],
            Duration::from_millis(5),
            Some(Duration::from_secs(2)),
        )?;

        let AwaitAnyOutcome::Finished(result) = result else {
            anyhow::bail!("expected finished outcome");
        };
        assert_eq!(result.finished, a);
        assert_eq!(result.still_running, vec![b.to_string()]);
        Ok(())
    }

    #[test]
    fn await_any_can_time_out() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let codex_home = dir.path().join("codex_home");

        let a = "019bd2b2-09f5-7dc0-a7d1-1d8e74b0d105";
        let b = "019bd2b2-09f5-7dc0-a7d1-1d8e74b0d106";
        let a_path = codex_home.join("live").join(format!("{a}.json"));
        let b_path = codex_home.join("live").join(format!("{b}.json"));

        write_record(&a_path, &mk_record(a, true, LiveSessionStatus::Running))?;
        write_record(&b_path, &mk_record(b, true, LiveSessionStatus::Running))?;

        let result = await_any_finished(
            &[(a.to_string(), a_path), (b.to_string(), b_path)],
            Duration::from_millis(5),
            Some(Duration::from_millis(50)),
        )?;

        let AwaitAnyOutcome::TimedOut(still_running) = result else {
            anyhow::bail!("expected timed out outcome");
        };
        assert_eq!(still_running, vec![a.to_string(), b.to_string()]);
        Ok(())
    }
}
