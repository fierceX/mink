//! Bounded LLM request recovery.
//!
//! This module owns the small pieces shared by every production request path
//! (agent turns and compaction):
//!
//! - the typed upstream failure ([`LlmUpstreamError`]) with a coarse recovery
//!   class, optional HTTP status/provider code and a bounded diagnostic;
//! - the per-logical-request retry accounting ([`RequestRetryState`]);
//! - the pure backoff calculation (exponential base with bounded jitter).
//!
//! It deliberately owns no conversation, tools or model wiring: each caller
//! keeps its own attempt loop and consumption logic.

use std::time::{Duration, Instant};

/// Coarse recovery class of an upstream failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamFailureKind {
    /// Transient transport/provider failure: retry the identical request.
    Recoverable,
    /// The response stream itself was corrupted (SSE parse damage, abnormal
    /// EOF): the current candidate is untrustworthy and is discarded.
    ProtocolDamaged,
    /// Permanent rejection: never repeat the identical request.
    Permanent,
}

impl UpstreamFailureKind {
    pub const fn is_retryable(self) -> bool {
        !matches!(self, Self::Permanent)
    }
}

/// Maximum diagnostic bytes carried by one typed upstream error.
const MAX_DIAGNOSTIC_BYTES: usize = 2_000;
/// Base exponential backoff: 1s, 2s, 4s, ...
const BASE_BACKOFF: Duration = Duration::from_secs(1);
/// Hard cap for ordinary backoff waits (a valid `Retry-After` may exceed it).
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Typed upstream failure produced by the built-in OpenAI-compatible backend
/// (or returned by a custom backend to explicitly request retry handling).
///
/// The kind is the only recovery contract; HTTP status and provider code are
/// retained for diagnostics and for `Retry-After` handling. Unknown provider
/// errors must default to `Permanent` with the diagnostic preserved — the
/// runtime never guesses recoverability from arbitrary message text.
#[derive(Debug)]
pub struct LlmUpstreamError {
    kind: UpstreamFailureKind,
    status: Option<u16>,
    provider_code: Option<String>,
    retry_after: Option<Duration>,
    message: String,
    source: Option<anyhow::Error>,
}

impl LlmUpstreamError {
    pub fn new(kind: UpstreamFailureKind, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_DIAGNOSTIC_BYTES {
            let mut cut = MAX_DIAGNOSTIC_BYTES;
            while !message.is_char_boundary(cut) {
                cut -= 1;
            }
            message.truncate(cut);
            message.push('…');
        }
        Self {
            kind,
            status: None,
            provider_code: None,
            retry_after: None,
            message,
            source: None,
        }
    }

    pub fn recoverable(message: impl Into<String>) -> Self {
        Self::new(UpstreamFailureKind::Recoverable, message)
    }

    pub fn protocol_damaged(message: impl Into<String>) -> Self {
        Self::new(UpstreamFailureKind::ProtocolDamaged, message)
    }

    pub fn permanent(message: impl Into<String>) -> Self {
        Self::new(UpstreamFailureKind::Permanent, message)
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_provider_code(mut self, code: impl Into<String>) -> Self {
        self.provider_code = Some(code.into());
        self
    }

    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }

    pub fn with_source(mut self, source: anyhow::Error) -> Self {
        self.source = Some(source);
        self
    }

    pub fn kind(&self) -> UpstreamFailureKind {
        self.kind
    }

    pub fn status(&self) -> Option<u16> {
        self.status
    }

    pub fn provider_code(&self) -> Option<&str> {
        self.provider_code.as_deref()
    }

    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for LlmUpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for LlmUpstreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|error| error.as_ref() as _)
    }
}

