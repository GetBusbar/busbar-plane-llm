//! The one seam every usage/billing count read in this crate goes through.

/// Read a JSON number as an exact, non-negative integer, or `None`. Tries the integer
pub fn read_count_u64(v: &serde_json::Value) -> Option<u64> {
    if let Some(u) = v.as_u64() {
        return Some(u);
    }
    let f = v.as_f64()?;

    const EXACT_INTEGER_LIMIT: f64 = 9_007_199_254_740_992.0;

    if (0.0..EXACT_INTEGER_LIMIT).contains(&f) && f.fract() == 0.0 {
        Some(f as u64)
    } else {
        None
    }
}

/// A usage field that was there and could not be read as a count. Distinct from absence: "reported
/// nothing" (honestly zero) and "reported something this build does not understand" are different facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableCount {
    /// Which usage field it was.
    pub field: &'static str,
    /// How it was spelled on the wire, bounded so a hostile body cannot write an unbounded log line.
    pub spelling: String,
}

impl std::fmt::Display for UnreadableCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "usage field `{}` is present but is not a count: {}",
            self.field, self.spelling
        )
    }
}

impl std::error::Error for UnreadableCount {}

/// How much of an unreadable value is quoted back: enough to diagnose, bounded against a hostile body.
const SPELLING_BUDGET: usize = 64;

// Limit string scanning as well as output: serde_json otherwise scans the entire
// unescaped string before submitting its first fragment to the capped writer.
fn spelling_string_prefix(s: &str) -> &str {
    let end = s
        .char_indices()
        .nth(SPELLING_BUDGET + 1)
        .map_or(s.len(), |(i, _)| i);
    &s[..end]
}

struct SpellingValue<'a>(&'a serde_json::Value);

impl serde::Serialize for SpellingValue<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self.0 {
            serde_json::Value::String(s) => serializer.serialize_str(spelling_string_prefix(s)),
            serde_json::Value::Array(values) => {
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(&SpellingValue(value))?;
                }
                seq.end()
            }
            serde_json::Value::Object(values) => {
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    map.serialize_entry(spelling_string_prefix(key), &SpellingValue(value))?;
                }
                map.end()
            }
            value => serde::Serialize::serialize(value, serializer),
        }
    }
}

