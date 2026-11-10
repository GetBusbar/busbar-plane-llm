//! Native Gemini text tariff profiles. Class identities are stable ABI indices; prices are
//! exclusively operator supplied and proved by host scope admission before paid dispatch.
//!
//! Service-tier request semantics checked 2026-11-10:
//! <https://ai.google.dev/gemini-api/docs/flex-inference>
//! <https://ai.google.dev/gemini-api/docs/priority-inference>
//! No automatic retry or tier upgrade is performed, including after Flex capacity errors.

/// First appended native class, after existing token/open/request classes.
pub const FIRST_CLASS: usize = 22;
/// Dedicated classes in stable input/output/cache order, separately priced for each tier.
pub const CLASSES: [&str; 6] = [
    "interactions_flex_input_tokens",
    "interactions_flex_output_tokens",
    "interactions_flex_cache_read_tokens",
    "interactions_priority_input_tokens",
    "interactions_priority_output_tokens",
    "interactions_priority_cache_read_tokens",
];

/// A disjoint text profile, including cached input and separately reported thinking output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextClasses {
    /// Uncached prompt tokens.
    pub input: u32,
    /// Output and thinking tokens (separate components at the same rate).
    pub output: u32,
    /// Cached prompt tokens, subtracted from inclusive input.
    pub cache_read: u32,
}
impl TextClasses {
    /// All rates required before creating or reattaching original host custody.
    pub fn required(self) -> [u32; 3] {
        [self.input, self.output, self.cache_read]
    }
}

/// Exact provider tier to declared classes. Unknown tiers never inherit standard prices.
pub fn text_classes(tier: &str) -> Result<TextClasses, &'static str> {
    let first = match tier {
        "standard" => 0,
        "flex" => FIRST_CLASS as u32,
        "priority" => FIRST_CLASS as u32 + 3,
        _ => return Err("unsupported native service tier"),
    };
    Ok(TextClasses {
        input: first,
        output: first + 1,
        cache_read: first + 2,
    })
}
