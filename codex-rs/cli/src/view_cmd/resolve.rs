use anyhow::Context;
use codex_common::CliConfigOverrides;
use codex_core::config::find_codex_home;
use serde_json::Value;
use std::fs::File;
use std::fs::read_dir;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

pub(super) fn resolve_events_file(
    file: Option<PathBuf>,
    root_overrides: &CliConfigOverrides,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = file {
        if path.as_os_str().is_empty() {
            anyhow::bail!("EVENTS_FILE is empty; pass a file path or run `exec-view` first.");
        }
        if path.exists() {
            return Ok(path);
        }

        let thread_id = path.to_string_lossy().trim().to_string();
        if looks_like_thread_id(&thread_id) {
            if let Some(found) = read_thread_events_pointer(root_overrides, &thread_id)? {
                return Ok(found);
            }
            if let Some(found) = find_events_file_by_thread_id(&thread_id, root_overrides)? {
                return Ok(found);
            }
            anyhow::bail!("No events file found for thread id: {thread_id}");
        }

        return Ok(path);
    }

    let dir = exec_view_dir(root_overrides)?;
    let latest = find_latest_events_file(&dir)?
        .with_context(|| format!("no events files under {}", dir.display()))?;
    Ok(latest)
}

fn looks_like_thread_id(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    // Typical UUID (including the thread_id format Codex uses).
    if s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return true;
    }
    // Also accept the common “019b…” style (still UUID-like, just not validated here).
    s.len() >= 16 && s.contains('-')
}

fn read_thread_events_pointer(
    root_overrides: &CliConfigOverrides,
    thread_id: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let codex_home = match &root_overrides.config_home {
        Some(home) => home.clone(),
        None => find_codex_home().unwrap_or_else(|_| default_codex_home()),
    };
    let pointer = codex_home
        .join("live")
        .join(format!("{thread_id}.events.jsonl.path"));
    let raw = match std::fs::read_to_string(&pointer) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context(format!("read {}", pointer.display())),
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(trimmed)))
}

fn find_events_file_by_thread_id(
    thread_id: &str,
    root_overrides: &CliConfigOverrides,
) -> anyhow::Result<Option<PathBuf>> {
    let dir = exec_view_dir(root_overrides)?;
    let entries = match read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context(format!("read {}", dir.display())),
    };

    let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.ends_with(".events.jsonl") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((modified, path));
    }
    candidates.sort_by(|(a, _), (b, _)| b.cmp(a));

    for (_, path) in candidates {
        if events_file_has_thread_id(&path, thread_id)? {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

fn events_file_has_thread_id(path: &Path, thread_id: &str) -> anyhow::Result<bool> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    for _ in 0..200 {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(event_type) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        if event_type != "thread.started" {
            continue;
        }
        let Some(found) = value.get("thread_id").and_then(Value::as_str) else {
            continue;
        };
        return Ok(found == thread_id);
    }

    Ok(false)
}

fn exec_view_dir(root_overrides: &CliConfigOverrides) -> anyhow::Result<PathBuf> {
    let codex_home = match &root_overrides.config_home {
        Some(home) => home.clone(),
        None => find_codex_home().unwrap_or_else(|_| default_codex_home()),
    };
    Ok(codex_home.join("live").join("exec-view"))
}

fn default_codex_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

fn find_latest_events_file(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let entries = match read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context(format!("read {}", dir.display())),
    };

    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.ends_with(".events.jsonl") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        match &mut newest {
            Some((t, p)) if modified > *t => {
                *t = modified;
                *p = path;
            }
            None => newest = Some((modified, path)),
            _ => {}
        }
    }

    Ok(newest.map(|(_, p)| p))
}
