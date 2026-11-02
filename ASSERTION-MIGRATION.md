# LLM ordinary-test migration

Baseline d19e29ac319e7f9dc23e7da4a56ac25cde1bc203 (accepted b5b2129 lineage).
SDK a35493a5e74cc935ff7dcc15c1af869290da91cb, peer pins, lockfile and public/policy pins
remain unchanged. No test execution or runtime-equivalence claim accompanies this source change.

## Diagnostic assertions

The former warning subscriber is replaced by a codec-domain event layer under tests/support.
It returns explicit CodecDiagnostic values (message, path, diag, media_kind and ordered fields)
and a scoped tracing Dispatch. No generic capture class, process-global subscriber, gate,
framework registry, forwarding fake or replacement package exists. WARN and DEBUG windows
remain distinct. Rendering of each individual event preserves the original message + ordered
key=value field spelling, including the trailing separating space when fields are absent.
All existing contains/count/empty and per-event equality assertions retain their predicates.
No diagnostic golden is widened, and no warning assertion is retired.

The following source-marker counts are conserved in each consumer file (all assertions in each
file, not a claim that all of them are diagnostics). The diff records each original predicate
converted to an explicit event snapshot/iterator.

| Consumer | Before | After |
|---|---:|---:|
| `plane-llm/src/codec/anthropic/tests/anthropic_hardening_tests.rs` | 377 | 377 |
| `plane-llm/src/codec/anthropic/tests/field_carry_tests.rs` | 109 | 109 |
| `plane-llm/src/codec/anthropic/tests/reasoning_carry_tests.rs` | 50 | 50 |
| `plane-llm/src/codec/bedrock/tests/handler_tests.rs` | 16 | 16 |
| `plane-llm/src/codec/bedrock/tests/identity_projection_regressions.rs` | 9 | 9 |
| `plane-llm/src/codec/bedrock/tests/tests.rs` | 481 | 481 |
| `plane-llm/src/codec/bedrock/tests/titan_roundtrip_regression_tests.rs` | 17 | 17 |
| `plane-llm/src/codec/bedrock/tests/usage_census_tests.rs` | 29 | 29 |
| `plane-llm/src/codec/cohere/tests/egress_media_regression_tests.rs` | 3 | 3 |
| `plane-llm/src/codec/cohere/tests/rerank_tests.rs` | 45 | 45 |
| `plane-llm/src/codec/gemini/tests/handler_tests.rs` | 86 | 86 |
| `plane-llm/src/codec/openai_chat/tests/audio_format_regression_tests.rs` | 4 | 4 |
| `plane-llm/src/codec/openai_chat/tests/handler_tests.rs` | 123 | 123 |
| `plane-llm/src/codec/openai_chat/tests/tests.rs` | 497 | 497 |
| `plane-llm/src/codec/openai_chat/tests/usage_census_tests.rs` | 12 | 12 |
| `plane-llm/src/codec/openai_responses/tests/field_carry_tests.rs` | 129 | 129 |
| `plane-llm/src/codec/openai_responses/tests/tests.rs` | 835 | 835 |
| `plane-llm/src/codec/openai_responses/tests/usage_census_tests.rs` | 11 | 11 |
| `plane-llm/src/codec/tests/drop_index_tests.rs` | 34 | 34 |
| `plane-llm/src/codec/tests/drop_names_tests.rs` | 7 | 7 |
| `plane-llm/src/codec/tests/drop_writer_tests.rs` | 60 | 60 |
| `plane-llm/src/codec/tests/drops_tests.rs` | 55 | 55 |
| `plane-llm/src/codec/tests/eventstream_tests.rs` | 124 | 124 |
| `plane-llm/src/codec/tests/image_detail_warn_tests.rs` | 6 | 6 |
| `plane-llm/src/codec/tests/ir_seam_tests.rs` | 200 | 200 |
| `plane-llm/src/codec/tests/proto/gemini_tests.rs` | 76 | 76 |
| `plane-llm/src/codec/tests/proto/max_tokens_precedence_tests.rs` | 9 | 9 |
| `plane-llm/src/codec/tests/proto/response_format_matrix_tests.rs` | 38 | 38 |
| `plane-llm/tests/bedrock_identity_regressions.rs` | 43 | 43 |
| `plane-llm/tests/exchange_attempt.rs` | 188 | 188 |

## Actual-door conformance

`plane-llm-plugin/tests/conformance.rs` now calls the actual LLM linked door and actual freshly
built dropped cdylib directly. BUSBAR_PLUGIN_CDYLIB must be an absolute existing artifact; absence
fails. Existing both_ways.rs and migrate.rs remain, including their artifact contract.

The pinned macro's actual LLM operations become inline first-crossing assertions, including
facts, invalid/valid validation, unopened arrival, open, READY, double-open refusal, hydrate/start,
claimed/unclaimed arrivals, kernel attempt/caller body/far-end reply, narrow answer and each more
recall, short-buffer + one replay, refusal, serve, project, driver wake, release, tick, cancel,
refresh/retire/close and post-close fault. Snapshot claims, request/response bytes, short capacity,
status, refusal, route and class census assertions remain. Resumes are observed by subtracting
them from the exact first-invocation counter; as in the original suite they are not pinned.

