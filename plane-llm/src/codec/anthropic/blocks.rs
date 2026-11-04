// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Reading Anthropic content blocks, messages and tool definitions into the IR.

use super::*;
use crate::codec::dialect::ir_parse_error;
use crate::codec::keys;

type BlockOrigin = crate::codec::ir::IrBlockOrigin;
type BlockSource = crate::codec::ir::IrBlockSource;

#[derive(Clone, Copy)]
pub(super) enum OriginSite {
    System,
    Message,
    Response,
    ToolResult,
}

const SYSTEM_STRING: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "system",
};
const SYSTEM_BLOCK: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "system[]",
};
const MESSAGE_STRING: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content",
};
const MESSAGE_BLOCK: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[]",
};
const RESPONSE_BLOCK: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "content[]",
};
const RESULT_STRING: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=tool_result.content",
};
const RESULT_BLOCK: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=tool_result.content[]",
};
const SYSTEM_THINKING: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "system[].type=thinking",
};
const SYSTEM_REDACTED: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "system[].type=redacted_thinking",
};
const MESSAGE_THINKING: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=thinking",
};
const MESSAGE_REDACTED: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=redacted_thinking",
};
const MESSAGE_TOOL_RESULT: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=tool_result",
};
const MESSAGE_TOOL_USE: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=tool_use",
};
const MESSAGE_IMAGE: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=image",
};
const MESSAGE_DOCUMENT: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=document",
};
const MESSAGE_SEARCH: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=search_result",
};
const RESPONSE_THINKING: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "content[].type=thinking",
};
const RESPONSE_REDACTED: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "content[].type=redacted_thinking",
};
const RESPONSE_TOOL_USE: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "content[].type=tool_use",
};
const RESPONSE_IMAGE: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "content[].type=image",
};
const STREAM_TEXT: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "content_block_start.content_block.type=text",
};
const STREAM_THINKING: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "content_block_start.content_block.type=thinking",
};
const STREAM_REDACTED: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "content_block_start.content_block.type=redacted_thinking",
};
const STREAM_TOOL_USE: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "content_block_start.content_block.type=tool_use",
};
const STREAM_IMAGE: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "content_block_start.content_block.type=image",
};

pub(super) fn stream_block_origin(kind: &str) -> Option<BlockOrigin> {
    let source = match kind {
        keys::TEXT => &STREAM_TEXT,
        keys::THINKING => &STREAM_THINKING,
        BLOCK_TYPE_REDACTED_THINKING => &STREAM_REDACTED,
        STOP_TOOL_USE => &STREAM_TOOL_USE,
        keys::IMAGE => &STREAM_IMAGE,
        _ => return None,
    };
    Some(BlockOrigin::at(source))
}

pub(super) fn string_origin(site: OriginSite) -> BlockOrigin {
    BlockOrigin::at(match site {
        OriginSite::System => &SYSTEM_STRING,
        OriginSite::Message => &MESSAGE_STRING,
        OriginSite::Response => &RESPONSE_BLOCK,
        OriginSite::ToolResult => &RESULT_STRING,
    })
}

const UNREAD_SYSTEM_DOCUMENT_PATH: &str = "system[].type=document";
const UNREAD_MESSAGE_DOCUMENT_PATH: &str = "messages[].content[].type=document";
const UNREAD_RESPONSE_DOCUMENT_PATH: &str = "content[].type=document";
const UNREAD_RESULT_DOCUMENT_PATH: &str =
    "messages[].content[].type=tool_result.content[].type=document";

fn unread_document_path(site: OriginSite) -> &'static str {
    match site {
        OriginSite::System => UNREAD_SYSTEM_DOCUMENT_PATH,
        OriginSite::Message => UNREAD_MESSAGE_DOCUMENT_PATH,
        OriginSite::Response => UNREAD_RESPONSE_DOCUMENT_PATH,
        OriginSite::ToolResult => UNREAD_RESULT_DOCUMENT_PATH,
    }
}

