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
const WEBHOOK_PROTOCOL_VERSION: &str = "pitchai-completion-webhook/v1";
const CLAIM_BATCH_SIZE: i64 = 8;
const CLAIM_LEASE_MS: i64 = 60_000;
const MAX_IDLE_SCAN_MS: i64 = 30_000;
const STORE_ERROR_RETRY_MS: i64 = 1_000;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CALLBACK_TEXT_CHARACTERS: usize = 65_536;
const MAX_RECORDED_ERROR_CHARACTERS: usize = 16_384;

pub(crate) struct CompletionWebhookSenderConfig {
    endpoint: Url,
    token: Option<String>,
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
    final_text: &'a str,
    terminal_at: String,
    callback_metadata: &'a Value,
    correlation_id: &'a str,
}

pub(crate) fn start(
    store: Option<CompletionStore>,
    shutdown: CancellationToken,
) -> io::Result<Option<JoinHandle<()>>> {
    let Some(config) = CompletionOutboxSenderConfig::from_environment()? else {
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
        let token = token.map(validate_secret).transpose()?.map(str::to_string);
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
    let attempt_error = match callback_metadata(event) {
        Ok(metadata) => post_event(config, client, event, &metadata)
            .await
            .err()
            .map(|error| bounded_error(&error)),
        Err(error) => Some(bounded_error(&error)),
    };
    let stored_error = attempt_error.as_deref().unwrap_or_default();
    match store
        .mark_webhook_attempted(&event.event_id, &event.lease_id, stored_error)
        .await
    {
        Ok(true) if attempt_error.is_none() => {
            info!(
                event_id = %event.event_id,
                completion_work_id = %event.completion_work_id,
                attempt = event.attempt,
                "completion webhook accepted"
            );
        }
        Ok(true) => {
            warn!(
                event_id = %event.event_id,
                completion_work_id = %event.completion_work_id,
                attempt = event.attempt,
                error = %stored_error,
                "completion webhook attempt failed and was finalized without retry"
            );
        }
        Ok(false) => {
            warn!(
                event_id = %event.event_id,
                attempt = event.attempt,
                "completion webhook lease changed before the attempt was finalized"
            );
        }
        Err(err) => {
            error!(
                event_id = %event.event_id,
                attempt = event.attempt,
                error = %err,
                "failed to finalize completion webhook attempt"
            );
        }
    }
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
        final_text: &event.final_text,
        terminal_at: terminal_timestamp(event.terminal_at_ms)?,
        callback_metadata,
        correlation_id: &event.completion_work_id,
    };
    let mut request = client.post(config.endpoint.clone()).json(&payload);
    if let Some(token) = config.token.as_deref() {
        request = request.bearer_auth(token);
    }
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
    let metadata: Value = serde_json::from_str(&event.callback_metadata_json)
        .map_err(|err| format!("stored completion callback metadata is invalid JSON: {err}"))?;
    let object = metadata
        .as_object()
        .ok_or_else(|| "stored completion callback metadata is not an object".to_string())?;
    let text = object.get("text").and_then(Value::as_str);
    let invalid_text = text.is_none_or(|text| {
        text.trim().is_empty() || text.chars().count() > MAX_CALLBACK_TEXT_CHARACTERS
    });
    if object.len() != 2
        || object.get("protocol_version").and_then(Value::as_str)
            != Some("pitchai-completion-callback/v1")
        || invalid_text
    {
        return Err(
            "stored completion callback metadata violates the producer contract".to_string(),
        );
    }
    Ok(metadata)
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
    if value.trim() != value || value.is_empty() || value.chars().any(char::is_control) {
        return Err(invalid_configuration(format!(
            "{WEBHOOK_TOKEN_ENV} must be non-empty, unpadded, and contain no control characters"
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

    const TOKEN: &str = "test-completion-webhook-token";

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
        assert_eq!(config.token.as_deref(), Some(TOKEN));
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
                None,
                None,
            )
            .is_ok()
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
        assert!(request.contains("authorization: Bearer test-completion-webhook-token"));
        assert!(request.contains("\"protocol_version\":\"pitchai-completion-webhook/v1\""));
        assert!(request.contains("\"callback_metadata\""));
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
    async fn failed_webhook_is_attempted_once_without_blocking_central_completion() {
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
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("respond");
            request
        });
        let config =
            CompletionWebhookSenderConfig::from_values(&endpoint, Some(TOKEN), Some("dev-main"))
                .expect("config");
        let shutdown = CancellationToken::new();
        let sender = tokio::spawn(run(
            store.clone(),
            config,
            Client::builder().build().expect("client"),
            shutdown.clone(),
        ));

        let request = timeout(Duration::from_secs(5), capture)
            .await
            .expect("one webhook request should arrive")
            .expect("capture task");
        assert!(complete_http_request(&request));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let stats = store.webhook_outbox_stats().await.expect("stats");
            if stats.sent_count == 1 {
                assert_eq!(1, stats.total_attempts);
                assert_eq!(0, stats.pending_count);
                assert_eq!(0, stats.sending_count);
                break;
            }
            assert!(Instant::now() < deadline, "attempt was not finalized");
            sleep(Duration::from_millis(20)).await;
        }
        let central_stats = store.outbox_stats().await.expect("central stats");
        assert_eq!(1, central_stats.pending_count);
        assert_eq!(0, central_stats.total_attempts);

        shutdown.cancel();
        sender.await.expect("sender");
    }

    fn sample_event() -> CompletionOutboxEvent {
        CompletionOutboxEvent {
            event_id: "10000000-0000-0000-0000-000000000001".to_string(),
            completion_work_id: "10000000-0000-0000-0000-000000000001".to_string(),
            thread_id: "20000000-0000-0000-0000-000000000001".to_string(),
            execution_kind: "normal".to_string(),
            execution_id: "10000000-0000-0000-0000-000000000001".to_string(),
            callback_metadata_json: json!({
                "protocol_version": "pitchai-completion-callback/v1",
                "text": "Publish this result."
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
