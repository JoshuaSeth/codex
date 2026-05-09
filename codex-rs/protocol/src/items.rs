use crate::models::MessagePhase;
use crate::models::WebSearchAction;
use crate::protocol::AgentMessageEvent;
use crate::protocol::AgentReasoningEvent;
use crate::protocol::AgentReasoningRawContentEvent;
use crate::protocol::ContextCompactedEvent;
use crate::protocol::EventMsg;
use crate::protocol::ImageGenerationEndEvent;
use crate::protocol::UserMessageEvent;
use crate::protocol::UserMessageSource;
use crate::protocol::WebSearchEndEvent;
use crate::protocol::unwrap_voice_transcript;
use crate::user_input::ByteRange;
use crate::user_input::TextElement;
use crate::user_input::UserInput;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
#[serde(tag = "type")]
#[ts(tag = "type")]
pub enum TurnItem {
    UserMessage(UserMessageItem),
    AgentMessage(AgentMessageItem),
    Plan(PlanItem),
    Reasoning(ReasoningItem),
    WebSearch(WebSearchItem),
    ImageGeneration(ImageGenerationItem),
    ContextCompaction(ContextCompactionItem),
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
pub struct UserMessageItem {
    pub id: String,
    pub content: Vec<UserInput>,
    #[serde(default)]
    pub source: UserMessageSource,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
#[serde(tag = "type")]
#[ts(tag = "type")]
pub enum AgentMessageContent {
    Text { text: String },
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
/// Assistant-authored message payload used in turn-item streams.
///
/// `phase` is optional because not all providers/models emit it. Consumers
/// should use it when present, but retain legacy completion semantics when it
/// is `None`.
pub struct AgentMessageItem {
    pub id: String,
    pub content: Vec<AgentMessageContent>,
    /// Optional phase metadata carried through from `ResponseItem::Message`.
    ///
    /// This is currently used by TUI rendering to distinguish mid-turn
    /// commentary from a final answer and avoid status-indicator jitter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub phase: Option<MessagePhase>,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
pub struct PlanItem {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
pub struct ReasoningItem {
    pub id: String,
    pub summary_text: Vec<String>,
    #[serde(default)]
    pub raw_content: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq)]
pub struct WebSearchItem {
    pub id: String,
    pub query: String,
    pub action: WebSearchAction,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq)]
pub struct ImageGenerationItem {
    pub id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub revised_prompt: Option<String>,
    pub result: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
pub struct ContextCompactionItem {
    pub id: String,
}

impl ContextCompactionItem {
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub fn as_legacy_event(&self) -> EventMsg {
        EventMsg::ContextCompacted(ContextCompactedEvent {})
    }
}

impl Default for ContextCompactionItem {
    fn default() -> Self {
        Self::new()
    }
}

impl UserMessageItem {
    pub fn new(content: &[UserInput]) -> Self {
        let source = detect_user_message_source(content);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            content: content.to_vec(),
            source,
        }
    }

    pub fn as_legacy_event(&self) -> EventMsg {
        // Legacy user-message events flatten only text inputs into `message` and
        // rebase text element ranges onto that concatenated text.
        EventMsg::UserMessage(UserMessageEvent {
            message: self.raw_message(),
            images: Some(self.image_urls()),
            local_images: self.local_image_paths(),
            text_elements: self.text_elements(),
        })
    }

    pub fn message(&self) -> String {
        let message = self.raw_message();
        if self.source == UserMessageSource::Voice {
            unwrap_voice_transcript(&message).unwrap_or(message)
        } else {
            message
        }
    }

    pub fn raw_message(&self) -> String {
        self.content
            .iter()
            .map(|c| match c {
                UserInput::Text { text, .. } => text.clone(),
                _ => String::new(),
            })
            .collect::<Vec<String>>()
            .join("")
    }

    pub fn text_elements(&self) -> Vec<TextElement> {
        let mut out = Vec::new();
        let mut offset = 0usize;
        for input in &self.content {
            if let UserInput::Text {
                text,
                text_elements,
            } = input
            {
                // Text element ranges are relative to each text chunk; offset them so they align
                // with the concatenated message returned by `message()`.
                for elem in text_elements {
                    let byte_range = ByteRange {
                        start: offset + elem.byte_range.start,
                        end: offset + elem.byte_range.end,
                    };
                    out.push(TextElement::new(
                        byte_range,
                        elem.placeholder(text).map(str::to_string),
                    ));
                }
                offset += text.len();
            }
        }
        out
    }

