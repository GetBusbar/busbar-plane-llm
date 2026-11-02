// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! FIELD-COVERAGE CARRY TESTS for the Gemini (generateContent) dialect.

use super::super::proto_codec::Protocol;
use serde_json::json;

/// The `generationConfig` sampling knobs are each promoted into a typed IR field on read and

#[test]
fn gemini_request_sampling_knobs_survive() {
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {
            "temperature": 0.35,
            "topP": 0.8,
            "topK": 40,
            "candidateCount": 2,
            "maxOutputTokens": 256,
            "stopSequences": ["END", "STOP"],
            "seed": 12345,
            "presencePenalty": 0.5,
            "frequencyPenalty": 0.25
        }
    });
    let ir = Protocol::gemini()
        .reader()
        .read_request(&body)
        .expect("read");

    assert_eq!(ir.temperature, Some(0.35));
    assert_eq!(ir.top_p, Some(0.8));
    assert_eq!(ir.top_k, Some(40));
    assert_eq!(ir.n, Some(2));
    assert_eq!(ir.max_tokens, Some(256));
    assert_eq!(ir.stop, vec!["END".to_string(), "STOP".to_string()]);
    assert_eq!(ir.seed, Some(12345));
    assert_eq!(ir.presence_penalty, Some(0.5));
    assert_eq!(ir.frequency_penalty, Some(0.25));

    let out = Protocol::gemini()
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    let gc = &out["generationConfig"];
    assert_eq!(gc["temperature"], 0.35);
    assert_eq!(gc["topP"], 0.8);
    assert_eq!(gc["topK"], 40);
    assert_eq!(gc["candidateCount"], 2);
    assert_eq!(gc["maxOutputTokens"], 256);
    assert_eq!(gc["stopSequences"], json!(["END", "STOP"]));
    assert_eq!(gc["seed"], 12345);
    assert_eq!(gc["presencePenalty"], 0.5);
    assert_eq!(gc["frequencyPenalty"], 0.25);
}

/// The logprobs ask: boolean `responseLogprobs` + top-count `logprobs`, promoted and re-emitted.
#[test]
fn gemini_request_logprobs_ask_survives() {
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {"responseLogprobs": true, "logprobs": 3}
    });
    let ir = Protocol::gemini()
        .reader()
        .read_request(&body)
        .expect("read");
    assert_eq!(ir.logprobs, Some(true));
    assert_eq!(ir.top_logprobs, Some(3));

    let out = Protocol::gemini()
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["generationConfig"]["responseLogprobs"], true);
    assert_eq!(out["generationConfig"]["logprobs"], 3);
}

/// Structured output: `responseMimeType` + `responseSchema` normalize into the IR response_format

#[test]
fn gemini_request_response_format_survives() {
    let schema = json!({"type": "object", "properties": {"x": {"type": "string"}}});
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {
            "responseMimeType": "application/json",
            "responseSchema": schema
        }
    });
    let ir = Protocol::gemini()
        .reader()
        .read_request(&body)
        .expect("read");
    let rf = ir
        .response_format
        .as_ref()
        .expect("response_format present");
    assert!(rf.json, "responseMimeType application/json → json output");
    assert!(rf.schema.is_some(), "responseSchema must reach the IR");

    let out = Protocol::gemini()
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        out["generationConfig"]["responseSchema"]["properties"]["x"]["type"],
        "string"
    );
}

/// The thinking ask `generationConfig.thinkingConfig.thinkingBudget` reaches the IR reasoning

#[test]
fn gemini_request_thinking_budget_survives() {
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {"thinkingConfig": {"thinkingBudget": 2048}}
    });
    let ir = Protocol::gemini()
        .reader()
        .read_request(&body)
        .expect("read");
    assert_eq!(
        ir.reasoning,
        Some(crate::codec::ir::IrReasoningAsk::Budget(2048)),
        "thinkingBudget must reach the IR reasoning carrier"
    );
    let out = Protocol::gemini()
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);

    assert_eq!(
        out["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        2048
    );
}

