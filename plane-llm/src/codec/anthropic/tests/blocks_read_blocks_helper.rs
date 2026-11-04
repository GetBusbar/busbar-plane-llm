/// Read a content-block array into the IR: every block [`is_read_into_ir`] keeps, in order. A
/// `document` with no `source` goes through the one drop path; an unmodelled kind is named by the
/// grammar walk at the seam.
#[cfg(test)]
pub(super) fn read_blocks(
    arr: &[serde_json::Value],
) -> Result<Vec<crate::codec::ir::IrBlock>, IrError> {
    read_blocks_with_origins(arr, OriginSite::Message).map(|(blocks, _)| blocks)
}