pub(super) fn read_blocks_with_origins(
    arr: &[serde_json::Value],
    site: OriginSite,
) -> Result<(Vec<crate::codec::ir::IrBlock>, Vec<BlockOrigin>), IrError> {
    let mut blocks = Vec::with_capacity(arr.len());
    let mut origins = Vec::with_capacity(arr.len());
    for block_val in arr {
        if is_read_into_ir(block_val) {
            let (block, origin) = read_block_with_origin(block_val, site)?;
            blocks.push(block);
            origins.push(origin);
        } else if block_val.get(keys::TYPE).and_then(|v| v.as_str()) == Some(BLOCK_TYPE_DOCUMENT) {
            crate::codec::drops::note(crate::codec::drops::Dropped::new(
                unread_document_path(site),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                "dropping an Anthropic `document` block with no `source` on the cross-protocol seam: it carries no payload to translate (nothing is put in its place)",
            ));
        }
    }
    Ok((blocks, origins))
}

pub(super) fn read_message_with_origins(
    msg_val: &serde_json::Value,
) -> Result<(crate::codec::ir::IrMessage, Vec<BlockOrigin>), IrError> {
    let obj = msg_val.as_object().ok_or_else(ir_parse_error)?;

    let role_str = obj.get(keys::ROLE).and_then(|v| v.as_str()).unwrap_or("");
    let role = match role_str {
        keys::USER => crate::codec::ir::IrRole::User,
        keys::ASSISTANT => crate::codec::ir::IrRole::Assistant,
        keys::SYSTEM => crate::codec::ir::IrRole::System,
        _ => return Err(ir_parse_error()),
    };

    let content_val = obj.get(keys::CONTENT).unwrap_or(&serde_json::Value::Null);

    if !content_val.is_null() && !content_val.is_string() && !content_val.is_array() {
        return Err(ir_parse_error());
    }
    let (content, origins) = if let Some(arr) = content_val.as_array() {
        read_blocks_with_origins(arr, OriginSite::Message)?
    } else {
        (
            vec![crate::codec::ir::IrBlock::text(
                content_val.as_str().unwrap_or("").to_string(),
            )],
            vec![if content_val.is_string() {
                string_origin(OriginSite::Message)
            } else {
                BlockOrigin::default()
            }],
        )
    };

    Ok((crate::codec::ir::IrMessage { role, content }, origins))
}

fn block_origin(value: &serde_json::Value, site: OriginSite) -> BlockOrigin {
    let kind = value.get(keys::TYPE).and_then(|v| v.as_str());
    let source = match (site, kind) {
        (OriginSite::System, Some(keys::THINKING)) => &SYSTEM_THINKING,
        (OriginSite::System, Some(BLOCK_TYPE_REDACTED_THINKING)) => &SYSTEM_REDACTED,
        (OriginSite::System, _) => &SYSTEM_BLOCK,
        (OriginSite::Message, Some(keys::THINKING)) => &MESSAGE_THINKING,
        (OriginSite::Message, Some(BLOCK_TYPE_REDACTED_THINKING)) => &MESSAGE_REDACTED,
        (OriginSite::Message, Some(super::TOOL_RESULT)) => &MESSAGE_TOOL_RESULT,
        (OriginSite::Message, Some(STOP_TOOL_USE)) => &MESSAGE_TOOL_USE,
        (OriginSite::Message, Some(keys::IMAGE)) => &MESSAGE_IMAGE,
        (OriginSite::Message, Some(BLOCK_TYPE_DOCUMENT)) => &MESSAGE_DOCUMENT,
        (OriginSite::Message, Some(BLOCK_TYPE_SEARCH_RESULT)) => &MESSAGE_SEARCH,
        (OriginSite::Message, _) => &MESSAGE_BLOCK,
        (OriginSite::Response, Some(keys::THINKING)) => &RESPONSE_THINKING,
        (OriginSite::Response, Some(BLOCK_TYPE_REDACTED_THINKING)) => &RESPONSE_REDACTED,
        (OriginSite::Response, Some(STOP_TOOL_USE)) => &RESPONSE_TOOL_USE,
        (OriginSite::Response, Some(keys::IMAGE)) => &RESPONSE_IMAGE,
        (OriginSite::Response, _) => &RESPONSE_BLOCK,
        (OriginSite::ToolResult, _) => &RESULT_BLOCK,
    };
    BlockOrigin::at(source)
}

