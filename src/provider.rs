//! What an AI provider is asked and answers, independent of any one provider's API.
//!
//! Shaped around a request, not around clips: a clip description, a subtitles-only summary
//! or another provider later all fit it.

use std::sync::atomic::AtomicBool;

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
    /// The provider rejected the key (401/403). Stops the job.
    KeyRejected,
    /// The account has no credit left. Stops the job.
    OutOfCredit,
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
            Self::KeyRejected => "Anthropic rejected the key".to_string(),
            Self::OutOfCredit => "The Anthropic account has no credit left".to_string(),
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
            Self::KeyRejected => {
                Some("Stopped: Anthropic rejected the key. Check it in Settings.".to_string())
            }
            Self::OutOfCredit => {
                Some("Stopped: the Anthropic account has no credit left.".to_string())
            }
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
