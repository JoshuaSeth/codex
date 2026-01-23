use std::io;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;

use super::SESSIONS_SUBDIR;

fn create_fork_path(
    codex_home: &Path,
    now_local: OffsetDateTime,
    id: ThreadId,
) -> io::Result<PathBuf> {
    let mut dir = codex_home.to_path_buf();
    dir.push(SESSIONS_SUBDIR);
    dir.push(now_local.year().to_string());
    dir.push(format!("{:02}", u8::from(now_local.month())));
    dir.push(format!("{:02}", now_local.day()));
    std::fs::create_dir_all(&dir)?;

    let format: &[FormatItem] =
        format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]");
    let date_str = now_local
        .format(format)
        .map_err(|e| io::Error::other(format!("failed to format timestamp: {e}")))?;
    Ok(dir.join(format!("rollout-{date_str}-{id}.jsonl")))
}

fn format_rollout_timestamp(now_utc: OffsetDateTime) -> io::Result<String> {
    let timestamp_format: &[FormatItem] =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    now_utc
        .format(timestamp_format)
        .map_err(|e| io::Error::other(format!("failed to format timestamp: {e}")))
}

/// Fork a rollout file into a new file (new session id) so it can be resumed independently.
///
/// This preserves all original JSONL lines *as-is*, except the first SessionMeta line which is
/// rewritten with a new `meta.id` and timestamp. This avoids losing any newer/unknown item types.
pub async fn fork_rollout_file(codex_home: &Path, source_path: &Path) -> io::Result<PathBuf> {
    let contents = tokio::fs::read_to_string(source_path).await?;
    if contents.trim().is_empty() {
        return Err(io::Error::other("rollout file is empty"));
    }

    let now_local = OffsetDateTime::now_local()
        .map_err(|e| io::Error::other(format!("failed to get local time: {e}")))?;
    let now_utc = OffsetDateTime::now_utc();
    let new_id = ThreadId::default();
    let new_ts = format_rollout_timestamp(now_utc)?;
    let dest_path = create_fork_path(codex_home, now_local, new_id)?;

    let mut out = String::new();
    let mut replaced_meta = false;
    for raw in contents.lines() {
        if raw.trim().is_empty() {
            out.push_str(raw);
            out.push('\n');
            continue;
        }

        if !replaced_meta {
            let mut rollout_line: RolloutLine = serde_json::from_str(raw).map_err(|err| {
                io::Error::other(format!(
                    "failed to parse rollout head line as JSON: {err}; offending line: {raw}"
                ))
            })?;

            let RolloutItem::SessionMeta(mut session_meta_line) = rollout_line.item else {
                return Err(io::Error::other(
                    "rollout file does not start with a SessionMeta line",
                ));
            };

            session_meta_line.meta.id = new_id;
            session_meta_line.meta.timestamp = new_ts.clone();

            rollout_line.timestamp = new_ts.clone();
            rollout_line.item = RolloutItem::SessionMeta(session_meta_line);

            out.push_str(&serde_json::to_string(&rollout_line).map_err(|err| {
                io::Error::other(format!("failed to encode SessionMeta line: {err}"))
            })?);
            out.push('\n');
            replaced_meta = true;
            continue;
        }

        out.push_str(raw);
        out.push('\n');
    }

    if !replaced_meta {
        return Err(io::Error::other("missing SessionMeta line in rollout"));
    }

    tokio::fs::write(&dest_path, out).await?;
    Ok(dest_path)
}
