//! One immutable host exchange, with an explicit first-post boundary between consecutive intents.
//!
//! A callback can still carry the preceding observation's receipt when the next component is
//! planned. It must cross the host with the new intent before that input can acknowledge it.

use busbar_contract::abi::sdk::plane::{conditional, observation};

/// Exact host result for the active operation.
#[derive(Debug)]
pub enum Completion {
    /// A scope or component result, including explicit unresolved/refused outcomes.
    Observation(observation::ObservationResult),
    /// Exact digest-, epoch- and correlation-bound transaction receipt.
    Conditional(conditional::ConditionalResult),
}

/// The next callback output, or a definitive result. Pending output must be leased unchanged.
pub enum Progress {
    /// Emit these extensions with PENDING and consume no provider/caller piece yet.
    Pending(Vec<u8>),
    /// Consume once, then construct the next exchange if needed.
    Complete(Completion),
}

enum Intent {
    Scope(observation::ScopeRequest),
    Observation,
    Conditional(conditional::Request),
}

/// One intent. Not cloneable: a completed exchange cannot be silently replayed as a new send.
pub struct Crossing {
    intent: Intent,
    encoded: Vec<u8>,
    posted: bool,
    complete: bool,
}
impl Crossing {
    /// Require both per-call mechanisms before any new paid operation or stored-object action.
    pub fn supported(input: &[u8]) -> Result<(), &'static str> {
        if !observation::supported(input).map_err(|_| "invalid observation capability")?
            || conditional::read_capability(input)?.is_none()
        {
            return Err("durable observation and conditional-record capabilities required");
        }
        Ok(())
    }

    /// Create, attach or explicitly close original host custody.
    pub fn scope(request: observation::ScopeRequest, input: &[u8]) -> Result<Self, &'static str> {
        Self::supported(input)?;
        let encoded = request.extension().map_err(|_| "invalid scope request")?;
        Ok(Self {
            intent: Intent::Scope(request),
            encoded,
            posted: false,
            complete: false,
        })
    }

    /// Submit one final component whose evidence and manifest are already durable.
    pub fn observation(
        value: &observation::Observation,
        input: &[u8],
    ) -> Result<Self, &'static str> {
        Self::supported(input)?;
        if observation::scope(input).map_err(|_| "invalid host scope")? != Some(value.scope) {
            return Err("component is not attached to original host scope");
        }
        Ok(Self {
            intent: Intent::Observation,
            encoded: value.extension().map_err(|_| "invalid observation")?,
            posted: false,
            complete: false,
        })
    }

    /// Publish one immutable exact-byte compare-and-swap under this callback's capability.
    pub fn conditional(
        candidate: conditional::ConditionalCandidate,
        input: &[u8],
    ) -> Result<Self, &'static str> {
        Self::supported(input)?;
        let capability =
            conditional::read_capability(input)?.ok_or("missing conditional capability")?;
        let request = conditional::Request {
            capability,
            candidate,
        };
        let encoded = conditional::encode_request(&request)?;
        Ok(Self {
            intent: Intent::Conditional(request),
            encoded,
            posted: false,
            complete: false,
        })
    }

    /// Emit once before inspecting completion. Re-calls keep the original immutable intent.
    pub fn poll(&mut self, input: &[u8]) -> Result<Progress, &'static str> {
        if self.complete {
            return Err("host exchange already consumed");
        }
        if !self.posted {
            self.posted = true;
            return Ok(Progress::Pending(self.encoded.clone()));
        }
        let completion = match &self.intent {
            Intent::Conditional(request) => {
                if conditional::read_capability(input)? != Some(request.capability) {
                    return Err("conditional operation authority changed");
                }
                conditional::read_result(input, request)?.map(Completion::Conditional)
            }
            Intent::Scope(request) => {
                let result = observation::result(input).map_err(|_| "invalid scope receipt")?;
                if let Some(observation::ObservationResult::ScopeReady { scope, closed }) = &result
                {
                    if request.scope.is_some_and(|expected| expected != *scope)
                        || request.close != *closed
                    {
                        return Err("unrelated scope receipt");
                    }
                } else if matches!(result, Some(observation::ObservationResult::Accepted(_))) {
                    return Err("component receipt cannot acknowledge scope");
                }
                result.map(Completion::Observation)
            }
            Intent::Observation => {
                let result =
                    observation::result(input).map_err(|_| "invalid observation receipt")?;
                if matches!(
                    result,
                    Some(observation::ObservationResult::ScopeReady { .. })
                ) {
                    return Err("scope receipt cannot acknowledge component");
                }
                result.map(Completion::Observation)
            }
        };
        match completion {
            None => Ok(Progress::Pending(self.encoded.clone())),
            Some(completion) => {
                self.complete = true;
                Ok(Progress::Complete(completion))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use busbar_contract::abi::mechanism::{extensions, ticket::Ticket};
    fn input() -> Vec<u8> {
        let mut v = conditional::encode_capability(conditional::Capability {
            ticket: Ticket {
                slot: 1,
                generation: 1,
            },
            epoch: 1,
        });
        v.extend(extensions::encode(&[
            (observation::CAPABILITY, b"1"),
            (observation::SCOPE, &[7; 32]),
        ]));
        v
    }
    fn receipt() -> Vec<u8> {
        let mut v = input();
        let receipt = observation::Receipt {
            settlement_id: [1; 16],
            duplicate: true,
            quarantined: false,
        };
        v.extend(extensions::encode(&[(
            observation::RESULT,
            &serde_json::to_vec(&receipt).unwrap(),
        )]));
        v
    }
    #[test]
    fn every_new_component_crosses_before_consuming_a_previous_receipt() {
        let o = observation::Observation::new([7; 32], vec![1], vec![2])
            .count(0, busbar_contract::Count::ONE)
            .unwrap()
            .final_totals();
        let mut first = Crossing::observation(&o, &input()).unwrap();
        assert!(matches!(
            first.poll(&receipt()).unwrap(),
            Progress::Pending(_)
        ));
        assert!(matches!(
            first.poll(&receipt()).unwrap(),
            Progress::Complete(Completion::Observation(_))
        ));
        assert!(first.poll(&receipt()).is_err());
        let mut next = Crossing::observation(&o, &input()).unwrap();
        assert!(matches!(
            next.poll(&receipt()).unwrap(),
            Progress::Pending(_)
        ));
        assert!(matches!(next.poll(&input()).unwrap(), Progress::Pending(_)));
    }
    #[test]
    fn unavailable_capability_or_unrelated_scope_never_authorizes_work() {
        assert!(Crossing::supported(&[]).is_err());
        let mut crossing =
            Crossing::scope(observation::ScopeRequest::attach([8; 32]), &input()).unwrap();
        assert!(matches!(
            crossing.poll(&input()).unwrap(),
            Progress::Pending(_)
        ));
        let mut result = input();
        result.extend(extensions::encode(&[(
            observation::RESULT,
            &serde_json::to_vec(&serde_json::json!({"scope":vec![7;32],"closed":false})).unwrap(),
        )]));
        assert!(crossing.poll(&result).is_err());
    }
}