    pub fn image_urls(&self) -> Vec<String> {
        self.content
            .iter()
            .filter_map(|c| match c {
                UserInput::Image { image_url } => Some(image_url.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn local_image_paths(&self) -> Vec<std::path::PathBuf> {
        self.content
            .iter()
            .filter_map(|c| match c {
                UserInput::LocalImage { path } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }
}

fn detect_user_message_source(content: &[UserInput]) -> UserMessageSource {
    let Some([UserInput::Text { text, .. }]) = content.first_chunk::<1>() else {
        return UserMessageSource::Typed;
    };
    if unwrap_voice_transcript(text).is_some() {
        UserMessageSource::Voice
    } else {
        UserMessageSource::Typed
    }
}

impl AgentMessageItem {
    pub fn new(content: &[AgentMessageContent]) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            content: content.to_vec(),
            phase: None,
        }
    }

    pub fn as_legacy_events(&self) -> Vec<EventMsg> {
        self.content
            .iter()
            .map(|c| match c {
                AgentMessageContent::Text { text } => EventMsg::AgentMessage(AgentMessageEvent {
                    message: text.clone(),
                    phase: self.phase.clone(),
                }),
            })
            .collect()
    }
}

impl ReasoningItem {
    pub fn as_legacy_events(&self, show_raw_agent_reasoning: bool) -> Vec<EventMsg> {
        let mut events = Vec::new();
        for summary in &self.summary_text {
            events.push(EventMsg::AgentReasoning(AgentReasoningEvent {
                text: summary.clone(),
            }));
        }

        if show_raw_agent_reasoning {
            for entry in &self.raw_content {
                events.push(EventMsg::AgentReasoningRawContent(
                    AgentReasoningRawContentEvent {
                        text: entry.clone(),
                    },
                ));
            }
        }

        events
    }
}

impl WebSearchItem {
    pub fn as_legacy_event(&self) -> EventMsg {
        EventMsg::WebSearchEnd(WebSearchEndEvent {
            call_id: self.id.clone(),
            query: self.query.clone(),
            action: self.action.clone(),
        })
    }
}

impl ImageGenerationItem {
    pub fn as_legacy_event(&self) -> EventMsg {
        EventMsg::ImageGenerationEnd(ImageGenerationEndEvent {
            call_id: self.id.clone(),
            status: self.status.clone(),
            revised_prompt: self.revised_prompt.clone(),
            result: self.result.clone(),
        })
    }
}

impl TurnItem {
    pub fn id(&self) -> String {
        match self {
            TurnItem::UserMessage(item) => item.id.clone(),
            TurnItem::AgentMessage(item) => item.id.clone(),
            TurnItem::Plan(item) => item.id.clone(),
            TurnItem::Reasoning(item) => item.id.clone(),
            TurnItem::WebSearch(item) => item.id.clone(),
            TurnItem::ImageGeneration(item) => item.id.clone(),
            TurnItem::ContextCompaction(item) => item.id.clone(),
        }
    }

    pub fn as_legacy_events(&self, show_raw_agent_reasoning: bool) -> Vec<EventMsg> {
        match self {
            TurnItem::UserMessage(item) => vec![item.as_legacy_event()],
            TurnItem::AgentMessage(item) => item.as_legacy_events(),
            TurnItem::Plan(_) => Vec::new(),
            TurnItem::WebSearch(item) => vec![item.as_legacy_event()],
            TurnItem::ImageGeneration(item) => vec![item.as_legacy_event()],
            TurnItem::Reasoning(item) => item.as_legacy_events(show_raw_agent_reasoning),
            TurnItem::ContextCompaction(item) => vec![item.as_legacy_event()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::wrap_voice_transcript;

    #[test]
    fn voice_wrapped_user_message_uses_voice_source_and_unwrapped_legacy_text() {
        let item = UserMessageItem::new(&[UserInput::Text {
            text: wrap_voice_transcript("hello from voice"),
            text_elements: Vec::new(),
        }]);

        assert_eq!(item.source, UserMessageSource::Voice);
        assert_eq!(item.message(), "hello from voice".to_string());

        let EventMsg::UserMessage(event) = item.as_legacy_event() else {
            panic!("expected user message event");
        };
        assert_eq!(event.message, wrap_voice_transcript("hello from voice"));
    }
}
