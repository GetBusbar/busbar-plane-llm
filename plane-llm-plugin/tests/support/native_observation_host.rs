// SPDX-License-Identifier: Apache-2.0
//! Host callback fixture for real linked/drop native observation delivery.
//! Stores exact conditional records and requires scope/receipt custody before success.
use busbar_contract::services::*;
#[derive(Default)]
pub struct NativeHost {
    pub state: std::sync::Mutex<State>,
    pub fills: std::sync::atomic::AtomicU64,
}
impl HostServices for NativeHost {
    fn supports_record_transactions(&self) -> bool {
        true
    }
    fn conditional_record_transaction(
        &self,
        caller: &Caller,
        unit: Option<u64>,
        candidate: conditional::ConditionalCandidate,
        acked: Box<dyn FnOnce(conditional::ConditionalResult) + Send>,
    ) -> Result<(), &'static str> {
        assert_eq!(caller.namespace().as_ref(), "llm-concrete-cases");
        let mut state = self.state.lock().unwrap();
        assert!(state.scopes.contains_key(&unit.unwrap()));
        assert!(candidate.chains.is_empty());
        for predicate in &candidate.predicates {
            assert_eq!(predicate.kind, 0);
            let old = state.records.get(&predicate.key);
            assert!(
                match &predicate.expected {
                    Expected::Absent => old.is_none(),
                    Expected::Bytes(bytes) => old == Some(bytes),
                },
                "exact original bytes required"
            );
        }
        for mutation in candidate.mutations {
            match mutation {
                conditional::RecordMutation::Put { kind, key, value } => {
                    assert_eq!(kind, 0);
                    state.records.insert(key, value);
                }
                conditional::RecordMutation::Delete { kind, key } => {
                    assert_eq!(kind, 0);
                    state.records.remove(&key);
                }
            }
        }
        state.publications += 1;
        drop(state);
        acked(conditional::ConditionalResult {
            correlation: candidate.correlation,
            decision: Ok(Decision::Applied),
        });
        Ok(())
    }
    fn usage_observation_input(
        &self,
        _: &Caller,
        ticket: Ticket,
        unit: Option<u64>,
    ) -> Option<Vec<u8>> {
        let state = self.state.lock().unwrap();
        let mut bytes = extensions::encode(&[(observation::CAPABILITY, b"1")]);
        if let Some(scope) = state.scopes.get(&unit?) {
            bytes.extend(extensions::encode(&[(observation::SCOPE, scope)]));
        }
        if let Some(result) = state.results.get(&ticket) {
            bytes.extend_from_slice(result);
        }
        Some(bytes)
    }
    fn usage_observation_scope(
        &self,
        _: &Caller,
        ticket: Ticket,
        unit: Option<u64>,
        request: observation::ScopeRequest,
        done: ObservationDone,
    ) -> Result<(), &'static str> {
        let unit = unit.unwrap();
        let mut state = self.state.lock().unwrap();
        let scope = if let Some(scope) = request.scope {
            assert_eq!(state.scopes.get(&unit), Some(&scope));
            scope
        } else {
            assert_eq!(
                request.required_classes,
                if unit == 90 {
                    vec![22, 23, 24]
                } else {
                    vec![25, 26, 27]
                }
            );
            let scope = [unit as u8; 32];
            assert!(state.scopes.insert(unit, scope).is_none());
            scope
        };
        if request.close {
            assert!(state.observations.iter().any(|o| o.scope == scope));
            state.closed.insert(unit);
        }
        let result = extensions::encode(&[(
            observation::RESULT,
            &serde_json::to_vec(&serde_json::json!({"scope":scope,"closed":request.close}))
                .unwrap(),
        )]);
        state.results.insert(ticket, result.clone());
        drop(state);
        done(Ok(result));
        Ok(())
    }
    fn usage_observation_submit(
        &self,
        _: &Caller,
        ticket: Ticket,
        unit: Option<u64>,
        observation: observation::Observation,
        done: ObservationDone,
    ) -> Result<(), &'static str> {
        let mut state = self.state.lock().unwrap();
        assert_eq!(state.scopes.get(&unit.unwrap()), Some(&observation.scope));
        assert!(
            state.publications >= 2,
            "provider evidence must be persisted first"
        );
        assert!(!state
            .observations
            .iter()
            .any(|old| old.component_key == observation.component_key
                && old.scope == observation.scope));
        state.observations.push(observation);
        let receipt = observation::Receipt {
            settlement_id: [state.observations.len() as u8; 16],
            duplicate: false,
            quarantined: false,
        };
        let result =
            extensions::encode(&[(observation::RESULT, &serde_json::to_vec(&receipt).unwrap())]);
        state.results.insert(ticket, result.clone());
        drop(state);
        done(Ok(result));
        Ok(())
    }
    fn usage_observation_retire(&self, _: &Caller, ticket: Ticket, _: Option<u64>) {
        self.state.lock().unwrap().results.remove(&ticket);
    }
    fn now(&self) -> Reading {
        Reading {
            wall_ns: 0,
            mono_ns: busbar_plugin_loader::dispatch::now_ns(),
        }
    }
    fn snapshot_read(
        &self,
        _caller: &Caller,
        _scope: u32,
        _later: busbar_contract::services::Later,
    ) -> Ran {
        Ran::Now((Snapshot::Refused(UNSERVED)).stored())
    }
    fn dest_judge(&self, _dest: &str, _class: u32, _flags: u32, _later: Option<Later>) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn records_get(&self, _caller: &Caller, _kind: &str, _key: &[u8], _later: Later) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn records_list(&self, _caller: &Caller, _list: RecordsList, _later: Later) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn records_claim(
        &self,
        _caller: &Caller,
        _kind: &str,
        _key: &[u8],
        _ttl_ms: u64,
        _later: Later,
    ) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn sign(&self, _caller: &Caller, _data: &[u8]) -> Stored {
        Stored::refused(UNSERVED)
    }
    fn trust_sight(
        &self,
        _caller: &Caller,
        _counterparty: &str,
        _hash: &str,
        _later: Later,
    ) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn trust_unreached(&self, _caller: &Caller, _counterparty: &str) -> Stored {
        Stored::refused(UNSERVED)
    }
    fn trust_sight_item(
        &self,
        _caller: &Caller,
        _counterparty: &str,
        _item: &str,
        _digest: &str,
        _later: busbar_contract::services::Later,
    ) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn trust_decide(
        &self,
        _caller: &Caller,
        _ticket: busbar_contract::abi::mechanism::ticket::Ticket,
        _key: TrustKeyRef<'_>,
        _expected: Option<&str>,
        _approve: bool,
        _later: busbar_contract::services::Later,
    ) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn trust_state(&self, _caller: &Caller, _counterparty: &str) -> Stored {
        Stored::refused(UNSERVED)
    }
    fn trust_serves(
        &self,
        _caller: &Caller,
        _counterparty: &str,
        _item: Option<&str>,
        _digest: Option<&str>,
    ) -> Stored {
        Stored::refused(UNSERVED)
    }
    fn trust_due(&self, _caller: &Caller) -> Stored {
        Stored::refused(UNSERVED)
    }
    fn trust_verify(
        &self,
        _caller: &Caller,
        _counterparty: &str,
        _payload: &[u8],
        _signatures: &[u8],
    ) -> Stored {
        Stored::refused(UNSERVED)
    }
    fn entitlement_check(&self, _caller: &Caller, _unit: Option<u64>, _target: &str) -> Stored {
        Stored::refused(UNSERVED)
    }
    fn session_emit(&self, _caller: &Caller, _session: u64, _bytes: &[u8]) -> Stored {
        Stored::refused(UNSERVED)
    }
    fn ticket_mint(
        &self,
        _caller: &Caller,
        _unit: Option<u64>,
        _ttl_secs: u64,
        _record: &[u8],
        _later: busbar_contract::services::Later,
    ) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn ticket_take(&self, _caller: &Caller, _unit: Option<u64>) -> Stored {
        Stored::refused(UNSERVED)
    }
    fn random_fill(&self, len: u64) -> Stored {
        assert!(len > 0 && len <= busbar_contract::abi::host::service::MAX_RANDOM_FILL);
        let mut bytes = vec![0; len as usize];
        getrandom::fill(&mut bytes).expect("OS entropy for actual LLM translation");
        self.fills.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut stored = Stored::ready(0);
        stored.bytes = bytes;
        stored
    }
    fn records_secret(&self, _kind: &str, _id: &str, _later: Later) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn unit_nest(&self, _caller: &Caller, _unit: Option<u64>, _ask: NestAsk, _later: Later) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn work_open(
        &self,
        _caller: &Caller,
        unit: Option<u64>,
        kind: &str,
        record: &[u8],
        _later: Later,
    ) -> Ran {
        assert_eq!(kind, "interaction_runs");
        assert!(busbar_contract::abi::sdk::recovery::poll_target(record)
            .unwrap()
            .is_some());
        let mut state = self.state.lock().unwrap();
        assert!(
            !state.records.is_empty(),
            "intent must be durable before work registration"
        );
        let handle = unit.unwrap();
        assert!(state.work.insert(handle, record.to_vec()).is_none());
        let mut stored = Stored::ready(handle);
        stored.bytes = b"work-reference".to_vec();
        stored
            .spans
            .push(busbar_contract::abi::host::service::ItemSpan {
                key: busbar_contract::abi::mechanism::call::Span { offset: 0, len: 14 },
                value: busbar_contract::abi::mechanism::call::Span {
                    offset: u32::MAX,
                    len: 0,
                },
                ..busbar_contract::abi::host::service::ItemSpan::ZERO
            });
        Ran::Now(stored)
    }
    fn work_find(
        &self,
        _caller: &Caller,
        _unit: Option<u64>,
        _reference: &[u8],
        _later: Later,
    ) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn work_settle(
        &self,
        _caller: &Caller,
        unit: Option<u64>,
        handle: u64,
        record: &[u8],
        _later: Later,
    ) -> Ran {
        let mut state = self.state.lock().unwrap();
        assert_eq!(state.work.remove(&handle).as_deref(), Some(record));
        assert!(state.closed.contains(&unit.unwrap()));
        Ran::Now(Stored::ready(0))
    }
    fn work_resume(
        &self,
        _caller: &Caller,
        _unit: Option<u64>,
        _handle: u64,
        _later: Later,
    ) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn disk_append(&self, _dest: &DiskDest, _bytes: Vec<u8>, _later: Later) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn verify_lookup(&self, _caller: &Caller, _key: &[u8], _later: Later) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn verify_store(&self, _caller: &Caller, _key: &[u8], _entry: &[u8], _ttl_ms: u64) -> Stored {
        Stored::refused(UNSERVED)
    }
    fn content_scan(
        &self,
        _caller: &Caller,
        _unit: Option<u64>,
        _content: &[u8],
        _later: Later,
    ) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
    fn hook_call(&self, _caller: &Caller, _unit: Option<u64>, _ask: HookAsk, _later: Later) -> Ran {
        Ran::Now(Stored::refused(UNSERVED))
    }
}

use busbar_contract::abi::{
    mechanism::{extensions, ticket::Ticket},
    sdk::plane::{conditional, observation},
};
use busbar_contract::host_records::{Decision, Expected};
use std::collections::{BTreeMap, BTreeSet, HashMap};
#[derive(Default)]
pub struct State {
    records: BTreeMap<Vec<u8>, Vec<u8>>,
    scopes: BTreeMap<u64, [u8; 32]>,
    results: HashMap<Ticket, Vec<u8>>,
    work: BTreeMap<u64, Vec<u8>>,
    pub observations: Vec<observation::Observation>,
    pub closed: BTreeSet<u64>,
    pub publications: usize,
}
