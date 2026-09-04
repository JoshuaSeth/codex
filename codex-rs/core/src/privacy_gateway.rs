use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use chrono::DateTime;
use chrono::TimeDelta;
use chrono::Utc;
use codex_api::ResponseEvent;
use codex_api::ResponsesApiRequest;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::WebSearchAction;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use url::Url;
use uuid::Uuid;

const ENABLE_ENV: &str = "PITCHAI_CODEX_PRIVACY_GATEWAY_ENABLED";
const MODE_ENV: &str = "PITCHAI_CODEX_PRIVACY_GATEWAY_MODE";
const URL_ENV: &str = "PITCHAI_CODEX_PRIVACY_GATEWAY_URL";
const TOKEN_FILE_ENV: &str = "PITCHAI_CODEX_PRIVACY_GATEWAY_TOKEN_FILE";
const CLASSIFICATION_ENV: &str = "PITCHAI_CODEX_PRIVACY_GATEWAY_DATA_CLASSIFICATION";
const LANGUAGE_ENV: &str = "PITCHAI_CODEX_PRIVACY_GATEWAY_LANGUAGE";
const LOCALE_ENV: &str = "PITCHAI_CODEX_PRIVACY_GATEWAY_LOCALE";
const TTL_ENV: &str = "PITCHAI_CODEX_PRIVACY_GATEWAY_TTL_SECONDS";
const TIMEOUT_ENV: &str = "PITCHAI_CODEX_PRIVACY_GATEWAY_TIMEOUT_MS";
const LEGACY_ENABLE_ENV: &str = "PITCHAI_CODEX_PRIVACY_MIDDLEWARE";
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 10_000;
const MAX_GATEWAY_TEXT_CHARS: usize = 20_000;
const MAX_FRAME_COUNT: usize = 5_000;
const PREFERRED_SPLIT_WINDOW_CHARS: usize = 2_048;
const RESTORE_CHUNK_CHARS: usize = 16_000;
const PURGE_TEXT: &str = "privacy mapping purge";

#[derive(Clone)]
pub(crate) struct PrivacyGateway {
    state: Arc<GatewayState>,
    context_id: String,
}

enum GatewayState {
    Disabled,
    Enabled(Arc<GatewayConfig>),
    Invalid(String),
}

struct GatewayConfig {
    mode: GatewayMode,
    base_url: Url,
    bearer_token: String,
    data_classification: String,
    language: String,
    locale: String,
    ttl_seconds: u64,
    timeout_ms: u64,
    client: reqwest::Client,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GatewayMode {
    Pseudonymize,
    DeepPseudonymize,
}

impl GatewayMode {
    fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim).unwrap_or("pseudonymize") {
            "pseudonymize" => Ok(Self::Pseudonymize),
            "deep-pseudonymize" => Ok(Self::DeepPseudonymize),
            _ => bail!("{MODE_ENV} must be pseudonymize or deep-pseudonymize"),
        }
    }

    const fn path(self) -> &'static str {
        match self {
            Self::Pseudonymize => "v1/pseudonymize",
            Self::DeepPseudonymize => "v1/deep-pseudonymize",
        }
    }

    const fn schema_version(self) -> &'static str {
        match self {
            Self::Pseudonymize => "reversible-pseudonymization-v3",
            Self::DeepPseudonymize => "deep-pseudonymization-v1",
        }
    }

    const fn date_policy(self) -> &'static str {
        match self {
            Self::Pseudonymize => "plausible_keyed_shift",
            Self::DeepPseudonymize => "irreversible_placeholder",
        }
    }

    const fn restore_match_mode(self) -> &'static str {
        match self {
            Self::Pseudonymize => "exact_and_registered_aliases",
            Self::DeepPseudonymize => "exact_values",
        }
    }

    const fn default_ttl_seconds(self) -> u64 {
        match self {
            Self::Pseudonymize => 900,
            Self::DeepPseudonymize => 300,
        }
    }

    const fn maximum_ttl_seconds(self) -> u64 {
        match self {
            Self::Pseudonymize => 3_600,
            Self::DeepPseudonymize => 300,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Pseudonymize => "pseudonymize",
            Self::DeepPseudonymize => "deep-pseudonymize",
        }
    }
}