/// `systemInstruction` → IR system blocks → re-emitted `systemInstruction.parts[].text`.
#[test]
fn gemini_request_system_instruction_survives() {
    let body = json!({
        "systemInstruction": {"parts": [{"text": "be terse"}]},
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
    });
    let ir = Protocol::gemini()
        .reader()
        .read_request(&body)
        .expect("read");
    assert!(
        ir.system.iter().any(|b| matches!(
            b, crate::codec::ir::IrBlock::Text { text, .. } if text == "be terse"
        )),
        "systemInstruction must reach the IR system carrier: {:?}",
        ir.system
    );
    let out = Protocol::gemini()
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["systemInstruction"]["parts"][0]["text"], "be terse");
}

/// `tools[].functionDeclarations[]` → IR tools → re-emitted.
#[test]
fn gemini_request_tools_survive() {
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "tools": [{"functionDeclarations": [
            {"name": "get_weather", "description": "look up weather",
             "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}
        ]}]
    });
    let ir = Protocol::gemini()
        .reader()
        .read_request(&body)
        .expect("read");
    assert_eq!(ir.tools.len(), 1);
    assert_eq!(ir.tools[0].name, "get_weather");
    assert_eq!(ir.tools[0].description.as_deref(), Some("look up weather"));

    let out = Protocol::gemini()
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    let decl = &out["tools"][0]["functionDeclarations"][0];
    assert_eq!(decl["name"], "get_weather");
    assert_eq!(decl["description"], "look up weather");
    assert_eq!(decl["parameters"]["properties"]["city"]["type"], "string");
}

/// `toolConfig.functionCallingConfig` → IR tool_choice → re-emitted (the raw toolConfig also rides

#[test]
fn gemini_request_tool_config_survives() {
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "tools": [{"functionDeclarations": [{"name": "f", "parameters": {"type": "object"}}]}],
        "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["f"]}}
    });
    let ir = Protocol::gemini()
        .reader()
        .read_request(&body)
        .expect("read");
    assert!(
        ir.tool_choice.is_some(),
        "toolConfig must reach the IR tool_choice carrier"
    );
    let out = Protocol::gemini()
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);

    assert!(
        out["toolConfig"]["functionCallingConfig"].is_object(),
        "toolConfig.functionCallingConfig must be re-emitted: {out}"
    );
    assert_eq!(out["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
}

/// `safetySettings`, `labels`, and the unmodeled `generationConfig` sub-fields

#[test]
fn gemini_request_provider_specific_fields_survive_same_proto() {
    let safety = json!([{"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_ONLY_HIGH"}]);
    let labels = json!({"team": "search", "env": "prod"});
    let speech = json!({"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": "Kore"}}});
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "safetySettings": safety,
        "labels": labels,
        "generationConfig": {
            "responseModalities": ["TEXT", "AUDIO"],
            "mediaResolution": "MEDIA_RESOLUTION_HIGH",
            "speechConfig": speech,
            "thinkingConfig": {"includeThoughts": true}
        }
    });
    let mut ir = Protocol::gemini()
        .reader()
        .read_request(&body)
        .expect("read");

    let out = Protocol::gemini()
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["safetySettings"], safety, "safetySettings must survive");
    assert_eq!(out["labels"], labels, "labels must survive");
    assert_eq!(
        out["generationConfig"]["responseModalities"],
        json!(["TEXT", "AUDIO"]),
        "responseModalities must survive"
    );
    assert_eq!(
        out["generationConfig"]["mediaResolution"], "MEDIA_RESOLUTION_HIGH",
        "mediaResolution must survive"
    );
    assert_eq!(
        out["generationConfig"]["speechConfig"], speech,
        "speechConfig must survive"
    );
    assert_eq!(
        out["generationConfig"]["thinkingConfig"]["includeThoughts"], true,
        "thinkingConfig.includeThoughts must survive"
    );

    ir.extra.clear();
    let dropped = Protocol::gemini()
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        dropped.get("safetySettings").is_none(),
        "safetySettings must drop cross-protocol"
    );
    assert_eq!(dropped["labels"], labels, "labels carry typed (IR-03)");
    let gc = dropped.get("generationConfig");
    let has = |k: &str| gc.and_then(|g| g.get(k)).is_some();
    assert_eq!(
        dropped["generationConfig"]["responseModalities"],
        json!(["TEXT", "AUDIO"]),
        "responseModalities carry typed (IR-19)"
    );
    assert!(
        !has("mediaResolution"),
        "mediaResolution must drop cross-protocol"
    );
    assert!(
        !has("speechConfig"),
        "speechConfig must drop cross-protocol"
    );
}

