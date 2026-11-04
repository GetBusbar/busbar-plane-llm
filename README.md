<!-- fleet:header:begin (rendered by `busbar-release plugin heal` from GetBusbar/busbar-release template/ and busbar's plugins.yaml; edit it there) -->
# busbar-plane-llm

First-party signed kind:plane plugin cdylib: the llm plane, packaged as a droppable busbar plugin. Drop the signed tarball into plugins/.

| kind | alias | crate | busbar | license |
|---|---|---|---|---|
| `plane` | `llm` | `busbar-plane-llm-plugin` | 1.6.0 (pinned in `.busbar-ref`) | Apache-2.0 |

[![ci](https://github.com/GetBusbar/busbar-plane-llm/actions/workflows/ci.yml/badge.svg?branch=dev)](https://github.com/GetBusbar/busbar-plane-llm/actions/workflows/ci.yml)
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
