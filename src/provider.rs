//! What an AI provider is asked and answers, independent of any one provider's API.
//!
//! Shaped around a request, not around clips: a clip description, a subtitles-only summary
//! or another provider later all fit it.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Which AI service answers a request; see [`crate::Model::provider`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Provider {
    #[default]
    Anthropic,
    OpenAi,
}

impl Provider {
    /// The proper name used in error messages (`AiError::reason`, `stops_job`) and
    /// `retry_loop`'s own rate-limit message. Not the CLI's `--provider` value (lowercase, e.g.
    /// `"openai"`), which the `clipscribe` binary's own `ProviderArg` owns instead.
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
        }
    }
}

/// Time for the answer; a request that has not answered by then (plus its upload time, see
/// [`timeout_for`]) fails the file. See [`crate::AiError::Timeout`].
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);
/// Upload speed the timeout allows for, in bytes per second.
const SLOW_UPLOAD_BYTES_PER_S: u64 = 50_000;
/// Shared by every [`AiProvider`]: none of them should wait longer than this just to open the
/// connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The whole request's timeout: `answer` plus the upload of `body_len` bytes on a slow uplink,
/// so a request is not given up while it is still being sent.
pub fn timeout_for(answer: Duration, body_len: usize) -> Duration {
    answer + Duration::from_secs(body_len as u64 / SLOW_UPLOAD_BYTES_PER_S)
}

/// How failed requests are retried; shared by every [`AiProvider`] implementation.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Waits before each retry of a server error or a lost connection; one retry per entry.
    pub delays: Vec<Duration>,
    /// Wait after a 429 without a `retry-after` header. A 429 uses up no retry: a long job
    /// reaching a new key's per-minute limit should slow down, not fail.
    pub rate_limit_wait: Duration,
    /// Waits are slept in steps this long, checking the cancel flag between them.
    pub step: Duration,
    /// 429s in a row after which the request fails instead of waiting again: an account
    /// whose per-minute limit is below one request would otherwise wait forever.
    pub max_rate_limit_waits: usize,
    /// Time for the answer, before the upload time is added (see [`timeout_for`]).
    pub answer_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            delays: vec![
                Duration::from_secs(2),
                Duration::from_secs(8),
                Duration::from_secs(30),
            ],
            rate_limit_wait: Duration::from_secs(30),
            step: Duration::from_millis(250),
            max_rate_limit_waits: 20,
            answer_timeout: ANSWER_TIMEOUT,
        }
    }
}

/// One piece of a request's content, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum AiContent {
    Text(String),
    /// A JPEG image.
    Jpeg(Vec<u8>),
}

/// A request for one JSON answer.
#[derive(Debug, Clone, PartialEq)]
pub struct AiRequest {
    /// The provider's model id.
    pub model: String,
    pub content: Vec<AiContent>,
    /// JSON schema the answer must follow.
    pub schema: serde_json::Value,
    pub max_tokens: u32,
    /// How much the model may think (`low` … `max`); `None` leaves it out, for models
    /// without the setting.
    pub effort: Option<&'static str>,
}

/// Tokens a request was billed for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AiUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl std::ops::AddAssign for AiUsage {
    fn add_assign(&mut self, other: Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// A provider's answer: the JSON it returned and what it cost.
#[derive(Debug, Clone, PartialEq)]
pub struct AiResponse {
    /// The answer. `Null` when the model stopped before writing one.
    pub json: serde_json::Value,
    /// Why the model stopped; `"end_turn"` when it finished its answer.
    pub stop_reason: String,
    pub usage: AiUsage,
}

/// Why a request got no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiError {
    /// The provider rejected the key (401/403). Stops the job. The provider's display name
    /// (see [`Provider::label`]).
    KeyRejected(String),
    /// The account has no credit left. Stops the job. The provider's display name.
    OutOfCredit(String),
    /// A usage or spend limit of the account was reached; the API's message. Stops the job.
    LimitReached(String),
    /// No connection, or the provider kept failing after the retries.
    Network(String),
    /// No answer within the request timeout. Not retried: the provider may have billed it.
    Timeout,
    /// The request could not be sent as asked: the provider refused it (its own message), it
    /// could not even be built for something only the caller can fix (e.g. a request needing a
    /// parameter this client has no way to send yet), or retries and rate-limit waits ran out.
    Rejected(String),
    /// The answer could not be read.
    BadAnswer(String),
    /// The job was cancelled during a wait.
    Cancelled,
}

