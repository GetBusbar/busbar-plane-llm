// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Concrete chat IR types (request, response and stream structs and enums), shared by path with
//! busbar-core's test build so `crate::codec::ir::IrRequest` resolves in both crates.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IrRequest {
    pub system: Vec<IrBlock>,
    pub messages: Vec<IrMessage>,
    /// Count of wire turns folded out of `messages` into `system` (a `system`/`developer` entry
    /// anywhere in `messages`/`input`). Zero where system is its own wire field. A hook's
    /// `message_count` counts the raw wire array, so `IrFacts::shape().turn_count` adds this back.
    pub system_turns_folded: usize,
    /// Where each folded turn stood on the wire, in wire order. The hook view reads it so a
    /// `prompt: rw` hook's reply does not delete the in-band system prompt. Writers never read it.
    pub system_folds: Vec<IrSystemFold>,
    pub tools: Vec<IrTool>,
    pub max_tokens: Option<u32>,
    // f64, not f32: an f32 round-trip mutates the caller's temperature (0.7 -> 0.699999988).
    pub temperature: Option<f64>,
    /// Nucleus-sampling cutoff (`top_p`). First-class, not left in `extra`, because `extra` is
    /// cleared on the cross-protocol seam and a control left there would be dropped on every hop.
    /// `f64` for the same reason as `temperature`.
    pub top_p: Option<f64>,
    /// Top-k cutoff (`top_k`). OpenAI has no top_k, so its writer omits it and its reader never
    /// sets it.
    pub top_k: Option<u32>,
    /// `frequency_penalty`. Written only by protocols that model it (OpenAI/Responses/Cohere).
    pub frequency_penalty: Option<f64>,
    /// `presence_penalty`. Written only by protocols that model it (OpenAI/Responses/Cohere).
    pub presence_penalty: Option<f64>,
    /// `seed`. Written only by protocols that model it (OpenAI/Responses, Gemini). `i64` keeps the
    /// full JSON integer range.
    pub seed: Option<i64>,
    /// Candidate count (OpenAI `n`, Gemini `candidateCount`). Cohere v2 has no `n` parameter, so the
    /// Cohere reader and writer omit it, like Anthropic/Bedrock/Responses.
    pub n: Option<u32>,
    /// The reasoning/thinking ask, normalized across spellings (OpenAI `reasoning_effort` word,
    /// Anthropic/Gemini numeric budget). Number to number is a copy; word to number goes through
    /// [`IrRequest::reasoning_budgets`]. The egress seam clears it with a warn when the lane lacks the
    /// `reasoning` capability, so a non-reasoning model never 400s. Response-side thinking content is
    /// carried by Thinking blocks and not gated.
    pub reasoning: Option<IrReasoningAsk>,
    /// Effort-word to token-budget table [minimal, low, medium, high], stamped by the egress seam
    /// from `limits.reasoning_effort_budgets` (defaults 1024/4096/8192/16384). Writers fall back to
    /// the same defaults when `None`.
    pub reasoning_budgets: Option<[u32; 4]>,
    /// Per-token logprob request (OpenAI `logprobs`, Gemini `generationConfig.responseLogprobs`).
    /// Response data rides [`IrResponse::logprobs`] / [`IrDelta::LogprobsDelta`].
    pub logprobs: Option<bool>,
    /// Top-alternative token count per position (OpenAI `top_logprobs`, Gemini `generationConfig.logprobs`).
    pub top_logprobs: Option<u32>,
    /// Structured-output directive, canonicalized on read so a writer only emits its own native shape.
    pub response_format: Option<IrResponseFormat>,
    /// Stop sequences. Empty means omitted, so a request without stops does not gain an empty array.
    pub stop: Vec<String>,
    /// Tool-selection directive. First-class because `extra` is cleared on the cross-protocol seam,
    /// which would degrade forced tool use to `auto`. `None` emits no `tool_choice`.
    pub tool_choice: Option<IrToolChoice>,
    /// End-user id: OpenAI `user`, Anthropic `metadata.user_id`. Other writers never emit it.
    pub user: Option<String>,
    /// "Parallel tool calls allowed?". OpenAI `parallel_tool_calls` and Anthropic
    /// `tool_choice.disable_parallel_tool_use` are the same switch, inverted. `None` emits nothing.
    pub parallel_tool_calls: Option<bool>,
    // Typed request slots: absent unless the source dialect carried the concept; writers emit
    // nothing for an absent slot.
    /// IR-03. Caller metadata map in key order: OpenAI `metadata`, Bedrock `requestMetadata`, Gemini
    /// `labels`. `Some(empty)` means the caller sent `{}`. A key-validating writer drops bad entries
    /// with a warn, not the whole map.
    pub metadata: Option<Vec<(String, String)>>,
    /// IR-04. Requested capacity tier ([`IrServiceTier`]).
    pub service_tier: Option<IrServiceTier>,
    /// IR-05. OpenAI Chat / Responses `store`.
    pub store: Option<bool>,
    /// IR-06. OpenAI `safety_identifier` (successor of `user`; a caller may send both).
    pub safety_identifier: Option<String>,
    /// IR-06. OpenAI Chat / Responses `prompt_cache_key`.
    pub prompt_cache_key: Option<String>,
    /// IR-07. OpenAI Chat `verbosity`, Responses `text.verbosity`.
    pub verbosity: Option<IrVerbosity>,
    /// IR-10. The tool subset the model may call, paired with `tool_choice`. Chat/Responses
    /// `allowed_tools`, Gemini `allowedFunctionNames`. A writer without a native subset sends only
    /// the listed tools. Never set with `tool_choice: Tool{..}`.
    pub allowed_tools: Option<Vec<String>>,
    /// IR-11. Provider-hosted tools that cross the seam. A reader that recognises a hosted tool's
    /// kind puts it here, not as a raw `IrTool`. A writer with no analog drops it with a warn and
    /// reports it from `dropped_egress_controls`.
    pub hosted_tools: Vec<IrHostedTool>,
    /// IR-14. `Some(Developer)` iff every folded system entry used the `developer` role,
    /// `Some(System)` iff every one used `system`, `None` otherwise.
    pub system_role: Option<IrSystemRole>,
    /// IR-19. Requested output modalities ([`IrModality`]): Chat `modalities`, Gemini
    /// `generationConfig.responseModalities`. `None` == absent (text).
    pub output_modalities: Option<Vec<IrModality>>,
    pub stream: bool,
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Normalized cross-protocol `tool_choice`, so forced/targeted tool use round-trips instead of
/// degrading to `auto`:
///
/// | Variant    | OpenAI / Responses (Cohere)        | Anthropic                    | Gemini (`functionCallingConfig`)            | Bedrock (`toolChoice`) |
/// |------------|------------------------------------|------------------------------|---------------------------------------------|------------------------|
/// | `Auto`     | `"auto"` (Cohere: omit — default)  | `{type:"auto"}`              | `{mode:"AUTO"}`                             | `{auto:{}}`            |
/// | `None`     | `"none"` (Cohere: `"NONE"`)        | `{type:"none"}`*             | `{mode:"NONE"}`                             | (omit — no native)*    |
/// | `Required` | `"required"` (Cohere: `"REQUIRED"`)| `{type:"any"}`              | `{mode:"ANY"}`                             | `{any:{}}`             |
/// | `Tool{n}`  | `{type:"function",function:{name}}`| `{type:"tool",name:n}`      | `{mode:"ANY",allowedFunctionNames:[n]}`    | `{tool:{name:n}}`      |
///
/// *Older Anthropic targets without a native "none" omit `tool_choice`. A reader maps an unknown
/// native value to `Auto` rather than dropping the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrToolChoice {
    /// Model decides. The universal default.
    Auto,
    /// Text only.
    None,
    /// Must call some tool, not a pinned one (OpenAI `"required"`, Anthropic `"any"`, Gemini `ANY`, Bedrock `any`).
    Required,
    /// Must call this tool by name.
    Tool { name: String },
}

