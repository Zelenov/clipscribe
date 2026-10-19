//! Google Gemini through the Gemini API's `generateContent`, over plain HTTP: the frames go as
//! JPEG images, the way every other provider gets them. See `docs/design/gemini-provider.md`.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::provider::{
    self, timeout_for, AiContent, AiError, AiProvider, AiRequest, AiResponse, AiUsage, Attempt,
    Provider, RetryPolicy, RetryReason, CONNECT_TIMEOUT,
};

const API_URL: &str = "https://generativelanguage.googleapis.com";

/// The Gemini provider, for one key.
pub struct Gemini {
    key: String,
    base_url: String,
    retry: RetryPolicy,
    client: reqwest::blocking::Client,
    /// Shared with the other clients of a folder run; see [`provider::RateGate`].
    gate: Option<std::sync::Arc<provider::RateGate>>,
}

impl Gemini {
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

    /// This client, pausing together with every other client sharing `gate` (on its own with
    /// `None`).
    pub(crate) fn with_rate_gate(
        mut self,
        gate: Option<std::sync::Arc<provider::RateGate>>,
    ) -> Self {
        self.gate = gate;
        self
    }

    /// The request body. `effort`, when a model has one, is Gemini's `thinkingLevel`.
    fn body(request: &AiRequest) -> Value {
        let parts: Vec<Value> = request
            .content
            .iter()
            .map(|block| match block {
                AiContent::Text(text) => json!({"text": text}),
                AiContent::Jpeg(bytes) => json!({
                    "inline_data": {
                        "mime_type": "image/jpeg",
                        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                    },
                }),
            })
            .collect();
        let mut config = json!({
            "maxOutputTokens": request.max_tokens,
            "responseMimeType": "application/json",
            "responseJsonSchema": request.schema,
            // 280 tokens a frame instead of 1100: a 512-pixel frame has nothing more to give.
            "mediaResolution": "MEDIA_RESOLUTION_LOW",
        });
        if let Some(effort) = request.effort {
            config["thinkingConfig"] = json!({"thinkingLevel": effort});
        }
        json!({
            "contents": [{"role": "user", "parts": parts}],
            "generationConfig": config,
        })
    }
}

impl AiProvider for Gemini {
    fn complete(&self, request: &AiRequest, cancel: &AtomicBool) -> Result<AiResponse, AiError> {
        self.complete_notifying(request, cancel, &mut |_, _| {})
    }

    fn complete_notifying(
        &self,
        request: &AiRequest,
        cancel: &AtomicBool,
        on_retry: &mut dyn FnMut(Duration, RetryReason),
    ) -> Result<AiResponse, AiError> {
        let body = Self::body(request).to_string();
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, request.model
        );
        provider::retry_loop(
            &self.retry,
            Provider::Gemini.label(),
            self.gate.as_deref(),
            cancel,
            on_retry,
            || self.attempt(&url, &body),
        )
    }
}

impl Gemini {
    fn attempt(&self, url: &str, body: &str) -> Attempt {
        let sent = self
            .client
            .post(url)
            .header("x-goog-api-key", &self.key)
            .header("content-type", "application/json")
            .timeout(timeout_for(self.retry.answer_timeout, body.len()))
            .body(body.to_string())
            .send();
        let response = match sent {
            Ok(response) => response,
            // Nothing reached Google yet: safe to try again.
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

/// Whether Google's error says the key is wrong. A bad key is a 400 (`API_KEY_INVALID`), not a
/// 401, so the status alone does not tell.
fn key_is_bad(json: &Value) -> bool {
    json["error"]["details"]
        .as_array()
        .is_some_and(|details| details.iter().any(|d| d["reason"] == "API_KEY_INVALID"))
        || json["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("API key not valid") || m.contains("API key expired"))
}

/// Whether a 429 says the prepaid credit or the billing account is used up, not that requests
/// came too fast.
fn credit_is_gone(message: &str) -> bool {
    let message = message.to_lowercase();
    message.contains("prepayment")
        || message.contains("credits are depleted")
        || message.contains("billing")
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
            Provider::Gemini.label().to_string(),
        ))),
        400 if key_is_bad(&json) => Attempt::Done(Err(AiError::KeyRejected(
            Provider::Gemini.label().to_string(),
        ))),
        429 if credit_is_gone(&message) => Attempt::Done(Err(AiError::OutOfCredit(
            Provider::Gemini.label().to_string(),
        ))),
        429 => Attempt::RateLimited(retry_after.unwrap_or(rate_limit_wait)),
        500 | 502 | 503 | 504 => Attempt::Retry(format!("HTTP {status}: {message}")),
        _ => Attempt::Done(Err(AiError::Rejected(message))),
    }
}

