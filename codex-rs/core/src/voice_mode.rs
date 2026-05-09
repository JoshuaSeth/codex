use crate::default_client::build_reqwest_client;
use anyhow::Context;
use anyhow::Result;
use codex_protocol::items::TurnItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteDeferredByNonStopEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use reqwest::header::AUTHORIZATION;
use serde::Serialize;
use std::collections::HashMap;
use std::env;
use std::time::Duration;
use tokio::sync::mpsc;

const DEFAULT_ENDPOINT: &str = "https://dispatch.pitchai.net/ui/api/agent/voice_push";
const DEFAULT_SOURCE: &str = "codex";
const DEFAULT_VOICE: &str = "M1";
const DEFAULT_TIMEOUT_SECS: f64 = 4.0;
const DEFAULT_INITIAL_EMIT_CHARS: usize = 24;
const DEFAULT_INCREMENTAL_EMIT_CHARS: usize = 72;

#[derive(Clone)]
pub(crate) struct VoiceOutputClient {
    tx: mpsc::UnboundedSender<QueuedVoiceEvent>,
}

struct VoiceOutputRuntime {
    http: reqwest::Client,
    endpoint: String,
    token: Option<String>,
    basic_auth: Option<String>,
    voice: String,
    source: String,
    tmux_session: Option<String>,
    routine_job_id: Option<String>,
    timeout: Duration,
    tracker: VoiceEventTracker,
}

#[derive(Debug)]
struct QueuedVoiceEvent {
    conversation_id: String,
    msg: EventMsg,
}

#[derive(Serialize)]
struct VoicePushBody<'a> {
    text: &'a str,
    voice: &'a str,
    source: &'a str,
    conversation_id: &'a str,
    message_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    logical_message_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sequence: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_final: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tmux_session: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    routine_job_id: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceOutputEvent {
    pub(crate) message_id: String,
    pub(crate) logical_message_id: String,
    pub(crate) sequence: u32,
    pub(crate) text: String,
    pub(crate) phase: Option<MessagePhase>,
    pub(crate) is_final: bool,
}

#[derive(Debug, Default)]
struct VoiceEventTracker {
    messages: HashMap<String, TrackedVoiceMessage>,
    initial_emit_chars: usize,
    incremental_emit_chars: usize,
}

#[derive(Debug, Default)]
struct TrackedVoiceMessage {
    spoken_text: String,
    last_sent_text: String,
    next_sequence: u32,
}

impl VoiceOutputClient {
    pub(crate) fn from_env(enabled: bool) -> Result<Option<Self>> {
        let endpoint = env_string("PITCHAI_CODEX_SPEECH_ENDPOINT")
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
        let token = env_string("PITCHAI_CODEX_SPEECH_TOKEN")
            .or_else(|| env_string("PITCHAI_DISPATCH_TOKEN"));
        let basic_auth = env_string("PITCHAI_CODEX_SPEECH_BASIC_AUTH");
        if token.is_none() && basic_auth.is_none() {
            if !enabled {
                return Ok(None);
            }
            anyhow::bail!(
                "`--voice` requires `PITCHAI_CODEX_SPEECH_TOKEN`, `PITCHAI_DISPATCH_TOKEN`, or `PITCHAI_CODEX_SPEECH_BASIC_AUTH` so Codex can push assistant speech to Dispatcher."
            );
        }

        let voice = env_string("PITCHAI_CODEX_SPEECH_VOICE")
            .or_else(|| env_string("PITCHAI_TTS_VOICE"))
            .unwrap_or_else(|| DEFAULT_VOICE.to_string());
        let source =
            env_string("PITCHAI_CODEX_SPEECH_SOURCE").unwrap_or_else(|| DEFAULT_SOURCE.to_string());
        let tmux_session = env_string("PITCHAI_CODEX_TMUX_SESSION");
        let routine_job_id = env_string("PITCHAI_CODEX_ROUTINE_JOB_ID");
        let timeout = parse_timeout_seconds(
            &env_string("PITCHAI_CODEX_SPEECH_TIMEOUT_S")
                .unwrap_or_else(|| DEFAULT_TIMEOUT_SECS.to_string()),
        )?;
        let initial_emit_chars = parse_usize_clamped(
            "PITCHAI_CODEX_SPEECH_INITIAL_CHARS",
            DEFAULT_INITIAL_EMIT_CHARS,
            8,
            320,
        )?;
        let incremental_emit_chars = parse_usize_clamped(
            "PITCHAI_CODEX_SPEECH_UPDATE_CHARS",
            DEFAULT_INCREMENTAL_EMIT_CHARS,
            16,
            640,
        )?;

        let (tx, mut rx) = mpsc::unbounded_channel::<QueuedVoiceEvent>();
        let mut runtime = VoiceOutputRuntime {
            http: build_reqwest_client(),
            endpoint,
            token,
            basic_auth,
            voice,
            source,
            tmux_session,
            routine_job_id,
            timeout,
            tracker: VoiceEventTracker::new(initial_emit_chars, incremental_emit_chars),
        };
        tokio::spawn(async move {
            while let Some(queued) = rx.recv().await {
                if let Err(err) = runtime.handle_event(queued).await {
                    tracing::warn!(error = %err, "voice push failed");
                }
            }
        });

        Ok(Some(Self { tx }))
    }

