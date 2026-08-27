use crate::completion_callback_metadata::canonical_completion_callback_metadata;
use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use codex_state::CompletionOutboxEvent;
use codex_state::CompletionStore;
use reqwest::Client;
use reqwest::Url;
use reqwest::redirect::Policy;
use serde::Serialize;
use serde_json::Value;
use std::env;
use std::io;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;
use tracing::warn;

const WEBHOOK_URL_ENV: &str = "PITCHAI_CODEX_APP_SERVER_COMPLETION_WEBHOOK_URL";
const WEBHOOK_TOKEN_ENV: &str = "PITCHAI_CODEX_APP_SERVER_COMPLETION_WEBHOOK_TOKEN";
const SOURCE_CELL_SLUG_ENV: &str = "PITCHAI_PLATFORM_CELL_SLUG";
const WEBHOOK_PROTOCOL_VERSION: &str = "pitchai-completion-webhook/v2";
const CLAIM_BATCH_SIZE: i64 = 8;
const CLAIM_LEASE_MS: i64 = 60_000;
const MAX_IDLE_SCAN_MS: i64 = 30_000;
const STORE_ERROR_RETRY_MS: i64 = 1_000;
const WEBHOOK_RETRY_BASE_MS: i64 = 1_000;
const WEBHOOK_RETRY_MAX_MS: i64 = 300_000;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MIN_WEBHOOK_TOKEN_CHARACTERS: usize = 32;
const MAX_WEBHOOK_CALLBACK_TEXT_CHARACTERS: usize = 4_096;
const MAX_RECORDED_ERROR_CHARACTERS: usize = 16_384;
const REDACTED_CALLBACK_TEXT: &str = "[redacted: callback text contained a potential secret]";
const TRUNCATED_CALLBACK_SUFFIX: &str = " … [truncated for private event ingestion]";

pub(crate) struct CompletionWebhookSenderConfig {
    endpoint: Url,
    token: String,
    source_cell_slug: Option<String>,
}

#[derive(Serialize)]
struct CompletionWebhookPayload<'a> {
    protocol_version: &'static str,
    event_id: &'a str,
    completion_work_id: &'a str,
    source_thread_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_cell_slug: Option<&'a str>,
    execution_kind: &'a str,
    execution_id: &'a str,
    terminal_status: &'a str,
    terminal_at: String,
    callback_metadata: &'a Value,
    final_output_reference: CompletionFinalOutputReference<'a>,
    correlation_id: &'a str,
}

#[derive(Serialize)]
struct CompletionFinalOutputReference<'a> {
    storage: &'static str,
    source_thread_id: &'a str,
    execution_kind: &'a str,
    execution_id: &'a str,
}

pub(crate) fn start(
    store: Option<CompletionStore>,
    shutdown: CancellationToken,
) -> io::Result<Option<JoinHandle<()>>> {
    let Some(config) = CompletionWebhookSenderConfig::from_environment()? else {
        return Ok(None);
    };
    let store = store.ok_or_else(|| {
        io::Error::other(
            "completion webhook is configured but the persistent app-server state database is unavailable",
        )
    })?;
    let endpoint = config.endpoint.clone();
    let client = Client::builder()
        .redirect(Policy::none())
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()
        .map_err(|err| {
            io::Error::other(format!("failed to build completion webhook client: {err}"))
        })?;
    let handle = tokio::spawn(async move {
        info!(%endpoint, "completion webhook producer started");
        run(store, config, client, shutdown).await;
        info!("completion webhook producer stopped");
    });
    Ok(Some(handle))
}

impl CompletionWebhookSenderConfig {
    fn from_environment() -> io::Result<Option<Self>> {
        let endpoint = environment_value(WEBHOOK_URL_ENV)?;
        let token = environment_value(WEBHOOK_TOKEN_ENV)?;
        let source_cell_slug = environment_value(SOURCE_CELL_SLUG_ENV)?;
        match endpoint {
            Some(endpoint) => {
                Self::from_values(&endpoint, token.as_deref(), source_cell_slug.as_deref())
                    .map(Some)
            }
            None if token.is_none() => Ok(None),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{WEBHOOK_TOKEN_ENV} requires {WEBHOOK_URL_ENV}"),
            )),
        }
    }

    fn from_values(
        endpoint: &str,
        token: Option<&str>,
        source_cell_slug: Option<&str>,
    ) -> io::Result<Self> {
        let endpoint = parse_webhook_url(endpoint)?;
        let token = token
            .ok_or_else(|| {
                invalid_configuration(format!(
                    "{WEBHOOK_TOKEN_ENV} is required when {WEBHOOK_URL_ENV} is configured"
                ))
            })
            .and_then(validate_secret)?
            .to_string();
        let source_cell_slug = source_cell_slug
            .map(validate_cell_slug)
            .transpose()?
            .map(str::to_string);
        Ok(Self {
            endpoint,
            token,
            source_cell_slug,
        })
    }
}

