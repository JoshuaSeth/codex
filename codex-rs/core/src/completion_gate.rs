use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::CompletionGateBlockedStopEvent;
use codex_protocol::protocol::CompletionGateDecisionEvent;
use codex_protocol::protocol::CompletionGateErrorEvent;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::time::timeout;
use tracing::warn;

use crate::AuthManager;
use crate::Prompt;
use crate::client::ModelClient;
use crate::codex::TurnContext;
use crate::compact::content_items_to_text;
use crate::config::CompletionGateConfig;
use crate::contextual_user_message::COMPLETION_GATE_FEEDBACK_FRAGMENT;
use crate::error::CodexErr;
use crate::error::Result as CodexResult;
use crate::models_manager::manager::ModelsManager;
use crate::util::backoff;

const SYSTEM_PROMPT: &str = "You are the completion gate for a Codex coding session.\n\
Decide only whether the candidate assistant response is allowed to stop under the provided completion criterion.\n\
Base your decision only on the supplied conversation context.\n\
Do not speculate about hidden state, tools, files, or web content that are not present in the request.\n\
Return strict JSON matching the requested schema.";

const DEFAULT_FAILURE_CONTINUE_PROMPT: &str = "The completion gate could not verify that the task is complete. Re-check the task against the completion criterion, continue the work, and only stop once the criterion is clearly satisfied.";

#[derive(Debug, Clone)]
pub(crate) struct AllowStopDecision {
    pub(crate) event: CompletionGateDecisionEvent,
}

#[derive(Debug, Clone)]
pub(crate) struct DenyStopDecision {
    pub(crate) decision_event: CompletionGateDecisionEvent,
    pub(crate) blocked_event: CompletionGateBlockedStopEvent,
    pub(crate) continuation: ResponseInputItem,
}

#[derive(Debug, Clone)]
pub(crate) struct CompletionGateFailure {
    pub(crate) event: CompletionGateErrorEvent,
    pub(crate) continuation: ResponseInputItem,
}

