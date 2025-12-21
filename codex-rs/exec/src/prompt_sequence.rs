use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct PromptSequenceRunner {
    steps: Vec<PromptSequenceStep>,
    current: usize,
    source: PathBuf,
}

#[derive(Debug, Clone)]
pub struct PromptSequenceEntry {
    pub items: Vec<UserInput>,
    pub description: String,
    pub index: usize,
    pub total: usize,
}

impl PromptSequenceRunner {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let data = fs::read_to_string(path)
            .with_context(|| format!("failed to read prompt-sequence {}", path.display()))?;
        let sequence: PromptSequenceToml = toml::from_str(&data)
            .with_context(|| format!("invalid prompt-sequence {}", path.display()))?;
        if sequence.steps.is_empty() {
            anyhow::bail!(
                "prompt-sequence {} does not define any [[steps]] entries",
                path.display()
            );
        }

        let base_dir = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let steps = sequence
            .steps
            .into_iter()
            .map(|step| PromptSequenceStep::from_toml(step, &base_dir))
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(Self {
            steps,
            current: 0,
            source: path.to_path_buf(),
        })
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn has_remaining(&self) -> bool {
        self.current < self.steps.len()
    }

    pub fn next_entry(&mut self) -> Option<PromptSequenceEntry> {
        let step = self.steps.get(self.current)?;
        let index = self.current;
        self.current += 1;

        let mut items: Vec<UserInput> = step
            .attachments
            .iter()
            .flatten()
            .map(|path| UserInput::LocalImage { path: path.clone() })
            .collect();
        items.push(UserInput::Text {
            text: step.prompt.clone(),
        });

        Some(PromptSequenceEntry {
            items,
            description: step
                .name
                .clone()
                .unwrap_or_else(|| format!("Step {}", index + 1)),
            index,
            total: self.steps.len(),
        })
    }
}

#[derive(Debug, Clone)]
struct PromptSequenceStep {
    prompt: String,
    name: Option<String>,
    attachments: Option<Vec<PathBuf>>,
}

impl PromptSequenceStep {
    fn from_toml(toml: PromptSequenceStepToml, base_dir: &Path) -> anyhow::Result<Self> {
        if toml.prompt.trim().is_empty() {
            anyhow::bail!("prompt-sequence step is missing a prompt");
        }

        let attachments = toml.attachments.map(|paths| {
            paths
                .into_iter()
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        base_dir.join(path)
                    }
                })
                .collect()
        });

        Ok(Self {
            prompt: toml.prompt,
            name: toml.name,
            attachments,
        })
    }
}

#[derive(Debug, Deserialize)]
struct PromptSequenceToml {
    #[serde(default)]
    steps: Vec<PromptSequenceStepToml>,
}

#[derive(Debug, Deserialize)]
struct PromptSequenceStepToml {
    prompt: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    attachments: Option<Vec<PathBuf>>,
}
