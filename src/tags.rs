//! Suggesting tags for a clip from a closed vocabulary: the vocabulary format, the two request
//! shapes (frames plus a description, or an existing description alone; see the design notes in
//! `docs/design/tag-suggestions.md`), and reading the answer back.
//!
//! No GStreamer or image dependency: like `describe.rs`, this module builds requests from
//! already-decoded [`Frame`]s or an already-computed [`Description`], so it compiles without the
//! `frames` feature too.

use serde_json::{json, Value};

use crate::describe::{self, format_time, Description, Frame, Model};
use crate::provider::{AiContent, AiRequest, AiResponse, AiUsage};
use crate::Cue;

/// Characters of tag/hint or description text per token, for the estimate — the same rule
/// `describe::estimate_usage` uses for subtitle text.
const CHARS_PER_TOKEN: f64 = 3.5;
/// Instruction tokens for the tags-only (no-frames) request, for the estimate.
const INSTRUCTION_TOKENS: u64 = 500;

/// One entry of a closed tag vocabulary: a name the model must copy exactly, and an optional
/// hint of what it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    pub hint: Option<String>,
}

/// Read a vocabulary file: one tag per line, `name — hint` or `name - hint` (the hint is
/// optional), blank lines and lines starting with `#` skipped.
pub fn parse_vocabulary(text: &str) -> Vec<Tag> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (name, hint) = split_tag_line(line);
            let name = name.trim();
            (!name.is_empty()).then(|| Tag {
                name: name.to_string(),
                hint: hint
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Split `line` on the first `" — "` (an em dash) or `" - "` it finds; the whole line is the
/// name, with no hint, if it has neither.
fn split_tag_line(line: &str) -> (&str, Option<&str>) {
    for sep in [" \u{2014} ", " - "] {
        if let Some(index) = line.find(sep) {
            return (&line[..index], Some(&line[index + sep.len()..]));
        }
    }
    (line, None)
}

/// A stretch of the clip a suggested tag applies to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TagRange {
    pub start_s: f64,
    pub end_s: f64,
}

/// One suggested tag from the vocabulary: how sure the model is, and where it applies —
/// `ranges` empty means the whole clip.
#[derive(Debug, Clone, PartialEq)]
pub struct TagSuggestion {
    pub name: String,
    pub confidence: f64,
    pub ranges: Vec<TagRange>,
}

/// What suggesting tags for a clip answers: suggestions from the vocabulary, and any tag ideas
/// the model noticed that were not in it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TagSuggestions {
    pub tags: Vec<TagSuggestion>,
    pub new_tag_ideas: Vec<String>,
}

/// The JSON schema of a suggestion in the `tags` array, shared by both request shapes.
fn tag_suggestion_property() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "confidence": {"type": "number"},
                "ranges": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "start_s": {"type": "number"},
                            "end_s": {"type": "number"}
                        },
                        "required": ["start_s", "end_s"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["name", "confidence", "ranges"],
            "additionalProperties": false
        }
    })
}

fn new_tag_ideas_property() -> Value {
    json!({"type": "array", "items": {"type": "string"}})
}

/// The JSON schema of an answer that describes the clip and suggests tags in one response
/// (`describe_with_tags`): `describe::schema`'s `summary`/`segments`, plus `tags` and
/// `new_tag_ideas`.
pub fn combined_schema() -> Value {
    let mut schema = describe::schema();
    schema["properties"]["tags"] = tag_suggestion_property();
    schema["properties"]["new_tag_ideas"] = new_tag_ideas_property();
    if let Some(required) = schema["required"].as_array_mut() {
        required.push(json!("tags"));
        required.push(json!("new_tag_ideas"));
    }
    schema
}

/// The JSON schema of a tags-only answer (`suggest_tags`, no description to write).
pub fn tags_only_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "tags": tag_suggestion_property(),
            "new_tag_ideas": new_tag_ideas_property()
        },
        "required": ["tags", "new_tag_ideas"],
        "additionalProperties": false
    })
}

