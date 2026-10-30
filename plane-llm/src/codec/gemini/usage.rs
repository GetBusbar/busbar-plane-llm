//! Gemini `usageMetadata` → [`crate::codec::ir::IrUsage`].

use super::*;
use crate::codec::usage_count::{read_usage, CountRead, CountSlot, UsageCount};

/// Gemini's usage counts, as data. `cachedContentTokenCount` is a slice inside `promptTokenCount`, so
/// it is subtracted (saturating) there and carried as `cache_read_input_tokens`, as Bedrock's and
/// Anthropic's cache reads are. `toolUsePromptTokenCount` is additive beside the prompt (see
/// [`GEMINI_USAGE_ADDITIVE_TERMS`]: 32 tool-use tokens against an 18-token prompt) and counted at the
/// input rate, so it is added to `input_tokens` and recorded as attribution. Thinking tokens are
/// output: `candidatesTokenCount` counts only the visible answer and 2.5-series reasoning arrives in
/// the additive `thoughtsTokenCount` (Google's `totalTokenCount` is prompt + candidates + thoughts),
/// so it is added to `output_tokens` (what it means for every other provider) and recorded as the
/// reasoning sub-bucket; the writer writes the whole output as `candidatesTokenCount`. Audio entries
/// of the per-modality lists are attribution slices.
///
/// The ledger records what Google reports. Every count the pinned wire lock
/// (`testing/llm-conformance/wire/gemini.wire.json`) declares under `usageMetadata` is itemized into
/// an existing meter class: prompt to input, `cachedContentTokenCount` to cache read (out of the
/// prompt), `toolUsePromptTokenCount` to input, candidates and `thoughtsTokenCount` to output.
/// `totalTokenCount` is Google's sum, not a unit, and changes no itemized count; a total above the
/// parts is the open class `unitemized_tokens`, which `gemini_usage_identity_note` WARNs naming. The
/// plane decides units only; the rate card prices them. Streaming frames, the buffered response and
/// truncated-body recovery read this one table, so all count alike.
pub(super) const USAGE: &[UsageCount] = &[
    (
        CountSlot::Input,
        CountRead::Zero(&[FIELD_PROMPT_TOKEN_COUNT]),
    ),
    (
        CountSlot::Input,
        CountRead::Less(&[FIELD_CACHED_CONTENT_TOKEN_COUNT]),
    ),
    (
        CountSlot::Input,
        CountRead::More(&[FIELD_TOOL_USE_PROMPT_TOKEN_COUNT]),
    ),
    (
        CountSlot::Output,
        CountRead::Zero(&[FIELD_CANDIDATES_TOKEN_COUNT]),
    ),
    (
        CountSlot::Output,
        CountRead::More(&[FIELD_THOUGHTS_TOKEN_COUNT]),
    ),
    (
        CountSlot::CacheRead,
        CountRead::Opt(&[FIELD_CACHED_CONTENT_TOKEN_COUNT]),
    ),
    (
        CountSlot::Reasoning,
        CountRead::Opt(&[FIELD_THOUGHTS_TOKEN_COUNT]),
    ),
    (
        CountSlot::ToolUsePrompt,
        CountRead::Opt(&[FIELD_TOOL_USE_PROMPT_TOKEN_COUNT]),
    ),
    (
        CountSlot::InputAudio,
        CountRead::ListFirst {
            list: &[MODALITY_LISTS[0]],
            key: FIELD_MODALITY,
            value: GEMINI_AUDIO,
            count: FIELD_TOKEN_COUNT,
        },
    ),
    (
        CountSlot::OutputAudio,
        CountRead::ListFirst {
            list: &[MODALITY_LISTS[1]],
            key: FIELD_MODALITY,
            value: GEMINI_AUDIO,
            count: FIELD_TOKEN_COUNT,
        },
    ),
];

