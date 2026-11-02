// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The `Protocol::<dialect>()` TEST FIXTURE constructors: production resolves a dialect through

use super::*;
use crate::codec::bedrock::{BedrockReader, BedrockWriter};
use crate::codec::cohere::{CohereReader, CohereWriter};
use crate::codec::gemini::{GeminiReader, GeminiWriter};
use crate::codec::openai_chat::{OpenAiReader, OpenAiWriter};
use crate::codec::openai_responses::{ResponsesReader, ResponsesWriter};

impl Protocol {
    /// Construct an Anthropic protocol instance — TEST FIXTURE SHIM. The dialect is an extracted
    pub fn anthropic() -> Self {
        Self::new(
            PROTO_ANTHROPIC,
            crate::codec::anthropic::AnthropicReader,
            crate::codec::anthropic::AnthropicWriter,
        )
    }

    /// Construct an OpenAI protocol instance — TEST FIXTURE SHIM, same rationale as
    pub fn openai() -> Self {
        Self::new(PROTO_OPENAI, OpenAiReader, OpenAiWriter)
    }

    /// Construct a Gemini protocol instance — TEST FIXTURE SHIM, same rationale as
    pub fn gemini() -> Self {
        Self::new(PROTO_GEMINI, GeminiReader, GeminiWriter)
    }

    /// Construct an OpenAI Responses protocol instance — TEST FIXTURE SHIM, same rationale as
    pub fn responses() -> Self {
        Self::new(PROTO_RESPONSES, ResponsesReader, ResponsesWriter)
    }

    /// Construct a Bedrock protocol instance — TEST FIXTURE SHIM, same rationale as
    pub fn bedrock() -> Self {
        Self::new(PROTO_BEDROCK, BedrockReader, BedrockWriter)
    }

    /// Construct a Cohere (v2 chat) protocol instance — TEST FIXTURE SHIM, same rationale as
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