/// The vocabulary section every tag request's instructions end with.
fn tag_instructions(vocabulary: &[Tag]) -> String {
    let mut instructions = String::from(
        "\nFrom this list of tags, suggest every one that applies, each with a confidence from \
         0 to 1 and, only when it applies to part of the clip rather than the whole clip, one or \
         more time ranges (start_s, end_s) where it applies. Use the tag names exactly as given; \
         never suggest a name that is not in this list.\n\nTags:\n",
    );
    for tag in vocabulary {
        match &tag.hint {
            Some(hint) => instructions.push_str(&format!("{} \u{2014} {hint}\n", tag.name)),
            None => instructions.push_str(&format!("{}\n", tag.name)),
        }
    }
    instructions.push_str(
        "\nIf you notice something worth tagging that is not in this list, add a short name for \
         it to new_tag_ideas instead — never as a tag suggestion.\n",
    );
    instructions
}

/// The request to `model` describing a clip and suggesting tags from `vocabulary` in one answer:
/// [`describe::build_request`]'s request, with the vocabulary appended to its instructions and
/// [`combined_schema`] in place of `describe::schema`.
pub fn build_combined_request(
    model: Model,
    frames: &[Frame],
    subtitles: &[Cue],
    vocabulary: &[Tag],
    duration_s: f64,
    language: describe::SummaryLanguage,
    moments: describe::MomentsMode,
) -> AiRequest {
    let mut request =
        describe::build_request(model, frames, subtitles, duration_s, language, moments);
    let Some(AiContent::Text(instructions)) = request.content.first_mut() else {
        unreachable!(
            "describe::build_request always starts its content with the instructions text"
        );
    };
    instructions.push_str(&tag_instructions(vocabulary));
    request.schema = combined_schema();
    request
}

/// The request to `model` suggesting tags from `vocabulary` for a clip already described by
/// `description` (`duration_s` long), without reading its frames again.
pub fn build_tags_only_request(
    model: Model,
    description: &Description,
    subtitles: &[Cue],
    vocabulary: &[Tag],
    duration_s: f64,
) -> AiRequest {
    let mut instructions = format!(
        "A video editor already has this description of a video clip, {} long, without having \
         watched it.\nSummary: {}\n",
        format_time(duration_s),
        description.summary,
    );
    if !description.segments.is_empty() {
        instructions.push_str("Segments:\n");
        for segment in &description.segments {
            instructions.push_str(&format!(
                "[{}\u{2013}{}] {}\n",
                format_time(segment.start_s),
                format_time(segment.end_s),
                segment.description
            ));
        }
    }
    if !subtitles.is_empty() {
        instructions.push_str("\nSubtitles:\n");
        for cue in subtitles {
            instructions.push_str(&format!(
                "[{}\u{2013}{}] {}\n",
                format_time(cue.start.as_secs_f64()),
                format_time(cue.end.as_secs_f64()),
                cue.text.split_whitespace().collect::<Vec<_>>().join(" ")
            ));
        }
    }
    instructions.push_str(&tag_instructions(vocabulary));
    AiRequest {
        model: model.id.to_string(),
        content: vec![AiContent::Text(instructions)],
        schema: tags_only_schema(),
        max_tokens: model.max_answer_tokens,
        effort: model.effort,
    }
}

