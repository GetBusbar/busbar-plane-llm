//! Read-only authenticated stored-object lookup through the plane SDK's record services.
//!
//! The host owns caller-reference continuity across signing-key rotation. A miss stays
//! unattributed; the plugin never scans other principals or migrates ownership itself.

use super::lifecycle::{intent_key, provider_lookup_prefix, IntentIndex, ProviderIndex, Run};
use busbar_contract::abi::host::service::ItemSpan;
use busbar_contract::abi::mechanism::ticket::{CompletionHandle, Ticket};
use busbar_contract::abi::sdk::Services;
use busbar_contract::plugin::markers::PlaneKind;
use std::task::Poll;

/// The plugin's declared conditional record kind; no work/owner state is held only in memory.
pub const RECORD_KIND: &str = "interaction_runs";
/// Position in this plugin's record-kind declaration.
pub const RECORD_INDEX: u32 = 0;
const MAX_RECORD_BYTES: usize = 32 * 1024;

/// Lookup failure, deliberately without record content, IDs or principal details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// No index exists under this current principal reference; never an import authorization.
    Unattributed,
    /// An ID names multiple provider accounts. Native URLs do not disambiguate those accounts.
    Ambiguous,
    /// Stored data does not match the authenticated lookup or its frozen run.
    Invalid,
    /// The host did not supply a successful bounded record read.
    Unavailable,
}

/// Original protected run and its exact bytes for subsequent conditional updates.
pub struct Found {
    /// Original route, model, price mode and host scope. Original-event operations must attach
    /// this scope; an owned continuation creates a new scope under its newly admitted route.
    pub run: Run,
    /// Exact original bytes, never reconstructed from caller input.
    pub record: Vec<u8>,
}

/// Recovery resolves the pre-dispatch intent rather than requiring a provider ID at registration.
pub struct IntentLookup {
    caller: String,
    intent: String,
    ticket: Ticket,
    index: Option<IntentIndex>,
}
impl IntentLookup {
    /// Bind to this authenticated callback only.
    pub fn new(caller: &str, intent: &str, ticket: Ticket) -> Result<Self, Failure> {
        intent_key(caller, intent).map_err(|_| Failure::Invalid)?;
        if ticket.is_none() {
            return Err(Failure::Invalid);
        }
        Ok(Self {
            caller: caller.into(),
            intent: intent.into(),
            ticket,
            index: None,
        })
    }
    /// Read two protected rows and validate their immutable original binding.
    pub fn poll(
        &mut self,
        services: &Services<PlaneKind>,
        ticket: Ticket,
        caller: &str,
    ) -> Poll<Result<Found, Failure>> {
        if self.ticket != ticket || self.caller != caller {
            return Poll::Ready(Err(Failure::Invalid));
        }
        let mut buffer = vec![0; MAX_RECORD_BYTES];
        if self.index.is_none() {
            let key = intent_key(caller, &self.intent).map_err(|_| Failure::Invalid)?;
            let handle = CompletionHandle {
                ticket,
                seq: 4,
                ..CompletionHandle::ZERO
            };
            let bytes = match services.records_get(handle, RECORD_KIND, &key, &mut buffer) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(Some(bytes))) => bytes,
                Poll::Ready(Ok(None)) => return Poll::Ready(Err(Failure::Unattributed)),
                Poll::Ready(Err(_)) => return Poll::Ready(Err(Failure::Unavailable)),
            };
            self.index = Some(
                IntentIndex::decode(bytes, caller, &self.intent).map_err(|_| Failure::Invalid)?,
            );
        }
        let index = self.index.as_ref().unwrap();
        let handle = CompletionHandle {
            ticket,
            seq: 5,
            ..CompletionHandle::ZERO
        };
        let record = match services.records_get(handle, RECORD_KIND, index.run_key(), &mut buffer) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Ok(Some(bytes))) => bytes.to_vec(),
            _ => return Poll::Ready(Err(Failure::Unavailable)),
        };
        let run = Run::decode(&record, index.partition()).map_err(|_| Failure::Invalid)?;
        if run.intent() != self.intent || run.key() != index.run_key() {
            return Poll::Ready(Err(Failure::Invalid));
        }
        Poll::Ready(Ok(Found { run, record }))
    }
}

/// One lookup invocation, retaining only completed reads and an immutable principal/ID binding.
pub struct Lookup {
    caller_ref: String,
    id: String,
    ticket: Ticket,
    index: Option<ProviderIndex>,
}
impl Lookup {
    /// Start with the host's authenticated reference and the retained native request's ID.
    pub fn new(caller_ref: &str, id: &str, ticket: Ticket) -> Result<Self, Failure> {
        provider_lookup_prefix(caller_ref, id).map_err(|_| Failure::Invalid)?;
        if ticket.is_none() {
            return Err(Failure::Invalid);
        }
        Ok(Self {
            caller_ref: caller_ref.into(),
            id: id.into(),
            ticket,
            index: None,
        })
    }

