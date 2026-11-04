/// A fresh writer as a value, for one-shot `write_request` / `write_response` calls in tests.
#[cfg(test)]
pub(crate) fn anthropic_writer() -> AnthropicWriter {
    AnthropicWriter
}