impl AiError {
    /// Why, in the words the failed list shows.
    pub fn reason(&self) -> String {
        match self {
            Self::KeyRejected(provider) => format!("{provider} rejected the key"),
            Self::OutOfCredit(provider) => format!("The {provider} account has no credit left"),
            Self::Network(_) => "Network error".to_string(),
            Self::LimitReached(message) => message.clone(),
            Self::Timeout => "No answer in time".to_string(),
            Self::Rejected(message) => message.clone(),
            Self::BadAnswer(_) => "The answer could not be read".to_string(),
            Self::Cancelled => "Cancelled".to_string(),
        }
    }

    /// The summary line of a job this error stops, or `None` when only the file fails.
    pub fn stops_job(&self) -> Option<String> {
        match self {
            Self::KeyRejected(provider) => Some(format!(
                "Stopped: {provider} rejected the key. Check it in Settings."
            )),
            Self::OutOfCredit(provider) => Some(format!(
                "Stopped: the {provider} account has no credit left."
            )),
            Self::LimitReached(message) => Some(format!("Stopped: {message}")),
            _ => None,
        }
    }
}

/// Something that answers [`AiRequest`]s. Blocking: runs on a worker thread. `cancel` is
/// checked during waits between retries.
pub trait AiProvider {
    fn complete(&self, request: &AiRequest, cancel: &AtomicBool) -> Result<AiResponse, AiError>;
}

/// What one HTTP attempt decided, for [`retry_loop`]. Every provider's own `classify` builds
/// this from its response; nothing past this point is provider-specific.
pub(crate) enum Attempt {
    Done(Result<AiResponse, AiError>),
    /// Retry after the policy's next delay, if any is left.
    Retry(String),
    /// Wait this long, then try again without using up a retry.
    RateLimited(Duration),
}

/// A pause shared by several clients of one provider, so that a 429 on one of them holds all of
/// them back instead of each sending into the limit until it gets its own 429: a folder run
/// ([`crate::describe_folder`]) gives every worker's client the same gate. [`retry_loop`] closes it
/// for a 429's wait and waits for it to open before every attempt.
#[derive(Debug, Default)]
pub(crate) struct RateGate {
    open_at: std::sync::Mutex<Option<std::time::Instant>>,
}

impl RateGate {
    /// Keep the gate closed for at least `duration` from now (never shortens a longer pause).
    fn close_for(&self, duration: Duration) {
        let until = std::time::Instant::now() + duration;
        let mut open_at = self.open_at.lock().unwrap_or_else(|e| e.into_inner());
        if open_at.is_none_or(|at| at < until) {
            *open_at = Some(until);
        }
    }

    /// How long until the gate opens; zero when it is open.
    fn closed_for(&self) -> Duration {
        let open_at = self.open_at.lock().unwrap_or_else(|e| e.into_inner());
        open_at.map_or(Duration::ZERO, |at| {
            at.saturating_duration_since(std::time::Instant::now())
        })
    }

    /// Wait until the gate is open, however often another client closes it again meanwhile.
    fn wait_open(&self, retry: &RetryPolicy, cancel: &AtomicBool) -> Result<(), AiError> {
        loop {
            let left = self.closed_for();
            if left.is_zero() {
                return Ok(());
            }
            log::info!(
                "ai: another request was rate limited, waiting {} s",
                left.as_secs()
            );
            wait(retry, left, cancel)?;
        }
    }
}

/// Sleep `duration` in steps, returning `Cancelled` as soon as `cancel` is set.
fn wait(retry: &RetryPolicy, duration: Duration, cancel: &AtomicBool) -> Result<(), AiError> {
    use std::sync::atomic::Ordering;

    let mut left = duration;
    while !left.is_zero() {
        if cancel.load(Ordering::Relaxed) {
            return Err(AiError::Cancelled);
        }
        let step = left.min(retry.step);
        std::thread::sleep(step);
        left -= step;
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(AiError::Cancelled);
    }
    Ok(())
}

