//! Durable request-driven observation lifecycle plans over the existing conditional-record SDK.
//!
//! The owner submits each candidate through `Out::conditional_request`, waits for its exact
//! applied receipt, and reads rows through `Services::records_get`/`records_list`. Nothing here
//! substitutes an in-memory ownership table. The native door must declare a guarded record kind
//! and require durable conditional/observation capabilities before enabling paid create.
//! A recovered dispatch intent without a provider ID is unresolved: it never grants another send.

use super::interactions::{valid_id, Binding};
use busbar_contract::abi::sdk::plane::{
    conditional::{ConditionalCandidate, ConditionalResult, RecordMutation, RecordPredicate},
    observation::{Observation, ObservationResult, ScopeRequest, State},
};
use busbar_contract::host_records::{Decision, Expected};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const MAX_RECORD: usize = 32 * 1024;

/// Host-issued principal reference plus frozen provider account partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Partition {
    caller: String,
    account: String,
}
impl Partition {
    /// Build only from `OnPieceIn.caller_ref` and admitted configuration, never caller headers.
    pub fn new(caller_ref: &str, account_namespace: &str) -> Result<Self, &'static str> {
        if caller_ref.len() < 32
            || caller_ref.len() > 128
            || !caller_ref
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
            || account_namespace.is_empty()
            || account_namespace.len() > 128
        {
            return Err("missing or invalid host principal/account partition");
        }
        Ok(Self {
            caller: caller_ref.into(),
            account: account_namespace.into(),
        })
    }
    /// Principal/account prefix for bounded request-driven reconciliation listing.
    pub fn run_prefix(&self) -> Vec<u8> {
        format!("runs/{}/{}/", self.caller, hex::encode(&self.account)).into_bytes()
    }
    /// Exact provider-ID lookup key. A miss is unattributed and grants no new scope.
    pub fn provider_key(&self, id: &str) -> Result<Vec<u8>, &'static str> {
        let mut key = provider_lookup_prefix(&self.caller, id)?;
        key.extend(hex::encode(&self.account).as_bytes());
        Ok(key)
    }
}

/// Authenticated pre-route lookup prefix. List at most two rows: zero is unattributed, two is
/// ambiguous across provider accounts, and only exactly one may resolve a stored object's route.
pub fn provider_lookup_prefix(caller_ref: &str, id: &str) -> Result<Vec<u8>, &'static str> {
    Partition::new(caller_ref, "validation")?;
    if !valid_id(id) {
        return Err("invalid provider ID");
    }
    Ok(format!("ids/{caller_ref}/{id}/").into_bytes())
}

/// Principal-bound lookup for the host-random intent used by autonomous polling.
pub fn intent_key(caller_ref: &str, intent: &str) -> Result<Vec<u8>, &'static str> {
    Partition::new(caller_ref, "validation")?;
    if !valid_id(intent) {
        return Err("invalid create intent");
    }
    Ok(format!("intents/{caller_ref}/{intent}").into_bytes())
}

/// Protected original intent lookup, published atomically with the create intent before work
/// registration and before any paid dispatch. It never authorizes recreating a lost request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentIndex {
    partition: Partition,
    intent: String,
    run_key: Vec<u8>,
}
impl IntentIndex {
    /// Validate the original principal and exact run key before reading the run.
    pub fn decode(bytes: &[u8], caller: &str, intent: &str) -> Result<Self, &'static str> {
        if bytes.len() > MAX_RECORD {
            return Err("oversized intent index");
        }
        intent_key(caller, intent)?;
        let index: Self = serde_json::from_slice(bytes).map_err(|_| "invalid intent index")?;
        Partition::new(&index.partition.caller, &index.partition.account)?;
        if index.partition.caller != caller || index.intent != intent {
            return Err("intent ownership mismatch");
        }
        let mut expected = index.partition.run_prefix();
        expected.extend(intent.as_bytes());
        if expected != index.run_key {
            return Err("intent run key mismatch");
        }
        Ok(index)
    }
    /// Exact protected run key.
    pub fn run_key(&self) -> &[u8] {
        &self.run_key
    }
    /// Frozen original partition.
    pub fn partition(&self) -> &Partition {
        &self.partition
    }
}