/// Normalize a native stop-sequence field (bare string or array) into `Vec<String>`. Non-string
/// elements and empty strings are skipped, so `""` or `[""]` yields an empty vec (omitted) and
/// never a spurious `stop: [""]`.
pub fn read_stop_sequences(val: Option<&Value>) -> Vec<String> {
    match val {
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// Truncate `stop` to the egress vendor's published cap (vendors 400 on more). Warns only when it
/// truncates. This is a clamp-and-warn degrade at HTTP 200, not a 400.
pub fn clamp_stop(stop: &[String], cap: usize, proto: &'static str) -> Vec<String> {
    if stop.len() <= cap {
        return stop.to_vec();
    }
    let provided = stop.len();
    // `stop.len() > cap` holds after the early return, so this cannot underflow.
    let dropped = provided - cap;
    crate::codec::drops::note(crate::codec::drops::Dropped::slot(
        "stop",
        &crate::codec::diagnostics::IR_TRUNCATE_STOP_SEQUENCES,
        format!(
            "truncating stop sequences to {proto}'s documented cap of {cap}; the request carried \
             {provided}, so {dropped} were dropped"
        ),
    ));
    stop[..cap].to_vec()
}

#[derive(Debug, Clone, PartialEq)]
pub enum IrStreamEvent {
    MessageStart {
        role: IrRole,
        usage: Option<IrUsage>,
        /// Stream identity from the backend's stream-start metadata, so a writer can emit the native
        /// top-level identity fields. Readers populate it; the writer synthesizes one when absent.
        ///
        /// On a cross-protocol stream `StreamTranslate::translate_event` clears only `id` and `created`,
        /// so the ingress writer mints a native id instead of leaking `chatcmpl-`/`msg_`. `model` is kept:
        /// writers anchor the native stream-start skeleton on it (clearing it gave a degenerate Anthropic
        /// `message_start` and a Gemini frame without `modelVersion`). Same-protocol streams stay byte-exact.
        id: Option<String>,
        /// Unix epoch seconds of stream creation (OpenAI chunk `created`).
        created: Option<u64>,
        /// The model that served the stream. Mirrors `IrResponse::model`.
        model: Option<String>,
    },
    BlockStart {
        index: usize,
        block: IrBlockMeta,
        /// IR-02. `true` for a text block whose deltas are a refusal message; the streaming twin of
        /// `IrBlock::Text::refusal`.
        refusal: bool,
    },
    BlockDelta {
        index: usize,
        delta: IrDelta,
    },
    BlockStop {
        index: usize,
    },
    MessageDelta {
        stop_reason: Option<IrStopReason>,
        /// Anthropic `message_delta.delta.stop_sequence`: the matched stop string. Only the Anthropic
        /// reader sets it and only its writer emits it, which keeps same-protocol passthrough byte-faithful.
        stop_sequence: Option<String>,
        /// IR-16 / IR-02. Streaming twin of [`IrResponse::stop_detail`].
        stop_detail: Option<IrStopDetail>,
        usage: IrUsage,
    },
    MessageStop,
    Error(busbar_contract::protocol::IrError),
}

/// Protocol-neutral stop/finish reason. A reader maps any unknown native token to [`Self::Other`]
/// (no String payload, so nothing foreign can be echoed into another protocol's closed
/// `finish_reason` enum), and every writer matches exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrStopReason {
    /// Natural end of turn.
    EndTurn,
    /// A provided stop sequence was generated.
    StopSequence,
    /// The output token cap (or model max) was reached.
    MaxTokens,
    /// The model wants to call one or more tools.
    ToolUse,
    /// Content moderation / safety filter stopped generation.
    Safety,
    /// The model refused to continue (Anthropic `refusal`, Responses `incomplete_details:refusal`).
    Refusal,
    /// A long-running turn was paused (Anthropic `pause_turn`).
    PauseTurn,
    /// An upstream/server error terminated generation (Cohere `ERROR`).
    Error,
    /// An unrecognized or future native reason. Every writer projects it to its natural-stop default.
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IrResponse {
    pub role: IrRole,
    pub content: Vec<IrBlock>,
    pub stop_reason: Option<IrStopReason>,
    pub usage: IrUsage,
    /// The model that served the response, so a pool route names the serving member. `None` if the upstream sent none.
    pub model: Option<String>,
    /// Response identity from the backend's `read_response`; readers populate it and the writer
    /// synthesizes one when absent.
    ///
    /// On a cross-protocol response the foreign `id` is stripped (`proxy engine` sets `ir.id = None`)
    /// and the ingress writer mints a native id when `created` is `Some`. Same-protocol keeps the
    /// native id verbatim.
    pub id: Option<String>,
    /// Unix epoch seconds of creation (OpenAI `created`).
    pub created: Option<u64>,
    /// OpenAI `system_fingerprint`.
    pub system_fingerprint: Option<String>,
    /// Anthropic `stop_sequence` (the matched stop string).
    pub stop_sequence: Option<String>,
    /// Per-token logprobs in generation order (OpenAI `choices[].logprobs.content`; Gemini
    /// `candidates[].logprobsResult`). Empty means the backend sent none.
    pub logprobs: Vec<IrTokenLogprob>,
    /// The original ingress request body, captured on the cross-protocol response seam so a
    /// Responses writer can mirror client-set request values (`temperature`, `top_p`,
    /// `instructions`, `metadata`, `tool_choice`, `parallel_tool_calls`, `tools`). `None` on
    /// same-protocol and for every non-Responses writer.
    /// IR-16 / IR-02. Refinement of `stop_reason` ([`IrStopDetail`]); `stop_reason` is always set to
    /// the nearest coarse variant beside it. `None` == no refinement.
    pub stop_detail: Option<IrStopDetail>,
    pub request_echo: Option<Value>,
    /// Safety/moderation verdicts the upstream reported, one per category.
    pub safety: Vec<IrSafetyVerdict>,
    /// Audio the model produced.
    pub audio: Option<IrAudioOutput>,
}

/// A safety or moderation verdict, the core every dialect that reports one shares (Gemini
/// `safetyRatings`, the OpenAI family's `moderation`, Bedrock's guardrail trace): which category,
/// whether it was flagged, whether it blocked content. Scores, probabilities and trace detail are not
/// comparable across providers and do not cross.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IrSafetyVerdict {
    /// The category as the provider names it.
    pub category: String,
    pub flagged: bool,
    pub blocked: bool,
}

impl IrSafetyVerdict {
    /// A `{"<category>": true|false, …}` map (the OpenAI family's moderation `categories`) as one
    /// flagged, non-blocking verdict per category that is `true`.
    pub fn flagged_categories(categories: Option<&Value>) -> Vec<IrSafetyVerdict> {
        categories
            .and_then(|c| c.as_object())
            .into_iter()
            .flatten()
            .filter(|(_, on)| on.as_bool() == Some(true))
            .map(|(category, _)| IrSafetyVerdict {
                category: category.clone(),
                flagged: true,
                blocked: false,
            })
            .collect()
    }
}

/// Audio the model produced in its answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IrAudioOutput {
    /// The audio bytes, base64, as the provider sent them.
    pub data: Option<String>,
    /// The audio format word (`wav`, `mp3`, `pcm16`, ...).
    pub format: Option<String>,
    /// The spoken text.
    pub transcript: Option<String>,
}

/// The kind of a [`IrBlock::HostedToolRecord`]: only web search has a counterpart in more than one
/// dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrHostedToolKind {
    WebSearch,
}

/// One result of a hosted search.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IrSearchResult {
    pub url: String,
    pub title: Option<String>,
    pub snippet: Option<String>,
}

/// An empty assistant answer, so construction sites can spread `..Default::default()` and survive
/// new response slots. `role` is `Assistant` because every IR response is the model's turn.
impl Default for IrResponse {
    fn default() -> Self {
        IrResponse {
            role: IrRole::Assistant,
            content: Vec::new(),
            stop_reason: None,
            usage: IrUsage::default(),
            model: None,
            id: None,
            created: None,
            system_fingerprint: None,
            stop_sequence: None,
            logprobs: Vec::new(),
            request_echo: None,
            stop_detail: None,
            safety: Vec::new(),
            audio: None,
        }
    }
}

/// The normalized reasoning/thinking ask (see [`IrRequest::reasoning`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrReasoningAsk {
    /// A word-form ask (OpenAI `reasoning_effort` / Responses `reasoning.effort`).
    Effort(IrReasoningEffort),
    /// A numeric thinking-token budget (Anthropic `budget_tokens`, Gemini `thinkingBudget`).
    Budget(u32),
    /// Gemini `thinkingBudget: -1`, "the model decides" (adaptive Claude, Cohere budget-less enable).
    /// Where the far end has no such form it is dropped on the one drop path, never substituted.
    /// [`Self::to_budget`] / [`Self::to_effort`] have no table entry for it.
    Dynamic,
    /// IR-09. Reasoning explicitly off (`thinking:{type:"disabled"}`, effort `"none"`, Gemini
    /// `thinkingBudget: 0`). Different from `None` on [`IrRequest::reasoning`] ("never said"): a
    /// reasoning-by-default model keeps thinking when nothing is said. No table entry, since the
    /// smallest value would invert the meaning.
    Off,
}

/// The effort words, ordered by ascending budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    /// IR-09. OpenAI/Anthropic `"xhigh"`. Budget projection is the table's top entry.
    XHigh,
    /// IR-09. Anthropic `output_config.effort` `"max"`.
    Max,
}

/// Compiled-in effort table (mirrors the config defaults), the writer fallback when the seam did
/// not stamp [`IrRequest::reasoning_budgets`].
pub const REASONING_BUDGET_DEFAULTS: [u32; 4] = [1024, 4096, 8192, 16384];

impl IrReasoningAsk {
    /// Project to a numeric budget via the table. `None` for `Dynamic` and `Off`, which a writer
    /// carries in its own form or drops, never substitutes.
    pub fn to_budget(self, table: [u32; 4]) -> Option<u32> {
        match self {
            IrReasoningAsk::Budget(n) => Some(n),
            IrReasoningAsk::Effort(IrReasoningEffort::Minimal) => Some(table[0]),
            IrReasoningAsk::Effort(IrReasoningEffort::Low) => Some(table[1]),
            IrReasoningAsk::Effort(IrReasoningEffort::Medium) => Some(table[2]),
            // No table row above `high`: the two words above it take its top budget.
            IrReasoningAsk::Effort(
                IrReasoningEffort::High | IrReasoningEffort::XHigh | IrReasoningEffort::Max,
            ) => Some(table[3]),
            IrReasoningAsk::Dynamic | IrReasoningAsk::Off => None,
        }
    }

