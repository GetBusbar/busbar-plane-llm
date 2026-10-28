// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The `Protocol::<dialect>()` TEST FIXTURE constructors: production resolves a dialect through
//! the registry ([`super::protocol_for`]); the fixture surface names it directly.

use super::*;
use crate::codec::bedrock::{BedrockReader, BedrockWriter};
use crate::codec::cohere::{CohereReader, CohereWriter};
use crate::codec::gemini::{GeminiReader, GeminiWriter};
use crate::codec::openai_chat::{OpenAiReader, OpenAiWriter};
use crate::codec::openai_responses::{ResponsesReader, ResponsesWriter};

impl Protocol {
    /// Construct an Anthropic protocol instance — TEST FIXTURE SHIM. The dialect is an extracted
    /// module of the LLM plugin crate (`busbar-llm`); production resolves it through the registry after the
    /// composition root installs its declaration, and core has no production path that could name
    /// it. The pre-extraction fixture surface calls this constructor by name in hundreds of tests,
    /// so it survives for the builds that compile the dialect back in (see the `mod anthropic`
    /// decl) rather than rewriting every fixture to a registry lookup in the extraction commit.
    pub fn anthropic() -> Self {
        Self::new(
            PROTO_ANTHROPIC,
            crate::codec::anthropic::AnthropicReader,
            crate::codec::anthropic::AnthropicWriter,
        )
    }

    /// Construct an OpenAI protocol instance — TEST FIXTURE SHIM, same rationale as
    /// [`Protocol::anthropic`]: the dialect is a module of the LLM plugin crate (`busbar-llm`);
    /// production resolves it through the registry after the composition root installs its
    /// declaration.
    pub fn openai() -> Self {
        Self::new(PROTO_OPENAI, OpenAiReader, OpenAiWriter)
    }

    /// Construct a Gemini protocol instance — TEST FIXTURE SHIM, same rationale as
    /// [`Protocol::anthropic`]: the dialect is a module of the LLM plugin crate (`busbar-llm`);
    /// production resolves it through the registry after the composition root installs its
    /// declaration.
    pub fn gemini() -> Self {
        Self::new(PROTO_GEMINI, GeminiReader, GeminiWriter)
    }

    /// Construct an OpenAI Responses protocol instance — TEST FIXTURE SHIM, same rationale as
    /// [`Protocol::anthropic`]: the dialect is a module of the LLM plugin crate (`busbar-llm`);
    /// production resolves it through the registry after the composition root installs its
    /// declaration.
    pub fn responses() -> Self {
        Self::new(PROTO_RESPONSES, ResponsesReader, ResponsesWriter)
    }

    /// Construct a Bedrock protocol instance — TEST FIXTURE SHIM, same rationale as
    /// [`Protocol::anthropic`]: the dialect is a module of the LLM plugin crate (`busbar-llm`).
    pub fn bedrock() -> Self {
        Self::new(PROTO_BEDROCK, BedrockReader, BedrockWriter)
    }

    /// Construct a Cohere (v2 chat) protocol instance — TEST FIXTURE SHIM, same rationale as
    /// [`Protocol::anthropic`]: the dialect is a module of the LLM plugin crate (`busbar-llm`).
    pub fn cohere() -> Self {
        Self::new(PROTO_COHERE, CohereReader, CohereWriter)
    }
}

impl ToolIdRemap {
    /// Number of memoized pairs currently retained — the retention the cap bounds.
    pub fn memo_len(&self) -> usize {
        self.map.len()
    }
}
