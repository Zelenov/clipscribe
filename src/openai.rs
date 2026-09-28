//! ChatGPT through OpenAI's Chat Completions API, over plain HTTP — the same shape
//! [`crate::anthropic`] uses, translated to OpenAI's request, response and error bodies. See
//! `docs/design/openai-provider.md` for why this endpoint and these models.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::provider::{
    self, timeout_for, AiContent, AiError, AiProvider, AiRequest, AiResponse, AiUsage, Attempt,
    Provider, RetryPolicy, CONNECT_TIMEOUT,
};

const API_URL: &str = "https://api.openai.com";

/// The OpenAI provider, for one key.
pub struct OpenAi {
    key: String,
    base_url: String,
    retry: RetryPolicy,
    client: reqwest::blocking::Client,
    /// Shared with the other clients of a folder run; see [`provider::RateGate`].
    gate: Option<std::sync::Arc<provider::RateGate>>,
}

impl OpenAi {
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
            gate: None,
        })
    }

    /// This client, pausing together with every other client sharing `gate`.
    #[cfg(feature = "frames")]
    pub(crate) fn with_rate_gate(mut self, gate: std::sync::Arc<provider::RateGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Fails (even in a release build, unlike a `debug_assert!`) if `request.effort` is set: no
    /// model in `MODELS` gives OpenAI one today (neither picked model reasons), so there is no
    /// field name to put it in yet. Guessing at one now would silently drop it; refusing the
    /// request is the safer failure the day a reasoning OpenAI model is added here.
    fn body(request: &AiRequest) -> Result<Value, AiError> {
        if request.effort.is_some() {
            return Err(AiError::Rejected(
                "this OpenAI model has no effort parameter implemented yet".to_string(),
            ));
        }
        let content: Vec<Value> = request
            .content
            .iter()
            .map(|block| match block {
                AiContent::Text(text) => json!({"type": "text", "text": text}),
                AiContent::Jpeg(bytes) => json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!(
                            "data:image/jpeg;base64,{}",
                            base64::engine::general_purpose::STANDARD.encode(bytes)
                        ),
                    },
                }),
            })
            .collect();
        Ok(json!({
            "model": request.model,
            "max_completion_tokens": request.max_tokens,
            "messages": [{"role": "user", "content": content}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": "answer", "strict": true, "schema": request.schema},
            },
        }))
    }
}

impl AiProvider for OpenAi {
    fn complete(&self, request: &AiRequest, cancel: &AtomicBool) -> Result<AiResponse, AiError> {
        let body = Self::body(request)?.to_string();
        provider::retry_loop(
            &self.retry,
            Provider::OpenAi.label(),
            self.gate.as_deref(),
            cancel,
            || self.attempt(&body),
        )
    }
}

impl OpenAi {
    fn attempt(&self, body: &str) -> Attempt {
        let sent = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.key))
            .header("content-type", "application/json")
            .timeout(timeout_for(self.retry.answer_timeout, body.len()))
            .body(body.to_string())
            .send();
        let response = match sent {
            Ok(response) => response,
            // Nothing reached OpenAI yet: safe to try again.
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
    let code = json["error"]["code"].as_str().unwrap_or_default();
    match status {
        200 => Attempt::Done(parse_message(&json)),
        401 | 403 => Attempt::Done(Err(AiError::KeyRejected(
            Provider::OpenAi.label().to_string(),
        ))),
        // OpenAI signals "no credit left" through a 429 with this error code, not a distinct
        // HTTP status the way Anthropic's 402 does.
        429 if code == "insufficient_quota" => Attempt::Done(Err(AiError::OutOfCredit(
            Provider::OpenAi.label().to_string(),
        ))),
        429 => Attempt::RateLimited(retry_after.unwrap_or(rate_limit_wait)),
        500 | 502 | 503 | 504 => Attempt::Retry(format!("HTTP {status}: {message}")),
        _ => Attempt::Done(Err(AiError::Rejected(message))),
    }
}

