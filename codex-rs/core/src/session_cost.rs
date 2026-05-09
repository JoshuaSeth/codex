use std::path::PathBuf;

use anyhow::Context;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;
use serde::Deserialize;
use serde::Serialize;
use tiktoken_rs::CoreBPE;
use tiktoken_rs::cl100k_base_singleton;
use tiktoken_rs::o200k_base_singleton;
use tiktoken_rs::o200k_harmony_singleton;
use tiktoken_rs::p50k_base_singleton;
use tiktoken_rs::p50k_edit_singleton;
use tiktoken_rs::r50k_base_singleton;
use tiktoken_rs::tokenizer::Tokenizer;
use tiktoken_rs::tokenizer::get_tokenizer;
use tokio::fs;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::error;
use tracing::warn;

use crate::Prompt;
use crate::tools::spec::create_tools_json_for_responses_api;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackedTurnItemSource {
    Local,
    ModelOutput,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TrackedTurnPromptItem {
    pub(crate) item: ResponseItem,
    pub(crate) source: TrackedTurnItemSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PromptTokenEstimate {
    pub(crate) already_in_session_tokens: u64,
    pub(crate) new_local_tokens: u64,
    pub(crate) replayed_model_output_tokens: u64,
    pub(crate) instructions_tokens: u64,
    pub(crate) tools_tokens: u64,
    pub(crate) output_schema_tokens: u64,
    pub(crate) unclassified_input_tokens: u64,
    pub(crate) total_tokens: u64,
}

impl PromptTokenEstimate {
    pub(crate) fn add_assign(&mut self, other: &Self) {
        self.already_in_session_tokens = self
            .already_in_session_tokens
            .saturating_add(other.already_in_session_tokens);
        self.new_local_tokens = self.new_local_tokens.saturating_add(other.new_local_tokens);
        self.replayed_model_output_tokens = self
            .replayed_model_output_tokens
            .saturating_add(other.replayed_model_output_tokens);
        self.instructions_tokens = self
            .instructions_tokens
            .saturating_add(other.instructions_tokens);
        self.tools_tokens = self.tools_tokens.saturating_add(other.tools_tokens);
        self.output_schema_tokens = self
            .output_schema_tokens
            .saturating_add(other.output_schema_tokens);
        self.unclassified_input_tokens = self
            .unclassified_input_tokens
            .saturating_add(other.unclassified_input_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RequestPromptEstimate {
    pub(crate) request_index: u64,
    #[serde(default)]
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) tokenizer: String,
    pub(crate) estimate: PromptTokenEstimate,
}

#[derive(Debug, Clone)]
pub(crate) struct TurnCostTrackingState {
    pub(crate) turn_id: String,
    pub(crate) model: String,
    pub(crate) started_at: i64,
    pub(crate) baseline_prompt_items: Option<Vec<ResponseItem>>,
    pub(crate) tracked_turn_items: Vec<TrackedTurnPromptItem>,
    pub(crate) request_estimates: Vec<RequestPromptEstimate>,
    pub(crate) total_estimate: PromptTokenEstimate,
    pub(crate) classification_reset_count: u64,
    pub(crate) tokenizer: Option<String>,
    pub(crate) tokenizers: Vec<String>,
    pub(crate) errors: Vec<String>,
}

impl TurnCostTrackingState {
    pub(crate) fn new(turn_id: String, model: String) -> Self {
        Self {
            turn_id,
            model,
            started_at: Utc::now().timestamp(),
            baseline_prompt_items: None,
            tracked_turn_items: Vec::new(),
            request_estimates: Vec::new(),
            total_estimate: PromptTokenEstimate::default(),
            classification_reset_count: 0,
            tokenizer: None,
            tokenizers: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub(crate) fn set_baseline_prompt_items(&mut self, baseline_prompt_items: Vec<ResponseItem>) {
        self.baseline_prompt_items = Some(baseline_prompt_items);
    }

    pub(crate) fn reset_baseline_prompt_items(&mut self, baseline_prompt_items: Vec<ResponseItem>) {
        self.baseline_prompt_items = Some(baseline_prompt_items);
        self.tracked_turn_items.clear();
        self.classification_reset_count = self.classification_reset_count.saturating_add(1);
    }

    pub(crate) fn record_turn_items(
        &mut self,
        items: &[ResponseItem],
        source: TrackedTurnItemSource,
    ) {
        self.tracked_turn_items.extend(
            items
                .iter()
                .cloned()
                .map(|item| TrackedTurnPromptItem { item, source }),
        );
    }

    pub(crate) fn record_request_estimate(
        &mut self,
        model: String,
        tokenizer: String,
        estimate: PromptTokenEstimate,
    ) {
        self.tokenizer = Some(tokenizer.clone());
        if !self.tokenizers.contains(&tokenizer) {
            self.tokenizers.push(tokenizer.clone());
        }
        let request_index = u64::try_from(self.request_estimates.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        self.request_estimates.push(RequestPromptEstimate {
            request_index,
            model,
            tokenizer,
            estimate: estimate.clone(),
        });
        self.total_estimate.add_assign(&estimate);
    }

    pub(crate) fn record_error(&mut self, error: String) {
        if !self.errors.contains(&error) {
            self.errors.push(error);
        }
    }

    pub(crate) fn has_activity(&self) -> bool {
        !self.request_estimates.is_empty() || !self.errors.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionCostTurnEntry {
    pub(crate) turn_id: String,
    pub(crate) model: String,
    pub(crate) tokenizer: Option<String>,
    #[serde(default)]
    pub(crate) tokenizers: Vec<String>,
    pub(crate) started_at: i64,
    pub(crate) completed_at: i64,
    pub(crate) request_count: u64,
    pub(crate) classification_reset_count: u64,
    pub(crate) estimated_input_tokens: PromptTokenEstimate,
    pub(crate) reported_usage: TokenUsage,
    pub(crate) errors: Vec<String>,
    pub(crate) requests: Vec<RequestPromptEstimate>,
}

impl SessionCostTurnEntry {
    pub(crate) fn from_tracking(
        tracking: TurnCostTrackingState,
        completed_at: i64,
        reported_usage: TokenUsage,
    ) -> Self {
        let request_count = u64::try_from(tracking.request_estimates.len()).unwrap_or(u64::MAX);
        Self {
            turn_id: tracking.turn_id,
            model: tracking.model,
            tokenizer: tracking.tokenizer,
            tokenizers: tracking.tokenizers,
            started_at: tracking.started_at,
            completed_at,
            request_count,
            classification_reset_count: tracking.classification_reset_count,
            estimated_input_tokens: tracking.total_estimate,
            reported_usage,
            errors: tracking.errors,
            requests: tracking.request_estimates,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionCostTotals {
    pub(crate) turn_count: u64,
    pub(crate) request_count: u64,
    pub(crate) estimated_input_tokens: PromptTokenEstimate,
    pub(crate) reported_usage: TokenUsage,
}

impl SessionCostTotals {
    fn include_turn(&mut self, turn: &SessionCostTurnEntry) {
        self.turn_count = self.turn_count.saturating_add(1);
        self.request_count = self.request_count.saturating_add(turn.request_count);
        self.estimated_input_tokens
            .add_assign(&turn.estimated_input_tokens);
        self.reported_usage.add_assign(&turn.reported_usage);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SessionCostFile {
    schema_version: u32,
    session_id: String,
    session_source: SessionSource,
    provider_id: String,
    rollout_path: Option<PathBuf>,
    created_at: i64,
    updated_at: i64,
    totals: SessionCostTotals,
    turns: Vec<SessionCostTurnEntry>,
}

impl SessionCostFile {
    fn new(
        session_id: ThreadId,
        session_source: SessionSource,
        provider_id: String,
        rollout_path: Option<PathBuf>,
    ) -> Self {
        let now = Utc::now().timestamp();
        Self {
            schema_version: 1,
            session_id: session_id.to_string(),
            session_source,
            provider_id,
            rollout_path,
            created_at: now,
            updated_at: now,
            totals: SessionCostTotals::default(),
            turns: Vec::new(),
        }
    }

    fn append_turn(&mut self, turn: SessionCostTurnEntry) {
        self.updated_at = Utc::now().timestamp();
        self.totals.include_turn(&turn);
        self.turns.push(turn);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionCostRecorder {
    tx: mpsc::UnboundedSender<SessionCostCommand>,
    pub(crate) path: PathBuf,
}

#[derive(Debug)]
enum SessionCostCommand {
    RecordTurn(SessionCostTurnEntry),
    Flush { ack: oneshot::Sender<()> },
}

#[derive(Debug, Clone)]
pub(crate) struct SessionCostRecorderParams {
    pub(crate) codex_home: PathBuf,
    pub(crate) session_id: ThreadId,
    pub(crate) session_source: SessionSource,
    pub(crate) provider_id: String,
    pub(crate) rollout_path: Option<PathBuf>,
}

impl SessionCostRecorder {
    pub(crate) fn new(params: SessionCostRecorderParams) -> Self {
        let path = params
            .codex_home
            .join("sessions")
            .join(format!("cost_{}.json", params.session_id));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let path_for_worker = path.clone();
        tokio::spawn(async move {
            let mut file_state = load_cost_file(&path_for_worker, &params).await;
            while let Some(command) = rx.recv().await {
                match command {
                    SessionCostCommand::RecordTurn(turn) => {
                        file_state.append_turn(turn);
                        if let Err(err) = persist_cost_file(&path_for_worker, &file_state).await {
                            error!(
                                path = %path_for_worker.display(),
                                "failed to persist session cost file: {err:#}"
                            );
                        }
                    }
                    SessionCostCommand::Flush { ack } => {
                        let _ = ack.send(());
                    }
                }
            }
        });
        Self { tx, path }
    }

    pub(crate) fn record_turn(&self, turn: SessionCostTurnEntry) {
        if let Err(err) = self.tx.send(SessionCostCommand::RecordTurn(turn)) {
            warn!("failed to queue session cost write: {err}");
        }
    }

    pub(crate) async fn flush(&self) -> anyhow::Result<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(SessionCostCommand::Flush { ack: ack_tx })
            .map_err(|err| anyhow::anyhow!("failed to queue session cost flush: {err}"))?;
        ack_rx
            .await
            .map_err(|err| anyhow::anyhow!("failed waiting for session cost flush: {err}"))
    }
}

async fn load_cost_file(path: &PathBuf, params: &SessionCostRecorderParams) -> SessionCostFile {
    match fs::read(path).await {
        Ok(bytes) => match serde_json::from_slice::<SessionCostFile>(&bytes) {
            Ok(file) => file,
            Err(err) => {
                error!(
                    path = %path.display(),
                    "failed to parse existing session cost file, recreating it: {err:#}"
                );
                SessionCostFile::new(
                    params.session_id,
                    params.session_source.clone(),
                    params.provider_id.clone(),
                    params.rollout_path.clone(),
                )
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => SessionCostFile::new(
            params.session_id,
            params.session_source.clone(),
            params.provider_id.clone(),
            params.rollout_path.clone(),
        ),
        Err(err) => {
            error!(
                path = %path.display(),
                "failed to read existing session cost file, recreating it: {err:#}"
            );
            SessionCostFile::new(
                params.session_id,
                params.session_source.clone(),
                params.provider_id.clone(),
                params.rollout_path.clone(),
            )
        }
    }
}

async fn persist_cost_file(path: &PathBuf, file: &SessionCostFile) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("session cost file should have a parent directory")?;
    fs::create_dir_all(parent).await?;
    let tmp_path = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(file)?;
    fs::write(&tmp_path, bytes).await?;
    fs::rename(&tmp_path, path).await?;
    Ok(())
}

struct PromptTokenCounter {
    encoding_name: &'static str,
    bpe: &'static CoreBPE,
}

impl PromptTokenCounter {
    fn for_model(model: &str) -> anyhow::Result<Self> {
        let tokenizer = get_tokenizer(model)
            .ok_or_else(|| anyhow::anyhow!("unsupported tokenizer model: {model}"))?;
        let (encoding_name, bpe) = match tokenizer {
            Tokenizer::O200kHarmony => ("o200k_harmony", o200k_harmony_singleton()),
            Tokenizer::O200kBase => ("o200k_base", o200k_base_singleton()),
            Tokenizer::Cl100kBase => ("cl100k_base", cl100k_base_singleton()),
            Tokenizer::P50kBase => ("p50k_base", p50k_base_singleton()),
            Tokenizer::R50kBase | Tokenizer::Gpt2 => ("r50k_base", r50k_base_singleton()),
            Tokenizer::P50kEdit => ("p50k_edit", p50k_edit_singleton()),
        };
        Ok(Self { encoding_name, bpe })
    }

    fn count_text(&self, text: &str) -> u64 {
        u64::try_from(self.bpe.encode_ordinary(text).len()).unwrap_or(u64::MAX)
    }

    fn count_json<T: Serialize>(&self, value: &T) -> anyhow::Result<u64> {
        let serialized = serde_json::to_string(value)?;
        Ok(self.count_text(&serialized))
    }
}

pub(crate) fn estimate_request_tokens(
    prompt: &Prompt,
    model: &str,
    tracking: &TurnCostTrackingState,
) -> anyhow::Result<(String, PromptTokenEstimate)> {
    let counter = PromptTokenCounter::for_model(model)?;
    let raw_input = &prompt.input;
    let formatted_input = prompt.get_formatted_input();
    if raw_input.len() != formatted_input.len() {
        return Err(anyhow::anyhow!(
            "prompt input formatting changed item count from {} to {}",
            raw_input.len(),
            formatted_input.len()
        ));
    }

    let mut estimate = PromptTokenEstimate::default();
    let baseline = tracking
        .baseline_prompt_items
        .as_deref()
        .unwrap_or_default();
    let mut baseline_index = 0usize;
    let mut tracked_index = 0usize;
    for (raw_item, formatted_item) in raw_input.iter().zip(formatted_input.iter()) {
        let tokens = counter.count_json(formatted_item)?;
        if baseline_index < baseline.len() && baseline[baseline_index] == *raw_item {
            estimate.already_in_session_tokens =
                estimate.already_in_session_tokens.saturating_add(tokens);
            baseline_index = baseline_index.saturating_add(1);
            continue;
        }

        if tracked_index < tracking.tracked_turn_items.len()
            && tracking.tracked_turn_items[tracked_index].item == *raw_item
        {
            match tracking.tracked_turn_items[tracked_index].source {
                TrackedTurnItemSource::Local => {
                    estimate.new_local_tokens = estimate.new_local_tokens.saturating_add(tokens);
                }
                TrackedTurnItemSource::ModelOutput => {
                    estimate.replayed_model_output_tokens =
                        estimate.replayed_model_output_tokens.saturating_add(tokens);
                }
            }
            tracked_index = tracked_index.saturating_add(1);
            continue;
        }

        estimate.unclassified_input_tokens =
            estimate.unclassified_input_tokens.saturating_add(tokens);
    }

    estimate.instructions_tokens = counter.count_text(&prompt.base_instructions.text);
    let tools = create_tools_json_for_responses_api(&prompt.tools)?;
    estimate.tools_tokens = counter.count_json(&tools)?;
    if let Some(output_schema) = prompt.output_schema.as_ref() {
        estimate.output_schema_tokens = counter.count_json(output_schema)?;
    }
    estimate.total_tokens = estimate
        .already_in_session_tokens
        .saturating_add(estimate.new_local_tokens)
        .saturating_add(estimate.replayed_model_output_tokens)
        .saturating_add(estimate.instructions_tokens)
        .saturating_add(estimate.tools_tokens)
        .saturating_add(estimate.output_schema_tokens)
        .saturating_add(estimate.unclassified_input_tokens);

    Ok((counter.encoding_name.to_string(), estimate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;

    fn user_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            end_turn: None,
            phase: None,
        }
    }

    fn assistant_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: text.to_string(),
            }],
            end_turn: None,
            phase: None,
        }
    }

    #[test]
    fn estimate_request_tokens_splits_prompt_segments() {
        let baseline_item = user_message("baseline");
        let local_item = user_message("new local");
        let replayed_model_item = assistant_message("tool call");

        let mut tracking = TurnCostTrackingState::new("turn-1".to_string(), "gpt-5".to_string());
        tracking.set_baseline_prompt_items(vec![baseline_item.clone()]);
        tracking.record_turn_items(
            std::slice::from_ref(&local_item),
            TrackedTurnItemSource::Local,
        );
        tracking.record_turn_items(
            std::slice::from_ref(&replayed_model_item),
            TrackedTurnItemSource::ModelOutput,
        );

        let prompt = Prompt {
            input: vec![
                baseline_item,
                local_item.clone(),
                replayed_model_item.clone(),
            ],
            tools: Vec::new(),
            parallel_tool_calls: false,
            base_instructions: BaseInstructions {
                text: "system instructions".to_string(),
            },
            personality: None,
            output_schema: None,
        };

        let (_tokenizer, estimate) =
            estimate_request_tokens(&prompt, "gpt-5", &tracking).expect("estimate should succeed");

        assert!(estimate.already_in_session_tokens > 0);
        assert!(estimate.new_local_tokens > 0);
        assert!(estimate.replayed_model_output_tokens > 0);
        assert!(estimate.instructions_tokens > 0);
        assert!(estimate.tools_tokens > 0);
        assert_eq!(estimate.unclassified_input_tokens, 0);
        assert_eq!(
            estimate.total_tokens,
            estimate.already_in_session_tokens
                + estimate.new_local_tokens
                + estimate.replayed_model_output_tokens
                + estimate.instructions_tokens
                + estimate.tools_tokens
                + estimate.output_schema_tokens
                + estimate.unclassified_input_tokens
        );
    }

    #[test]
    fn record_request_estimate_accumulates_totals() {
        let mut tracking = TurnCostTrackingState::new("turn-1".to_string(), "gpt-5".to_string());
        tracking.record_request_estimate(
            "gpt-5".to_string(),
            "o200k_base".to_string(),
            PromptTokenEstimate {
                total_tokens: 10,
                new_local_tokens: 4,
                ..PromptTokenEstimate::default()
            },
        );
        tracking.record_request_estimate(
            "gpt-5".to_string(),
            "o200k_base".to_string(),
            PromptTokenEstimate {
                total_tokens: 6,
                replayed_model_output_tokens: 2,
                ..PromptTokenEstimate::default()
            },
        );

        assert_eq!(tracking.tokenizer.as_deref(), Some("o200k_base"));
        assert_eq!(tracking.tokenizers, vec!["o200k_base".to_string()]);
        assert_eq!(tracking.request_estimates.len(), 2);
        assert_eq!(tracking.request_estimates[0].model, "gpt-5".to_string());
        assert_eq!(
            tracking.request_estimates[0].tokenizer,
            "o200k_base".to_string()
        );
        assert_eq!(tracking.total_estimate.total_tokens, 16);
        assert_eq!(tracking.total_estimate.new_local_tokens, 4);
        assert_eq!(tracking.total_estimate.replayed_model_output_tokens, 2);
    }
}
