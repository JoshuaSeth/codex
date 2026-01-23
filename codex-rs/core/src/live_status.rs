use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use chrono::SecondsFormat;
use chrono::Utc;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveFrontend {
    Exec,
    Tui,
    Tui2,
    AppServer,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveSessionStatus {
    Running,
    WaitingPendingTool,
    WaitingUserInput,
    Completed,
    Errored,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct LiveStatusDetail {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveIpc {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveStatusRecordV1 {
    pub schema_version: u32,
    pub thread_id: String,
    pub instance_id: String,
    pub frontend: LiveFrontend,

    pub status: LiveSessionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<LiveStatusDetail>,

    pub alive: bool,

    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ppid: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,

    pub started_at: String,
    pub last_heartbeat_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipc: Option<LiveIpc>,

    // Backward compat: exec used to write `{host,port}` only. Keeping these fields means older
    // `codex exec deliver-pending` binaries can still deserialize successfully.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl LiveStatusRecordV1 {
    pub fn path_for(codex_home: &Path, thread_id: &ThreadId) -> PathBuf {
        codex_home.join("live").join(format!("{thread_id}.json"))
    }
}

pub struct LiveStatusWriterConfig {
    pub codex_home: PathBuf,
    pub thread_id: ThreadId,
    pub frontend: LiveFrontend,
    pub status: LiveSessionStatus,
    pub detail: Option<LiveStatusDetail>,
    pub cwd: Option<PathBuf>,
    pub cli_version: Option<String>,
    pub heartbeat_interval: Option<Duration>,
}

#[derive(Debug)]
enum LiveStatusCommand {
    SetStatus {
        status: LiveSessionStatus,
        detail: Option<LiveStatusDetail>,
    },
    SetIpc(LiveIpc),
    Shutdown {
        status: LiveSessionStatus,
        note: Option<String>,
    },
}

pub struct LiveStatusWriter {
    tx: mpsc::UnboundedSender<LiveStatusCommand>,
    task: JoinHandle<()>,
}

impl LiveStatusWriter {
    pub fn spawn(cfg: LiveStatusWriterConfig) -> anyhow::Result<Self> {
        let LiveStatusWriterConfig {
            codex_home,
            thread_id,
            frontend,
            status,
            detail,
            cwd,
            cli_version,
            heartbeat_interval,
        } = cfg;

        let path = LiveStatusRecordV1::path_for(&codex_home, &thread_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let now = now_rfc3339_utc_millis();
        let mut record = LiveStatusRecordV1 {
            schema_version: 1,
            thread_id: thread_id.to_string(),
            instance_id: Uuid::new_v4().to_string(),
            frontend,
            status,
            detail,
            alive: true,
            pid: std::process::id(),
            ppid: process_ppid(),
            hostname: hostname(),
            device_id: device_id(),
            tty: tty_name(),
            cwd: cwd.map(|p| p.to_string_lossy().into_owned()),
            cli_version,
            started_at: now.clone(),
            last_heartbeat_at: now,
            ended_at: None,
            ipc: None,
            host: None,
            port: None,
        };

        // Write initial record synchronously so callers can depend on existence immediately.
        write_json_atomic_sync(&path, &record)?;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let heartbeat_interval = heartbeat_interval.unwrap_or(DEFAULT_HEARTBEAT_INTERVAL);

        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(heartbeat_interval);
            // First tick should not be delayed by interval's initial behavior.
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    cmd = rx.recv() => {
                        let Some(cmd) = cmd else {
                            break;
                        };
                        match cmd {
                            LiveStatusCommand::SetStatus { status, detail } => {
                                record.status = status;
                                record.detail = detail;
                                record.last_heartbeat_at = now_rfc3339_utc_millis();
                                if let Err(err) = write_json_atomic(&path, &record).await {
                                    tracing::warn!("failed writing live status {}: {err}", path.display());
                                }
                            }
                            LiveStatusCommand::SetIpc(ipc) => {
                                record.host = Some(ipc.host.clone());
                                record.port = Some(ipc.port);
                                record.ipc = Some(ipc);
                                record.last_heartbeat_at = now_rfc3339_utc_millis();
                                if let Err(err) = write_json_atomic(&path, &record).await {
                                    tracing::warn!("failed writing live status {}: {err}", path.display());
                                }
                            }
                            LiveStatusCommand::Shutdown { status, note } => {
                                record.status = status;
                                record.detail = note.map(|n| LiveStatusDetail {
                                    note: Some(n),
                                    ..Default::default()
                                });
                                record.alive = false;
                                let now = now_rfc3339_utc_millis();
                                record.last_heartbeat_at = now.clone();
                                record.ended_at = Some(now);
                                if let Err(err) = write_json_atomic(&path, &record).await {
                                    tracing::warn!("failed writing live status {}: {err}", path.display());
                                }
                                break;
                            }
                        }
                    }
                    _ = interval.tick() => {
                        record.last_heartbeat_at = now_rfc3339_utc_millis();
                        if let Err(err) = write_json_atomic(&path, &record).await {
                            tracing::warn!("failed writing live status {}: {err}", path.display());
                        }
                    }
                }
            }
        });

        Ok(Self { tx, task })
    }

    pub fn set_status(&self, status: LiveSessionStatus, detail: Option<LiveStatusDetail>) {
        let _ = self
            .tx
            .send(LiveStatusCommand::SetStatus { status, detail });
    }

    pub fn set_ipc(&self, ipc: LiveIpc) {
        let _ = self.tx.send(LiveStatusCommand::SetIpc(ipc));
    }

    pub async fn shutdown(self, status: LiveSessionStatus, note: Option<String>) {
        let _ = self.tx.send(LiveStatusCommand::Shutdown { status, note });
        let _ = self.task.await;
    }
}

fn now_rfc3339_utc_millis() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn device_id() -> Option<String> {
    for key in ["CODEX_LIVE_DEVICE_ID", "PITCHAI_DEVICE_ID"] {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn hostname() -> Option<String> {
    for key in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    #[cfg(unix)]
    {
        use std::ffi::CStr;

        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let cstr = unsafe { CStr::from_ptr(buf.as_ptr().cast()) };
            if let Ok(s) = cstr.to_str() {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }

    None
}

fn process_ppid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(unsafe { libc::getppid() } as u32)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn tty_name() -> Option<String> {
    #[cfg(unix)]
    {
        use std::ffi::CStr;

        let mut buf = [0u8; 256];
        let rc = unsafe { libc::ttyname_r(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            return None;
        }
        let cstr = unsafe { CStr::from_ptr(buf.as_ptr().cast()) };
        cstr.to_str().ok().map(std::string::ToString::to_string)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("live.json");
    let tmp_name = format!(".{name}.tmp.{}", Uuid::new_v4());
    path.with_file_name(tmp_name)
}

async fn write_json_atomic(path: &Path, record: &LiveStatusRecordV1) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    let tmp = tmp_path_for(path);
    tokio::fs::write(&tmp, bytes).await?;

    if tokio::fs::rename(&tmp, path).await.is_ok() {
        return Ok(());
    }

    // Windows doesn't allow renaming over an existing file. Best-effort: remove then rename.
    let _ = tokio::fs::remove_file(path).await;
    tokio::fs::rename(&tmp, path).await
}

fn write_json_atomic_sync(path: &Path, record: &LiveStatusRecordV1) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    let tmp = tmp_path_for(path);
    std::fs::write(&tmp, bytes)?;
    if std::fs::rename(&tmp, path).is_ok() {
        return Ok(());
    }
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)
}