    /// Project to a word, bucketing a numeric budget to the largest effort whose table entry it
    /// reaches. `None` for `Dynamic` and `Off`.
    pub fn to_effort(self, table: [u32; 4]) -> Option<IrReasoningEffort> {
        match self {
            IrReasoningAsk::Effort(e) => Some(e),
            IrReasoningAsk::Dynamic | IrReasoningAsk::Off => None,
            IrReasoningAsk::Budget(n) => Some({
                if n >= table[3] {
                    IrReasoningEffort::High
                } else if n >= table[2] {
                    IrReasoningEffort::Medium
                } else if n >= table[1] {
                    IrReasoningEffort::Low
                } else {
                    IrReasoningEffort::Minimal
                }
            }),
        }
    }
}

impl IrReasoningEffort {
    /// The effort on the `low`/`medium`/`high` scale every reasoning model accepts. Same as
    /// [`Self::as_str`] except `Minimal` maps to `"low"` (only gpt-5 accepts `"minimal"`) and
    /// `XHigh`/`Max` map to `"high"`. The lane's model is unknown here, and these are the values
    /// that never cause a 400. A writer that knows its lane accepts more may use [`Self::as_str`].
    pub fn as_three_word_str(self) -> &'static str {
        match self {
            IrReasoningEffort::Minimal => "low",
            IrReasoningEffort::XHigh | IrReasoningEffort::Max => "high",
            other => other.as_str(),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            IrReasoningEffort::Minimal => "minimal",
            IrReasoningEffort::Low => "low",
            IrReasoningEffort::Medium => "medium",
            IrReasoningEffort::High => "high",
            IrReasoningEffort::XHigh => "xhigh",
            IrReasoningEffort::Max => "max",
        }
    }

    /// The four words every reader has always accepted. Not widened to `xhigh`/`max`: a reader
    /// opts in through [`Self::parse_extended`] once every reachable writer projects them.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "minimal" => Some(IrReasoningEffort::Minimal),
            "low" => Some(IrReasoningEffort::Low),
            "medium" => Some(IrReasoningEffort::Medium),
            "high" => Some(IrReasoningEffort::High),
            _ => None,
        }
    }

    /// [`Self::parse`] plus `xhigh` and `max` (IR-09).
    pub fn parse_extended(s: &str) -> Option<Self> {
        match s {
            "xhigh" => Some(IrReasoningEffort::XHigh),
            "max" => Some(IrReasoningEffort::Max),
            other => Self::parse(other),
        }
    }
}

/// One generated token's logprob plus the top alternatives at that position, the neutral pivot
/// between OpenAI entries and Gemini's parallel `logprobsResult` arrays. `bytes` is OpenAI-only
/// (a token can be a partial UTF-8 fragment); a writer needing bytes synthesizes them from the token.
#[derive(Debug, Clone, PartialEq)]
pub struct IrTokenLogprob {
    pub token: String,
    pub logprob: f64,
    pub bytes: Option<Vec<u8>>,
    /// Top alternatives at this position (may include the chosen token). Empty when not asked for.
    pub top: Vec<IrTopLogprob>,
}

/// One alternative token candidate inside [`IrTokenLogprob::top`].
#[derive(Debug, Clone, PartialEq)]
pub struct IrTopLogprob {
    pub token: String,
    pub logprob: f64,
    pub bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IrMessage {
    pub role: IrRole,
    pub content: Vec<IrBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IrBlock {
    Text {
        text: String,
        cache_control: Option<CacheControl>,
        citations: Vec<IrCitation>,
        /// IR-02. `true` when this text is the model's refusal message. A writer with no refusal
        /// shape emits it as ordinary text.
        refusal: bool,
    },
    Thinking {
        text: String,
        signature: Option<String>,
        /// `true` for an opaque redacted/encrypted reasoning block (Anthropic `redacted_thinking`,
        /// Bedrock `redactedContent`); `text` holds opaque bytes, not plaintext. Only the
        /// Anthropic/Bedrock readers set it and only those writers re-emit it; others drop the block.
        redacted: bool,
        /// Anthropic `cache_control` on a `thinking` block. Only the Anthropic reader and writer
        /// touch it; other protocols leave it `None`.
        cache_control: Option<CacheControl>,
        /// IR-17. Full reasoning vs a summary ([`IrThinkingKind`]). `None` == unknown.
        kind: Option<IrThinkingKind>,
        /// IR-18. Which model family minted `signature` ([`IrSignatureOrigin`]). `None` == unknown.
        signature_origin: Option<IrSignatureOrigin>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        /// Anthropic `cache_control` on a `tool_use` block. Only the Anthropic reader and writer
        /// touch it.
        cache_control: Option<CacheControl>,
        /// Gemini 3 `thoughtSignature`, a sibling of `functionCall` that must be echoed back verbatim
        /// next turn or Gemini 400s. Set by Gemini readers, and injected by [`IrReq::prepare_for_egress`]
        /// with Google's documented sentinel when the egress lane is Gemini AI Studio and the block has
        /// no real signature (e.g. OpenAI history replayed against Gemini).
        thought_signature: Option<String>,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<IrBlock>,
        is_error: bool,
        /// Anthropic `cache_control` on a `tool_result` block.
        cache_control: Option<CacheControl>,
    },
    Image {
        /// The typed image source. A writer projects the variants it can represent and drops the
        /// rest with a warn.
        source: IrImageSource,
        /// Anthropic `cache_control` on an `image` block. Only the Anthropic reader and writer touch it.
        cache_control: Option<CacheControl>,
        /// IR-08. Requested fidelity ([`IrImageDetail`]): Chat / Responses / Cohere `detail`.
        detail: Option<IrImageDetail>,
    },
    /// A non-image attachment (document, audio clip or video clip).
    ///
    /// Every writer projects it into the target's native slot when one exists, and otherwise drops
    /// it with a `warn!` naming the field, never as an empty text block (which looks like the
    /// caller sent nothing). Like [`IrBlock::Image`], it contributes no item to `ir::facts`
    /// projections, so attachment bytes and filenames are not shown to content hooks.
    Media {
        /// Attachment class; decides the target slot (Bedrock `document` vs `video`).
        kind: IrMediaKind,
        /// The attachment source, the same carrier `Image` uses (inline base64, URL, or an opaque
        /// vendor-scoped reference re-emitted only by its own protocol).
        source: IrImageSource,
        /// Filename/title where the wire carries one (OpenAI `file.filename`, Anthropic
        /// `document.title`, Bedrock `document.name`). Bedrock requires a name on a document block.
        name: Option<String>,
        /// Anthropic `cache_control` on a `document` block.
        cache_control: Option<CacheControl>,
        /// IR-12. Document citations switch: Anthropic and Bedrock `citations:{enabled}`.
        citations: Option<bool>,
        /// IR-12. Document context string: Anthropic and Bedrock document `context`.
        context: Option<String>,
    },
    /// Structured JSON content, a Bedrock `{"json": <value>}` tool-result content block. Bedrock
    /// re-emits it; protocols whose tool-result content is text/image-only drop it with a warn.
    Json(Value),
    /// A record of a tool the provider ran (Anthropic `web_search_tool_result`, Responses
    /// `web_search_call`, Gemini grounding). Citations stay on the text blocks ([`IrCitation`]).
    HostedToolRecord {
        kind: IrHostedToolKind,
        call_id: Option<String>,
        status: Option<String>,
        results: Vec<IrSearchResult>,
    },
}

/// A caller-supplied RAG passage (Anthropic `search_result`, Converse `searchResult`) as the parts
/// both wires spell.
#[derive(Debug, Clone, PartialEq)]
pub struct IrSearchResultParts<'a> {
    pub source: &'a str,
    pub title: &'a str,
    /// The passage text: the block's text parts joined with `\n`, the header line stripped.
    pub body: &'a str,
    /// The block's `citations` switch (`{"enabled": bool}`) verbatim, when the caller sent one.
    pub citations_config: Option<&'a Value>,
}

impl IrBlock {
    /// The block's IR kind, as a dropped block is named (`codec::drops::Member::Block`): `text`,
    /// `thinking`, `tool_use`, `tool_result`, `image`, or the attachment's media kind.
    pub fn kind_name(&self) -> &'static str {
        match self {
            IrBlock::Text { .. } => "text",
            IrBlock::Thinking { .. } => "thinking",
            IrBlock::ToolUse { .. } => "tool_use",
            IrBlock::ToolResult { .. } | IrBlock::Json(_) => "tool_result",
            IrBlock::Image { .. } => "image",
            IrBlock::Media { kind, .. } => kind.as_str(),
            IrBlock::HostedToolRecord { .. } => "hosted_tool",
        }
    }

    /// A plain text block: no cache marker, no citations, not a refusal.
    pub fn text(text: impl Into<String>) -> IrBlock {
        IrBlock::Text {
            text: text.into(),
            cache_control: None,
            citations: Vec::new(),
            refusal: false,
        }
    }