    pub(crate) fn enqueue_event(&self, conversation_id: &str, msg: &EventMsg) {
        if let Err(err) = self.tx.send(QueuedVoiceEvent {
            conversation_id: conversation_id.to_string(),
            msg: msg.clone(),
        }) {
            tracing::warn!(error = %err, "voice push queue send failed");
        }
    }
}

impl VoiceOutputRuntime {
    async fn handle_event(&mut self, queued: QueuedVoiceEvent) -> Result<()> {
        let events = self.tracker.events_for_event(&queued.msg);
        for event in events {
            self.push(queued.conversation_id.as_str(), event).await?;
        }
        Ok(())
    }

    async fn push(&self, conversation_id: &str, event: VoiceOutputEvent) -> Result<()> {
        let phase = event.phase.as_ref().map(message_phase_name);
        let body = VoicePushBody {
            text: event.text.as_str(),
            voice: self.voice.as_str(),
            source: self.source.as_str(),
            conversation_id,
            message_id: event.message_id.as_str(),
            logical_message_id: Some(event.logical_message_id.as_str()),
            sequence: Some(event.sequence),
            is_final: Some(event.is_final),
            phase,
            tmux_session: self.tmux_session.as_deref(),
            routine_job_id: self.routine_job_id.as_deref(),
        };

        let mut request = self
            .http
            .post(self.endpoint.as_str())
            .timeout(self.timeout)
            .json(&body);
        if let Some(token) = self.token.as_deref() {
            request = request.header("X-PitchAI-Dispatch-Token", token);
        }
        if let Some(basic_auth) = self.basic_auth.as_deref() {
            request = request.header(AUTHORIZATION, format!("Basic {basic_auth}"));
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("posting assistant voice output to {}", self.endpoint))?;
        response
            .error_for_status_ref()
            .with_context(|| format!("voice push endpoint rejected {}", self.endpoint))?;
        Ok(())
    }
}

impl VoiceEventTracker {
    fn new(initial_emit_chars: usize, incremental_emit_chars: usize) -> Self {
        Self {
            messages: HashMap::new(),
            initial_emit_chars,
            incremental_emit_chars,
        }
    }