/// Every representable `parts[]` construct survives read → write: text, a `thought` reasoning part

#[test]
fn gemini_request_content_parts_survive() {
    let body = json!({
        "contents": [
            {"role": "model", "parts": [
                {"text": "visible answer"},
                {"thought": true, "text": "let me think", "thoughtSignature": "SIG-abc"},
                {"functionCall": {"name": "search", "args": {"q": "rust"}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "search", "response": {"hits": 3}}},
                {"fileData": {"fileUri": "gs://bucket/doc.pdf", "mimeType": "application/pdf"}}
            ]}
        ]
    });
    let ir = Protocol::gemini()
        .reader()
        .read_request(&body)
        .expect("read");

    let model_turn = &ir.messages[0].content;

    assert!(
        model_turn.iter().any(|b| matches!(
            b, crate::codec::ir::IrBlock::Text { text, .. } if text == "visible answer"
        )),
        "parts[].text must reach the IR"
    );

    assert!(
        model_turn.iter().any(|b| matches!(
            b, crate::codec::ir::IrBlock::Thinking { text, signature, .. }
            if text == "let me think" && signature.as_deref() == Some("SIG-abc")
        )),
        "parts[].thought + thoughtSignature must reach the IR Thinking carrier: {model_turn:?}"
    );

    assert!(
        model_turn.iter().any(|b| matches!(
            b, crate::codec::ir::IrBlock::ToolUse { name, input, .. }
            if name == "search" && input["q"] == "rust"
        )),
        "parts[].functionCall name+args must reach the IR ToolUse carrier"
    );

    let call_id = model_turn
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("the model turn carries the call");
    let user_turn = &ir.messages[1].content;
    assert!(
        user_turn.iter().any(|b| matches!(
            b, crate::codec::ir::IrBlock::ToolResult { tool_use_id, content, .. }
            if tool_use_id == &call_id
               && content.iter().any(|c| matches!(c, crate::codec::ir::IrBlock::Text { text, .. } if text.contains("\"hits\":3")))
        )),
        "parts[].functionResponse name+response must reach the IR ToolResult carrier: {user_turn:?}"
    );

    assert!(
        user_turn.iter().any(|b| matches!(
            b, crate::codec::ir::IrBlock::Media {
                kind: crate::codec::ir::IrMediaKind::Document,
                source: crate::codec::ir::IrImageSource::Url(uri), ..
            } if uri == "gs://bucket/doc.pdf"
        )),
        "parts[].fileData (pdf) must reach the IR as a Document URL Media block: {user_turn:?}"
    );

    let out = Protocol::gemini()
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    let mparts = out["contents"][0]["parts"].as_array().expect("model parts");
    assert!(
        mparts.iter().any(|p| p["text"] == "visible answer"),
        "text re-emit"
    );
    assert!(
        mparts.iter().any(|p| p["thought"] == true
            && p["text"] == "let me think"
            && p["thoughtSignature"] == "SIG-abc"),
        "thought + thoughtSignature re-emit: {out}"
    );
    assert!(
        mparts
            .iter()
            .any(|p| p["functionCall"]["name"] == "search"
                && p["functionCall"]["args"]["q"] == "rust"),
        "functionCall re-emit: {out}"
    );
    let uparts = out["contents"][1]["parts"].as_array().expect("user parts");
    assert!(
        uparts
            .iter()
            .any(|p| p["functionResponse"]["name"] == "search"
                && p["functionResponse"]["response"]["hits"] == 3),
        "functionResponse re-emit: {out}"
    );
    assert!(
        uparts
            .iter()
            .any(|p| p["fileData"]["fileUri"] == "gs://bucket/doc.pdf"
                && p["fileData"]["mimeType"] == "application/pdf"),
        "fileData fileUri+mimeType re-emit: {out}"
    );
}

