//! Request-driven native lifecycle. All paid dispatch and settlement barriers use durable host
//! receipts; restored records never manufacture another create permit.

use super::{
    crossing::{Completion, Crossing, Progress},
    interactions::{valid_id, Binding},
    lifecycle::{self, Checkpoint, CreatePlan, Partition, Recovery, Run},
    lookup::{Found, RECORD_INDEX},
};
use busbar_contract::abi::sdk::plane::observation::{Observation, ObservationResult, ScopeRequest};

/// A callback either returns one owned pending intent or has completed this lifecycle barrier.
pub enum Step {
    /// Lease these bytes and return PENDING without consuming the current piece.
    Pending(Vec<u8>),
    /// The caller can consume its piece. This does not assert that unresolved usage is finalized.
    Ready,
}

enum AfterScope {
    Create {
        partition: Partition,
        intent: String,
        binding: Box<Binding>,
    },
    Existing,
}
enum AfterPublish {
    Ready,
    Reconcile,
    Response,
}
enum Publication {
    Create(CreatePlan),
    Checkpoint(Checkpoint),
}
enum Stage {
    Scope(Crossing, AfterScope),
    Publish(Crossing, Box<Publication>, AfterPublish),
    Component(Crossing, Observation),
    Close(Crossing),
    Ready,
    Failed,
}