    /// The search-result slot: one caller-supplied RAG passage (Anthropic `search_result`, Converse
    /// `searchResult`) modelled as a `Text` block any dialect can read (a `title — source` header line,
    /// then the passage), carrying one provenance citation of kind `search_result_location` with no
    /// location indices (which tells it apart from an answer's search-result citation, which always has
    /// them). The citation's `raw` holds the block's `citations` switch verbatim (`{"enabled": bool}`,
    /// spelled identically by both wires). A dialect with the block re-emits it from
    /// [`IrBlock::as_search_result`]; any other sends the text.
    pub fn search_result(
        source: &str,
        title: &str,
        body: String,
        citations_config: Option<Value>,
        cache_control: Option<CacheControl>,
    ) -> IrBlock {
        let header = search_result_header(source, title);
        let text = if header.is_empty() {
            body
        } else if body.is_empty() {
            header
        } else {
            format!("{header}\n{body}")
        };
        let citations = if source.is_empty() && title.is_empty() {
            Vec::new()
        } else {
            vec![IrCitation {
                kind: Some(crate::codec::keys::SEARCH_RESULT_LOCATION.to_string()),
                title: (!title.is_empty()).then(|| title.to_string()),
                url: (!source.is_empty()).then(|| source.to_string()),
                raw: citations_config,
                ..Default::default()
            }]
        };
        IrBlock::Text {
            text,
            cache_control,
            citations,
            refusal: false,
        }
    }

    /// The search-result slot's parts, or `None` for any other block ([`IrBlock::search_result`]).
    pub fn as_search_result(&self) -> Option<IrSearchResultParts<'_>> {
        let IrBlock::Text {
            text, citations, ..
        } = self
        else {
            return None;
        };
        let [c] = citations.as_slice() else {
            return None;
        };
        let provenance_only = c.kind.as_deref() == Some(crate::codec::keys::SEARCH_RESULT_LOCATION)
            && c.cited_text.is_none()
            && c.document_index.is_none()
            && c.start_index.is_none()
            && c.end_index.is_none()
            && c.encrypted_index.is_none()
            && c.domain.is_none()
            && c.file.is_none();
        if !provenance_only {
            return None;
        }
        let source = c.url.as_deref().unwrap_or("");
        let title = c.title.as_deref().unwrap_or("");
        let header = search_result_header(source, title);
        let body = if text.as_str() == header {
            ""
        } else {
            text.strip_prefix(header.as_str())
                .and_then(|t| t.strip_prefix('\n'))
                .unwrap_or(text)
        };
        Some(IrSearchResultParts {
            source,
            title,
            body,
            citations_config: c.raw.as_ref(),
        })
    }

    /// Is this block's content opaque to busbar (carried verbatim, never readable as plaintext, never
    /// disclosable to a sidecar)? Opaque shapes: Anthropic `redacted_thinking`, Bedrock
    /// `redactedContent` (both `Thinking { redacted: true, text: <ciphertext> }`), and a Responses
    /// `reasoning` item with only `encrypted_content` (`redacted: false`, empty text, blob in
    /// `signature`), which a redaction check keyed on `redacted` would pass, hence this predicate.
    ///
    /// `redacted` is deliberately not set for the Responses shape: it is a writer instruction ("this
    /// block's `text` is the opaque payload to re-emit"). The Responses writer drops `redacted` blocks
    /// (losing `encrypted_content` on a round-trip) and the Anthropic and Bedrock writers would emit an
    /// empty-ciphertext block cross-protocol.
    ///
    /// The empty-text-plus-blob arm is general: a text-less Gemini thought part with `thoughtSignature`
    /// lands the same way. A block with real plaintext and a signature is readable. Call this before
    /// touching `text`, which is empty or the ciphertext itself when this returns true.
    /// COH-17: an empty text block that only carries citations (a grounded turn with no text
    /// part). A writer whose dialect rejects an empty text block (Anthropic, Bedrock, Gemini) omits it.
    pub fn is_citation_carrier(&self) -> bool {
        matches!(self, IrBlock::Text { text, citations, .. } if text.is_empty() && !citations.is_empty())
    }

    pub fn is_opaque(&self) -> bool {
        match self {
            // `text` holds the opaque bytes (Anthropic `redacted_thinking`, Bedrock `redactedContent`).
            IrBlock::Thinking { redacted: true, .. } => true,
            // No plaintext and a carrier blob in its place (a Responses `encrypted_content`-only item).
            IrBlock::Thinking {
                text, signature, ..
            } => text.is_empty() && signature.is_some(),
            // Listed explicitly, not `_`, so a future variant that can carry an opaque payload must decide here.
            IrBlock::Text { .. }
            | IrBlock::ToolUse { .. }
            | IrBlock::ToolResult { .. }
            | IrBlock::Image { .. }
            | IrBlock::Media { .. }
            | IrBlock::Json(_)
            | IrBlock::HostedToolRecord { .. } => false,
        }
    }
}

/// Class of non-image attachment an [`IrBlock::Media`] carries. The target slot differs per class
/// (Bedrock has `document` and `video`, no audio; OpenAI Chat has `input_audio` and `file`, no video).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrMediaKind {
    /// PDF / CSV / DOCX / TXT / MD / HTML.
    Document,
    /// An audio clip (OpenAI `input_audio`, Gemini `inlineData` `audio/*`).
    Audio,
    /// A video clip (Bedrock `video`, Gemini `video/*`).
    Video,
}

impl IrMediaKind {
    /// Classify a mime type by its `audio/*` / `video/*` prefix; anything else is a Document.
    /// `image/*` is not a Media kind, so callers check for it first.
    pub fn from_media_type(media_type: &str) -> Self {
        let lower = media_type.to_ascii_lowercase();
        if lower.starts_with("audio/") {
            IrMediaKind::Audio
        } else if lower.starts_with("video/") {
            IrMediaKind::Video
        } else {
            IrMediaKind::Document
        }
    }

    /// The field name for a drop `warn!`, in the caller's own vocabulary.
    pub fn as_str(self) -> &'static str {
        match self {
            IrMediaKind::Document => "document",
            IrMediaKind::Audio => "audio",
            IrMediaKind::Video => "video",
        }
    }
}

/// Image media types every protocol accepts: `image/{jpeg,png,gif,webp}`. Emitting anything else
/// verbatim (e.g. a Gemini `audio/mp3` `inlineData` read as an image) 400s the backend. Returns
/// `None` for a malformed or unsupported mime so the caller drops the block with a warn, otherwise
/// the lowercased subtype (`jpg` normalized to `jpeg`).
pub fn image_subtype_if_supported(media_type: &str) -> Option<&'static str> {
    let subtype = media_type.strip_prefix("image/").or_else(|| {
        // Tolerate "Image/PNG" without allocating in the common lowercase case.
        media_type
            .get(..6)
            .filter(|p| p.eq_ignore_ascii_case("image/"))
            .map(|_| &media_type[6..])
    })?;
    match subtype.to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => Some("jpeg"),
        "png" => Some("png"),
        "gif" => Some("gif"),
        "webp" => Some("webp"),
        _ => None,
    }
}

/// Typed source for an [`IrBlock::Image`]. `Base64` and `Url` are protocol-neutral. A reference with
/// no neutral form (Bedrock `s3Location`, Responses `file_id`) rides the opaque [`Self::Vendor`]
/// escape: only the producing protocol re-emits it and every other writer drops it with a warn.
#[derive(Debug, Clone, PartialEq)]
pub enum IrImageSource {
    /// Inline base64 image bytes with a real `image/<fmt>` media type.
    Base64 { media_type: String, data: String },
    /// A remote image URL.
    Url(String),
    /// Opaque vendor-scoped reference with no neutral form. `vendor` is the producing protocol; only
    /// its writer re-emits `value`, others drop it.
    Vendor { vendor: &'static str, value: Value },
}

/// Protocol-neutral structured-output directive. Each protocol module owns its own reader (native
/// to this) and writer (this to native), so a writer can never echo a foreign `response_format`
/// shape, which 400s the backend.
#[derive(Debug, Clone, PartialEq)]
pub struct IrResponseFormat {
    /// `false` is plain text, `true` means the model must emit JSON.
    pub json: bool,
    /// The JSON Schema to conform to, if any (`None` is free-form JSON).
    pub schema: Option<Value>,
    /// Schema name (OpenAI/Responses `json_schema.name`).
    pub name: Option<String>,
    /// Strict-schema flag (OpenAI/Responses `strict`).
    pub strict: Option<bool>,
    /// Schema description (OpenAI/Responses `json_schema.description`).
    pub description: Option<String>,
}

impl IrResponseFormat {
    /// A directive with no `name`/`strict`/`description`.
    pub fn bare(json: bool, schema: Option<Value>) -> Self {
        Self {
            json,
            schema,
            name: None,
            strict: None,
            description: None,
        }
    }

    /// A `json_schema` directive read from the object holding `name`/`schema`/`strict`/`description`:
    /// nested under `json_schema` in Chat, flat beside `type` in Responses `text.format`.
    pub fn from_json_schema_members(js: Option<&Value>) -> Self {
        let text = |k: &str| {
            js.and_then(|j| j.get(k))
                .and_then(|v| v.as_str())
                .map(String::from)
        };
        Self {
            json: true,
            schema: js.and_then(|j| j.get(crate::codec::keys::SCHEMA)).cloned(),
            name: text(crate::codec::keys::NAME),
            strict: js
                .and_then(|j| j.get(crate::codec::keys::STRICT))
                .and_then(|s| s.as_bool()),
            description: text(crate::codec::keys::DESCRIPTION),
        }
    }