fn bounded_spelling(value: &serde_json::Value) -> String {
    struct Prefix {
        bytes: Vec<u8>,
        truncated: bool,
    }
    impl std::io::Write for Prefix {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let keep = bytes.len().min(SPELLING_BUDGET - self.bytes.len());
            self.bytes.extend_from_slice(&bytes[..keep]);
            if keep < bytes.len() {
                self.truncated = true;
                return Err(std::io::Error::other("diagnostic prefix complete"));
            }
            Ok(keep)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut prefix = Prefix {
        bytes: Vec::with_capacity(SPELLING_BUDGET),
        truncated: false,
    };
    // The only possible error for a JSON Value is our deliberate writer cutoff.
    let _ = serde_json::to_writer(&mut prefix, &SpellingValue(value));
    let valid = match std::str::from_utf8(&prefix.bytes) {
        Ok(_) => prefix.bytes.len(),
        Err(error) => error.valid_up_to(),
    };
    prefix.bytes.truncate(valid);
    let mut spelling = String::from_utf8(prefix.bytes).expect("validated UTF-8 prefix");
    if prefix.truncated {
        spelling.push('…');
    }
    spelling
}

/// Read one billed count out of a usage object: absent is zero, unreadable is a refusal.
pub fn billed_count(
    usage: &serde_json::Value,
    field: &'static str,
) -> Result<u64, UnreadableCount> {
    match usage.get(field) {
        None => Ok(0),
        Some(v) if v.is_null() => Ok(0),
        Some(v) => read_count_u64(v).ok_or_else(|| UnreadableCount {
            field,
            spelling: bounded_spelling(v),
        }),
    }
}

/// [`billed_count`] for a count whose absence is distinct from zero (a cache tier, a per-TTL
/// split): an absent usage object, field or `null` is `None` (never `Some(0)`), a readable count is
/// `Some(n)`, and a present-but-unreadable count is the same [`UnreadableCount`] refusal, never the
/// `None` that reads as "no cache reported".
pub fn billed_count_opt(
    usage: Option<&serde_json::Value>,
    field: &'static str,
) -> Result<Option<u64>, UnreadableCount> {
    match usage {
        Some(u) if u.get(field).is_some_and(|v| !v.is_null()) => billed_count(u, field).map(Some),
        _ => Ok(None),
    }
}

/// A recorded count (one the ledger notes and never bills): absent or `null` is `Some(0)` (as
/// [`billed_count`] reads it), a readable count is `Some(n)`, and a present unreadable one is
/// unknown, `None`.
pub fn recorded_count(
    usage: &serde_json::Value,
    field: &'static str,
    warnings: &mut Vec<String>,
) -> Option<u64> {
    match billed_count(usage, field) {
        Ok(n) => Some(n),
        Err(unreadable) => {
            ::tracing::warn!(
                field = unreadable.field,
                spelling = %unreadable.spelling,
                "a recorded usage count is unreadable: recorded as unknown, never as 0"
            );
            warnings.push(unreadable.to_string());
            None
        }
    }
}

/// Where one usage count lands in the IR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountSlot {
    /// `IrUsage::input_tokens` (uncached input).
    Input,
    /// `IrUsage::output_tokens`.
    Output,
    /// `IrUsage::cache_creation_input_tokens`.
    CacheWrite,
    /// `IrUsage::cache_read_input_tokens`.
    CacheRead,
    /// `IrUsageDetail::reasoning_tokens`.
    Reasoning,
    /// `IrUsageDetail::cache_creation_5m_input_tokens`.
    CacheWrite5m,
    /// `IrUsageDetail::cache_creation_1h_input_tokens`.
    CacheWrite1h,
    /// `IrUsageDetail::web_search_requests`.
    WebSearchRequests,
    /// `IrUsageDetail::web_fetch_requests`.
    WebFetchRequests,
    /// `IrUsageDetail::search_units`.
    SearchUnits,
    /// `IrUsageDetail::billed_input_tokens`.
    ProviderInput,
    /// `IrUsageDetail::billed_output_tokens`.
    ProviderOutput,
    /// `IrUsageDetail::billed_classifications`.
    Classifications,
    /// `IrUsageDetail::tool_use_prompt_tokens`.
    ToolUsePrompt,
    /// `IrUsageDetail::input_audio_tokens`.
    InputAudio,
    /// `IrUsageDetail::output_audio_tokens`.
    OutputAudio,
    /// `IrUsageDetail::accepted_prediction_tokens`.
    AcceptedPrediction,
    /// `IrUsageDetail::rejected_prediction_tokens`.
    RejectedPrediction,
}

const SLOTS: usize = 18;

/// How one count is read off the usage object and folded into its slot. A path names the member
/// from the usage object down; every member before the last is a parent object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountRead {
    /// A required counter: absent or `null` is 0, unreadable REFUSES. Sets the slot.
    Zero(&'static [&'static str]),
    /// An optional counter: absent or `null` is `None` (never `Some(0)`), unreadable REFUSES.
    /// Sets the slot.
    Opt(&'static [&'static str]),
    /// An attribution slice inside a total that is already read: unreadable is `None`, never a
    /// refusal. Sets the slot.
    Lenient(&'static [&'static str]),
    /// A slice the provider counts inside the slot's total (a cached prefix inside the prompt): read
    /// as [`CountRead::Opt`] and subtracted (saturating) from the slot.
    Less(&'static [&'static str]),
    /// A term the provider reports beside the slot's total (thinking beside the answer): read as
    /// [`CountRead::Opt`] and added (saturating) to the slot.
    More(&'static [&'static str]),
    /// The sum of `count` over every entry of the list at `list` whose `key` member is `value`;
    /// `None` when no such entry reports one, unreadable REFUSES (a per-TTL cache list).
    ListSum {
        /// The path to the list.
        list: &'static [&'static str],
        /// The member that names an entry.
        key: &'static str,
        /// The name this slot takes.
        value: &'static str,
        /// The entry's count member.
        count: &'static str,
    },
    /// `count` of the FIRST entry of the list at `list` whose `key` member is `value`, as an
    /// attribution slice: unreadable is `None` (a per-modality list).
    ListFirst {
        /// The path to the list.
        list: &'static [&'static str],
        /// The member that names an entry.
        key: &'static str,
        /// The name this slot takes.
        value: &'static str,
        /// The entry's count member.
        count: &'static str,
    },
}

/// One row of a dialect's usage table.
pub type UsageCount = (CountSlot, CountRead);