#[cfg(test)]
include!("tests/blocks_read_block_helper.rs");

pub(super) fn read_block_with_origin(
    block_val: &serde_json::Value,
    site: OriginSite,
) -> Result<(crate::codec::ir::IrBlock, BlockOrigin), IrError> {
    let obj = block_val.as_object().ok_or_else(ir_parse_error)?;

    let block_type = obj.get(keys::TYPE).and_then(|v| v.as_str()).unwrap_or("");

    let mut origin = block_origin(block_val, site);
    let block = match block_type {
        keys::TEXT => {
            let text = match obj.get(keys::TEXT) {
                Some(value) => value.as_str().ok_or_else(ir_parse_error)?,
                None => "",
            }
            .to_string();

            let cache_control = read_cache_control(obj.get(super::CACHE_CONTROL))?;
            let citations = obj
                .get(keys::CITATIONS)
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().map(read_citation).collect())
                .unwrap_or_default();
            Ok(crate::codec::ir::IrBlock::Text {
                text,
                cache_control,
                citations,
                refusal: false,
            })
        }
        keys::THINKING => {
            let text = obj
                .get(keys::THINKING)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let signature = obj
                .get(keys::SIGNATURE)
                .and_then(|v| v.as_str().map(String::from));
            let cache_control = read_cache_control(obj.get(super::CACHE_CONTROL))?;

            let (signature, signature_origin) = crate::codec::ir::sig_envelope::read_carried_opt(
                signature,
                Some(crate::codec::ir::IrSignatureOrigin::Anthropic),
            );
            Ok(crate::codec::ir::IrBlock::Thinking {
                text,
                signature,
                redacted: false,
                cache_control,
                kind: None,
                signature_origin,
            })
        }
        STOP_TOOL_USE => {
            let id = obj
                .get(keys::ID)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(ir_parse_error)?
                .to_string();
            let name = obj
                .get(keys::NAME)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input = obj
                .get(keys::INPUT)
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let cache_control = read_cache_control(obj.get(super::CACHE_CONTROL))?;
            Ok(crate::codec::ir::IrBlock::ToolUse {
                id,
                name,
                input,
                cache_control,

                thought_signature: None,
            })
        }
        super::TOOL_RESULT => {
            let tool_use_id = obj
                .get(super::TOOL_USE_ID)
                .and_then(|v| v.as_str())
                .filter(|id| !id.is_empty())
                .ok_or_else(ir_parse_error)?
                .to_string();
            let content_val = obj.get(keys::CONTENT).unwrap_or(&serde_json::Value::Null);
            let content = if let Some(arr) = content_val.as_array() {
                let (blocks, origins) = read_blocks_with_origins(arr, OriginSite::ToolResult)?;
                origin.children = origins;
                blocks
            } else {
                if !content_val.is_null() && !content_val.is_string() {
                    return Err(ir_parse_error());
                }
                origin.children.push(if content_val.is_string() {
                    string_origin(OriginSite::ToolResult)
                } else {
                    BlockOrigin::default()
                });
                vec![crate::codec::ir::IrBlock::text(
                    content_val.as_str().unwrap_or("").to_string(),
                )]
            };
            let is_error = obj
                .get(super::IS_ERROR)
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if obj
                .get(super::IS_ERROR)
                .is_some_and(|value| !value.is_null() && !value.is_boolean())
            {
                return Err(ir_parse_error());
            }
            let cache_control = read_cache_control(obj.get(super::CACHE_CONTROL))?;
            Ok(crate::codec::ir::IrBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                cache_control,
            })
        }
        keys::IMAGE => {
            let source = obj.get(keys::SOURCE).ok_or_else(ir_parse_error)?;

            let cache_control = read_cache_control(obj.get(super::CACHE_CONTROL))?;
            if let Some(src_obj) = source.as_object() {
                let src_type = src_obj.get(keys::TYPE).and_then(|v| v.as_str());
                if src_type.is_some_and(|t| t != keys::URL && t != keys::BASE64) {
                    return Ok((
                        crate::codec::ir::IrBlock::Image {
                            source: crate::codec::ir::IrImageSource::Vendor {
                                vendor: VENDOR_NAME,
                                value: source.clone(),
                            },
                            cache_control,
                            detail: None,
                        },
                        origin,
                    ));
                }
                if src_type == Some(keys::URL) {
                    let url = src_obj
                        .get(keys::URL)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    return Ok((
                        crate::codec::ir::IrBlock::Image {
                            source: crate::codec::ir::IrImageSource::Url(url),
                            cache_control,
                            detail: None,
                        },
                        origin,
                    ));
                }

                let media_type = match src_obj.get(super::MEDIA_TYPE) {
                    None => String::new(),
                    Some(v) => v.as_str().ok_or_else(ir_parse_error)?.to_string(),
                };
                let data = src_obj
                    .get(keys::DATA)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(crate::codec::ir::IrBlock::Image {
                    source: crate::codec::ir::IrImageSource::Base64 { media_type, data },
                    cache_control,
                    detail: None,
                })
            } else {
                Err(ir_parse_error())
            }
        }

        BLOCK_TYPE_DOCUMENT => {
            let Some(source) = obj.get(keys::SOURCE) else {
                return Err(ir_parse_error());
            };
            let cache_control = read_cache_control(obj.get(super::CACHE_CONTROL))?;

            let doc_citations = obj
                .get(keys::CITATIONS)
                .and_then(|c| c.get(keys::ENABLED))
                .and_then(|v| v.as_bool());
            let doc_context = obj
                .get(keys::CONTEXT)
                .and_then(|v| v.as_str())
                .map(String::from);
            let name = obj
                .get(keys::TITLE)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            let src_type = source
                .get(keys::TYPE)
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ir_source = match src_type {
                keys::URL => crate::codec::ir::IrImageSource::Url(
                    source
                        .get(keys::URL)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),

                keys::TEXT => crate::codec::ir::IrImageSource::Base64 {
                    media_type: source
                        .get(super::MEDIA_TYPE)
                        .and_then(|v| v.as_str())
                        .unwrap_or(DOCUMENT_MIME_TEXT_PLAIN)
                        .to_string(),
                    data: busbar_contract::media::base64_encode(
                        source
                            .get(keys::DATA)
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .as_bytes(),
                    ),
                },

                keys::BASE64 => crate::codec::ir::IrImageSource::Base64 {
                    media_type: source
                        .get(super::MEDIA_TYPE)
                        .and_then(|v| v.as_str())
                        .unwrap_or(keys::APPLICATION_PDF)
                        .to_string(),
                    data: source
                        .get(keys::DATA)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                },
                _ => crate::codec::ir::IrImageSource::Vendor {
                    vendor: VENDOR_NAME,
                    value: source.clone(),
                },
            };
            Ok(crate::codec::ir::IrBlock::Media {
                kind: crate::codec::ir::IrMediaKind::Document,
                source: ir_source,
                name,
                cache_control,
                citations: doc_citations,
                context: doc_context,
            })
        }

        BLOCK_TYPE_SEARCH_RESULT => {
            let source = obj.get(keys::SOURCE).and_then(|v| v.as_str()).unwrap_or("");
            let title = obj.get(keys::TITLE).and_then(|v| v.as_str()).unwrap_or("");

            let body: String = obj
                .get(keys::CONTENT)
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|b| b.get(keys::TYPE).and_then(|t| t.as_str()) == Some(keys::TEXT))
                        .filter_map(|b| b.get(keys::TEXT).and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();

            let cache_control = read_cache_control(obj.get(super::CACHE_CONTROL))?;
            Ok(crate::codec::ir::IrBlock::search_result(
                source,
                title,
                body,
                obj.get(keys::CITATIONS).cloned(),
                cache_control,
            ))
        }

        BLOCK_TYPE_REDACTED_THINKING => {
            let data = block_val
                .get(keys::DATA)
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            let cache_control = read_cache_control(block_val.get(super::CACHE_CONTROL))?;
            Ok(crate::codec::ir::IrBlock::Thinking {
                text: data,
                signature: None,
                redacted: true,
                cache_control,
                kind: None,
                signature_origin: None,
            })
        }

        WEB_SEARCH_TOOL_RESULT => Ok(read_web_search_tool_result(obj)),

        _ => Err(ir_parse_error()),
    }?;
    Ok((block, origin))
}