/// `parts[].executableCode` and `parts[].codeExecutionResult` are the code-interpreter tool's

#[test]
fn gemini_code_execution_parts_drop_cross_proto_without_corruption() {
    let req = json!({
        "contents": [{"role": "model", "parts": [
            {"executableCode": {"language": "PYTHON", "code": "print(1)"}},
            {"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "1"}}
        ]}]
    });
    let ir = Protocol::gemini()
        .reader()
        .read_request(&req)
        .expect("read");
    assert!(
        ir.messages[0].content.is_empty(),
        "executableCode/codeExecutionResult request parts must be dropped, not corrupted: {:?}",
        ir.messages[0].content
    );

    let resp = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [
                {"executableCode": {"language": "PYTHON", "code": "print(1)"}},
                {"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "1"}}
            ]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 3, "totalTokenCount": 8}
    });
    let ir = Protocol::gemini()
        .reader()
        .read_response(crate::codec::test_host::HOST, &resp)
        .expect("read");
    assert!(
        ir.content.is_empty(),
        "executableCode/codeExecutionResult response parts must be dropped, not corrupted: {:?}",
        ir.content
    );

    let openai = Protocol::openai()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    let msg = openai["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(
        !msg.contains("executableCode") && !msg.contains("print(1)"),
        "code-execution artifacts must not leak into cross-protocol text: {openai}"
    );
}

/// `candidates[].content`, `candidates[].finishReason`, top-level `modelVersion` and `responseId`

#[test]
fn gemini_response_core_fields_survive() {
    let body = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "the answer"}]},
            "finishReason": "MAX_TOKENS"
        }],
        "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 6, "totalTokenCount": 10},
        "modelVersion": "gemini-2.5-flash-001",
        "responseId": "PXmFaPzVMIabcdef"
    });
    let ir = Protocol::gemini()
        .reader()
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");

    assert!(
        ir.content.iter().any(|b| matches!(
            b, crate::codec::ir::IrBlock::Text { text, .. } if text == "the answer"
        )),
        "candidates[].content must reach the IR"
    );

    assert_eq!(
        ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::MaxTokens)
    );

    assert_eq!(ir.model.as_deref(), Some("gemini-2.5-flash-001"));
    assert_eq!(ir.id.as_deref(), Some("PXmFaPzVMIabcdef"));

    let out = Protocol::gemini()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["candidates"][0]["content"]["parts"][0]["text"],
        "the answer"
    );
    assert_eq!(out["candidates"][0]["finishReason"], "MAX_TOKENS");
    assert_eq!(out["modelVersion"], "gemini-2.5-flash-001");
    assert_eq!(out["responseId"], "PXmFaPzVMIabcdef");
}

/// `candidates[].citationMetadata` re-emits same-protocol, and `candidates[].groundingMetadata`

