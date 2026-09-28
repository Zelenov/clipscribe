//! Describing one moment of a clip — a name and a short description for the frame at a given
//! time, for a marker a video editor is placing there — rather than the whole clip
//! ([`crate::describe`]). See [`crate::describe_moment`].

use serde_json::{json, Value};

use crate::describe::{format_time, Frame, Model};
use crate::provider::{AiContent, AiRequest, AiResponse};
use crate::{Cue, SummaryLanguage};

/// How far on each side of the requested time [`crate::describe_moment`] reads a frame from,
/// in seconds: close enough that the moment is still recognisably the same action, far enough to
/// show which way it is moving. Subtitle lines within the same window are sent along.
pub const MOMENT_WINDOW_S: f64 = 1.0;

/// A moment of a clip: a short name and a one-to-two sentence description, for a marker.
#[derive(Debug, Clone, PartialEq)]
pub struct Moment {
    /// A few words, suitable as a marker name.
    pub name: String,
    pub description: String,
}

/// The JSON schema of a moment answer.
pub fn moment_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "description": {"type": "string"}
        },
        "required": ["name", "description"],
        "additionalProperties": false
    })
}

/// The request to `model` naming and describing the moment at `at_s`, from the frames
/// [`crate::frames::Clip::sample_moment`] read `window_s` around it and any subtitle lines that
/// overlap that same window.
pub fn build_moment_request(
    model: Model,
    frames: &[Frame],
    subtitles: &[Cue],
    at_s: f64,
    window_s: f64,
    language: SummaryLanguage,
) -> AiRequest {
    let nearby: Vec<&Cue> = subtitles
        .iter()
        .filter(|cue| {
            cue.start.as_secs_f64() <= at_s + window_s && cue.end.as_secs_f64() >= at_s - window_s
        })
        .collect();
    let has_subtitles = !nearby.is_empty();
    let mut instructions = format!(
        "You look at one moment of a video clip, at t={}, for a video editor placing a marker \
         there. You get frames around that time, each preceded by its time as t=m:ss{}.\n\
         Answer with a short name (a few words, plain text, fit for a marker label) and a \
         one-to-two sentence description of what is happening at that moment.\n\
         Stay factual: describe only what is seen and said. Do not guess who people are.\n{}",
        format_time(at_s),
        if has_subtitles {
            ", and nearby subtitles, which may be inaccurate"
        } else {
            ""
        },
        language.instruction(has_subtitles),
    );
    if has_subtitles {
        instructions.push_str("\n\nSubtitles:\n");
        for cue in nearby {
            instructions.push_str(&format!(
                "[{}\u{2013}{}] {}\n",
                format_time(cue.start.as_secs_f64()),
                format_time(cue.end.as_secs_f64()),
                cue.text.split_whitespace().collect::<Vec<_>>().join(" ")
            ));
        }
    }
    let mut content = vec![AiContent::Text(instructions)];
    for frame in frames {
        content.push(AiContent::Text(format!("t={}", format_time(frame.time_s))));
        content.push(AiContent::Jpeg(frame.jpeg.clone()));
    }
    AiRequest {
        model: model.id.to_string(),
        content,
        schema: moment_schema(),
        max_tokens: model.max_answer_tokens,
        effort: model.effort,
    }
}