/// One admitted native request, retaining durable run bytes for exact conditional updates.
/// Dropping this object never closes a scope or discards the host's durable unresolved liability.
pub struct Session {
    run: Option<Run>,
    original: Vec<u8>,
    stage: Stage,
    correlation: u64,
    dispatched: bool,
    responded: bool,
    unresolved_final: bool,
    response: Option<(u16, Vec<u8>, bool)>,
}
impl Session {
    /// Start only after concrete route selection, request/profile validation and capability checks.
    /// `intent` is host-random; `binding.scope` is replaced by the newly issued scope.
    pub fn create(
        partition: Partition,
        intent: String,
        binding: Binding,
        input: &[u8],
    ) -> Result<Self, &'static str> {
        if !valid_id(&intent) {
            return Err("invalid create intent");
        }
        Ok(Self {
            run: None,
            original: Vec::new(),
            correlation: 0,
            dispatched: false,
            responded: false,
            unresolved_final: false,
            response: None,
            stage: Stage::Scope(
                Crossing::scope(ScopeRequest::create_requiring([0, 1, 2]), input)?,
                AfterScope::Create {
                    partition,
                    intent,
                    binding: Box::new(binding),
                },
            ),
        })
    }
    /// Attach the original host scope before any stored-object provider operation. Possession of
    /// the protected index alone is not host authorization and never creates a replacement scope.
    pub fn stored(found: Found, input: &[u8]) -> Result<Self, &'static str> {
        let stage = Stage::Scope(
            Crossing::scope(
                ScopeRequest::attach_requiring(found.run.binding().scope, [0, 1, 2]),
                input,
            )?,
            AfterScope::Existing,
        );
        Ok(Self {
            run: Some(found.run),
            original: found.record,
            stage,
            correlation: 0,
            dispatched: false,
            responded: false,
            unresolved_final: false,
            response: None,
        })
    }
    /// The original run is available only after scope/create publication has completed.
    pub fn run(&self) -> Option<&Run> {
        self.run.as_ref()
    }
    /// A terminal provider response lacked a supported complete economic manifest.
    pub fn unresolved_final(&self) -> bool {
        self.unresolved_final
    }
    /// Consume the single dispatch permission. No retry, restore or uncertain receipt can grant
    /// another paid create; even a second invocation on this same object is refused.
    pub fn dispatch(&mut self) -> Result<(), &'static str> {
        if !matches!(self.stage, Stage::Ready) || self.dispatched || self.run.is_none() {
            return Err("native request has no dispatch permission");
        }
        self.dispatched = true;
        Ok(())
    }
    fn correlation(&mut self) -> Result<u64, &'static str> {
        self.correlation = self
            .correlation
            .checked_add(1)
            .ok_or("exhausted transaction correlation")?;
        Ok(self.correlation)
    }
    fn publish(
        &mut self,
        checkpoint: Checkpoint,
        after: AfterPublish,
        input: &[u8],
    ) -> Result<(), &'static str> {
        let crossing = Crossing::conditional(checkpoint.candidate.clone(), input)?;
        self.stage = Stage::Publish(
            crossing,
            Box::new(Publication::Checkpoint(checkpoint)),
            after,
        );
        Ok(())
    }
    fn reconcile(&mut self, input: &[u8]) -> Result<(), &'static str> {
        let run = self.run.as_ref().ok_or("missing original run")?;
        self.stage = match run.recovery() {
            Recovery::Submit { observations, .. } => {
                let next = observations
                    .into_iter()
                    .next()
                    .ok_or("empty component manifest")?;
                Stage::Component(Crossing::observation(&next, input)?, next)
            }
            Recovery::Quarantined => {
                self.unresolved_final = true;
                Stage::Ready
            }
            Recovery::Close(close) => Stage::Close(Crossing::scope(close, input)?),
            Recovery::Poll { .. }
            | Recovery::Tombstone
            | Recovery::DeletedUnresolved
            | Recovery::LostCreate => Stage::Ready,
        };
        Ok(())
    }
    /// Supply one complete buffered response. The caller's bytes remain unchanged outside this
    /// object. `supported_final_profile` must be established by the native provider validator;
    /// false preserves unresolved accounting instead of pretending all components are complete.
    pub fn response(
        &mut self,
        status: u16,
        body: &[u8],
        deleted: bool,
        supported_final_profile: bool,
        input: &[u8],
    ) -> Result<(), &'static str> {
        if !self.dispatched
            || self.responded
            || !matches!(self.stage, Stage::Ready)
            || body.len() > 1024 * 1024
        {
            return Err("unexpected native response");
        }
        self.responded = true;
        if !(200..300).contains(&status) {
            return Ok(());
        }
        if deleted {
            let correlation = self.correlation()?;
            let run = self.run.as_ref().ok_or("missing run")?;
            let checkpoint = run.deleted(RECORD_INDEX, correlation, &self.original)?;
            return self.publish(checkpoint, AfterPublish::Reconcile, input);
        }
        let value: serde_json::Value =
            crate::codec::json::parse(body).map_err(|_| "invalid native response")?;
        let terminal = matches!(
            value.get("status").and_then(serde_json::Value::as_str),
            Some("completed" | "failed" | "cancelled" | "incomplete" | "budget_exceeded")
        );
        self.unresolved_final = terminal && !supported_final_profile;
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| valid_id(id))
            .ok_or("response has no provider ID")?;
        let run = self.run.as_ref().ok_or("missing run")?;
        if run.provider_id().is_some_and(|original| original != id) {
            return Err("provider response identity changed");
        }
        self.response = Some((status, body.to_vec(), supported_final_profile));
        if run.provider_id().is_none() {
            let correlation = self.correlation()?;
            let checkpoint = self.run.as_ref().unwrap().bind_provider(
                RECORD_INDEX,
                correlation,
                &self.original,
                id,
            )?;
            return self.publish(checkpoint, AfterPublish::Response, input);
        }
        self.final_response(input)
    }
    fn final_response(&mut self, input: &[u8]) -> Result<(), &'static str> {
        let (status, body, supported) = self.response.take().ok_or("missing response evidence")?;
        let correlation = self.correlation()?;
        let run = self.run.as_ref().ok_or("missing run")?;
        // Existing complete manifests are immutable. Retrieval and replay settle the same stored
        // observations; provider content deletion does not remove their protected evidence.
        if !supported || run.same_final_response(status, &body) {
            return self.reconcile(input);
        }
        match run.final_response(RECORD_INDEX, correlation, &self.original, status, &body) {
            Ok(checkpoint) => self.publish(checkpoint, AfterPublish::Reconcile, input),
            Err(_) => {
                self.unresolved_final = matches!(
                    crate::codec::json::parse::<serde_json::Value>(&body)
                        .ok()
                        .as_ref()
                        .and_then(|v| v.get("status"))
                        .and_then(serde_json::Value::as_str),
                    Some("completed" | "failed" | "cancelled" | "incomplete" | "budget_exceeded")
                );
                self.stage = Stage::Ready;
                Ok(())
            } // missing/unsupported evidence remains unresolved
        }
    }
    /// Advance without consuming an external piece until every current host intent is definitive.
    pub fn poll(&mut self, input: &[u8]) -> Result<Step, &'static str> {
        loop {
            let stage = std::mem::replace(&mut self.stage, Stage::Failed);
            let (mut crossing, remaining) = match stage {
                Stage::Ready => {
                    self.stage = Stage::Ready;
                    return Ok(Step::Ready);
                }
                Stage::Failed => {
                    return Err("native lifecycle failed; durable state requires reconciliation")
                }
                Stage::Scope(c, after) => (c, Pending::Scope(after)),
                Stage::Publish(c, plan, after) => (c, Pending::Publish(plan, after)),
                Stage::Component(c, o) => (c, Pending::Component(o)),
                Stage::Close(c) => (c, Pending::Close),
            };
            let result = match crossing.poll(input)? {
                Progress::Pending(bytes) => {
                    self.stage = remaining.restore(crossing);
                    return Ok(Step::Pending(bytes));
                }
                Progress::Complete(result) => result,
            };
            match (remaining, result) {
                (
                    Pending::Scope(after),
                    Completion::Observation(ObservationResult::ScopeReady {
                        scope,
                        closed: false,
                    }),
                ) => match after {
                    AfterScope::Create {
                        partition,
                        intent,
                        mut binding,
                    } => {
                        binding.scope = scope;
                        let correlation = self.correlation()?;
                        let plan = lifecycle::prepare_create(
                            RECORD_INDEX,
                            correlation,
                            partition,
                            &intent,
                            *binding,
                        )?;
                        let crossing = Crossing::conditional(plan.candidate().clone(), input)?;
                        self.stage = Stage::Publish(
                            crossing,
                            Box::new(Publication::Create(plan)),
                            AfterPublish::Ready,
                        );
                    }
                    AfterScope::Existing => self.reconcile(input)?,
                },
                (Pending::Publish(plan, after), Completion::Conditional(result)) => {
                    let run = match *plan {
                        Publication::Create(plan) => plan.committed(&result)?.into_run(),
                        Publication::Checkpoint(plan) => plan.committed(&result)?,
                    };
                    self.original = run.encode()?;
                    self.run = Some(run);
                    match after {
                        AfterPublish::Ready => self.stage = Stage::Ready,
                        AfterPublish::Reconcile => self.reconcile(input)?,
                        AfterPublish::Response => self.final_response(input)?,
                    }
                }
                (Pending::Component(observation), Completion::Observation(result)) => {
                    let correlation = self.correlation()?;
                    let run = self.run.as_ref().ok_or("missing run")?;
                    let checkpoint = if matches!(&result,ObservationResult::Accepted(r) if r.quarantined)
                    {
                        self.unresolved_final = true;
                        run.quarantined_receipt(
                            RECORD_INDEX,
                            correlation,
                            &self.original,
                            &observation,
                            &result,
                        )?
                    } else {
                        run.accepted(
                            RECORD_INDEX,
                            correlation,
                            &self.original,
                            &observation,
                            &result,
                        )?
                    };
                    self.publish(checkpoint, AfterPublish::Reconcile, input)?;
                }
                (Pending::Close, Completion::Observation(result)) => {
                    let correlation = self.correlation()?;
                    let checkpoint = self.run.as_ref().ok_or("missing run")?.closed(
                        RECORD_INDEX,
                        correlation,
                        &self.original,
                        &result,
                    )?;
                    self.publish(checkpoint, AfterPublish::Ready, input)?;
                }
                _ => return Err("host refused or could not resolve native lifecycle"),
            }
        }
    }
}

