//! Claude through Anthropic's Messages API, over plain HTTP (there is no official Rust SDK).

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::provider::{
    self, AiContent, AiError, AiProvider, AiRequest, AiResponse, AiUsage, Attempt, Provider,
    CONNECT_TIMEOUT,
};

/// Kept for anyone already naming `anthropic::RetryPolicy`: the type now lives in
/// [`crate::provider`], shared with the OpenAI client, since none of it was ever
/// Anthropic-specific.
pub use crate::provider::{timeout_for, RetryPolicy};

const API_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";

/// The Anthropic provider, for one key.
pub struct Anthropic {
    key: String,
    base_url: String,
    retry: RetryPolicy,
    client: reqwest::blocking::Client,
}

impl Anthropic {
    pub fn new(key: String) -> Result<Self, AiError> {
        Self::with_endpoint(key, API_URL.to_string(), RetryPolicy::default())
    }

    /// A provider talking to `base_url` (tests use a local server).
    pub fn with_endpoint(
        key: String,
        base_url: String,
        retry: RetryPolicy,
    ) -> Result<Self, AiError> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| AiError::Network(e.to_string()))?;
        Ok(Self {
            key,
            base_url,
            retry,
            client,
        })
    }

    fn body(request: &AiRequest) -> Value {
        let content: Vec<Value> = request
            .content
            .iter()
            .map(|block| match block {
                AiContent::Text(text) => json!({"type": "text", "text": text}),
                AiContent::Jpeg(bytes) => json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/jpeg",
                        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                    }
                }),
            })
            .collect();
        let mut output_config =
            json!({"format": {"type": "json_schema", "schema": request.schema}});
        if let Some(effort) = request.effort {
            output_config["effort"] = json!(effort);
        }
        json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "messages": [{"role": "user", "content": content}],
            "output_config": output_config,
        })
    }
}

impl AiProvider for Anthropic {
    fn complete(&self, request: &AiRequest, cancel: &AtomicBool) -> Result<AiResponse, AiError> {
        let body = Self::body(request).to_string();
        provider::retry_loop(&self.retry, Provider::Anthropic.label(), cancel, || {
            self.attempt(&body)
        })
    }
}

