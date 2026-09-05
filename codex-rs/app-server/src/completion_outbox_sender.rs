use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use codex_state::CompletionOutboxEvent;
use codex_state::CompletionStore;
use reqwest::Client;
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::redirect::Policy;
use serde::Deserialize;
use serde::Serialize;
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::Hash;
use std::hash::Hasher;
use std::io;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

const CENTRAL_URL_ENV: &str = "PITCHAI_PLATFORM_CENTRAL_URL";
const CELL_TOKEN_ENV: &str = "PITCHAI_PLATFORM_CELL_TOKEN";
const COMPLETION_EVENT_PATH: &str = "/internal/cell-api/v2/completion-events";
const COMPLETION_PROTOCOL_V1: &str = "pitchai-completion/v1";
const COMPLETION_PROTOCOL_V2: &str = "pitchai-completion/v2";
const CLAIM_BATCH_SIZE: i64 = 8;
const CLAIM_LEASE_MS: i64 = 60_000;
const MAX_IDLE_SCAN_MS: i64 = 30_000;
const RETRY_BASE_MS: i64 = 1_000;
const RETRY_CAP_MS: i64 = 300_000;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MIN_CELL_TOKEN_LENGTH: usize = 32;

pub(crate) struct CompletionOutboxSenderConfig {
    endpoint: Url,
    cell_token: String,
}

#[derive(Serialize)]
struct CompletionEventPayload<'a> {
    protocol_version: &'static str,
    boot_id: Uuid,
    event_id: &'a str,
    completion_work_id: &'a str,
    source_thread_id: &'a str,
    execution_kind: &'a str,
    execution_id: &'a str,
    terminal_turn_id: Option<&'a str>,
    terminal_status: &'a str,
    final_text: &'a str,
    terminal_at: String,
    correlation_id: Uuid,
}

#[derive(Deserialize)]
struct CompletionEventReceipt {
    outcome: CompletionEventOutcome,
    event_id: Uuid,
    completion_work_id: Uuid,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum CompletionEventOutcome {
    Accepted,
    Duplicate,
}

enum CompletionDeliveryFailure {
    Retryable(String),
    Permanent(String),
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
            "completion delivery is configured but the persistent app-server state database is unavailable",
        )
    })?;
    let endpoint = config.endpoint.clone();
    let client = Client::builder()
        .redirect(Policy::none())
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()
        .map_err(|err| io::Error::other(format!("failed to build callback HTTP client: {err}")))?;
    let boot_id = Uuid::now_v7();
    let handle = tokio::spawn(async move {
        info!(%boot_id, %endpoint, "completion outbox sender started");
        run(store, config, client, boot_id, shutdown).await;
        info!(%boot_id, "completion outbox sender stopped");
    });
    Ok(Some(handle))
}

impl CompletionOutboxSenderConfig {
    fn from_environment() -> io::Result<Option<Self>> {
        let central_url = environment_value(CENTRAL_URL_ENV)?;
        let cell_token = environment_value(CELL_TOKEN_ENV)?;
        match (central_url, cell_token) {
            (None, None) => Ok(None),
            (Some(central_url), Some(cell_token)) => {
                Self::from_values(&central_url, &cell_token).map(Some)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{CENTRAL_URL_ENV} and {CELL_TOKEN_ENV} must either both be set or both be absent"
                ),
            )),
        }
    }

    fn from_values(central_url: &str, cell_token: &str) -> io::Result<Self> {
        if central_url.trim() != central_url || central_url.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{CENTRAL_URL_ENV} must be a non-empty URL without surrounding whitespace"),
            ));
        }
        if cell_token.trim() != cell_token
            || cell_token.len() < MIN_CELL_TOKEN_LENGTH
            || cell_token.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{CELL_TOKEN_ENV} must contain at least {MIN_CELL_TOKEN_LENGTH} non-padded characters"
                ),
            ));
        }
        let mut endpoint = Url::parse(central_url).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{CENTRAL_URL_ENV} is not a valid absolute URL: {err}"),
            )
        })?;
        validate_central_origin(&endpoint)?;
        endpoint.set_path(COMPLETION_EVENT_PATH);
        Ok(Self {
            endpoint,
            cell_token: cell_token.to_string(),
        })
    }
}

