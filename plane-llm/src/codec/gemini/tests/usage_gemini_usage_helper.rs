/// [`read_gemini_usage`] for a fixture the test already knows carries readable counts. Test-only:
#[cfg(test)]
pub(super) fn gemini_usage(data: &serde_json::Value) -> crate::codec::ir::IrUsage {
    read_gemini_usage(data).expect("test fixture carries readable usage counts")
}