impl fmt::Debug for GatewayConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayConfig")
            .field("mode", &self.mode)
            .field("base_url", &self.base_url)
            .field("data_classification", &self.data_classification)
            .field("language", &self.language)
            .field("locale", &self.locale)
            .field("ttl_seconds", &self.ttl_seconds)
            .field("timeout_ms", &self.timeout_ms)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for PrivacyGateway {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivacyGateway")
            .field("enabled", &self.enabled())
            .field("context_id", &self.context_id)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for GatewayState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("Disabled"),
            Self::Enabled(config) => formatter
                .debug_struct("Enabled")
                .field("mode", &config.mode)
                .field("base_url", &config.base_url)
                .field("data_classification", &config.data_classification)
                .field("language", &config.language)
                .field("locale", &config.locale)
                .field("ttl_seconds", &config.ttl_seconds)
                .finish_non_exhaustive(),
            Self::Invalid(_) => formatter.write_str("Invalid(<redacted>)"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct PreparedGatewayRequest {
    pub(crate) body: Value,
    pub(crate) session: Option<GatewayRequestSession>,
}

#[derive(Debug)]
pub(crate) struct GatewayRequestSession {
    config: Arc<GatewayConfig>,
    mappings: Vec<GatewayMapping>,
    match_values: Vec<String>,
    pending: BTreeMap<StreamKey, String>,
}

#[derive(Debug)]
struct GatewayMapping {
    mapping_id: Uuid,
    restoration_capability: String,
    expires_at: DateTime<Utc>,
    match_values: Vec<String>,
    purged: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum StreamKey {
    OutputText,
    ToolCall {
        item_id: String,
        call_id: Option<String>,
    },
    ReasoningSummary(i64),
    ReasoningContent(i64),
}

#[derive(Clone, Debug)]
enum PathPart {
    Key(String),
    Index(usize),
}

#[derive(Debug)]
struct TextLocation {
    path: Vec<PathPart>,
    text: String,
}

#[derive(Debug)]
struct TextFragment {
    field_index: usize,
    text: String,
}

#[derive(Debug)]
struct PreparedTextBatches {
    batches: Vec<Vec<TextFragment>>,
    field_paths: Vec<Vec<PathPart>>,
}

#[derive(Debug, Serialize)]
struct PseudonymizePayload<'a> {
    text: &'a str,
    data_classification: &'a str,
    language: &'a str,
    locale: &'a str,
    context_id: &'a str,
    ttl_seconds: u64,
    date_policy: &'static str,
}

#[derive(Debug, Deserialize)]
struct PseudonymizeResponse {
    schema_version: String,
    data_classification: String,
    pseudonymized_text: String,
    mapping_id: Uuid,
    restoration_capability: String,
    expires_at: DateTime<Utc>,
    streaming_restoration_matches: Vec<StreamingRestorationMatch>,
}

#[derive(Debug, Deserialize)]
struct StreamingRestorationMatch {
    value: String,
    source_fingerprint: String,
}

#[derive(Debug, Serialize)]
struct RestorePayload<'a> {
    text: &'a str,
    data_classification: &'a str,
    mapping_id: Uuid,
    restoration_capability: &'a str,
    match_mode: &'static str,
    purge_after_restore: bool,
}

#[derive(Debug, Deserialize)]
struct RestoreResponse {
    schema_version: String,
    data_classification: String,
    restored_text: String,
    mapping_id: Uuid,
    mapping_purged: bool,
}

impl PrivacyGateway {
    pub(crate) fn from_env(context_id: String) -> Self {
        let state = match parse_enable_value(std::env::var(ENABLE_ENV).ok().as_deref()) {
            Ok(false) => GatewayState::Disabled,
            Ok(true) => match load_config() {
                Ok(config) => GatewayState::Enabled(Arc::new(config)),
                Err(error) => GatewayState::Invalid(error.to_string()),
            },
            Err(error) => GatewayState::Invalid(error.to_string()),
        };
        Self {
            state: Arc::new(state),
            context_id,
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        !matches!(self.state.as_ref(), GatewayState::Disabled)
    }

    fn config(&self) -> Result<&GatewayConfig> {
        match self.state.as_ref() {
            GatewayState::Disabled => bail!("privacy gateway is disabled"),
            GatewayState::Enabled(config) => Ok(config.as_ref()),
            GatewayState::Invalid(message) => {
                bail!("privacy gateway configuration is invalid: {message}")
            }
        }
    }

    pub(crate) async fn prepare_responses_request(
        &self,
        request: &ResponsesApiRequest,
        trusted_catalog_instructions: &str,
    ) -> Result<PreparedGatewayRequest> {
        let body = serde_json::to_value(request)
            .context("failed to encode Responses request before privacy gateway")?;
        self.prepare_json_body_with_trusted_instructions(body, trusted_catalog_instructions)
            .await
    }

    pub(crate) async fn prepare_json_body(&self, body: Value) -> Result<PreparedGatewayRequest> {
        self.prepare_json_body_inner(body, None).await
    }

    pub(crate) async fn prepare_json_body_with_trusted_instructions(
        &self,
        body: Value,
        trusted_catalog_instructions: &str,
    ) -> Result<PreparedGatewayRequest> {
        // Only the byte-identical model-catalog rendering is trusted. A configured/custom base
        // instruction string still flows through the gateway with the dynamic request content.
        self.prepare_json_body_inner(body, Some(trusted_catalog_instructions))
            .await
    }

    async fn prepare_json_body_inner(
        &self,
        mut body: Value,
        trusted_catalog_instructions: Option<&str>,
    ) -> Result<PreparedGatewayRequest> {
        let config = self.config()?;
        let started_at = Instant::now();
        let locations = match trusted_catalog_instructions {
            Some(instructions) => {
                request_text_locations_with_trusted_instructions(&body, Some(instructions))?
            }
            None => request_text_locations(&body)?,
        };
        if locations.is_empty() {
            return Ok(PreparedGatewayRequest {
                body,
                session: None,
            });
        }
        let field_count = locations.len();
        let protected_text_chars = locations
            .iter()
            .map(|location| location.text.chars().count())
            .sum::<usize>();

        let PreparedTextBatches {
            batches,
            field_paths,
        } = build_batches(locations)?;
        let batch_count = batches.len();
        tracing::info!(
            privacy_mode = config.mode.label(),
            field_count,
            protected_text_chars,
            batch_count,
            "privacy gateway preparing outbound OpenAI request"
        );
        let mut protected_fields = vec![String::new(); field_paths.len()];
        let mut mappings = Vec::new();
        let mut fingerprints: HashMap<String, String> = HashMap::new();
        let mut all_match_values = Vec::new();
        for batch in batches {
            let framed = frame_batch(&batch)?;
            let response = match pseudonymize(config, &self.context_id, &framed).await {
                Ok(response) => response,
                Err(error) => {
                    purge_mappings_best_effort(config, &mut mappings).await;
                    return Err(error);
                }
            };
            let mut mapping = match mapping_from_response(config, &response) {
                Ok(mapping) => mapping,
                Err(error) => {
                    let mut cleanup_mapping = mapping_for_cleanup(&response);
                    purge_mapping_best_effort(config, &mut cleanup_mapping).await;
                    purge_mappings_best_effort(config, &mut mappings).await;
                    return Err(error);
                }
            };
            let transformed = match unframe_batch(&response.pseudonymized_text, batch.len()) {
                Ok(transformed) => transformed,
                Err(error) => {
                    purge_mapping_best_effort(config, &mut mapping).await;
                    purge_mappings_best_effort(config, &mut mappings).await;
                    return Err(error);
                }
            };
            if transformed.len() != batch.len() {
                purge_mapping_best_effort(config, &mut mapping).await;
                purge_mappings_best_effort(config, &mut mappings).await;
                bail!("privacy gateway returned the wrong protected field count");
            }
            for (fragment, transformed_text) in batch.iter().zip(transformed) {
                let Some(field) = protected_fields.get_mut(fragment.field_index) else {
                    purge_mapping_best_effort(config, &mut mapping).await;
                    purge_mappings_best_effort(config, &mut mappings).await;
                    bail!("privacy gateway field reconstruction index is invalid");
                };
                field.push_str(&transformed_text);
            }

            for item in &response.streaming_restoration_matches {
                if let Err(error) = validate_match(item) {
                    purge_mapping_best_effort(config, &mut mapping).await;
                    purge_mappings_best_effort(config, &mut mappings).await;
                    return Err(error);
                }
                let normalized = normalize_match_value(&item.value);
                if let Some(previous) = fingerprints.get(&normalized)
                    && previous != &item.source_fingerprint
                {
                    purge_mapping_best_effort(config, &mut mapping).await;
                    purge_mappings_best_effort(config, &mut mappings).await;
                    bail!(
                        "privacy gateway returned a cross-batch restoration collision; request was not sent"
                    );
                }
                fingerprints.insert(normalized, item.source_fingerprint.clone());
                if !all_match_values.contains(&item.value) {
                    all_match_values.push(item.value.clone());
                }
            }
            if mapping.match_values.is_empty() {
                purge_mapping(config, &mut mapping).await?;
            } else {
                mappings.push(mapping);
            }
        }

        for (path, transformed_text) in field_paths.iter().zip(protected_fields) {
            if let Err(error) = set_text_at_path(&mut body, path, transformed_text) {
                purge_mappings_best_effort(config, &mut mappings).await;
                return Err(error);
            }
        }

        tracing::info!(
            privacy_mode = config.mode.label(),
            field_count,
            protected_text_chars,
            batch_count,
            mapping_count = mappings.len(),
            duration_ms = started_at.elapsed().as_millis(),
            "privacy gateway prepared outbound OpenAI request"
        );

        Ok(PreparedGatewayRequest {
            body,
            session: (!mappings.is_empty()).then(|| GatewayRequestSession {
                config: match self.state.as_ref() {
                    GatewayState::Enabled(config) => Arc::clone(config),
                    GatewayState::Disabled | GatewayState::Invalid(_) => unreachable!(),
                },
                mappings,
                match_values: all_match_values,
                pending: BTreeMap::new(),
            }),
        })
    }

    pub(crate) fn reject_unprotected_endpoint(&self, endpoint_name: &str) -> Result<()> {
        if self.enabled() {
            bail!(
                "privacy gateway is enabled, but {endpoint_name} has no protected edge adapter; request was blocked before OpenAI"
            );
        }
        Ok(())
    }
}

impl GatewayRequestSession {
    pub(crate) async fn transform_event(
        &mut self,
        event: ResponseEvent,
    ) -> Result<Vec<ResponseEvent>> {
        match event {
            ResponseEvent::OutputTextDelta(delta) => {
                let restored = self.restore_delta(StreamKey::OutputText, delta).await?;
                Ok(vec![ResponseEvent::OutputTextDelta(restored)])
            }
            ResponseEvent::ToolCallInputDelta {
                item_id,
                call_id,
                delta,
            } => {
                let key = StreamKey::ToolCall {
                    item_id: item_id.clone(),
                    call_id: call_id.clone(),
                };
                let restored = self.restore_delta(key, delta).await?;
                Ok(vec![ResponseEvent::ToolCallInputDelta {
                    item_id,
                    call_id,
                    delta: restored,
                }])
            }
            ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            } => {
                let restored = self
                    .restore_delta(StreamKey::ReasoningSummary(summary_index), delta)
                    .await?;
                Ok(vec![ResponseEvent::ReasoningSummaryDelta {
                    delta: restored,
                    summary_index,
                }])
            }
            ResponseEvent::ReasoningContentDelta {
                delta,
                content_index,
            } => {
                let restored = self
                    .restore_delta(StreamKey::ReasoningContent(content_index), delta)
                    .await?;
                Ok(vec![ResponseEvent::ReasoningContentDelta {
                    delta: restored,
                    content_index,
                }])
            }
            ResponseEvent::OutputItemAdded(mut item) => {
                self.restore_response_item(&mut item).await?;
                Ok(vec![ResponseEvent::OutputItemAdded(item)])
            }
            ResponseEvent::OutputItemDone(mut item) => {
                let mut events = self.flush_pending().await?;
                self.restore_response_item(&mut item).await?;
                events.push(ResponseEvent::OutputItemDone(item));
                Ok(events)
            }
            ResponseEvent::Completed {
                response_id,
                token_usage,
                end_turn,
            } => {
                let mut events = self.flush_pending().await?;
                self.purge().await?;
                events.push(ResponseEvent::Completed {
                    response_id,
                    token_usage,
                    end_turn,
                });
                Ok(events)
            }
            event => Ok(vec![event]),
        }
    }

    pub(crate) async fn abort(&mut self) {
        purge_mappings_best_effort(&self.config, &mut self.mappings).await;
        self.pending.clear();
    }

    pub(crate) async fn restore_response_items_and_purge(
        &mut self,
        items: &mut [ResponseItem],
    ) -> Result<()> {
        for item in items {
            self.restore_response_item(item).await?;
        }
        self.purge().await
    }

    async fn purge(&mut self) -> Result<()> {
        let mut first_error = None;
        for mapping in &mut self.mappings {
            if let Err(error) = purge_mapping(&self.config, mapping).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        self.pending.clear();
        match first_error {
            Some(error) => Err(error.context("failed to purge every privacy gateway mapping")),
            None => Ok(()),
        }
    }

    async fn restore_delta(&mut self, key: StreamKey, delta: String) -> Result<String> {
        let mut combined = self.pending.remove(&key).unwrap_or_default();
        combined.push_str(&delta);
        let hold_from = pending_suffix_start(&combined, &self.match_values);
        let pending = combined.split_off(hold_from);
        if !pending.is_empty() {
            self.pending.insert(key, pending);
        }
        self.restore_complete_text(&combined).await
    }

    async fn flush_pending(&mut self) -> Result<Vec<ResponseEvent>> {
        let pending = std::mem::take(&mut self.pending);
        let mut events = Vec::with_capacity(pending.len());
        for (key, text) in pending {
            let restored = self.restore_complete_text(&text).await?;
            events.push(key.into_event(restored));
        }
        Ok(events)
    }

    async fn restore_complete_text(&self, text: &str) -> Result<String> {
        if text.is_empty() {
            return Ok(String::new());
        }
        let chunks = split_char_chunks(text, RESTORE_CHUNK_CHARS);
        let mut output = String::with_capacity(text.len());
        let mut pending = String::new();
        for chunk in chunks {
            pending.push_str(chunk);
            let hold_from = pending_suffix_start(&pending, &self.match_values);
            let held = pending.split_off(hold_from);
            output.push_str(&self.restore_through_mappings(&pending).await?);
            pending = held;
        }
        output.push_str(&self.restore_through_mappings(&pending).await?);
        Ok(output)
    }

    async fn restore_through_mappings(&self, text: &str) -> Result<String> {
        let mut restored = text.to_string();
        for mapping in &self.mappings {
            if !contains_any_match(&restored, &mapping.match_values) {
                continue;
            }
            if Utc::now() >= mapping.expires_at {
                bail!("privacy gateway restoration mapping expired before response restoration");
            }
            restored = restore_mapping(&self.config, mapping, &restored, false).await?;
        }
        Ok(restored)
    }

    async fn restore_response_item(&self, item: &mut ResponseItem) -> Result<()> {
        match item {
            ResponseItem::Message { content, .. } => {
                for item in content {
                    match item {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            *text = self.restore_complete_text(text).await?;
                        }
                        ContentItem::InputImage { .. } => {}
                    }
                }
            }
            ResponseItem::AgentMessage { content, .. } => {
                for item in content {
                    if let AgentMessageInputContent::InputText { text } = item {
                        *text = self.restore_complete_text(text).await?;
                    }
                }
            }
            ResponseItem::Reasoning {
                summary, content, ..
            } => {
                for item in summary {
                    let ReasoningItemReasoningSummary::SummaryText { text } = item;
                    *text = self.restore_complete_text(text).await?;
                }
                if let Some(content) = content {
                    for item in content {
                        match item {
                            ReasoningItemContent::ReasoningText { text }
                            | ReasoningItemContent::Text { text } => {
                                *text = self.restore_complete_text(text).await?;
                            }
                        }
                    }
                }
            }
            ResponseItem::LocalShellCall { action, .. } => {
                let LocalShellAction::Exec(action) = action;
                for command in &mut action.command {
                    *command = self.restore_complete_text(command).await?;
                }
                if let Some(value) = &mut action.working_directory {
                    *value = self.restore_complete_text(value).await?;
                }
                if let Some(value) = &mut action.user {
                    *value = self.restore_complete_text(value).await?;
                }
                if let Some(env) = &mut action.env {
                    for value in env.values_mut() {
                        *value = self.restore_complete_text(value).await?;
                    }
                }
            }
            ResponseItem::FunctionCall { arguments, .. } => {
                *arguments = self.restore_complete_text(arguments).await?;
            }
            ResponseItem::ToolSearchCall {
                execution,
                arguments,
                ..
            } => {
                *execution = self.restore_complete_text(execution).await?;
                self.restore_json_strings(arguments).await?;
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(text) = output.text_content_mut() {
                    *text = self.restore_complete_text(text).await?;
                } else if let Some(items) = output.content_items_mut() {
                    for item in items {
                        if let FunctionCallOutputContentItem::InputText { text } = item {
                            *text = self.restore_complete_text(text).await?;
                        }
                    }
                }
            }
            ResponseItem::CustomToolCall { input, .. } => {
                *input = self.restore_complete_text(input).await?;
            }
            ResponseItem::ToolSearchOutput {
                execution, tools, ..
            } => {
                *execution = self.restore_complete_text(execution).await?;
                for tool in tools {
                    self.restore_json_strings(tool).await?;
                }
            }
            ResponseItem::WebSearchCall { action, .. } => {
                if let Some(action) = action {
                    self.restore_web_search_action(action).await?;
                }
            }
            ResponseItem::ImageGenerationCall { revised_prompt, .. } => {
                if let Some(prompt) = revised_prompt {
                    *prompt = self.restore_complete_text(prompt).await?;
                }
            }
            ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => {}
        }
        Ok(())
    }

    async fn restore_json_strings(&self, value: &mut Value) -> Result<()> {
        let pointers = string_pointers(value);
        for path in pointers {
            let text = text_at_path(value, &path)?.to_string();
            let restored = self.restore_complete_text(&text).await?;
            set_text_at_path(value, &path, restored)?;
        }
        Ok(())
    }

    async fn restore_web_search_action(&self, action: &mut WebSearchAction) -> Result<()> {
        match action {
            WebSearchAction::Search { query, queries } => {
                if let Some(query) = query {
                    *query = self.restore_complete_text(query).await?;
                }
                if let Some(queries) = queries {
                    for query in queries {
                        *query = self.restore_complete_text(query).await?;
                    }
                }
            }
            WebSearchAction::OpenPage { url } => {
                if let Some(url) = url {
                    *url = self.restore_complete_text(url).await?;
                }
            }
            WebSearchAction::FindInPage { url, pattern } => {
                if let Some(url) = url {
                    *url = self.restore_complete_text(url).await?;
                }
                if let Some(pattern) = pattern {
                    *pattern = self.restore_complete_text(pattern).await?;
                }
            }
            WebSearchAction::Other => {}
        }
        Ok(())
    }
}

impl StreamKey {
    fn into_event(self, delta: String) -> ResponseEvent {
        match self {
            Self::OutputText => ResponseEvent::OutputTextDelta(delta),
            Self::ToolCall { item_id, call_id } => ResponseEvent::ToolCallInputDelta {
                item_id,
                call_id,
                delta,
            },
            Self::ReasoningSummary(summary_index) => ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            },
            Self::ReasoningContent(content_index) => ResponseEvent::ReasoningContentDelta {
                delta,
                content_index,
            },
        }
    }
}

