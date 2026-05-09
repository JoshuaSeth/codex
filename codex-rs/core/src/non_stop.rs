use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub const NON_STOP_DURATION_USAGE: &str =
    "Duration must be a positive integer; plain numbers mean minutes, suffix with s, m, or h.";
pub const NON_STOP_BUDGET_USAGE: &str = "Budget must be a positive integer stop-attempt count.";
pub const DEEP_FORCE_CONTINUE_ITERATIONS: u32 = 4;

pub fn parse_non_stop_budget(value: &str) -> Result<u32, String> {
    let budget = value
        .trim()
        .parse::<u32>()
        .map_err(|_| NON_STOP_BUDGET_USAGE.to_string())?;
    if budget == 0 {
        return Err(NON_STOP_BUDGET_USAGE.to_string());
    }
    Ok(budget)
}

pub fn parse_non_stop_duration(value: &str) -> Result<Duration, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(NON_STOP_DURATION_USAGE.to_string());
    }

    let (amount_text, unit) = match trimmed.chars().last() {
        Some(unit @ ('s' | 'S' | 'm' | 'M' | 'h' | 'H')) => {
            (&trimmed[..trimmed.len().saturating_sub(1)], Some(unit))
        }
        Some(_) => (trimmed, None),
        None => return Err(NON_STOP_DURATION_USAGE.to_string()),
    };
    if amount_text.is_empty() {
        return Err(NON_STOP_DURATION_USAGE.to_string());
    }

    let amount = amount_text
        .parse::<u64>()
        .map_err(|_| NON_STOP_DURATION_USAGE.to_string())?;
    if amount == 0 {
        return Err("Duration must be greater than zero.".to_string());
    }

    let multiplier_secs = match unit.map(|value| value.to_ascii_lowercase()) {
        None | Some('m') => 60,
        Some('s') => 1,
        Some('h') => 60 * 60,
        Some(_) => return Err("Duration suffix must be s, m, or h.".to_string()),
    };
    let seconds = amount
        .checked_mul(multiplier_secs)
        .ok_or_else(|| "Duration is too large.".to_string())?;
    Ok(Duration::from_secs(seconds))
}

pub fn non_stop_is_active(
    non_stop: bool,
    non_stop_expires_at: Option<i64>,
    now_unix_seconds: i64,
) -> bool {
    non_stop
        && match non_stop_expires_at {
            Some(expires_at) => now_unix_seconds < expires_at,
            None => true,
        }
}

pub fn current_unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

pub fn non_stop_expires_at_after(duration: Duration) -> Result<i64, String> {
    let now = current_unix_timestamp();
    let duration_seconds: i64 = duration
        .as_secs()
        .try_into()
        .map_err(|_| "Duration is too large.".to_string())?;
    now.checked_add(duration_seconds)
        .ok_or_else(|| "Duration is too large.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_plain_number_as_minutes() {
        assert_eq!(
            parse_non_stop_duration("5").unwrap(),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn parse_non_stop_budget_requires_positive_integer() {
        assert_eq!(parse_non_stop_budget("300").unwrap(), 300);
        assert_eq!(
            parse_non_stop_budget("0").unwrap_err(),
            NON_STOP_BUDGET_USAGE
        );
        assert_eq!(
            parse_non_stop_budget("1h").unwrap_err(),
            NON_STOP_BUDGET_USAGE
        );
    }

    #[test]
    fn parse_suffixes() {
        assert_eq!(
            parse_non_stop_duration("7s").unwrap(),
            Duration::from_secs(7)
        );
        assert_eq!(
            parse_non_stop_duration("8m").unwrap(),
            Duration::from_secs(480)
        );
        assert_eq!(
            parse_non_stop_duration("2h").unwrap(),
            Duration::from_secs(7200)
        );
    }

    #[test]
    fn non_stop_active_respects_expiry() {
        assert!(non_stop_is_active(true, None, 100));
        assert!(non_stop_is_active(true, Some(101), 100));
        assert!(!non_stop_is_active(true, Some(100), 100));
        assert!(!non_stop_is_active(false, None, 100));
    }
}
