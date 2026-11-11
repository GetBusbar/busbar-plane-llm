<!-- fleet:header:begin (rendered by `busbar-release plugin heal` from GetBusbar/busbar-release template/ and busbar's plugins.yaml; edit it there) -->
# busbar-plane-llm

First-party signed kind:plane plugin cdylib: the llm plane, packaged as a droppable busbar plugin. Drop the signed tarball into plugins/.

| kind | alias | crate | busbar | license |
|---|---|---|---|---|
| `plane` | `llm` | `busbar-plane-llm-plugin` | 1.6.0 (pinned in `.busbar-ref`) | Apache-2.0 |

<!-- fleet:header:end -->

## What it is for

`busbar-plane-llm` is a `kind: plane` busbar plugin.

## Config

Configured under the `llm` module name.

## Build

```bash
cargo build --release -p busbar-plane-llm-plugin
```

## Tests

```bash
cargo build --locked -p busbar-plane-llm-plugin
case "$(uname -s)" in
  Darwin) plugin_image=libbusbar_plane_llm_plugin.dylib ;;
  Linux) plugin_image=libbusbar_plane_llm_plugin.so ;;
  *) echo "Set BUSBAR_PLUGIN_CDYLIB to this platform's fresh plugin image" >&2; exit 1 ;;
esac
export BUSBAR_PLUGIN_CDYLIB="$(cd "${CARGO_TARGET_DIR:-target}/debug" && pwd)/$plugin_image"
cargo test --workspace --locked
```

## License

Apache-2.0. See [LICENSE](LICENSE).

## Differences from 1.5.5

The source references below are paths and line numbers in busbar tag `v1.5.5` (`a71b21e9fa0e8a35bd0c86f53f8a79eac2974d05`). They describe intentional corrections to inherited behavior.

Gemini `thinkingBudget: 0` now retains an explicit reasoning-disable request across dialects. The destination's capability checks still apply; destinations that accept disabling reasoning receive their native disable field. v1.5.5 treated zero as an absent request (`crates/busbar/src/proto/gemini/reader.rs:549–557`), which could leave a reasoning-by-default destination thinking.

Malformed Titan and OpenAI embedding responses now fail when a vector component is nonnumeric or outside the finite `f32` range, instead of silently deleting components or producing infinite values. OpenAI also rejects missing or malformed embedding containers/items instead of manufacturing empty successful output (`crates/busbar/src/handlers/openai.rs:554–584`); valid empty data arrays remain accepted. Valid vectors retain their dimensions and values (`crates/busbar/src/handlers/bedrock.rs:229–250`, `crates/busbar/src/handlers/openai.rs:567–575`).

Gemini transcription selects audio attachments rather than allowing an image to replace the audio. Malformed Gemini speech JSON is refused rather than returned as MPEG audio (`crates/busbar/src/handlers/gemini.rs:147–170,363–370`). Multiple transcription prompt text parts are retained in order and the prompt is emitted on egress; v1.5.5 retained only the last text part and omitted the prompt from its generated request (`:171–172,201–212`). Gemini remote `fileData` attachments also preserve their explicit MIME type, which v1.5.5 discarded when constructing a URL-only source (`crates/busbar/src/proto/gemini/reader.rs:410–422`).

OpenAI transcription now preserves CR/LF in prompt form fields and plain transcripts that happen to parse as JSON scalars or non-transcript objects. The v1.5.5 implementation stripped field line breaks (`crates/busbar/src/handlers/openai.rs:283–289`) and interpreted all JSON-looking transcript bytes as structured responses (`:321–334`); text-field multipart delimiter collisions are refused instead of modifying legitimate prompt text.

Gemini schema references now resolve their full local JSON Pointer, including escaped names, and sibling validation constraints intersect instead of overwriting the referenced constraints. The v1.5.5 inliner used bare definition names and overwrote sibling keys (`crates/busbar/src/proto/gemini/mod.rs:952–1084`). Unresolvable, cyclic, or over-budget schemas produce an explicit rejecting schema; expansion now also bounds copied string/key bytes. Intersections that cannot be flattened safely retain `allOf`, which a provider may refuse rather than silently accept a weaker constraint.

Cohere rejects present malformed or out-of-range tool stream indices instead of aliasing them to index zero or 4096; genuinely absent indices retain zero compatibility (`crates/busbar/src/proto/cohere/mod.rs:93–98`). Cohere streams can continue with a new explicit text block after an earlier block ends; this corrects the permanent closed-text latch inherited from v1.5.5 (`crates/busbar/src/proto/cohere/reader.rs:540–562`) while still ignoring stray deltas without a new block start. Safety stops retain v1.5.5's v2-compatible `ERROR` response (`crates/busbar/src/proto/cohere/mod.rs:218`). Cohere responses now reject missing or wrong-shaped embedding containers and non-object chat messages instead of returning an empty success (`crates/busbar/src/handlers/cohere.rs:225–234`, `crates/busbar/src/proto/cohere/reader.rs:882–889`); independently readable reported usage remains recoverable. Cohere `usage.cached_tokens` now contributes cache-read usage and is subtracted from ordinary input, instead of being ignored (`crates/busbar/src/proto/cohere/reader.rs:964–979`).

Responses tool-call streams now emit `response.function_call_arguments.done` before `response.output_item.done`, and exhausted-credit `insufficient_quota` failures classify as billing failures. Both correct inherited v1.5.5 behavior (`crates/busbar/src/proto/openai_responses/writer.rs:727–759` and `mod.rs:398`). Multipart text streams now track both `output_index` and `content_index`, keeping separate parts distinct; malformed or out-of-range indices are refused instead of aliasing another block (`crates/busbar/src/proto/openai_responses/reader.rs:703–718`).

Buffered cross-dialect Responses failures now return an upstream-generation 502 and retain valid reported usage. In v1.5.5, `status=failed` was rejected by the reader and fell through to a generic translation 500 (`crates/busbar/src/proto/openai_responses/reader.rs:1128`, `crates/busbar/src/proxy/engine/mod.rs:427–437`).

Malformed JSON in translated upstream SSE or Bedrock eventstream payloads now terminates translation with an ingress-native stream error instead of silently dropping the frame and completing successfully (`crates/busbar/src/proto/stream.rs:676–677,776–777`). Native same-protocol pass-through retains its existing behavior.

Bedrock responses with missing or wrong-shaped message content now fail decoding instead of becoming successful empty answers (`crates/busbar/src/proto/bedrock/reader.rs:1114–1128`). Explicit invalid `inferenceConfig.maxTokens` values are refused instead of being replaced by the backend default (`:544–557`).

Hook output-cap projections now recognize OpenAI `max_completion_tokens` and Gemini `generationConfig.maxOutputTokens`; v1.5.5 checked only `max_output_tokens` for Responses and `max_tokens` otherwise (`crates/busbar/src/proxy/hooks.rs:292–300`).

Request arrival now recognizes JSON and multipart media types without regard to case and matches the JSON media-type token exactly, so valid mixed-case content types route correctly and `application/json` prefix lookalikes are not treated as JSON (`crates/busbar/src/ingress/dispatch.rs:98–100,126–127`).

Malformed OpenAI and Gemini image containers or entries now fail decoding instead of becoming empty successful images (`crates/busbar/src/handlers/openai.rs:734–758`, `crates/busbar/src/handlers/gemini.rs:449–471`). Independently reported image usage remains recoverable. Documented Imagen storage outputs retain their URI through the existing URL carrier, and filtered entries do not count as delivered images. Large same-dialect replies retain Titan input token counts, transcription usage, and validated image counts without retaining image payloads; the original reported fields are present in v1.5.5 (`crates/busbar/src/handlers/bedrock.rs:244–252`, `crates/busbar/src/handlers/openai.rs:336–339`).

## Responses cache write attribution

When `usage.input_tokens_details.cache_write_tokens_details` reports both
`ttl_30m_tokens` and `ttl_12h_tokens`, the plane validates their sum against
`cache_write_tokens` and the complete input partition. It emits `input`,
`cache_read`, `cache_write_30m`, `cache_write_12h`, and `output` exclusively.
The aggregate cache write count remains available in response totals but is not
another priced count. Configure dated operator rates for the two open classes;
the plane does not derive a rate from the TTL name.

Explicit zeros and exact fractional values survive buffered, streaming, and
recovered usage. A malformed or inconsistent split carries an explicit count
refusal and does not fall back to a cheaper aggregate tariff. Responses without
a split retain legacy aggregate semantics unless their configured model requires
the new usage contract. Declare exact model versions in the provider table:

```yaml
model_capabilities:
  - models: [cache-model-version]
    responses_cache_ttl: true
```

The first matching rule applies to the resolved upstream model name. Enabling
this flag on a non-Responses lane is refused. A declared model must report both
write counts, including explicit zeros, even when the caller sends no cache
controls. Missing splits remain unresolved; a response model name cannot weaken
the requirement. This applies to buffered, streamed, translated and cut replies.

Declared native Responses lanes preserve `prompt_cache_options` (`ttl: 30m|12h`,
`mode: implicit|explicit`, `prewarm: boolean`) and explicit content/tool-output
breakpoints with optional TTL overrides. Invalid values, undeclared model
support, and cross-protocol cache controls are refused before dispatch. These
controls never silently become another provider's cache settings. Legacy native
`prompt_cache_retention` remains separate and cannot be combined with the new
model contract. Prices always follow reported usage, not the requested TTL.

The split classes belong to the existing token family and declare `cache_write`
as their parent through neutral host metadata. Host cap/reporting projections
include both children in that reserved total; raw metering and pricing retain
only the disjoint components. Historical class indices, including fee index 19,
are preserved. Hosted-tool settlement remains a separate evidence integration.

## Native Interactions and observation accounting

`observations::interactions` provides a native operation table and validates
stored model creates. Its final-response/SSE adapter keeps input, cache reads,
output, thoughts and explicitly priced tool-use prompts separate. Missing usage,
changed models, unsupported modalities or tariffs remain unresolved. Event-stream
cursors are not accounting revisions. Unknown provider IDs are unattributed.

`observations::lifecycle` produces exact conditional-record publications using
the host SDK: an absent-only intent before paid dispatch, an atomic provider-ID
index and frozen run binding, and persistent final observations/receipt progress.
The host's original scope carries ownership and prices. A restored intent with
no provider ID never authorizes another create. Content deletion preserves the
accounting record, and scope close requires all declared final components to
have non-quarantined host receipts. Native serving retains these records and their
protected usage facts; it does not delete them when upstream content is deleted.

`observations::hosted` normalizes authenticated, typed supplemental evidence for
OpenAI container intervals, Anthropic execution duration and image-tool leaf
models. Configuration supplies dated tariff variants, rate bindings, explicit
duration rules and allowance policy; the adapter contains no monetary defaults.
A container ID alone is insufficient. Automatic hosted settlement is not enabled.
The [Responses reference](https://developers.openai.com/api/reference/resources/responses/methods/create)
exposes image usage as `tool_usage.image_gen`, which can skip incomplete call
records and omit cache or output detail. Image result items provide no per-call
image usage. Consequently, that aggregate alone cannot establish the complete
per-call manifest required here, even when some totals are present. The
[image guide](https://developers.openai.com/api/docs/guides/image-generation)
also describes billed cache hits that are absent from response usage. No missing
cache count is converted to zero, and no aggregate is assigned to an invented
leaf-call record.

Container/execution settlement additionally needs authoritative dated interval,
eligibility and duration evidence, and custody for an interval distinct from the
parent response. The current live path supplies neither that complete evidence
nor multiple independent economic scopes within one unit. Typed supplemental
fixtures do not establish a live evidence acquisition path. Native Interactions
requests with hosted tools are refused before scope creation or paid dispatch;
ordinary existing routes do not opt into exact hosted settlement through this
adapter API.

Native create/get/cancel/delete and create/get streaming are opt-in through the LLM-owned
`interactions` section. For each concrete Gemini model entry, configure
`interactions.models.<entry>.account_namespace` and
`interactions.models.<entry>.pricing_modes: [standard, flex, priority]` (enable only the tiers supported for that model/account). Ordinary routes do not
require a conditional-record backend merely because this feature exists.

The served plane also needs the host-owned `usage_observations.<entry>` policy:
a matching stable `accounting_namespace`, `classes: [input, output, cache_read]`,
a positive `reserve_nanos`, and explicit dated rates for those classes on the
serving subject. Native create requires host proof of all three priced classes
and durable create-intent and recovery-work receipts before emitting any upstream request head.
No provider token counts are also emitted through ordinary request metering.

Stored IDs resolve only after authentication through the original protected
principal/account index, then ordinary destination authorization and original
host-scope attachment. Unknown, ambiguous or changed-account IDs are refused.
Continuations require the same protected predecessor lookup and original
route/model/account; each continuation creates a new paid run and new scope.
The host preserves authenticated caller-reference continuity across signing-key rotation;
the plugin never scans other principals or migrates ownership itself. Pre-existing
indices from before host continuity was established still need host-proven migration.

The native profile supports buffered and streamed ordinary text model requests,
including background creation followed by request-driven polling and stream
replay with an opaque `last_event_id` cursor. It refuses agents, hosted tools,
multimodal controls and unsupported request fields before paid dispatch. A
provider response with missing usage or unsupported final steps remains
unresolved; it never fabricates zero usage or closes the reservation. Buffered responses and individual retained stream buffers are limited to 1 MiB.
Successful native delivery preserves original response/event bytes. Stream IDs
are durably bound before forwarding; terminal events wait for clean EOF and
accounting settlement. A replay needs a complete final snapshot to establish
usage; partial history remains unresolved. Live streams may establish the text
profile from a complete observed step sequence. Neither cursors nor local event
counts become billing revisions. A lost create with no recorded provider ID is retained as an
uncertain intent and never automatically resubmitted. Changed final totals retain
new protected evidence and submit the same economic keys for host quarantine;
no provider revision is invented and quarantined components cannot close a scope.

Flex and Priority use dedicated token classes, appended without renumbering existing
classes. Configure the three relevant names in both `usage_observations.<entry>.classes`
and the serving model's `rate_card.<model>.units`; rates are operator supplied:

| Tier | Input | Output and thinking | Cached input |
| --- | --- | --- | --- |
| Flex | `interactions_flex_input_tokens` (22) | `interactions_flex_output_tokens` (23) | `interactions_flex_cache_read_tokens` (24) |
| Priority | `interactions_priority_input_tokens` (25) | `interactions_priority_output_tokens` (26) | `interactions_priority_cache_read_tokens` (27) |

Each class rolls up to its ordinary token parent for reporting. Original scope
reattachment proves the frozen tier classes again; configuration changes never
substitute standard prices. Nonstandard final usage must explicitly confirm the
original `service_tier`. No automatic create retry or tier upgrade occurs, including
Flex capacity errors. These semantics follow the provider's [Flex](https://ai.google.dev/gemini-api/docs/flex-inference)
and [Priority](https://ai.google.dev/gemini-api/docs/priority-inference) specifications.

For new runs, the host owns autonomous recovery. Before paid dispatch the plugin
atomically stores an original-principal intent index and registers durable work
for `GET /_busbar/llm/interactions/<host-random-intent>`. The host restores the
original principal and rechecks current standing before polling. The plugin reads
its protected intent, reattaches the original scope, and retrieves only the recorded
provider ID. The host work reference must resolve under that principal and name the
exact recovery target; it is stripped from upstream headers. Work is settled only
after final observations, their receipts, scope closure, and the closed checkpoint
are durable. A lost create without a recorded provider ID remains unresolved and
cannot trigger a replacement create. Historical runs predating work registration
retain request-driven recovery; this version does not invent background jobs for them.

Anthropic `usage.server_tool_use.code_execution_requests` is reported through the
ordinary unit-count path as `code_execution_requests` (stable class index 28,
family `count`). Buffered replies and SSE usage frames retain the provider's
nonnegative integer quantity beside independently reported token/search/fetch
counts. An absent or null execution counter stays unknown; it is not zero seconds.
Malformed counts are refused without discarding valid token observations.

The plugin declares and measures units. The host applies the selected rate card,
including an explicitly zero rate, and handles missing rates. The plugin neither
reads prices nor decides request exemptions, minimum durations, or organization
allowances. There is no execution-policy setting or invoice-completeness admission
gate. Execution, image-generation, and container requests follow the ordinary
protocol path; reported quantities do not assert complete provider invoice coverage.

The broader hosted module is still a typed adapter with no production Anthropic
interval-settlement caller; it is not automatic image/container invoice settlement.
Complete authenticated interval/image acquisition remains unresolved. Available
call identities, requested image models, and aggregate cost quantities do not by
themselves supply the missing complete accounting records.
The exact public-provider evidence limits checked on 2026-11-10 are:

| Provider path checked | What it supplies | Missing for exact hosted settlement |
| --- | --- | --- |
| [OpenAI Responses retrieve](https://developers.openai.com/api/reference/go/resources/responses/methods/retrieve) | Aggregate `tool_usage.image_gen` when available | Calls with incomplete usage may be skipped, cache/output detail may be omitted, and no complete per-call leaf manifest is guaranteed. |
| [OpenAI container retrieve](https://developers.openai.com/api/reference/resources/containers/methods/retrieve) | Container identity, creation/last-active time, expiration policy, memory tier | Economic interval identity, eligible-minute classification, authoritative billed duration, and interval ownership across requests. Expiration policy is not a billing measurement. |
| [OpenAI code-interpreter usage](https://developers.openai.com/api/reference/resources/admin/subresources/organization/subresources/usage/methods/code_interpreter_sessions) and [costs](https://developers.openai.com/api/reference/resources/admin/subresources/organization/subresources/usage/methods/costs) | Bucketed project session counts; daily cost/quantity groups | No container/session/request identity joining those aggregates to an original caller scope. Account totals cannot be allocated speculatively. |
| [Anthropic code execution](https://platform.claude.com/docs/en/agents-and-tools/tool-use/code-execution-tool) | Request count and documented request-level exemptions | Duration and authoritative organization allowance allocation for each economic interval; preloading can accrue time without a call. |
| [Anthropic analytics costs](https://platform.claude.com/docs/en/api/beta/organization/analytics/cost/list) | Aggregated cost dimensions, including Managed Agents session IDs | The documented session IDs refer to Managed Agents, not Messages code-execution containers, and do not provide their execution interval or allowance allocation. |

Completing general interval and raster-image settlement requires authenticated
provider evidence with complete
call or interval identity, usage and allowance facts, plus host-issued interval
custody. None was established by the public schemas above. No paid probe was made,
no account aggregate was assigned to a caller, and refusal is not counted as completion
of these features.

Hook request projections now attest text completeness only after checking the
concrete normalized chat request. Media, tools, unsupported structured content,
remote history and raw fallback never receive that attestation.
