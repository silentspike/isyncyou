//! The single closed product provider identity, shared by the HTTP API, the settings file, and
//! the runtime. Parsing rejects every alias/unknown value (no `anthropic`/`openai` normalization),
//! so an unsupported id fails closed with a 400 at the edge and can never reach a provider builder.

/// One of the two supported product providers. There is deliberately no `Other`/alias variant:
/// the type itself is the allowlist.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ProductProviderId {
    Claude,
    Codex,
}

impl ProductProviderId {
    /// Every supported id, for exhaustive per-provider iteration (e.g. status projection).
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    /// Parse a wire/settings value. Only the exact product ids `claude`/`codex` are accepted;
    /// legacy aliases (`anthropic`/`openai`) and everything else are rejected — there is no
    /// normalization layer, so an alias fails closed rather than silently mapping to a product id.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// The canonical wire id (`claude` / `codex`).
    pub fn wire(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

impl std::fmt::Display for ProductProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire())
    }
}

#[cfg(test)]
mod tests {
    use super::ProductProviderId;

    #[test]
    fn parses_only_the_two_product_ids() {
        assert_eq!(
            ProductProviderId::parse("claude"),
            Some(ProductProviderId::Claude)
        );
        assert_eq!(
            ProductProviderId::parse("codex"),
            Some(ProductProviderId::Codex)
        );
    }

    #[test]
    fn rejects_aliases_and_unknown_without_normalization() {
        // legacy BYO aliases must NOT normalize to a product id
        assert_eq!(ProductProviderId::parse("anthropic"), None);
        assert_eq!(ProductProviderId::parse("openai"), None);
        // empty + arbitrary
        assert_eq!(ProductProviderId::parse(""), None);
        assert_eq!(ProductProviderId::parse("Claude"), None); // case-sensitive
        assert_eq!(ProductProviderId::parse("gpt"), None);
    }

    #[test]
    fn wire_roundtrips_and_all_is_exhaustive() {
        for p in ProductProviderId::ALL {
            assert_eq!(ProductProviderId::parse(p.wire()), Some(p));
        }
        assert_eq!(ProductProviderId::ALL.len(), 2);
    }
}