/// Walk the wrapped chain (including `LlmRequestFailure` wrapping) to find the
/// structured upstream error. A custom backend may return the typed error
/// directly or wrapped in `LlmRequestFailure`; both must classify the same.
pub fn find_upstream_failure(error: &anyhow::Error) -> Option<&LlmUpstreamError> {
    if let Some(found) = error.downcast_ref::<LlmUpstreamError>() {
        return Some(found);
    }
    for cause in error.chain() {
        if let Some(found) = cause.downcast_ref::<LlmUpstreamError>() {
            return Some(found);
        }
        if let Some(failure) = cause.downcast_ref::<crate::llm::client::LlmRequestFailure>() {
            if let Some(found) = failure.error.downcast_ref::<LlmUpstreamError>() {
                return Some(found);
            }
            for inner in failure.error.chain() {
                if let Some(found) = inner.downcast_ref::<LlmUpstreamError>() {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// HTTP statuses that are transient for the identical request.
pub fn is_retryable_http_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// Classify one HTTP status into a recovery kind. 5xx statuses outside the
/// explicitly retryable set are treated as permanent (unknown).
pub fn classify_http_status(status: u16) -> UpstreamFailureKind {
    if is_retryable_http_status(status) {
        UpstreamFailureKind::Recoverable
    } else {
        UpstreamFailureKind::Permanent
    }
}

/// Parse a `Retry-After` header value: whole seconds or an HTTP-date
/// (IMF-fixdate, e.g. `Sun, 06 Nov 1994 08:49:37 GMT`). Invalid values yield
/// `None` so the ordinary backoff applies.
pub fn parse_retry_after(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let target = parse_http_date(raw)?;
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let seconds = target.checked_sub(now)?;
    u64::try_from(seconds).ok().map(Duration::from_secs)
}

/// IMF-fixdate parser (`Sun, 06 Nov 1994 08:49:37 GMT`). Returns a Unix
/// timestamp at UTC; the `time` crate's parsing feature is intentionally not
/// enabled for this single fixed format.
fn parse_http_date(raw: &str) -> Option<i64> {
    let mut parts = raw.split_whitespace();
    // Weekday (ignored) must be present and comma-terminated.
    let weekday = parts.next()?;
    if !weekday.ends_with(',') {
        return None;
    }
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let time_part = parts.next()?;
    let zone = parts.next()?;
    if !matches!(zone, "GMT" | "UTC") {
        return None;
    }
    let mut clock = time_part.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if clock.next().is_some() || !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return None;
    }
    if !(0..=60).contains(&second) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since 1970-01-01 (Howard Hinnant's civil-days algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Pure exponential backoff with bounded jitter.
///
/// `attempt` is the 0-based retry index (the wait before retry 1 uses 0);
/// `jitter` in `[0, 1)` scales the base by 0.75..=1.25. The result never
/// exceeds [`MAX_BACKOFF`].
pub fn backoff_delay(attempt: u32, jitter: f64) -> Duration {
    let shift = attempt.min(4);
    let base = BASE_BACKOFF.saturating_mul(1u32 << shift).min(MAX_BACKOFF);
    let scaled = base.mul_f64(0.75 + 0.5 * jitter.clamp(0.0, 1.0));
    scaled.min(MAX_BACKOFF)
}

/// Clock-derived jitter fraction in `[0, 1)`, avoiding a `rand` dependency.
pub(crate) fn jitter_fraction() -> f64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos.rem_euclid(1_000_000) as f64) / 1_000_000.0
}

/// Per-logical-request retry accounting.
///
/// One state instance covers a single logical request (round attempt in the
/// agent loop, or one compaction summary request). Physical callers create a
/// fresh instance for each; it owns only counters, the optional deadline and
/// the next-wait calculation.
#[derive(Debug)]
pub(crate) struct RequestRetryState {
    max_retries: u32,
    used_retries: u32,
    deadline: Option<Instant>,
}

/// The optional total request deadline would be exceeded by the next wait.
#[derive(Debug)]
pub(crate) struct DeadlineExceeded;

impl RequestRetryState {
    /// Retry state whose deadline is the earlier of the configured
    /// per-logical-request deadline and an absolute round deadline: overflow
    /// re-projections inside one round must not extend the total budget.
    pub(crate) fn new_bounded(
        policy: &crate::config::LlmRecoveryPolicy,
        absolute_deadline: Option<Instant>,
    ) -> Self {
        let own = policy
            .request_timeout_secs
            .map(|secs| Instant::now() + Duration::from_secs(secs));
        let deadline = match (own, absolute_deadline) {
            (Some(own), Some(absolute)) => Some(own.min(absolute)),
            (Some(own), None) => Some(own),
            (None, Some(absolute)) => Some(absolute),
            (None, None) => None,
        };
        Self {
            max_retries: policy.request_max_retries,
            used_retries: 0,
            deadline,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_deadline(max_retries: u32, deadline: Option<Instant>) -> Self {
        Self {
            max_retries,
            used_retries: 0,
            deadline,
        }
    }

    pub(crate) fn used_retries(&self) -> u32 {
        self.used_retries
    }

    /// Whether one more attempt (beyond those already started) may begin now.
    pub(crate) fn can_retry(&self) -> bool {
        self.used_retries < self.max_retries && !self.deadline_expired()
    }

    pub(crate) fn note_retry(&mut self) {
        self.used_retries = self.used_retries.saturating_add(1);
    }

    pub(crate) fn deadline_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Absolute deadline of the whole logical request (every attempt and every
    /// wait). `None` means no configured total timeout.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Wait before the next retry: `max(backoff, retry_after)`, bounded by the
    /// optional remaining deadline (which is never extended by a wait).
    pub(crate) fn retry_wait(
        &self,
        attempt: u32,
        retry_after: Option<Duration>,
        jitter: f64,
    ) -> std::result::Result<Duration, DeadlineExceeded> {
        let mut wait = backoff_delay(attempt, jitter);
        if let Some(retry_after) = retry_after {
            wait = wait.max(retry_after);
        }
        if let Some(deadline) = self.deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if wait > remaining {
                return Err(DeadlineExceeded);
            }
        }
        Ok(wait)
    }
}

/// Terminal reason of one logical request, shared by the caller error paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestTerminal {
    /// Retry calls, optional deadline or format-window budget exhausted.
    RetryExhausted,
    /// The optional total request deadline elapsed.
    Timeout,
}

impl RequestTerminal {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::RetryExhausted => "request_retry_exhausted",
            Self::Timeout => "request_timeout",
        }
    }
}

/// Sleep until the optional deadline; without one this future never resolves,
/// so callers can keep a uniform `select!` shape.
pub(crate) async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending::<()>().await,
    }
}

/// Build the terminal error for one exhausted logical request. `detail` is a
/// bounded diagnostic (last failure reason / counts).
pub(crate) fn terminal_error(
    terminal: RequestTerminal,
    detail: impl std::fmt::Display,
) -> anyhow::Error {
    anyhow::Error::new(LlmUpstreamError::permanent(format!(
        "{}: {}",
        terminal.message(),
        detail
    )))
}

/// Classify a reqwest transport error: connection/timeout failures are
/// retryable, everything else defaults to permanent with the diagnostic
/// preserved.
pub(crate) fn classify_transport_error(error: &reqwest::Error) -> UpstreamFailureKind {
    if error.is_timeout() || error.is_connect() || error.is_body() {
        return UpstreamFailureKind::Recoverable;
    }
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("connection reset")
        || message.contains("connection closed")
        || message.contains("broken pipe")
        || message.contains("unexpected eof")
        || message.contains("incomplete message")
    {
        return UpstreamFailureKind::Recoverable;
    }
    UpstreamFailureKind::Permanent
}

/// Build a typed error from an HTTP response status and bounded body text.
pub(crate) fn http_failure(
    status: u16,
    body: &str,
    retry_after: Option<Duration>,
) -> LlmUpstreamError {
    let kind = classify_http_status(status);
    let body = body.trim();
    let message = if body.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {body}")
    };
    LlmUpstreamError::new(kind, message)
        .with_status(status)
        .with_retry_after(retry_after)
}

