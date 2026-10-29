// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Reading Anthropic content blocks, messages and tool definitions into the IR.

use super::*;
use crate::codec::dialect::ir_parse_error;
use crate::codec::keys;

// Helper functions for IR mapping (used by read_request/write_request)
pub(super) fn read_block(
    block_val: &serde_json::Value,
) -> Result<crate::codec::ir::IrBlock, IrError> {
    let obj = block_val.as_object().ok_or_else(ir_parse_error)?;

    let block_type = obj.get(keys::TYPE).and_then(|v| v.as_str()).unwrap_or("");

    match block_type {
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
                .unwrap_or("")
                .to_string();
            let content_val = obj.get(keys::CONTENT).unwrap_or(&serde_json::Value::Null);
            let content = if let Some(arr) = content_val.as_array() {
                read_blocks(arr)?
            } else {
                vec![crate::codec::ir::IrBlock::text(
                    content_val.as_str().unwrap_or("").to_string(),
                )]
            };
            let is_error = obj
                .get(super::IS_ERROR)
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
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
                    return Ok(crate::codec::ir::IrBlock::Image {
                        source: crate::codec::ir::IrImageSource::Vendor {
                            vendor: VENDOR_NAME,
                            value: source.clone(),
                        },
                        cache_control,
                        detail: None,
                    });
                }
                if src_type == Some(keys::URL) {
                    let url = src_obj
                        .get(keys::URL)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    return Ok(crate::codec::ir::IrBlock::Image {
                        source: crate::codec::ir::IrImageSource::Url(url),
                        cache_control,
                        detail: None,
                    });
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
    }
}

/// Whether [`read_block`] reads `block_val` into the IR: a kind in [`BLOCK_KINDS`] that carries a
/// payload. Another kind and a `document` with no `source` are not read, and nothing stands in for
/// them. A same-dialect write splices the raw block back ([`ANTHROPIC_UNMODELED_BLOCKS_SENTINEL`]);
/// a translate attempt drops it, named. A non-object is read (and refused by [`read_block`]).
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
pub(super) fn read_blocks(
    arr: &[serde_json::Value],
) -> Result<Vec<crate::codec::ir::IrBlock>, IrError> {
    let mut out = Vec::with_capacity(arr.len());
    for block_val in arr {
        if is_read_into_ir(block_val) {
            out.push(read_block(block_val)?);
        } else if block_val.get(keys::TYPE).and_then(|v| v.as_str()) == Some(BLOCK_TYPE_DOCUMENT) {
            crate::codec::drops::note(crate::codec::drops::Dropped::new(
                "messages[].content[].type=document",
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                "dropping an Anthropic `document` block with no `source` on the cross-protocol \
                 seam: it carries no payload to translate (nothing is put in its place)",
            ));
        }
    }
    Ok(out)
}

pub(super) fn read_message(
    msg_val: &serde_json::Value,
) -> Result<crate::codec::ir::IrMessage, IrError> {
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
    let content = if let Some(arr) = content_val.as_array() {
        read_blocks(arr)?
    } else {
        vec![crate::codec::ir::IrBlock::text(
            content_val.as_str().unwrap_or("").to_string(),
        )]
    };

    Ok(crate::codec::ir::IrMessage { role, content })
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