    /// Inverse of [`Self::from_json_schema_members`]. OpenAI requires `name`, so one is synthesized
    /// when the source had none.
    pub fn write_json_schema_members(
        &self,
        schema: &Value,
        out: &mut serde_json::Map<String, Value>,
    ) {
        use crate::codec::keys;
        let name = self.name.as_deref().unwrap_or(keys::RESPONSE);
        out.insert(keys::NAME.to_string(), serde_json::json!(name));
        out.insert(keys::SCHEMA.to_string(), schema.clone());
        if let Some(s) = self.strict {
            out.insert(keys::STRICT.to_string(), serde_json::json!(s));
        }
        if let Some(d) = &self.description {
            out.insert(keys::DESCRIPTION.to_string(), serde_json::json!(d));
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CacheControl {
    pub kind: CacheKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    Ephemeral,
}

/// Protocol-neutral citation on a `Text` block, the union of the native shapes.
///
/// Anthropic citations must round-trip byte-exact: the reader stashes the source object verbatim
/// in [`IrCitation::raw`] and the writer re-emits it unchanged whenever `raw` is present. Only on
/// a cross-protocol egress does the writer synthesize an Anthropic object from the neutral fields.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IrCitation {
    /// Citation type tag. For Anthropic the `type` verbatim (`char_location`, `page_location`,
    /// `content_block_location`, `web_search_result_location`); for a Gemini `citationSources[]`
    /// entry `web_search_result_location`.
    pub kind: Option<String>,
    /// Quoted source span (Anthropic `cited_text`). `None` for Gemini.
    pub cited_text: Option<String>,
    /// Title of the cited document or web result (Anthropic `document_title` / `title`).
    pub title: Option<String>,
    /// Source URL (Anthropic `web_search_result_location.url`, Gemini `citationSources[].uri`).
    pub url: Option<String>,
    /// Index of the cited document in the request's `documents` (Anthropic `document_index`).
    pub document_index: Option<i64>,
    /// Start offset, interpreted per `kind`: char index, page number or block index. Gemini
    /// `startIndex` (character offset).
    pub start_index: Option<i64>,
    /// End offset, paired with `start_index` per `kind` (Gemini `endIndex`).
    pub end_index: Option<i64>,
    /// Anthropic web-search `encrypted_index`, an opaque cursor.
    pub encrypted_index: Option<String>,
    /// Verbatim source citation object for byte-exact same-protocol re-emission (Anthropic, and the
    /// Gemini `citationSources[]` entry). `None` for a citation synthesized from neutral fields.
    pub raw: Option<Value>,
    /// BED-14: the web source's domain (Converse `web` location `domain`). Bedrock only.
    pub domain: Option<String>,
    /// A citation of an uploaded file (Responses `file_citation` / `container_file_citation` / `file_path`).
    pub file: Option<IrFileLocation>,
}

/// Where in an uploaded file a citation points.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IrFileLocation {
    pub file_id: Option<String>,
    pub filename: Option<String>,
    pub index: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IrTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    /// Anthropic `cache_control` on a tool definition. Only the Anthropic reader and writer touch it.
    pub cache_control: Option<CacheControl>,
    /// Hosted (built-in) tool passthrough: the raw Responses `tools[]` object for `web_search`,
    /// `file_search`, `code_interpreter`, `mcp` etc. A hosted tool has no `name`/`parameters`, so
    /// projecting it as a function tool would emit `{"type":"function","name":""}`, which 400s.
    /// Only the Responses reader sets it and only its writer re-emits it. `IrReq::prepare_for_egress`
    /// drops every hosted tool on the cross-protocol seam with a warn.
    pub hosted: Option<Value>,
    /// Strict function calling (OpenAI Chat `function.strict`, Responses `strict`). A behavioural
    /// contract: the caller may parse arguments unvalidated, so dropping it silently breaks that.
    /// Carried by OpenAI Chat/Responses, Anthropic (ANT-04/05), Bedrock `toolSpec.strict`
    /// (BED-08) and Cohere request-level `strict_tools` when every tool agrees (COH-11/12). Gemini
    /// has no flag and Cohere cannot express disagreeing tools, so those writers drop it with a
    /// `warn!` naming the tools.
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IrUsage {
    /// Uncached input tokens. Readers normalize to this: providers whose wire total includes the
    /// cached prefix (OpenAI, Gemini, Responses) subtract the cached count; Anthropic and Bedrock
    /// store the wire value. The cache fields are then additive everywhere, so
    /// [`IrUsage::billable_tokens`] sums them provider-agnostically.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    /// Per-bucket attribution of the totals above. `None` in a bucket means "not reported", which
    /// is not `Some(0)`.
    pub detail: IrUsageDetail,
}

/// Sub-bucket attribution for [`IrUsage`]. Most fields are a slice of a total, never an addition,
/// and [`IrUsage::billable_tokens`] ignores this struct, so none can change what busbar bills.
///
/// The exception is the Cohere `billed_*` trio, Cohere's separately metered bucket.
/// [`IrUsage::to_token_usage`] ledgers the raw totals; the billed token counts are attribution the
/// Cohere writer re-emits, and `billed_classifications` is the open class `classifications`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IrUsageDetail {
    /// Reasoning tokens, a sub-bucket of `output_tokens` (never added to it). OpenAI
    /// `completion_tokens_details.reasoning_tokens`, Responses `output_tokens_details.reasoning_tokens`,
    /// Gemini `usageMetadata.thoughtsTokenCount`.
    pub reasoning_tokens: Option<u64>,
    /// Anthropic 5-minute-TTL slice of `cache_creation_input_tokens`
    /// (`usage.cache_creation.ephemeral_5m_input_tokens`). The two TTL tiers are priced differently.
    pub cache_creation_5m_input_tokens: Option<u64>,
    /// Anthropic 1-hour-TTL slice of `cache_creation_input_tokens`
    /// (`usage.cache_creation.ephemeral_1h_input_tokens`).
    pub cache_creation_1h_input_tokens: Option<u64>,
    /// Cohere `usage.billed_units.search_units`, a separately billed unit that is not a token count.
    pub search_units: Option<u64>,
    /// Anthropic `usage.server_tool_use.web_search_requests`, a separately metered bucket. Only the
    /// Anthropic reader and writer touch it.
    pub web_search_requests: Option<u64>,
    /// Anthropic `usage.server_tool_use.web_fetch_requests`, ledgered as the open class
    /// `web_fetch_requests`. Only the Anthropic reader populates it.
    pub web_fetch_requests: Option<u64>,
    /// Anthropic `usage.service_tier` (`standard` / `priority` / `batch`): the tier that served and
    /// billed the request. Only the Anthropic reader and writer touch it.
    pub service_tier: Option<String>,
    // OpenAI Chat usage sub-buckets. Each is a slice of a total in `IrUsage`, never an addition.
    /// OpenAI `usage.prompt_tokens_details.audio_tokens`: the audio slice of the input tokens.
    pub input_audio_tokens: Option<u64>,
    /// OpenAI `usage.completion_tokens_details.audio_tokens`: the audio slice of the output tokens.
    pub output_audio_tokens: Option<u64>,
    /// OpenAI `completion_tokens_details.accepted_prediction_tokens`, a sub-bucket of `output_tokens`.
    pub accepted_prediction_tokens: Option<u64>,
    /// OpenAI `completion_tokens_details.rejected_prediction_tokens` (still billed as output).
    pub rejected_prediction_tokens: Option<u64>,
    /// Gemini `usageMetadata.toolUsePromptTokenCount`, the server-side tool-use input tokens. Only
    /// the Gemini reader and writer touch it.
    ///
    /// Unlike the other buckets it is NOT a slice of the prompt count: in the Vertex recording
    /// `resp_g2g_vertex_grounding.json` it is 32 against a `promptTokenCount` of 18, and Google's
    /// `totalTokenCount` reconciles only when it is added as a fourth term (see
    /// `gemini/mod.rs::GEMINI_USAGE_ADDITIVE_TERMS`). The Gemini reader adds it to `input_tokens`
    /// (billed at the input rate); this field keeps the attribution so the writer can put the term
    /// back beside `promptTokenCount`. `billable_tokens` ignores it, so nothing double-counts.
    pub tool_use_prompt_tokens: Option<u64>,
    /// Gemini/Vertex `usageMetadata.trafficType` (`ON_DEMAND` vs `PROVISIONED`): which billing lane
    /// served the turn. Informational, not a count. Only the Gemini reader and writer touch it.
    pub traffic_type: Option<String>,
    /// Gemini/Vertex top-level `createTime` (RFC3339), carried as the wire string. Not a usage
    /// figure: it lives here because `IrUsageDetail` literals all spread `..Default::default()`,
    /// while `IrResponse` is built field-by-field at many sites. A string because converting to
    /// `IrResponse::created` (epoch seconds) would lose precision. Only the Gemini reader and writer touch it.
    pub create_time: Option<String>,
    // Cohere's `billed_units` bucket, carried so it survives a Cohere to Cohere read and write.
    // No cross-protocol analog. `None` means not reported.
    /// Cohere `usage.billed_units.input_tokens`, distinct from the raw `usage.tokens.input_tokens`.
    pub billed_input_tokens: Option<u64>,
    /// Cohere `usage.billed_units.output_tokens`, distinct from the raw `usage.tokens.output_tokens`.
    pub billed_output_tokens: Option<u64>,
    /// Cohere `usage.billed_units.classifications`, a separately billed unit, not a token count.
    pub billed_classifications: Option<u64>,
    // Usage-identity cross-check.
    /// A reported disagreement between the provider's stated total and the sum of the decoded
    /// buckets (Gemini's `totalTokenCount` is the case). `None` when the provider states no total
    /// or the buckets reconcile. Nothing is zeroed or back-filled to close the sum; the discrepancy
    /// travels beside the buckets. `billable_tokens` ignores it.
    pub usage_identity_note: Option<UsageIdentityNote>,
    /// Counts under an open class the reader names (Bedrock guardrail policy units), keyed by class
    /// ([`crate::codec::ir::open_class`]). Billed via [`busbar_contract::billing::TokenUsage::open_units`].
    pub open_units: std::collections::BTreeMap<String, u64>,
    /// Counts no billing class records, keyed by the provider's count name. Never billed
    /// ([`busbar_contract::billing::TokenUsage::residual_units`]). No reader uses it today.
    pub residual_units: std::collections::BTreeMap<String, u64>,
    /// Tokens by modality (OpenAI `*_tokens_details`, Gemini `*TokensDetails[]`). Presentation only:
    /// no meter class, no ledger, no billed figure.
    pub by_modality: Option<IrUsageByModality>,
}

/// Token counts by modality, each a slice of a total in [`IrUsage`], never an addition to it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IrUsageByModality {
    pub input: IrModalityCounts,
    pub output: IrModalityCounts,
    pub cache: IrModalityCounts,
}

/// One side's tokens by modality. `None` == the provider did not report that modality.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IrModalityCounts {
    pub text: Option<u64>,
    pub image: Option<u64>,
    pub audio: Option<u64>,
    pub video: Option<u64>,
}