async fn run(
    store: CompletionStore,
    config: CompletionWebhookSenderConfig,
    client: Client,
    shutdown: CancellationToken,
) {
    let wakeup = store.wakeup();
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let events = match store
            .claim_webhook_outbox(CLAIM_BATCH_SIZE, CLAIM_LEASE_MS)
            .await
        {
            Ok(events) => events,
            Err(err) => {
                error!(error = %err, "failed to claim completion webhook outbox");
                wait_for_wakeup(
                    wakeup.as_ref(),
                    Duration::from_millis(STORE_ERROR_RETRY_MS as u64),
                    &shutdown,
                )
                .await;
                continue;
            }
        };
        if events.is_empty() {
            let delay_ms = store
                .next_webhook_outbox_wakeup_delay_ms(MAX_IDLE_SCAN_MS)
                .await
                .unwrap_or(STORE_ERROR_RETRY_MS);
            wait_for_wakeup(
                wakeup.as_ref(),
                Duration::from_millis(delay_ms as u64),
                &shutdown,
            )
            .await;
            continue;
        }
        for event in events {
            if shutdown.is_cancelled() {
                return;
            }
            deliver_event(&store, &config, &client, &event).await;
        }
    }
}

async fn deliver_event(
    store: &CompletionStore,
    config: &CompletionWebhookSenderConfig,
    client: &Client,
    event: &CompletionOutboxEvent,
) {
    let metadata = match callback_metadata(event) {
        Ok(metadata) => metadata,
        Err(error) => {
            finalize_invalid_event(store, event, &error).await;
            return;
        }
    };
    match post_event(config, client, event, &metadata).await {
        Ok(()) => match store
            .mark_webhook_attempted(&event.event_id, &event.lease_id, "")
            .await
        {
            Ok(true) => {
                info!(
                    event_id = %event.event_id,
                    completion_work_id = %event.completion_work_id,
                    attempt = event.attempt,
                    "completion webhook accepted"
                );
            }
            Ok(false) => {
                warn!(
                    event_id = %event.event_id,
                    attempt = event.attempt,
                    "completion webhook lease changed before acceptance was finalized"
                );
            }
            Err(err) => {
                error!(
                    event_id = %event.event_id,
                    attempt = event.attempt,
                    error = %err,
                    "failed to finalize accepted completion webhook"
                );
            }
        },
        Err(error) => {
            let stored_error = bounded_error(&error);
            let delay_ms = webhook_retry_delay_ms(event.attempt);
            match store
                .retry_webhook_later(&event.event_id, &event.lease_id, &stored_error, delay_ms)
                .await
            {
                Ok(true) => {
                    warn!(
                        event_id = %event.event_id,
                        completion_work_id = %event.completion_work_id,
                        attempt = event.attempt,
                        retry_delay_ms = delay_ms,
                        error = %stored_error,
                        "completion webhook failed and remains durably pending"
                    );
                }
                Ok(false) => {
                    warn!(
                        event_id = %event.event_id,
                        attempt = event.attempt,
                        "completion webhook lease changed before retry was scheduled"
                    );
                }
                Err(err) => {
                    error!(
                        event_id = %event.event_id,
                        attempt = event.attempt,
                        error = %err,
                        "failed to persist completion webhook retry"
                    );
                }
            }
        }
    }
}

async fn finalize_invalid_event(
    store: &CompletionStore,
    event: &CompletionOutboxEvent,
    error: &str,
) {
    let stored_error = bounded_error(error);
    match store
        .mark_webhook_attempted(&event.event_id, &event.lease_id, &stored_error)
        .await
    {
        Ok(true) => {
            warn!(
                event_id = %event.event_id,
                completion_work_id = %event.completion_work_id,
                attempt = event.attempt,
                error = %stored_error,
                "completion webhook metadata is invalid and cannot be retried"
            );
        }
        Ok(false) => {
            warn!(
                event_id = %event.event_id,
                attempt = event.attempt,
                "completion webhook lease changed before invalid metadata was finalized"
            );
        }
        Err(err) => {
            error!(
                event_id = %event.event_id,
                attempt = event.attempt,
                error = %err,
                "failed to finalize invalid completion webhook"
            );
        }
    }
}