/// Read a `generateContent` answer: the JSON in its text, why it stopped, and its usage. Thinking
/// tokens are billed as output.
fn parse_message(json: &Value) -> Result<AiResponse, AiError> {
    let usage = AiUsage {
        input_tokens: json["usageMetadata"]["promptTokenCount"]
            .as_u64()
            .unwrap_or(0),
        output_tokens: json["usageMetadata"]["candidatesTokenCount"]
            .as_u64()
            .unwrap_or(0)
            + json["usageMetadata"]["thoughtsTokenCount"]
                .as_u64()
                .unwrap_or(0),
    };
    let candidate = &json["candidates"][0];
    // A prompt Google blocked has no candidate at all, only `promptFeedback.blockReason`.
    if candidate.is_null() {
        let blocked = json["promptFeedback"]["blockReason"].is_string();
        return if blocked {
            Ok(AiResponse {
                json: Value::Null,
                stop_reason: "refusal".to_string(),
                usage,
            })
        } else {
            Err(AiError::BadAnswer("no candidate in the answer".to_string()))
        };
    }
    let stop_reason = match candidate["finishReason"].as_str().unwrap_or_default() {
        "STOP" | "" => "end_turn",
        "MAX_TOKENS" => "max_tokens",
        "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "RECITATION" | "IMAGE_SAFETY" => {
            "refusal"
        }
        other => other,
    }
    .to_string();
    let text: String = candidate["content"]["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                // A thinking summary is marked `thought`; only the answer is JSON.
                .filter(|p| p["thought"] != true)
                .filter_map(|p| p["text"].as_str())
                .collect()
        })
        .unwrap_or_default();
    // A model that stopped early (the output cap) may have written no valid JSON; the caller
    // fails the file with the stop reason and still counts the usage.
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
            r#"{"candidates":[{"content":{"parts":[{"text":"{\"summary\":\"S\",\"segments\":[]}"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"thoughtsTokenCount":7}}"#,
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
            model: "gemini-3.1-flash-lite".to_string(),
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
        let provider = Gemini::with_endpoint("k".into(), url, fast_retries()).expect("client");
        let result = provider.complete(&request(), &AtomicBool::new(false));
        let n = *count.lock().expect("lock");
        (result, n)
    }

    #[test]
    fn the_body_carries_images_the_schema_and_low_resolution() {
        let body = Gemini::body(&request());
        assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
        assert_eq!(
            body["contents"][0]["parts"][1]["inline_data"]["data"],
            "AQI="
        );
        assert_eq!(
            body["contents"][0]["parts"][1]["inline_data"]["mime_type"],
            "image/jpeg"
        );
        let config = &body["generationConfig"];
        assert_eq!(config["responseMimeType"], "application/json");
        assert_eq!(config["responseJsonSchema"]["type"], "object");
        assert_eq!(config["maxOutputTokens"], 10);
        assert_eq!(config["mediaResolution"], "MEDIA_RESOLUTION_LOW");
        assert!(
            config.get("thinkingConfig").is_none(),
            "no effort, none sent"
        );
    }

    #[test]
    fn an_effort_is_the_thinking_level() {
        let request = AiRequest {
            effort: Some("low"),
            ..request()
        };
        let body = Gemini::body(&request);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );
    }

    #[test]
    fn an_answer_gives_its_json_and_usage_with_thinking_billed_as_output() {
        let (result, n) = complete(vec![ok()]);
        let response = result.expect("answer");
        assert_eq!(response.json["summary"], "S");
        assert_eq!(response.stop_reason, "end_turn");
        assert_eq!(response.usage.input_tokens, 100);
        assert_eq!(response.usage.output_tokens, 27);
        assert_eq!(n, 1);
    }

    #[test]
    fn a_thinking_summary_is_not_part_of_the_answer() {
        let body = r#"{"candidates":[{"content":{"parts":[{"text":"hmm","thought":true},{"text":"{\"summary\":\"S\"}"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1}}"#;
        let (result, _) = complete(vec![http("200 OK", "", body)]);
        assert_eq!(result.expect("answer").json["summary"], "S");
    }

    #[test]
    fn a_blocked_prompt_or_answer_is_a_refusal() {
        let blocked = r#"{"promptFeedback":{"blockReason":"SAFETY"},"usageMetadata":{"promptTokenCount":50}}"#;
        let (result, _) = complete(vec![http("200 OK", "", blocked)]);
        let response = result.expect("still a response, not a transport error");
        assert_eq!(response.stop_reason, "refusal");
        assert!(response.json.is_null());
        let stopped =
            r#"{"candidates":[{"finishReason":"SAFETY"}],"usageMetadata":{"promptTokenCount":50}}"#;
        let (result, _) = complete(vec![http("200 OK", "", stopped)]);
        assert_eq!(result.expect("response").stop_reason, "refusal");
    }

    #[test]
    fn an_answer_without_a_candidate_cannot_be_read() {
        let (result, _) = complete(vec![http("200 OK", "", "{}")]);
        assert!(matches!(result, Err(AiError::BadAnswer(_))));
    }

    #[test]
    fn a_max_tokens_finish_reason_keeps_the_usage() {
        let body = r#"{"candidates":[{"content":{"parts":[{"text":"{\"summ"}]},"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":4000}}"#;
        let (result, _) = complete(vec![http("200 OK", "", body)]);
        let response = result.expect("response");
        assert_eq!(response.stop_reason, "max_tokens");
        assert!(response.json.is_null());
        assert_eq!(response.usage.output_tokens, 4000);
    }

    /// Google answers a wrong key with a 400, not a 401.
    #[test]
    fn a_bad_key_is_rejected_whichever_status_says_so() {
        let invalid = r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"reason":"API_KEY_INVALID"}]}}"#;
        let (result, n) = complete(vec![http("400 Bad Request", "", invalid)]);
        assert_eq!(result, Err(AiError::KeyRejected("Gemini".to_string())));
        assert_eq!(n, 1);
        let (result, _) = complete(vec![http("403 Forbidden", "", "{}")]);
        assert_eq!(result, Err(AiError::KeyRejected("Gemini".to_string())));
        assert!(AiError::KeyRejected("Gemini".to_string())
            .stops_job()
            .is_some());
    }

    #[test]
    fn depleted_prepaid_credit_stops_the_job_as_out_of_credit() {
        let body = r#"{"error":{"code":429,"message":"Your prepayment credits are depleted. Please go to AI Studio at https://ai.studio/projects to manage your project and billing.","status":"RESOURCE_EXHAUSTED"}}"#;
        let (result, _) = complete(vec![http("429 Too Many Requests", "", body)]);
        assert_eq!(result, Err(AiError::OutOfCredit("Gemini".to_string())));
    }

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
    fn a_server_error_is_retried_three_times_then_fails() {
        let overloaded = || {
            http(
                "503 Service Unavailable",
                "",
                r#"{"error":{"message":"This model is currently experiencing high demand."}}"#,
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
    fn a_retry_is_reported_with_its_wait_and_reason() {
        let unavailable = http("503 Service Unavailable", "", "{}");
        let (url, _) = server(vec![unavailable, ok()]);
        let provider = Gemini::with_endpoint("k".into(), url, fast_retries()).expect("client");
        let mut waits = Vec::new();
        let result = provider.complete_notifying(
            &request(),
            &AtomicBool::new(false),
            &mut |after, reason| waits.push((after, reason)),
        );
        assert!(result.is_ok());
        assert_eq!(waits.len(), 1);
        assert_eq!(waits[0].1, RetryReason::Temporary);
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
        let provider = Gemini::with_endpoint("k".into(), url, fast_retries()).expect("client");
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

    /// A real request, only when `CLIPSCRIBE_LIVE_GEMINI_KEY` is set (never in CI without the
    /// secret): a generated frame, one sentence back, about a thousandth of a cent.
    #[test]
    fn live_request_to_gemini() {
        let Ok(key) = std::env::var("CLIPSCRIBE_LIVE_GEMINI_KEY") else {
            eprintln!("CLIPSCRIBE_LIVE_GEMINI_KEY not set: live test skipped");
            return;
        };
        let request = AiRequest {
            model: "gemini-3.1-flash-lite".to_string(),
            content: vec![AiContent::Text(
                "Answer with the summary field: say hi.".to_string(),
            )],
            schema: json!({
                "type": "object",
                "properties": {"summary": {"type": "string"}},
                "required": ["summary"],
                "additionalProperties": false,
            }),
            max_tokens: 200,
            effort: None,
        };
        let provider = Gemini::new(key).expect("client");
        match provider.complete(&request, &AtomicBool::new(false)) {
            Ok(response) => {
                assert_eq!(response.stop_reason, "end_turn");
                assert!(response.json["summary"].is_string());
                assert!(response.usage.input_tokens > 0);
            }
            // The service is busy at times: not this code's failure.
            Err(AiError::Network(e)) => eprintln!("live test skipped: {e}"),
            Err(e) => panic!("{}", e.reason()),
        }
    }
}
