//! What this plane declares about itself: its key, its operation classes, its `busbar --help`
//! rows and its provider keys. The door ([`crate::plane_door`]) states them.

use busbar_contract::ids::OpClassId;

/// The plane's registry key.
pub const KEY: &str = "llm";

/// The operation classes a unit of this plane can be.
///
/// These are the classes that PRICE a unit, so the list is the one the previous release billed
/// against and no wider. The draft names one at the decode step and the audit step is checked
/// against it.
pub const OP_CLASSES: &[OpClassId] = &[
    OpClassId::new("chat"),
    OpClassId::new("embeddings"),
    OpClassId::new("moderation"),
    OpClassId::new("image"),
    OpClassId::new("transcription"),
    OpClassId::new("speech"),
    OpClassId::new("rerank"),
];

/// [`HELP_TAGLINE`]'s text, spelled once for it and for [`CLI_HELP_TEXT`].
macro_rules! help_tagline {
    () => {
        "native-protocol LLM gateway"
    };
}

/// The one-line description `busbar --help` opens with (`busbar <version> — <this>`), byte for byte
/// as the binary has always printed it (shadow-oracle cells `cli__--help` / `cli__-h`). It names
/// this plane, so this plane owns it (#47/#49) and the composition root reads it from here.
pub const HELP_TAGLINE: &str = help_tagline!();

/// [`HELP_ENDPOINTS`]'s text, spelled once for it and for [`CLI_HELP_TEXT`].
macro_rules! help_endpoints {
    () => {
        "    POST /<model>/v1/messages              Anthropic-format ingress (single model)
    POST /<pool>/v1/messages               route to a configured pool
    POST /<provider>/<model>/v1/messages   ad-hoc direct route
    POST /v1/chat/completions              OpenAI-format ingress
    POST /v2/chat                          Cohere-format ingress
    POST /v1/responses                     Responses-API ingress
    POST /v1/models/<model>:<action>       Gemini-format ingress (stable v1)
    POST /v1beta/models/<model>:<action>   Gemini-format ingress
    POST /model/<modelId>/converse         Bedrock Converse ingress
    POST /model/<modelId>/converse-stream  Bedrock Converse streaming ingress
    GET  /v1/models  /v1beta/models        list models (answers in the caller's dialect)"
    };
}

/// This plane's lines in the `ENDPOINTS` block of `busbar --help` — the doors its dialects answer
/// on — byte for byte as the binary has always printed them (shadow-oracle cells `cli__--help` /
/// `cli__-h`). They name this plane's dialects, so this plane owns them (#47/#49); no trailing
/// newline, the root splices them between two lines it owns.
pub const HELP_ENDPOINTS: &str = help_endpoints!();

/// THE SAME ROWS AS THE DOOR'S TAIL STATES THEM (`PlaneTail::cli_help`), in the contract's grammar
/// (`busbar_contract::abi::plane::cli_help`): the tagline row, then the `ENDPOINTS` row. The host
/// reads them off the door, so a dropped-in build of this plane prints the same help as a linked one.
pub const CLI_HELP_TEXT: &str = concat!(
    "\u{1e}tagline\u{1f}",
    help_tagline!(),
    "\u{1e}endpoint\u{1f}",
    help_endpoints!()
);

/// THE PROVIDER KEYS THIS PLANE OWNS, per form of a `providers:` entry (row 155 B1c, ARCHITECT
/// ruling "Option A"): the descriptor this plane states in its Statement's extensions under
/// `busbar_contract::provider_keys::EXT_PROVIDER_KEYS`, in the contract's neutral shape vocabulary,
/// so the kernel parses an entry without holding one of these names.
///
/// Each form's keys are in the order of the previous release's frozen grammar for that form (the
/// catalog definition, the operator deployment, the whole resolved entry), with the kernel's own keys
/// left out; each says whether it must be written, and each form whether it refuses a key it does not
/// name. No form refuses one here: a key that is not busbar's is a provider option, judged by the
/// plane against the list of the provider's dialect ([`crate::options`]), which refuses one on no
/// list in its own words. Read by nothing yet: the kernel
/// keeps it at its door fold.
pub const PROVIDER_KEYS: &str = r#"{
  "catalog": {
    "deny_unknown": false,
    "keys": [
      {"name": "protocol", "shape": "str", "required": true},
      {"name": "error_map", "shape": "str-map", "required": false},
      {"name": "max_output_key", "shape": {"one-of": ["max_tokens", "max_completion_tokens"]}, "required": false},
      {"name": "anthropic_adaptive_thinking", "shape": "bool", "required": false},
      {"name": "native_structured_output", "shape": "bool", "required": false},
      {"name": "model_capabilities", "shape": {"list": {"table": {
        "deny_unknown": true,
        "keys": [
          {"name": "models", "shape": {"list": "str"}, "required": true},
          {"name": "max_output_key", "shape": {"one-of": ["max_tokens", "max_completion_tokens"]}, "required": false},
          {"name": "anthropic_adaptive_thinking", "shape": "bool", "required": false},
          {"name": "native_structured_output", "shape": "bool", "required": false},
          {"name": "reasoning_none", "shape": "bool", "required": false},
          {"name": "thinking_always_on", "shape": "bool", "required": false}
        ]
      }}}, "required": false}
    ]
  },
  "deployment": {
    "deny_unknown": false,
    "keys": [
      {"name": "protocol", "shape": "str", "required": false},
      {"name": "error_map", "shape": "str-map", "required": false},
      {"name": "max_output_key", "shape": {"one-of": ["max_tokens", "max_completion_tokens"]}, "required": false},
      {"name": "anthropic_adaptive_thinking", "shape": "bool", "required": false},
      {"name": "native_structured_output", "shape": "bool", "required": false},
      {"name": "model_capabilities", "shape": {"list": {"table": {
        "deny_unknown": true,
        "keys": [
          {"name": "models", "shape": {"list": "str"}, "required": true},
          {"name": "max_output_key", "shape": {"one-of": ["max_tokens", "max_completion_tokens"]}, "required": false},
          {"name": "anthropic_adaptive_thinking", "shape": "bool", "required": false},
          {"name": "native_structured_output", "shape": "bool", "required": false},
          {"name": "reasoning_none", "shape": "bool", "required": false},
          {"name": "thinking_always_on", "shape": "bool", "required": false}
        ]
      }}}, "required": false}
    ]
  },
  "entry": {
    "deny_unknown": false,
    "keys": [
      {"name": "protocol", "shape": "str", "required": false},
      {"name": "error_map", "shape": "str-map", "required": true},
      {"name": "max_output_key", "shape": {"one-of": ["max_tokens", "max_completion_tokens"]}, "required": false},
      {"name": "anthropic_adaptive_thinking", "shape": "bool", "required": false},
      {"name": "native_structured_output", "shape": "bool", "required": false},
      {"name": "model_capabilities", "shape": {"list": {"table": {
        "deny_unknown": true,
        "keys": [
          {"name": "models", "shape": {"list": "str"}, "required": true},
          {"name": "max_output_key", "shape": {"one-of": ["max_tokens", "max_completion_tokens"]}, "required": false},
          {"name": "anthropic_adaptive_thinking", "shape": "bool", "required": false},
          {"name": "native_structured_output", "shape": "bool", "required": false},
          {"name": "reasoning_none", "shape": "bool", "required": false},
          {"name": "thinking_always_on", "shape": "bool", "required": false}
        ]
      }}}, "required": false}
    ]
  }
}"#;