/// Whether [`read_block_with_origin`] reads `block_val` into the IR: a kind in [`BLOCK_KINDS`] that carries a
/// payload.
pub(super) fn is_read_into_ir(block_val: &serde_json::Value) -> bool {
    let Some(obj) = block_val.as_object() else {
        return true;
    };
    let kind = obj.get(keys::TYPE).and_then(|v| v.as_str()).unwrap_or("");
    BLOCK_KINDS.contains(&kind) && !(kind == BLOCK_TYPE_DOCUMENT && obj.get(keys::SOURCE).is_none())
}

#[cfg(test)]
include!("tests/blocks_read_blocks_helper.rs");

pub(super) fn read_tool(tool_val: &serde_json::Value) -> Result<crate::codec::ir::IrTool, IrError> {
    let obj = tool_val.as_object().ok_or_else(ir_parse_error)?;

    let name = obj
        .get(keys::NAME)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = obj
        .get(keys::DESCRIPTION)
        .and_then(|v| v.as_str().map(String::from));
    let input_schema = obj
        .get(keys::INPUT_SCHEMA)
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let cache_control = read_cache_control(obj.get(super::CACHE_CONTROL))?;

    let strict = obj.get(keys::STRICT).and_then(|v| v.as_bool());

    let hosted = obj
        .get(keys::TYPE)
        .and_then(|v| v.as_str())
        .filter(|t| *t != TOOL_TYPE_CUSTOM)
        .map(|_| tool_val.clone());

    Ok(crate::codec::ir::IrTool {
        name,
        description,
        input_schema,
        cache_control,
        hosted,
        strict,
    })
}

