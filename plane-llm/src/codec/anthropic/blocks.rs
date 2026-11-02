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

// Called alongside the native decoder, before any reordering or system-message fold. The
// surviving wire member, rather than the normalized IR variant, selects the source path.
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
    // `content` is legally a string or an array (absent/`null` is an empty turn). Any other present
    // type is a violation the lenient fallback below would swallow into an empty Text block: 400.
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

// Helper functions for IR mapping (used by read_request/write_request)
#[cfg(test)]
pub(super) fn read_block(
    block_val: &serde_json::Value,
) -> Result<crate::codec::ir::IrBlock, IrError> {
    read_block_with_origin(block_val, OriginSite::Message).map(|(block, _)| block)
}

pub(super) fn read_block_with_origin(
    block_val: &serde_json::Value,
    site: OriginSite,
) -> Result<(crate::codec::ir::IrBlock, BlockOrigin), IrError> {
    let obj = block_val.as_object().ok_or_else(ir_parse_error)?;

    let block_type = obj.get(keys::TYPE).and_then(|v| v.as_str()).unwrap_or("");

    let mut origin = block_origin(block_val, site);
    let block = match block_type {
        keys::TEXT => {
            let text = obj
                .get(keys::TEXT)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Parse cache_control - object form: {"type": "ephemeral"}
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
            // A signature off the Anthropic wire is Anthropic's, so a foreign writer never sends
            // it as its own, unless it is busbar's provenance envelope (another family's
            // signature handed to this client earlier), which restores the original bytes and origin.
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
            // A `tool_use` block must carry a non-empty string `id`, the key a later `tool_result`
            // pairs against; reject rather than invent one.
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
                // Anthropic has no wire concept of a Gemini thoughtSignature.
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
            // `cache_control` sits on the outer image block, a sibling of `source`.
            let cache_control = read_cache_control(obj.get(super::CACHE_CONTROL))?;
            if let Some(src_obj) = source.as_object() {
                // Two native image sources: `{"type":"url","url":…}` and
                // `{"type":"base64","media_type":…,"data":…}`. A url source has no `media_type`/
                // `data`, so it round-trips through the `media_type:"image_url"` sentinel `write_block`
                // recognizes, the raw url living in `data`.
                // A Files-API `file_id` source (or any future type that is neither) is an
                // Anthropic-hosted handle with no neutral form; it rides the opaque `Vendor` escape,
                // which this protocol re-emits verbatim and every other writer drops with a warn.
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
                // A present non-string `media_type` is the caller's error, not read as "".
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
        // A native `document` block, modelled as `IrBlock::Media` so it crosses to Gemini and
        // Bedrock (see `is_modeled_anthropic_block_type`). `document.source` has four shapes: the
        // two with a neutral form map onto neutral IR sources, and the Anthropic-hosted ones
        // (`file_id`, `content`) ride the opaque `Vendor` escape, so no other protocol emits a
        // handle its backend cannot resolve.
        BLOCK_TYPE_DOCUMENT => {
            // A `document` with no `source` carries no payload and must not be a 400, which would
            // reject forward-compatible history. [`read_blocks`] never hands it to this arm; a
            // direct call with one is malformed.
            let Some(source) = obj.get(keys::SOURCE) else {
                return Err(ir_parse_error());
            };
            let cache_control = read_cache_control(obj.get(super::CACHE_CONTROL))?;
            // `document.context` and `document.citations` ride `Media.context` / `Media.citations`,
            // crossing to Bedrock (which has both). Same-protocol they stay byte-exact via
            // `stash_unmodeled_blocks` and the splice in `write_message`.
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
                // A `text` source carries raw text, but the neutral `Base64` source is base64 by
                // contract (foreign writers emit it as such), so encode it on the way in; the
                // Anthropic writer decodes it back into a `text` source.
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
                // `base64` carries inline bytes (a PDF) plus a real mime type.
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
                // A `document` is a document whatever its mime says: there is no audio/video block.
                kind: crate::codec::ir::IrMediaKind::Document,
                source: ir_source,
                name,
                cache_control,
                citations: doc_citations,
                context: doc_context,
            })
        }
        // A native `search_result` block, the RAG grounding payload a caller supplies:
        // `{"type":"search_result","source":"<uri>","title":"…","content":[{"type":"text",…}],
        //   "citations":{"enabled":true}}`.
        //
        // The payload is entirely text, so every protocol can express it as text; the provenance
        // rides an `IrCitation` so targets that model citations still show the source.
        //
        // Deliberately not in `is_modeled_anthropic_block_type`: the raw block stays parked, so an
        // Anthropic-to-Anthropic hop splices the original bytes back. This arm is what the
        // cross-protocol egresses read, where the sentinel is cleared.
        BLOCK_TYPE_SEARCH_RESULT => {
            let source = obj.get(keys::SOURCE).and_then(|v| v.as_str()).unwrap_or("");
            let title = obj.get(keys::TITLE).and_then(|v| v.as_str()).unwrap_or("");
            // Concatenate the text parts in wire order; a non-text part contributes nothing.
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
            // Search-result slot (`IrBlock::search_result`, shared with Converse `searchResult`): a
            // `title — source` header line, the provenance as a `search_result_location` citation
            // with no indices, and the `citations` switch verbatim in its `raw`. The Anthropic and
            // Bedrock writers re-emit the native block from it.
            let cache_control = read_cache_control(obj.get(super::CACHE_CONTROL))?;
            Ok(crate::codec::ir::IrBlock::search_result(
                source,
                title,
                body,
                obj.get(keys::CITATIONS).cloned(),
                cache_control,
            ))
        }
        // A `redacted_thinking` block (opaque encrypted reasoning in `data`) maps onto the carrier
        // Bedrock's `redactedContent` uses: `Thinking { redacted: true }` with the bytes in `text`
        // and no signature, which the writer re-emits natively. A client-supplied `thinking` block
        // only ever reads as `redacted: false`, so it cannot forge one.
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
        // A provider-run web search's results: the IR's hosted-tool record. A
        // `web_search_tool_result_error` content carries its error code as the status, no results.
        WEB_SEARCH_TOOL_RESULT => Ok(read_web_search_tool_result(obj)),
        // A kind the IR does not model never reaches here: [`read_blocks`] leaves it out.
        _ => Err(ir_parse_error()),
    }?;
    Ok((block, origin))
}

