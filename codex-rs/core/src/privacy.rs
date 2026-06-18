use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use rand::Rng;
use serde::Deserialize;
use serde::Serialize;
use sha1::Digest;
use sha1::Sha1;
use std::collections::HashMap;
use std::process::Command;

const ENABLE_ENV: &str = "PITCHAI_CODEX_PRIVACY_MIDDLEWARE";
const DETECTOR_CMD_ENV: &str = "PITCHAI_CODEX_PRIVACY_FILTER_CMD";
const DEFAULT_OPENAI_DETECTOR: &str =
    "python3 /code/pitchai-cli-new/vendor/codex/scripts/privacy_filter_openai.py";

#[derive(Debug)]
pub(crate) struct PrivacyFilter {
    enabled: bool,
    detector_cmd: Option<String>,
    secret: String,
    real_to_fake: HashMap<String, String>,
    fake_to_real: HashMap<String, String>,
    inbound_pending: String,
}

#[derive(Debug, Deserialize)]
struct DetectorOutput {
    spans: Vec<DetectedSpan>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct DetectedSpan {
    pub start: usize,
    pub end: usize,
    pub kind: String,
}

#[derive(Debug, Serialize)]
struct DetectorInput<'a> {
    text: &'a str,
}

impl PrivacyFilter {
    pub(crate) fn from_env() -> Self {
        let enabled = std::env::var(ENABLE_ENV)
            .map(|value| matches_enabled(value.as_str()))
            .unwrap_or(false);
        Self {
            enabled,
            detector_cmd: std::env::var(DETECTOR_CMD_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| enabled.then(|| DEFAULT_OPENAI_DETECTOR.to_string())),
            secret: random_secret(),
            real_to_fake: HashMap::new(),
            fake_to_real: HashMap::new(),
            inbound_pending: String::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            detector_cmd: None,
            secret: random_secret(),
            real_to_fake: HashMap::new(),
            fake_to_real: HashMap::new(),
            inbound_pending: String::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests(spans_json_command: String) -> Self {
        Self {
            enabled: true,
            detector_cmd: Some(spans_json_command),
            secret: "test-secret".to_string(),
            real_to_fake: HashMap::new(),
            fake_to_real: HashMap::new(),
            inbound_pending: String::new(),
        }
    }

    pub(crate) fn anonymize_items(&mut self, items: &mut [ResponseItem]) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        for item in items {
            self.anonymize_response_item(item)?;
        }
        Ok(())
    }

    pub(crate) fn de_anonymize_event(&mut self, event: &mut codex_api::ResponseEvent) {
        if !self.enabled {
            return;
        }
        match event {
            codex_api::ResponseEvent::OutputTextDelta(delta) => {
                *delta = self.de_anonymize_stream_delta(delta);
            }
            codex_api::ResponseEvent::OutputItemDone(item)
            | codex_api::ResponseEvent::OutputItemAdded(item) => {
                self.de_anonymize_response_item(item);
            }
            codex_api::ResponseEvent::Created
            | codex_api::ResponseEvent::ServerModel(_)
            | codex_api::ResponseEvent::ModelVerifications(_)
            | codex_api::ResponseEvent::TurnModerationMetadata(_)
            | codex_api::ResponseEvent::ServerReasoningIncluded(_)
            | codex_api::ResponseEvent::Completed { .. }
            | codex_api::ResponseEvent::ToolCallInputDelta { .. }
            | codex_api::ResponseEvent::ReasoningSummaryDelta { .. }
            | codex_api::ResponseEvent::ReasoningContentDelta { .. }
            | codex_api::ResponseEvent::ReasoningSummaryPartAdded { .. }
            | codex_api::ResponseEvent::RateLimits(_)
            | codex_api::ResponseEvent::ModelsEtag(_) => {}
        }
    }

    pub(crate) fn take_pending_de_anonymized_delta(&mut self) -> Option<String> {
        if self.inbound_pending.is_empty() {
            return None;
        }
        let pending = std::mem::take(&mut self.inbound_pending);
        Some(self.de_anonymize_text(&pending))
    }

    fn anonymize_response_item(&mut self, item: &mut ResponseItem) -> anyhow::Result<()> {
        match item {
            ResponseItem::Message { content, .. } => {
                for content_item in content {
                    if let ContentItem::InputText { text } | ContentItem::OutputText { text } =
                        content_item
                    {
                        *text = self.anonymize_text(text)?;
                    }
                }
            }
            ResponseItem::AgentMessage { content, .. } => {
                for content_item in content {
                    if let AgentMessageInputContent::InputText { text } = content_item {
                        *text = self.anonymize_text(text)?;
                    }
                }
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content) = output.content_items_mut() {
                    for content_item in content {
                        if let codex_protocol::models::FunctionCallOutputContentItem::InputText {
                            text,
                        } = content_item
                        {
                            *text = self.anonymize_text(text)?;
                        }
                    }
                }
            }
            ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => {}
        }
        Ok(())
    }