    /// Poll only on the same authenticated callback ticket. Pending service handles are reused;
    /// a completed index read advances to a distinct handle for the immutable run read.
    pub fn poll(
        &mut self,
        services: &Services<PlaneKind>,
        ticket: Ticket,
        caller_ref: &str,
    ) -> Poll<Result<Found, Failure>> {
        if ticket != self.ticket || caller_ref != self.caller_ref {
            return Poll::Ready(Err(Failure::Invalid));
        }
        let mut buffer = vec![0; MAX_RECORD_BYTES];
        if self.index.is_none() {
            let prefix = match provider_lookup_prefix(&self.caller_ref, &self.id) {
                Ok(p) => p,
                Err(_) => return Poll::Ready(Err(Failure::Invalid)),
            };
            let mut spans = [ItemSpan::ZERO; 2];
            let handle = CompletionHandle {
                ticket,
                seq: 0,
                ..CompletionHandle::ZERO
            };
            let records = match services.records_list(
                handle,
                RECORD_KIND,
                &prefix,
                None,
                2,
                (&mut buffer, &mut spans),
            ) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(_)) => return Poll::Ready(Err(Failure::Unavailable)),
                Poll::Ready(Ok(rows)) => rows
                    .records()
                    .map(|(k, v)| (k.to_vec(), v.to_vec()))
                    .collect::<Vec<_>>(),
            };
            match unique_index(&self.caller_ref, &self.id, &records) {
                Ok(index) => self.index = Some(index),
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
        let index = self.index.as_ref().expect("completed index read");
        let handle = CompletionHandle {
            ticket,
            seq: 1,
            ..CompletionHandle::ZERO
        };
        let record = match services.records_get(handle, RECORD_KIND, index.run_key(), &mut buffer) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(_)) => return Poll::Ready(Err(Failure::Unavailable)),
            Poll::Ready(Ok(None)) => return Poll::Ready(Err(Failure::Invalid)),
            Poll::Ready(Ok(Some(record))) => record.to_vec(),
        };
        Poll::Ready(read_run(index, &self.id, record))
    }
}

fn unique_index(
    caller_ref: &str,
    id: &str,
    rows: &[(Vec<u8>, Vec<u8>)],
) -> Result<ProviderIndex, Failure> {
    let [(key, value)] = rows else {
        return Err(if rows.is_empty() {
            Failure::Unattributed
        } else {
            Failure::Ambiguous
        });
    };
    let index = ProviderIndex::decode(value, caller_ref, id).map_err(|_| Failure::Invalid)?;
    if index
        .partition()
        .provider_key(id)
        .map_err(|_| Failure::Invalid)?
        != *key
    {
        return Err(Failure::Invalid);
    }
    Ok(index)
}
fn read_run(index: &ProviderIndex, id: &str, record: Vec<u8>) -> Result<Found, Failure> {
    let run = Run::decode(&record, index.partition()).map_err(|_| Failure::Invalid)?;
    if run.provider_id() != Some(id) || run.key() != index.run_key() {
        return Err(Failure::Invalid);
    }
    Ok(Found { run, record })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observations::{
        interactions::Binding,
        lifecycle::{prepare_create, Partition},
    };
    use busbar_contract::{
        abi::sdk::plane::conditional::{ConditionalResult, RecordMutation},
        host_records::Decision,
    };
    type StoredRows = Vec<(Vec<u8>, Vec<u8>)>;
    fn fixture() -> (StoredRows, Vec<u8>) {
        let partition = Partition::new(&"a".repeat(64), "account").unwrap();
        let binding = Binding {
            scope: [7; 32],
            account_namespace: "account".into(),
            route_entry: "original-member".into(),
            model: "wire-model".into(),
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
        let run = prepare_create(RECORD_INDEX, 1, partition, "intent", binding)
            .unwrap()
            .committed(&applied(1))
            .unwrap()
            .into_run();
        let checkpoint = run
            .bind_provider(RECORD_INDEX, 2, &run.encode().unwrap(), "provider-id")
            .unwrap();
        let RecordMutation::Put { key, value, .. } = &checkpoint.candidate.mutations[1] else {
            panic!("index")
        };
        let rows = vec![(key.clone(), value.clone())];
        let run = checkpoint.committed(&applied(2)).unwrap();
        (rows, run.encode().unwrap())
    }
    #[test]
    fn lookup_returns_original_entry_and_scope_only_after_both_protected_reads() {
        let (rows, record) = fixture();
        let index = unique_index(&"a".repeat(64), "provider-id", &rows).unwrap();
        let found = read_run(&index, "provider-id", record).unwrap();
        assert_eq!(found.run.binding().route_entry, "original-member");
        assert_eq!(found.run.binding().scope, [7; 32]);
    }
    #[test]
    fn rotation_unknown_id_and_multiple_accounts_never_adopt_ownership() {
        let (mut rows, _) = fixture();
        assert!(matches!(
            unique_index(&"b".repeat(64), "provider-id", &[]),
            Err(Failure::Unattributed)
        ));
        assert!(matches!(
            unique_index(&"b".repeat(64), "provider-id", &rows),
            Err(Failure::Invalid)
        ));
        assert!(matches!(
            unique_index(&"a".repeat(64), "unknown-id", &rows),
            Err(Failure::Invalid)
        ));
        rows.push(rows[0].clone());
        assert!(matches!(
            unique_index(&"a".repeat(64), "provider-id", &rows),
            Err(Failure::Ambiguous)
        ));
    }
    #[test]
    fn index_cannot_redirect_to_another_run() {
        let (rows, mut record) = fixture();
        let index = unique_index(&"a".repeat(64), "provider-id", &rows).unwrap();
        let mut parsed: serde_json::Value = serde_json::from_slice(&record).unwrap();
        parsed["intent"] = serde_json::json!("other-intent");
        record = serde_json::to_vec(&parsed).unwrap();
        assert!(matches!(
            read_run(&index, "provider-id", record),
            Err(Failure::Invalid)
        ));
    }
}