async fn run(
    store: CompletionStore,
    config: CompletionOutboxSenderConfig,
    client: Client,
    boot_id: Uuid,
    shutdown: CancellationToken,
) {
    let wakeup = store.wakeup();
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let events = match store.claim_outbox(CLAIM_BATCH_SIZE, CLAIM_LEASE_MS).await {
            Ok(events) => events,
            Err(err) => {
                error!(error = %err, "failed to claim completion outbox");
                wait_for_wakeup(
                    wakeup.as_ref(),
                    Duration::from_millis(RETRY_BASE_MS as u64),
                    &shutdown,
                )
                .await;
                continue;
            }
        };
        if events.is_empty() {
            let delay_ms = match store.next_outbox_wakeup_delay_ms(MAX_IDLE_SCAN_MS).await {
                Ok(delay_ms) => delay_ms,
                Err(err) => {
                    warn!(error = %err, "failed to read next completion outbox wakeup");
                    RETRY_BASE_MS
                }
            };
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
            deliver_event(&store, &config, &client, boot_id, &event).await;
        }
    }
}

async fn deliver_event(
    store: &CompletionStore,
    config: &CompletionOutboxSenderConfig,
    client: &Client,
    boot_id: Uuid,
    event: &CompletionOutboxEvent,
) {
    let delivery = post_event(config, client, boot_id, event).await;
    match delivery {
        Ok(()) => match store.mark_sent(&event.event_id, &event.lease_id).await {
            Ok(true) => {
                info!(
                    event_id = %event.event_id,
                    completion_work_id = %event.completion_work_id,
                    attempt = event.attempt,
                    "completion event accepted by central"
                );
            }
            Ok(false) => {
                warn!(
                    event_id = %event.event_id,
                    attempt = event.attempt,
                    "completion outbox lease changed before sent state was recorded"
                );
            }
            Err(err) => {
                error!(
                    event_id = %event.event_id,
                    attempt = event.attempt,
                    error = %err,
                    "failed to mark accepted completion event as sent"
                );
            }
        },
        Err(CompletionDeliveryFailure::Permanent(err)) => {
            let stored_error = bounded_error(&err);
            match store
                .mark_undeliverable(&event.event_id, &event.lease_id, &stored_error)
                .await
            {
                Ok(true) => {
                    warn!(
                        event_id = %event.event_id,
                        completion_work_id = %event.completion_work_id,
                        attempt = event.attempt,
                        error = %stored_error,
                        "completion event was permanently rejected by central"
                    );
                }
                Ok(false) => {
                    warn!(
                        event_id = %event.event_id,
                        attempt = event.attempt,
                        "completion outbox lease changed before permanent rejection was recorded"
                    );
                }
                Err(store_err) => {
                    error!(
                        event_id = %event.event_id,
                        attempt = event.attempt,
                        error = %store_err,
                        "failed to persist permanent completion rejection"
                    );
                }
            }
        }
        Err(CompletionDeliveryFailure::Retryable(err)) => {
            let delay_ms = retry_delay_ms(event);
            match store
                .retry_later(
                    &event.event_id,
                    &event.lease_id,
                    &bounded_error(&err),
                    delay_ms,
                )
                .await
            {
                Ok(true) => {
                    warn!(
                        event_id = %event.event_id,
                        completion_work_id = %event.completion_work_id,
                        attempt = event.attempt,
                        retry_delay_ms = delay_ms,
                        error = %err,
                        "completion event delivery will retry"
                    );
                }
                Ok(false) => {
                    warn!(
                        event_id = %event.event_id,
                        attempt = event.attempt,
                        "completion outbox lease changed before retry was recorded"
                    );
                }
                Err(store_err) => {
                    error!(
                        event_id = %event.event_id,
                        attempt = event.attempt,
                        error = %store_err,
                        "failed to persist completion event retry"
                    );
                }
            }
        }
    }
}