/// Whether [`read_block_with_origin`] reads `block_val` into the IR: a kind in [`BLOCK_KINDS`] that carries a
/// payload. Another kind and a `document` with no `source` are not read, and nothing stands in for
/// them. A same-dialect write splices the raw block back ([`ANTHROPIC_UNMODELED_BLOCKS_SENTINEL`]);
/// a translate attempt drops it, named. A non-object is read (and refused by [`read_block_with_origin`]).
pub(super) fn is_read_into_ir(block_val: &serde_json::Value) -> bool {
    let Some(obj) = block_val.as_object() else {
        return true;
    };
    let kind = obj.get(keys::TYPE).and_then(|v| v.as_str()).unwrap_or("");
    BLOCK_KINDS.contains(&kind) && !(kind == BLOCK_TYPE_DOCUMENT && obj.get(keys::SOURCE).is_none())
}

/// Read a content-block array into the IR: every block [`is_read_into_ir`] keeps, in order. A
/// `document` with no `source` goes through the one drop path; an unmodelled kind is named by the
/// grammar walk at the seam.
#[cfg(test)]
pub(super) fn read_blocks(
    arr: &[serde_json::Value],
) -> Result<Vec<crate::codec::ir::IrBlock>, IrError> {
    read_blocks_with_origins(arr, OriginSite::Message).map(|(blocks, _)| blocks)
}

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
    // Per-tool `strict: true` is the same contract as OpenAI's `function.strict`; a non-boolean is
    // not a flag.
    let strict = obj.get(keys::STRICT).and_then(|v| v.as_bool());
    // An Anthropic-defined tool (`web_search_*`, `bash_*`, `text_editor_*`, `code_execution_*`,
    // `mcp_toolset`, …) is not a function tool and has no caller schema. Mark it hosted with its raw
    // definition: the cross-protocol seam drops hosted tools, and this writer re-emits it verbatim.
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
mod origin_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn creation_keeps_filtered_children_and_folded_system_origins_paired() {
        let request = AnthropicReader.read_request(&json!({
            "model": "fixture-model",
            "system": "same text",
            "messages": [
                {"role": "system", "content": "same text"},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "fixture-call", "content": [
                    {"type": "future_fixture", "payload": true},
                    {"type": "text", "text": "answer"},
                    {"type": "document"}
                ]}]}
            ]
        })).expect("native request");
        let system_paths: Vec<_> = request
            .block_origins
            .system
            .iter()
            .map(|origin| origin.source.unwrap().path)
            .collect();
        assert_eq!(system_paths, vec![SYSTEM_STRING.path, MESSAGE_STRING.path]);
        assert_eq!(request.system.len(), system_paths.len());
        assert_eq!(
            request.system_folds,
            vec![crate::codec::ir::IrSystemFold {
                before: 0,
                role: crate::codec::ir::IrSystemRole::System,
                blocks: 1..2,
            }]
        );
        let result = request.messages.first().unwrap().content.first().unwrap();
        let crate::codec::ir::IrBlock::ToolResult { content, .. } = result else {
            panic!("fixture tool result")
        };
        let origin = request
            .block_origins
            .messages
            .first()
            .unwrap()
            .first()
            .unwrap();
        assert_eq!(origin.source.unwrap().path, MESSAGE_TOOL_RESULT.path);
        assert_eq!(content.len(), origin.children.len());
        let child_paths: Vec<_> = origin
            .children
            .iter()
            .map(|origin| origin.source.unwrap().path)
            .collect();
        assert_eq!(child_paths, vec![RESULT_BLOCK.path]);
    }

    #[test]
    fn direct_response_reader_captures_native_reasoning_tag() {
        let response = AnthropicReader
            .read_response(
                crate::codec::test_host::HOST,
                &json!({
                    "role": "assistant", "content": [
                        {"type": "thinking", "thinking": "fixture thought"},
                        {"type": "text", "text": "fixture answer"}
                    ]
                }),
            )
            .expect("native response");
        let paths: Vec<_> = response
            .block_origins
            .response
            .iter()
            .map(|origin| origin.source.unwrap().path)
            .collect();
        assert_eq!(response.content.len(), paths.len());
        assert_eq!(paths, vec![RESPONSE_THINKING.path, RESPONSE_BLOCK.path]);
    }
    #[test]
    fn native_stream_origins_follow_emitted_start_indices_and_suppression() {
        let host = crate::codec::test_host::HOST;
        for (block, source) in [
            (json!({"type": "text", "text": ""}), &STREAM_TEXT),
            (
                json!({"type": "thinking", "thinking": ""}),
                &STREAM_THINKING,
            ),
            (
                json!({"type": "redacted_thinking", "data": "fixture-opaque"}),
                &STREAM_REDACTED,
            ),
            (
                json!({"type": "tool_use", "id": "fixture-call", "name": "fixture_tool"}),
                &STREAM_TOOL_USE,
            ),
        ] {
            let mut state = crate::codec::ir::StreamDecodeState::default();
            let events = AnthropicReader.read_response_events(
                host,
                EVT_CONTENT_BLOCK_START,
                &json!({"index": 0, "content_block": block}),
                &mut state,
            );
            let index = events
                .iter()
                .find_map(|event| match event {
                    IrStreamEvent::BlockStart { index, .. } => Some(index),
                    _ => None,
                })
                .expect("native block start");
            assert_eq!(
                state.block_origins.get(index).unwrap().source.unwrap().path,
                source.path
            );
        }
        let mut state = crate::codec::ir::StreamDecodeState::default();
        let unknown = AnthropicReader.read_response_events(
            host,
            EVT_CONTENT_BLOCK_START,
            &json!({"index": 0, "content_block": {"type": "future_fixture"}}),
            &mut state,
        );
        assert!(unknown.is_empty());
        assert!(state.block_origins.is_empty());
    }
    #[test]
    fn absent_document_payload_is_reported_at_the_actual_native_decode_site() {
        let (_, paths) = crate::codec::drops::scope(
            crate::codec::drops::Seam {
                direction: crate::codec::drops::Direction::Request,
                ingress: "anthropic",
                egress: "openai",
            },
            || {
                AnthropicReader.read_request(&json!({
            "system": [{"type": "document"}],
            "messages": [{"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "fixture-call", "content": [{"type": "document"}]
            }]}]
        })).expect("missing document payload is omitted")
            },
        );
        assert_eq!(
            paths,
            [
                "system[].type=document",
                "messages[].content[].type=tool_result.content[].type=document"
            ]
        );
    }
}
