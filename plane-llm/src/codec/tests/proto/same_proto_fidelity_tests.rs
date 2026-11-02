use super::StreamTranslate;

fn run_same_proto(proto: &str, chunks: &[&[u8]]) -> (Vec<u8>, StreamTranslate) {
    let mut t = StreamTranslate::new_same_proto(proto).expect("same-proto translator");
    let mut out = Vec::new();
    for c in chunks {
        out.extend_from_slice(&t.feed(crate::codec::test_host::HOST, c));
    }
    out.extend_from_slice(&t.finish(crate::codec::test_host::HOST));
    (out, t)
}

fn concat(chunks: &[&[u8]]) -> Vec<u8> {
    chunks.iter().flat_map(|c| c.iter().copied()).collect()
}

#[test]
fn anthropic_sse_round_trip_byte_exact() {
    let frames: &[&[u8]] = &[
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":7}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];
    let (out, t) = run_same_proto("anthropic", frames);
    assert_eq!(
        out,
        concat(frames),
        "anthropic same-proto SSE must be byte-exact"
    );
    let u = t.usage().expect("anthropic A-tap usage");

    assert_eq!((u.input_tokens, u.output_tokens), (11, 7));
}

#[test]
fn openai_bare_data_round_trip_byte_exact() {
    let frames: &[&[u8]] = &[
            b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":13,\"completion_tokens\":9,\"total_tokens\":22}}\n\n",
            b"data: [DONE]\n\n",
        ];
    let mut t = StreamTranslate::new_same_proto("openai").expect("same-proto translator");
    t.set_client_include_usage(true);
    let mut out = Vec::new();
    for c in frames {
        out.extend_from_slice(&t.feed(crate::codec::test_host::HOST, c));
    }
    out.extend_from_slice(&t.finish(crate::codec::test_host::HOST));
    assert_eq!(
        out,
        concat(frames),
        "openai bare data: opted-in same-proto must be byte-exact (incl the trailing usage chunk + [DONE])"
    );
    let u = t.usage().expect("openai A-tap usage");
    assert_eq!((u.input_tokens, u.output_tokens), (13, 9));
}

#[test]
fn gemini_sse_round_trip_byte_exact() {
    let frames: &[&[u8]] = &[
            b"data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hi\"}]}}]}\n\n",
            b"data: {\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"\"}]}}],\"usageMetadata\":{\"promptTokenCount\":15,\"candidatesTokenCount\":4,\"totalTokenCount\":19}}\n\n",
        ];
    let (out, t) = run_same_proto("gemini", frames);
    assert_eq!(
        out,
        concat(frames),
        "gemini same-proto SSE must be byte-exact"
    );
    let u = t.usage().expect("gemini A-tap usage");
    assert_eq!((u.input_tokens, u.output_tokens), (15, 4));
}

#[test]
fn cohere_sse_round_trip_byte_exact() {
    let frames: &[&[u8]] = &[
            b"event: message-start\ndata: {\"type\":\"message-start\",\"id\":\"co_1\"}\n\n",
            b"event: content-delta\ndata: {\"type\":\"content-delta\",\"index\":0,\"delta\":{\"message\":{\"content\":{\"text\":\"hi\"}}}}\n\n",
            b"event: message-end\ndata: {\"type\":\"message-end\",\"delta\":{\"finish_reason\":\"COMPLETE\",\"usage\":{\"tokens\":{\"input_tokens\":17,\"output_tokens\":6}}}}\n\n",
        ];
    let (out, t) = run_same_proto("cohere", frames);
    assert_eq!(
        out,
        concat(frames),
        "cohere same-proto SSE must be byte-exact"
    );
    let u = t.usage().expect("cohere A-tap usage");
    assert_eq!((u.input_tokens, u.output_tokens), (17, 6));
}

#[test]
fn responses_sse_round_trip_byte_exact() {
    let frames: &[&[u8]] = &[
            b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":21,\"output_tokens\":8}}}\n\n",
        ];
    let (out, t) = run_same_proto("responses", frames);
    assert_eq!(
        out,
        concat(frames),
        "responses same-proto SSE must be byte-exact"
    );
    let u = t.usage().expect("responses A-tap usage");
    assert_eq!((u.input_tokens, u.output_tokens), (21, 8));
}

#[test]
fn bedrock_binary_eventstream_round_trip_byte_exact() {
    use crate::codec::eventstream::encode_frame;
    let mut input = Vec::new();
    input.extend(encode_frame("messageStart", br#"{"role":"assistant"}"#));
    input.extend(encode_frame(
        "contentBlockDelta",
        br#"{"contentBlockIndex":0,"delta":{"text":"hi"}}"#,
    ));
    input.extend(encode_frame("messageStop", br#"{"stopReason":"end_turn"}"#));
    input.extend(encode_frame(
        "metadata",
        br#"{"usage":{"inputTokens":31,"outputTokens":12},"metrics":{"latencyMs":5}}"#,
    ));

    let mid = input.len() / 2;
    let (out, t) = run_same_proto("bedrock", &[&input[..mid], &input[mid..]]);
    assert_eq!(
        out, input,
        "bedrock same-proto binary eventstream must be re-emitted byte-for-byte (no re-encode)"
    );
    let u = t.usage().expect("bedrock A-tap usage");
    assert_eq!((u.input_tokens, u.output_tokens), (31, 12));
}

#[test]
fn same_proto_cross_chunk_split_is_verbatim() {
    let whole: &[u8] = b"data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n";
    let split = whole.len() / 2;
    let (out, _t) = run_same_proto("openai", &[&whole[..split], &whole[split..]]);
    assert_eq!(
        out, whole,
        "a frame split across feeds must re-emit byte-exact once complete"
    );
}

/// BENCHMARK: same-proto streaming per-feed latency / throughput. The short-circuit's

#[test]
#[ignore = "a timing bench: run with --release --ignored --nocapture"]
fn bench_same_proto_short_circuit() {
    use crate::codec::eventstream::encode_frame;

    let mut openai_frames: Vec<Vec<u8>> = (0..32)
            .map(|i| {
                format!(
                    "data: {{\"id\":\"chatcmpl-b\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"tok{i} \"}},\"finish_reason\":null}}]}}\n\n"
                )
                .into_bytes()
            })
            .collect();
    openai_frames.extend([
        b"data: {\"id\":\"chatcmpl-b\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_vec(),
        b"data: {\"id\":\"chatcmpl-b\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":13,\"completion_tokens\":32,\"total_tokens\":45}}\n\n".to_vec(),
        b"data: [DONE]\n\n".to_vec(),
    ]);
    let mut bedrock_frames = vec![encode_frame("messageStart", br#"{"role":"assistant"}"#)];
    bedrock_frames.extend((0..32).map(|i| {
        encode_frame(
            "contentBlockDelta",
            format!("{{\"contentBlockIndex\":0,\"delta\":{{\"text\":\"tok{i} \"}}}}").as_bytes(),
        )
    }));
    bedrock_frames.extend([
        encode_frame("messageStop", br#"{"stopReason":"end_turn"}"#),
        encode_frame(
            "metadata",
            br#"{"usage":{"inputTokens":31,"outputTokens":32},"metrics":{"latencyMs":5}}"#,
        ),
    ]);

    fn bench_memcpy(frames: &[Vec<u8>], iters: u64) -> f64 {
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let mut out: Vec<u8> = Vec::new();
            for f in frames {
                out.extend_from_slice(std::hint::black_box(f));
            }
            std::hint::black_box(out);
        }
        start.elapsed().as_nanos() as f64 / iters as f64
    }

    fn bench_translator(ctor: &dyn Fn() -> StreamTranslate, frames: &[Vec<u8>], iters: u64) -> f64 {
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let mut t = ctor();
            let mut out: Vec<u8> = Vec::new();
            for f in frames {
                out.extend_from_slice(
                    &t.feed(crate::codec::test_host::HOST, std::hint::black_box(f)),
                );
            }
            out.extend_from_slice(&t.finish(crate::codec::test_host::HOST));
            std::hint::black_box(out);
        }
        start.elapsed().as_nanos() as f64 / iters as f64
    }

    let iters = 50_000u64;

    let openai_sc = || {
        let mut translator = StreamTranslate::new_same_proto("openai").unwrap();
        translator.set_client_include_usage(true);
        translator
    };
    let openai_xproto = || StreamTranslate::new("anthropic", "openai").unwrap();
    let bedrock_sc = || StreamTranslate::new_same_proto("bedrock").unwrap();
    let bedrock_xproto = || StreamTranslate::new("anthropic", "bedrock").unwrap();

    for (ctor, frames, input) in [
        (
            &openai_sc as &dyn Fn() -> StreamTranslate,
            &openai_frames,
            13,
        ),
        (
            &openai_xproto as &dyn Fn() -> StreamTranslate,
            &openai_frames,
            13,
        ),
        (
            &bedrock_sc as &dyn Fn() -> StreamTranslate,
            &bedrock_frames,
            31,
        ),
        (
            &bedrock_xproto as &dyn Fn() -> StreamTranslate,
            &bedrock_frames,
            31,
        ),
    ] {
        let mut translator = ctor();
        let mut output = Vec::new();
        for frame in frames {
            output.extend(translator.feed(crate::codec::test_host::HOST, frame));
        }
        output.extend(translator.finish(crate::codec::test_host::HOST));
        assert!(!translator.aborted());
        assert!(translator.terminal_error().is_none());
        assert!(!translator.local_encoding_failure());
        for index in 0..32 {
            let text = format!("tok{index} ");
            assert!(output
                .windows(text.len())
                .any(|bytes| bytes == text.as_bytes()));
        }
        let billed = translator
            .usage()
            .expect("the stream supplies real usage")
            .to_token_usage();
        assert_eq!((billed.input, billed.output), (input, 32));
    }

    let _ = bench_memcpy(&openai_frames, 2_000);
    let _ = bench_translator(&openai_sc, &openai_frames, 2_000);
    let _ = bench_translator(&openai_xproto, &openai_frames, 2_000);

    for (label, sc_ctor, xproto_ctor, frames) in [
        (
            "openai-sse",
            &openai_sc as &dyn Fn() -> StreamTranslate,
            &openai_xproto as &dyn Fn() -> StreamTranslate,
            &openai_frames,
        ),
        (
            "bedrock-eventstream",
            &bedrock_sc as &dyn Fn() -> StreamTranslate,
            &bedrock_xproto as &dyn Fn() -> StreamTranslate,
            &bedrock_frames,
        ),
    ] {
        let memcpy = bench_memcpy(frames, iters);
        let sc = bench_translator(sc_ctor, frames, iters);
        let reserialize = bench_translator(xproto_ctor, frames, iters);
        println!(
            "BENCH same_proto[{label}] ({} frames/stream, {iters} streams): memcpy floor \
                 {memcpy:.0} ns/stream | short-circuit {sc:.0} ns/stream | re-serialize baseline \
                 {reserialize:.0} ns/stream => short-circuit is {:.2}x the re-serialize cost",
            frames.len(),
            sc / reserialize.max(1.0)
        );

        assert!(
            sc <= reserialize * 1.25,
            "same_proto[{label}] short-circuit {sc:.0} ns is NOT faster than the re-serialize \
                 baseline {reserialize:.0} ns — the verbatim short-circuit regressed (did it \
                 re-serialize the IR instead of re-emitting original bytes?)"
        );
    }
}