/// Classify a provider error envelope delivered inside a `200` stream by its
/// structured code/type/status fields. Unknown envelopes stay permanent.
pub(crate) fn classify_provider_error(
    code: Option<&str>,
    status: Option<u16>,
) -> UpstreamFailureKind {
    if let Some(status) = status {
        return classify_http_status(status);
    }
    let Some(code) = code else {
        return UpstreamFailureKind::Permanent;
    };
    let code = code.to_ascii_lowercase();
    if code.contains("rate_limit")
        || code.contains("rate limit")
        || code.contains("overloaded")
        || code.contains("server_error")
        || code.contains("timeout")
        || code.contains("unavailable")
    {
        return UpstreamFailureKind::Recoverable;
    }
    UpstreamFailureKind::Permanent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_exponential_capped_and_jittered() {
        assert_eq!(backoff_delay(0, 0.5), Duration::from_millis(1000));
        assert_eq!(backoff_delay(1, 0.5), Duration::from_millis(2000));
        assert_eq!(backoff_delay(2, 0.5), Duration::from_millis(4000));
        assert_eq!(backoff_delay(3, 0.5), Duration::from_millis(8000));
        // Base caps at 10s: 16s*0.75 == 12s would exceed the cap, so the jitter
        // window is applied on the capped value too.
        assert_eq!(backoff_delay(4, 0.5), Duration::from_secs(10));
        assert_eq!(backoff_delay(9, 1.0), Duration::from_secs(10));
        assert!(backoff_delay(2, 0.0) >= Duration::from_millis(3000));
        assert!(backoff_delay(2, 0.999) <= Duration::from_secs(5));
    }

    #[test]
    fn retry_after_parses_seconds_and_http_date() {
        assert_eq!(parse_retry_after("7"), Some(Duration::from_secs(7)));
        // Past HTTP-date parses but clamps to no wait (None), never negative.
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT"), None);
        assert_eq!(parse_retry_after("not-a-date"), None);
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("06 Nov 1994 08:49:37 GMT"), None);
        let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        let raw = format!(
            "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
            future.weekday(),
            future.day(),
            match future.month() {
                time::Month::January => "Jan",
                time::Month::February => "Feb",
                time::Month::March => "Mar",
                time::Month::April => "Apr",
                time::Month::May => "May",
                time::Month::June => "Jun",
                time::Month::July => "Jul",
                time::Month::August => "Aug",
                time::Month::September => "Sep",
                time::Month::October => "Oct",
                time::Month::November => "Nov",
                time::Month::December => "Dec",
            },
            future.year(),
            future.hour(),
            future.minute(),
            future.second(),
        );
        let parsed =
            parse_retry_after(&raw).unwrap_or_else(|| panic!("future date {raw:?} parses"));
        assert!(parsed > Duration::from_secs(3500));
        assert!(parsed < Duration::from_secs(3700));
    }

    #[test]
    fn http_classification_matches_the_design_table() {
        for status in [408, 409, 425, 429, 500, 502, 503, 504] {
            assert_eq!(
                classify_http_status(status),
                UpstreamFailureKind::Recoverable
            );
        }
        for status in [400, 401, 403, 404, 422, 501, 505] {
            assert_eq!(classify_http_status(status), UpstreamFailureKind::Permanent);
        }
        assert_eq!(
            classify_provider_error(None, Some(503)),
            UpstreamFailureKind::Recoverable
        );
        assert_eq!(
            classify_provider_error(Some("rate_limit_exceeded"), None),
            UpstreamFailureKind::Recoverable
        );
        // Unknown envelopes stay permanent: no text guessing.
        assert_eq!(
            classify_provider_error(Some("weird_unknown_code"), None),
            UpstreamFailureKind::Permanent
        );
    }

    #[test]
    fn retry_state_counts_and_deadline() {
        let mut state = RequestRetryState::with_deadline(2, None);
        assert!(state.can_retry());
        state.note_retry();
        state.note_retry();
        assert!(!state.can_retry());
        // Deadline in the past refuses retries even with budget left.
        let expired =
            RequestRetryState::with_deadline(3, Some(Instant::now() - Duration::from_secs(1)));
        assert!(expired.deadline_expired());
        assert!(!expired.can_retry());
        // A wait beyond the remaining deadline is refused instead of extending
        // it.
        let short =
            RequestRetryState::with_deadline(3, Some(Instant::now() + Duration::from_millis(5)));
        assert!(short.retry_wait(0, None, 0.5).is_err());
        // Retry-After is a floor, not capped by the ordinary backoff cap.
        let waited = RequestRetryState::with_deadline(3, None)
            .retry_wait(0, Some(Duration::from_secs(30)), 0.5)
            .expect("no deadline");
        assert_eq!(waited, Duration::from_secs(30));
    }

    #[test]
    fn bounded_retry_state_uses_the_earlier_deadline() {
        let policy = crate::config::LlmRecoveryPolicy {
            request_max_retries: 2,
            request_timeout_secs: Some(600),
            ..Default::default()
        };
        // 绝对期限更早：立即过期且拒绝重试。
        let expired =
            RequestRetryState::new_bounded(&policy, Some(Instant::now() - Duration::from_secs(1)));
        assert!(expired.deadline_expired());
        assert!(!expired.can_retry());
        // 无绝对期限时退回自身预算。
        let own = RequestRetryState::new_bounded(&policy, None);
        assert!(!own.deadline_expired());
        assert!(own.can_retry());
        // 未配置总期限时，绝对期限仍生效。
        let no_policy = crate::config::LlmRecoveryPolicy {
            request_timeout_secs: None,
            ..Default::default()
        };
        let absolute = RequestRetryState::new_bounded(
            &no_policy,
            Some(Instant::now() + Duration::from_secs(30)),
        );
        assert!(absolute.deadline().is_some());
    }

    #[test]
    fn typed_error_keeps_status_code_and_source() {
        let error =
            anyhow::Error::new(http_failure(502, "bad gateway", None)).context("request failed");
        let typed = find_upstream_failure(&error).expect("typed root is preserved");
        assert_eq!(typed.kind(), UpstreamFailureKind::Recoverable);
        assert_eq!(typed.status(), Some(502));
        // The full chain keeps the bounded diagnostic (context layer included).
        assert!(format!("{error:#}").contains("502"));
    }
}