/// A provider `usage` block whose bucket counts do not sum to the total it stated. Holds numbers
/// so a caller can act on it.
///
/// `unaccounted` is `reported_total - summed_total`. Positive means the provider billed tokens
/// busbar did not decode, the direction that silently costs money.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageIdentityNote {
    /// The total the provider stated (Gemini `usageMetadata.totalTokenCount`).
    pub reported_total: u64,
    /// The sum of the buckets busbar decoded from that same block.
    pub summed_total: u64,
    /// `reported_total - summed_total`.
    pub unaccounted: i64,
    /// Which identity was checked, e.g. `"gemini.usageMetadata"`.
    pub identity: &'static str,
}

impl IrUsage {
    /// Total billable tokens: `input_tokens` (uncached) + cache reads + cache creation +
    /// `output_tokens`. Provider-agnostic because readers normalize `input_tokens` to uncached.
    /// Uses `saturating_add` since the operands are upstream-controlled.
    // Production billing ledgers the tier split (`proxy::usage::tier_tokens`); this total is what a
    // provider's stated total is checked against (`usage_count::stated_total_note`).
    pub fn billable_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_input_tokens.unwrap_or(0))
            .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
            .saturating_add(self.output_tokens)
    }

    /// Project the four normalized totals into the core [`busbar_contract::billing::TokenUsage`],
    /// so billing and metering need not name this IR type. Attribution buckets are not carried.
    ///
    /// The raw token totals are what a key is charged, for every dialect, including Cohere (only
    /// its `tokens` bucket is billed, buffered and streamed alike). Cohere's billed counts stay on
    /// `IrUsageDetail` as attribution. Its `search_units` and `classifications` are open classes
    /// (see `open_units`).
    pub fn to_token_usage(&self) -> busbar_contract::billing::TokenUsage {
        busbar_contract::billing::TokenUsage {
            input: self.input_tokens,
            output: self.output_tokens,
            cache_read: self.cache_read_input_tokens,
            cache_creation: self.cache_creation_input_tokens,
            open_units: self.open_units(),
            residual_units: self.residual_units(),
            ..Default::default()
        }
    }

    /// Separately reported counts by open class. Each is a ledger line under its meter class; the
    /// plane reports units, never a price.
    ///
    /// - `search_units`: Anthropic server-side web searches and Cohere `billed_units.search_units`.
    /// - `web_fetch_requests`: Anthropic `server_tool_use.web_fetch_requests`.
    /// - `classifications`: Cohere `billed_units.classifications`.
    /// - `unitemized_tokens`: a provider-stated total above the itemized sum
    ///   ([`IrUsageDetail::usage_identity_note`] with positive `unaccounted`). A total below its
    ///   terms adds nothing; the note and WARN still say so.
    /// - every class in [`IrUsageDetail::open_units`].
    ///
    /// Zero counts are not carried.
    fn open_units(&self) -> std::collections::BTreeMap<String, u64> {
        use crate::codec::ir::open_class::{
            add, CLASSIFICATIONS_CLASS, UNITEMIZED_TOKENS_CLASS, WEB_FETCH_REQUESTS_CLASS,
        };
        let mut open = std::collections::BTreeMap::new();
        for (class, n) in &self.detail.open_units {
            add(&mut open, class, *n);
        }
        let searches = (self.detail.web_search_requests.unwrap_or(0))
            .saturating_add(self.detail.search_units.unwrap_or(0));
        add(
            &mut open,
            crate::codec::ir::rerank::SEARCH_UNITS_CLASS,
            searches,
        );
        add(
            &mut open,
            WEB_FETCH_REQUESTS_CLASS,
            self.detail.web_fetch_requests.unwrap_or(0),
        );
        add(
            &mut open,
            CLASSIFICATIONS_CLASS,
            self.detail.billed_classifications.unwrap_or(0),
        );
        if let Some(note) = &self.detail.usage_identity_note {
            add(
                &mut open,
                UNITEMIZED_TOKENS_CLASS,
                u64::try_from(note.unaccounted).unwrap_or(0),
            );
        }
        open
    }

    /// Residuals, never billed: [`IrUsageDetail::residual_units`]. Every reported unit has an open
    /// class ([`Self::open_units`]), so the map is empty; it stays for a count a reader cannot class.
    fn residual_units(&self) -> std::collections::BTreeMap<String, u64> {
        self.detail.residual_units.clone()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum IrBlockMeta {
    Text,
    /// A plaintext reasoning block. `kind` (IR-17) is full reasoning vs a summary, so a Responses
    /// writer puts a summary back into `summary[]` on the stream. `None` == unknown.
    Thinking {
        kind: Option<IrThinkingKind>,
    },
    /// A streaming redacted (opaque) reasoning block, the counterpart of buffered
    /// `IrBlock::Thinking { redacted: true }`. Known at block start so a writer can pick the native
    /// shape; the bytes arrive on the following [`IrDelta::RedactedReasoningDelta`] (readers emit
    /// the pair). The Anthropic writer suppresses the plaintext seed and emits a native
    /// `redacted_thinking` start with the bytes from that delta, since Anthropic carries `data`
    /// inline on the start. The Bedrock writer treats it like `Thinking`. Writers with no redacted
    /// shape drop it.
    RedactedThinking,
    ToolUse {
        id: String,
        name: String,
    },
    Image,
}

#[derive(Debug, Clone, PartialEq)]
// Every variant is live on the egress path (`read_response_events` to `StreamTranslate::feed`).
// `enum_variant_names` is allowed because all variants mirror the wire delta-event names.
#[allow(clippy::enum_variant_names)]
pub enum IrDelta {
    TextDelta(String),
    ThinkingDelta(String),
    InputJsonDelta(String),
    SignatureDelta(String),
    /// Streamed opaque redacted-reasoning bytes (Anthropic `redacted_thinking`, Bedrock
    /// `redactedContent`). A writer that models redacted reasoning (Bedrock) re-emits it; others drop it.
    RedactedReasoningDelta(String),
    /// Streaming citations, carried as [`crate::codec::ir::IrCitation`]. The Anthropic reader maps
    /// `citations_delta` here (one per delta); the Gemini reader maps a late chunk's
    /// `citationMetadata` (possibly several). Anthropic and Gemini writers re-emit it; others do
    /// not emit it. Attached to the active text block's index.
    CitationsDelta(Vec<IrCitation>),
    /// Streaming per-token logprobs on the active text block (OpenAI `choices[].logprobs.content[]`,
    /// Gemini `logprobsResult`). OpenAI and Gemini writers re-emit it.
    LogprobsDelta(Vec<IrTokenLogprob>),
    /// IR-21 (GEM-17 stream): a whole generated [`IrBlock::Image`] or [`IrBlock::Media`] as the one
    /// delta of a block opened by `BlockStart { block: IrBlockMeta::Image }`. Buffered-to-stream
    /// synthesis emits it after the answer's other blocks so dialects without streamed media keep
    /// contiguous indices. The Gemini writer re-emits it as `inlineData` / `fileData`; every other
    /// writer drops it.
    MediaDelta(Box<IrBlock>),
}

/// Per-request decode state for stateful stream fan-out. Anthropic events are 1:1 and ignore it;
/// OpenAI's flat stream uses it to synthesize block boundaries from chunks.
#[derive(Debug, Clone, Default)]
pub struct StreamDecodeState {
    /// Far-end wire paths this stream already dropped on a TRANSLATE attempt, each warned once;
    /// read at stream end (`codec::drops::read_stream_frame`).
    pub dropped: Vec<String>,
    pub started: bool,
    pub text_block_open: bool,
    /// IR block index the Gemini reader assigned to the text block, by order of first appearance, so
    /// text and tools never collide whatever order parts arrive in. `None` until it opens. Gemini only.
    pub text_index: Option<usize>,
    /// One-way latch set once the text block is closed (`content-end`, or the first `tool-call-start`
    /// after a leading tool-plan block). The open sites gate on it so an upstream that resumes text
    /// after tools is dropped instead of emitting a duplicate `BlockStart` at one index. Cohere and
    /// Gemini readers (Gemini keeps the index so a late citation still annotates it, GEM-20).
    pub text_block_closed: bool,
    pub open_tools: std::collections::BTreeSet<usize>,
    /// Set once a reasoning delta is seen. The thinking block then takes IR index 0 and text/tool
    /// indices shift up by one (OpenAI and Gemini streaming readers).
    pub reasoning_seen: bool,
    /// Whether the reasoning Thinking block (index 0) is currently open.
    pub thinking_block_open: bool,
    /// Set once a `delta.refusal` chunk is seen. A refusal reports `finish_reason: "stop"`, so
    /// without it the terminal frame maps to `EndTurn` and the refusal signal is lost. OpenAI Chat only.
    pub refusal_seen: bool,
    /// Stop reason buffered across Bedrock's `messageStop` and `metadata` frames, so one combined
    /// `MessageDelta{stop_reason, usage}` is emitted. Bedrock reader only.
    pub pending_stop_reason: Option<IrStopReason>,
    /// Matched stop string buffered across the same two Bedrock frames as
    /// [`Self::pending_stop_reason`] (BED-11; from `additionalModelResponseFields.stop_sequence`).
    pub pending_stop_sequence: Option<String>,
    /// [`IrStopDetail`] buffered across the same two frames (IR-16, BED-10): `messageStop`'s
    /// `model_context_window_exceeded` becomes `MaxTokens` plus `ContextWindowExceeded`.
    pub pending_stop_detail: Option<IrStopDetail>,
    /// Maps each opened tool-call wire index (OpenAI `oai_idx`, Cohere `frame_idx`) to the IR block
    /// index its `BlockStart` used, for O(log n) lookup instead of scanning `open_tools`. Every key
    /// here is also in `open_tools`, but not the reverse (Cohere's `open_tools` also holds
    /// `TEXT_BLOCK_SEEN_SENTINEL`). Neither is shrunk during a stream, so close paths replay the
    /// exact opened index, except the OpenAI Chat terminal branch, which `mem::take`s both.
    ///
    /// OpenAI: text can arrive after tool calls and shifts the tool index base, so an index
    /// recomputed at close could differ from the one opened. Replaying the recorded index keeps every
    /// `BlockStop` paired with its `BlockStart`. Empty for readers that do not need the lookup.
    pub tool_ir_index: std::collections::BTreeMap<usize, usize>,
    /// One-way latch so the multi-candidate drop `warn!` fires once per stream, not per chunk
    /// (the single-candidate IR keeps `[0]`). The engine rejects `n>1` on cross-protocol routes up
    /// front, so this is defense in depth.
    pub multi_candidate_warned: bool,
    /// One-way latch so the tool-frame cap drop `warn!` fires once per stream. Set by readers that
    /// cap tool frames (Gemini today).
    pub tool_frame_cap_warned: bool,
    /// Monotone next-free IR block index for readers allocating by first appearance. Never reset:
    /// the OpenAI Chat terminal `mem::take` of `open_tools`/`tool_ir_index` clears who is open, not
    /// which slots are spent, so a chunk after finish cannot re-claim an index. OpenAI Chat only.
    pub next_ir_index: usize,
    /// How many candidate-level citation sources this stream already carried in a `CitationsDelta`.
    /// Gemini restates the full cumulative `citationSources[]` list on every chunk, so this watermark
    /// makes each chunk emit only the tail it added. Gemini only.
    pub citations_emitted: usize,
    /// Answer text streamed so far (GEM-16). Gemini states streamed citation offsets as UTF-8 byte
    /// offsets into the whole response while [`IrCitation::start_index`] is a character offset, so
    /// converting needs this text. Gemini reader only.
    pub streamed_text: String,
    /// GEM-16: character offset into [`Self::streamed_text`] where the current text block began.
    /// IR citation offsets are relative to their text block, so a later block's are shifted by this.
    pub text_block_start: usize,
    /// GEM-16: character count of [`Self::streamed_text`], so a new block's start is not a recount.
    pub streamed_chars: usize,
    /// GEM-16: byte offset of every 64th character of [`Self::streamed_text`], so a citation's byte
    /// offsets convert without walking the whole answer per chunk.
    pub streamed_char_marks: Vec<usize>,
}

impl StreamDecodeState {
    /// Claim the next free IR block index. `reasoning_seen` reserves index 0 for the thinking block
    /// (the OpenAI reader emits it with a hardcoded 0).
    pub fn claim_ir_index(&mut self) -> usize {
        let idx = self.next_ir_index.max(usize::from(self.reasoning_seen));
        self.next_ir_index = idx + 1;
        idx
    }
}

// The typed slot vocabulary: one typed carrier per concept that two or more dialects model
// natively, never a generic bag. Variant names are the concept, not a wire spelling. A slot is
// `None`/empty when the source did not carry it, and a writer emits nothing for an absent slot.

/// Requested capacity tier (IR-04): OpenAI `service_tier`, Anthropic `service_tier`, Bedrock
/// Converse `serviceTier: {type}` (BED-14).
///
/// | IR          | OpenAI Chat / Responses | Anthropic        | Bedrock Converse `serviceTier.type` |
/// |-------------|-------------------------|------------------|-------------------------------------|
/// | `Auto`      | `"auto"`                | `"auto"`         | — (omit + warn) |
/// | `Default`   | `"default"`             | `"standard_only"`| `"default"` |
/// | `Flex`      | `"flex"`                | — (not representable: omit + warn) | `"flex"` |
/// | `Scale`     | `"scale"`               | — (not representable: omit + warn) | — (omit + warn) |
/// | `Priority`  | `"priority"`            | `"auto"` (Anthropic serves priority capacity under `auto` when the org has it; closest honest ask) | `"priority"` |
///
/// Converse's `reserved` has no IR tier and is not read. Cohere v2 `priority` is a queue ordering,
/// not a tier, so it is not read here. The response-side served tier is
/// [`IrUsageDetail::service_tier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrServiceTier {
    Auto,
    Default,
    Flex,
    Scale,
    Priority,
}

