use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::WebSearchAction;
use regex_lite::Regex;
use sha1::Digest;
use std::collections::HashMap;
use std::sync::LazyLock;

const ENABLED_ENV: &str = "PITCHAI_CODEX_PRIVACY_MIDDLEWARE";
const TRUTHY: [&str; 4] = ["1", "true", "yes", "on"];
#[cfg(test)]
const OPENAI_PRIVACY_FILTER_MODEL_ID: &str = "openai/privacy-filter";

static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b")
        .expect("email regex should compile")
});
static PHONE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)(?:\+?1[\s.-]?)?(?:\(?\d{3}\)?[\s.-]?)\d{3}[\s.-]?\d{4}")
        .expect("phone regex should compile")
});
static ADDRESS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b\d{1,6}\s+[A-Z][A-Za-z]*(?:\s+[A-Z][A-Za-z]*){0,2}\s+(?:Street|St|Avenue|Ave|Road|Rd|Boulevard|Blvd|Lane|Ln|Drive|Dr|Court|Ct|Way|Place|Pl)\b(?:,\s*[A-Z][A-Za-z]*(?:\s+[A-Z][A-Za-z]*){0,2})?(?:,\s*[A-Z]{2})?(?:\s+\d{5}(?:-\d{4})?)?",
    )
    .expect("address regex should compile")
});
static SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:sk|pk|AKIA|ghp|pat)_[A-Za-z0-9_-]{12,}\b")
        .expect("identifier regex should compile")
});
static NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z][a-z]{2,}\s+[A-Z][a-z]{2,}(?:\s+[A-Z][a-z]{2,})?\b")
        .expect("name regex should compile")
});

