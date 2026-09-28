# ChatGPT (OpenAI) as a second provider

Issue #4 (moved from `Zelenov/frename#39`). Editors can use the account they already pay for, and
pick the cheaper provider for large folders.

## What could not be verified live

`api.openai.com` and `developers.openai.com` are both blocked by this environment's egress proxy
(`CONNECT tunnel failed, response 403`), exactly as the issue itself already noted for end-to-end
runs. That also means the model names and per-token prices below could not be checked against
OpenAI's current pricing page from this session; they are the last models and prices from training
data I have real confidence in (GPT-4.1's launch pricing), not a live quote. A web search surfaced
several "2026 pricing" pages naming a "GPT-5.6" family with codenamed tiers ("Sol"/"Terra"/"Luna")
that do not match any OpenAI naming convention I can verify and read as low-quality SEO content, not
OpenAI's own documentation — not trusted here. **The owner should confirm current OpenAI model
names and prices before relying on `--estimate` for `--provider openai` budgeting**; this is flagged
in the PR body too.

## API shape

OpenAI's Chat Completions API (`POST /v1/chat/completions`, `Authorization: Bearer <key>`), not the
newer Responses API: Chat Completions is the longer-stable, more widely documented surface, and its
`response_format: {type: "json_schema", json_schema: {name, strict, schema}}` gives the same
"answer must match this schema" guarantee `describe`'s Anthropic request already relies on.

- Images: a content part `{"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,..."}}`
  per frame, same JPEG bytes `AiContent::Jpeg` already carries.
- Output cap: `max_completion_tokens` (the current chat-completions parameter name; `max_tokens` is
  deprecated on this endpoint but still accepted — using the current name avoids a deprecation
  warning on every request).
- No `effort`/reasoning parameter: both models picked below are non-reasoning models, so
  `Model.effort` stays `None` for them, exactly like Haiku's shape — `AiRequest.effort` already
  means "omit the field when `None`", so `openai::body` never emits one either.
- Usage fields differ from Anthropic's: `usage.prompt_tokens` / `usage.completion_tokens` (not
  `input_tokens` / `output_tokens`), mapped into the same provider-neutral `AiUsage`.
- `finish_reason` (not `stop_reason`) values: `"stop"` (finished normally) is normalised to
  `"end_turn"`, `"length"` (hit the output cap) to `"max_tokens"`, `"content_filter"` (blocked) to
  `"refusal"` — reusing the exact stop-reason strings `describe::parse_answer` and
  `tags::parse_*_answer` already special-case, so neither needs a provider-specific branch. Any
  other `finish_reason` (e.g. `"tool_calls"`, unreached since no tools are offered) passes through
  unchanged into the existing `"The model stopped early ({other})"` fallback.

## Models

`gpt-4.1-mini` (cheap default, comparable to Haiku) and `gpt-4.1` (the stronger option), the two
the issue asks for. Both are vision-capable and support `json_schema` structured outputs on Chat
Completions. Prices (not live-verified, see above): `gpt-4.1` $2.00 / $8.00 per Mtok,
`gpt-4.1-mini` $0.40 / $1.60 per Mtok.

`MODELS` grows from 3 to 5 entries; `MODELS[0]` (Claude Haiku 4.5) stays the crate's overall
default — adding cheaper-on-paper OpenAI entries does not change which provider is picked when
none is requested, since `--provider`/`Options.model` default to Anthropic regardless of `MODELS`'
order. The doc comment on `MODELS` is adjusted to say so instead of a strict "cheapest first"
ordering claim that no longer holds crate-wide.

## Where the provider choice lives

`Model` gains a `provider: Provider` field (`Provider::Anthropic` or `Provider::OpenAi`) instead of
`Options` gaining a separate `provider` field that could disagree with the model actually chosen:
a model belongs to exactly one provider, so carrying it on `Model` makes an inconsistent
combination (an Anthropic model id sent to the OpenAI client) unrepresentable instead of merely
checked for. `Options.model.provider` is what `describe`/`describe_with_tags`/`suggest_tags` switch
on to build the right client; `Options.api_key`'s doc comment changes from "An Anthropic API key"
to "The API key for `model`'s provider" — same field, same type, no shape change for that field
itself.

## Errors are provider-neutral

`AiError::KeyRejected` and `AiError::OutOfCredit` were unit variants whose `reason()`/`stops_job()`
text hardcoded "Anthropic" — correct only because there was one provider. They become
`KeyRejected(String)` / `OutOfCredit(String)`, the string the provider's display name ("Anthropic"
or "OpenAI"), filled in by each provider module's own error classification (`anthropic::classify`
passes `"Anthropic"`, `openai::classify` passes `"OpenAI"`) — a breaking change to a public enum,
covered in `version.md`. `describe::parse_answer`'s separate `"Claude declined to describe it"`
message (a `refusal` stop reason, unrelated to `AiError`) is generalised to `"The model declined to
describe it"` at the same time, for the same reason.