#[test]
fn gemini_response_citations_survive() {
    let cited = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Paris is the capital."}]},
            "finishReason": "STOP",
            "citationMetadata": {"citationSources": [
                {"startIndex": 0, "endIndex": 5, "uri": "https://atlas", "title": "Atlas"}
            ]}
        }],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4, "totalTokenCount": 7}
    });
    let ir = Protocol::gemini()
        .reader()
        .read_response(crate::codec::test_host::HOST, &cited)
        .expect("read");

    let Some(crate::codec::ir::IrBlock::Text { citations, .. }) = ir
        .content
        .iter()
        .find(|b| matches!(b, crate::codec::ir::IrBlock::Text { .. }))
    else {
        panic!("expected a Text block, got {:?}", ir.content);
    };
    assert_eq!(citations.len(), 1, "citationMetadata must reach the IR");
    assert_eq!(citations[0].url.as_deref(), Some("https://atlas"));
    let out = Protocol::gemini()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    assert!(
        out["candidates"][0]["citationMetadata"]["citationSources"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "citationMetadata must be re-emitted same-protocol: {out}"
    );

    let grounded = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Paris is the capital."}]},
            "finishReason": "STOP",
            "groundingMetadata": {
                "groundingChunks": [{"web": {"uri": "https://atlas", "title": "Atlas"}}],
                "groundingSupports": [{
                    "segment": {"startIndex": 0, "endIndex": 5, "text": "Paris"},
                    "groundingChunkIndices": [0]
                }]
            }
        }],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4, "totalTokenCount": 7}
    });
    let ir = Protocol::gemini()
        .reader()
        .read_response(crate::codec::test_host::HOST, &grounded)
        .expect("read");

    let Some(crate::codec::ir::IrBlock::Text { citations, .. }) = ir
        .content
        .iter()
        .find(|b| matches!(b, crate::codec::ir::IrBlock::Text { .. }))
    else {
        panic!("expected a Text block, got {:?}", ir.content);
    };
    assert_eq!(
        citations.len(),
        1,
        "groundingMetadata source must reach the IR"
    );
    assert_eq!(citations[0].url.as_deref(), Some("https://atlas"));
    let cohere = Protocol::cohere()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        cohere["message"]["citations"][0]["sources"][0]["document"]["url"],
        json!("https://atlas"),
        "{cohere}"
    );
}

/// `candidates[].logprobsResult` reaches the IR logprobs carrier and re-emits.
#[test]
fn gemini_response_logprobs_result_survives() {
    let body = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Hi"}]},
            "finishReason": "STOP",
            "logprobsResult": {
                "chosenCandidates": [{"token": "Hi", "logProbability": -0.031}],
                "topCandidates": [{"candidates": [
                    {"token": "Hi", "logProbability": -0.031},
                    {"token": "Hello", "logProbability": -3.5}
                ]}]
            }
        }],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2, "totalTokenCount": 5}
    });
    let ir = Protocol::gemini()
        .reader()
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");
    assert_eq!(ir.logprobs.len(), 1, "logprobsResult must reach the IR");
    assert_eq!(ir.logprobs[0].token, "Hi");
    assert_eq!(ir.logprobs[0].top.len(), 2);

    let out = Protocol::gemini()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["candidates"][0]["logprobsResult"]["chosenCandidates"][0]["token"], "Hi",
        "logprobsResult must be re-emitted: {out}"
    );
}

/// `usageMetadata.cachedContentTokenCount` (→ cache_read) and `usageMetadata.toolUsePromptTokenCount`

