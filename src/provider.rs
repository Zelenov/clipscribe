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
    /// The name used in error messages and the CLI's `--provider` values.
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
    /// The provider refused the request; its own message.
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