/// The block type of a provider-run web search's results.
pub(super) const WEB_SEARCH_TOOL_RESULT: &str = "web_search_tool_result";

/// Anthropic `web_search_tool_result` -> [`crate::codec::ir::IrBlock::HostedToolRecord`].
fn read_web_search_tool_result(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> crate::codec::ir::IrBlock {
    let content = obj.get(keys::CONTENT);
    let results = content
        .and_then(|c| c.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|r| {
                    Some(crate::codec::ir::IrSearchResult {
                        url: r.get(keys::URL)?.as_str()?.to_string(),
                        title: r
                            .get(keys::TITLE)
                            .and_then(|t| t.as_str())
                            .map(String::from),
                        snippet: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let status = content
        .and_then(|c| c.get(ERROR_CODE))
        .and_then(|e| e.as_str())
        .map(String::from);
    crate::codec::ir::IrBlock::HostedToolRecord {
        kind: crate::codec::ir::IrHostedToolKind::WebSearch,
        call_id: obj
            .get(TOOL_USE_ID)
            .and_then(|v| v.as_str())
            .map(String::from),
        status,
        results,
    }
}

/// The error member of a `web_search_tool_result_error` content.
const ERROR_CODE: &str = "error_code";

#[cfg(test)]
#[path = "tests/blocks_origin_tests.rs"]
mod origin_tests;