#[test]
fn gemini_response_usage_buckets_survive() {
    let body = json!({
        "candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]}, "finishReason": "STOP"}],
        "usageMetadata": {
            "promptTokenCount": 100,
            "candidatesTokenCount": 20,
            "cachedContentTokenCount": 30,
            "toolUsePromptTokenCount": 12,
            "totalTokenCount": 120
        }
    });
    let ir = Protocol::gemini()
        .reader()
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");

    assert_eq!(
        ir.usage.cache_read_input_tokens,
        Some(30),
        "cachedContentTokenCount must reach the IR"
    );

    assert_eq!(ir.usage.input_tokens, 82);

    assert_eq!(
        ir.usage.detail.tool_use_prompt_tokens,
        Some(12),
        "toolUsePromptTokenCount must reach the IR usage detail"
    );

    let out = Protocol::gemini()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["usageMetadata"]["cachedContentTokenCount"], 30,
        "cachedContentTokenCount must be re-emitted: {out}"
    );

    assert_eq!(
        out["usageMetadata"]["promptTokenCount"], 100,
        "the tool-use term must not be double-counted into promptTokenCount: {out}"
    );
    assert_eq!(
        out["usageMetadata"]["toolUsePromptTokenCount"], 12,
        "toolUsePromptTokenCount must be re-emitted: {out}"
    );
}

/// `promptFeedback.blockReason` (a prompt-level content block, no candidates) reaches the IR as a

#[test]
fn gemini_response_prompt_feedback_block_reason_survives() {
    let body = json!({
        "promptFeedback": {"blockReason": "SAFETY"},
        "usageMetadata": {"promptTokenCount": 8, "totalTokenCount": 8}
    });
    let ir = Protocol::gemini()
        .reader()
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");
    assert_eq!(
        ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::Safety),
        "promptFeedback.blockReason SAFETY must map to a Safety stop reason"
    );
    assert!(
        ir.content.is_empty(),
        "a prompt-blocked response carries no content"
    );
}

/// The provider-specific response fields have no cross-protocol home, so they are carried as a

#[test]
fn gemini_response_provider_specific_fields_drop_cross_proto_without_corruption() {
    let body = json!({
        "candidates": [{
            "index": 0,
            "content": {"role": "model", "parts": [{"text": "the answer"}]},
            "finishReason": "STOP",
            "avgLogprobs": -0.125,
            "tokenCount": 7,
            "safetyRatings": [{"category": "HARM_CATEGORY_HATE_SPEECH", "probability": "NEGLIGIBLE"}]
        }],
        "promptFeedback": {
            "safetyRatings": [{"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "probability": "NEGLIGIBLE"}]
        },
        "usageMetadata": {
            "promptTokenCount": 5,
            "candidatesTokenCount": 4,
            "totalTokenCount": 9,
            "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 5}]
        }
    });
    let ir = Protocol::gemini()
        .reader()
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");

    assert_eq!(
        ir.content.len(),
        1,
        "no provider field may synthesize an extra block: {:?}",
        ir.content
    );
    assert!(
        matches!(&ir.content[0], crate::codec::ir::IrBlock::Text { text, .. } if text == "the answer"),
        "the only content block is the visible answer: {:?}",
        ir.content[0]
    );

    let openai = Protocol::openai()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir)
        .to_string();
    for field in [
        "safetyRatings",
        "avgLogprobs",
        "tokenCount",
        "promptFeedback",
        "promptTokensDetails",
    ] {
        assert!(
            !openai.contains(field),
            "provider-specific field {field:?} must not leak onto a foreign egress: {openai}"
        );
    }
}

/// `usageMetadata.trafficType` (ON_DEMAND vs PROVISIONED) reaches the IR's usage-detail attribution

#[test]
fn gemini_vertex_traffic_type_survives() {
    let body = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "the answer"}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 18,
            "candidatesTokenCount": 6,
            "totalTokenCount": 24,
            "trafficType": "ON_DEMAND"
        }
    });
    let ir = Protocol::gemini()
        .reader()
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");
    assert_eq!(
        ir.usage.detail.traffic_type.as_deref(),
        Some("ON_DEMAND"),
        "usageMetadata.trafficType must reach the IR usage-detail carrier"
    );

    assert_eq!(
        ir.usage.input_tokens, 18,
        "trafficType must not perturb the billed input total"
    );
    assert_eq!(
        ir.usage.output_tokens, 6,
        "trafficType must not perturb the billed output total"
    );

    let out = Protocol::gemini()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["usageMetadata"]["trafficType"], "ON_DEMAND",
        "trafficType must be re-emitted: {out}"
    );
}