fn load_config() -> Result<GatewayConfig> {
    if std::env::var(LEGACY_ENABLE_ENV)
        .map(|value| matches_enabled(&value))
        .unwrap_or(false)
    {
        bail!("legacy local privacy middleware and privacy gateway cannot both be enabled");
    }
    let base_url = required_env(URL_ENV)?;
    let base_url = Url::parse(&base_url).context("privacy gateway URL is invalid")?;
    validate_gateway_url(&base_url)?;
    let token_path = required_env(TOKEN_FILE_ENV)?;
    let bearer_token = read_token_file(Path::new(&token_path))?;
    let data_classification = required_env(CLASSIFICATION_ENV)?;
    if data_classification != "synthetic_demo" {
        bail!(
            "{CLASSIFICATION_ENV} must be synthetic_demo; the deployed privacy gateway is not authorized for real personal data"
        );
    }
    let language = std::env::var(LANGUAGE_ENV).unwrap_or_else(|_| "en".to_string());
    let locale = std::env::var(LOCALE_ENV).unwrap_or_else(|_| "en_GB".to_string());
    validate_language_locale(&language, &locale)?;
    let mode = GatewayMode::parse(std::env::var(MODE_ENV).ok().as_deref())?;
    let ttl_seconds = parse_bounded_env(
        TTL_ENV,
        mode.default_ttl_seconds(),
        60,
        mode.maximum_ttl_seconds(),
    )?;
    let timeout_ms = parse_bounded_env(TIMEOUT_ENV, DEFAULT_TIMEOUT_MS, 100, MAX_TIMEOUT_MS)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .context("failed to build privacy gateway HTTP client")?;
    Ok(GatewayConfig {
        mode,
        base_url,
        bearer_token,
        data_classification,
        language,
        locale,
        ttl_seconds,
        timeout_ms,
        client,
    })
}