/// The member at `path` under `usage` (`None` when any step is absent).
fn at<'a>(usage: Option<&'a serde_json::Value>, path: &[&str]) -> Option<&'a serde_json::Value> {
    path.iter().try_fold(usage?, |v, k| v.get(*k))
}

/// The parent object of the last member of `path`, and that member's name.
fn parent<'a>(
    usage: Option<&'a serde_json::Value>,
    path: &'static [&'static str],
) -> (Option<&'a serde_json::Value>, &'static str) {
    match path.split_last() {
        Some((last, up)) => (at(usage, up), last),
        None => (None, ""),
    }
}

/// One optional counter: absent or `null` is `None`, readable is `Some`, unreadable is the
/// [`UnreadableCount`] the walk refuses on.
fn opt(
    usage: Option<&serde_json::Value>,
    path: &'static [&'static str],
) -> Result<Option<u64>, UnreadableCount> {
    let (up, field) = parent(usage, path);
    billed_count_opt(up, field)
}

/// One billed leaf-operation usage count that is present and unreadable, refused on its own: warned
/// loudly, it bills nothing, nothing stands in for it, and the answer it rode on is served.
pub fn refused_count(protocol: &'static str, unreadable: &UnreadableCount) {
    tracing::warn!(
        protocol,
        field = unreadable.field,
        spelling = %unreadable.spelling,
        "usage count is present but unreadable: REFUSED, it bills nothing and is never read as \
         zero (#42, #81); the answer is served and the readable counts bill as stated"
    );
}

/// Attribution is already included in a billed total: an unreadable split stays unknown,
/// while its diagnostic must remain visible without refusing or changing that total.
fn recorded_optional(
    protocol: &'static str,
    read: Result<Option<u64>, UnreadableCount>,
) -> Option<u64> {
    match read {
        Ok(count) => count,
        Err(unreadable) => {
            tracing::warn!(
                protocol,
                field = unreadable.field,
                spelling = %unreadable.spelling,
                "recorded usage attribution is unreadable: recorded as unknown; billed totals stay as reported"
            );
            None
        }
    }
}

/// The one usage reader. Walks a dialect's usage table over one wire usage object (`None` when
/// absent) and builds the IR usage it states, each row under [`billed_count`]'s contract (absent is
/// zero or `None`, readable is the count, present-but-unreadable is refused).
pub fn read_usage(
    protocol: &'static str,
    usage: Option<&serde_json::Value>,
    table: &[UsageCount],
) -> Result<crate::codec::ir::IrUsage, busbar_contract::protocol::IrError> {
    let kept = |read: Result<Option<u64>, UnreadableCount>| match read {
        Ok(n) => n,
        Err(unreadable) => {
            tracing::warn!(
                protocol,
                field = unreadable.field,
                spelling = %unreadable.spelling,
                "usage count is present but unreadable: REFUSED, it bills nothing and is never \
                 read as zero (#42, #81); the readable counts bill as stated"
            );
            None
        }
    };
    let mut slots = [None::<u64>; SLOTS];
    for &(slot, read) in table {
        let s = &mut slots[slot as usize];
        match read {
            CountRead::Zero(path) => {
                let (up, field) = parent(usage, path);
                *s = kept(up.map_or(Ok(0), |u| billed_count(u, field)).map(Some));
            }
            CountRead::Opt(path) => *s = kept(opt(usage, path)),
            CountRead::Lenient(path) => *s = recorded_optional(protocol, opt(usage, path)),

            CountRead::Less(path) => {
                if let Some(n) = kept(opt(usage, path)) {
                    *s = Some(s.unwrap_or(0).saturating_sub(n));
                }
            }
            CountRead::More(path) => {
                if let Some(n) = kept(opt(usage, path)) {
                    *s = Some(s.unwrap_or(0).saturating_add(n));
                }
            }
            CountRead::ListSum {
                list,
                key,
                value,
                count,
            } => {
                let mut sum: Option<u64> = None;
                for entry in at(usage, list)
                    .and_then(|l| l.as_array())
                    .into_iter()
                    .flatten()
                {
                    if entry.get(key).and_then(|k| k.as_str()) != Some(value) {
                        continue;
                    }
                    if let Some(n) = kept(billed_count_opt(Some(entry), count)) {
                        sum = Some(sum.unwrap_or(0).saturating_add(n));
                    }
                }
                *s = sum;
            }
            CountRead::ListFirst {
                list,
                key,
                value,
                count,
            } => {
                let entry = at(usage, list).and_then(|l| l.as_array()).and_then(|l| {
                    l.iter()
                        .find(|e| e.get(key).and_then(|k| k.as_str()) == Some(value))
                });
                *s = recorded_optional(protocol, billed_count_opt(entry, count));
            }
        }
    }
    let get = |slot: CountSlot| slots[slot as usize];
    Ok(crate::codec::ir::IrUsage {
        input_tokens: get(CountSlot::Input).unwrap_or(0),
        output_tokens: get(CountSlot::Output).unwrap_or(0),
        cache_creation_input_tokens: get(CountSlot::CacheWrite),
        cache_read_input_tokens: get(CountSlot::CacheRead),
        detail: crate::codec::ir::IrUsageDetail {
            reasoning_tokens: get(CountSlot::Reasoning),
            cache_creation_5m_input_tokens: get(CountSlot::CacheWrite5m),
            cache_creation_1h_input_tokens: get(CountSlot::CacheWrite1h),
            web_search_requests: get(CountSlot::WebSearchRequests),
            web_fetch_requests: get(CountSlot::WebFetchRequests),
            search_units: get(CountSlot::SearchUnits),
            billed_input_tokens: get(CountSlot::ProviderInput),
            billed_output_tokens: get(CountSlot::ProviderOutput),
            billed_classifications: get(CountSlot::Classifications),
            tool_use_prompt_tokens: get(CountSlot::ToolUsePrompt),
            input_audio_tokens: get(CountSlot::InputAudio),
            output_audio_tokens: get(CountSlot::OutputAudio),
            accepted_prediction_tokens: get(CountSlot::AcceptedPrediction),
            rejected_prediction_tokens: get(CountSlot::RejectedPrediction),
            ..Default::default()
        },
    })
}