/// A Gemini response (or stream frame) to IR usage: `usageMetadata` through [`USAGE`], plus what
/// rides beside the counts: the identity cross-check against Google's `totalTokenCount` (`None` when
/// they agree), `usageMetadata.trafficType` (informational, ON_DEMAND vs PROVISIONED) and the
/// top-level `createTime` (a sibling of `usageMetadata`).
pub(super) fn read_gemini_usage(
    data: &serde_json::Value,
) -> Result<crate::codec::ir::IrUsage, IrError> {
    let u = data.get(FIELD_USAGE_METADATA);
    let mut usage = read_usage(COUNT_LABEL, u, USAGE)?;
    usage.detail.usage_identity_note = gemini_usage_identity_note(u);
    usage.detail.traffic_type = u
        .and_then(|u| u.get(FIELD_TRAFFIC_TYPE))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    usage.detail.create_time = read_gemini_create_time(data);
    usage.detail.by_modality = u.and_then(read_by_modality);
    // The tier that served the turn.
    usage.detail.service_tier = u
        .and_then(|u| u.get(FIELD_SERVICE_TIER))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(usage)
}

/// The `usageMetadata` member naming the tier that served the turn.
pub(super) const FIELD_SERVICE_TIER: &str = "serviceTier";

/// The per-modality detail lists of `usageMetadata`, in the IR's input / output / cache order.
pub(super) const MODALITY_LISTS: [&str; 3] = [
    "promptTokensDetails",
    "candidatesTokensDetails",
    "cacheTokensDetails",
];

/// The `modality` words of a detail entry other than audio ([`GEMINI_AUDIO`]).
const MODALITY_TEXT: &str = "TEXT";
const MODALITY_IMAGE: &str = "IMAGE";
const MODALITY_VIDEO: &str = "VIDEO";

/// `usageMetadata.{prompt,candidates,cache}TokensDetails[]{modality,tokenCount}` -> the IR's
/// by-modality split (presentation only, never billed). `None` when no list is present.
fn read_by_modality(u: &serde_json::Value) -> Option<crate::codec::ir::IrUsageByModality> {
    if !MODALITY_LISTS.iter().any(|l| u.get(*l).is_some()) {
        return None;
    }
    let side = |list: &str| {
        let mut c = crate::codec::ir::IrModalityCounts::default();
        for e in u.get(list).and_then(|l| l.as_array()).into_iter().flatten() {
            let n = e.get(FIELD_TOKEN_COUNT).and_then(|v| v.as_u64());
            match e.get(FIELD_MODALITY).and_then(|v| v.as_str()) {
                Some(MODALITY_TEXT) => c.text = n,
                Some(MODALITY_IMAGE) => c.image = n,
                Some(GEMINI_AUDIO) => c.audio = n,
                Some(MODALITY_VIDEO) => c.video = n,
                _ => {}
            }
        }
        c
    };
    let [input, output, cache] = MODALITY_LISTS.map(side);
    Some(crate::codec::ir::IrUsageByModality {
        input,
        output,
        cache,
    })
}

/// The IR's by-modality split as Gemini's three detail lists (each list only when it has a count).
pub(super) fn write_by_modality(
    m: &crate::codec::ir::IrUsageByModality,
    out: &mut serde_json::Map<String, serde_json::Value>,
) {
    for (list, c) in MODALITY_LISTS.iter().zip([&m.input, &m.output, &m.cache]) {
        let entries: Vec<serde_json::Value> = [
            (MODALITY_TEXT, c.text),
            (MODALITY_IMAGE, c.image),
            (GEMINI_AUDIO, c.audio),
            (MODALITY_VIDEO, c.video),
        ]
        .into_iter()
        .filter_map(|(modality, n)| {
            n.map(|n| serde_json::json!({ (FIELD_MODALITY): modality, (FIELD_TOKEN_COUNT): n }))
        })
        .collect();
        if !entries.is_empty() {
            out.insert((*list).to_string(), serde_json::Value::Array(entries));
        }
    }
}

/// [`read_gemini_usage`] for a fixture the test already knows carries readable counts. Test-only:
/// production goes through the refusing form, never through a read that could default a count.
#[cfg(test)]
pub(super) fn gemini_usage(data: &serde_json::Value) -> crate::codec::ir::IrUsage {
    read_gemini_usage(data).expect("test fixture carries readable usage counts")
}

// Usage counts.

/// This dialect's label on a refused usage count.
pub(super) const COUNT_LABEL: &str = "gemini";