    fn events_for_event(&mut self, msg: &EventMsg) -> Vec<VoiceOutputEvent> {
        // Fork-critical behavior:
        //
        // Voice mode must start speaking during an assistant message, not only after the item is
        // completed. If an upstream merge reduces this back to completed-item-only pushes, the
        // browser voice cockpit regresses into long silent gaps and loses the "latest text wins"
        // feel that operators depend on for continuous voice sessions.
        match msg {
            EventMsg::AgentMessageContentDelta(event) => self.delta_events(event),
            EventMsg::ItemCompleted(event) => self.completed_events(event),
            EventMsg::TurnComplete(TurnCompleteEvent { .. })
            | EventMsg::TurnCompleteDeferredByNonStop(TurnCompleteDeferredByNonStopEvent {
                ..
            })
            | EventMsg::TurnAborted(TurnAbortedEvent { .. }) => {
                self.messages.clear();
                Vec::new()
            }
            EventMsg::Error(_) => {
                self.messages.clear();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn delta_events(&mut self, event: &AgentMessageContentDeltaEvent) -> Vec<VoiceOutputEvent> {
        let key = message_key(
            event.thread_id.as_str(),
            event.turn_id.as_str(),
            event.item_id.as_str(),
        );
        let tracked = self.messages.entry(key).or_default();
        tracked.spoken_text.push_str(event.delta.as_str());
        let candidate = normalize_spoken_text(tracked.spoken_text.as_str());
        if !should_emit_incremental(
            tracked.last_sent_text.as_str(),
            candidate.as_str(),
            self.initial_emit_chars,
            self.incremental_emit_chars,
        ) {
            return Vec::new();
        }
        let sequence = tracked.next_sequence;
        tracked.next_sequence += 1;
        tracked.last_sent_text = candidate.clone();
        vec![VoiceOutputEvent {
            message_id: emitted_message_id(event.item_id.as_str(), sequence),
            logical_message_id: event.item_id.clone(),
            sequence,
            text: candidate,
            phase: None,
            is_final: false,
        }]
    }

    fn completed_events(&mut self, event: &ItemCompletedEvent) -> Vec<VoiceOutputEvent> {
        match &event.item {
            TurnItem::AgentMessage(item) => {
                let key = message_key(
                    event.thread_id.to_string().as_str(),
                    event.turn_id.as_str(),
                    item.id.as_str(),
                );
                let mut tracked = self.messages.remove(&key).unwrap_or_default();
                let candidate = normalize_spoken_text(
                    &item
                        .content
                        .iter()
                        .map(|entry| match entry {
                            codex_protocol::items::AgentMessageContent::Text { text } => {
                                text.as_str()
                            }
                        })
                        .collect::<String>(),
                );
                if candidate.is_empty() || candidate == tracked.last_sent_text {
                    return Vec::new();
                }
                let sequence = tracked.next_sequence;
                tracked.next_sequence += 1;
                vec![VoiceOutputEvent {
                    message_id: emitted_message_id(item.id.as_str(), sequence),
                    logical_message_id: item.id.clone(),
                    sequence,
                    text: candidate,
                    phase: item.phase.clone(),
                    is_final: true,
                }]
            }
            _ => Vec::new(),
        }
    }
}

fn env_string(key: &str) -> Option<String> {
    let value = env::var(key).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_timeout_seconds(raw: &str) -> Result<Duration> {
    let seconds = raw
        .parse::<f64>()
        .with_context(|| format!("invalid timeout seconds `{raw}`"))?;
    let seconds = seconds.clamp(1.0, 15.0);
    Ok(Duration::from_secs_f64(seconds))
}

fn parse_usize_clamped(key: &str, default: usize, min: usize, max: usize) -> Result<usize> {
    let Some(raw) = env_string(key) else {
        return Ok(default);
    };
    let parsed = raw
        .parse::<usize>()
        .with_context(|| format!("invalid usize value `{raw}` for `{key}`"))?;
    Ok(parsed.clamp(min, max))
}

fn message_phase_name(phase: &MessagePhase) -> &'static str {
    match phase {
        MessagePhase::Commentary => "commentary",
        MessagePhase::FinalAnswer => "final_answer",
    }
}

fn emitted_message_id(item_id: &str, sequence: u32) -> String {
    format!("{item_id}:voice:{sequence}")
}

fn message_key(thread_id: &str, turn_id: &str, item_id: &str) -> String {
    format!("{thread_id}:{turn_id}:{item_id}")
}

fn normalize_spoken_text(text: &str) -> String {
    let mut lines = Vec::new();
    let mut pending_blank = false;

    for raw_line in text.lines() {
        let normalized_line = raw_line.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized_line.is_empty() {
            pending_blank = !lines.is_empty();
            continue;
        }
        if pending_blank {
            lines.push(String::new());
            pending_blank = false;
        }
        lines.push(normalized_line);
    }

    lines.join("\n").trim().to_string()
}

fn should_emit_incremental(
    last_sent_text: &str,
    candidate: &str,
    initial_emit_chars: usize,
    incremental_emit_chars: usize,
) -> bool {
    if candidate.is_empty() || candidate == last_sent_text {
        return false;
    }
    if last_sent_text.is_empty() {
        return candidate.chars().count() >= initial_emit_chars
            || ends_at_spoken_boundary(candidate);
    }
    if !candidate.starts_with(last_sent_text) {
        // Fork-critical behavior:
        //
        // Streaming models sometimes revise earlier words or punctuation while they are still
        // generating the same spoken block. Treating every non-prefix revision as a brand-new
        // spoken update floods the voice web with near-duplicate TTS jobs, which leads to
        // superseded stream URLs and audible silence / repeated play_drop cycles. Only emit when
        // the revised text contains a meaningful new trailing segment or lands on a spoken
        // boundary.
        let overlap = trailing_novel_char_count(last_sent_text, candidate);
        return overlap >= incremental_emit_chars || ends_at_spoken_boundary(candidate);
    }
    let new_chars = candidate[last_sent_text.len()..].chars().count();
    new_chars >= incremental_emit_chars || ends_at_spoken_boundary(candidate)
}

fn trailing_novel_char_count(previous: &str, current: &str) -> usize {
    let previous_words = transcript_words(previous);
    let current_words = transcript_words(current);
    if previous_words.len() < 4 || current_words.len() <= previous_words.len() {
        return current.chars().count();
    }

    let matched_current_indexes = lcs_current_indexes(&previous_words, &current_words);
    let minimum_matched_words = previous_words.len().max(5).div_ceil(5) * 4;
    if matched_current_indexes.len() < minimum_matched_words {
        return current.chars().count();
    }
    if matched_current_indexes.first().copied().unwrap_or_default() > 1 {
        return current.chars().count();
    }

    let Some(last_matched_index) = matched_current_indexes.last().copied() else {
        return current.chars().count();
    };
    if last_matched_index >= current_words.len().saturating_sub(1) {
        return current.chars().count();
    }

    let overlap_density = matched_current_indexes.len() as f64 / (last_matched_index + 1) as f64;
    if overlap_density < 0.75 {
        return current.chars().count();
    }

    current[current_words[last_matched_index + 1].start..]
        .trim_start_matches(|c: char| c.is_whitespace() || ",.;:!?-".contains(c))
        .chars()
        .count()
}

fn transcript_words(text: &str) -> Vec<TranscriptWord> {
    let mut words = Vec::new();
    let mut current_start = None;
    for (idx, ch) in text.char_indices() {
        if ch.is_whitespace() {
            if let Some(start) = current_start.take() {
                push_transcript_word(text, start, idx, &mut words);
            }
            continue;
        }
        if current_start.is_none() {
            current_start = Some(idx);
        }
    }
    if let Some(start) = current_start {
        push_transcript_word(text, start, text.len(), &mut words);
    }
    words
}

fn push_transcript_word(text: &str, start: usize, end: usize, words: &mut Vec<TranscriptWord>) {
    let raw = &text[start..end];
    let normalized = raw
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '\'')
        .to_ascii_lowercase();
    if normalized.is_empty() {
        return;
    }
    words.push(TranscriptWord { normalized, start });
}

fn lcs_current_indexes(previous: &[TranscriptWord], current: &[TranscriptWord]) -> Vec<usize> {
    let previous_len = previous.len();
    let current_len = current.len();
    let mut dp = vec![vec![0usize; current_len + 1]; previous_len + 1];
    for previous_idx in (0..previous_len).rev() {
        for current_idx in (0..current_len).rev() {
            dp[previous_idx][current_idx] =
                if previous[previous_idx].normalized == current[current_idx].normalized {
                    dp[previous_idx + 1][current_idx + 1] + 1
                } else {
                    dp[previous_idx + 1][current_idx].max(dp[previous_idx][current_idx + 1])
                };
        }
    }

    let mut matched_current_indexes = Vec::new();
    let (mut previous_idx, mut current_idx) = (0usize, 0usize);
    while previous_idx < previous_len && current_idx < current_len {
        if previous[previous_idx].normalized == current[current_idx].normalized {
            matched_current_indexes.push(current_idx);
            previous_idx += 1;
            current_idx += 1;
        } else if dp[previous_idx + 1][current_idx] >= dp[previous_idx][current_idx + 1] {
            previous_idx += 1;
        } else {
            current_idx += 1;
        }
    }
    matched_current_indexes
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptWord {
    normalized: String,
    start: usize,
}

fn ends_at_spoken_boundary(text: &str) -> bool {
    matches!(
        text.chars().last(),
        Some('.' | '!' | '?' | ':' | ';' | ',' | ')' | ']' | '}')
    )
}

#[cfg(test)]
mod tests {
    use super::VoiceEventTracker;
    use super::emitted_message_id;
    use super::should_emit_incremental;
    use codex_protocol::ThreadId;
    use codex_protocol::items::AgentMessageContent;
    use codex_protocol::items::AgentMessageItem;
    use codex_protocol::items::TurnItem;
    use codex_protocol::models::MessagePhase;
    use codex_protocol::protocol::AgentMessageContentDeltaEvent;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ItemCompletedEvent;
    use pretty_assertions::assert_eq;

    #[test]
    fn emits_incremental_then_final_voice_updates() {
        let thread_id = ThreadId::new();
        let mut tracker = VoiceEventTracker::new(24, 32);
        let first = tracker.events_for_event(&EventMsg::AgentMessageContentDelta(
            AgentMessageContentDeltaEvent {
                thread_id: thread_id.to_string(),
                turn_id: "turn-123".to_string(),
                item_id: "msg-123".to_string(),
                delta: "First line of commentary".to_string(),
            },
        ));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].message_id, emitted_message_id("msg-123", 0));
        assert_eq!(first[0].text, "First line of commentary");
        assert_eq!(first[0].is_final, false);

        let second = tracker.events_for_event(&EventMsg::AgentMessageContentDelta(
            AgentMessageContentDeltaEvent {
                thread_id: thread_id.to_string(),
                turn_id: "turn-123".to_string(),
                item_id: "msg-123".to_string(),
                delta: " and more detail before the final answer".to_string(),
            },
        ));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].message_id, emitted_message_id("msg-123", 1));
        assert_eq!(
            second[0].text,
            "First line of commentary and more detail before the final answer"
        );
        assert_eq!(second[0].is_final, false);

