/// Map a candidate's `groundingMetadata` to neutral [`crate::codec::ir::IrCitation`]s. It is where a
/// Google-Search-grounded answer puts its sources (`citationMetadata` is the older corpus slot and
/// is absent on a grounded answer).
#[cfg(test)]
pub(super) fn read_gemini_grounding_citations(
    candidate: &serde_json::Value,
    anchor_text: Option<&str>,
) -> Vec<crate::codec::ir::IrCitation> {
    grounding_citations_at(candidate, &Anchor::text(anchor_text), None, None)
}