fn parse_enable_value(value: Option<&str>) -> Result<bool> {
    let Some(value) = value else {
        return Ok(false);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!(
            "{ENABLE_ENV} must be one of 1/true/yes/on or 0/false/no/off; request execution is blocked"
        ),
    }
}

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required when enabled"))?;
    if value.trim().is_empty() {
        bail!("{name} must not be blank when enabled");
    }
    Ok(value)
}

fn matches_enabled(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn validate_gateway_url(url: &Url) -> Result<()> {
    if url.username() != "" || url.password().is_some() {
        bail!("privacy gateway URL must not contain credentials");
    }
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        bail!("privacy gateway URL must use HTTPS, except loopback test endpoints");
    }
    Ok(())
}

fn read_token_file(path: &Path) -> Result<String> {
    if !path.is_absolute() {
        bail!("privacy gateway token file path must be absolute");
    }
    let metadata = fs::metadata(path).with_context(|| {
        format!(
            "failed to inspect privacy gateway token file {}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        bail!("privacy gateway token path must identify a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("privacy gateway token file must not be accessible by group or other users");
        }
    }
    let token = fs::read_to_string(path).with_context(|| {
        format!(
            "failed to read privacy gateway token file {}",
            path.display()
        )
    })?;
    let token = token.trim().to_string();
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        bail!("privacy gateway token file must contain one non-empty bearer token");
    }
    Ok(token)
}

fn validate_language_locale(language: &str, locale: &str) -> Result<()> {
    match (language, locale) {
        ("en", "en_GB" | "en_US") | ("nl", "nl_NL" | "nl_BE") => Ok(()),
        _ => bail!("privacy gateway language/locale pair is unsupported"),
    }
}

fn parse_bounded_env(name: &str, default: u64, minimum: u64, maximum: u64) -> Result<u64> {
    let value = match std::env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .with_context(|| format!("{name} must be an integer"))?,
        Err(_) => default,
    };
    if !(minimum..=maximum).contains(&value) {
        bail!("{name} must be between {minimum} and {maximum}");
    }
    Ok(value)
}

async fn pseudonymize(
    config: &GatewayConfig,
    context_id: &str,
    text: &str,
) -> Result<PseudonymizeResponse> {
    let response = config
        .client
        .post(endpoint(&config.base_url, config.mode.path())?)
        .bearer_auth(&config.bearer_token)
        .json(&PseudonymizePayload {
            text,
            data_classification: &config.data_classification,
            language: &config.language,
            locale: &config.locale,
            context_id,
            ttl_seconds: config.ttl_seconds,
            date_policy: config.mode.date_policy(),
        })
        .send()
        .await
        .map_err(|error| gateway_request_error("pseudonymization", config.timeout_ms, error))?;
    require_success(response.status(), "pseudonymization")?;
    response
        .json()
        .await
        .context("privacy gateway pseudonymization response was invalid")
}

async fn restore_mapping(
    config: &GatewayConfig,
    mapping: &GatewayMapping,
    text: &str,
    purge_after_restore: bool,
) -> Result<String> {
    let response = config
        .client
        .post(endpoint(&config.base_url, "v1/de-pseudonymize")?)
        .bearer_auth(&config.bearer_token)
        .json(&RestorePayload {
            text,
            data_classification: &config.data_classification,
            mapping_id: mapping.mapping_id,
            restoration_capability: &mapping.restoration_capability,
            match_mode: config.mode.restore_match_mode(),
            purge_after_restore,
        })
        .send()
        .await
        .map_err(|error| gateway_request_error("restoration", config.timeout_ms, error))?;
    require_success(response.status(), "restoration")?;
    let response: RestoreResponse = response
        .json()
        .await
        .context("privacy gateway restoration response was invalid")?;
    if response.schema_version != "authorized-restoration-v2"
        || response.data_classification != config.data_classification
        || response.mapping_id != mapping.mapping_id
        || response.mapping_purged != purge_after_restore
    {
        bail!("privacy gateway restoration response failed integrity checks");
    }
    Ok(response.restored_text)
}

async fn purge_mapping(config: &GatewayConfig, mapping: &mut GatewayMapping) -> Result<()> {
    if mapping.purged {
        return Ok(());
    }
    let _ = restore_mapping(config, mapping, PURGE_TEXT, true).await?;
    mapping.purged = true;
    mapping.restoration_capability.clear();
    Ok(())
}

async fn purge_mapping_best_effort(config: &GatewayConfig, mapping: &mut GatewayMapping) {
    if let Err(error) = purge_mapping(config, mapping).await {
        tracing::warn!(error = %error, "failed to purge privacy gateway mapping during cleanup");
    }
}

async fn purge_mappings_best_effort(config: &GatewayConfig, mappings: &mut [GatewayMapping]) {
    for mapping in mappings {
        purge_mapping_best_effort(config, mapping).await;
    }
}

fn endpoint(base: &Url, path: &str) -> Result<Url> {
    let mut base = base.clone();
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    base.join(path)
        .context("failed to construct privacy gateway endpoint")
}

fn require_success(status: StatusCode, operation: &str) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    bail!("privacy gateway {operation} failed with HTTP {status}")
}

fn gateway_request_error(operation: &str, timeout_ms: u64, error: reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow::anyhow!("privacy gateway {operation} timed out after {timeout_ms} ms")
    } else {
        anyhow::Error::new(error).context(format!("privacy gateway {operation} request failed"))
    }
}

fn mapping_from_response(
    config: &GatewayConfig,
    response: &PseudonymizeResponse,
) -> Result<GatewayMapping> {
    if response.schema_version != config.mode.schema_version()
        || response.data_classification != config.data_classification
    {
        bail!("privacy gateway returned an incompatible pseudonymization contract");
    }
    if response.restoration_capability.len() != 43
        || !response
            .restoration_capability
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        bail!("privacy gateway returned a malformed restoration capability");
    }
    let now = Utc::now();
    let latest_allowed_expiry = now
        + TimeDelta::seconds(
            i64::try_from(config.ttl_seconds).context("privacy gateway TTL does not fit i64")? + 5,
        );
    if response.mapping_id.is_nil()
        || response.expires_at <= now
        || response.expires_at > latest_allowed_expiry
    {
        bail!("privacy gateway returned invalid restoration mapping identity or expiry");
    }
    Ok(GatewayMapping {
        mapping_id: response.mapping_id,
        restoration_capability: response.restoration_capability.clone(),
        expires_at: response.expires_at,
        match_values: response
            .streaming_restoration_matches
            .iter()
            .map(|item| item.value.clone())
            .collect(),
        purged: false,
    })
}