fn webhook_retry_delay_ms(attempt: i64) -> i64 {
    let exponent = u32::try_from(attempt.saturating_sub(1).clamp(0, 9)).unwrap_or(9);
    WEBHOOK_RETRY_BASE_MS
        .saturating_mul(2_i64.saturating_pow(exponent))
        .min(WEBHOOK_RETRY_MAX_MS)
}

async fn post_event(
    config: &CompletionWebhookSenderConfig,
    client: &Client,
    event: &CompletionOutboxEvent,
    callback_metadata: &Value,
) -> Result<(), String> {
    let payload = CompletionWebhookPayload {
        protocol_version: WEBHOOK_PROTOCOL_VERSION,
        event_id: &event.event_id,
        completion_work_id: &event.completion_work_id,
        source_thread_id: &event.thread_id,
        source_cell_slug: config.source_cell_slug.as_deref(),
        execution_kind: &event.execution_kind,
        execution_id: &event.execution_id,
        terminal_status: &event.terminal_status,
        terminal_at: terminal_timestamp(event.terminal_at_ms)?,
        callback_metadata,
        final_output_reference: CompletionFinalOutputReference {
            storage: "pitchai_cli_app_server_state",
            source_thread_id: &event.thread_id,
            execution_kind: &event.execution_kind,
            execution_id: &event.execution_id,
        },
        correlation_id: &event.completion_work_id,
    };
    let mut request = client.post(config.endpoint.clone()).json(&payload);
    request = request.bearer_auth(&config.token);
    let response = request
        .send()
        .await
        .map_err(|err| format!("completion webhook transport failed: {err}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "completion webhook returned HTTP {}",
            response.status().as_u16()
        ))
    }
}

fn callback_metadata(event: &CompletionOutboxEvent) -> Result<Value, String> {
    if event.callback_metadata_json.is_empty() {
        return Err(
            "completion callback metadata is absent; webhook was not attempted".to_string(),
        );
    }
    let mut metadata: Value = serde_json::from_str(&event.callback_metadata_json)
        .map_err(|err| format!("stored completion callback metadata is invalid JSON: {err}"))?;
    canonical_completion_callback_metadata(Some(&event.completion_work_id), Some(&metadata))
        .map_err(|_| {
            "stored completion callback metadata violates the producer contract".to_string()
        })?;
    let text = metadata
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| "stored completion callback metadata has no text".to_string())?;
    let safe_text = safe_callback_text(text);
    let object = metadata
        .as_object_mut()
        .ok_or_else(|| "stored completion callback metadata is not an object".to_string())?;
    object.insert("text".to_string(), Value::String(safe_text));
    Ok(metadata)
}

fn safe_callback_text(text: &str) -> String {
    if contains_potential_secret(text) {
        return REDACTED_CALLBACK_TEXT.to_string();
    }
    let mut characters = text.chars();
    let bounded: String = characters
        .by_ref()
        .take(MAX_WEBHOOK_CALLBACK_TEXT_CHARACTERS)
        .collect();
    if characters.next().is_none() {
        bounded
    } else {
        format!("{bounded}{TRUNCATED_CALLBACK_SUFFIX}")
    }
}

fn contains_potential_secret(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if (lower.contains("-----begin") && lower.contains("private key-----"))
        || contains_bearer_credential(&lower)
        || contains_credential_url(text)
    {
        return true;
    }
    if text
        .split(|character: char| {
            character.is_whitespace() || matches!(character, ',' | ';' | '"' | '\'')
        })
        .any(looks_like_known_token)
    {
        return true;
    }
    text.lines().any(line_contains_assigned_secret)
}

fn contains_bearer_credential(text: &str) -> bool {
    text.split_once("bearer ").is_some_and(|(_, remainder)| {
        remainder
            .split_whitespace()
            .next()
            .is_some_and(|value| value.len() >= 12)
    })
}

fn contains_credential_url(text: &str) -> bool {
    text.split_whitespace().any(|word| {
        let Some((_, authority_and_path)) = word.split_once("://") else {
            return false;
        };
        authority_and_path
            .split_once('@')
            .is_some_and(|(userinfo, _)| userinfo.contains(':'))
    })
}