    fn de_anonymize_response_item(&self, item: &mut ResponseItem) {
        match item {
            ResponseItem::Message { content, .. } => {
                for content_item in content {
                    if let ContentItem::InputText { text } | ContentItem::OutputText { text } =
                        content_item
                    {
                        *text = self.de_anonymize_text(text);
                    }
                }
            }
            ResponseItem::AgentMessage { content, .. } => {
                for content_item in content {
                    if let AgentMessageInputContent::InputText { text } = content_item {
                        *text = self.de_anonymize_text(text);
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) fn anonymize_text(&mut self, text: &str) -> anyhow::Result<String> {
        if !self.enabled {
            return Ok(text.to_string());
        }
        if text.is_empty() {
            return Ok(String::new());
        }
        let mut spans = self.detect(text)?;
        spans.retain(|span| {
            span.start < span.end
                && span.end <= text.len()
                && text.is_char_boundary(span.start)
                && text.is_char_boundary(span.end)
        });
        spans.sort_by_key(|span| (span.start, span.end));
        spans = remove_overlaps(spans);
        if spans.is_empty() {
            return Ok(text.to_string());
        }

        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for span in spans {
            out.push_str(&text[cursor..span.start]);
            let real = &text[span.start..span.end];
            let fake = self.fake_for(real, &span.kind);
            out.push_str(&fake);
            cursor = span.end;
        }
        out.push_str(&text[cursor..]);
        Ok(out)
    }

    pub(crate) fn de_anonymize_text(&self, text: &str) -> String {
        if self.fake_to_real.is_empty() {
            return text.to_string();
        }
        let mut restored = text.to_string();
        let mut fake_values: Vec<_> = self.fake_to_real.keys().cloned().collect();
        fake_values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        for fake in fake_values {
            if let Some(real) = self.fake_to_real.get(&fake) {
                restored = restored.replace(&fake, real);
            }
        }
        restored
    }

    fn de_anonymize_stream_delta(&mut self, delta: &str) -> String {
        let combined = format!("{}{}", self.inbound_pending, delta);
        let restored = self.de_anonymize_text(&combined);
        let hold_len = self.longest_fake_prefix_suffix(&restored);
        let split_at = restored.len().saturating_sub(hold_len);
        self.inbound_pending = restored[split_at..].to_string();
        restored[..split_at].to_string()
    }

    fn longest_fake_prefix_suffix(&self, text: &str) -> usize {
        let mut longest = 0;
        for boundary in text.char_indices().map(|(idx, _)| idx).chain([text.len()]) {
            let suffix = &text[boundary..];
            if suffix.is_empty() {
                continue;
            }
            if self
                .fake_to_real
                .keys()
                .any(|fake| fake.starts_with(suffix) && fake.len() > suffix.len())
            {
                longest = longest.max(suffix.len());
            }
        }
        longest
    }

    fn detect(&self, text: &str) -> anyhow::Result<Vec<DetectedSpan>> {
        let Some(command) = &self.detector_cmd else {
            anyhow::bail!(
                "{ENABLE_ENV}=1 requires {DETECTOR_CMD_ENV}; refusing to send text upstream without a real local privacy detector"
            );
        };
        run_detector_command(command, text)
    }

    fn fake_for(&mut self, real: &str, kind: &str) -> String {
        let key = stable_key(&self.secret, real, kind);
        if let Some(fake) = self.real_to_fake.get(&key) {
            return fake.clone();
        }
        let mut fake = realistic_fake(kind, &key);
        let mut counter = 1;
        while self.fake_to_real.contains_key(&fake) {
            counter += 1;
            fake = format!("{fake} {counter}");
        }
        self.real_to_fake.insert(key, fake.clone());
        self.fake_to_real.insert(fake.clone(), real.to_string());
        fake
    }

    #[cfg(test)]
    pub(crate) fn mapping_debug_counts(&self) -> (usize, usize) {
        (self.real_to_fake.len(), self.fake_to_real.len())
    }
}

fn run_detector_command(command: &str, text: &str) -> anyhow::Result<Vec<DetectedSpan>> {
    let argv = shlex::split(command)
        .ok_or_else(|| anyhow::anyhow!("failed to parse {DETECTOR_CMD_ENV}"))?;
    let Some((program, args)) = argv.split_first() else {
        anyhow::bail!("{DETECTOR_CMD_ENV} is empty");
    };
    let input = serde_json::to_vec(&DetectorInput { text })?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("privacy detector stdin unavailable"))?
            .write_all(&input)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "privacy detector command failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let parsed: DetectorOutput = serde_json::from_slice(&output.stdout)?;
    Ok(parsed.spans)
}

fn remove_overlaps(spans: Vec<DetectedSpan>) -> Vec<DetectedSpan> {
    let mut kept: Vec<DetectedSpan> = Vec::new();
    let mut last_end = 0;
    for span in spans {
        if span.start >= last_end {
            last_end = span.end;
            kept.push(span);
        }
    }
    kept
}

fn matches_enabled(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn random_secret() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn stable_key(secret: &str, real: &str, kind: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(secret.as_bytes());
    hasher.update(b"\0");
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(real.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn realistic_fake(kind: &str, key: &str) -> String {
    let idx = usize::from_str_radix(&key[..6], 16).unwrap_or(0);
    let lower = kind.to_ascii_lowercase();
    if lower.contains("email") {
        return format!("{}.{}@example.net", first_name(idx), last_name(idx + 1))
            .to_ascii_lowercase();
    }
    if lower.contains("phone") {
        return format!("({}) 555-{:04}", 200 + (idx % 700), 1000 + (idx % 9000));
    }
    if lower.contains("address") || lower.contains("location") {
        return format!(
            "{} {} St, {}",
            100 + (idx % 8900),
            street_name(idx),
            city(idx)
        );
    }
    if lower.contains("name") || lower.contains("person") {
        return format!("{} {}", first_name(idx), last_name(idx + 1));
    }
    format!("{} {}", first_name(idx), last_name(idx + 1))
}

fn first_name(idx: usize) -> &'static str {
    const VALUES: &[&str] = &[
        "Avery", "Jordan", "Morgan", "Casey", "Riley", "Taylor", "Cameron", "Quinn",
    ];
    VALUES[idx % VALUES.len()]
}

fn last_name(idx: usize) -> &'static str {
    const VALUES: &[&str] = &[
        "Bennett", "Reed", "Hayes", "Carter", "Brooks", "Parker", "Sullivan", "Foster",
    ];
    VALUES[idx % VALUES.len()]
}

fn street_name(idx: usize) -> &'static str {
    const VALUES: &[&str] = &[
        "Maple", "Cedar", "Walnut", "Lake", "Hill", "Pine", "Oak", "River",
    ];
    VALUES[idx % VALUES.len()]
}

fn city(idx: usize) -> &'static str {
    const VALUES: &[&str] = &[
        "Springfield, IL",
        "Madison, WI",
        "Raleigh, NC",
        "Boulder, CO",
        "Portland, ME",
        "Eugene, OR",
        "Albany, NY",
        "Plano, TX",
    ];
    VALUES[idx % VALUES.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn detector_script() -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"#!/usr/bin/env python3
import json, sys
data = json.load(sys.stdin)
text = data["text"]
targets = [
    ("PERSON_NAME", "Jane Smith"),
    ("EMAIL", "jane.smith@example.com"),
    ("PHONE", "212-555-0199"),
    ("ADDRESS", "14 Pearl St, Boston, MA"),
]
spans = []
for kind, value in targets:
    start = 0
    while True:
        start = text.find(value, start)
        if start < 0:
            break
        spans.append({{"start": start, "end": start + len(value), "kind": kind}})
        start += len(value)
print(json.dumps({{"spans": spans}}))
"#
        )
        .unwrap();
        file
    }

    #[test]
    fn anonymizes_to_stable_realistic_fakes_and_restores() {
        let script = detector_script();
        let mut filter =
            PrivacyFilter::new_for_tests(format!("python3 {}", script.path().display()));
        let text = "Ask Jane Smith at jane.smith@example.com or 212-555-0199 about 14 Pearl St, Boston, MA. Jane Smith owns it.";
        let anonymized = filter.anonymize_text(text).unwrap();
        assert!(!anonymized.contains("Jane Smith"));
        assert!(!anonymized.contains("jane.smith@example.com"));
        assert!(!anonymized.contains("212-555-0199"));
        assert!(!anonymized.contains("14 Pearl St"));
        assert!(anonymized.contains("@example.net"));
        assert!(anonymized.contains("555-"));
        assert_eq!(filter.de_anonymize_text(&anonymized), text);

        let again = filter.anonymize_text("Jane Smith").unwrap();
        assert!(anonymized.contains(&again));
        assert_eq!(filter.mapping_debug_counts(), (4, 4));
    }

    #[test]
    fn disabled_mode_is_identity() {
        let mut filter = PrivacyFilter {
            enabled: false,
            detector_cmd: None,
            secret: "test".to_string(),
            real_to_fake: HashMap::new(),
            fake_to_real: HashMap::new(),
            inbound_pending: String::new(),
        };
        assert_eq!(filter.anonymize_text("Jane Smith").unwrap(), "Jane Smith");
        assert_eq!(filter.de_anonymize_text("Jane Smith"), "Jane Smith");
    }

    #[test]
    fn de_anonymizes_backend_like_response_events() {
        let script = detector_script();
        let mut filter =
            PrivacyFilter::new_for_tests(format!("python3 {}", script.path().display()));
        let outbound = filter.anonymize_text("Jane Smith").unwrap();
        let mut event = codex_api::ResponseEvent::OutputTextDelta(format!("Hello {outbound}"));
        filter.de_anonymize_event(&mut event);
        let codex_api::ResponseEvent::OutputTextDelta(delta) = event else {
            panic!("expected output text delta");
        };
        assert_eq!(delta, "Hello Jane Smith");
    }

    #[test]
    fn de_anonymizes_fake_values_split_across_streaming_chunks() {
        let script = detector_script();
        let mut filter =
            PrivacyFilter::new_for_tests(format!("python3 {}", script.path().display()));
        let fake = filter.anonymize_text("Jane Smith").unwrap();
        let split = fake.find(' ').unwrap_or(fake.len() / 2) + 1;
        let mut first = codex_api::ResponseEvent::OutputTextDelta(fake[..split].to_string());
        filter.de_anonymize_event(&mut first);
        let codex_api::ResponseEvent::OutputTextDelta(first_delta) = first else {
            panic!("expected first text delta");
        };
        assert_eq!(first_delta, "");

        let mut second = codex_api::ResponseEvent::OutputTextDelta(fake[split..].to_string());
        filter.de_anonymize_event(&mut second);
        let codex_api::ResponseEvent::OutputTextDelta(second_delta) = second else {
            panic!("expected second text delta");
        };
        assert_eq!(second_delta, "Jane Smith");
        assert_eq!(filter.take_pending_de_anonymized_delta(), None);
    }

    #[test]
    fn overlap_filter_keeps_outer_span_and_drops_inner_overlap() {
        let spans = remove_overlaps(vec![
            DetectedSpan {
                start: 0,
                end: 10,
                kind: "private_person".to_string(),
            },
            DetectedSpan {
                start: 5,
                end: 10,
                kind: "private_person".to_string(),
            },
            DetectedSpan {
                start: 20,
                end: 30,
                kind: "private_address".to_string(),
            },
        ]);
        assert_eq!(
            spans,
            vec![
                DetectedSpan {
                    start: 0,
                    end: 10,
                    kind: "private_person".to_string(),
                },
                DetectedSpan {
                    start: 20,
                    end: 30,
                    kind: "private_address".to_string(),
                },
            ]
        );
    }

    #[test]
    fn model_backed_detector_contract_when_configured() {
        let Ok(command) = std::env::var("PITCHAI_CODEX_PRIVACY_FILTER_CMD") else {
            eprintln!(
                "skipping real model detector contract test; PITCHAI_CODEX_PRIVACY_FILTER_CMD is unset"
            );
            return;
        };
        let mut filter = PrivacyFilter::new_for_tests(command);
        let original = "Jane Smith lives at 14 Pearl St, Boston, MA and uses jane.smith@example.com or 212-555-0199.";
        let outbound = filter.anonymize_text(original).unwrap();
        assert!(!outbound.contains("Jane Smith"));
        assert!(!outbound.contains("14 Pearl St, Boston, MA"));
        assert!(!outbound.contains("jane.smith@example.com"));
        assert!(!outbound.contains("212-555-0199"));
        assert!(outbound.contains("@example.net"));
        assert_eq!(filter.de_anonymize_text(&outbound), original);
    }
}