fn mapping_for_cleanup(response: &PseudonymizeResponse) -> GatewayMapping {
    GatewayMapping {
        mapping_id: response.mapping_id,
        restoration_capability: response.restoration_capability.clone(),
        expires_at: response.expires_at,
        match_values: response
            .streaming_restoration_matches
            .iter()
            .map(|item| item.value.clone())
            .collect(),
        purged: false,
    }
}

fn validate_match(item: &StreamingRestorationMatch) -> Result<()> {
    if item.value.is_empty()
        || item.source_fingerprint.len() != 64
        || !item
            .source_fingerprint
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        bail!("privacy gateway returned malformed streaming restoration metadata");
    }
    Ok(())
}

fn request_text_locations(body: &Value) -> Result<Vec<TextLocation>> {
    request_text_locations_with_trusted_instructions(body, None)
}

fn request_text_locations_with_trusted_instructions(
    body: &Value,
    trusted_catalog_instructions: Option<&str>,
) -> Result<Vec<TextLocation>> {
    let object = body
        .as_object()
        .context("Responses request did not serialize as an object")?;
    let mut result = Vec::new();
    if let Some(Value::String(instructions)) = object.get("instructions")
        && !instructions.trim().is_empty()
        && trusted_catalog_instructions != Some(instructions.as_str())
    {
        result.push(TextLocation {
            path: vec![PathPart::Key("instructions".to_string())],
            text: instructions.clone(),
        });
    }
    if let Some(input) = object.get("input") {
        collect_dynamic_strings(
            input,
            &mut vec![PathPart::Key("input".to_string())],
            &mut result,
        );
    }
    Ok(result)
}