fn looks_like_known_token(raw: &str) -> bool {
    let token = raw.trim_matches(|character: char| {
        matches!(
            character,
            '(' | ')' | '[' | ']' | '{' | '}' | '.' | ':' | '='
        )
    });
    let lower = token.to_ascii_lowercase();
    [
        "sk-",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix) && token.len() >= prefix.len() + 12)
        || (token.starts_with("eyJ") && token.matches('.').count() == 2 && token.len() >= 32)
}

fn line_contains_assigned_secret(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "client_secret",
        "client-secret",
        "api_key",
        "api-key",
        "access_token",
        "refresh_token",
        "private_key",
    ]
    .iter()
    .any(|label| {
        let Some(index) = lower.find(label) else {
            return false;
        };
        let remainder = line[index + label.len()..].trim_start();
        let Some(value) = remainder
            .strip_prefix('=')
            .or_else(|| remainder.strip_prefix(':'))
            .map(str::trim)
        else {
            return false;
        };
        value
            .split_whitespace()
            .next()
            .is_some_and(|candidate| candidate.len() >= 8)
    })
}

fn terminal_timestamp(epoch_millis: i64) -> Result<String, String> {
    DateTime::<Utc>::from_timestamp_millis(epoch_millis)
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
        .ok_or_else(|| {
            "completion webhook terminal timestamp is outside the supported range".to_string()
        })
}

fn parse_webhook_url(raw: &str) -> io::Result<Url> {
    if raw.trim() != raw || raw.is_empty() {
        return Err(invalid_configuration(format!(
            "{WEBHOOK_URL_ENV} must be non-empty without surrounding whitespace"
        )));
    }
    let endpoint = Url::parse(raw).map_err(|err| {
        invalid_configuration(format!("{WEBHOOK_URL_ENV} must be an absolute URL: {err}"))
    })?;
    let loopback_http = endpoint.scheme() == "http"
        && endpoint
            .host_str()
            .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
    if endpoint.scheme() != "https" && !loopback_http {
        return Err(invalid_configuration(format!(
            "{WEBHOOK_URL_ENV} must use HTTPS, except for loopback HTTP"
        )));
    }
    if !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(invalid_configuration(format!(
            "{WEBHOOK_URL_ENV} must not contain credentials, a query, or a fragment"
        )));
    }
    Ok(endpoint)
}

fn validate_secret(value: &str) -> io::Result<&str> {
    if value.trim() != value
        || value.chars().count() < MIN_WEBHOOK_TOKEN_CHARACTERS
        || value.chars().any(char::is_whitespace)
    {
        return Err(invalid_configuration(format!(
            "{WEBHOOK_TOKEN_ENV} must contain at least {MIN_WEBHOOK_TOKEN_CHARACTERS} non-whitespace characters"
        )));
    }
    Ok(value)
}

fn validate_cell_slug(value: &str) -> io::Result<&str> {
    let valid = !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-');
    if !valid {
        return Err(invalid_configuration(format!(
            "{SOURCE_CELL_SLUG_ENV} is not a valid lowercase cell slug"
        )));
    }
    Ok(value)
}

fn environment_value(name: &str) -> io::Result<Option<String>> {
    match env::var(name) {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(invalid_configuration(format!(
            "{name} must contain valid UTF-8"
        ))),
    }
}

