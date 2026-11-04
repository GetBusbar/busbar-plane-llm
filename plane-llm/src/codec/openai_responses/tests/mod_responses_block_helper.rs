#[cfg(test)]
fn responses_block(block_val: &serde_json::Value) -> Result<crate::codec::ir::IrBlock, IrError> {
    responses_block_with_origin(block_val, origin::PartSite::Message).map(|(block, _)| block)
}