impl IrServiceTier {
    /// The OpenAI-family word (`auto`/`default`/`flex`/`scale`/`priority`).
    pub fn as_str(self) -> &'static str {
        match self {
            IrServiceTier::Auto => "auto",
            IrServiceTier::Default => "default",
            IrServiceTier::Flex => "flex",
            IrServiceTier::Scale => "scale",
            IrServiceTier::Priority => "priority",
        }
    }

    /// Parse the OpenAI-family word. Unknown is `None`; the reader leaves the key in `extra`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(IrServiceTier::Auto),
            "default" => Some(IrServiceTier::Default),
            "flex" => Some(IrServiceTier::Flex),
            "scale" => Some(IrServiceTier::Scale),
            "priority" => Some(IrServiceTier::Priority),
            _ => None,
        }
    }
}

/// Output verbosity ask (IR-07): OpenAI Chat `verbosity`, Responses `text.verbosity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrVerbosity {
    Low,
    Medium,
    High,
}

impl IrVerbosity {
    pub fn as_str(self) -> &'static str {
        match self {
            IrVerbosity::Low => "low",
            IrVerbosity::Medium => "medium",
            IrVerbosity::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "low" => Some(IrVerbosity::Low),
            "medium" => Some(IrVerbosity::Medium),
            "high" => Some(IrVerbosity::High),
            _ => None,
        }
    }
}

/// Image fidelity ask (IR-08): OpenAI Chat `image_url.detail`, Responses `input_image.detail`,
/// Cohere `image_url.detail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrImageDetail {
    Auto,
    Low,
    High,
}

impl IrImageDetail {
    pub fn as_str(self) -> &'static str {
        match self {
            IrImageDetail::Auto => "auto",
            IrImageDetail::Low => "low",
            IrImageDetail::High => "high",
        }
    }

    /// An unknown word is `None`; the reader drops it with a warn rather than coercing it.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(IrImageDetail::Auto),
            "low" => Some(IrImageDetail::Low),
            "high" => Some(IrImageDetail::High),
            _ => None,
        }
    }
}

/// Role a folded system prompt was written in (IR-14). OpenAI Chat and Responses accept both
/// `system` and `developer`; other dialects have only system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrSystemRole {
    System,
    Developer,
}

impl IrSystemRole {
    pub fn as_str(self) -> &'static str {
        match self {
            IrSystemRole::System => "system",
            IrSystemRole::Developer => "developer",
        }
    }
}

/// One in-band system turn a reader folded out of the turn array into [`IrRequest::system`].
#[derive(Debug, Clone, PartialEq)]
pub struct IrSystemFold {
    /// `messages.len()` when the turn was read: the index of the IR message it came before.
    pub before: usize,
    /// The role it was written in.
    pub role: IrSystemRole,
    /// The blocks it contributed to [`IrRequest::system`].
    pub blocks: std::ops::Range<usize>,
}

impl IrSystemFold {
    /// Record a fold after its blocks were appended: `blocks_before` is `system.len()` before them.
    pub fn record(
        folds: &mut Vec<IrSystemFold>,
        before: usize,
        role: IrSystemRole,
        blocks_before: usize,
        system_len: usize,
    ) {
        folds.push(IrSystemFold {
            before,
            role,
            blocks: blocks_before..system_len,
        });
    }
}