/// Protected provider-ID index. Its route is read from the immutable original run record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderIndex {
    partition: Partition,
    provider_id: String,
    run_key: Vec<u8>,
}
impl ProviderIndex {
    /// Decode only the unique lookup row under this authenticated principal and exact opaque ID.
    pub fn decode(bytes: &[u8], caller_ref: &str, id: &str) -> Result<Self, &'static str> {
        if bytes.len() > MAX_RECORD {
            return Err("oversized provider index");
        }
        provider_lookup_prefix(caller_ref, id)?;
        let index: Self = serde_json::from_slice(bytes).map_err(|_| "invalid provider index")?;
        Partition::new(&index.partition.caller, &index.partition.account)?;
        if index.partition.caller != caller_ref
            || index.provider_id != id
            || !index.run_key.starts_with(&index.partition.run_prefix())
        {
            return Err("provider index ownership mismatch");
        }
        Ok(index)
    }
    /// Exact original record key to read before returning its concrete admitted route entry.
    pub fn run_key(&self) -> &[u8] {
        &self.run_key
    }
    /// Original account partition against which the run decoder validates custody.
    pub fn partition(&self) -> &Partition {
        &self.partition
    }
}

/// Durable state only; original model, scope and ownership are private and immutable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    version: u32,
    partition: Partition,
    intent: String,
    binding: Binding,
    provider_id: Option<String>,
    components: Option<Vec<Observation>>,
    accepted: BTreeSet<Vec<u8>>,
    content_deleted: bool,
    closed: bool,
    /// Protected identity and raw usage facts, published atomically with the final manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    final_evidence: Option<FinalEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    evidence_history: Vec<FinalEvidence>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    quarantined: BTreeSet<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalEvidence {
    http_status: u16,
    body: Vec<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    profile: Vec<u8>,
}

/// Safe next step after a restart or retry. There is intentionally no create action.
#[derive(Debug, PartialEq, Eq)]
pub enum Recovery {
    /// The create may have reached the provider. Retain reservation and reconcile externally.
    LostCreate,
    /// Reattach this original scope, then retrieve the known provider ID under the original route.
    Poll {
        /// Original provider ID.
        id: String,
        /// Reattach request; the host verifies original principal and rates.
        attach: ScopeRequest,
    },
    /// Persisted final observations can be replayed safely before reading provider content.
    Submit {
        /// Original scope attach, authorized by the host.
        attach: ScopeRequest,
        /// Final components still lacking a definitive accepted receipt.
        observations: Vec<Observation>,
    },
    /// Content was removed before all economic components settled. Keep the unresolved record.
    DeletedUnresolved,
    /// All final components have receipts; explicit host close is now allowed.
    Close(ScopeRequest),
    /// Retain the tombstone through the provider replay horizon.
    Tombstone,
    /// Conflicting authoritative totals were durably quarantined; never close or invent ordering.
    Quarantined,
}

/// One create-intent publication. This value is consumed before it can grant one dispatch.
pub struct CreatePlan {
    candidate: ConditionalCandidate,
    run: Run,
}

/// A single paid-create authorization following a definitive durable receipt.
/// This is not cloneable or reconstructible from persisted rows.
pub struct CreatePermit {
    run: Run,
}
impl CreatePermit {
    /// Consume the permit immediately before sending; retain the returned run for ID binding.
    pub fn into_run(self) -> Run {
        self.run
    }
}

impl CreatePlan {
    /// Exact immutable candidate to publish before any external paid effect.
    pub fn candidate(&self) -> &ConditionalCandidate {
        &self.candidate
    }
    /// Authorize one send only after this exact candidate applied. Failure never grants retry.
    pub fn committed(self, result: &ConditionalResult) -> Result<CreatePermit, &'static str> {
        if result.correlation != self.candidate.correlation
            || result.decision != Ok(Decision::Applied)
        {
            return Err("create intent has no definitive applied receipt");
        }
        Ok(CreatePermit { run: self.run })
    }
}