#[derive(Debug, Clone)]
pub(crate) enum CompletionGateOutcome {
    Allow(AllowStopDecision),
    Deny(DenyStopDecision),
    Error(CompletionGateFailure),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionGateDecisionPayload {
    allow_stop: bool,
    reason: String,
    missing_requirements: Vec<String>,
    continue_prompt: String,
    #[serde(rename = "evidence")]
    _evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptEntry {
    kind: TranscriptEntryKind,
    phase: Option<MessagePhase>,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptEntryKind {
    User,
    Assistant,
}

pub(crate) async fn evaluate_candidate_stop(
    auth_manager: &Arc<AuthManager>,
    models_manager: &Arc<ModelsManager>,
    turn_context: &Arc<TurnContext>,
    conversation_id: ThreadId,
    history: &[ResponseItem],
    boundary_history_len: Option<usize>,
    gate: &CompletionGateConfig,
    candidate_response: &str,
) -> CompletionGateOutcome {
    let criteria_hash = criteria_hash(&gate.criteria);
    let judge_model = gate
        .judge_model
        .clone()
        .unwrap_or_else(|| turn_context.model_info.slug.clone());
    let request_xml = build_request_xml(history, boundary_history_len, gate, candidate_response);

    let Some(request_xml) = request_xml else {
        let latency_ms = 0;
        let event = CompletionGateErrorEvent {
            thread_id: conversation_id.to_string(),
            turn_id: turn_context.sub_id.clone(),
            criteria_hash,
            judge_model,
            request_id: None,
            latency_ms,
            message: "completion gate could not find any conversation context to judge".to_string(),
        };
        return CompletionGateOutcome::Error(CompletionGateFailure {
            continuation: failure_continuation_message(
                "Completion gate failed closed because no conversation context was available."
                    .to_string(),
            ),
            event,
        });
    };

    match run_judge_with_retries(
        auth_manager,
        models_manager,
        turn_context,
        gate,
        &judge_model,
        &request_xml,
    )
    .await
    {
        Ok((payload, latency_ms)) => {
            let decision_event = CompletionGateDecisionEvent {
                thread_id: conversation_id.to_string(),
                turn_id: turn_context.sub_id.clone(),
                criteria_hash: criteria_hash.clone(),
                judge_model: judge_model.clone(),
                request_id: None,
                latency_ms,
                allow_stop: payload.allow_stop,
                reason: payload.reason.clone(),
            };

            if payload.allow_stop {
                CompletionGateOutcome::Allow(AllowStopDecision {
                    event: decision_event,
                })
            } else {
                let continue_prompt = payload.continue_prompt.trim().to_string();
                if continue_prompt.is_empty() {
                    let event = CompletionGateErrorEvent {
                        thread_id: conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        criteria_hash,
                        judge_model,
                        request_id: None,
                        latency_ms,
                        message: "completion gate denied stop without a continuation prompt"
                            .to_string(),
                    };
                    CompletionGateOutcome::Error(CompletionGateFailure {
                        continuation: failure_continuation_message(
                            "Completion gate failed closed because the judge denied stop without supplying a continuation prompt.".to_string(),
                        ),
                        event,
                    })
                } else {
                    let blocked_event = CompletionGateBlockedStopEvent {
                        thread_id: conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        criteria_hash,
                        judge_model,
                        request_id: None,
                        latency_ms,
                        reason: format!(
                            "{}{}",
                            payload.reason,
                            format_missing_requirements(&payload.missing_requirements)
                        ),
                        continue_prompt: continue_prompt.clone(),
                    };
                    CompletionGateOutcome::Deny(DenyStopDecision {
                        continuation: continuation_message(
                            blocked_event.reason.clone(),
                            continue_prompt,
                        ),
                        decision_event,
                        blocked_event,
                    })
                }
            }
        }
        Err((message, latency_ms)) => CompletionGateOutcome::Error(CompletionGateFailure {
            continuation: failure_continuation_message(format!(
                "{DEFAULT_FAILURE_CONTINUE_PROMPT}\nCompletion gate error: {message}"
            )),
            event: CompletionGateErrorEvent {
                thread_id: conversation_id.to_string(),
                turn_id: turn_context.sub_id.clone(),
                criteria_hash,
                judge_model,
                request_id: None,
                latency_ms,
                message,
            },
        }),
    }
}

fn build_request_xml(
    history: &[ResponseItem],
    boundary_history_len: Option<usize>,
    gate: &CompletionGateConfig,
    candidate_response: &str,
) -> Option<String> {
    let original_user_request = history.iter().find_map(first_user_message_text)?;
    let window_entries = collect_window_entries(
        history,
        boundary_history_len,
        gate.max_assistant_messages,
        gate.max_user_messages,
    );

    let mut xml = String::from("<completion_gate_request>\n");
    xml.push_str("  <criteria>\n");
    xml.push_str(&indent_xml(&xml_escape(&gate.criteria), 4));
    xml.push_str("\n  </criteria>\n");
    xml.push_str("  <original_user_request>\n");
    xml.push_str(&indent_xml(&xml_escape(&original_user_request), 4));
    xml.push_str("\n  </original_user_request>\n");
    xml.push_str("  <judge_window mode=\"since_last_judge_or_bounded_recent\">\n");
    for (idx, entry) in window_entries.iter().enumerate() {
        let tag = match entry.kind {
            TranscriptEntryKind::User => "user_message",
            TranscriptEntryKind::Assistant => "assistant_message",
        };
        let phase_attr = entry
            .phase
            .as_ref()
            .map(|phase| format!(" phase=\"{}\"", phase_label(phase)))
            .unwrap_or_default();
        xml.push_str(&format!("    <{tag} index=\"{}\"{phase_attr}>\n", idx + 1));
        xml.push_str(&indent_xml(&xml_escape(&entry.text), 6));
        xml.push_str(&format!("\n    </{tag}>\n"));
    }
    xml.push_str("  </judge_window>\n");
    xml.push_str("  <candidate_final_response>\n");
    xml.push_str(&indent_xml(&xml_escape(candidate_response), 4));
    xml.push_str("\n  </candidate_final_response>\n");
    xml.push_str("</completion_gate_request>");
    Some(xml)
}

fn collect_window_entries(
    history: &[ResponseItem],
    boundary_history_len: Option<usize>,
    max_assistant_messages: usize,
    max_user_messages: usize,
) -> Vec<TranscriptEntry> {
    if let Some(boundary) = boundary_history_len
        && boundary <= history.len()
    {
        return history[boundary..]
            .iter()
            .filter_map(transcript_entry_from_item)
            .collect();
    }

    let mut user_count = 0usize;
    let mut assistant_count = 0usize;
    let mut selected = Vec::new();
    for item in history.iter().rev() {
        let Some(entry) = transcript_entry_from_item(item) else {
            continue;
        };
        match entry.kind {
            TranscriptEntryKind::User => {
                if user_count >= max_user_messages {
                    continue;
                }
                user_count += 1;
            }
            TranscriptEntryKind::Assistant => {
                if assistant_count >= max_assistant_messages {
                    continue;
                }
                assistant_count += 1;
            }
        }
        selected.push(entry);
        if user_count >= max_user_messages && assistant_count >= max_assistant_messages {
            break;
        }
    }
    selected.reverse();
    selected
}

fn transcript_entry_from_item(item: &ResponseItem) -> Option<TranscriptEntry> {
    match item {
        ResponseItem::Message {
            role,
            content,
            phase,
            ..
        } if role == "assistant" => {
            let text = content_items_to_text(content)?;
            let text = text.trim().to_string();
            if text.is_empty() {
                return None;
            }
            Some(TranscriptEntry {
                kind: TranscriptEntryKind::Assistant,
                phase: phase.clone(),
                text,
            })
        }
        ResponseItem::Message { role, content, .. } if role == "user" => {
            if crate::event_mapping::is_contextual_user_message_content(content) {
                return None;
            }
            let text = content_items_to_text(content)?;
            let text = text.trim().to_string();
            if text.is_empty() {
                return None;
            }
            Some(TranscriptEntry {
                kind: TranscriptEntryKind::User,
                phase: None,
                text,
            })
        }
        _ => None,
    }
}

fn first_user_message_text(item: &ResponseItem) -> Option<String> {
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    if role != "user" || crate::event_mapping::is_contextual_user_message_content(content) {
        return None;
    }
    content_items_to_text(content).map(|text| text.trim().to_string())
}

async fn run_judge_with_retries(
    auth_manager: &Arc<AuthManager>,
    models_manager: &Arc<ModelsManager>,
    turn_context: &Arc<TurnContext>,
    gate: &CompletionGateConfig,
    judge_model: &str,
    request_xml: &str,
) -> Result<(CompletionGateDecisionPayload, u64), (String, u64)> {
    let start = Instant::now();
    let max_attempts = u64::from(gate.max_retries.max(1));
    let mut attempts = 0u64;
    loop {
        attempts += 1;
        match run_single_judge_attempt(
            auth_manager,
            models_manager,
            turn_context,
            gate,
            judge_model,
            request_xml,
        )
        .await
        {
            Ok(payload) => {
                let latency_ms = saturating_millis(start.elapsed());
                return Ok((payload, latency_ms));
            }
            Err(err) => {
                if attempts >= max_attempts || !should_retry_judge_error(&err) {
                    let latency_ms = saturating_millis(start.elapsed());
                    return Err((err.to_string(), latency_ms));
                }
                let delay = backoff(attempts);
                warn!(
                    turn_id = %turn_context.sub_id,
                    attempt = attempts,
                    max_attempts,
                    error = %err,
                    ?delay,
                    "completion gate judge failed; retrying"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

async fn run_single_judge_attempt(
    auth_manager: &Arc<AuthManager>,
    models_manager: &Arc<ModelsManager>,
    turn_context: &Arc<TurnContext>,
    gate: &CompletionGateConfig,
    judge_model: &str,
    request_xml: &str,
) -> CodexResult<CompletionGateDecisionPayload> {
    let judge_provider = build_judge_provider(turn_context, gate);
    let mut judge_config = (*turn_context.config).clone();
    judge_config.model = Some(judge_model.to_string());
    let judge_model_info = models_manager
        .get_model_info(judge_model, &judge_config)
        .await;
    let judge_client = ModelClient::new(
        Some(Arc::clone(auth_manager)),
        ThreadId::new(),
        judge_provider,
        turn_context.session_source.clone(),
        None,
        false,
        false,
        false,
        None,
    );
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: request_xml.to_string(),
            }],
            end_turn: None,
            phase: None,
        }],
        tools: Vec::new(),
        parallel_tool_calls: false,
        base_instructions: BaseInstructions {
            text: SYSTEM_PROMPT.to_string(),
        },
        personality: None,
        output_schema: Some(decision_schema()),
    };
    let mut client_session = judge_client.new_session();
    // Completion-gate requests are deliberately cheap judge calls. They run at
    // every candidate stop boundary, so asking for the session's normal
    // reasoning effort is wasteful, and hard-coding a legacy effort (for
    // example `minimal`) is unsafe because newer GPT-5.x judge models no longer
    // accept it.
    //
    // Keep this model-aware helper aligned with the live smoke test. Removing
    // it regresses the real failure mode that originally surfaced in this fork:
    // the judge request is rejected before the schema can be evaluated, the
    // gate fail-closes, and Codex loops on repeated continuation prompts even
    // though the candidate response is otherwise valid.
    let judge_reasoning_effort = preferred_judge_reasoning_effort(
        judge_model_info.default_reasoning_level,
        &judge_model_info.supported_reasoning_levels,
    );
    let mut stream = timeout(
        Duration::from_millis(gate.timeout_ms),
        client_session.stream(
            &prompt,
            &judge_model_info,
            &turn_context.otel_manager,
            judge_reasoning_effort,
            ReasoningSummaryConfig::None,
            None,
            None,
        ),
    )
    .await
    .map_err(|_| CodexErr::Stream("completion gate judge request timed out".to_string(), None))??;

    let mut raw_output = String::new();
    while let Some(event) = stream.next().await.transpose()? {
        match event {
            crate::ResponseEvent::OutputTextDelta(delta) => raw_output.push_str(&delta),
            crate::ResponseEvent::OutputItemDone(item) => {
                if raw_output.is_empty()
                    && let ResponseItem::Message { content, .. } = item
                    && let Some(text) = content_items_to_text(&content)
                {
                    raw_output.push_str(&text);
                }
            }
            crate::ResponseEvent::Completed { .. } => break,
            _ => {}
        }
    }

    let payload: CompletionGateDecisionPayload =
        serde_json::from_str(&raw_output).map_err(|err| {
            CodexErr::InvalidRequest(format!(
                "completion gate judge returned invalid JSON: {err}"
            ))
        })?;
    if !payload.allow_stop && payload.continue_prompt.trim().is_empty() {
        return Err(CodexErr::InvalidRequest(
            "completion gate judge denied stop without a continuation prompt".to_string(),
        ));
    }
    Ok(payload)
}

fn preferred_judge_reasoning_effort(
    default_reasoning_level: Option<ReasoningEffortConfig>,
    supported_reasoning_levels: &[ReasoningEffortPreset],
) -> Option<ReasoningEffortConfig> {
    let supported = supported_reasoning_levels
        .iter()
        .map(|preset| preset.effort)
        .collect::<Vec<_>>();

    for candidate in [
        ReasoningEffortConfig::Low,
        ReasoningEffortConfig::None,
        default_reasoning_level.unwrap_or_default(),
    ] {
        if supported.contains(&candidate) {
            return Some(candidate);
        }
    }

    supported.first().copied()
}

fn should_retry_judge_error(err: &CodexErr) -> bool {
    match err {
        // Invalid JSON/schema-like responses and malformed deny payloads are
        // semantic judge failures, not transport failures. Retrying them here
        // burns extra judge calls and can skip past the fail-closed path the
        // completion gate is supposed to take when the judge output is bad.
        //
        // Keep these non-retryable so the gate fails closed immediately and
        // the active Codex session receives the continuation prompt instead of
        // spinning additional judge attempts.
        CodexErr::Stream(message, _)
            if message.starts_with("completion gate judge returned invalid JSON:")
                || message == "completion gate judge denied stop without a continuation prompt" =>
        {
            false
        }
        _ => err.is_retryable(),
    }
}

fn build_judge_provider(
    turn_context: &TurnContext,
    gate: &CompletionGateConfig,
) -> crate::ModelProviderInfo {
    let mut provider = turn_context.provider.clone();
    provider.supports_websockets = false;
    if let Some(base_url) = gate.judge_base_url.clone() {
        provider.base_url = Some(base_url);
    }
    if let Some(env_key) = gate.judge_api_key_env.clone() {
        provider.env_key = Some(env_key);
    }
    provider.request_max_retries = Some(u64::from(gate.max_retries.max(1)));
    provider.stream_max_retries = Some(1);
    provider
}

fn continuation_message(reason: String, continue_prompt: String) -> ResponseInputItem {
    ResponseInputItem::Message {
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: COMPLETION_GATE_FEEDBACK_FRAGMENT.wrap(format!(
                "<decision>continue</decision>\n<reason>{}</reason>\n<continue_prompt>{}</continue_prompt>",
                xml_escape(&reason),
                xml_escape(&continue_prompt),
            )),
        }],
    }
}

fn failure_continuation_message(message: String) -> ResponseInputItem {
    continuation_message(
        "completion gate failed closed".to_string(),
        if message.trim().is_empty() {
            DEFAULT_FAILURE_CONTINUE_PROMPT.to_string()
        } else {
            message
        },
    )
}

pub(crate) fn criteria_hash(criteria: &str) -> String {
    let digest = codex_utils_cache::sha1_digest(criteria.as_bytes());
    let mut out = String::with_capacity(12);
    for byte in digest.iter().take(6) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn phase_label(phase: &MessagePhase) -> &'static str {
    match phase {
        MessagePhase::Commentary => "commentary",
        MessagePhase::FinalAnswer => "final_answer",
    }
}

fn decision_schema() -> Value {
    serde_json::from_str(include_str!(
        "../schemas/completion_gate_decision.schema.json"
    ))
    .expect("valid completion gate decision schema")
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn indent_xml(text: &str, spaces: usize) -> String {
    let indent = " ".repeat(spaces);
    text.lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_missing_requirements(missing_requirements: &[String]) -> String {
    if missing_requirements.is_empty() {
        String::new()
    } else {
        format!(
            " Missing requirements: {}.",
            missing_requirements.join("; ")
        )
    }
}

fn saturating_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contextual_user_message::COMPLETION_GATE_FEEDBACK_FRAGMENT;
    use pretty_assertions::assert_eq;

    #[test]
    fn criteria_hash_is_stable_and_short() {
        assert_eq!(criteria_hash("abc"), "a9993e364706");
    }

    #[test]
    fn failure_message_is_wrapped_as_contextual_fragment() {
        let ResponseInputItem::Message { content, .. } =
            failure_continuation_message("keep going".to_string())
        else {
            panic!("expected message");
        };
        let ContentItem::InputText { text } = &content[0] else {
            panic!("expected input text");
        };
        assert!(text.contains("<completion_gate_feedback>"));
        assert!(text.contains("keep going"));
    }

    #[test]
    fn collect_window_entries_prefers_items_after_last_judge_boundary() {
        let history = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "original request".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "older assistant update".to_string(),
                }],
                end_turn: None,
                phase: Some(MessagePhase::Commentary),
            },
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: COMPLETION_GATE_FEEDBACK_FRAGMENT
                        .wrap("<decision>continue</decision>".to_string()),
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "new assistant update".to_string(),
                }],
                end_turn: None,
                phase: Some(MessagePhase::FinalAnswer),
            },
        ];

        let entries = collect_window_entries(&history, Some(1), 10, 3);

        assert_eq!(
            entries,
            vec![
                TranscriptEntry {
                    kind: TranscriptEntryKind::Assistant,
                    phase: Some(MessagePhase::Commentary),
                    text: "older assistant update".to_string(),
                },
                TranscriptEntry {
                    kind: TranscriptEntryKind::Assistant,
                    phase: Some(MessagePhase::FinalAnswer),
                    text: "new assistant update".to_string(),
                },
            ]
        );
    }

    #[test]
    fn collect_window_entries_bounds_recent_history_when_no_boundary_exists() {
        let history = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "first user".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "assistant one".to_string(),
                }],
                end_turn: None,
                phase: Some(MessagePhase::Commentary),
            },
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "second user".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "assistant two".to_string(),
                }],
                end_turn: None,
                phase: Some(MessagePhase::FinalAnswer),
            },
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "third user".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
        ];

        let entries = collect_window_entries(&history, None, 1, 2);

        assert_eq!(
            entries,
            vec![
                TranscriptEntry {
                    kind: TranscriptEntryKind::User,
                    phase: None,
                    text: "second user".to_string(),
                },
                TranscriptEntry {
                    kind: TranscriptEntryKind::Assistant,
                    phase: Some(MessagePhase::FinalAnswer),
                    text: "assistant two".to_string(),
                },
                TranscriptEntry {
                    kind: TranscriptEntryKind::User,
                    phase: None,
                    text: "third user".to_string(),
                },
            ]
        );
    }

    #[test]
    fn preferred_judge_reasoning_effort_uses_low_when_supported() {
        let supported_reasoning_levels = vec![
            ReasoningEffortPreset {
                effort: ReasoningEffortConfig::Low,
                description: "cheap".to_string(),
            },
            ReasoningEffortPreset {
                effort: ReasoningEffortConfig::Medium,
                description: "default".to_string(),
            },
        ];

        assert_eq!(
            preferred_judge_reasoning_effort(
                Some(ReasoningEffortConfig::Medium),
                &supported_reasoning_levels,
            ),
            Some(ReasoningEffortConfig::Low)
        );
    }
}
