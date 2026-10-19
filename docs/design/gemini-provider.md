# Gemini (Google) as a third provider

Needed by `Zelenov/frename#166` (more AI providers in Describe with AI). frename's research is in
`docs/research/video-understanding.md` there; this note is only what clipscribe does and why.

## What was verified live, and what was not

A real Gemini API key was available in this session, so the request shape, the answer, the usage
fields, the image cost and the models list below are measured, not read from documentation.
`ai.google.dev` is blocked by the egress proxy, so the **prices** come from the Vertex AI pricing
page read for frename's research (Gemini 3.1 Flash-Lite $0.25 / $1.50, Gemini 3.5 Flash $1.50 /
$9.00 per million tokens, input / output, thinking billed as output); AI Studio is assumed to charge
the same. `PRICES_CHECKED` says so. The owner should confirm them on <https://ai.google.dev/pricing>.

## API shape

`POST {base}/v1beta/models/{model}:generateContent`, header `x-goog-api-key`. Not the Interactions
API (the newer one): `generateContent` is stable, simple and what the measurements used.

- Frames: `parts: [{"text"}, {"inline_data": {"mime_type": "image/jpeg", "data": base64}}]`, the
  same JPEG bytes every provider gets. Frames, not Gemini's native video input: the whole pipeline
  (key frames, shots, groups, moments at frame times) is built on frames, and a 512 px frame gives
  the same answer for a fraction of the upload.
- Answer shape: `generationConfig.responseMimeType: "application/json"` with `responseJsonSchema`
  (full JSON Schema, so the schemas of `describe` go in unchanged; checked live with `additionalProperties:false`).
- Output cap: `maxOutputTokens`. Usage: `usageMetadata.promptTokenCount` is the input;
  `candidatesTokenCount + thoughtsTokenCount` is the output (thinking is billed as output).
- `finishReason`: `STOP` → `end_turn`, `MAX_TOKENS` → `max_tokens`, the safety reasons →
  `refusal`; a blocked prompt has no candidate, only `promptFeedback.blockReason` → `refusal`.
- `Model::effort` is `thinkingConfig.thinkingLevel`; Flash-Lite thinks little by itself, 3.5 Flash
  is sent `low`.

## Frame cost

Measured with one 512×288 frame: **1100 tokens** by default, **about 280** with
`mediaResolution: MEDIA_RESOLUTION_LOW` (3 frames and a sentence: 798 tokens). A 512 px frame has
nothing more to give, so every request sends LOW: a 16-frame clip is 4754 input tokens (frames
16 × 280 + 274 of instructions). `image_tokens_bound` says 280 for Gemini, and `estimate_usage`
now uses it for every provider instead of Anthropic's tiles (the estimate for that clip: 5080,
real 4754, so still an upper bound).

## Models

`gemini-3.1-flash-lite` (the cheap default, like Haiku) and `gemini-3.5-flash` (stronger). Both
are in `GET /v1beta/models` for the key used. Per clip of 30 s (16 frames): about $0.0013 and
$0.0076 (measured).

## Errors

- A wrong key is **HTTP 400** with `error.details[].reason == "API_KEY_INVALID"` (or the message
  "API key not valid"), not 401: `KeyRejected("Gemini")`. 401/403 are rejected too.
- 429 whose message says prepayment credits are depleted (or names billing) → `OutOfCredit`;
  any other 429 is a rate limit and waits (`retry-after`, else the policy's wait).
- 500/502/503/504 retry (a real 503 "high demand" was seen during the measurements).

## Breaking

`Provider` gains `Gemini`: a `match` without `_` breaks, so 0.17.0 and a note in `version.md`
(frename's `ApiKey::of_provider`).