The eight concrete sessions are 32 explicit real-product operations with literal expected-field
assertions: Anthropic server tools, OpenAI stated total, Cohere billed units, three Bedrock
guardrail variants, OpenAI images and transcription. The generic Subject/Recorder/Fold/Act/Session
engine and runtime expectation interpreter are absent. The retained helpers only construct,
lend and inspect the plane ABI frames and buffers exercised by these tests.

Direct malformed metadata tests retain ABI +1/-1, layout xor1/zero, three Statement-mismatch
branches, all six wrong-kind loads linked/dropped, plane answer-layout refusal, and actual six-need no-table/Probe behavior. Failed READY
retains its exact plugin text and one invocation. Declared-needs/profile checks remain.

The allocation gates use a binary-local System allocator counter for alloc/alloc_zeroed/realloc.
All existing warmed-forward, echo-scaling and host-resolution allocation thresholds remain exact.
No payload inspection, allocator framework or new fake product is introduced.

## Framework selfchecks retired, and unresolved core mapping

- Moved-count comparator RED and injected unreported billing-class RED exercise removed framework
  bookkeeping. Positive exact crossing pins and the actual declared/reported billing-class census
  remain. Existing real-door altered-unit-count RED in both_ways.rs remains.
- Canary with no configured credentials did no work for these LLM inputs.
- Transport frame-piece, outbound wrong-field/no-field/second-token and newer-store-schema checks
  were no-ops for this kind/input. No product behavior is retired with them.
- The undeclared-need and foreign-connection generic arms formerly replaced the LLM's real
  needs with a synthetic singleton. Their source transfer is ACKed once on the real GitHub
  product: candidate abd7663be59b670c8cceabaf30475334e771c566, tree
  86a49682b4f52fbe99edd6247e73b207e4398285. `support/remaining.rs::custody::github_connections_refuse_undeclared_needs_and_other_instances`
  preserves actual-need open/close, next+u32::MAX exact UndeclaredNeed refusal, foreign READ/WRITE/CLOSE
  exact NotOwner refusal, and owner-write/close after refusals, through linked/dropped origins.
  The shared ACK also names GitHub's NoConnectionTable/Probe test, but that does not replace
  this product's declaration check: `llm_serving_requires_all_six_actual_http_needs_in_both_deliveries`
  separately retains exact linked/dropped named NoConnectionTable{needs:6}, all-six actual HTTP
  declarations and allowed Probe behavior over the actual LLM door.
  Authority: A/diet-loader-d306/transfer-packets/generic-connection-negative-transfer-ack.json,
  status SOURCE_EQUIVALENCE_ACK_FOR_THREE_ARMS_NATIVE_PENDING (doc_macro_bindings).
  This source ACK is limited to these three shared loader arms; GitHub native execution is pending,
  and it waives no per-product behavior, declaration, callback, TLS or guard assertion.

## Root composition hooks intentionally held

Exact root migration ACK is pending. Existing references remain in their original source:

- tests/on_driver/seam.rs::request_id_counter_is_unique_and_monotonic_across_sequential_requests:
  two TestApp::build calls; a,b,c strictly increase and differ by exactly one; an independent
  boot seed's first value d differs from a. Replace only with an actual root AppState composition.
- tests/on_driver/projection.rs::hook_content_uncapped_by_default_and_omits_whole_when_opted_in:
  LIMITS_TEST_LOCK protects actual global content-limit transitions from default 0 (full 200000
  characters) to 64KiB and back, including the whole-body omission branches. Preserve the shared
  root lock across all limit-mutating tests; a private replacement lock would weaken exclusion.

These root-owned consumers are not native package tests. Neither is deleted, skipped, renamed
away or replaced by a fake composition. Root source transfer was requested from finish_kernel.

Validation: cargo fmt --all -- --check and git diff --check pass. No build ran without the
resource coordinator's lane approval. Native compilation/live execution, final SDK compatibility,
root composition transfer, GitHub host-safety native execution and reusable CI artifact production remain explicit
qualification work.

## Native qualification follow-up

The native c5dfa854 candidate built its actual library. Logic compilation found one mechanical
snapshot typo: baseline_cap.messages() had become baseline_diagnostic_texts(&cap). It now reads
`diagnostic_texts(&baseline_cap)`, preserving the original baseline-empty assertion. Two captures
with no original diagnostic assertions keep their scoped Dispatch but name the unobserved handles
`_cap`; no assertion is removed.

The existing both_ways translation-cap test used Dispatcher::new with no HostServices. The actual
DoorHost refuses entropy in that setup; cross-dialect OpenAI response synthesis uses CheckedEntropy
and answers an internal 500 while still reporting the far end's 7/3 usage. That fixture now supplies
only a direct host entropy/clock implementation for this test's dispatcher, obtaining bytes from
OS getrandom. Other host services refuse. Both actual deliveries retain the original default-cap
200/chat.completion and refreshed-64-byte-cap refusal assertions; an additional per-door counter
asserts the actual translation draws host entropy. No product logic, expected answer or cap changes.
Source causality and the corrected candidate's native result require the lane executor's rerun.
