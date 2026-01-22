use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecViewMetaV1 {
    pub schema_version: u32,
    pub created_at_unix_s: u64,
    pub root_overrides: RootOverrides,
    pub exec_args: Vec<String>,
    #[serde(default)]
    pub current_prompt: Option<String>,
    #[serde(default)]
    pub queued_user_prompts: Vec<String>,
    pub stderr_file: PathBuf,
    pub process_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RootOverrides {
    pub raw_overrides: Vec<String>,
    pub config_home: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
}

impl ExecViewMetaV1 {
    pub fn new(
        root_overrides: RootOverrides,
        exec_args: Vec<String>,
        stderr_file: PathBuf,
    ) -> Self {
        Self {
            schema_version: 1,
            created_at_unix_s: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            root_overrides,
            exec_args,
            current_prompt: None,
            queued_user_prompts: Vec::new(),
            stderr_file,
            process_pid: None,
        }
    }

    pub fn path_for_events(events_file: &Path) -> PathBuf {
        events_file.with_extension("meta.json")
    }

    pub fn load_for_events(events_file: &Path) -> anyhow::Result<Option<Self>> {
        let path = Self::path_for_events(events_file);
        let data = match std::fs::read_to_string(&path) {
            Ok(data) => data,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).context(format!("read {}", path.display())),
        };

        let meta: Self =
            serde_json::from_str(&data).with_context(|| format!("parse {}", path.display()))?;
        if meta.schema_version != 1 {
            return Ok(None);
        }
        Ok(Some(meta))
    }

    pub fn save_for_events(&self, events_file: &Path) -> anyhow::Result<PathBuf> {
        let path = Self::path_for_events(events_file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }

        let json = serde_json::to_string_pretty(self).context("serialize exec-view meta")?;
        std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::ExecViewMetaV1;
    use super::RootOverrides;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[test]
    fn meta_roundtrips() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let events_file = dir.path().join("run.events.jsonl");
        std::fs::write(&events_file, "")?;

        let mut meta = ExecViewMetaV1::new(
            RootOverrides {
                raw_overrides: vec!["model=\"o3\"".to_string()],
                config_home: Some(dir.path().join("codex_home")),
                config_file: None,
            },
            vec!["--json".to_string(), "--skip-git-repo-check".to_string()],
            dir.path().join("stderr.log"),
        );
        meta.process_pid = Some(123);
        let meta_path = meta.save_for_events(&events_file)?;
        assert!(meta_path.exists());

        let loaded = ExecViewMetaV1::load_for_events(&events_file)?.expect("meta file present");
        assert_eq!(loaded.schema_version, 1);
        assert_eq!(loaded.root_overrides.raw_overrides, vec!["model=\"o3\""]);
        assert_eq!(loaded.exec_args, vec!["--json", "--skip-git-repo-check"]);
        assert_eq!(loaded.stderr_file, dir.path().join("stderr.log"));
        assert_eq!(loaded.process_pid, Some(123));
        assert_eq!(loaded.queued_user_prompts, Vec::<String>::new());
        Ok(())
    }
}
