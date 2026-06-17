use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::WebSearchAction;
use serde::Deserialize;
use sha1::Digest;
use std::collections::HashMap;
use std::io::Write;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;

use crate::error::CodexErr;
use crate::error::Result;

const ENABLED_ENV: &str = "PITCHAI_CODEX_PRIVACY_MIDDLEWARE";
const PYTHON_ENV: &str = "PITCHAI_CODEX_PRIVACY_FILTER_PYTHON";
const COMMAND_ENV: &str = "PITCHAI_CODEX_PRIVACY_FILTER_COMMAND";
const MODEL_ENV: &str = "PITCHAI_CODEX_PRIVACY_FILTER_MODEL";
const DEVICE_ENV: &str = "PITCHAI_CODEX_PRIVACY_FILTER_DEVICE";
const N_CTX_ENV: &str = "PITCHAI_CODEX_PRIVACY_FILTER_N_CTX";
const TRUTHY: [&str; 4] = ["1", "true", "yes", "on"];
#[cfg(test)]
const OPENAI_PRIVACY_FILTER_MODEL_ID: &str = "openai/privacy-filter";

const OPF_PYTHON_SNIPPET: &str = r#"
import json
import os
import sys

from opf import OPF

text = sys.stdin.read()
kwargs = {
    "device": os.environ.get("__DEVICE_ENV__", "cpu"),
    "output_mode": "typed",
    "output_text_only": False,
}
model = os.environ.get("__MODEL_ENV__")
if model:
    kwargs["model"] = model
n_ctx = os.environ.get("__N_CTX_ENV__")
if n_ctx:
    kwargs["context_window_length"] = int(n_ctx)

result = OPF(**kwargs).redact(text)
print(result.to_json(indent=None))
"#;

#[derive(Debug, Clone, Deserialize)]
struct OpfSpan {
    label: String,
    start: usize,
    end: usize,
    #[serde(rename = "text")]
    text: String,
}

#[derive(Debug, Deserialize)]
struct OpfResult {
    detected_spans: Vec<OpfSpan>,
}

trait PrivacyDetector: Send + Sync {
    fn detect(&self, text: &str) -> Result<Vec<OpfSpan>>;
}

#[derive(Debug, Default)]
struct OpfCommandDetector;

impl PrivacyDetector for OpfCommandDetector {
    fn detect(&self, text: &str) -> Result<Vec<OpfSpan>> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }

        let output = if let Ok(command) = std::env::var(COMMAND_ENV) {
            run_detector_command(Command::new("sh").arg("-c").arg(command), text)?
        } else {
            let python = std::env::var(PYTHON_ENV).unwrap_or_else(|_| "python3".to_string());
            run_detector_command(
                Command::new(python).arg("-c").arg(opf_python_snippet()),
                text,
            )?
        };

        let result: OpfResult = serde_json::from_slice(&output).map_err(|err| {
            CodexErr::Fatal(format!(
                "failed to parse OpenAI Privacy Filter JSON output: {err}"
            ))
        })?;

        Ok(valid_spans(text, result.detected_spans))
    }
}

fn opf_python_snippet() -> String {
    OPF_PYTHON_SNIPPET
        .replace("__DEVICE_ENV__", DEVICE_ENV)
        .replace("__MODEL_ENV__", MODEL_ENV)
        .replace("__N_CTX_ENV__", N_CTX_ENV)
}

