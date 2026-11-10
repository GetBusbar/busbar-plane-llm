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
    };
    let key = run.key();
    let candidate = ConditionalCandidate {
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
                    || (run.closed && run.accepted != identities)
            })
            || (run.components.is_none() && !run.accepted.is_empty())
            || (run.closed && (run.components.is_none() || run.provider_id.is_none()))
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
                .filter(|o| !self.accepted.contains(&component_identity(o)))
                .cloned()
                .collect();
            if observations.is_empty() {
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
