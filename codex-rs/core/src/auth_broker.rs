use base64::Engine;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;
use thiserror::Error;

use crate::auth::AuthDotJson;

pub(crate) const CODEX_AUTH_BROKER_URL_ENV_VAR: &str = "CODEX_AUTH_BROKER_URL";
pub(crate) const CODEX_AUTH_BROKER_TOKEN_ENV_VAR: &str = "CODEX_AUTH_BROKER_TOKEN";
const CODEX_AUTH_BROKER_CLIENT_NAME_ENV_VAR: &str = "CODEX_AUTH_BROKER_CLIENT_NAME";
const CODEX_AUTH_BROKER_LEASE_REASON_ENV_VAR: &str = "CODEX_AUTH_BROKER_LEASE_REASON";
const CODEX_AUTH_BROKER_HEARTBEAT_INTERVAL_SECONDS_ENV_VAR: &str =
    "CODEX_AUTH_BROKER_HEARTBEAT_INTERVAL_SECONDS";
const DEFAULT_CLIENT_NAME: &str = "codex-dev";
const DEFAULT_LEASE_REASON: &str = "usage-limit-recovery";
const DEFAULT_HEARTBEAT_INTERVAL_SECONDS: u64 = 300;
const MAX_DETAIL_BYTES: usize = 3_800;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthBrokerConfig {
    pub(crate) url: String,
    pub(crate) token: String,
    pub(crate) client_name: String,
    pub(crate) lease_reason: String,
    pub(crate) heartbeat_interval: Duration,
}