fn collect_dynamic_strings(
    value: &Value,
    path: &mut Vec<PathPart>,
    output: &mut Vec<TextLocation>,
) {
    match value {
        Value::String(text) => {
            if !text.trim().is_empty() {
                output.push(TextLocation {
                    path: path.clone(),
                    text: text.clone(),
                });
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                path.push(PathPart::Index(index));
                collect_dynamic_strings(value, path, output);
                path.pop();
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                if excluded_dynamic_key(path, key)
                    || (key == "result" && is_top_level_input_item_path(path))
                {
                    continue;
                }
                path.push(PathPart::Key(key.clone()));
                collect_dynamic_strings(value, path, output);
                path.pop();
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn is_top_level_input_item_path(path: &[PathPart]) -> bool {
    matches!(
        path,
        [PathPart::Key(input), PathPart::Index(_)] if input == "input"
    )
}

fn excluded_dynamic_key(path: &[PathPart], key: &str) -> bool {
    if matches!(
        key,
        "type"
            | "encrypted_content"
            | "image_url"
            | "detail"
            | "schema"
            | "parameters"
            | "input_schema"
    ) {
        return true;
    }

    let protocol_envelope_key = matches!(
        key,
        "id" | "call_id"
            | "name"
            | "namespace"
            | "role"
            | "status"
            | "phase"
            | "author"
            | "recipient"
    );
    protocol_envelope_key && (is_top_level_input_item_path(path) || is_tool_search_tool_path(path))
}

fn is_tool_search_tool_path(path: &[PathPart]) -> bool {
    matches!(
        path,
        [
            PathPart::Key(input),
            PathPart::Index(_),
            PathPart::Key(tools),
            PathPart::Index(_)
        ] if input == "input" && tools == "tools"
    )
}

fn build_batches(locations: Vec<TextLocation>) -> Result<PreparedTextBatches> {
    let fragment_limit = MAX_GATEWAY_TEXT_CHARS
        .checked_sub(frame_marker_char_count())
        .context("privacy gateway frame markers exceed the request bound")?;
    let mut field_paths = Vec::with_capacity(locations.len());
    let mut fragments = Vec::new();
    for (field_index, location) in locations.into_iter().enumerate() {
        field_paths.push(location.path);
        fragments.extend(
            split_text_fragments(&location.text, fragment_limit)
                .into_iter()
                .map(|text| TextFragment { field_index, text }),
        );
    }

    let mut batches: Vec<Vec<TextFragment>> = Vec::new();
    let mut current = Vec::new();
    for fragment in fragments {
        let single_size = framed_char_count(&fragment.text, 0);
        if single_size > MAX_GATEWAY_TEXT_CHARS {
            bail!(
                "one protected Responses fragment exceeds the privacy gateway 20,000-character bound"
            );
        }
        let prospective = current
            .iter()
            .enumerate()
            .map(|(index, item): (usize, &TextFragment)| framed_char_count(&item.text, index))
            .sum::<usize>()
            + framed_char_count(&fragment.text, current.len());
        if prospective > MAX_GATEWAY_TEXT_CHARS && !current.is_empty() {
            batches.push(std::mem::take(&mut current));
        }
        current.push(fragment);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    Ok(PreparedTextBatches {
        batches,
        field_paths,
    })
}

fn split_text_fragments(text: &str, maximum_chars: usize) -> Vec<String> {
    if text.chars().count() <= maximum_chars {
        return vec![text.to_string()];
    }

    let mut remaining = text;
    let mut fragments = Vec::new();
    while remaining.chars().count() > maximum_chars {
        let hard_end = remaining
            .char_indices()
            .nth(maximum_chars)
            .map(|(index, _)| index)
            .unwrap_or(remaining.len());
        let candidate = &remaining[..hard_end];
        let preferred_window_start = candidate
            .char_indices()
            .nth(maximum_chars.saturating_sub(PREFERRED_SPLIT_WINDOW_CHARS))
            .map(|(index, _)| index)
            .unwrap_or(0);
        let split_at =
            preferred_fragment_boundary(candidate, preferred_window_start).unwrap_or(hard_end);
        fragments.push(remaining[..split_at].to_string());
        remaining = &remaining[split_at..];
    }
    if !remaining.is_empty() {
        fragments.push(remaining.to_string());
    }
    fragments
}

fn preferred_fragment_boundary(text: &str, minimum: usize) -> Option<usize> {
    let boundary_after = |index: usize, character: char| index + character.len_utf8();
    if let Some(index) = text.rfind("\n\n")
        && index + 2 >= minimum
    {
        return Some(index + 2);
    }
    if let Some(index) = text.rfind('\n')
        && index + 1 >= minimum
    {
        return Some(index + 1);
    }
    if let Some((index, character)) = text
        .char_indices()
        .rev()
        .find(|(index, character)| *index >= minimum && matches!(*character, '.' | '?' | '!' | ';'))
    {
        return Some(boundary_after(index, character));
    }
    text.char_indices()
        .rev()
        .find(|(index, character)| *index >= minimum && character.is_whitespace())
        .map(|(index, character)| boundary_after(index, character))
}

fn frame_batch(batch: &[TextFragment]) -> Result<String> {
    if batch.len() > MAX_FRAME_COUNT {
        bail!("privacy gateway request contains too many protected text fields");
    }
    let mut framed = String::new();
    for (index, location) in batch.iter().enumerate() {
        let (open, close) = frame_markers(index)?;
        if location.text.contains(&open) || location.text.contains(&close) {
            bail!("protected Responses field contains a reserved privacy frame marker");
        }
        framed.push_str(&open);
        framed.push_str(&location.text);
        framed.push_str(&close);
    }
    if framed.chars().count() > MAX_GATEWAY_TEXT_CHARS {
        bail!("framed privacy gateway request exceeds 20,000 characters");
    }
    Ok(framed)
}

fn unframe_batch(text: &str, expected: usize) -> Result<Vec<String>> {
    let mut cursor = 0;
    let mut values = Vec::with_capacity(expected);
    for index in 0..expected {
        let (open, close) = frame_markers(index)?;
        if !text[cursor..].starts_with(&open) {
            bail!("privacy gateway changed a protected request frame marker");
        }
        cursor += open.len();
        let relative_end = text[cursor..]
            .find(&close)
            .context("privacy gateway removed a protected request frame marker")?;
        values.push(text[cursor..cursor + relative_end].to_string());
        cursor += relative_end + close.len();
    }
    if cursor != text.len() {
        bail!("privacy gateway returned unexpected data outside protected request frames");
    }
    Ok(values)
}

fn frame_markers(index: usize) -> Result<(String, String)> {
    if index >= MAX_FRAME_COUNT {
        bail!("privacy frame index exceeds its private-use character range");
    }
    let index_character =
        char::from_u32(0xE100 + index as u32).context("privacy frame index is invalid")?;
    Ok((
        format!("\u{E000}\u{E001}{index_character}\u{E002}"),
        format!("\u{E003}\u{E004}{index_character}\u{E005}"),
    ))
}

fn framed_char_count(text: &str, index: usize) -> usize {
    let marker_chars = if index < MAX_FRAME_COUNT {
        frame_marker_char_count()
    } else {
        usize::MAX
    };
    text.chars().count().saturating_add(marker_chars)
}

const fn frame_marker_char_count() -> usize {
    8
}

fn text_at_path<'a>(value: &'a Value, path: &[PathPart]) -> Result<&'a str> {
    let mut current = value;
    for part in path {
        current = match part {
            PathPart::Key(key) => current
                .as_object()
                .and_then(|object| object.get(key))
                .context("privacy text path no longer exists")?,
            PathPart::Index(index) => current
                .as_array()
                .and_then(|array| array.get(*index))
                .context("privacy text index no longer exists")?,
        };
    }
    current
        .as_str()
        .context("privacy text path no longer points to a string")
}

fn set_text_at_path(value: &mut Value, path: &[PathPart], text: String) -> Result<()> {
    let mut current = value;
    for part in path {
        current = match part {
            PathPart::Key(key) => current
                .as_object_mut()
                .and_then(|object| object.get_mut(key))
                .context("privacy text path no longer exists")?,
            PathPart::Index(index) => current
                .as_array_mut()
                .and_then(|array| array.get_mut(*index))
                .context("privacy text index no longer exists")?,
        };
    }
    if !current.is_string() {
        bail!("privacy text path no longer points to a string");
    }
    *current = Value::String(text);
    Ok(())
}

fn string_pointers(value: &Value) -> Vec<Vec<PathPart>> {
    fn collect(value: &Value, path: &mut Vec<PathPart>, output: &mut Vec<Vec<PathPart>>) {
        match value {
            Value::String(_) => output.push(path.clone()),
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    path.push(PathPart::Index(index));
                    collect(value, path, output);
                    path.pop();
                }
            }
            Value::Object(values) => {
                for (key, value) in values {
                    path.push(PathPart::Key(key.clone()));
                    collect(value, path, output);
                    path.pop();
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
    let mut output = Vec::new();
    collect(value, &mut Vec::new(), &mut output);
    output
}

fn normalize_match_value(value: &str) -> String {
    value
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn contains_any_match(text: &str, matches: &[String]) -> bool {
    let normalized = text.to_lowercase();
    matches
        .iter()
        .any(|value| normalized.contains(&value.to_lowercase()))
}

fn pending_suffix_start(text: &str, patterns: &[String]) -> usize {
    let mut hold_from = text.len();
    for start in text.char_indices().map(|(index, _)| index) {
        let suffix = &text[start..];
        if suffix.is_empty() || !candidate_word_boundary(text, start) {
            continue;
        }
        let suffix_lower = suffix.to_lowercase();
        if patterns.iter().any(|pattern| {
            let pattern_lower = pattern.to_lowercase();
            suffix_lower.len() < pattern_lower.len() && pattern_lower.starts_with(&suffix_lower)
        }) {
            hold_from = hold_from.min(start);
        }
    }
    hold_from
}

fn candidate_word_boundary(text: &str, start: usize) -> bool {
    let Some(current) = text[start..].chars().next() else {
        return false;
    };
    if !current.is_alphanumeric() || start == 0 {
        return true;
    }
    !text[..start]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric)
}

fn split_char_chunks(text: &str, maximum_chars: usize) -> Vec<&str> {
    if text.chars().count() <= maximum_chars {
        return vec![text];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut count = 0;
    for (index, _) in text.char_indices() {
        if count == maximum_chars {
            chunks.push(&text[start..index]);
            start = index;
            count = 0;
        }
        count += 1;
    }
    chunks.push(&text[start..]);
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    const TEST_MAPPING_ID: &str = "43e5c8ab-acde-43ee-86ff-ea6940c9fc11";
    const TEST_CAPABILITY: &str = "abcdefghijklmnopqrstuvwxyz0123456789_-ABCDE";

    fn test_gateway(base_url: &str, timeout: Duration) -> PrivacyGateway {
        test_gateway_with_mode(base_url, timeout, GatewayMode::Pseudonymize)
    }

    fn test_gateway_with_mode(
        base_url: &str,
        timeout: Duration,
        mode: GatewayMode,
    ) -> PrivacyGateway {
        let client = reqwest::Client::builder().timeout(timeout).build().unwrap();
        PrivacyGateway {
            state: Arc::new(GatewayState::Enabled(Arc::new(GatewayConfig {
                mode,
                base_url: Url::parse(base_url).unwrap(),
                bearer_token: "test-token".to_string(),
                data_classification: "synthetic_demo".to_string(),
                language: "en".to_string(),
                locale: "en_GB".to_string(),
                ttl_seconds: 300,
                timeout_ms: timeout.as_millis().try_into().unwrap_or(u64::MAX),
                client,
            }))),
            context_id: "synthetic-test-context".to_string(),
        }
    }

    fn pseudonymize_response(request: &wiremock::Request) -> ResponseTemplate {
        let request_body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            request_body["date_policy"],
            Value::String("plausible_keyed_shift".to_string())
        );
        let source = request_body["text"].as_str().unwrap();
        let pseudonymized = source
            .replace("Alice Stone", "Ava Woods")
            .replace("alice@example.invalid", "ava@example.invalid");
        ResponseTemplate::new(200).set_body_json(json!({
            "schema_version": "reversible-pseudonymization-v3",
            "data_classification": "synthetic_demo",
            "pseudonymized_text": pseudonymized,
            "mapping_id": TEST_MAPPING_ID,
            "restoration_capability": TEST_CAPABILITY,
            "expires_at": (Utc::now() + TimeDelta::minutes(5)).to_rfc3339(),
            "streaming_restoration_matches": [
                {"value": "Ava Woods", "source_fingerprint": "a".repeat(64)},
                {"value": "ava@example.invalid", "source_fingerprint": "b".repeat(64)}
            ]
        }))
    }

    fn deep_pseudonymize_response(request: &wiremock::Request) -> ResponseTemplate {
        let request_body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            request_body["date_policy"],
            Value::String("irreversible_placeholder".to_string())
        );
        let source = request_body["text"].as_str().unwrap();
        let pseudonymized = source.replace("Alice Stone", "Ava Woods");
        ResponseTemplate::new(200).set_body_json(json!({
            "schema_version": "deep-pseudonymization-v1",
            "data_classification": "synthetic_demo",
            "pseudonymized_text": pseudonymized,
            "mapping_id": TEST_MAPPING_ID,
            "restoration_capability": TEST_CAPABILITY,
            "expires_at": (Utc::now() + TimeDelta::minutes(5)).to_rfc3339(),
            "streaming_restoration_matches": [
                {"value": "Ava Woods", "source_fingerprint": "a".repeat(64)}
            ]
        }))
    }

    fn restore_response(request: &wiremock::Request) -> ResponseTemplate {
        let request_body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            request_body["match_mode"],
            Value::String("exact_and_registered_aliases".to_string())
        );
        let restored = request_body["text"]
            .as_str()
            .unwrap()
            .replace("Ava Woods", "Alice Stone")
            .replace("ava@example.invalid", "alice@example.invalid");
        let purge = request_body["purge_after_restore"].as_bool().unwrap();
        ResponseTemplate::new(200).set_body_json(json!({
            "schema_version": "authorized-restoration-v2",
            "data_classification": "synthetic_demo",
            "restored_text": restored,
            "mapping_id": TEST_MAPPING_ID,
            "mapping_purged": purge
        }))
    }

    fn deep_restore_response(request: &wiremock::Request) -> ResponseTemplate {
        let request_body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            request_body["match_mode"],
            Value::String("exact_values".to_string())
        );
        let restored = request_body["text"]
            .as_str()
            .unwrap()
            .replace("Ava Woods", "Alice Stone");
        let purge = request_body["purge_after_restore"].as_bool().unwrap();
        ResponseTemplate::new(200).set_body_json(json!({
            "schema_version": "authorized-restoration-v2",
            "data_classification": "synthetic_demo",
            "restored_text": restored,
            "mapping_id": TEST_MAPPING_ID,
            "mapping_purged": purge
        }))
    }

    #[test]
    fn disabled_mode_does_not_require_any_other_configuration() {
        let gateway = PrivacyGateway {
            state: Arc::new(GatewayState::Disabled),
            context_id: "test".to_string(),
        };
        assert!(!gateway.enabled());
    }

    #[test]
    fn feature_switch_defaults_off_and_rejects_ambiguous_values() {
        assert!(!parse_enable_value(None).unwrap());
        assert!(!parse_enable_value(Some("false")).unwrap());
        assert!(parse_enable_value(Some("ON")).unwrap());
        assert!(parse_enable_value(Some("enabled")).is_err());
    }

    #[test]
    fn protection_mode_defaults_to_normal_and_bounds_deep_ttl() {
        assert_eq!(GatewayMode::parse(None).unwrap(), GatewayMode::Pseudonymize);
        assert_eq!(
            GatewayMode::parse(Some("deep-pseudonymize")).unwrap(),
            GatewayMode::DeepPseudonymize
        );
        assert!(GatewayMode::parse(Some("deep")).is_err());
        assert_eq!(GatewayMode::Pseudonymize.maximum_ttl_seconds(), 3_600);
        assert_eq!(GatewayMode::DeepPseudonymize.maximum_ttl_seconds(), 300);
        assert_eq!(GatewayMode::DeepPseudonymize.default_ttl_seconds(), 300);
    }

    #[test]
    fn mapping_expiry_must_stay_within_the_requested_ttl() {
        let gateway = test_gateway("http://127.0.0.1:9", Duration::from_secs(1));
        let response = PseudonymizeResponse {
            schema_version: "reversible-pseudonymization-v3".to_string(),
            data_classification: "synthetic_demo".to_string(),
            pseudonymized_text: "Ava Woods".to_string(),
            mapping_id: Uuid::parse_str(TEST_MAPPING_ID).unwrap(),
            restoration_capability: TEST_CAPABILITY.to_string(),
            expires_at: Utc::now() + TimeDelta::seconds(306),
            streaming_restoration_matches: Vec::new(),
        };

        let error = mapping_from_response(gateway.config().unwrap(), &response).unwrap_err();

        assert!(error.to_string().contains("expiry"));
    }

    #[test]
    fn streaming_suffix_buffer_holds_only_word_boundary_prefixes() {
        let patterns = vec!["Alice Stone".to_string()];
        assert_eq!(pending_suffix_start("Hello Ali", &patterns), 6);
        assert_eq!(pending_suffix_start("invalid", &patterns), "invalid".len());
        assert_eq!(pending_suffix_start("data", &patterns), "data".len());
    }

    #[test]
    fn request_collection_excludes_structural_encrypted_and_static_tool_values() {
        let body = serde_json::json!({
            "instructions": "Ask Alice Stone",
            "input": [{
                "type": "function_call",
                "name": "lookup_student",
                "arguments": "{\"email\":\"alice@example.invalid\"}",
                "call_id": "call-Alice-Stone",
                "encrypted_content": "Alice Stone"
            }],
            "tools": [{"name": "lookup_student", "description": "Find Alice Stone"}]
        });
        let locations = request_text_locations(&body).unwrap();
        let values = locations
            .iter()
            .map(|location| location.text.as_str())
            .collect::<Vec<_>>();
        assert!(values.contains(&"Ask Alice Stone"));
        assert!(values.contains(&"{\"email\":\"alice@example.invalid\"}"));
        assert!(!values.contains(&"Find Alice Stone"));
        assert!(!values.contains(&"lookup_student"));
        assert!(!values.contains(&"call-Alice-Stone"));
    }

    #[test]
    fn request_collection_protects_dynamic_nested_result_text_but_not_image_bytes() {
        let body = serde_json::json!({
            "input": [
                {
                    "type": "tool_search_output",
                    "call_id": "call-1",
                    "status": "completed",
                    "execution": "client",
                    "tools": [{
                        "name": "lookup",
                        "description": "Lookup Alice Stone",
                        "metadata": {"result": "alice@example.invalid"}
                    }]
                },
                {
                    "type": "image_generation_call",
                    "id": "image-1",
                    "status": "completed",
                    "revised_prompt": "Portrait of Alice Stone",
                    "result": "base64-image-bytes"
                }
            ]
        });
        let locations = request_text_locations(&body).unwrap();
        let values = locations
            .iter()
            .map(|location| location.text.as_str())
            .collect::<Vec<_>>();

        assert!(values.contains(&"Lookup Alice Stone"));
        assert!(values.contains(&"alice@example.invalid"));
        assert!(values.contains(&"Portrait of Alice Stone"));
        assert!(!values.contains(&"base64-image-bytes"));
    }

    #[test]
    fn request_collection_protects_identity_named_fields_inside_dynamic_metadata() {
        let body = serde_json::json!({
            "input": [{
                "type": "tool_search_output",
                "call_id": "call-1",
                "status": "completed",
                "tools": [{
                    "name": "lookup",
                    "description": "Lookup directory entry",
                    "metadata": {
                        "id": "student-alice@example.invalid",
                        "name": "Alice Stone",
                        "author": "Alice Stone",
                        "recipient": "Bob Stone"
                    }
                }]
            }]
        });
        let locations = request_text_locations(&body).unwrap();
        let values = locations
            .iter()
            .map(|location| location.text.as_str())
            .collect::<Vec<_>>();

        assert!(!values.contains(&"call-1"));
        assert!(!values.contains(&"lookup"));
        assert!(values.contains(&"student-alice@example.invalid"));
        assert!(values.contains(&"Alice Stone"));
        assert!(values.contains(&"Bob Stone"));
    }

    #[test]
    fn frames_round_trip_multiple_fields_without_text_changes() {
        let batch = vec![
            TextFragment {
                field_index: 0,
                text: "Alice Stone".to_string(),
            },
            TextFragment {
                field_index: 1,
                text: "alice@example.invalid".to_string(),
            },
        ];
        let framed = frame_batch(&batch).unwrap();
        assert_eq!(
            unframe_batch(&framed, 2).unwrap(),
            vec!["Alice Stone", "alice@example.invalid"]
        );
    }

    #[test]
    fn oversized_fields_are_segmented_and_reconstructed_losslessly() {
        let source = format!("{}\n\n{}", "A".repeat(19_500), "B".repeat(2_000));
        let prepared = build_batches(vec![TextLocation {
            path: vec![PathPart::Key("instructions".to_string())],
            text: source.clone(),
        }])
        .unwrap();
        let fragments = prepared.batches.iter().flatten().collect::<Vec<_>>();
        let reconstructed = fragments
            .iter()
            .map(|fragment| fragment.text.as_str())
            .collect::<String>();

        assert!(prepared.batches.len() >= 2);
        assert_eq!(reconstructed, source);
        assert!(fragments[0].text.ends_with("\n\n"));
        assert!(
            prepared
                .batches
                .iter()
                .all(|batch| frame_batch(batch).unwrap().chars().count() <= MAX_GATEWAY_TEXT_CHARS)
        );
    }

    #[tokio::test]
    async fn oversized_outbound_field_round_trips_through_bounded_gateway_calls() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/pseudonymize"))
            .respond_with(pseudonymize_response)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/de-pseudonymize"))
            .respond_with(restore_response)
            .expect(2)
            .mount(&server)
            .await;
        let gateway = test_gateway(&server.uri(), Duration::from_secs(1));
        let source = format!(
            "{}\n\nAlice Stone uses alice@example.invalid. {}",
            "A".repeat(19_500),
            "B".repeat(2_000)
        );

        let prepared = gateway
            .prepare_json_body(json!({"instructions": source.clone()}))
            .await
            .unwrap();

        assert_eq!(
            prepared.body["instructions"].as_str().unwrap(),
            source
                .replace("Alice Stone", "Ava Woods")
                .replace("alice@example.invalid", "ava@example.invalid")
        );
        let mut session = prepared.session.unwrap();
        session.abort().await;
    }

    #[tokio::test]
    async fn outbound_fields_and_streamed_response_round_trip_through_gateway() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/pseudonymize"))
            .respond_with(pseudonymize_response)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/de-pseudonymize"))
            .respond_with(restore_response)
            .expect(3)
            .mount(&server)
            .await;
        let gateway = test_gateway(&server.uri(), Duration::from_secs(1));

        let prepared = gateway
            .prepare_json_body(json!({
                "instructions": "Help Alice Stone",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Email alice@example.invalid"}]
                }],
                "tools": [{"name": "lookup", "description": "Find Alice Stone"}]
            }))
            .await
            .unwrap();
        assert_eq!(prepared.body["instructions"], "Help Ava Woods");
        assert_eq!(
            prepared.body["input"][0]["content"][0]["text"],
            "Email ava@example.invalid"
        );
        assert_eq!(prepared.body["tools"][0]["description"], "Find Alice Stone");

        let mut session = prepared.session.unwrap();
        let first = session
            .transform_event(ResponseEvent::OutputTextDelta("Welcome Ava Wo".to_string()))
            .await
            .unwrap();
        assert!(matches!(
            first.as_slice(),
            [ResponseEvent::OutputTextDelta(delta)] if delta == "Welcome "
        ));
        let second = session
            .transform_event(ResponseEvent::OutputTextDelta("ods".to_string()))
            .await
            .unwrap();
        assert!(matches!(
            second.as_slice(),
            [ResponseEvent::OutputTextDelta(delta)] if delta == "Alice Stone"
        ));
        let tool_first = session
            .transform_event(ResponseEvent::ToolCallInputDelta {
                item_id: "item-test".to_string(),
                call_id: Some("call-test".to_string()),
                delta: r#"{"email":"ava@example."#.to_string(),
            })
            .await
            .unwrap();
        assert!(matches!(
            tool_first.as_slice(),
            [ResponseEvent::ToolCallInputDelta { delta, .. }] if delta == r#"{"email":""#
        ));
        let tool_second = session
            .transform_event(ResponseEvent::ToolCallInputDelta {
                item_id: "item-test".to_string(),
                call_id: Some("call-test".to_string()),
                delta: r#"invalid"}"#.to_string(),
            })
            .await
            .unwrap();
        assert!(matches!(
            tool_second.as_slice(),
            [ResponseEvent::ToolCallInputDelta { delta, .. }]
                if delta == r#"alice@example.invalid"}"#
        ));
        let completed = session
            .transform_event(ResponseEvent::Completed {
                response_id: "response-test".to_string(),
                token_usage: None,
                end_turn: Some(true),
            })
            .await
            .unwrap();
        assert!(matches!(
            completed.last(),
            Some(ResponseEvent::Completed { response_id, .. }) if response_id == "response-test"
        ));
    }

    #[tokio::test]
    async fn trusted_catalog_instructions_are_skipped_but_dynamic_and_custom_text_is_protected() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/pseudonymize"))
            .respond_with(pseudonymize_response)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/de-pseudonymize"))
            .respond_with(restore_response)
            .expect(2)
            .mount(&server)
            .await;
        let gateway = test_gateway(&server.uri(), Duration::from_secs(1));
        let trusted_catalog_instructions = "Catalog defaults for Alice Stone";

        let prepared = gateway
            .prepare_json_body_with_trusted_instructions(
                json!({
                    "instructions": trusted_catalog_instructions,
                    "input": [{
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "Email alice@example.invalid"
                        }]
                    }]
                }),
                trusted_catalog_instructions,
            )
            .await
            .unwrap();
        assert_eq!(
            prepared.body["instructions"],
            Value::String(trusted_catalog_instructions.to_string())
        );
        assert_eq!(
            prepared.body["input"][0]["content"][0]["text"],
            "Email ava@example.invalid"
        );
        prepared.session.unwrap().abort().await;

        let prepared = gateway
            .prepare_json_body_with_trusted_instructions(
                json!({"instructions": "Custom instructions for Alice Stone"}),
                trusted_catalog_instructions,
            )
            .await
            .unwrap();
        assert_eq!(
            prepared.body["instructions"],
            "Custom instructions for Ava Woods"
        );
        prepared.session.unwrap().abort().await;
    }

    #[tokio::test]
    async fn deep_mode_uses_deep_route_and_exact_stream_restoration() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/deep-pseudonymize"))
            .respond_with(deep_pseudonymize_response)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/de-pseudonymize"))
            .respond_with(deep_restore_response)
            .expect(2)
            .mount(&server)
            .await;
        let gateway = test_gateway_with_mode(
            &server.uri(),
            Duration::from_secs(1),
            GatewayMode::DeepPseudonymize,
        );

        let prepared = gateway
            .prepare_json_body(json!({"instructions": "Help Alice Stone"}))
            .await
            .unwrap();
        assert_eq!(prepared.body["instructions"], "Help Ava Woods");
        let mut session = prepared.session.unwrap();
        let restored = session
            .transform_event(ResponseEvent::OutputTextDelta("Ava Woods".to_string()))
            .await
            .unwrap();
        assert!(matches!(
            restored.as_slice(),
            [ResponseEvent::OutputTextDelta(delta)] if delta == "Alice Stone"
        ));
        session
            .transform_event(ResponseEvent::Completed {
                response_id: "response-deep-test".to_string(),
                token_usage: None,
                end_turn: Some(true),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn gateway_failure_and_timeout_block_before_provider_transport() {
        let failure_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/pseudonymize"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&failure_server)
            .await;
        let failure_gateway = test_gateway(&failure_server.uri(), Duration::from_secs(1));
        let failure = failure_gateway
            .prepare_json_body(json!({"instructions": "Alice Stone"}))
            .await
            .unwrap_err();
        assert!(failure.to_string().contains("HTTP 503"));

        let timeout_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/pseudonymize"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_json(json!({})),
            )
            .mount(&timeout_server)
            .await;
        let timeout_gateway = test_gateway(&timeout_server.uri(), Duration::from_millis(20));
        let timeout = timeout_gateway
            .prepare_json_body(json!({"instructions": "Alice Stone"}))
            .await
            .unwrap_err();
        assert!(timeout.to_string().contains("timed out after 20 ms"));
    }

    #[tokio::test]
    async fn expired_mapping_never_releases_a_provider_alias() {
        let config = Arc::new(GatewayConfig {
            mode: GatewayMode::Pseudonymize,
            base_url: Url::parse("http://127.0.0.1:9").unwrap(),
            bearer_token: "test-token".to_string(),
            data_classification: "synthetic_demo".to_string(),
            language: "en".to_string(),
            locale: "en_GB".to_string(),
            ttl_seconds: 60,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            client: reqwest::Client::new(),
        });
        let mut session = GatewayRequestSession {
            config,
            mappings: vec![GatewayMapping {
                mapping_id: Uuid::parse_str(TEST_MAPPING_ID).unwrap(),
                restoration_capability: TEST_CAPABILITY.to_string(),
                expires_at: Utc::now() - TimeDelta::seconds(1),
                match_values: vec!["Ava Woods".to_string()],
                purged: false,
            }],
            match_values: vec!["Ava Woods".to_string()],
            pending: BTreeMap::new(),
        };

        let error = session
            .transform_event(ResponseEvent::OutputTextDelta("Ava Woods".to_string()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("expired"));
    }
}