/// Read a moment answer: an answer the model did not finish, or with an empty name or
/// description, is an error with the reason for the failed list.
pub fn parse_moment_answer(response: &AiResponse) -> Result<Moment, String> {
    match response.stop_reason.as_str() {
        "end_turn" => {}
        "max_tokens" => return Err("The answer was too long".to_string()),
        "refusal" => return Err("The model declined to describe it".to_string()),
        other => return Err(format!("The model stopped early ({other})")),
    }
    let name = response.json["name"].as_str().unwrap_or_default().trim();
    let description = response.json["description"]
        .as_str()
        .unwrap_or_default()
        .trim();
    if name.is_empty() || description.is_empty() {
        return Err("The answer had no name or no description".to_string());
    }
    Ok(Moment {
        name: name.to_string(),
        description: description.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::AiUsage;
    use std::time::Duration;

    fn frame(time_s: f64) -> Frame {
        Frame {
            time_s,
            jpeg: vec![1, 2, 3],
        }
    }

    fn cue(start_s: f64, end_s: f64, text: &str) -> Cue {
        Cue {
            start: Duration::from_secs_f64(start_s),
            end: Duration::from_secs_f64(end_s),
            text: text.to_string(),
        }
    }

    fn response(json: Value, stop_reason: &str) -> AiResponse {
        AiResponse {
            json,
            stop_reason: stop_reason.to_string(),
            usage: AiUsage::default(),
        }
    }

    #[test]
    fn the_request_labels_frames_and_carries_only_nearby_subtitles() {
        let subtitles = vec![cue(1.0, 2.0, "far away"), cue(9.5, 10.4, "right\nhere")];
        let request = build_moment_request(
            Model::default(),
            &[frame(9.0), frame(10.0), frame(11.0)],
            &subtitles,
            10.0,
            MOMENT_WINDOW_S,
            SummaryLanguage::English,
        );
        let AiContent::Text(instructions) = &request.content[0] else {
            panic!("instructions first");
        };
        assert!(instructions.contains("right here"), "{instructions}");
        assert!(!instructions.contains("far away"), "{instructions}");
        assert!(instructions.contains("t=0:10"), "{instructions}");
        // instructions, then (label, jpeg) per frame
        assert_eq!(request.content.len(), 1 + 3 * 2);
        assert_eq!(request.schema, moment_schema());
    }

    #[test]
    fn no_subtitles_nearby_is_a_silent_clip() {
        let request = build_moment_request(
            Model::default(),
            &[frame(10.0)],
            &[],
            10.0,
            MOMENT_WINDOW_S,
            SummaryLanguage::English,
        );
        let AiContent::Text(instructions) = &request.content[0] else {
            panic!("instructions first");
        };
        assert!(!instructions.contains("Subtitles"), "{instructions}");
    }

    #[test]
    fn a_wider_window_pulls_in_a_cue_the_default_window_excludes() {
        let subtitles = vec![cue(7.5, 8.0, "further away")];
        let narrow = build_moment_request(
            Model::default(),
            &[frame(10.0)],
            &subtitles,
            10.0,
            MOMENT_WINDOW_S,
            SummaryLanguage::English,
        );
        let AiContent::Text(instructions) = &narrow.content[0] else {
            panic!("instructions first");
        };
        assert!(!instructions.contains("further away"), "{instructions}");

        let wide = build_moment_request(
            Model::default(),
            &[frame(10.0)],
            &subtitles,
            10.0,
            3.0,
            SummaryLanguage::English,
        );
        let AiContent::Text(instructions) = &wide.content[0] else {
            panic!("instructions first");
        };
        assert!(instructions.contains("further away"), "{instructions}");
    }

    #[test]
    fn a_full_answer_round_trips() {
        let answer = response(
            json!({"name": "Goat crosses path", "description": "A goat walks in front of the hikers."}),
            "end_turn",
        );
        let moment = parse_moment_answer(&answer).expect("moment");
        assert_eq!(moment.name, "Goat crosses path");
        assert_eq!(moment.description, "A goat walks in front of the hikers.");
    }

    #[test]
    fn an_empty_name_or_description_fails() {
        let empty_name = response(json!({"name": "", "description": "Something."}), "end_turn");
        assert!(parse_moment_answer(&empty_name).is_err());
        let empty_description =
            response(json!({"name": "Something", "description": ""}), "end_turn");
        assert!(parse_moment_answer(&empty_description).is_err());
    }

    #[test]
    fn an_early_stop_fails_with_the_reason() {
        let cut_off = response(Value::Null, "max_tokens");
        assert_eq!(
            parse_moment_answer(&cut_off),
            Err("The answer was too long".to_string())
        );
    }

    /// The real path end to end: a frame read from a test clip with
    /// [`crate::frames::Clip::sample_moment`], a request built from it, sent to a mock server,
    /// and the answer parsed back — not just the request/answer shapes on their own.
    #[cfg(all(feature = "frames", target_os = "linux"))]
    #[test]
    fn a_moment_round_trips_through_a_mock_server_with_a_real_frame() {
        use crate::anthropic::{Anthropic, RetryPolicy};
        use crate::frames::Clip;
        use crate::provider::AiProvider;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::AtomicBool;

        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/clips/file_example_MP4_480_1_5MG.mp4");
        let clip = Clip::open(&path, Duration::from_secs(20)).expect("opens");
        let frames = clip
            .sample_moment(2.0, MOMENT_WINDOW_S, &AtomicBool::new(false))
            .expect("sampled")
            .expect("not cancelled");
        assert!(!frames.is_empty());
        drop(clip);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
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
            let answer = r#"{"content":[{"type":"text","text":"{\"name\":\"Opening frame\",\"description\":\"The clip's opening frame.\"}"}],"stop_reason":"end_turn","usage":{"input_tokens":500,"output_tokens":20}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                answer.len(),
                answer
            );
            let _ = reader.get_mut().write_all(response.as_bytes());
        });

        let request = build_moment_request(
            Model::default(),
            &frames,
            &[],
            2.0,
            MOMENT_WINDOW_S,
            SummaryLanguage::English,
        );
        let provider =
            Anthropic::with_endpoint("k".to_string(), url, RetryPolicy::default()).expect("client");
        let response = provider
            .complete(&request, &AtomicBool::new(false))
            .expect("answer");
        let moment = parse_moment_answer(&response).expect("parsed");
        assert_eq!(moment.name, "Opening frame");
        assert_eq!(moment.description, "The clip's opening frame.");
    }
}