/// The top-level `createTime` (RFC3339) reaches the IR's usage-detail carrier (see

#[test]
fn gemini_vertex_create_time_survives() {
    let body = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "the answer"}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4, "totalTokenCount": 7},
        "createTime": "2026-09-07T16:54:20.179017Z",
        "responseId": "POyeasn2CreNtPUP6qaJwQg"
    });
    let ir = Protocol::gemini()
        .reader()
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");
    assert_eq!(
        ir.usage.detail.create_time.as_deref(),
        Some("2026-09-07T16:54:20.179017Z"),
        "createTime must reach the IR's dedicated carrier"
    );

    let out = Protocol::gemini()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["createTime"], "2026-09-07T16:54:20.179017Z",
        "createTime must be re-emitted verbatim: {out}"
    );
}

/// `groundingMetadata.groundingChunks[].web.domain` reaches the IR (stashed in `IrCitation::raw` —

#[test]
fn gemini_vertex_grounding_chunk_domain_survives() {
    let body = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Paris is the capital."}]},
            "finishReason": "STOP",
            "groundingMetadata": {
                "groundingChunks": [{
                    "web": {"uri": "https://atlas", "title": "Atlas", "domain": "atlas.example"}
                }],
                "groundingSupports": [{
                    "segment": {"startIndex": 0, "endIndex": 5, "text": "Paris"},
                    "groundingChunkIndices": [0]
                }]
            }
        }],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4, "totalTokenCount": 7}
    });
    let ir = Protocol::gemini()
        .reader()
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");

    let Some(crate::codec::ir::IrBlock::Text { citations, .. }) = ir
        .content
        .iter()
        .find(|b| matches!(b, crate::codec::ir::IrBlock::Text { .. }))
    else {
        panic!("expected a Text block, got {:?}", ir.content);
    };
    assert_eq!(citations.len(), 1, "the grounding chunk must reach the IR");
    let carried_domain = citations[0]
        .raw
        .as_ref()
        .and_then(|r| r.get("web"))
        .and_then(|w| w.get("domain"))
        .and_then(|d| d.as_str());
    assert_eq!(
        carried_domain,
        Some("atlas.example"),
        "groundingChunks[].web.domain must reach the IR (via the stashed raw chunk)"
    );

    let out = Protocol::gemini()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    let sources = out["candidates"][0]["citationMetadata"]["citationSources"]
        .as_array()
        .expect("citationSources array");
    assert!(
        sources
            .iter()
            .any(|s| s.get("domain").and_then(|d| d.as_str()) == Some("atlas.example")),
        "domain must survive the write: {out}"
    );

    let cited = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Paris is the capital."}]},
            "finishReason": "STOP",
            "citationMetadata": {"citationSources": [
                {"startIndex": 0, "endIndex": 5, "uri": "https://atlas", "title": "Atlas"}
            ]}
        }],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4, "totalTokenCount": 7}
    });
    let ir2 = Protocol::gemini()
        .reader()
        .read_response(crate::codec::test_host::HOST, &cited)
        .expect("read");
    let crate::codec::ir::IrBlock::Text { citations, .. } = &ir2.content[0] else {
        panic!("expected a Text block, got {:?}", ir2.content[0]);
    };
    assert!(
        citations[0]
            .raw
            .as_ref()
            .and_then(|r| r.get("domain"))
            .is_none(),
        "a plain citationSources entry must not fabricate a domain"
    );
    let out2 = Protocol::gemini()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir2);
    let sources2 = out2["candidates"][0]["citationMetadata"]["citationSources"]
        .as_array()
        .expect("citationSources array");
    assert!(
        sources2[0].get("domain").is_none(),
        "no domain must be fabricated: {out2}"
    );
}