fn run_detector_command(command: &mut Command, text: &str) -> Result<Vec<u8>> {
    let mut child = command
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            CodexErr::Fatal(format!(
                "failed to start local OpenAI Privacy Filter detector. Install the local OPF package from https://github.com/openai/privacy-filter or set {COMMAND_ENV}: {err}"
            ))
        })?;

    let mut stdin = child.stdin.take().ok_or_else(|| {
        CodexErr::Fatal("failed to open local OpenAI Privacy Filter stdin".to_string())
    })?;
    stdin.write_all(text.as_bytes()).map_err(|err| {
        CodexErr::Fatal(format!(
            "failed to send text to local OpenAI Privacy Filter detector: {err}"
        ))
    })?;
    drop(stdin);

    let output = child.wait_with_output().map_err(|err| {
        CodexErr::Fatal(format!(
            "failed while waiting for local OpenAI Privacy Filter detector: {err}"
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CodexErr::Fatal(format!(
            "local OpenAI Privacy Filter detector failed with status {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    Ok(output.stdout)
}

fn valid_spans(text: &str, spans: Vec<OpfSpan>) -> Vec<OpfSpan> {
    let mut spans = spans
        .into_iter()
        .filter(|span| {
            span.start < span.end
                && span.end <= text.len()
                && text.is_char_boundary(span.start)
                && text.is_char_boundary(span.end)
                && text
                    .get(span.start..span.end)
                    .is_some_and(|detected_text| detected_text == span.text)
        })
        .collect::<Vec<_>>();
    spans.sort_by_key(|span| (span.start, std::cmp::Reverse(span.end - span.start)));

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrivacyMapping {
    real: String,
    fake: String,
    label: String,
}

pub(crate) struct PrivacyFilter {
    detector: Arc<dyn PrivacyDetector>,
    by_real: HashMap<String, PrivacyMapping>,
    by_fake: HashMap<String, PrivacyMapping>,
}

impl std::fmt::Debug for PrivacyFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivacyFilter")
            .field("mapping_count", &self.by_real.len())
            .finish_non_exhaustive()
    }
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

    pub(crate) fn from_env_if_enabled() -> Option<Self> {
        Self::enabled_from_env().then(|| Self::new(Arc::new(OpfCommandDetector)))
    }

    fn new(detector: Arc<dyn PrivacyDetector>) -> Self {
        Self {
            detector,
            by_real: HashMap::new(),
            by_fake: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_static_spans_for_tests(spans: Vec<(usize, usize, &str, &str)>) -> Self {
        #[derive(Debug)]
        struct StaticDetector {
            spans: Vec<OpfSpan>,
        }

        impl PrivacyDetector for StaticDetector {
            fn detect(&self, _text: &str) -> Result<Vec<OpfSpan>> {
                Ok(self.spans.clone())
            }
        }

        Self::new(Arc::new(StaticDetector {
            spans: spans
                .into_iter()
                .map(|(start, end, label, text)| OpfSpan {
                    label: label.to_string(),
                    start,
                    end,
                    text: text.to_string(),
                })
                .collect(),
        }))
    }

    pub(crate) fn anonymize_response_items(&mut self, items: &mut [ResponseItem]) -> Result<()> {
        for item in items {
            self.anonymize_response_item(item)?;
        }
        Ok(())
    }

    pub(crate) fn deanonymize_response_item(&mut self, item: &mut ResponseItem) {
        let _ = self.transform_response_item(item, TransformDirection::Deanonymize);
    }

    fn anonymize_response_item(&mut self, item: &mut ResponseItem) -> Result<()> {
        self.transform_response_item(item, TransformDirection::Anonymize)
    }

    fn transform_response_item(
        &mut self,
        item: &mut ResponseItem,
        direction: TransformDirection,
    ) -> Result<()> {
        match item {
            ResponseItem::Message { content, .. } => {
                for content_item in content {
                    self.transform_content_item(content_item, direction)?;
                }
            }
            ResponseItem::FunctionCall { arguments, .. }
            | ResponseItem::CustomToolCall {
                input: arguments, ..
            } => self.transform_string(arguments, direction)?,
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                self.transform_output_body(&mut output.body, direction)?;
            }
            ResponseItem::WebSearchCall { action, .. } => {
                if let Some(action) = action {
                    self.transform_web_search_action(action, direction)?;
                }
            }
            ResponseItem::ImageGenerationCall { revised_prompt, .. } => {
                if let Some(revised_prompt) = revised_prompt {
                    self.transform_string(revised_prompt, direction)?;
                }
            }
            ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::GhostSnapshot { .. }
            | ResponseItem::Other => {}
        }
        Ok(())
    }

    fn transform_content_item(
        &mut self,
        item: &mut ContentItem,
        direction: TransformDirection,
    ) -> Result<()> {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                self.transform_string(text, direction)?;
            }
            ContentItem::InputImage { .. } => {}
        }
        Ok(())
    }

    fn transform_output_body(
        &mut self,
        output: &mut FunctionCallOutputBody,
        direction: TransformDirection,
    ) -> Result<()> {
        match output {
            FunctionCallOutputBody::Text(text) => self.transform_string(text, direction)?,
            FunctionCallOutputBody::ContentItems(items) => {
                for item in items {
                    let FunctionCallOutputContentItem::InputText { text } = item else {
                        continue;
                    };
                    self.transform_string(text, direction)?;
                }
            }
        }
        Ok(())
    }

    fn transform_web_search_action(
        &mut self,
        action: &mut WebSearchAction,
        direction: TransformDirection,
    ) -> Result<()> {
        match action {
            WebSearchAction::Search { query, queries } => {
                if let Some(query) = query {
                    self.transform_string(query, direction)?;
                }
                if let Some(queries) = queries {
                    for query in queries {
                        self.transform_string(query, direction)?;
                    }
                }
            }
            WebSearchAction::OpenPage { url } => {
                if let Some(url) = url {
                    self.transform_string(url, direction)?;
                }
            }
            WebSearchAction::FindInPage { url, pattern } => {
                if let Some(url) = url {
                    self.transform_string(url, direction)?;
                }
                if let Some(pattern) = pattern {
                    self.transform_string(pattern, direction)?;
                }
            }
            WebSearchAction::Other => {}
        }
        Ok(())
    }

    fn transform_string(&mut self, text: &mut String, direction: TransformDirection) -> Result<()> {
        match direction {
            TransformDirection::Anonymize => *text = self.anonymize_text(text)?,
            TransformDirection::Deanonymize => *text = self.deanonymize_text(text),
        }
        Ok(())
    }

    fn anonymize_text(&mut self, text: &str) -> Result<String> {
        let spans = self.detector.detect(text)?;
        if spans.is_empty() {
            return Ok(text.to_string());
        }
        let mut output = String::with_capacity(text.len());
        let mut cursor = 0;
        for span in spans {
            output.push_str(&text[cursor..span.start]);
            let real = &text[span.start..span.end];
            let fake = self.fake_for(real, &span.label);
            output.push_str(&fake);
            cursor = span.end;
        }
        output.push_str(&text[cursor..]);
        Ok(output)
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

    fn fake_for(&mut self, real: &str, label: &str) -> String {
        if let Some(existing) = self.by_real.get(real) {
            return existing.fake.clone();
        }
        let fake = fake_value(real, label);
        let mapping = PrivacyMapping {
            real: real.to_string(),
            fake: fake.clone(),
            label: label.to_string(),
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

fn fake_value(real: &str, label: &str) -> String {
    let digest = sha1::Sha1::digest(format!("{label}\0{real}").as_bytes());
    match label {
        "private_email" => format!(
            "{}.{}{}@example.com",
            FIRST_NAMES[digest[0] as usize % FIRST_NAMES.len()].to_ascii_lowercase(),
            LAST_NAMES[digest[1] as usize % LAST_NAMES.len()].to_ascii_lowercase(),
            100 + digest[2] as u16 % 900
        ),
        "private_phone" => format!(
            "(555) {:03}-{:04}",
            200 + digest[0] as u16 % 700,
            1000 + u16::from_be_bytes([digest[1], digest[2]]) % 9000
        ),
        "private_address" => {
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
        "private_person" => format!(
            "{} {}",
            FIRST_NAMES[digest[0] as usize % FIRST_NAMES.len()],
            LAST_NAMES[digest[1] as usize % LAST_NAMES.len()]
        ),
        "private_url" => format!(
            "https://example{}.com/profile",
            100 + digest[0] as u16 % 900
        ),
        "private_date" => format!(
            "20{:02}-{:02}-{:02}",
            digest[0] as u16 % 30,
            1 + digest[1] as u16 % 12,
            1 + digest[2] as u16 % 28
        ),
        "account_number" | "secret" => format!(
            "acct-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            digest[0], digest[1], digest[2], digest[3], digest[4], digest[5]
        ),
        _ => format!(
            "record-{:02x}{:02x}{:02x}{:02x}",
            digest[0], digest[1], digest[2], digest[3]
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct StaticDetector {
        spans: Vec<OpfSpan>,
    }

    impl PrivacyDetector for StaticDetector {
        fn detect(&self, _text: &str) -> Result<Vec<OpfSpan>> {
            Ok(self.spans.clone())
        }
    }

    fn filter_with(spans: Vec<OpfSpan>) -> PrivacyFilter {
        PrivacyFilter::new(Arc::new(StaticDetector { spans }))
    }

    fn span(text: &str, needle: &str, label: &str) -> OpfSpan {
        let start = text.find(needle).expect("test span should exist");
        OpfSpan {
            label: label.to_string(),
            start,
            end: start + needle.len(),
            text: needle.to_string(),
        }
    }

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
    fn roundtrip_anonymizes_and_restores_model_detected_spans() {
        let text = "Tell Alice Smith at alice.smith@example.com about 742 Evergreen Street.";
        let mut filter = filter_with(vec![
            span(text, "Alice Smith", "private_person"),
            span(text, "alice.smith@example.com", "private_email"),
            span(text, "742 Evergreen Street", "private_address"),
        ]);
        let mut items = vec![user_message(text)];

        filter
            .anonymize_response_items(&mut items)
            .expect("anonymize");
        let serialized = serde_json::to_string(&items).expect("serialize items");
        assert!(!serialized.contains("Alice Smith"));
        assert!(!serialized.contains("alice.smith@example.com"));
        assert!(!serialized.contains("742 Evergreen Street"));
        assert!(serialized.contains("@example.com"));

        filter.deanonymize_response_item(&mut items[0]);
        assert_eq!(items, vec![user_message(text)]);
    }

    #[test]
    fn stable_mapping_reuses_fake_values() {
        let text = "Call Alice Smith.";
        let mut filter = filter_with(vec![span(text, "Alice Smith", "private_person")]);
        let first = filter.anonymize_text(text).expect("first");
        let second = filter.anonymize_text(text).expect("second");

        assert_eq!(first, second);
        assert!(!first.contains("Alice Smith"));
        assert_eq!(filter.deanonymize_text(&first), text);
    }

    #[test]
    fn mapping_is_not_serialized_in_outbound_payload_shape() {
        let text = "Alice Smith: alice.smith@example.com";
        let mut filter = filter_with(vec![
            span(text, "Alice Smith", "private_person"),
            span(text, "alice.smith@example.com", "private_email"),
        ]);
        let mut items = vec![ResponseItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(text.to_string()),
        }];

        filter
            .anonymize_response_items(&mut items)
            .expect("anonymize");
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

    #[test]
    #[ignore = "requires the local openai/privacy-filter OPF package and checkpoint"]
    fn real_openai_privacy_filter_model_roundtrip() {
        let original =
            "Ask Alice Smith to email alice.smith@example.com about 742 Evergreen Street.";
        let mut filter = PrivacyFilter::new(Arc::new(OpfCommandDetector));

        let anonymized = filter.anonymize_text(original).expect("real OPF anonymize");
        assert!(!anonymized.contains("Alice Smith"));
        assert!(!anonymized.contains("alice.smith@example.com"));

        let fake_name = filter
            .by_real
            .get("Alice Smith")
            .map(|mapping| mapping.fake.clone())
            .expect("real OPF should detect the person name");
        let fake_email = filter
            .by_real
            .get("alice.smith@example.com")
            .map(|mapping| mapping.fake.clone())
            .expect("real OPF should detect the email");
        let mut response = user_message(&format!(
            "I sent the update to {fake_name} at {fake_email}."
        ));
        let fake_response = serde_json::to_string(&response).expect("serialize fake response");
        filter.deanonymize_response_item(&mut response);
        let restored_response = serde_json::to_string(&response).expect("serialize restored");

        println!("proof_model={OPENAI_PRIVACY_FILTER_MODEL_ID}");
        println!("proof_original={original}");
        println!("proof_anonymized={anonymized}");
        println!("proof_model_response_fake={fake_response}");
        println!("proof_restored_response={restored_response}");

        assert!(fake_response.contains(&fake_name));
        assert!(fake_response.contains(&fake_email));
        assert!(restored_response.contains("Alice Smith"));
        assert!(restored_response.contains("alice.smith@example.com"));
    }
}