/// Whether [`read_usage`] refused a count of `usage` under `table` (a billed row present, not
/// `null`, not a count).
#[must_use]
pub fn refused_any(usage: Option<&serde_json::Value>, table: &[UsageCount]) -> bool {
    table.iter().any(|&(_, read)| match read {
        CountRead::Zero(path)
        | CountRead::Opt(path)
        | CountRead::Less(path)
        | CountRead::More(path) => opt(usage, path).is_err(),
        CountRead::ListSum {
            list,
            key,
            value,
            count,
        } => at(usage, list)
            .and_then(|l| l.as_array())
            .into_iter()
            .flatten()
            .filter(|entry| entry.get(key).and_then(|k| k.as_str()) == Some(value))
            .any(|entry| billed_count_opt(Some(entry), count).is_err()),
        CountRead::Lenient(_) | CountRead::ListFirst { .. } => false,
    })
}

/// The stated-total cross-check: the total a provider states beside its itemized counts, against
/// what the usage table ledgers from them (the four meter classes: uncached input, cache read, cache
/// write, output).
#[must_use]
pub fn stated_total_note(
    protocol: &'static str,
    identity: &'static str,
    total: Option<&serde_json::Value>,
    ledgered: &crate::codec::ir::IrUsage,
) -> Option<crate::codec::ir::UsageIdentityNote> {
    let reported_total = total.and_then(read_count_u64)?;
    let summed_total = ledgered.billable_tokens();
    if summed_total == reported_total {
        return None;
    }
    let unaccounted = if reported_total > summed_total {
        i64::try_from(reported_total - summed_total).unwrap_or(i64::MAX)
    } else {
        i64::try_from(summed_total - reported_total).map_or(i64::MIN, |gap| -gap)
    };
    tracing::warn!(
        protocol,
        identity,
        reported_total,
        summed_total,
        unaccounted,
        "usage does not reconcile: the provider's stated total disagrees with the sum of the \
         meter classes its itemized counts ledger (input, cache read, cache write, output). The \
         itemized counts are ledgered as reported; a positive gap is ledgered as the open class \
         unitemized_tokens, a count the provider reports that this dialect's usage table does not \
         itemize."
    );
    Some(crate::codec::ir::UsageIdentityNote {
        reported_total,
        summed_total,
        unaccounted,
        identity,
    })
}

#[cfg(test)]
#[path = "tests/usage_count_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/usage_total_boundary_tests.rs"]
mod usage_total_boundary_tests;