impl AuthBrokerConfig {
    pub(crate) fn from_env() -> Result<Option<Self>, AuthBrokerError> {
        let url = read_env(CODEX_AUTH_BROKER_URL_ENV_VAR);
        let token = read_env(CODEX_AUTH_BROKER_TOKEN_ENV_VAR);

        match (url, token) {
            (None, None) => Ok(None),
            (Some(_), None) | (None, Some(_)) => Err(AuthBrokerError::Config(
                "Both CODEX_AUTH_BROKER_URL and CODEX_AUTH_BROKER_TOKEN are required.".to_string(),
            )),
            (Some(url), Some(token)) => {
                let heartbeat_interval =
                    read_env(CODEX_AUTH_BROKER_HEARTBEAT_INTERVAL_SECONDS_ENV_VAR)
                        .map(|value| {
                            value.parse::<u64>().map_err(|err| {
                        AuthBrokerError::Config(format!(
                            "Invalid {CODEX_AUTH_BROKER_HEARTBEAT_INTERVAL_SECONDS_ENV_VAR}: {err}",
                        ))
                    })
                        })
                        .transpose()?
                        .unwrap_or(DEFAULT_HEARTBEAT_INTERVAL_SECONDS)
                        .clamp(30, 3_600);

                Ok(Some(Self {
                    url: url.trim_end_matches('/').to_string(),
                    token,
                    client_name: read_env(CODEX_AUTH_BROKER_CLIENT_NAME_ENV_VAR)
                        .unwrap_or_else(|| DEFAULT_CLIENT_NAME.to_string()),
                    lease_reason: read_env(CODEX_AUTH_BROKER_LEASE_REASON_ENV_VAR)
                        .unwrap_or_else(|| DEFAULT_LEASE_REASON.to_string()),
                    heartbeat_interval: Duration::from_secs(heartbeat_interval),
                }))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BrokerLease {
    pub(crate) lease_id: String,
    pub(crate) account_id: String,
    pub(crate) auth: AuthDotJson,
}

#[derive(Clone, Debug)]
pub(crate) struct AuthBrokerClient {
    config: AuthBrokerConfig,
    http: reqwest::Client,
}

impl AuthBrokerClient {
    pub(crate) fn from_env() -> Result<Option<Self>, AuthBrokerError> {
        let Some(config) = AuthBrokerConfig::from_env()? else {
            return Ok(None);
        };
        Ok(Some(Self {
            config,
            http: reqwest::Client::new(),
        }))
    }

    pub(crate) fn heartbeat_interval(&self) -> Duration {
        self.config.heartbeat_interval
    }

    pub(crate) async fn acquire_lease(
        &self,
        affinity_key: &str,
    ) -> Result<BrokerLease, AuthBrokerError> {
        let response = self
            .http
            .post(format!("{}/v1/leases", self.config.url))
            .bearer_auth(&self.config.token)
            .json(&LeaseRequest {
                client_name: self.config.client_name.clone(),
                affinity_key: Some(affinity_key.to_string()),
                lease_reason: Some(self.config.lease_reason.clone()),
            })
            .send()
            .await?;

        let payload: LeaseResponse = self.decode_json_response(response).await?;
        let raw = base64::engine::general_purpose::STANDARD.decode(payload.auth_json_b64)?;
        let auth = serde_json::from_slice::<AuthDotJson>(&raw)?;
        Ok(BrokerLease {
            lease_id: payload.lease_id,
            account_id: payload.account_id,
            auth,
        })
    }

    pub(crate) async fn report_lease(
        &self,
        lease_id: &str,
        outcome: &str,
        updated_auth: Option<&AuthDotJson>,
        detail: Option<&str>,
    ) -> Result<(), AuthBrokerError> {
        let updated_auth_json_b64 = updated_auth
            .map(serde_json::to_vec)
            .transpose()?
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes));

        let response = self
            .http
            .post(format!("{}/v1/leases/{lease_id}/report", self.config.url))
            .bearer_auth(&self.config.token)
            .json(&ReportLeaseRequest {
                outcome: outcome.to_string(),
                updated_auth_json_b64,
                detail: detail.map(truncate_detail),
            })
            .send()
            .await?;

        self.expect_success(response).await
    }

    pub(crate) async fn heartbeat(
        &self,
        lease_id: &str,
    ) -> Result<Option<BrokerFreezeSignal>, AuthBrokerError> {
        let response = self
            .http
            .post(format!(
                "{}/v1/leases/{lease_id}/heartbeat",
                self.config.url
            ))
            .bearer_auth(&self.config.token)
            .send()
            .await?;

        let payload: HeartbeatResponse = self.decode_json_response(response).await?;
        Ok(payload.freeze.filter(|freeze| freeze.active))
    }

    pub(crate) async fn sync_lease(
        &self,
        lease_id: &str,
        updated_auth: &AuthDotJson,
    ) -> Result<(), AuthBrokerError> {
        let updated_auth_json_b64 =
            base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(updated_auth)?);

        let response = self
            .http
            .post(format!("{}/v1/leases/{lease_id}/sync", self.config.url))
            .bearer_auth(&self.config.token)
            .json(&SyncLeaseRequest {
                updated_auth_json_b64,
            })
            .send()
            .await?;

        self.expect_success(response).await
    }

    async fn expect_success(&self, response: reqwest::Response) -> Result<(), AuthBrokerError> {
        if response.status().is_success() {
            return Ok(());
        }

        Err(read_broker_error(response).await)
    }

    async fn decode_json_response<T: for<'de> Deserialize<'de>>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, AuthBrokerError> {
        if !response.status().is_success() {
            return Err(read_broker_error(response).await);
        }

        response.json::<T>().await.map_err(AuthBrokerError::Http)
    }
}

#[derive(Debug, Error)]
pub(crate) enum AuthBrokerError {
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Decode(#[from] base64::DecodeError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("auth broker {status}: {detail}")]
    Broker { status: StatusCode, detail: String },
}

#[derive(Debug, Serialize)]
struct LeaseRequest {
    client_name: String,
    affinity_key: Option<String>,
    lease_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LeaseResponse {
    lease_id: String,
    account_id: String,
    auth_json_b64: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct BrokerFreezeSignal {
    #[serde(default)]
    pub(crate) active: bool,
    pub(crate) reason: Option<String>,
    pub(crate) message: Option<String>,
}

impl BrokerFreezeSignal {
    pub(crate) fn display_message(&self) -> String {
        self.message
            .as_ref()
            .map(|message| message.trim())
            .filter(|message| !message.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                let reason = self
                    .reason
                    .as_deref()
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                    .unwrap_or("broker_freeze");
                format!("auth broker requested freeze: {reason}")
            })
    }
}

#[derive(Debug, Deserialize)]
struct HeartbeatResponse {
    freeze: Option<BrokerFreezeSignal>,
}

#[derive(Debug, Serialize)]
struct ReportLeaseRequest {
    outcome: String,
    updated_auth_json_b64: Option<String>,
    detail: Option<String>,
}

#[derive(Debug, Serialize)]
struct SyncLeaseRequest {
    updated_auth_json_b64: String,
}

#[derive(Debug, Deserialize)]
struct BrokerErrorResponse {
    detail: Option<Value>,
}

fn read_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn truncate_detail(detail: &str) -> String {
    if detail.len() <= MAX_DETAIL_BYTES {
        return detail.to_string();
    }

    let mut cut = MAX_DETAIL_BYTES;
    while !detail.is_char_boundary(cut) {
        cut = cut.saturating_sub(1);
    }
    let mut truncated = detail[..cut].to_string();
    truncated.push_str("...");
    truncated
}

async fn read_broker_error(response: reqwest::Response) -> AuthBrokerError {
    let status = response.status();
    let body = match response.text().await {
        Ok(body) => body,
        Err(err) => {
            return AuthBrokerError::Broker {
                status,
                detail: err.to_string(),
            };
        }
    };

    if let Ok(payload) = serde_json::from_str::<BrokerErrorResponse>(&body) {
        return AuthBrokerError::Broker {
            status,
            detail: payload
                .detail
                .map(format_broker_detail)
                .unwrap_or_else(|| status.to_string()),
        };
    }

    let detail = body.trim();
    AuthBrokerError::Broker {
        status,
        detail: if detail.is_empty() {
            status.to_string()
        } else {
            truncate_detail(detail)
        },
    }
}

fn format_broker_detail(detail: Value) -> String {
    let formatted = match detail {
        Value::String(text) => return truncate_detail(&text),
        Value::Object(object) => format_availability_diagnostics(&object)
            .unwrap_or_else(|| Value::Object(object).to_string()),
        other => other.to_string(),
    };
    truncate_detail(&formatted)
}

fn format_availability_diagnostics(object: &serde_json::Map<String, Value>) -> Option<String> {
    let message = object.get("message")?.as_str()?;
    let summary = object.get("summary")?.as_object()?;
    let selectable = summary_count(summary, "selectable_accounts");
    let leased = summary_count(summary, "leased_accounts");
    let rate_limited = summary_count(summary, "rate_limited_accounts");
    let auth_invalid = summary_count(summary, "auth_invalid_accounts");
    let disabled = summary_count(summary, "disabled_accounts");

    let mut lines = vec![format!(
        "{message} ({selectable} selectable, {leased} leased, {rate_limited} rate-limited, {auth_invalid} auth-invalid, {disabled} disabled)."
    )];
    if let Some(next_lease) = object_string(summary, "next_lease_expires_at") {
        lines.push(format!("Next leased account frees at {next_lease}."));
    }
    if let Some(next_cooldown) = object_string(summary, "next_cooldown_until") {
        lines.push(format!(
            "Next rate-limited account cools down at {next_cooldown}."
        ));
    }

    let accounts = object.get("accounts").and_then(Value::as_array)?;
    if !accounts.is_empty() {
        lines.push("Accounts:".to_string());
    }
    for account in accounts.iter().take(8) {
        let Some(account) = account.as_object() else {
            continue;
        };
        let account_id = object_string(account, "account_id").unwrap_or("unknown");
        let label = object_string(account, "label")
            .filter(|label| !label.is_empty())
            .unwrap_or(account_id);
        let availability = object_string(account, "availability").unwrap_or("unknown");
        let mut fragments = vec![format!("- {label} ({account_id}): {availability}")];
        if let Some(reasons) = skip_reasons(account) {
            fragments.push(reasons);
        }
        if let Some(usage) = usage_summary(account.get("usage")) {
            fragments.push(usage);
        }
        lines.push(fragments.join("; "));
    }
    if accounts.len() > 8 {
        let remaining = accounts.len() - 8;
        lines.push(format!("- ... {remaining} more accounts"));
    }

    Some(lines.join("\n"))
}

fn summary_count(object: &serde_json::Map<String, Value>, key: &str) -> u64 {
    object.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn object_string<'a>(object: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn skip_reasons(account: &serde_json::Map<String, Value>) -> Option<String> {
    let reasons = account
        .get("skip_reasons")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .take(3)
        .collect::<Vec<_>>();
    if reasons.is_empty() {
        None
    } else {
        Some(reasons.join("; "))
    }
}

fn usage_summary(usage: Option<&Value>) -> Option<String> {
    let usage = usage?.as_object()?;
    let mut parts = Vec::new();
    if let Some(percent) = usage
        .get("primary_window")
        .and_then(|window| window.get("used_percent"))
        .and_then(format_percent)
    {
        parts.push(format!("5h {percent}% used"));
    }
    if let Some(percent) = usage
        .get("secondary_window")
        .and_then(|window| window.get("used_percent"))
        .and_then(format_percent)
    {
        parts.push(format!("weekly {percent}% used"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

fn format_percent(value: &Value) -> Option<String> {
    if let Some(percent) = value.as_i64() {
        return Some(percent.to_string());
    }
    value.as_f64().map(|percent| {
        if percent.fract() == 0.0 {
            format!("{percent:.0}")
        } else {
            format!("{percent:.1}")
        }
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::time::Duration;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::AuthBrokerClient;
    use super::AuthBrokerConfig;
    use super::format_broker_detail;

    #[test]
    fn broker_diagnostics_detail_is_human_readable() {
        let detail = format_broker_detail(json!({
            "message": "No enabled account is currently available",
            "summary": {
                "selectable_accounts": 0,
                "leased_accounts": 1,
                "rate_limited_accounts": 1,
                "auth_invalid_accounts": 1,
                "disabled_accounts": 0,
                "next_lease_expires_at": "2026-05-16T15:53:05+00:00",
                "next_cooldown_until": "2026-05-16T18:22:48+00:00"
            },
            "accounts": [
                {
                    "account_id": "acc-a",
                    "label": "root-codex",
                    "availability": "available",
                    "skip_reasons": ["leased until 2026-05-16T15:53:05+00:00"],
                    "usage": {
                        "primary_window": {"used_percent": 0},
                        "secondary_window": {"used_percent": 21}
                    }
                },
                {
                    "account_id": "acc-b",
                    "label": "privaterelay-codex",
                    "availability": "rate_limited",
                    "skip_reasons": [
                        "cooling down until 2026-05-16T18:22:48+00:00",
                        "rate_limited: no credits remaining (balance 0)"
                    ],
                    "usage": {
                        "primary_window": {"used_percent": 100},
                        "secondary_window": {"used_percent": 100}
                    }
                }
            ]
        }));

        assert_eq!(
            detail,
            "No enabled account is currently available (0 selectable, 1 leased, 1 rate-limited, 1 auth-invalid, 0 disabled).\n\
             Next leased account frees at 2026-05-16T15:53:05+00:00.\n\
             Next rate-limited account cools down at 2026-05-16T18:22:48+00:00.\n\
             Accounts:\n\
             - root-codex (acc-a): available; leased until 2026-05-16T15:53:05+00:00; 5h 0% used, weekly 21% used\n\
             - privaterelay-codex (acc-b): rate_limited; cooling down until 2026-05-16T18:22:48+00:00; rate_limited: no credits remaining (balance 0); 5h 100% used, weekly 100% used"
        );
    }

    #[tokio::test]
    async fn heartbeat_returns_active_freeze_signal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/leases/lease-1/heartbeat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "lease_id": "lease-1",
                "account_id": "account-1",
                "expires_at": "2026-06-10T12:00:00+00:00",
                "freeze": {
                    "active": true,
                    "reason": "low_five_hour_headroom",
                    "message": "FROZEN due to 5h quota headroom: 9% remaining <= 10% threshold."
                }
            })))
            .mount(&server)
            .await;

        let client = AuthBrokerClient {
            config: AuthBrokerConfig {
                url: server.uri(),
                token: "client-token".to_string(),
                client_name: "codex-dev".to_string(),
                lease_reason: "run".to_string(),
                heartbeat_interval: Duration::from_secs(30),
            },
            http: reqwest::Client::new(),
        };

        let freeze = client.heartbeat("lease-1").await.unwrap().unwrap();

        assert_eq!(freeze.reason.as_deref(), Some("low_five_hour_headroom"));
        assert_eq!(
            freeze.display_message(),
            "FROZEN due to 5h quota headroom: 9% remaining <= 10% threshold."
        );
    }
}