impl Anthropic {
    fn attempt(&self, body: &str) -> Attempt {
        let sent = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .timeout(timeout_for(self.retry.answer_timeout, body.len()))
            .body(body.to_string())
            .send();
        let response = match sent {
            Ok(response) => response,
            // Nothing reached Anthropic yet: safe to try again.
            Err(e) if e.is_connect() => return Attempt::Retry(format!("connection failed: {e}")),
            // The request could not even be built (e.g. a pasted key with a control character):
            // trying again cannot help, and it is not the network.
            Err(e) if e.is_builder() => {
                return Attempt::Done(Err(AiError::Rejected(format!(
                    "The request could not be made; check the API key ({e})"
                ))))
            }
            Err(e) if e.is_timeout() => return Attempt::Done(Err(AiError::Timeout)),
            Err(e) => return Attempt::Retry(format!("connection failed: {e}")),
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let text = match response.text() {
            Ok(text) => text,
            Err(e) if e.is_timeout() => return Attempt::Done(Err(AiError::Timeout)),
            Err(e) => return Attempt::Retry(format!("reading the answer failed: {e}")),
        };
        classify(status, retry_after, &text, self.retry.rate_limit_wait)
    }
}

/// Turn one HTTP answer into the next step.
fn classify(
    status: u16,
    retry_after: Option<Duration>,
    text: &str,
    rate_limit_wait: Duration,
) -> Attempt {
    let json: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let message = json["error"]["message"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {status}"));
    match status {
        200 => Attempt::Done(parse_message(&json)),
        401 | 403 => Attempt::Done(Err(AiError::KeyRejected(
            Provider::Anthropic.label().to_string(),
        ))),
        402 => Attempt::Done(Err(AiError::OutOfCredit(
            Provider::Anthropic.label().to_string(),
        ))),
        400 if message.to_lowercase().contains("credit balance") => Attempt::Done(Err(
            AiError::OutOfCredit(Provider::Anthropic.label().to_string()),
        )),
        400 if is_limit(&message) => Attempt::Done(Err(AiError::LimitReached(message))),
        429 => Attempt::RateLimited(retry_after.unwrap_or(rate_limit_wait)),
        500 | 502 | 503 | 529 => Attempt::Retry(format!("HTTP {status}: {message}")),
        _ => Attempt::Done(Err(AiError::Rejected(message))),
    }
}

/// Whether a 400's message is about a usage or spend limit of the account, which every later
/// request would hit too.
fn is_limit(message: &str) -> bool {
    let message = message.to_lowercase();
    ["usage limit", "spend limit", "spending limit"]
        .iter()
        .any(|phrase| message.contains(phrase))
}

/// Read a Messages API answer: the JSON in its text block, why it stopped, and its usage.
fn parse_message(json: &Value) -> Result<AiResponse, AiError> {
    let usage = AiUsage {
        input_tokens: json["usage"]["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: json["usage"]["output_tokens"].as_u64().unwrap_or(0),
    };
    let stop_reason = json["stop_reason"].as_str().unwrap_or_default().to_string();
    let text: String = json["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect()
        })
        .unwrap_or_default();
    // A model that stopped early (max_tokens, refusal) may have written no valid JSON; the
    // caller fails the file with the stop reason and still counts the usage.
    let answer = serde_json::from_str(&text).unwrap_or(Value::Null);
    if answer.is_null() && stop_reason == "end_turn" {
        return Err(AiError::BadAnswer(format!("not JSON: {text}")));
    }
    Ok(AiResponse {
        json: answer,
        stop_reason,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A local HTTP server answering each request with the next canned response, recording
    /// how many it got.
    fn server(responses: Vec<String>) -> (String, Arc<Mutex<usize>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let count = Arc::new(Mutex::new(0));
        let seen = count.clone();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                *seen.lock().expect("lock") += 1;
                let _ = reader.get_mut().write_all(response.as_bytes());
            }
        });
        (url, count)
    }

    fn http(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\n{headers}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn ok() -> String {
        http(
            "200 OK",
            "",
            r#"{"content":[{"type":"text","text":"{\"summary\":\"S\",\"segments\":[]}"}],"stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":20}}"#,
        )
    }

    fn fast_retries() -> RetryPolicy {
        RetryPolicy {
            delays: vec![Duration::from_millis(1); 3],
            rate_limit_wait: Duration::from_millis(1),
            step: Duration::from_millis(1),
            max_rate_limit_waits: 20,
            answer_timeout: Duration::from_millis(500),
        }
    }

    fn request() -> AiRequest {
        AiRequest {
            model: "claude-haiku-4-5".to_string(),
            content: vec![
                AiContent::Text("hi".to_string()),
                AiContent::Jpeg(vec![1, 2]),
            ],
            schema: json!({"type": "object"}),
            max_tokens: 10,
            effort: None,
        }
    }

    fn complete(responses: Vec<String>) -> (Result<AiResponse, AiError>, usize) {
        let (url, count) = server(responses);
        let provider = Anthropic::with_endpoint("k".into(), url, fast_retries()).expect("client");
        let result = provider.complete(&request(), &AtomicBool::new(false));
        let n = *count.lock().expect("lock");
        (result, n)
    }

    #[test]
    fn the_body_carries_images_and_the_schema() {
        let body = Anthropic::body(&request());
        assert_eq!(body["messages"][0]["content"][1]["source"]["data"], "AQI=");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert!(body.get("thinking").is_none());
        assert!(
            body["output_config"].get("effort").is_none(),
            "Haiku 4.5 rejects it"
        );
        let body = Anthropic::body(&AiRequest {
            effort: Some("low"),
            ..request()
        });
        assert_eq!(body["output_config"]["effort"], "low");
    }

    #[test]
    fn an_answer_gives_its_json_and_usage() {
        let (result, n) = complete(vec![ok()]);
        let response = result.expect("answer");
        assert_eq!(response.json["summary"], "S");
        assert_eq!(response.usage.input_tokens, 100);
        assert_eq!(n, 1);
    }

    #[test]
    fn overloaded_is_retried_three_times_then_fails() {
        let overloaded = || {
            http(
                "529 Overloaded",
                "",
                r#"{"error":{"message":"Overloaded"}}"#,
            )
        };
        let (result, n) = complete(vec![overloaded(), overloaded(), ok()]);
        assert!(result.is_ok());
        assert_eq!(n, 3);
        let (result, n) = complete(vec![overloaded(); 4]);
        assert!(matches!(result, Err(AiError::Network(_))));
        assert_eq!(n, 4);
    }

    #[test]
    fn a_rejected_key_or_no_credit_stops_the_job() {
        let (result, _) = complete(vec![http("401 Unauthorized", "", "{}")]);
        assert_eq!(result, Err(AiError::KeyRejected("Anthropic".to_string())));
        assert!(AiError::KeyRejected("Anthropic".to_string())
            .stops_job()
            .is_some());
        let credit = r#"{"error":{"message":"Your credit balance is too low to access the Anthropic API."}}"#;
        let (result, _) = complete(vec![http("400 Bad Request", "", credit)]);
        assert_eq!(result, Err(AiError::OutOfCredit("Anthropic".to_string())));
    }

    #[test]
    fn a_reached_spend_limit_stops_the_job_with_the_apis_message() {
        let body = r#"{"error":{"message":"You have reached your specified workspace API usage limits."}}"#;
        let (result, _) = complete(vec![http("400 Bad Request", "", body)]);
        let Err(error) = result else {
            panic!("an error");
        };
        assert!(matches!(error, AiError::LimitReached(_)));
        assert!(error
            .stops_job()
            .is_some_and(|s| s.contains("usage limits")));
    }

    /// A request that got no answer in time may have been billed: it is not sent again.
    #[test]
    fn a_timeout_is_not_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let count = Arc::new(Mutex::new(0));
        let seen = count.clone();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept() {
                *seen.lock().expect("lock") += 1;
                held.push(stream); // read nothing, answer nothing
            }
        });
        let provider = Anthropic::with_endpoint("k".into(), url, fast_retries()).expect("client");
        let result = provider.complete(&request(), &AtomicBool::new(false));
        assert_eq!(result, Err(AiError::Timeout));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(*count.lock().expect("lock"), 1, "sent once");
    }

    #[test]
    fn a_429_waits_as_long_as_retry_after_says() {
        let wait = |retry_after| match classify(429, retry_after, "{}", Duration::from_secs(30)) {
            Attempt::RateLimited(wait) => wait,
            _ => panic!("a 429 waits"),
        };
        assert_eq!(wait(Some(Duration::from_secs(7))), Duration::from_secs(7));
        assert_eq!(wait(None), Duration::from_secs(30));
        let (url, count) = server(vec![
            http("429 Too Many Requests", "Retry-After: 1\r\n", "{}"),
            ok(),
        ]);
        let slow_default = RetryPolicy {
            rate_limit_wait: Duration::from_secs(60),
            ..fast_retries()
        };
        let provider = Anthropic::with_endpoint("k".into(), url, slow_default).expect("client");
        let started = std::time::Instant::now();
        assert!(provider
            .complete(&request(), &AtomicBool::new(false))
            .is_ok());
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_secs(1) && waited < Duration::from_secs(30),
            "the header's 1 s, not the 60 s default: {waited:?}"
        );
        assert_eq!(*count.lock().expect("lock"), 2);
    }

    #[test]
    fn the_timeout_grows_with_the_upload() {
        let answer = Duration::from_secs(120);
        assert_eq!(timeout_for(answer, 0), Duration::from_secs(120));
        assert_eq!(timeout_for(answer, 3_000_000), Duration::from_secs(180));
    }

    #[test]
    fn another_400_fails_the_file_with_the_apis_message() {
        let body = r#"{"error":{"message":"image too large"}}"#;
        let (result, n) = complete(vec![http("400 Bad Request", "", body)]);
        assert_eq!(
            result,
            Err(AiError::Rejected("image too large".to_string()))
        );
        assert!(AiError::Rejected(String::new()).stops_job().is_none());
        assert_eq!(n, 1);
    }

    #[test]
    fn a_stop_before_the_answer_keeps_the_usage() {
        let body = r#"{"content":[{"type":"text","text":"{\"summ"}],"stop_reason":"max_tokens","usage":{"input_tokens":5,"output_tokens":4000}}"#;
        let (result, _) = complete(vec![http("200 OK", "", body)]);
        let response = result.expect("response");
        assert_eq!(response.stop_reason, "max_tokens");
        assert!(response.json.is_null());
        assert_eq!(response.usage.output_tokens, 4000);
    }
}