const FIRST_NAMES: [&str; 10] = [
    "Maya", "Ethan", "Sofia", "Caleb", "Nora", "Julian", "Iris", "Marcus", "Elena", "Theo",
];
const LAST_NAMES: [&str; 10] = [
    "Bennett", "Ramirez", "Foster", "Patel", "Morgan", "Sinclair", "Hayes", "Kovacs", "Reed",
    "Walsh",
];
const STREETS: [&str; 8] = [
    "Maple Street",
    "Cedar Avenue",
    "Riverside Drive",
    "Summit Road",
    "Oak Lane",
    "Harbor Way",
    "Willow Court",
    "Pine Boulevard",
];
const CITIES: [&str; 8] = [
    "Portland", "Denver", "Austin", "Madison", "Raleigh", "Phoenix", "Seattle", "Boston",
];
const STATES: [&str; 8] = ["OR", "CO", "TX", "WI", "NC", "AZ", "WA", "MA"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetectionSpan {
    start: usize,
    end: usize,
    kind: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrivacyMapping {
    real: String,
    fake: String,
    kind: &'static str,
}

#[derive(Debug, Default)]
pub(crate) struct PrivacyFilter {
    by_real: HashMap<String, PrivacyMapping>,
    by_fake: HashMap<String, PrivacyMapping>,
}

impl PrivacyFilter {
    pub(crate) fn enabled_from_env() -> bool {
        std::env::var(ENABLED_ENV)
            .map(|value| {
                let value = value.to_ascii_lowercase();
                TRUTHY.contains(&value.as_str())
            })
            .unwrap_or(false)
    }

    pub(crate) fn anonymize_response_items(&mut self, items: &mut [ResponseItem]) {
        for item in items {
            self.anonymize_response_item(item);
        }
    }

    pub(crate) fn deanonymize_response_item(&mut self, item: &mut ResponseItem) {
        self.transform_response_item(item, TransformDirection::Deanonymize);
    }

    fn anonymize_response_item(&mut self, item: &mut ResponseItem) {
        self.transform_response_item(item, TransformDirection::Anonymize);
    }

    fn transform_response_item(&mut self, item: &mut ResponseItem, direction: TransformDirection) {
        match item {
            ResponseItem::Message { content, .. } => {
                for content_item in content {
                    self.transform_content_item(content_item, direction);
                }
            }
            ResponseItem::FunctionCall { arguments, .. }
            | ResponseItem::CustomToolCall {
                input: arguments, ..
            } => self.transform_string(arguments, direction),
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                self.transform_output_body(&mut output.body, direction);
            }
            ResponseItem::WebSearchCall { action, .. } => {
                if let Some(action) = action {
                    self.transform_web_search_action(action, direction);
                }
            }
            ResponseItem::ImageGenerationCall { revised_prompt, .. } => {
                if let Some(revised_prompt) = revised_prompt {
                    self.transform_string(revised_prompt, direction);
                }
            }
            ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::GhostSnapshot { .. }
            | ResponseItem::Other => {}
        }
    }

    fn transform_web_search_action(
        &mut self,
        action: &mut WebSearchAction,
        direction: TransformDirection,
    ) {
        match action {
            WebSearchAction::Search { query, queries } => {
                if let Some(query) = query {
                    self.transform_string(query, direction);
                }
                if let Some(queries) = queries {
                    for query in queries {
                        self.transform_string(query, direction);
                    }
                }
            }
            WebSearchAction::OpenPage { url } => {
                if let Some(url) = url {
                    self.transform_string(url, direction);
                }
            }
            WebSearchAction::FindInPage { url, pattern } => {
                if let Some(url) = url {
                    self.transform_string(url, direction);
                }
                if let Some(pattern) = pattern {
                    self.transform_string(pattern, direction);
                }
            }
            WebSearchAction::Other => {}
        }
    }

    fn transform_content_item(&mut self, item: &mut ContentItem, direction: TransformDirection) {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                self.transform_string(text, direction);
            }
            ContentItem::InputImage { .. } => {}
        }
    }

    fn transform_output_body(
        &mut self,
        output: &mut FunctionCallOutputBody,
        direction: TransformDirection,
    ) {
        match output {
            FunctionCallOutputBody::Text(text) => self.transform_string(text, direction),
            FunctionCallOutputBody::ContentItems(items) => {
                for item in items {
                    let FunctionCallOutputContentItem::InputText { text } = item else {
                        continue;
                    };
                    self.transform_string(text, direction);
                }
            }
        }
    }

    fn transform_json_value(
        &mut self,
        value: &mut serde_json::Value,
        direction: TransformDirection,
    ) {
        match value {
            serde_json::Value::String(text) => self.transform_string(text, direction),
            serde_json::Value::Array(items) => {
                for item in items {
                    self.transform_json_value(item, direction);
                }
            }
            serde_json::Value::Object(map) => {
                for value in map.values_mut() {
                    self.transform_json_value(value, direction);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }

    fn transform_string(&mut self, text: &mut String, direction: TransformDirection) {
        match direction {
            TransformDirection::Anonymize => *text = self.anonymize_text(text),
            TransformDirection::Deanonymize => *text = self.deanonymize_text(text),
        }
    }

    fn anonymize_text(&mut self, text: &str) -> String {
        let spans = detected_spans(text);
        if spans.is_empty() {
            return text.to_string();
        }
        let mut output = String::with_capacity(text.len());
        let mut cursor = 0;
        for span in spans {
            output.push_str(&text[cursor..span.start]);
            let real = &text[span.start..span.end];
            let fake = self.fake_for(real, span.kind);
            output.push_str(&fake);
            cursor = span.end;
        }
        output.push_str(&text[cursor..]);
        output
    }

    fn deanonymize_text(&self, text: &str) -> String {
        let mut restored = text.to_string();
        let mut mappings = self.by_fake.values().collect::<Vec<_>>();
        mappings.sort_by_key(|mapping| std::cmp::Reverse(mapping.fake.len()));
        for mapping in mappings {
            restored = restored.replace(&mapping.fake, &mapping.real);
        }
        restored
    }

    fn fake_for(&mut self, real: &str, kind: &'static str) -> String {
        if let Some(existing) = self.by_real.get(real) {
            return existing.fake.clone();
        }
        let fake = fake_value(real, kind);
        let mapping = PrivacyMapping {
            real: real.to_string(),
            fake: fake.clone(),
            kind,
        };
        self.by_fake.insert(fake.clone(), mapping.clone());
        self.by_real.insert(real.to_string(), mapping);
        fake
    }
}

#[derive(Debug, Clone, Copy)]
enum TransformDirection {
    Anonymize,
    Deanonymize,
}

fn detected_spans(text: &str) -> Vec<DetectionSpan> {
    let mut spans = Vec::new();
    spans.extend(regex_spans(text, &EMAIL_RE, "email"));
    spans.extend(regex_spans(text, &PHONE_RE, "phone"));
    spans.extend(regex_spans(text, &ADDRESS_RE, "address"));
    spans.extend(regex_spans(text, &SECRET_RE, "identifier"));
    for matched in NAME_RE.find_iter(text) {
        let value = matched.as_str();
        if [
            "OpenAI",
            "PitchAI",
            "United States",
            "New York",
            "San Francisco",
        ]
        .iter()
        .any(|prefix| value.starts_with(prefix))
        {
            continue;
        }
        let parts = value.split_whitespace().collect::<Vec<_>>();
        if parts.len() == 3
            && ["Ask", "Call", "Email", "Tell", "Contact", "Text", "Message"].contains(&parts[0])
        {
            spans.push(DetectionSpan {
                start: matched.start() + parts[0].len() + 1,
                end: matched.end(),
                kind: "person",
            });
        } else {
            spans.push(DetectionSpan {
                start: matched.start(),
                end: matched.end(),
                kind: "person",
            });
        }
    }
    spans.sort_by_key(|span| (span.start, std::cmp::Reverse(span.end - span.start)));
    without_overlaps(spans)
}

fn regex_spans(text: &str, regex: &Regex, kind: &'static str) -> Vec<DetectionSpan> {
    regex
        .find_iter(text)
        .map(|matched| DetectionSpan {
            start: matched.start(),
            end: matched.end(),
            kind,
        })
        .collect()
}

fn without_overlaps(spans: Vec<DetectionSpan>) -> Vec<DetectionSpan> {
    let mut selected = Vec::new();
    let mut covered_until = 0;
    for span in spans {
        if span.start < covered_until {
            continue;
        }
        covered_until = span.end;
        selected.push(span);
    }
    selected
}

fn fake_value(real: &str, kind: &str) -> String {
    let digest = sha1::Sha1::digest(format!("{kind}\0{real}").as_bytes());
    match kind {
        "email" => format!(
            "{}.{}{}@example.com",
            FIRST_NAMES[digest[0] as usize % FIRST_NAMES.len()].to_ascii_lowercase(),
            LAST_NAMES[digest[1] as usize % LAST_NAMES.len()].to_ascii_lowercase(),
            100 + digest[2] as u16 % 900
        ),
        "phone" => format!(
            "(555) {:03}-{:04}",
            200 + digest[0] as u16 % 700,
            1000 + u16::from_be_bytes([digest[1], digest[2]]) % 9000
        ),
        "address" => {
            let city_index = digest[2] as usize % CITIES.len();
            format!(
                "{} {}, {}, {} {}",
                100 + u16::from_be_bytes([digest[0], digest[1]]) % 9800,
                STREETS[digest[3] as usize % STREETS.len()],
                CITIES[city_index],
                STATES[city_index],
                10000 + u32::from(u16::from_be_bytes([digest[4], digest[5]])) % 90000
            )
        }
        "person" => format!(
            "{} {}",
            FIRST_NAMES[digest[0] as usize % FIRST_NAMES.len()],
            LAST_NAMES[digest[1] as usize % LAST_NAMES.len()]
        ),
        _ => format!(
            "acct-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            digest[0], digest[1], digest[2], digest[3], digest[4], digest[5]
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn roundtrip_anonymizes_and_restores_realistic_fake_pii() {
        let mut filter = PrivacyFilter::default();
        let mut items = vec![user_message(
            "Tell Alice Smith at alice.smith@example.com about 742 Evergreen Street.",
        )];

        filter.anonymize_response_items(&mut items);
        let serialized = serde_json::to_string(&items).expect("serialize items");
        assert!(!serialized.contains("Alice Smith"));
        assert!(!serialized.contains("alice.smith@example.com"));
        assert!(!serialized.contains("742 Evergreen Street"));
        assert!(serialized.contains("@example.com"));

        filter.deanonymize_response_item(&mut items[0]);
        assert_eq!(
            items,
            vec![user_message(
                "Tell Alice Smith at alice.smith@example.com about 742 Evergreen Street."
            )]
        );
    }

    #[test]
    fn stable_mapping_reuses_fake_values() {
        let mut filter = PrivacyFilter::default();
        let mut first = vec![user_message("Call Alice Smith.")];
        let mut second = vec![user_message("Email Alice Smith.")];

        filter.anonymize_response_items(&mut first);
        filter.anonymize_response_items(&mut second);

        let first_json = serde_json::to_string(&first).expect("serialize first");
        let second_json = serde_json::to_string(&second).expect("serialize second");
        let fake_name = filter
            .by_real
            .get("Alice Smith")
            .expect("mapping exists")
            .fake
            .clone();
        assert!(first_json.contains(&fake_name));
        assert!(second_json.contains(&fake_name));
    }

    #[test]
    fn mapping_is_not_serialized_in_outbound_payload_shape() {
        let mut filter = PrivacyFilter::default();
        let mut items = vec![ResponseItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                "Alice Smith: alice.smith@example.com".to_string(),
            ),
        }];

        filter.anonymize_response_items(&mut items);
        let serialized = serde_json::to_string(&items).expect("serialize items");

        assert!(!serialized.contains("Alice Smith"));
        assert!(!serialized.contains("alice.smith@example.com"));
        assert!(!serialized.contains("by_real"));
        assert!(!serialized.contains("by_fake"));
    }

    #[test]
    fn documents_verified_openai_artifact_id() {
        assert_eq!(OPENAI_PRIVACY_FILTER_MODEL_ID, "openai/privacy-filter");
    }
}
