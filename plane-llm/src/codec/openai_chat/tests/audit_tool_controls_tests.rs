use super::*;
#[test]
fn custom_only_tools_preserve_parallel_call_constraint() {
    let request = crate::codec::ir::IrRequest {
        hosted_tools: vec![crate::codec::ir::IrHostedTool::Custom(
            crate::codec::ir::IrCustomTool {
                name: "command".into(),
                ..Default::default()
            },
        )],
        parallel_tool_calls: Some(false),
        ..Default::default()
    };
    let wire = openai_writer().write_request(crate::codec::test_host::HOST, &request);
    assert_eq!(wire["parallel_tool_calls"], false);
}