/// Read `response.json`'s `tags` and `new_tag_ideas` into a [`TagSuggestions`]: an unknown tag
/// name (not in `vocabulary`, compared exactly after trimming), a duplicate of one already kept,
/// or a suggestion whose `confidence` is missing or not a number is dropped; a range outside the
/// clip is dropped from its tag, and a tag whose ranges were *all* dropped (rather than never
/// given any) is dropped entirely, since an empty list otherwise means "the whole clip" — not
/// silently claiming that instead of what the model actually said.
fn parse_tag_suggestions(json: &Value, vocabulary: &[Tag], duration_s: f64) -> Vec<TagSuggestion> {
    let end = duration_s + 1.0;
    let mut seen = std::collections::HashSet::new();
    json.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = item["name"].as_str()?.trim();
                    let known = vocabulary.iter().any(|tag| tag.name == name);
                    if !known || !seen.insert(name.to_string()) {
                        return None;
                    }
                    let confidence = item["confidence"].as_f64()?.clamp(0.0, 1.0);
                    let raw_ranges = item["ranges"].as_array().cloned().unwrap_or_default();
                    let ranges: Vec<TagRange> = raw_ranges
                        .iter()
                        .filter_map(|range| {
                            let start_s = range["start_s"].as_f64()?;
                            let end_s = range["end_s"].as_f64()?.min(duration_s);
                            let valid = start_s >= 0.0 && end_s > start_s && start_s < end;
                            valid.then_some(TagRange { start_s, end_s })
                        })
                        .collect();
                    if !raw_ranges.is_empty() && ranges.is_empty() {
                        return None;
                    }
                    let mut ranges = ranges;
                    ranges.sort_by(|a, b| a.start_s.total_cmp(&b.start_s));
                    Some(TagSuggestion {
                        name: name.to_string(),
                        confidence,
                        ranges,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_new_tag_ideas(json: &Value) -> Vec<String> {
    json.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Read a combined description-and-tags answer: the description exactly as
/// [`describe::parse_answer`] reads it (it only looks at `summary`/`segments`, so the extra
/// `tags`/`new_tag_ideas` fields do not affect it), plus the tag suggestions.
pub fn parse_combined_answer(
    response: &AiResponse,
    duration_s: f64,
    vocabulary: &[Tag],
    moments: describe::MomentsMode,
) -> Result<(Description, TagSuggestions), String> {
    let description = describe::parse_answer(response, duration_s, moments)?;
    let tags = parse_tag_suggestions(&response.json["tags"], vocabulary, duration_s);
    let new_tag_ideas = parse_new_tag_ideas(&response.json["new_tag_ideas"]);
    Ok((
        description,
        TagSuggestions {
            tags,
            new_tag_ideas,
        },
    ))
}

/// Read a tags-only answer (no description to validate).
pub fn parse_tags_only_answer(
    response: &AiResponse,
    vocabulary: &[Tag],
    duration_s: f64,
) -> Result<TagSuggestions, String> {
    match response.stop_reason.as_str() {
        "end_turn" => {}
        "max_tokens" => return Err("The answer was too long".to_string()),
        "refusal" => return Err("The model declined to suggest tags".to_string()),
        other => return Err(format!("The model stopped early ({other})")),
    }
    let tags = parse_tag_suggestions(&response.json["tags"], vocabulary, duration_s);
    let new_tag_ideas = parse_new_tag_ideas(&response.json["new_tag_ideas"]);
    Ok(TagSuggestions {
        tags,
        new_tag_ideas,
    })
}

/// Estimated tokens of tagging a clip `duration_s` long from `vocabulary`, before its frames or
/// answer are known: `description` is `None` for the combined (frames) request — priced like
/// [`describe::estimate_usage`] — or the clip's already-computed description for the tags-only
/// request, whose input is that description's own text instead of frames. Output is priced with
/// a per-tag allowance on top of the model's normal answer size: a rougher estimate than
/// `estimate_usage`'s, since how many tags plausibly apply is not predicted by the vocabulary's
/// byte count.
pub fn estimate_tags_usage(
    model: Model,
    duration_s: f64,
    subtitle_bytes: usize,
    vocabulary: &[Tag],
    description: Option<&Description>,
) -> AiUsage {
    let vocabulary_bytes: usize = vocabulary
        .iter()
        .map(|tag| tag.name.len() + tag.hint.as_deref().map_or(0, str::len) + 4)
        .sum();
    let mut usage = match description {
        None => describe::estimate_usage(model, duration_s, subtitle_bytes),
        Some(description) => {
            let description_bytes = description.summary.len()
                + description
                    .segments
                    .iter()
                    .map(|s| s.description.len() + 20)
                    .sum::<usize>();
            AiUsage {
                input_tokens: ((subtitle_bytes + description_bytes) as f64 / CHARS_PER_TOKEN)
                    as u64
                    + INSTRUCTION_TOKENS,
                output_tokens: 0,
            }
        }
    };
    usage.input_tokens += (vocabulary_bytes as f64 / CHARS_PER_TOKEN) as u64;
    // A confidence and maybe a short range per plausible tag, plus room for a few new tag ideas.
    usage.output_tokens += vocabulary.len() as u64 * 15 + 100;
    usage
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::describe::Segment;

    fn goat() -> Tag {
        Tag {
            name: "Goat".to_string(),
            hint: Some("any goat or sheep on screen".to_string()),
        }
    }

    fn vocabulary() -> Vec<Tag> {
        vec![
            goat(),
            Tag {
                name: "Indoor".to_string(),
                hint: None,
            },
        ]
    }

    fn response(json: Value, stop_reason: &str) -> AiResponse {
        AiResponse {
            json,
            stop_reason: stop_reason.to_string(),
            usage: AiUsage::default(),
        }
    }

    #[test]
    fn a_vocabulary_file_is_parsed_with_or_without_a_hint() {
        let tags = parse_vocabulary(
            "Goat — any goat or sheep on screen\n\
             Indoor\n\
             # a comment\n\
             \n\
             Night - after dark\n",
        );
        assert_eq!(
            tags,
            vec![
                goat(),
                Tag {
                    name: "Indoor".to_string(),
                    hint: None
                },
                Tag {
                    name: "Night".to_string(),
                    hint: Some("after dark".to_string())
                },
            ]
        );
        assert!(parse_vocabulary("\n# only comments\n").is_empty());
    }

    #[test]
    fn schemas_forbid_extra_properties_and_require_the_tag_fields() {
        for schema in [combined_schema(), tags_only_schema()] {
            assert_eq!(schema["additionalProperties"], false);
            assert_eq!(
                schema["properties"]["tags"]["items"]["additionalProperties"],
                false
            );
            let required = schema["required"].as_array().expect("required");
            assert!(required.iter().any(|v| v == "tags"));
            assert!(required.iter().any(|v| v == "new_tag_ideas"));
        }
        assert_eq!(combined_schema()["properties"]["summary"]["type"], "string");
        assert!(tags_only_schema()["properties"].get("summary").is_none());
    }

    #[test]
    fn the_combined_request_carries_the_vocabulary_and_frames() {
        let frames = vec![Frame {
            time_s: 0.0,
            jpeg: vec![1],
        }];
        let request = build_combined_request(
            describe::MODELS[0],
            &frames,
            &[],
            &vocabulary(),
            10.0,
            describe::SummaryLanguage::English,
            describe::MomentsMode::Important,
        );
        let AiContent::Text(instructions) = &request.content[0] else {
            panic!("instructions first");
        };
        assert!(instructions.contains("Goat \u{2014} any goat or sheep on screen"));
        assert!(instructions.contains("Indoor"));
        assert!(instructions.contains("new_tag_ideas"));
        assert_eq!(request.content[2], AiContent::Jpeg(vec![1]));
        assert_eq!(request.schema, combined_schema());
    }

    #[test]
    fn the_tags_only_request_carries_the_description_and_no_images() {
        let description = Description {
            summary: "A walk in the park.".to_string(),
            segments: vec![Segment {
                start_s: 0.0,
                end_s: 5.0,
                description: "A dog runs past.".to_string(),
            }],
        };
        let request =
            build_tags_only_request(describe::MODELS[0], &description, &[], &vocabulary(), 10.0);
        assert_eq!(request.content.len(), 1, "no frames");
        let AiContent::Text(instructions) = &request.content[0] else {
            panic!("instructions first");
        };
        assert!(instructions.contains("A walk in the park."));
        assert!(instructions.contains("A dog runs past."));
        assert!(instructions.contains("Goat"));
        assert_eq!(request.schema, tags_only_schema());
    }

    #[test]
    fn unknown_tags_and_out_of_clip_ranges_are_dropped() {
        let answer = json!({
            "summary": "A farm.",
            "segments": [],
            "tags": [
                {"name": "Goat", "confidence": 0.9, "ranges": [{"start_s": 1.0, "end_s": 3.0}]},
                {"name": "Indoor", "confidence": 1.5, "ranges": []},
                {"name": "Not in vocabulary", "confidence": 0.5, "ranges": []},
                {"name": "Goat", "confidence": 0.1, "ranges": []},
                {"name": "Indoor", "confidence": 0.4, "ranges": [{"start_s": 50.0, "end_s": 60.0}]}
            ],
            "new_tag_ideas": ["Tractor", "  ", "Tractor"]
        });
        let (description, tags) = parse_combined_answer(
            &response(answer, "end_turn"),
            10.0,
            &vocabulary(),
            describe::MomentsMode::Important,
        )
        .expect("parsed");
        assert_eq!(description.summary, "A farm.");
        assert_eq!(tags.tags.len(), 2, "{:?}", tags.tags);
        let goat = tags.tags.iter().find(|t| t.name == "Goat").expect("goat");
        assert_eq!(goat.confidence, 0.9, "the first Goat, not the duplicate");
        assert_eq!(
            goat.ranges,
            vec![TagRange {
                start_s: 1.0,
                end_s: 3.0
            }]
        );
        let indoor = tags
            .tags
            .iter()
            .find(|t| t.name == "Indoor")
            .expect("indoor");
        assert_eq!(indoor.confidence, 1.0, "clamped");
        assert!(
            indoor.ranges.is_empty(),
            "the only range given was out of the clip: dropped, not treated as whole-clip"
        );
        assert_eq!(
            tags.new_tag_ideas,
            vec!["Tractor", "Tractor"],
            "blanks trimmed, not deduped"
        );
    }

    #[test]
    fn a_tag_with_no_ranges_at_all_means_the_whole_clip() {
        let answer = json!({
            "tags": [{"name": "Indoor", "confidence": 0.8, "ranges": []}],
            "new_tag_ideas": []
        });
        let tags = parse_tags_only_answer(&response(answer, "end_turn"), &vocabulary(), 10.0)
            .expect("parsed");
        assert_eq!(tags.tags.len(), 1);
        assert!(tags.tags[0].ranges.is_empty());
    }

    #[test]
    fn a_missing_confidence_drops_the_suggestion() {
        let answer = json!({"tags": [{"name": "Goat", "ranges": []}], "new_tag_ideas": []});
        let tags = parse_tags_only_answer(&response(answer, "end_turn"), &vocabulary(), 10.0)
            .expect("parsed");
        assert!(tags.tags.is_empty());
    }

    #[test]
    fn an_early_stop_fails_the_tags_only_answer() {
        let fine = json!({"tags": [], "new_tag_ideas": []});
        assert_eq!(
            parse_tags_only_answer(&response(fine.clone(), "max_tokens"), &vocabulary(), 10.0),
            Err("The answer was too long".to_string())
        );
        assert!(parse_tags_only_answer(&response(fine, "refusal"), &vocabulary(), 10.0).is_err());
    }

    #[test]
    fn the_estimate_grows_with_the_vocabulary_and_differs_by_input_mode() {
        let empty = estimate_tags_usage(describe::MODELS[0], 60.0, 0, &[], None);
        let with_vocab = estimate_tags_usage(describe::MODELS[0], 60.0, 0, &vocabulary(), None);
        assert!(with_vocab.input_tokens > empty.input_tokens);
        assert!(with_vocab.output_tokens > empty.output_tokens);

        let description = Description {
            summary: "x".repeat(200),
            segments: vec![],
        };
        let from_frames = estimate_tags_usage(describe::MODELS[0], 60.0, 0, &vocabulary(), None);
        let from_description = estimate_tags_usage(
            describe::MODELS[0],
            60.0,
            0,
            &vocabulary(),
            Some(&description),
        );
        assert_ne!(from_frames.input_tokens, from_description.input_tokens);
    }

    /// A tags-only request goes out over real HTTP, through [`crate::anthropic::Anthropic`], and
    /// the answer comes back and validates exactly like the pure-JSON tests above — the request
    /// builder, the wire format and the parser agree with each other, not just each with itself.
    #[test]
    fn a_tags_only_request_round_trips_through_a_mock_server() {
        use crate::anthropic::{Anthropic, RetryPolicy};
        use crate::provider::AiProvider;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::AtomicBool;

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
            let answer = r#"{"content":[{"type":"text","text":"{\"tags\":[{\"name\":\"Goat\",\"confidence\":0.8,\"ranges\":[]}],\"new_tag_ideas\":[\"Tractor\"]}"}],"stop_reason":"end_turn","usage":{"input_tokens":50,"output_tokens":10}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                answer.len(),
                answer
            );
            let _ = reader.get_mut().write_all(response.as_bytes());
        });

        let description = Description {
            summary: "A farm.".to_string(),
            segments: vec![],
        };
        let request =
            build_tags_only_request(describe::MODELS[0], &description, &[], &vocabulary(), 10.0);
        let provider =
            Anthropic::with_endpoint("k".to_string(), url, RetryPolicy::default()).expect("client");
        let response = provider
            .complete(&request, &AtomicBool::new(false))
            .expect("answer");
        let tags = parse_tags_only_answer(&response, &vocabulary(), 10.0).expect("parsed");
        assert_eq!(tags.tags.len(), 1);
        assert_eq!(tags.tags[0].name, "Goat");
        assert_eq!(tags.new_tag_ideas, vec!["Tractor"]);
    }
}
