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

    pub(crate) async fn heartbeat(&self, lease_id: &str) -> Result<(), AuthBrokerError> {
        let response = self
            .http
            .post(format!(
                "{}/v1/leases/{lease_id}/heartbeat",
                self.config.url
            ))
            .bearer_auth(&self.config.token)
            .send()
            .await?;

        self.expect_success(response).await
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
                .map(|detail| match detail {
                    Value::String(text) => text,
                    other => other.to_string(),
                })
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
