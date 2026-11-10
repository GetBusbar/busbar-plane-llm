//! Durable work is registered after the create intent and before paid dispatch. A host poll
//! restores authority and resolves the local intent, never resubmitting the paid creation.

use super::*;
use busbar_contract::abi::host::service::{ItemSpan, WORK_LIVE, WORK_SETTLED};
use busbar_contract::abi::sdk::recovery::{poll_record, poll_target};

pub(super) const PREFIX: &str = "/_busbar/llm/interactions/";
const WORK_HEADER: &[u8] = b"x-busbar-recovery-work";

pub(super) fn intent(method: &str, target: &str) -> Result<Option<String>, &'static str> {
    let Some(intent) = target.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    if method != "GET"
        || intent.len() != 64
        || !intent
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("invalid native recovery target");
    }
    Ok(Some(intent.into()))
}
pub(super) fn target(intent: &str) -> String {
    format!("{PREFIX}{intent}")
}

pub(super) struct Work {
    target: String,
    reference: Option<String>,
    handle: Option<u64>,
    settled: bool,
}
impl Work {
    pub(super) fn new(intent: &str) -> Self {
        Self {
            target: target(intent),
            reference: None,
            handle: None,
            settled: false,
        }
    }
    pub(super) fn restore(intent: &str, fields: &[(&[u8], &[u8])]) -> Result<Self, &'static str> {
        let mut values = fields
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case(WORK_HEADER));
        let (_, value) = values.next().ok_or("recovery work reference required")?;
        if values.next().is_some() || value.is_empty() || value.len() > 128 {
            return Err("invalid recovery work reference");
        }
        let reference =
            std::str::from_utf8(value).map_err(|_| "invalid recovery work reference")?;
        Ok(Self {
            reference: Some(reference.into()),
            ..Self::new(intent)
        })
    }
    pub(super) fn attach(
        &mut self,
        services: &Services,
        ticket: Ticket,
    ) -> Poll<Result<(), &'static str>> {
        if self.handle.is_some() || self.settled {
            return Poll::Ready(Ok(()));
        }
        let mut bytes = [0; 512];
        let mut spans = [ItemSpan::ZERO; 2];
        let handle = CompletionHandle {
            ticket,
            seq: 91,
            ..CompletionHandle::ZERO
        };
        if let Some(reference) = &self.reference {
            let found = match services.work_find(handle, reference, &mut bytes, &mut spans) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(Some(found))) => found,
                _ => return Poll::Ready(Err("original recovery work unavailable")),
            };
            if poll_target(found.record)? != Some(self.target.as_str()) {
                return Poll::Ready(Err("recovery work target differs from original intent"));
            }
            if !matches!(found.state, WORK_LIVE | WORK_SETTLED) {
                return Poll::Ready(Err("invalid recovery work state"));
            }
            self.handle = Some(found.handle);
            self.settled = found.state == WORK_SETTLED;
        } else {
            let record = poll_record(&self.target)?;
            let opened = match services.work_open(
                handle,
                crate::observations::lookup::RECORD_KIND,
                &record,
                &mut bytes,
                &mut spans,
            ) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(opened)) => opened,
                Poll::Ready(Err(_)) => {
                    return Poll::Ready(Err("durable recovery work registration failed"))
                }
            };
            self.handle = Some(opened.handle);
            self.reference = Some(opened.reference.into());
        }
        Poll::Ready(Ok(()))
    }
    pub(super) fn settle(
        &mut self,
        services: &Services,
        ticket: Ticket,
    ) -> Poll<Result<(), &'static str>> {
        if self.settled {
            return Poll::Ready(Ok(()));
        }
        let work = self.handle.ok_or("recovery work is not attached")?;
        let record = poll_record(&self.target)?;
        let handle = CompletionHandle {
            ticket,
            seq: 92,
            ..CompletionHandle::ZERO
        };
        match services.work_settle(handle, work, &record) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => {
                self.settled = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(_)) => Poll::Ready(Err("recovery work settlement failed")),
        }
    }
}

/// Recovery headers are host-local hints. They are never forwarded to a provider, even if the
/// operator enables broad caller-header forwarding.
pub(super) fn forwarded(fields: &[(&[u8], &[u8])]) -> Vec<(Vec<u8>, Vec<u8>)> {
    fields
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case(WORK_HEADER))
        .map(|(name, value)| (name.to_vec(), value.to_vec()))
        .collect()
}

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "../../../tests/support/native_recovery_work.rs"]
mod tests;