/// An output modality the caller asks for (IR-19): OpenAI Chat `modalities`, Gemini
/// `generationConfig.responseModalities`. Produced audio/image rides the response as
/// [`IrBlock::Media`] / [`IrBlock::Image`]. Voice selection is not carried (vendor catalogues).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrModality {
    Text,
    Audio,
    Image,
}

impl IrModality {
    /// Lower-case neutral word (OpenAI spelling; Gemini upper-cases it).
    pub fn as_str(self) -> &'static str {
        match self {
            IrModality::Text => "text",
            IrModality::Audio => "audio",
            IrModality::Image => "image",
        }
    }

    /// Case-insensitive, so both the OpenAI and the Gemini spellings parse.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "text" => Some(IrModality::Text),
            "audio" => Some(IrModality::Audio),
            "image" => Some(IrModality::Image),
            _ => None,
        }
    }
}

/// A provider-hosted (server-side) tool, neutral across dialects offering the same capability
/// (IR-11). Unlike the raw [`IrTool::hosted`], this type crosses the seam.
///
/// | kind            | Anthropic                         | Responses                  | Gemini            | OpenAI Chat |
/// |-----------------|-----------------------------------|----------------------------|-------------------|-------------|
/// | `WebSearch`     | `web_search_<version>`            | `web_search` (`web_search_preview`) | `googleSearch` | `web_search_options` |
/// | `CodeExecution` | `code_execution_<version>`        | `code_interpreter` (`container:{type:"auto"}`) | `codeExecution` | — |
/// | `WebFetch`      | `web_fetch_<version>`             | —                          | `urlContext`      | — |
///
/// | `Custom`        | —                                 | `custom` (flat)            | —                 | `custom` (nested) |
///
/// The versioned Anthropic tool type is chosen by the writer (its current GA version), not carried.
#[derive(Debug, Clone, PartialEq)]
pub enum IrHostedTool {
    WebSearch(IrWebSearch),
    CodeExecution,
    WebFetch(IrWebFetch),
    /// OAI-09: an OpenAI custom tool (free-text input, optionally grammar-constrained); Chat and
    /// Responses only.
    Custom(IrCustomTool),
}

/// An OpenAI custom tool (OAI-09). `format` is the same object on both wires and rides verbatim.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IrCustomTool {
    pub name: String,
    pub description: Option<String>,
    pub format: Option<serde_json::Value>,
}

impl IrCustomTool {
    /// Read the tool's members from the object holding them (Chat `custom`, or the flat Responses
    /// tool). `None` without a non-empty `name` or when the object has a member the IR cannot hold.
    pub fn read_members(
        obj: &serde_json::Map<String, serde_json::Value>,
        extra_ok: &[&str],
    ) -> Option<Self> {
        if obj.keys().any(|k| {
            !["name", "description", "format"].contains(&k.as_str())
                && !extra_ok.contains(&k.as_str())
        }) {
            return None;
        }
        let name = obj
            .get("name")?
            .as_str()
            .filter(|n| !n.is_empty())?
            .to_string();
        let description = match obj.get("description") {
            None | Some(serde_json::Value::Null) => None,
            Some(d) => Some(d.as_str()?.to_string()),
        };
        let format = obj.get("format").filter(|f| !f.is_null()).cloned();
        Some(IrCustomTool {
            name,
            description,
            format,
        })
    }

    /// The members as a JSON object (`name`, then `description` / `format` when set).
    pub fn write_members(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut out = serde_json::Map::new();
        out.insert("name".to_string(), serde_json::json!(self.name));
        if let Some(d) = &self.description {
            out.insert("description".to_string(), serde_json::json!(d));
        }
        if let Some(f) = &self.format {
            out.insert("format".to_string(), f.clone());
        }
        out
    }
}

impl IrHostedTool {
    /// The neutral name for a drop `warn!` / `dropped_egress_controls` entry.
    pub fn kind_str(&self) -> &'static str {
        match self {
            IrHostedTool::WebSearch(_) => "web_search",
            IrHostedTool::CodeExecution => "code_execution",
            IrHostedTool::WebFetch(_) => "web_fetch",
            IrHostedTool::Custom(_) => "custom_tool",
        }
    }
}

/// Parameters of a hosted web search. A writer that cannot express a set field drops that field with
/// a warn and keeps the tool.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IrWebSearch {
    /// Anthropic `max_uses`.
    pub max_uses: Option<u32>,
    /// Anthropic `allowed_domains`, Responses `filters.allowed_domains`.
    pub allowed_domains: Vec<String>,
    /// Anthropic `blocked_domains`.
    pub blocked_domains: Vec<String>,
    /// Anthropic / Responses / Chat `user_location` (`type:"approximate"` in all three).
    pub user_location: Option<IrUserLocation>,
    /// Responses `search_context_size`, Chat `web_search_options.search_context_size`
    /// (`low`/`medium`/`high`, the [`IrVerbosity`] words).
    pub search_context_size: Option<IrVerbosity>,
}

/// Parameters of a hosted URL fetch (Anthropic `web_fetch_*`, Gemini `urlContext`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IrWebFetch {
    /// Anthropic `max_uses`.
    pub max_uses: Option<u32>,
    /// Anthropic `allowed_domains`.
    pub allowed_domains: Vec<String>,
    /// Anthropic `blocked_domains`.
    pub blocked_domains: Vec<String>,
}

/// An approximate user location for a hosted web search. Anthropic, Responses and Chat all use the
/// same `{type:"approximate", city, region, country, timezone}` members.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IrUserLocation {
    pub city: Option<String>,
    pub region: Option<String>,
    /// ISO 3166-1 alpha-2 country code.
    pub country: Option<String>,
    /// IANA timezone name.
    pub timezone: Option<String>,
}

impl IrUserLocation {
    /// The `type` every dialect stamps on a user location.
    pub const TYPE_APPROXIMATE: &'static str = "approximate";
    /// The member names, the same on every wire, in field order.
    const MEMBERS: [&'static str; 4] = ["city", "region", "country", "timezone"];

    /// Read the string members of the object that holds them (a member of another type is absent).
    pub fn read_members(obj: &serde_json::Map<String, serde_json::Value>) -> Self {
        let mut loc = Self::default();
        let slots = [
            &mut loc.city,
            &mut loc.region,
            &mut loc.country,
            &mut loc.timezone,
        ];
        for (name, slot) in Self::MEMBERS.iter().zip(slots) {
            *slot = obj.get(*name).and_then(|v| v.as_str()).map(String::from);
        }
        loc
    }

    /// The set members as a JSON object.
    pub fn write_members(&self) -> serde_json::Map<String, serde_json::Value> {
        let values = [&self.city, &self.region, &self.country, &self.timezone];
        Self::MEMBERS
            .iter()
            .zip(values)
            .filter_map(|(name, v)| Some(((*name).to_string(), serde_json::json!(v.as_ref()?))))
            .collect()
    }

    /// The flat `{type: "approximate", city, region, country, timezone}` (Anthropic, Responses).
    pub fn write_flat(&self) -> serde_json::Value {
        let mut out = self.write_members();
        out.insert(
            "type".to_string(),
            serde_json::json!(Self::TYPE_APPROXIMATE),
        );
        serde_json::Value::Object(out)
    }
}

/// A refinement of [`IrStopReason`] some dialects can state (IR-16, and the refusal category half
/// of IR-02). The coarse `stop_reason` stays authoritative and is always set to the nearest variant.
#[derive(Debug, Clone, PartialEq)]
pub enum IrStopDetail {
    /// The context window filled before the output cap (Anthropic and Bedrock
    /// `model_context_window_exceeded`). Readers set `stop_reason = MaxTokens` beside it.
    ContextWindowExceeded,
    /// A refusal with the classifier's category / explanation where the dialect states them
    /// (Anthropic `stop_details:{type:"refusal", category, explanation}`). Readers set
    /// `stop_reason = Refusal` beside it.
    Refusal {
        category: Option<String>,
        explanation: Option<String>,
    },
}

/// Whether a thinking block's text is the model's full reasoning or a summary of it (IR-17):
/// Responses `reasoning.summary[]` vs `reasoning.content[]`; Gemini thought parts and Anthropic
/// `display:"summarized"` thinking are summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrThinkingKind {
    Full,
    Summary,
}

/// Which model family issued an opaque reasoning signature (IR-18). A signature is only valid for
/// the family that minted it: an Anthropic `signature` sent to Gemini as `thoughtSignature`, or to
/// Responses as `encrypted_content`, is a foreign blob the backend rejects or misreads.
///
/// Bedrock-served Claude signatures are `Anthropic`: the family, not the transport, issued them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrSignatureOrigin {
    /// Claude (first-party Anthropic, or Claude on Bedrock).
    Anthropic,
    /// Gemini `thoughtSignature`.
    Gemini,
    /// OpenAI Responses `reasoning.encrypted_content`.
    OpenAi,
    /// A non-Claude Bedrock reasoning model's signature.
    BedrockOther,
}

/// The search-result slot's header line: `title — source`, or whichever of the two is present.
fn search_result_header(source: &str, title: &str) -> String {
    match (title.is_empty(), source.is_empty()) {
        (false, false) => format!("{title} — {source}"),
        (false, true) => title.to_string(),
        (true, false) => source.to_string(),
        (true, true) => String::new(),
    }
}
