use crate::error::TransportError;
use crate::request::Request;
use rand::Rng;
use std::future::Future;
use std::time::Duration;
use tokio::time::sleep;

const FORCED_MAX_ATTEMPTS: u64 = 30;
const MAX_BACKOFF_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u64,
    pub base_delay: Duration,
    pub retry_on: RetryOn,
}

#[derive(Debug, Clone)]
pub struct RetryOn {
    pub retry_429: bool,
    pub retry_5xx: bool,
    pub retry_transport: bool,
}

impl RetryOn {
    pub fn should_retry(&self, err: &TransportError, attempt: u64, max_attempts: u64) -> bool {
        if attempt >= max_attempts {
            return false;
        }
        match err {
            // codex-dev policy: retry every backend transport failure up to the configured budget.
            TransportError::Http { .. }
            | TransportError::Timeout
            | TransportError::Network(_)
            | TransportError::Build(_)
            | TransportError::RetryLimit => true,
        }
    }
}

pub fn backoff(base: Duration, attempt: u64) -> Duration {
    let max_delay_ms = MAX_BACKOFF_DELAY.as_millis().min(u128::from(u64::MAX)) as u64;
    let base_delay_ms = base.as_millis().min(u128::from(u64::MAX)) as u64;
    let base_delay_ms = base_delay_ms.min(max_delay_ms);

    if attempt == 0 {
        return Duration::from_millis(base_delay_ms);
    }
    let exp = 2u64.saturating_pow(attempt as u32 - 1);
    let raw = base_delay_ms.saturating_mul(exp).min(max_delay_ms);
    let jitter: f64 = rand::rng().random_range(0.9..1.1);
    let jittered_ms = ((raw as f64 * jitter) as u64).min(max_delay_ms);
    Duration::from_millis(jittered_ms)
}

pub async fn run_with_retry<T, F, Fut>(
    policy: RetryPolicy,
    mut make_req: impl FnMut() -> Request,
    op: F,
) -> Result<T, TransportError>
where
    F: Fn(Request, u64) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    let mut max_attempts = policy.max_attempts;
    let mut attempt = 0;
    loop {
        let req = make_req();
        match op(req, attempt).await {
            Ok(resp) => return Ok(resp),
            Err(err) => {
                if should_force_retry_budget(&err) {
                    max_attempts = max_attempts.max(FORCED_MAX_ATTEMPTS);
                }

                let force_retry = should_force_retry(&err);
                let should_retry = policy.retry_on.should_retry(&err, attempt, max_attempts)
                    || (force_retry && attempt < max_attempts);
                if should_retry {
                    sleep(backoff(policy.base_delay, attempt + 1)).await;
                    attempt += 1;
                    continue;
                }

                return Err(err);
            }
        }
    }
}

fn should_force_retry_budget(err: &TransportError) -> bool {
    match err {
        TransportError::Network(message) => {
            normalize_for_matching(message).contains("decodingresponsebody")
        }
        TransportError::Http { status, body, .. } if *status == http::StatusCode::BAD_REQUEST => {
            body.as_deref().is_some_and(is_generic_bad_request_body)
        }
        _ => false,
    }
}

fn should_force_retry(err: &TransportError) -> bool {
    should_force_retry_budget(err)
}

fn is_generic_bad_request_body(body: &str) -> bool {
    if body.trim().eq_ignore_ascii_case("bad request") {
        return true;
    }

    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("detail")
                .and_then(serde_json::Value::as_str)
                .map(|detail| detail.eq_ignore_ascii_case("bad request"))
        })
        .unwrap_or(false)
}

fn normalize_for_matching(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[test]
    fn backoff_is_capped() {
        let delay = backoff(Duration::from_millis(200), 30);
        assert!(delay <= MAX_BACKOFF_DELAY);
    }

    #[tokio::test]
    async fn run_with_retry_forces_budget_on_decode_body_network_error() {
        let policy = RetryPolicy {
            max_attempts: 0,
            base_delay: Duration::from_millis(0),
            retry_on: RetryOn {
                retry_429: false,
                retry_5xx: false,
                retry_transport: false,
            },
        };

        let calls = AtomicUsize::new(0);
        let result = run_with_retry(
            policy,
            || Request::new(http::Method::GET, "/".to_string()),
            |_req, _attempt| async {
                let call_num = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call_num < 3 {
                    return Err(TransportError::Network(
                        "error decoding response body".to_string(),
                    ));
                }
                Ok(())
            },
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn run_with_retry_forces_retry_on_generic_bad_request() {
        let policy = RetryPolicy {
            max_attempts: 0,
            base_delay: Duration::from_millis(0),
            retry_on: RetryOn {
                retry_429: false,
                retry_5xx: false,
                retry_transport: false,
            },
        };

        let calls = AtomicUsize::new(0);
        let result = run_with_retry(
            policy,
            || Request::new(http::Method::GET, "/".to_string()),
            |_req, _attempt| async {
                let call_num = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call_num < 3 {
                    return Err(TransportError::Http {
                        status: http::StatusCode::BAD_REQUEST,
                        url: None,
                        headers: Some(HeaderMap::new()),
                        body: Some("{\"detail\": \"Bad Request\"}".to_string()),
                    });
                }
                Ok(())
            },
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn run_with_retry_retries_http_400_even_when_retry_flags_disabled() {
        let policy = RetryPolicy {
            max_attempts: 2,
            base_delay: Duration::from_millis(0),
            retry_on: RetryOn {
                retry_429: false,
                retry_5xx: false,
                retry_transport: false,
            },
        };

        let calls = AtomicUsize::new(0);
        let result = run_with_retry(
            policy,
            || Request::new(http::Method::GET, "/".to_string()),
            |_req, _attempt| async {
                let call_num = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call_num < 3 {
                    return Err(TransportError::Http {
                        status: http::StatusCode::BAD_REQUEST,
                        url: None,
                        headers: Some(HeaderMap::new()),
                        body: Some("bad request".to_string()),
                    });
                }
                Ok(())
            },
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