/// Prepare an absent-only durable create intent after the host issued the original scope.
/// `intent` must be a fresh server-generated operation identity retained across request retries.
pub fn prepare_create(
    kind: u32,
    correlation: u64,
    partition: Partition,
    intent: &str,
    binding: Binding,
) -> Result<CreatePlan, &'static str> {
    if correlation == 0
        || !valid_id(intent)
        || binding.scope == [0; 32]
        || binding.account_namespace != partition.account
        || binding.model.is_empty()
        || binding.route_entry.is_empty()
    {
        return Err("invalid create binding");
    }
    let run = Run {
        version: 1,
        partition,
        intent: intent.into(),
        binding,
        provider_id: None,
        components: None,
        accepted: BTreeSet::new(),
        content_deleted: false,
        closed: false,
        final_evidence: None,
        evidence_history: Vec::new(),
        quarantined: BTreeSet::new(),
    };
    let key = run.key();
    let mut candidate = ConditionalCandidate {
        correlation,
        predicates: vec![RecordPredicate {
            kind,
            key: key.clone(),
            expected: Expected::Absent,
        }],
        mutations: vec![RecordMutation::Put {
            kind,
            key,
            value: run.encode()?,
        }],
        chains: Vec::new(),
    };
    let key = intent_key(&run.partition.caller, intent)?;
    candidate.predicates.push(RecordPredicate {
        kind,
        key: key.clone(),
        expected: Expected::Absent,
    });
    candidate.mutations.push(RecordMutation::Put {
        kind,
        key,
        value: serde_json::to_vec(&IntentIndex {
            partition: run.partition.clone(),
            intent: intent.into(),
            run_key: run.key(),
        })
        .map_err(|_| "invalid intent index")?,
    });
    Ok(CreatePlan { candidate, run })
}

/// Atomic checkpoint and optional provider-ID index. Adopt `next` only after the applied receipt.
pub struct Checkpoint {
    /// Immutable exact-byte CAS to publish.
    pub candidate: ConditionalCandidate,
    next: Run,
}
impl Checkpoint {
    /// Consume a successful exact publication, keeping failed or uncertain records unchanged.
    pub fn committed(self, result: &ConditionalResult) -> Result<Run, &'static str> {
        if result.correlation != self.candidate.correlation
            || result.decision != Ok(Decision::Applied)
        {
            return Err("checkpoint has no definitive applied receipt");
        }
        Ok(self.next)
    }
}