        let final_event = tracker.events_for_event(&EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id,
            turn_id: "turn-123".to_string(),
            item: TurnItem::AgentMessage(AgentMessageItem {
                id: "msg-123".to_string(),
                content: vec![
                    AgentMessageContent::Text {
                        text: "First line of commentary".to_string(),
                    },
                    AgentMessageContent::Text {
                        text: "\nSecond line final answer".to_string(),
                    },
                ],
                phase: Some(MessagePhase::Commentary),
            }),
        }));
        assert_eq!(final_event.len(), 1);
        assert_eq!(final_event[0].message_id, emitted_message_id("msg-123", 2));
        assert_eq!(
            final_event[0].text,
            "First line of commentary\nSecond line final answer"
        );
        assert_eq!(final_event[0].phase, Some(MessagePhase::Commentary));
        assert!(final_event[0].is_final);
    }

    #[test]
    fn waits_for_more_than_tiny_delta_before_emitting() {
        let thread_id = ThreadId::new();
        let mut tracker = VoiceEventTracker::new(24, 32);
        let initial = tracker.events_for_event(&EventMsg::AgentMessageContentDelta(
            AgentMessageContentDeltaEvent {
                thread_id: thread_id.to_string(),
                turn_id: "turn-123".to_string(),
                item_id: "msg-123".to_string(),
                delta: "too short".to_string(),
            },
        ));
        assert_eq!(initial, Vec::new());

        let final_event = tracker.events_for_event(&EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id,
            turn_id: "turn-123".to_string(),
            item: TurnItem::AgentMessage(AgentMessageItem {
                id: "msg-123".to_string(),
                content: vec![AgentMessageContent::Text {
                    text: "too short but completed".to_string(),
                }],
                phase: Some(MessagePhase::FinalAnswer),
            }),
        }));
        assert_eq!(final_event.len(), 1);
        assert_eq!(final_event[0].text, "too short but completed");
        assert!(final_event[0].is_final);
    }

    #[test]
    fn suppresses_small_revisions_of_already_spoken_text() {
        assert!(!should_emit_incremental(
            "The inventory is stable: pitchai sees 13 colleague sessions under root code and all of them are idle rather than actively updating",
            "The inventory is stable: pitchai sees 13 colleague sessions under root code, and all of them are idle rather than actively updating",
            24,
            32,
        ));
        assert!(should_emit_incremental(
            "The inventory is stable: pitchai sees 13 colleague sessions under root code and all of them are idle rather than actively updating",
            "The inventory is stable: pitchai sees 13 colleague sessions under root code, and all of them are idle rather than actively updating. I am now switching to per-session inspection to confirm that no one is blocked.",
            24,
            32,
        ));
    }

    #[test]
    fn preserves_structural_line_breaks_for_spoken_text() {
        assert_eq!(
            super::normalize_spoken_text(
                "Current `pitchai` CLI status:\n\n- first point\n- second point\n\nSo the main update is:\n- still active\n"
            ),
            "Current `pitchai` CLI status:\n\n- first point\n- second point\n\nSo the main update is:\n- still active"
        );
    }
}