/// Read a Chat Completions answer: the JSON in its message, why it stopped, and its usage.
fn parse_message(json: &Value) -> Result<AiResponse, AiError> {
    let usage = AiUsage {
        input_tokens: json["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
        output_tokens: json["usage"]["completion_tokens"].as_u64().unwrap_or(0),
    };
    let message = &json["choices"][0]["message"];
    // A structured-output refusal is its own field, not a `finish_reason`: OpenAI still answers
    // with `finish_reason: "stop"` when it declines to fill the schema.
    if let Some(refusal) = message["refusal"].as_str() {
        if !refusal.is_empty() {
            return Ok(AiResponse {
                json: Value::Null,
                stop_reason: "refusal".to_string(),
                usage,
            });
        }
    }
    let finish_reason = json["choices"][0]["finish_reason"]
        .as_str()
        .unwrap_or_default();
    let stop_reason = match finish_reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "content_filter" => "refusal",
        other => other,
    }
    .to_string();
    let text = message["content"].as_str().unwrap_or_default();
    // A model that stopped early (max_completion_tokens) may have written no valid JSON; the
    // caller fails the file with the stop reason and still counts the usage.
    let answer = serde_json::from_str(text).unwrap_or(Value::Null);
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
            r#"{"choices":[{"message":{"content":"{\"summary\":\"S\",\"segments\":[]}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":20}}"#,
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
            model: "gpt-4.1-mini".to_string(),
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
        let provider = OpenAi::with_endpoint("k".into(), url, fast_retries()).expect("client");
        let result = provider.complete(&request(), &AtomicBool::new(false));
        let n = *count.lock().expect("lock");
        (result, n)
    }

    #[test]
    fn the_body_carries_images_and_the_schema_as_a_data_url() {
        let body = OpenAi::body(&request()).expect("no effort set");
        assert_eq!(
            body["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/jpeg;base64,AQI="
        );
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(body["max_completion_tokens"], 10);
    }

    #[test]
    fn a_request_with_an_effort_is_rejected_instead_of_silently_dropping_it() {
        let request = AiRequest {
            effort: Some("low"),
            ..request()
        };
        let error = OpenAi::body(&request).expect_err("no OpenAI model reasons yet");
        assert!(matches!(error, AiError::Rejected(_)));
    }

    #[test]
    fn an_answer_gives_its_json_and_usage() {
        let (result, n) = complete(vec![ok()]);
        let response = result.expect("answer");
        assert_eq!(response.json["summary"], "S");
        assert_eq!(response.stop_reason, "end_turn");
        assert_eq!(response.usage.input_tokens, 100);
        assert_eq!(response.usage.output_tokens, 20);
        assert_eq!(n, 1);
    }

    #[test]
    fn a_structured_output_refusal_is_its_own_field_not_a_finish_reason() {
        let refused = http(
            "200 OK",
            "",
            r#"{"choices":[{"message":{"refusal":"I can't help with that."},"finish_reason":"stop"}],"usage":{"prompt_tokens":50,"completion_tokens":5}}"#,
        );
        let (result, _) = complete(vec![refused]);
        let response = result.expect("still a response, not a transport error");
        assert_eq!(response.stop_reason, "refusal");
        assert!(response.json.is_null());
    }

    #[test]
    fn a_length_finish_reason_becomes_max_tokens() {
        let body = r#"{"choices":[{"message":{"content":"{\"summ"},"finish_reason":"length"}],"usage":{"prompt_tokens":5,"completion_tokens":4000}}"#;
        let (result, _) = complete(vec![http("200 OK", "", body)]);
        let response = result.expect("response");
        assert_eq!(response.stop_reason, "max_tokens");
        assert!(response.json.is_null());
        assert_eq!(response.usage.output_tokens, 4000);
    }

    #[test]
    fn insufficient_quota_stops_the_job_as_out_of_credit() {
        let body = r#"{"error":{"message":"You exceeded your current quota.","code":"insufficient_quota"}}"#;
        let (result, _) = complete(vec![http("429 Too Many Requests", "", body)]);
        assert_eq!(result, Err(AiError::OutOfCredit("OpenAI".to_string())));
    }

    /// A 429 with no quota code (a plain rate limit) waits instead of failing — unlike
    /// `insufficient_quota` just above. Direct against `classify`, the same way
    /// `anthropic::tests::a_429_waits_as_long_as_retry_after_says` checks Anthropic's mapping.
    #[test]
    fn a_plain_429_is_rate_limited_not_out_of_credit() {
        let wait = |retry_after| match classify(429, retry_after, "{}", Duration::from_secs(30)) {
            Attempt::RateLimited(wait) => wait,
            _ => panic!("a plain 429 waits"),
        };
        assert_eq!(wait(Some(Duration::from_secs(5))), Duration::from_secs(5));
        assert_eq!(wait(None), Duration::from_secs(30));
    }

    #[test]
    fn a_rejected_key_stops_the_job() {
        let (result, _) = complete(vec![http("401 Unauthorized", "", "{}")]);
        assert_eq!(result, Err(AiError::KeyRejected("OpenAI".to_string())));
        assert!(AiError::KeyRejected("OpenAI".to_string())
            .stops_job()
            .is_some());
    }

    #[test]
    fn a_server_error_is_retried_three_times_then_fails() {
        let overloaded = || {
            http(
                "503 Service Unavailable",
                "",
                r#"{"error":{"message":"The server is overloaded."}}"#,
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
        let provider = OpenAi::with_endpoint("k".into(), url, fast_retries()).expect("client");
        let result = provider.complete(&request(), &AtomicBool::new(false));
        assert_eq!(result, Err(AiError::Timeout));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(*count.lock().expect("lock"), 1, "sent once");
    }

    #[test]
    fn another_400_fails_the_file_with_the_apis_message() {
        let body = r#"{"error":{"message":"image too large"}}"#;
        let (result, n) = complete(vec![http("400 Bad Request", "", body)]);
        assert_eq!(
            result,
            Err(AiError::Rejected("image too large".to_string()))
        );
        assert_eq!(n, 1);
    }
}