fn invalid_configuration(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn bounded_error(error: &str) -> String {
    error.chars().take(MAX_RECORDED_ERROR_CHARACTERS).collect()
}

async fn wait_for_wakeup(
    wakeup: &tokio::sync::Notify,
    duration: Duration,
    shutdown: &CancellationToken,
) {
    tokio::select! {
        () = wakeup.notified() => {}
        () = tokio::time::sleep(duration) => {}
        () = shutdown.cancelled() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::ThreadId;
    use codex_state::StateRuntime;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::time::Instant;
    use tokio::time::sleep;
    use tokio::time::timeout;

    const TOKEN: &str = "test-completion-webhook-token-20260727";

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FailureMode {
        HttpUnavailable,
        Disconnect,
        Timeout,
    }

    #[test]
    fn configuration_preserves_the_fixed_webhook_path() {
        let config = CompletionWebhookSenderConfig::from_values(
            "https://events.example.test/webhooks/pitchai/completions",
            Some(TOKEN),
            Some("dev-main"),
        )
        .expect("valid config");

        assert_eq!(
            config.endpoint.as_str(),
            "https://events.example.test/webhooks/pitchai/completions"
        );
        assert_eq!(config.token, TOKEN);
        assert_eq!(config.source_cell_slug.as_deref(), Some("dev-main"));
    }

    #[test]
    fn configuration_rejects_arbitrary_insecure_or_ambiguous_urls() {
        assert!(
            CompletionWebhookSenderConfig::from_values(
                "http://events.example.test/callback",
                None,
                None,
            )
            .is_err()
        );
        assert!(
            CompletionWebhookSenderConfig::from_values(
                "https://events.example.test/callback?target=ori",
                None,
                None,
            )
            .is_err()
        );
        assert!(
            CompletionWebhookSenderConfig::from_values(
                "http://127.0.0.1:8123/callback",
                Some(TOKEN),
                None,
            )
            .is_ok()
        );
        assert!(
            CompletionWebhookSenderConfig::from_values(
                "http://127.0.0.1:8123/callback",
                None,
                None,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn post_contains_metadata_but_no_recipient_or_routing_fields() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let endpoint = format!("http://{}/capture", listener.local_addr().expect("address"));
        let capture = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            loop {
                let mut buffer = [0_u8; 4096];
                let count = stream.read(&mut buffer).await.expect("read");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if complete_http_request(&request) {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("respond");
            String::from_utf8(request).expect("utf8")
        });
        let config =
            CompletionWebhookSenderConfig::from_values(&endpoint, Some(TOKEN), Some("dev-main"))
                .expect("config");
        let client = Client::builder().build().expect("client");
        let event = sample_event();
        let metadata = callback_metadata(&event).expect("metadata");

        post_event(&config, &client, &event, &metadata)
            .await
            .expect("post");
        let request = capture.await.expect("capture");

        assert!(request.contains("POST /capture HTTP/1.1"));
        assert!(request.contains("authorization: Bearer test-completion-webhook-token-20260727"));
        assert!(request.contains("\"protocol_version\":\"pitchai-completion-webhook/v2\""));
        assert!(request.contains("\"callback_metadata\""));
        assert!(request.contains("\"source_agent_id\":\"worker-agent\""));
        assert!(request.contains("\"project_title\":\"PitchAI Infrastructure\""));
        assert!(request.contains("\"final_output_reference\""));
        assert!(!request.contains("\"final_text\""));
        assert!(!request.contains("\"target\""));
        assert!(!request.contains("\"recipient\""));
        assert!(!request.contains("\"route\""));
    }

    #[test]
    fn metadata_requires_the_strict_producer_shape() {
        let mut event = sample_event();
        event.callback_metadata_json = json!({
            "protocol_version": "pitchai-completion-callback/v1",
            "text": "Publish this result.",
            "target": "ori"
        })
        .to_string();

        assert!(callback_metadata(&event).is_err());
    }

    #[tokio::test]
    async fn failed_webhooks_remain_durably_pending_without_blocking_central_completion() {
        for failure_mode in [
            FailureMode::HttpUnavailable,
            FailureMode::Disconnect,
            FailureMode::Timeout,
        ] {
            let temp_dir = TempDir::new().expect("temp dir");
            let runtime =
                StateRuntime::init(temp_dir.path().to_path_buf(), "test-provider".to_string())
                    .await
                    .expect("state runtime");
            let store = runtime.completions().clone();
            let event = sample_event();
            let thread_id = ThreadId::from_string(&event.thread_id).expect("thread id");
            store
                .bind_turn_with_callback_metadata(
                    &event.completion_work_id,
                    thread_id,
                    &event.execution_id,
                    &event.callback_metadata_json,
                )
                .await
                .expect("binding");
            store
                .complete_turn(
                    thread_id,
                    &event.execution_id,
                    &event.final_text,
                    event.terminal_at_ms,
                )
                .await
                .expect("completion");

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let endpoint = format!("http://{}/capture", listener.local_addr().expect("address"));
            let capture = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0_u8; 4096];
                    let count = stream.read(&mut buffer).await.expect("read");
                    request.extend_from_slice(&buffer[..count]);
                    if count == 0 || complete_http_request(&request) {
                        break;
                    }
                }
                match failure_mode {
                    FailureMode::HttpUnavailable => {
                        stream
                            .write_all(
                                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await
                            .expect("respond");
                    }
                    FailureMode::Disconnect => {}
                    FailureMode::Timeout => sleep(Duration::from_millis(250)).await,
                }
                request
            });
            let config = CompletionWebhookSenderConfig::from_values(
                &endpoint,
                Some(TOKEN),
                Some("dev-main"),
            )
            .expect("config");
            let request_timeout = if failure_mode == FailureMode::Timeout {
                Duration::from_millis(100)
            } else {
                Duration::from_secs(5)
            };
            let client = Client::builder()
                .timeout(request_timeout)
                .build()
                .expect("client");
            let shutdown = CancellationToken::new();
            let sender = tokio::spawn(run(store.clone(), config, client, shutdown.clone()));

            let request = timeout(Duration::from_secs(5), capture)
                .await
                .expect("one webhook request should arrive")
                .expect("capture task");
            assert!(complete_http_request(&request));
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let stats = store.webhook_outbox_stats().await.expect("stats");
                if stats.pending_count == 1 && stats.total_attempts == 1 {
                    assert_eq!(1, stats.total_attempts);
                    assert_eq!(0, stats.sending_count);
                    assert_eq!(0, stats.sent_count);
                    break;
                }
                assert!(Instant::now() < deadline, "retry was not persisted");
                sleep(Duration::from_millis(20)).await;
            }
            let central_stats = store.outbox_stats().await.expect("central stats");
            assert_eq!(1, central_stats.pending_count);
            assert_eq!(0, central_stats.total_attempts);

            shutdown.cancel();
            sender.await.expect("sender");
        }
    }

    #[test]
    fn webhook_retry_delay_is_exponential_and_caps_at_five_minutes() {
        assert_eq!(1_000, webhook_retry_delay_ms(1));
        assert_eq!(256_000, webhook_retry_delay_ms(9));
        assert_eq!(WEBHOOK_RETRY_MAX_MS, webhook_retry_delay_ms(10));
        assert_eq!(WEBHOOK_RETRY_MAX_MS, webhook_retry_delay_ms(i64::MAX));
    }

    #[test]
    fn callback_text_is_bounded_and_secret_assignments_are_removed() {
        let long_text = "a".repeat(MAX_WEBHOOK_CALLBACK_TEXT_CHARACTERS + 1);

        assert!(safe_callback_text(&long_text).ends_with(TRUNCATED_CALLBACK_SUFFIX));
        assert_eq!(
            REDACTED_CALLBACK_TEXT,
            safe_callback_text("client_secret=supersecretvalue")
        );
        assert_eq!(
            REDACTED_CALLBACK_TEXT,
            safe_callback_text("Use Bearer abcdefghijklmnopqrstuvwxyz")
        );
        assert_eq!(
            "Report whether Jef's password reset completed.",
            safe_callback_text("Report whether Jef's password reset completed.")
        );
    }

    fn sample_event() -> CompletionOutboxEvent {
        CompletionOutboxEvent {
            event_id: "10000000-0000-0000-0000-000000000001".to_string(),
            completion_work_id: "10000000-0000-0000-0000-000000000001".to_string(),
            thread_id: "20000000-0000-0000-0000-000000000001".to_string(),
            execution_kind: "normal".to_string(),
            execution_id: "10000000-0000-0000-0000-000000000001".to_string(),
            terminal_turn_id: None,
            callback_metadata_json: json!({
                "protocol_version": "pitchai-completion-callback/v2",
                "text": "Publish this result.",
                "context": {
                    "source_agent_id": "worker-agent",
                    "project_id": "pitchai_infrastructure",
                    "project_title": "PitchAI Infrastructure",
                    "command_work_id": "10000000-0000-0000-0000-000000000001",
                    "origin_actor_kind": "agent",
                    "origin_agent_id": "ori",
                    "origin_source_ref_kind": "codex_thread",
                    "origin_source_ref_id": "40000000-0000-0000-0000-000000000001"
                }
            })
            .to_string(),
            terminal_status: "completed".to_string(),
            final_text: "Done.".to_string(),
            terminal_at_ms: 1_700_000_000_000,
            attempt: 1,
            lease_id: "30000000-0000-0000-0000-000000000001".to_string(),
        }
    }

    fn complete_http_request(request: &[u8]) -> bool {
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            })
            .unwrap_or_default();
        request.len() >= header_end + 4 + content_length
    }
}
