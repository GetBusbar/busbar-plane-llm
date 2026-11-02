//! What the same-dialect relay is allowed to allocate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
thread_local! { static FORWARD_ALLOCATIONS: Cell<u64> = const { Cell::new(0) }; }
struct ForwardAllocator;
// SAFETY: forwards every allocation and layout unchanged to System; the counter is thread-local.
unsafe impl GlobalAlloc for ForwardAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        FORWARD_ALLOCATIONS.with(|n| n.set(n.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        FORWARD_ALLOCATIONS.with(|n| n.set(n.get() + 1));
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        FORWARD_ALLOCATIONS.with(|n| n.set(n.get() + 1));
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}
fn count_forward_allocations(run: impl FnOnce()) -> u64 {
    let before = FORWARD_ALLOCATIONS.with(Cell::get);
    run();
    FORWARD_ALLOCATIONS.with(|n| n.get() - before)
}

#[global_allocator]
static ALLOC: ForwardAllocator = ForwardAllocator;

/// The generation the served door holds: one openai far end, under a pool that names it.
fn served_generation() -> busbar_plane_llm::exchange::shaping::Shaping {
    busbar_plane_llm::exchange::shaping::Shaping::from_settings(&serde_json::json!({
        "providers": { "oai": { "protocol": "openai", "base_url": "https://openai.invalid" } },
        "models": { "gpt-4o": { "provider": "oai" } },
        "pools": { "p": { "members": ["gpt-4o"] } }
    }))
    .expect("a well-formed generation reads")
}

/// The host the gated forward is handed: deterministic stack-only entropy and no clock. A native
struct GateHost;

impl busbar_contract::codec::CodecHost for GateHost {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        out.fill(7);
        true
    }

    fn wall_clock_now(&self) -> Option<u64> {
        None
    }
}

const HOST: &dyn busbar_contract::codec::CodecHost = &GateHost;

/// A canned far-end chat completion (the 1.5.5 gate's upstream answer).
const FAR_OK: &[u8] = br#"{"id":"chatcmpl-gate","object":"chat.completion","created":0,"model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;

/// ONE served forward, as the plane's door runs it for a unit: the arrival read off the caller's
fn one_forward(shaping: &busbar_plane_llm::exchange::shaping::Shaping, body: &[u8]) -> u16 {
    one_forward_with_host(shaping, body, HOST)
}

fn one_forward_with_host(
    shaping: &busbar_plane_llm::exchange::shaping::Shaping,
    body: &[u8],
    host: &dyn busbar_contract::codec::CodecHost,
) -> u16 {
    use busbar_plane_llm::exchange::attempt::{build, stream_intent};
    use busbar_plane_llm::exchange::reply::{At, Reply, ReplyCtx};
    use busbar_plane_llm::exchange::{arrive::arrive, handler_of};
    let caller: &[(&[u8], &[u8])] = &[(b"content-type", b"application/json")];
    let arrived = arrive("POST", "/v1/chat/completions", caller, body, shaping)
        .expect("the caller's request arrives");
    let far =
        build(host, &arrived, caller, shaping, "p", "gpt-4o").expect("the attempt is written");
    assert!(!far.body.is_empty(), "the far end is sent a body");
    let lane = shaping.lane("gpt-4o").expect("the member's lane");
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane,
        intent: handler_of(&arrived)
            .map(|h| stream_intent(h, arrived.parsed.as_ref()))
            .unwrap_or_default(),
        passthrough: false,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let head: &[(&[u8], &[u8])] = &[(b"content-type", b"application/json")];
    let mut reply = Reply::new(host, &ctx, 200, head);
    let at = At {
        now_s: 1_752_000_000,
        elapsed_ms: Some(1),
    };
    let piece = reply.feed(host, &ctx, FAR_OK, true, at);
    assert!(piece.done, "a whole answer ends on its last piece");
    piece.head.map_or(0, |h| h.status)
}

/// COMMITTED BOUND (1.5.5's `FORWARD_PASSTHROUGH_MAX_ALLOCS`): the most allocations ONE warmed
const FORWARD_PASSTHROUGH_MAX_ALLOCS: u64 = 107;

/// LEGACY `crates/busbar-llm/src/engine/tests/alloc_gate_tests.rs::alloc_gate_openai_passthrough_forward`
#[test]
fn alloc_gate_openai_passthrough_forward() {
    let shaping = served_generation();
    let body = br#"{"model":"p","messages":[{"role":"user","content":"hi"}],"max_tokens":16}"#;

    assert_eq!(one_forward(&shaping, body), 200, "passthrough must be 200");
    let mut min_allocs = u64::MAX;
    for _ in 0..4 {
        let mut status = 0;
        let count = count_forward_allocations(|| status = one_forward(&shaping, body));
        assert_eq!(status, 200, "passthrough must be 200");
        min_allocs = min_allocs.min(count);
    }
    println!("served forward openai>openai steady-state min allocations = {min_allocs}");
    assert!(
        min_allocs <= FORWARD_PASSTHROUGH_MAX_ALLOCS,
        "OPENAI>OPENAI FORWARD-PATH ALLOCATION COUNT REGRESSED: measured {min_allocs} > committed \
         bound {FORWARD_PASSTHROUGH_MAX_ALLOCS}. A new per-request allocation on the forward path \
         (or a scaling one) pushed it over."
    );
}

