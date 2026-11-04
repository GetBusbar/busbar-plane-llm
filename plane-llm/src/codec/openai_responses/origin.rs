use super::*;
use crate::codec::ir::{IrBlockOrigin, IrBlockSource};

pub(super) const INSTRUCTIONS: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "instructions",
};
pub(super) const INPUT_STRING: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input",
};
pub(super) const INPUT_TEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=input_text",
};
pub(super) const INPUT_IMAGE_ITEM: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=input_image",
};
pub(super) const OUTPUT_TEXT_INPUT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=output_text",
};
pub(super) const FUNCTION_CALL_INPUT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call",
};
pub(super) const FUNCTION_OUTPUT_INPUT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output",
};
pub(super) const REASONING_INPUT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=reasoning",
};
pub(super) const MESSAGE_STRING: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content",
};
pub(super) const SYSTEM_PART_TEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].text",
};
pub(super) const TOOL_OUTPUT_STRING: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output",
};
pub(super) const RESPONSE_TEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "output[].type=message.content[].type=output_text",
};
pub(super) const RESPONSE_REFUSAL: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "output[].type=message.content[].type=refusal",
};
pub(super) const RESPONSE_FUNCTION: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "output[].type=function_call",
};
pub(super) const RESPONSE_REASONING: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "output[].type=reasoning",
};
pub(super) const RESPONSE_WEB_SEARCH: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "output[].type=web_search_call",
};
pub(super) const STREAM_FUNCTION: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.output_item.added.item.type=function_call",
};
pub(super) const STREAM_REASONING: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.output_item.added.item.type=reasoning",
};
pub(super) const STREAM_REASONING_TEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.reasoning_text.delta.delta",
};
pub(super) const STREAM_REASONING_SUMMARY: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.reasoning_summary_text.delta.delta",
};
pub(super) const STREAM_TEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.output_text.delta.delta",
};
pub(super) const STREAM_REFUSAL_DELTA: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.refusal.delta.delta",
};
pub(super) const STREAM_REFUSAL_DONE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.refusal.done.refusal",
};
pub(super) const STREAM_REFUSAL_COMPLETED_SNAPSHOT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.completed.response.output[].content[].type=refusal",
};
const MESSAGE_INPUTTEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].type=input_text",
};
const MESSAGE_OUTPUTTEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].type=output_text",
};
const MESSAGE_IMAGE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].type=input_image",
};
const MESSAGE_FILE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].type=input_file",
};
const MESSAGE_REFUSAL: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].type=refusal",
};
const MESSAGE_AUDIO: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].type=input_audio",
};
const TOOL_OUTPUT_INPUTTEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output[].type=input_text",
};
const TOOL_OUTPUT_OUTPUTTEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output[].type=output_text",
};
const TOOL_OUTPUT_IMAGE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output[].type=input_image",
};
const TOOL_OUTPUT_FILE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output[].type=input_file",
};
const TOOL_OUTPUT_REFUSAL: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output[].type=refusal",
};
const TOOL_OUTPUT_AUDIO: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output[].type=input_audio",
};

#[derive(Clone, Copy)]
pub(super) enum PartSite {
    Message,
    ToolOutput,
}
pub(super) fn part(site: PartSite, native_tag: &str) -> IrBlockOrigin {
    let source = match (site, native_tag) {
        (PartSite::Message, CONTENT_TYPE_INPUT_TEXT) => &MESSAGE_INPUTTEXT,
        (PartSite::Message, CONTENT_TYPE_OUTPUT_TEXT) => &MESSAGE_OUTPUTTEXT,
        (PartSite::Message, INPUT_IMAGE) => &MESSAGE_IMAGE,
        (PartSite::Message, INPUT_FILE) => &MESSAGE_FILE,
        (PartSite::Message, keys::REFUSAL) => &MESSAGE_REFUSAL,
        (PartSite::Message, keys::INPUT_AUDIO) => &MESSAGE_AUDIO,
        (PartSite::ToolOutput, CONTENT_TYPE_INPUT_TEXT) => &TOOL_OUTPUT_INPUTTEXT,
        (PartSite::ToolOutput, CONTENT_TYPE_OUTPUT_TEXT) => &TOOL_OUTPUT_OUTPUTTEXT,
        (PartSite::ToolOutput, INPUT_IMAGE) => &TOOL_OUTPUT_IMAGE,
        (PartSite::ToolOutput, INPUT_FILE) => &TOOL_OUTPUT_FILE,
        (PartSite::ToolOutput, keys::REFUSAL) => &TOOL_OUTPUT_REFUSAL,
        (PartSite::ToolOutput, keys::INPUT_AUDIO) => &TOOL_OUTPUT_AUDIO,
        _ => unreachable!("only a successfully decoded native part has an origin"),
    };
    IrBlockOrigin::at(source)
}

pub(super) const STREAM_REFUSAL_FAILED_SNAPSHOT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.failed.response.output[].content[].type=refusal",
};
pub(super) const STREAM_REFUSAL_INCOMPLETE_SNAPSHOT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.incomplete.response.output[].content[].type=refusal",
};

#[cfg(test)]
#[path = "tests/origin_tests.rs"]
mod tests;
