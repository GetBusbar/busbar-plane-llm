use super::*;

#[test]
fn server_tool_usage_preserves_fetch_counts_in_both_envelopes() {
    for (search, fetch) in [(None, Some(3)), (Some(2), Some(3)), (Some(2), None)] {
        let mut usage = crate::codec::ir::IrUsage::default();
        usage.detail.web_search_requests = search;
        usage.detail.web_fetch_requests = fetch;
        for wire in [
            usage::write_usage_object(Some(&usage)),
            usage::write_message_delta_usage(&usage),
        ] {
            assert_eq!(
                wire["server_tool_use"]["web_search_requests"],
                search.unwrap_or(0)
            );
            assert_eq!(
                wire["server_tool_use"]["web_fetch_requests"],
                fetch.unwrap_or(0)
            );
        }
    }
    assert!(usage::write_server_tool_use(&crate::codec::ir::IrUsage::default()).is_null());
}