enum Pending {
    Scope(AfterScope),
    Publish(Box<Publication>, AfterPublish),
    Component(Observation),
    Close,
}
impl Pending {
    fn restore(self, crossing: Crossing) -> Stage {
        match self {
            Self::Scope(after) => Stage::Scope(crossing, after),
            Self::Publish(plan, after) => Stage::Publish(crossing, plan, after),
            Self::Component(o) => Stage::Component(crossing, o),
            Self::Close => Stage::Close(crossing),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use busbar_contract::abi::{
        mechanism::{extensions, ticket::Ticket},
        sdk::plane::{conditional, observation},
    };
    use busbar_contract::host_records::Decision;
    use std::collections::BTreeMap;
    struct Host {
        input: Vec<u8>,
        records: BTreeMap<Vec<u8>, Vec<u8>>,
        counts: usize,
        close: usize,
        settled: BTreeMap<Vec<u8>, BTreeMap<u32, busbar_contract::Count>>,
    }
    impl Host {
        fn base() -> Vec<u8> {
            let mut v = conditional::encode_capability(conditional::Capability {
                ticket: Ticket {
                    slot: 3,
                    generation: 1,
                },
                epoch: 7,
            });
            v.extend(extensions::encode(&[
                (observation::CAPABILITY, b"1"),
                (observation::SCOPE, &[7; 32]),
            ]));
            v
        }
        fn new() -> Self {
            Self {
                input: Self::base(),
                records: BTreeMap::new(),
                counts: 0,
                close: 0,
                settled: BTreeMap::new(),
            }
        }
        fn apply(&mut self, bytes: &[u8]) {
            self.input = Self::base();
            if let Some(request) = conditional::read_request(bytes).unwrap() {
                for predicate in &request.candidate.predicates {
                    match &predicate.expected {
                        busbar_contract::host_records::Expected::Absent => {
                            assert!(!self.records.contains_key(&predicate.key))
                        }
                        busbar_contract::host_records::Expected::Bytes(b) => {
                            assert_eq!(self.records.get(&predicate.key), Some(b))
                        }
                    }
                }
                for mutation in &request.candidate.mutations {
                    match mutation {
                        conditional::RecordMutation::Put { key, value, .. } => {
                            self.records.insert(key.clone(), value.clone());
                        }
                        conditional::RecordMutation::Delete { key, .. } => {
                            self.records.remove(key);
                        }
                    }
                }
                self.input.extend(
                    conditional::encode_result(
                        &request,
                        &conditional::ConditionalResult {
                            correlation: request.candidate.correlation,
                            decision: Ok(Decision::Applied),
                        },
                    )
                    .unwrap(),
                );
            } else if let Some(scope) = ScopeRequest::from_extensions(bytes).unwrap() {
                if scope.close {
                    self.close += 1;
                    assert_eq!(self.counts, 4);
                }
                self.input.extend(extensions::encode(&[(
                    observation::RESULT,
                    &serde_json::to_vec(
                        &serde_json::json!({"scope":vec![7;32],"closed":scope.close}),
                    )
                    .unwrap(),
                )]));
            } else if let Some(observation) = Observation::from_extensions(bytes).unwrap() {
                assert!(!observation.evidence_ref.is_empty());
                assert!(self
                    .records
                    .values()
                    .any(|row| String::from_utf8_lossy(row).contains("final_evidence")));
                self.counts += 1;
                let previous = self.settled.get(&observation.component_key);
                let quarantined = previous.is_some_and(|counts| counts != &observation.counts);
                let duplicate = previous == Some(&observation.counts);
                if previous.is_none() {
                    self.settled.insert(
                        observation.component_key.clone(),
                        observation.counts.clone(),
                    );
                }
                let receipt = observation::Receipt {
                    settlement_id: [self.counts as u8; 16],
                    duplicate,
                    quarantined,
                };
                self.input.extend(extensions::encode(&[(
                    observation::RESULT,
                    &serde_json::to_vec(&receipt).unwrap(),
                )]));
            } else {
                panic!("unknown host intent");
            }
        }
        fn drive(&mut self, session: &mut Session) {
            for _ in 0..40 {
                match session.poll(&self.input).unwrap() {
                    Step::Ready => return,
                    Step::Pending(bytes) => self.apply(&bytes),
                }
            }
            panic!("lifecycle failed to reach barrier");
        }
    }
    fn binding() -> Binding {
        Binding {
            scope: [0; 32],
            account_namespace: "a".into(),
            route_entry: "model".into(),
            model: "wire".into(),
            pricing_mode: "standard".into(),
            usage_schema: "gemini-interactions-2026-11-10".into(),
            input_class: 0,
            cache_read_class: 2,
            output_class: 1,
            tool_use_class: None,
            priced_classes: vec![0, 1, 2],
        }
    }
    fn create(host: &Host) -> Session {
        Session::create(
            Partition::new(&"a".repeat(64), "a").unwrap(),
            "intent".into(),
            binding(),
            &host.input,
        )
        .unwrap()
    }
    fn response() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"id":"provider-id","model":"wire","status":"completed","outputs":[{"text":"private output not retained"}],"usage":{"total_input_tokens":10,"total_cached_tokens":3,"total_output_tokens":5,"total_thought_tokens":2,"total_tool_use_tokens":0}})).unwrap()
    }
    #[test]
    fn paid_send_waits_for_scope_and_durable_intent_then_all_receipts_precede_close() {
        let mut host = Host::new();
        let mut session = create(&host);
        assert!(session.dispatch().is_err());
        host.drive(&mut session);
        assert_eq!(host.records.len(), 1);
        assert!(matches!(
            session.run().unwrap().recovery(),
            Recovery::LostCreate
        ));
        session.dispatch().unwrap();
        assert!(session.dispatch().is_err());
        session
            .response(200, &response(), false, true, &host.input)
            .unwrap();
        host.drive(&mut session);
        assert_eq!(host.counts, 4);
        assert_eq!(host.close, 1);
        assert!(matches!(
            session.run().unwrap().recovery(),
            Recovery::Tombstone
        ));
        assert!(!String::from_utf8_lossy(&session.original).contains("private output"));
    }
    #[test]
    fn lost_create_and_uncertain_receipt_never_authorize_paid_retry() {
        let mut host = Host::new();
        let mut session = create(&host);
        let Step::Pending(scope) = session.poll(&host.input).unwrap() else {
            panic!()
        };
        host.apply(&scope);
        let Step::Pending(intent) = session.poll(&host.input).unwrap() else {
            panic!()
        };
        let request = conditional::read_request(&intent).unwrap().unwrap();
        host.input = Host::base();
        host.input.extend(
            conditional::encode_result(
                &request,
                &conditional::ConditionalResult {
                    correlation: request.candidate.correlation,
                    decision: Err(conditional::ConditionalFailure::Indeterminate),
                },
            )
            .unwrap(),
        );
        assert!(session.poll(&host.input).is_err());
        assert!(session.dispatch().is_err());
    }
    #[test]
    fn changed_final_totals_are_durable_quarantined_and_never_reclosed() {
        let mut host = Host::new();
        let mut session = create(&host);
        host.drive(&mut session);
        session.dispatch().unwrap();
        session
            .response(200, &response(), false, true, &host.input)
            .unwrap();
        host.drive(&mut session);
        assert_eq!(host.close, 1);
        let found = Found {
            run: session.run().unwrap().clone(),
            record: session.original.clone(),
        };
        let mut resumed = Session::stored(found, &host.input).unwrap();
        host.drive(&mut resumed);
        resumed.dispatch().unwrap();
        let changed = String::from_utf8(response())
            .unwrap()
            .replace("\"total_output_tokens\":5", "\"total_output_tokens\":8");
        resumed
            .response(200, changed.as_bytes(), false, true, &host.input)
            .unwrap();
        host.drive(&mut resumed);
        assert!(matches!(
            resumed.run().unwrap().recovery(),
            Recovery::Quarantined
        ));
        assert!(resumed.unresolved_final());
        assert_eq!(host.close, 1);
        assert_eq!(
            host.settled[b"output".as_slice()][&1],
            busbar_contract::Count::from(5)
        );
        let persisted = String::from_utf8(resumed.original.clone()).unwrap();
        assert!(persisted.contains("final_evidence"));
        assert!(persisted.contains("evidence_history"));
        assert!(persisted.contains("quarantined"));
    }
    #[test]
    fn missing_or_unsupported_final_evidence_remains_unclosed() {
        let mut host = Host::new();
        let mut session = create(&host);
        host.drive(&mut session);
        session.dispatch().unwrap();
        session
            .response(200, &response(), false, false, &host.input)
            .unwrap();
        host.drive(&mut session);
        assert!(matches!(
            session.run().unwrap().recovery(),
            Recovery::Poll { .. }
        ));
        assert_eq!(host.counts, 0);
        assert_eq!(host.close, 0);
        assert_eq!(host.records.len(), 2);
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use busbar_contract::abi::sdk::plane::{
        conditional::{ConditionalResult, RecordMutation},
        observation::Receipt,
    };
    use busbar_contract::host_records::Decision;
    #[test]
    fn protected_final_evidence_survives_restart_and_content_delete() {
        let partition = Partition::new(&"a".repeat(64), "account").unwrap();
        let binding = Binding {
            scope: [7; 32],
            account_namespace: "account".into(),
            route_entry: "original".into(),
            model: "wire".into(),
            pricing_mode: "standard".into(),
            usage_schema: "gemini-interactions-2026-11-10".into(),
            input_class: 0,
            cache_read_class: 2,
            output_class: 1,
            tool_use_class: None,
            priced_classes: vec![0, 1, 2],
        };
        let applied = |correlation| ConditionalResult {
            correlation,
            decision: Ok(Decision::Applied),
        };
        let run = lifecycle::prepare_create(0, 1, partition.clone(), "intent", binding)
            .unwrap()
            .committed(&applied(1))
            .unwrap()
            .into_run();
        let run = run
            .bind_provider(0, 2, &run.encode().unwrap(), "id")
            .unwrap()
            .committed(&applied(2))
            .unwrap();
        let raw=br#"{"id":"id","model":"wire","status":"completed","steps":[{"type":"model_output","content":[{"type":"text","text":"private output"}]}],"usage":{"total_input_tokens":10,"total_cached_tokens":3,"total_output_tokens":5,"total_thought_tokens":2,"total_tool_use_tokens":0}}"#;
        let checkpoint = run
            .final_response(0, 3, &run.encode().unwrap(), 200, raw)
            .unwrap();
        let RecordMutation::Put { value, .. } = &checkpoint.candidate.mutations[0] else {
            panic!()
        };
        // Crash after durable publication, before even adopting the successful local checkpoint.
        let restored = Run::decode(value, &partition).unwrap();
        assert!(!String::from_utf8_lossy(value).contains("private output"));
        assert!(String::from_utf8_lossy(value).contains("final_evidence"));
        let restored = restored
            .deleted(0, 4, value)
            .unwrap()
            .committed(&applied(4))
            .unwrap();
        let Recovery::Submit { observations, .. } = restored.recovery() else {
            panic!("delete lost durable manifest")
        };
        assert_eq!(observations.len(), 4);
        let accepted = ObservationResult::Accepted(Receipt {
            settlement_id: [1; 16],
            duplicate: true,
            quarantined: false,
        });
        let first = restored
            .accepted(
                0,
                5,
                &restored.encode().unwrap(),
                &observations[0],
                &accepted,
            )
            .unwrap()
            .committed(&applied(5))
            .unwrap();
        let Recovery::Submit {
            observations: remaining,
            ..
        } = first.recovery()
        else {
            panic!()
        };
        assert_eq!(remaining.len(), 3);
        assert!(first
            .closed(
                0,
                6,
                &first.encode().unwrap(),
                &ObservationResult::ScopeReady {
                    scope: [7; 32],
                    closed: true
                }
            )
            .is_err());
    }
}
