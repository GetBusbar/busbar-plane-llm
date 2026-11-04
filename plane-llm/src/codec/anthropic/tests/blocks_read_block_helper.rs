#[cfg(test)]
pub(super) fn read_block(
    block_val: &serde_json::Value,
) -> Result<crate::codec::ir::IrBlock, IrError> {
    read_block_with_origin(block_val, OriginSite::Message).map(|(block, _)| block)
}