/// How many `messages` entries the large body carries (1.5.5's `ECHO_SCALING_MESSAGES`).
const ECHO_SCALING_MESSAGES: usize = 4_000;

/// COMMITTED BOUND (1.5.5's `ECHO_BODY_SCALING_MAX_ALLOCS`): how many more allocations a request of
const ECHO_BODY_SCALING_MAX_ALLOCS: u64 = 92_000;

/// An openai chat request with `n` messages, asking to STREAM.
fn openai_stream_body(n: usize) -> Vec<u8> {
    let messages: Vec<serde_json::Value> = (0..n)
        .map(|i| serde_json::json!({ "role": "user", "content": format!("message {i}") }))
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "model": "p",
        "messages": messages,
        "max_tokens": 16,
        "stream": true,
    }))
    .expect("serializes")
}

/// LEGACY `crates/busbar-llm/src/engine/tests/alloc_gate_tests.rs::alloc_gate_request_echo_body_materialized_once`
#[test]
fn alloc_gate_request_echo_body_materialized_once() {
    let shaping = served_generation();
    let small_body = openai_stream_body(1);
    let large_body = openai_stream_body(ECHO_SCALING_MESSAGES);

    assert_eq!(one_forward(&shaping, &small_body), 200);
    assert_eq!(one_forward(&shaping, &large_body), 200);
    let mut status = (0, 0);
    let small = count_forward_allocations(|| status.0 = one_forward(&shaping, &small_body));
    let large = count_forward_allocations(|| status.1 = one_forward(&shaping, &large_body));
    assert_eq!(status, (200, 200), "the arm under test must be 200");
    let delta = large.saturating_sub(small);
    println!(
        "buffered delivery arm: small={small} large={large} delta={delta} over \
         {ECHO_SCALING_MESSAGES} messages"
    );
    assert!(
        delta <= ECHO_BODY_SCALING_MAX_ALLOCS,
        "THE BUFFERED DELIVERY ARM GAINED A PER-NODE ALLOCATION WAVE: a body with \
         {ECHO_SCALING_MESSAGES} messages cost {delta} allocations more than a one-message body, \
         over the committed bound {ECHO_BODY_SCALING_MAX_ALLOCS}."
    );
}

#[test]
fn buffered_stream_synthesis_still_refuses_failed_entropy() {
    struct NoEntropy;
    impl busbar_contract::codec::CodecHost for NoEntropy {
        fn fill_entropy(&self, _: &mut [u8]) -> bool {
            false
        }
        fn wall_clock_now(&self) -> Option<u64> {
            None
        }
    }
    assert_eq!(
        one_forward_with_host(&served_generation(), &openai_stream_body(1), &NoEntropy),
        500
    );
}

/// LEGACY `crates/busbar-llm/src/engine/tests/alloc_gate_tests.rs::alloc_gate_dialect_seam_resolution_is_free_for_every_dialect`
#[test]
fn resolving_every_dialect_by_name_allocates_nothing() {
    use busbar_plane_llm::codec::proto_codec::{self as p, ProtocolWriter};
    use busbar_plane_llm::codec::{
        anthropic, bedrock, cohere, decl_of, gemini, openai_chat, openai_responses,
    };

    fn resolution_costs_what_the_writer_costs(
        name: &'static str,
        writer: &dyn ProtocolWriter,
        body: &serde_json::Value,
    ) {
        let codec = decl_of(name)
            .and_then(busbar_contract::protocol::ProtocolDecl::dialect)
            .unwrap_or_else(|| panic!("{name} declares a codec"));

        let _ = codec.requested_candidate_count(body);
        let _ = writer.requested_candidate_count(body);
        let direct = count_forward_allocations(|| {
            let _ = writer.requested_candidate_count(body);
        });
        let resolved = count_forward_allocations(|| {
            let codec = decl_of(name)
                .and_then(busbar_contract::protocol::ProtocolDecl::dialect)
                .expect("declared");
            let _ = codec.requested_candidate_count(body);
        });
        assert_eq!(
            resolved, direct,
            "asking the `{name}` dialect one stateless question cost {resolved} allocation(s) \
             through its resolution but {direct} on the writer itself"
        );
    }

    let body = serde_json::json!({ "model": "m", "messages": [] });

    let anthropic = anthropic::AnthropicWriter;
    let bedrock = bedrock::BedrockWriter;
    let cohere = cohere::CohereWriter;
    let gemini = gemini::GeminiWriter;
    let openai = openai_chat::OpenAiWriter;
    let responses = openai_responses::ResponsesWriter;
    resolution_costs_what_the_writer_costs(p::PROTO_ANTHROPIC, &anthropic, &body);
    resolution_costs_what_the_writer_costs(p::PROTO_BEDROCK, &bedrock, &body);
    resolution_costs_what_the_writer_costs(p::PROTO_COHERE, &cohere, &body);
    resolution_costs_what_the_writer_costs(p::PROTO_GEMINI, &gemini, &body);
    resolution_costs_what_the_writer_costs(p::PROTO_OPENAI, &openai, &body);
    resolution_costs_what_the_writer_costs(p::PROTO_RESPONSES, &responses, &body);
}