async fn post_event(
    config: &CompletionOutboxSenderConfig,
    client: &Client,
    boot_id: Uuid,
    event: &CompletionOutboxEvent,
) -> Result<(), CompletionDeliveryFailure> {
    let terminal_at = DateTime::<Utc>::from_timestamp_millis(event.terminal_at_ms)
        .ok_or_else(|| {
            CompletionDeliveryFailure::Permanent(
                "completion event has an invalid terminal timestamp".to_string(),
            )
        })?
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let terminal_turn_id = terminal_turn_id(event).map_err(CompletionDeliveryFailure::Permanent)?;
    let payload = CompletionEventPayload {
        protocol_version: if terminal_turn_id.is_some() {
            COMPLETION_PROTOCOL_V2
        } else {
            COMPLETION_PROTOCOL_V1
        },
        boot_id,
        event_id: &event.event_id,
        completion_work_id: &event.completion_work_id,
        source_thread_id: &event.thread_id,
        execution_kind: &event.execution_kind,
        execution_id: &event.execution_id,
        terminal_turn_id,
        terminal_status: &event.terminal_status,
        final_text: &event.final_text,
        terminal_at,
        correlation_id: Uuid::now_v7(),
    };
    let response = client
        .post(config.endpoint.clone())
        .bearer_auth(&config.cell_token)
        .json(&payload)
        .send()
        .await
        .map_err(|err| {
            CompletionDeliveryFailure::Retryable(format!(
                "central completion endpoint transport failed: {err}"
            ))
        })?;
    let status = response.status();
    let content_length = response.content_length();
    if content_length.is_some_and(|length| length > MAX_RESPONSE_BYTES as u64) {
        return Err(CompletionDeliveryFailure::Retryable(
            "central completion endpoint response was too large".to_string(),
        ));
    }
    let response_body = response.bytes().await.map_err(|err| {
        CompletionDeliveryFailure::Retryable(format!(
            "failed to read central completion response: {err}"
        ))
    })?;
    if response_body.len() > MAX_RESPONSE_BYTES {
        return Err(CompletionDeliveryFailure::Retryable(
            "central completion endpoint response was too large".to_string(),
        ));
    }
    if !status.is_success() {
        let failure = http_failure(status);
        if permanent_rejection_code(status, &response_body).is_some() {
            return Err(CompletionDeliveryFailure::Permanent(failure));
        }
        return Err(CompletionDeliveryFailure::Retryable(failure));
    }
    let receipt: CompletionEventReceipt =
        serde_json::from_slice(&response_body).map_err(|err| {
            CompletionDeliveryFailure::Retryable(format!(
                "central completion endpoint returned an invalid receipt: {err}"
            ))
        })?;
    let expected_event_id = Uuid::parse_str(&event.event_id).map_err(|err| {
        CompletionDeliveryFailure::Permanent(format!("completion event id is not a UUID: {err}"))
    })?;
    let expected_work_id = Uuid::parse_str(&event.completion_work_id).map_err(|err| {
        CompletionDeliveryFailure::Permanent(format!("completion work id is not a UUID: {err}"))
    })?;
    if receipt.event_id != expected_event_id || receipt.completion_work_id != expected_work_id {
        return Err(CompletionDeliveryFailure::Retryable(
            "central completion receipt did not match the posted event".to_string(),
        ));
    }
    match receipt.outcome {
        CompletionEventOutcome::Accepted | CompletionEventOutcome::Duplicate => Ok(()),
    }
}

fn terminal_turn_id(event: &CompletionOutboxEvent) -> Result<Option<&str>, String> {
    match event.execution_kind.as_str() {
        "normal" => {
            if event
                .terminal_turn_id
                .as_deref()
                .is_some_and(|turn_id| turn_id != event.execution_id)
            {
                return Err(
                    "normal completion terminal turn does not match its execution id".to_string(),
                );
            }
            Ok(Some(event.execution_id.as_str()))
        }
        "goal" => {
            if event.terminal_turn_id.is_none() && !event.final_text.trim().is_empty() {
                return Err(
                    "goal completion with final text has no terminal turn identity".to_string(),
                );
            }
            Ok(event.terminal_turn_id.as_deref())
        }
        _ => Err("completion event has an unsupported execution kind".to_string()),
    }
}

async fn wait_for_wakeup(
    wakeup: &tokio::sync::Notify,
    delay: Duration,
    shutdown: &CancellationToken,
) {
    tokio::select! {
        () = wakeup.notified() => {}
        () = sleep(delay) => {}
        () = shutdown.cancelled() => {}
    }
}