`AiError::LimitReached`'s heuristic (`is_limit`, matching "usage limit"/"spend limit" phrases in
Anthropic's own error message) is Anthropic-specific text matching, not a general HTTP shape,
so it is not reused for OpenAI: `openai::classify` maps a 429 whose JSON error `code` is
`"insufficient_quota"` to `OutOfCredit` (OpenAI's actual shape for "no credit left") and leaves rate
limiting (no such code, or a `Retry-After` header) to the same `RateLimited` wait-and-retry path
`anthropic::classify` already uses.

## Shared retry/timeout code

`RetryPolicy`, `timeout_for` and `CONNECT_TIMEOUT` moved from `anthropic.rs` to `provider.rs`: they
never had any Anthropic-specific content (delay lengths, an upload-speed allowance, a connect
timeout), and `openai.rs` needs the exact same shape. `anthropic::RetryPolicy` stays a valid path
(`pub use crate::provider::RetryPolicy;`) so nothing that already names it breaks.

The retry *loop* itself started out duplicated once per provider module (`Attempt`, `wait`, the
`complete` loop) — an early draft's reasoning was that the two providers' HTTP status
classification differs enough (different error-code shapes, different quota signal) that a
generic executor would need its classification step passed in anyway, buying no real
deduplication. Review found that reasoning didn't hold: the loop only ever consumes an already-
classified `Attempt`, so `provider::retry_loop(&RetryPolicy, provider_label, cancel, impl FnMut()
-> Attempt)` factors out cleanly, leaving each provider's own `body`, `classify` and
`parse_message` — the genuinely provider-specific parts — untouched. Both `anthropic::complete`
and `openai::complete` now call it; only `attempt()` (the actual HTTP round-trip and its status
classification) stays per provider.

## CLI

`--provider anthropic|openai` (default `anthropic`). `--model` becomes optional
(`--model gpt-4.1-mini`, `--model gpt-4.1`, alongside the existing `haiku`/`sonnet`/`opus`); when
omitted it picks the chosen provider's own default (`haiku` for Anthropic, `gpt-4.1-mini` for
OpenAI). Passing a model that belongs to the other provider (`--model haiku --provider openai`) is
a usage error, not a silent override, since the two are independently typeable. `--api-key` still
overrides either provider's key; without it, `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` is read
depending on `--provider` (replacing the previous static `env = "ANTHROPIC_API_KEY"` clap attribute,
since the variable to read is no longer known at compile time).

## Testing

`src/openai.rs` uses the same local-TCP-listener mock server as `src/anthropic.rs` for what stays
genuinely per-provider: request body shape, `classify`'s status-code mapping, `parse_message`'s
answer shape (including OpenAI's own `message.refusal` field, which Anthropic's shape has no
equivalent of). The generic retry/rate-limit/cancellation mechanics `retry_loop` and `wait`
implement are tested once, directly, in `provider.rs`, with a fake `Attempt`-returning closure and
no HTTP at all — review found the first version of this PR testing that mechanism twice, once per
provider, against a real mock server each time, which is exactly the production-code duplication
the same round asked to remove, just moved into `#[cfg(test)]`.

No network test can run against the real API from this environment (`api.openai.com` is blocked
here); a live test gated on `OPENAI_API_KEY`, skipped when unset, mirrors
`live_description_of_a_test_clip`'s existing pattern for Anthropic — it will only ever run once the
owner adds `OPENAI_API_KEY` as a CI secret, same as the issue's own text says.

## Decisions made without the owner

- Chat Completions over the Responses API: more stable, more widely mirrored in tooling and docs,
  and sufficient for a single-turn, no-tools, structured-JSON request.
- `gpt-4.1` / `gpt-4.1-mini` rather than a newer-sounding family this session cannot verify exists
  or reach — see "What could not be verified live" above. If the owner has since confirmed a newer,
  cheaper or better pair, swapping the two `MODELS` entries' ids and prices is a small, isolated
  change (nothing else in the crate names a model id directly).
- The provider lives on `Model`, not as a separate `Options` field — see "Where the provider choice
  lives" above.
- `RetryPolicy` and the retry loop itself (`provider::retry_loop`) both ended up shared — see
  "Shared retry/timeout code" above for why the loop wasn't from the start, and why that turned
  out to be wrong.
- No OpenAI equivalent of `AiError::LimitReached`'s account-usage-limit text matching: OpenAI's 429
  shape (an error `code`, not a free-text phrase to match) does not carry the same signal, and
  guessing at wording risks a wrong classification more than it helps; unclassified 429s without a
  quota code fall back to the existing rate-limit wait, which is safe (worst case: waits before
  failing) even if the true cause was something `LimitReached` would have named more precisely.