impl Run {
    /// Host-random local create intent, never a provider operation identifier.
    pub fn intent(&self) -> &str {
        &self.intent
    }
    /// Frozen provider normalization binding; host attach still validates authority.
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    /// Known provider ID, if its index was durably published.
    pub fn provider_id(&self) -> Option<&str> {
        self.provider_id.as_deref()
    }
    /// Logical record key inside the declared plugin instance record kind.
    pub fn key(&self) -> Vec<u8> {
        let mut key = self.partition.run_prefix();
        key.extend(self.intent.as_bytes());
        key
    }
    /// Bounded durable encoding. Retain exact read bytes for subsequent CAS.
    pub fn encode(&self) -> Result<Vec<u8>, &'static str> {
        let bytes = serde_json::to_vec(self).map_err(|_| "invalid run record")?;
        if bytes.len() > MAX_RECORD {
            return Err("oversized run record");
        }
        Ok(bytes)
    }
    /// Read only under the admitted principal/account partition; never adopt unknown ownership.
    pub fn decode(bytes: &[u8], partition: &Partition) -> Result<Self, &'static str> {
        if bytes.len() > MAX_RECORD {
            return Err("oversized run record");
        }
        let run: Self = serde_json::from_slice(bytes).map_err(|_| "invalid run record")?;
        if run.version != 1
            || run.partition != *partition
            || run.binding.account_namespace != partition.account
            || run.binding.scope == [0; 32]
            || run.binding.route_entry.is_empty()
            || !valid_id(&run.intent)
            || run.provider_id.as_deref().is_some_and(|id| !valid_id(id))
            || run.accepted.len() > 64
            || run.quarantined.len() > 64
            || run.evidence_history.len() > 8
            || run.evidence_history.iter().any(|e| {
                e.body.len() > 16 * 1024
                    || e.profile.len() > 4096
                    || !(200..300).contains(&e.http_status)
            })
            || run.components.as_ref().is_some_and(|v| {
                let identities: BTreeSet<_> = v.iter().map(component_identity).collect();
                v.is_empty()
                    || v.len() > 64
                    || identities.len() != v.len()
                    || v.iter().any(|o| {
                        o.scope != run.binding.scope
                            || o.state != State::Final
                            || o.validate().is_err()
                    })
                    || !run.accepted.is_subset(&identities)
                    || !run.quarantined.is_subset(&identities)
                    || !run.accepted.is_disjoint(&run.quarantined)
                    || (run.closed && run.accepted != identities)
            })
            || (run.components.is_none()
                && (!run.accepted.is_empty() || !run.quarantined.is_empty()))
            || (run.closed && !run.quarantined.is_empty())
            || (run.closed && (run.components.is_none() || run.provider_id.is_none()))
            || run.final_evidence.as_ref().is_some_and(|e| {
                e.body.len() > 16 * 1024
                    || e.profile.len() > 4096
                    || run.components.is_none()
                    || !(200..300).contains(&e.http_status)
            })
        {
            return Err("run ownership or schema mismatch");
        }
        Ok(run)
    }
    /// On restore, never recreate a possibly paid operation. Poll only after host scope attach.
    pub fn recovery(&self) -> Recovery {
        if self.closed {
            return Recovery::Tombstone;
        }
        if let Some(components) = &self.components {
            let observations: Vec<_> = components
                .iter()
                .filter(|o| {
                    !self.accepted.contains(&component_identity(o))
                        && !self.quarantined.contains(&component_identity(o))
                })
                .cloned()
                .collect();
            if observations.is_empty() {
                if !self.quarantined.is_empty() {
                    return Recovery::Quarantined;
                }
                return Recovery::Close(ScopeRequest::close(self.binding.scope));
            }
            return Recovery::Submit {
                attach: ScopeRequest::attach(self.binding.scope),
                observations,
            };
        }
        if self.content_deleted {
            return Recovery::DeletedUnresolved;
        }
        match &self.provider_id {
            None => Recovery::LostCreate,
            Some(id) => Recovery::Poll {
                id: id.clone(),
                attach: ScopeRequest::attach(self.binding.scope),
            },
        }
    }
    fn change(
        &self,
        kind: u32,
        correlation: u64,
        original: &[u8],
        next: Self,
    ) -> Result<Checkpoint, &'static str> {
        if correlation == 0 || self.encode()? != original {
            return Err("checkpoint requires exact original record");
        }
        let key = self.key();
        Ok(Checkpoint {
            candidate: ConditionalCandidate {
                correlation,
                predicates: vec![RecordPredicate {
                    kind,
                    key: key.clone(),
                    expected: Expected::Bytes(original.to_vec()),
                }],
                mutations: vec![RecordMutation::Put {
                    kind,
                    key,
                    value: next.encode()?,
                }],
                chains: Vec::new(),
            },
            next,
        })
    }
    /// Atomically associate one returned ID with the original scope; never overwrite another ID.
    pub fn bind_provider(
        &self,
        kind: u32,
        correlation: u64,
        original: &[u8],
        id: &str,
    ) -> Result<Checkpoint, &'static str> {
        if self.provider_id.is_some() || self.closed || !valid_id(id) {
            return Err("run cannot bind this provider ID");
        }
        let mut next = self.clone();
        next.provider_id = Some(id.into());
        let mut checkpoint = self.change(kind, correlation, original, next)?;
        let key = self.partition.provider_key(id)?;
        checkpoint.candidate.predicates.push(RecordPredicate {
            kind,
            key: key.clone(),
            expected: Expected::Absent,
        });
        checkpoint.candidate.mutations.push(RecordMutation::Put {
            kind,
            key,
            value: serde_json::to_vec(&ProviderIndex {
                partition: self.partition.clone(),
                provider_id: id.into(),
                run_key: self.key(),
            })
            .map_err(|_| "invalid provider index")?,
        });
        Ok(checkpoint)
    }
    /// Freeze the complete component manifest from an authoritative terminal response.
    /// Include hosted leaf components; absence of final usage must not call this method.
    pub fn final_components(
        &self,
        kind: u32,
        correlation: u64,
        original: &[u8],
        observations: &[Observation],
    ) -> Result<Checkpoint, &'static str> {
        if self.provider_id.is_none()
            || self.components.is_some()
            || self.closed
            || observations.is_empty()
            || observations.len() > 64
        {
            return Err("cannot finalize component manifest");
        }
        let mut components = BTreeSet::new();
        for observation in observations {
            observation.validate().map_err(|_| "invalid observation")?;
            if observation.scope != self.binding.scope
                || observation.state != State::Final
                || !components.insert(component_identity(observation))
            {
                return Err("unrelated or duplicate component");
            }
        }
        let mut next = self.clone();
        next.components = Some(observations.to_vec());
        self.change(kind, correlation, original, next)
    }

    /// Freeze authenticated buffered provider usage and its exact independent component manifest
    /// in one transaction. Generated text and caller data are not retained in the evidence row.
    /// The native owner must separately establish the supported text-only economic profile before
    /// calling this method; unknown hosted/modality evidence must remain unresolved.
    #[cfg(test)]
    pub fn final_response(
        &self,
        kind: u32,
        correlation: u64,
        original: &[u8],
        http_status: u16,
        body: &[u8],
    ) -> Result<Checkpoint, &'static str> {
        self.final_response_with_profile(
            kind,
            correlation,
            original,
            http_status,
            body,
            b"gemini-text-full-steps/v1",
        )
    }
    /// Retain the bounded type-only proof for a complete observed stream or full-step snapshot.
    /// This metadata attests the adapter's validation; it never supplies missing provider counts.
    pub fn final_response_with_profile(
        &self,
        kind: u32,
        correlation: u64,
        original: &[u8],
        http_status: u16,
        body: &[u8],
        profile: &[u8],
    ) -> Result<Checkpoint, &'static str> {
        if profile.is_empty() || profile.len() > 4096 {
            return Err("invalid economic profile evidence");
        }
        use super::interactions::{normalize, Evidence, Normalized};
        let id = self.provider_id().ok_or("unbound provider response")?;
        let mut reference = self.key();
        if self.components.is_some() {
            if self.evidence_history.len() >= 8 {
                return Err("conflict evidence bound reached");
            }
            reference.extend(format!("#evidence/{}", self.evidence_history.len()).as_bytes());
        } else {
            reference.extend(b"#final-evidence");
        }
        let Normalized::Final { observations, .. } = normalize(
            &self.binding,
            Evidence {
                known_id: Some(id),
                http_status,
                body,
                evidence_ref: &reference,
            },
        ) else {
            return Err("provider response has no complete authoritative usage");
        };
        let value: serde_json::Value =
            crate::codec::json::parse(body).map_err(|_| "invalid evidence")?;
        let object = value.as_object().ok_or("invalid buffered evidence")?;
        if object.contains_key("event_type") {
            return Err("buffered evidence cannot be an event envelope");
        }
        let retained = ["id", "model", "service_tier", "status", "usage"]
            .into_iter()
            .filter_map(|k| object.get(k).map(|v| (k.to_owned(), v.clone())))
            .collect::<serde_json::Map<_, _>>();
        let body = serde_json::to_vec(&retained).map_err(|_| "invalid evidence")?;
        if body.len() > 16 * 1024 {
            return Err("oversized usage evidence");
        }
        let mut checkpoint = if let Some(previous) = &self.components {
            if same_economics(previous, &observations) {
                return Err("unchanged final evidence");
            }
            if previous
                .iter()
                .map(component_identity)
                .collect::<BTreeSet<_>>()
                != observations
                    .iter()
                    .map(component_identity)
                    .collect::<BTreeSet<_>>()
            {
                return Err("changed economic component set");
            }
            let mut next = self.clone();
            next.components = Some(observations);
            next.accepted.clear();
            next.quarantined.clear();
            next.closed = false;
            self.change(kind, correlation, original, next)?
        } else {
            self.final_components(kind, correlation, original, &observations)?
        };
        if self.components.is_some() {
            checkpoint.next.evidence_history.push(FinalEvidence {
                http_status,
                body,
                profile: profile.to_vec(),
            });
        } else {
            checkpoint.next.final_evidence = Some(FinalEvidence {
                http_status,
                body,
                profile: profile.to_vec(),
            });
        }
        // Rebuild the one CAS value after adding the raw facts, preserving the exact predicate.
        let RecordMutation::Put { value, .. } = &mut checkpoint.candidate.mutations[0] else {
            return Err("invalid final checkpoint");
        };
        *value = checkpoint.next.encode()?;
        Ok(checkpoint)
    }
    /// Compare incoming final quantities with the durable manifest without inventing a revision.
    pub fn same_final_response(&self, http_status: u16, body: &[u8]) -> bool {
        let Some(previous) = &self.components else {
            return false;
        };
        let super::interactions::Normalized::Final { observations, .. } =
            super::interactions::normalize(
                &self.binding,
                super::interactions::Evidence {
                    known_id: self.provider_id(),
                    http_status,
                    body,
                    evidence_ref: b"comparison-only",
                },
            )
        else {
            return false;
        };
        same_economics(previous, &observations)
    }
    /// Retain a definitive quarantine receipt as unresolved state, never accepted closure.
    pub fn quarantined_receipt(
        &self,
        kind: u32,
        correlation: u64,
        original: &[u8],
        observation: &Observation,
        result: &ObservationResult,
    ) -> Result<Checkpoint, &'static str> {
        if self.closed
            || !self
                .components
                .as_ref()
                .is_some_and(|c| c.contains(observation))
            || !matches!(result,ObservationResult::Accepted(r) if r.quarantined)
        {
            return Err("component has no quarantine receipt");
        }
        let mut next = self.clone();
        next.accepted.remove(&component_identity(observation));
        next.quarantined.insert(component_identity(observation));
        self.change(kind, correlation, original, next)
    }
    /// Persist one non-quarantined final host receipt. Pending or refused results remain pending.
    pub fn accepted(
        &self,
        kind: u32,
        correlation: u64,
        original: &[u8],
        observation: &Observation,
        result: &ObservationResult,
    ) -> Result<Checkpoint, &'static str> {
        if self.closed
            || !self
                .components
                .as_ref()
                .is_some_and(|c| c.contains(observation))
            || !matches!(result,ObservationResult::Accepted(receipt) if !receipt.quarantined)
        {
            return Err("component has no accepted final receipt");
        }
        let mut next = self.clone();
        next.accepted.insert(component_identity(observation));
        self.change(kind, correlation, original, next)
    }
    /// Preserve a tombstone when provider content is deleted; this does not close accounting.
    pub fn deleted(
        &self,
        kind: u32,
        correlation: u64,
        original: &[u8],
    ) -> Result<Checkpoint, &'static str> {
        let mut next = self.clone();
        next.content_deleted = true;
        self.change(kind, correlation, original, next)
    }
    /// Persist explicit host close only after every final component has an accepted receipt.
    pub fn closed(
        &self,
        kind: u32,
        correlation: u64,
        original: &[u8],
        result: &ObservationResult,
    ) -> Result<Checkpoint, &'static str> {
        if !matches!(self.recovery(), Recovery::Close(_))
            || !matches!(result,ObservationResult::ScopeReady{scope,closed:true} if *scope==self.binding.scope)
        {
            return Err("scope is not definitively closed");
        }
        let mut next = self.clone();
        next.closed = true;
        self.change(kind, correlation, original, next)
    }
}

fn same_economics(left: &[Observation], right: &[Observation]) -> bool {
    left.len() == right.len()
        && left.iter().all(|a| {
            right.iter().any(|b| {
                a.scope == b.scope
                    && a.event_key == b.event_key
                    && a.component_key == b.component_key
                    && a.revision == b.revision
                    && a.state == b.state
                    && a.counts == b.counts
                    && a.rated_subject == b.rated_subject
            })
        })
}

fn component_identity(observation: &Observation) -> Vec<u8> {
    // Prefix framing is unambiguous even for arbitrary component bytes.
    let mut key = (observation.event_key.len() as u32).to_be_bytes().to_vec();
    key.extend(&observation.event_key);
    key.extend(&observation.component_key);
    key
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