fn validate_central_origin(url: &Url) -> io::Result<()> {
    let host = url.host_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{CENTRAL_URL_ENV} must include a host"),
        )
    })?;
    let loopback_host = matches!(host, "127.0.0.1" | "::1" | "localhost");
    let valid_scheme = url.scheme() == "https" || (url.scheme() == "http" && loopback_host);
    let root_path = matches!(url.path(), "" | "/");
    if !valid_scheme
        || !root_path
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{CENTRAL_URL_ENV} must be an HTTPS service origin without credentials, path, query, or fragment"
            ),
        ));
    }
    Ok(())
}

fn environment_value(name: &str) -> io::Result<Option<String>> {
    let Some(value) = env::var_os(name) else {
        return Ok(None);
    };
    let value = value.into_string().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must contain valid UTF-8"),
        )
    })?;
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn retry_delay_ms(event: &CompletionOutboxEvent) -> i64 {
    let exponent = event.attempt.saturating_sub(1).clamp(0, 8) as u32;
    let base_delay = RETRY_BASE_MS
        .saturating_mul(2_i64.saturating_pow(exponent))
        .min(RETRY_CAP_MS);
    let mut hasher = DefaultHasher::new();
    event.event_id.hash(&mut hasher);
    event.attempt.hash(&mut hasher);
    let jitter_limit = (base_delay / 4).max(1) as u64;
    let jitter = (hasher.finish() % jitter_limit) as i64;
    base_delay.saturating_add(jitter).min(RETRY_CAP_MS)
}

fn bounded_error(error: &str) -> String {
    error.chars().take(1_024).collect()
}

fn http_failure(status: StatusCode) -> String {
    format!(
        "central completion endpoint returned HTTP {}",
        status.as_u16()
    )
}