/// The retry loop every [`AiProvider`] runs: keeps calling `attempt` (one HTTP round-trip,
/// classified into an [`Attempt`]) until it is done, retrying a transient failure after
/// `retry`'s next delay and waiting out rate limits without spending one. `provider_label`
/// names who ran out of retries or waits (see [`Provider::label`]). `gate`, when given, is shared
/// with other clients: every attempt waits for it to be open, and a rate limit closes it for all.
pub(crate) fn retry_loop(
    retry: &RetryPolicy,
    provider_label: &str,
    gate: Option<&RateGate>,
    cancel: &AtomicBool,
    mut attempt: impl FnMut() -> Attempt,
) -> Result<AiResponse, AiError> {
    let mut retries = retry.delays.iter();
    let mut rate_limited = 0;
    loop {
        if let Some(gate) = gate {
            gate.wait_open(retry, cancel)?;
        }
        match attempt() {
            Attempt::Done(result) => return result,
            Attempt::RateLimited(_) if rate_limited >= retry.max_rate_limit_waits => {
                return Err(AiError::Rejected(format!(
                    "{provider_label}'s rate limit was still reached after many waits"
                )));
            }
            Attempt::RateLimited(duration) => {
                rate_limited += 1;
                if let Some(gate) = gate {
                    gate.close_for(duration);
                }
                log::info!("ai: rate limited, waiting {} s", duration.as_secs());
                wait(retry, duration, cancel)?;
            }
            Attempt::Retry(why) => {
                rate_limited = 0;
                let Some(delay) = retries.next() else {
                    return Err(AiError::Network(why));
                };
                log::warn!("ai: {why}; retrying in {} s", delay.as_secs());
                wait(retry, *delay, cancel)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    /// `anthropic.rs` and `openai.rs` each classify their own HTTP responses into an [`Attempt`]
    /// and test that mapping against a real mock server; these tests cover `retry_loop`/`wait`
    /// themselves, generically, with a fake `attempt` that needs no HTTP at all — the mechanism
    /// both providers share, tested once instead of twice.
    fn fast_retries() -> RetryPolicy {
        RetryPolicy {
            delays: vec![Duration::from_millis(1); 3],
            rate_limit_wait: Duration::from_millis(1),
            step: Duration::from_millis(1),
            max_rate_limit_waits: 20,
            answer_timeout: Duration::from_millis(500),
        }
    }

    fn ok() -> Attempt {
        Attempt::Done(Ok(AiResponse {
            json: serde_json::json!({"ok": true}),
            stop_reason: "end_turn".to_string(),
            usage: AiUsage::default(),
        }))
    }

    #[test]
    fn a_done_attempt_is_returned_without_retrying() {
        let calls = Arc::new(Mutex::new(0));
        let seen = calls.clone();
        let result = retry_loop(
            &fast_retries(),
            "Test",
            None,
            &AtomicBool::new(false),
            || {
                *seen.lock().expect("lock") += 1;
                Attempt::Done(Err(AiError::Timeout))
            },
        );
        assert_eq!(result, Err(AiError::Timeout));
        assert_eq!(*calls.lock().expect("lock"), 1, "not retried");
    }

    #[test]
    fn rate_limits_wait_without_spending_a_retry() {
        let calls = Arc::new(Mutex::new(0));
        let seen = calls.clone();
        // More rate-limited attempts than `delays` has entries: if a rate limit consumed a
        // retry, this would fail with `Network` before ever reaching `ok()`.
        let result = retry_loop(
            &fast_retries(),
            "Test",
            None,
            &AtomicBool::new(false),
            move || {
                let mut n = seen.lock().expect("lock");
                *n += 1;
                if *n <= 5 {
                    Attempt::RateLimited(Duration::from_millis(1))
                } else {
                    ok()
                }
            },
        );
        assert!(result.is_ok());
        assert_eq!(*calls.lock().expect("lock"), 6);
    }

    #[test]
    fn retries_are_exhausted_then_fails_with_the_last_reason() {
        // A different reason each call, so the assertion can't pass on the first or a hardcoded
        // one: it must be whichever `Retry` used up the final delay.
        let calls = Arc::new(Mutex::new(0));
        let seen = calls.clone();
        let result = retry_loop(
            &fast_retries(),
            "Test",
            None,
            &AtomicBool::new(false),
            move || {
                let mut n = seen.lock().expect("lock");
                *n += 1;
                Attempt::Retry(format!("attempt {n} failed"))
            },
        );
        assert_eq!(
            result,
            Err(AiError::Network("attempt 4 failed".to_string()))
        );
        assert_eq!(
            *calls.lock().expect("lock"),
            4,
            "3 delays, then it gives up"
        );
    }

    #[test]
    fn endless_rate_limits_fail_instead_of_waiting_forever() {
        let calls = Arc::new(Mutex::new(0));
        let seen = calls.clone();
        let result = retry_loop(
            &fast_retries(),
            "Test",
            None,
            &AtomicBool::new(false),
            move || {
                *seen.lock().expect("lock") += 1;
                Attempt::RateLimited(Duration::from_millis(1))
            },
        );
        assert!(
            matches!(&result, Err(AiError::Rejected(m)) if m.contains("Test")),
            "{result:?}"
        );
        assert_eq!(
            *calls.lock().expect("lock"),
            21,
            "20 waits, then it gives up"
        );
    }

    #[test]
    fn a_cancel_during_a_wait_ends_the_request() {
        let policy = RetryPolicy {
            rate_limit_wait: Duration::from_secs(30),
            ..fast_retries()
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            flag.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let result = retry_loop(&policy, "Test", None, &cancel, || {
            Attempt::RateLimited(Duration::from_secs(30))
        });
        assert_eq!(result, Err(AiError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// With several clients sharing one gate (a folder run's workers), a 429 on one of them holds
    /// back the *other's* next attempt for the 429's wait, instead of letting it send into the
    /// limit too.
    #[test]
    fn a_rate_limit_on_one_client_holds_back_another_sharing_the_gate() {
        let gate = Arc::new(RateGate::default());
        let first_gate = gate.clone();
        let first = std::thread::spawn(move || {
            let mut calls = 0;
            retry_loop(
                &fast_retries(),
                "Test",
                Some(&first_gate),
                &AtomicBool::new(false),
                || {
                    calls += 1;
                    if calls == 1 {
                        Attempt::RateLimited(Duration::from_millis(300))
                    } else {
                        ok()
                    }
                },
            )
        });
        // Start the second client only once the first one's 429 has closed the gate (polling
        // the gate itself, not a fixed sleep that a slow machine could outlast), and note when it
        // opens again.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let opens_at = loop {
            // Read before the gate is asked, so `opens_at` is never later than its real time.
            let now = std::time::Instant::now();
            let left = gate.closed_for();
            if !left.is_zero() {
                break now + left;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the first client never closed the gate"
            );
            std::thread::yield_now();
        };
        let mut attempted_at = None;
        let second = retry_loop(
            &fast_retries(),
            "Test",
            Some(&gate),
            &AtomicBool::new(false),
            || {
                attempted_at = Some(std::time::Instant::now());
                ok()
            },
        );
        assert!(second.is_ok());
        assert!(first.join().expect("thread").is_ok());
        let attempted_at = attempted_at.expect("attempted");
        assert!(
            attempted_at >= opens_at,
            "the second client waited for the first one's 429: {:?} early",
            opens_at.saturating_duration_since(attempted_at)
        );
    }

    #[test]
    fn a_gate_is_never_shortened_by_a_shorter_pause() {
        let gate = RateGate::default();
        assert!(
            gate.closed_for().is_zero(),
            "open until something closes it"
        );
        gate.close_for(Duration::from_secs(60));
        gate.close_for(Duration::from_millis(1));
        assert!(gate.closed_for() > Duration::from_secs(50));
    }
}