fn permanent_rejection_code(status: StatusCode, body: &[u8]) -> Option<&str> {
    if status != StatusCode::NOT_FOUND {
        return None;
    }
    let payload: serde_json::Value = serde_json::from_slice(body).ok()?;
    let code = payload.get("detail")?.get("code")?.as_str()?;
    (code == "command_not_found").then_some("command_not_found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::Router;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::http::StatusCode as AxumStatusCode;
    use axum::response::IntoResponse;
    use axum::response::Response;
    use axum::routing::post;
    use codex_protocol::ThreadId;
    use codex_state::StateRuntime;
    use serde_json::Value;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio::time::Instant;

    #[derive(Clone)]
    struct TestEndpoint {
        attempts: Arc<AtomicUsize>,
        expected_token: String,
    }

    #[test]
    fn configuration_requires_a_trusted_origin_and_token() {
        let token = "x".repeat(MIN_CELL_TOKEN_LENGTH);
        let config =
            CompletionOutboxSenderConfig::from_values("https://dispatch.pitchai.net", &token)
                .expect("valid central origin should be accepted");
        assert_eq!(
            "https://dispatch.pitchai.net/internal/cell-api/v2/completion-events",
            config.endpoint.as_str()
        );
        assert!(
            CompletionOutboxSenderConfig::from_values("http://dispatch.pitchai.net", &token,)
                .is_err()
        );
        assert!(
            CompletionOutboxSenderConfig::from_values(
                "https://dispatch.pitchai.net/untrusted",
                &token,
            )
            .is_err()
        );
        assert!(
            CompletionOutboxSenderConfig::from_values("http://127.0.0.1:8080", &token,).is_ok()
        );
    }

    #[test]
    fn retry_delay_is_bounded_and_stable() {
        let event = CompletionOutboxEvent {
            event_id: "10000000-0000-0000-0000-000000000001".to_string(),
            completion_work_id: "10000000-0000-0000-0000-000000000001".to_string(),
            thread_id: "20000000-0000-0000-0000-000000000001".to_string(),
            execution_kind: "normal".to_string(),
            execution_id: "turn-1".to_string(),
            terminal_turn_id: None,
            callback_metadata_json: String::new(),
            terminal_status: "completed".to_string(),
            final_text: "done".to_string(),
            terminal_at_ms: 1_000,
            attempt: 1,
            lease_id: "lease-1".to_string(),
        };
        let first_delay = retry_delay_ms(&event);
        assert!(first_delay >= RETRY_BASE_MS);
        assert!(first_delay <= RETRY_BASE_MS + (RETRY_BASE_MS / 4));
        assert_eq!(first_delay, retry_delay_ms(&event));

        let delayed_event = CompletionOutboxEvent {
            attempt: 100,
            ..event
        };
        assert!(retry_delay_ms(&delayed_event) <= RETRY_CAP_MS);
    }

    #[test]
    fn terminal_turn_protocol_preserves_goal_fallback_compatibility() {
        let normal = CompletionOutboxEvent {
            event_id: "10000000-0000-0000-0000-000000000001".to_string(),
            completion_work_id: "10000000-0000-0000-0000-000000000001".to_string(),
            thread_id: "20000000-0000-0000-0000-000000000001".to_string(),
            execution_kind: "normal".to_string(),
            execution_id: "turn-1".to_string(),
            terminal_turn_id: None,
            callback_metadata_json: String::new(),
            terminal_status: "completed".to_string(),
            final_text: "done".to_string(),
            terminal_at_ms: 1_000,
            attempt: 1,
            lease_id: "lease-1".to_string(),
        };
        let terminal_goal = CompletionOutboxEvent {
            execution_kind: "goal".to_string(),
            execution_id: "goal-1".to_string(),
            terminal_turn_id: Some("turn-1".to_string()),
            terminal_status: "complete".to_string(),
            ..normal.clone()
        };
        let empty_fallback = CompletionOutboxEvent {
            terminal_turn_id: None,
            final_text: String::new(),
            ..terminal_goal.clone()
        };
        let missing_goal_turn = CompletionOutboxEvent {
            terminal_turn_id: None,
            ..terminal_goal.clone()
        };
        let mismatched_normal_turn = CompletionOutboxEvent {
            terminal_turn_id: Some("another-turn".to_string()),
            ..normal.clone()
        };

        assert_eq!(
            Some("turn-1"),
            terminal_turn_id(&normal).expect("normal turn")
        );
        assert_eq!(
            Some("turn-1"),
            terminal_turn_id(&terminal_goal).expect("goal terminal turn")
        );
        assert_eq!(
            None,
            terminal_turn_id(&empty_fallback).expect("legacy empty goal fallback")
        );
        assert_eq!(
            Err("goal completion with final text has no terminal turn identity".to_string()),
            terminal_turn_id(&missing_goal_turn)
        );
        assert_eq!(
            Err("normal completion terminal turn does not match its execution id".to_string()),
            terminal_turn_id(&mismatched_normal_turn)
        );
    }

    #[tokio::test]
    async fn sender_retries_a_real_http_failure_and_marks_the_event_sent() {
        let temp_dir = TempDir::new().expect("temporary state directory should exist");
        let runtime =
            StateRuntime::init(temp_dir.path().to_path_buf(), "test-provider".to_string())
                .await
                .expect("state runtime should initialize");
        let store = runtime.completions().clone();
        let completion_work_id = "10000000-0000-0000-0000-000000000007";
        let thread_id = ThreadId::from_string("20000000-0000-0000-0000-000000000007")
            .expect("thread id should be valid");
        store
            .bind_turn(completion_work_id, thread_id, "turn-7")
            .await
            .expect("completion binding should persist");
        store
            .complete_turn(thread_id, "turn-7", "finished", 1_000)
            .await
            .expect("completion event should persist");

        let token = "test-cell-token-with-at-least-32-characters".to_string();
        let endpoint_state = TestEndpoint {
            attempts: Arc::new(AtomicUsize::new(0)),
            expected_token: token.clone(),
        };
        let router = Router::new()
            .route(COMPLETION_EVENT_PATH, post(test_completion_endpoint))
            .with_state(endpoint_state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test endpoint should bind");
        let address = listener
            .local_addr()
            .expect("test endpoint should have an address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test endpoint should serve");
        });

        let config =
            CompletionOutboxSenderConfig::from_values(&format!("http://{address}"), &token)
                .expect("loopback callback endpoint should be valid");
        let client = test_http_client();
        let shutdown = CancellationToken::new();
        let sender = tokio::spawn(run(
            store.clone(),
            config,
            client,
            Uuid::now_v7(),
            shutdown.clone(),
        ));

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let stats = store
                .outbox_stats()
                .await
                .expect("completion stats should be readable");
            if stats.sent_count == 1 {
                assert_eq!(0, stats.pending_count);
                assert_eq!(0, stats.sending_count);
                assert_eq!(2, stats.total_attempts);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "completion sender did not retry in time"
            );
            sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(2, endpoint_state.attempts.load(Ordering::SeqCst));

        shutdown.cancel();
        sender.await.expect("completion sender should stop cleanly");
        server.abort();
    }

    #[tokio::test]
    async fn sender_finalizes_central_command_not_found_without_retrying() {
        let temp_dir = TempDir::new().expect("temporary state directory should exist");
        let runtime =
            StateRuntime::init(temp_dir.path().to_path_buf(), "test-provider".to_string())
                .await
                .expect("state runtime should initialize");
        let store = runtime.completions().clone();
        let completion_work_id = "10000000-0000-0000-0000-000000000009";
        let thread_id = ThreadId::from_string("20000000-0000-0000-0000-000000000009")
            .expect("thread id should be valid");
        store
            .bind_turn(completion_work_id, thread_id, "turn-9")
            .await
            .expect("completion binding should persist");
        store
            .complete_turn(thread_id, "turn-9", "legacy completion", 1_000)
            .await
            .expect("completion event should persist");

        let token = "missing-command-token-with-at-least-32-characters".to_string();
        let endpoint_state = TestEndpoint {
            attempts: Arc::new(AtomicUsize::new(0)),
            expected_token: token.clone(),
        };
        let router = Router::new()
            .route(COMPLETION_EVENT_PATH, post(test_missing_command_endpoint))
            .with_state(endpoint_state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test endpoint should bind");
        let address = listener
            .local_addr()
            .expect("test endpoint should have an address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test endpoint should serve");
        });

        let config =
            CompletionOutboxSenderConfig::from_values(&format!("http://{address}"), &token)
                .expect("loopback callback endpoint should be valid");
        let shutdown = CancellationToken::new();
        let sender = tokio::spawn(run(
            store.clone(),
            config,
            test_http_client(),
            Uuid::now_v7(),
            shutdown.clone(),
        ));

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let stats = store
                .outbox_stats()
                .await
                .expect("completion stats should be readable");
            if stats.sent_count == 1 {
                assert_eq!(0, stats.pending_count);
                assert_eq!(0, stats.sending_count);
                assert_eq!(1, stats.total_attempts);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "completion sender did not finalize the permanent rejection in time"
            );
            sleep(Duration::from_millis(20)).await;
        }
        sleep(Duration::from_millis(RETRY_BASE_MS as u64 + 100)).await;
        assert_eq!(1, endpoint_state.attempts.load(Ordering::SeqCst));

        shutdown.cancel();
        sender.await.expect("completion sender should stop cleanly");
        server.abort();
    }

    #[tokio::test]
    async fn sender_restart_recovers_persisted_event_after_http_failure() {
        let temp_dir = TempDir::new().expect("temporary state directory should exist");
        let runtime =
            StateRuntime::init(temp_dir.path().to_path_buf(), "test-provider".to_string())
                .await
                .expect("state runtime should initialize");
        let store = runtime.completions().clone();
        let completion_work_id = "10000000-0000-0000-0000-000000000008";
        let thread_id = ThreadId::from_string("20000000-0000-0000-0000-000000000008")
            .expect("thread id should be valid");
        store
            .bind_turn(completion_work_id, thread_id, "turn-8")
            .await
            .expect("completion binding should persist");
        store
            .complete_turn(thread_id, "turn-8", "finished after restart", 1_000)
            .await
            .expect("completion event should persist");

        let token = "restart-cell-token-with-at-least-32-characters".to_string();
        let endpoint_state = TestEndpoint {
            attempts: Arc::new(AtomicUsize::new(0)),
            expected_token: token.clone(),
        };
        let router = Router::new()
            .route(COMPLETION_EVENT_PATH, post(test_completion_endpoint))
            .with_state(endpoint_state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test endpoint should bind");
        let address = listener
            .local_addr()
            .expect("test endpoint should have an address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test endpoint should serve");
        });

        let first_config =
            CompletionOutboxSenderConfig::from_values(&format!("http://{address}"), &token)
                .expect("loopback callback endpoint should be valid");
        let first_shutdown = CancellationToken::new();
        let first_sender = tokio::spawn(run(
            store.clone(),
            first_config,
            test_http_client(),
            Uuid::now_v7(),
            first_shutdown.clone(),
        ));
        let first_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let stats = store
                .outbox_stats()
                .await
                .expect("completion stats should be readable");
            if stats.pending_count == 1 && stats.total_attempts == 1 {
                break;
            }
            assert!(
                Instant::now() < first_deadline,
                "first sender did not persist its failed attempt"
            );
            sleep(Duration::from_millis(20)).await;
        }
        first_shutdown.cancel();
        first_sender
            .await
            .expect("first completion sender should stop cleanly");
        drop(store);
        drop(runtime);

        let restarted_runtime =
            StateRuntime::init(temp_dir.path().to_path_buf(), "test-provider".to_string())
                .await
                .expect("state runtime should reopen after restart");
        let restarted_store = restarted_runtime.completions().clone();
        let restarted_config =
            CompletionOutboxSenderConfig::from_values(&format!("http://{address}"), &token)
                .expect("loopback callback endpoint should remain valid");
        let restarted_shutdown = CancellationToken::new();
        let restarted_sender = tokio::spawn(run(
            restarted_store.clone(),
            restarted_config,
            test_http_client(),
            Uuid::now_v7(),
            restarted_shutdown.clone(),
        ));
        let restart_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let stats = restarted_store
                .outbox_stats()
                .await
                .expect("restarted completion stats should be readable");
            if stats.sent_count == 1 {
                assert_eq!(0, stats.pending_count);
                assert_eq!(0, stats.sending_count);
                assert_eq!(2, stats.total_attempts);
                break;
            }
            assert!(
                Instant::now() < restart_deadline,
                "restarted sender did not recover the persisted event"
            );
            sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(2, endpoint_state.attempts.load(Ordering::SeqCst));

        restarted_shutdown.cancel();
        restarted_sender
            .await
            .expect("restarted completion sender should stop cleanly");
        server.abort();
    }

    fn test_http_client() -> Client {
        Client::builder()
            .redirect(Policy::none())
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()
            .expect("test callback client should build")
    }

    async fn test_completion_endpoint(
        State(state): State<TestEndpoint>,
        headers: HeaderMap,
        Json(payload): Json<Value>,
    ) -> Response {
        let expected_authorization = format!("Bearer {}", state.expected_token);
        if headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            != Some(expected_authorization.as_str())
        {
            return AxumStatusCode::UNAUTHORIZED.into_response();
        }
        let attempt = state.attempts.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return AxumStatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let Some(event_id) = payload.get("event_id").and_then(Value::as_str) else {
            return AxumStatusCode::BAD_REQUEST.into_response();
        };
        let Some(boot_id) = payload.get("boot_id").and_then(Value::as_str) else {
            return AxumStatusCode::BAD_REQUEST.into_response();
        };
        if Uuid::parse_str(boot_id).is_err() {
            return AxumStatusCode::BAD_REQUEST.into_response();
        }
        let Some(completion_work_id) = payload.get("completion_work_id").and_then(Value::as_str)
        else {
            return AxumStatusCode::BAD_REQUEST.into_response();
        };
        if payload.get("protocol_version").and_then(Value::as_str) != Some(COMPLETION_PROTOCOL_V2)
            || payload.get("terminal_turn_id") != payload.get("execution_id")
        {
            return AxumStatusCode::BAD_REQUEST.into_response();
        }
        (
            AxumStatusCode::OK,
            Json(json!({
                "outcome": "accepted",
                "event_id": event_id,
                "completion_work_id": completion_work_id,
                "delivery_count": 1,
                "received_at": "2026-07-23T17:00:00Z"
            })),
        )
            .into_response()
    }

    async fn test_missing_command_endpoint(
        State(state): State<TestEndpoint>,
        headers: HeaderMap,
    ) -> Response {
        let expected_authorization = format!("Bearer {}", state.expected_token);
        if headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            != Some(expected_authorization.as_str())
        {
            return AxumStatusCode::UNAUTHORIZED.into_response();
        }
        state.attempts.fetch_add(1, Ordering::SeqCst);
        (
            AxumStatusCode::NOT_FOUND,
            Json(json!({
                "detail": {
                    "code": "command_not_found",
                    "message": "Completion work was not found in this source cell scope."
                }
            })),
        )
            .into_response()
    }
}
