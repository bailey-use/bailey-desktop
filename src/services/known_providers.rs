//! Central registry of known AI providers with auto-fill base URLs.
//!
//! Provider data is embedded from `src/data/providers.json` at compile time
//! and parsed once via `LazyLock`. Used by `keys add` for name-based URL
//! auto-detection.

use std::sync::LazyLock;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct KnownProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
}

/// All known providers, ordered so that more specific ids come first
/// (e.g. "openrouter" before "openai") to avoid substring false-positives.
static KNOWN_PROVIDERS: LazyLock<Vec<KnownProvider>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../data/providers.json"))
        .expect("embedded providers.json must be valid")
});

/// Find a provider whose id matches the input as a substring in either
/// direction (case-insensitive). Matches both "my-openrouter-key" (id
/// contained in input) and "google" (input contained in id).
///
/// Requires at least 3 characters of input — the shortest provider id is
/// 3 chars (e.g. "xai", "poe"), and 1–2 char inputs like "hi" or "ai"
/// produce coincidental substring hits against longer ids (e.g. "hi" in
/// "zhipuai"). Display names are intentionally not matched because they
/// contain descriptive words like "China" or "Gateway" that also produce
/// false positives on short inputs.
pub fn find_by_name_substring(input: &str) -> Option<&KnownProvider> {
    matching_providers(input).next()
}

/// Like `find_by_name_substring` but yields every match in declaration order.
/// Used by the interactive picker to hoist *all* matching providers (e.g. an
/// input of "bedrock" matches both `bedrock-mantle` and `bedrock-runtime`).
pub fn find_all_by_name_substring(input: &str) -> Vec<&KnownProvider> {
    matching_providers(input).collect()
}

fn matching_providers(input: &str) -> impl Iterator<Item = &'static KnownProvider> {
    const MIN_LEN: usize = 3;
    let input_lower = (input.len() >= MIN_LEN).then(|| input.to_ascii_lowercase());
    KNOWN_PROVIDERS.iter().filter(move |p| match &input_lower {
        Some(s) => {
            let id_lower = p.id.to_ascii_lowercase();
            id_lower.contains(s) || s.contains(&id_lower)
        }
        None => false,
    })
}

/// Returns all known providers (for the provider picker UI).
pub fn all() -> &'static [KnownProvider] {
    &KNOWN_PROVIDERS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_by_name_substring_matches() {
        let p = find_by_name_substring("my-openrouter-key").unwrap();
        assert_eq!(p.id, "openrouter");

        let p = find_by_name_substring("work_groq").unwrap();
        assert_eq!(p.id, "groq");
    }

    #[test]
    fn find_by_name_substring_no_match() {
        assert!(find_by_name_substring("random").is_none());
        assert!(find_by_name_substring("").is_none());
    }

    #[test]
    fn all_returns_every_provider() {
        let providers = super::all();
        assert!(providers.len() >= 13, "expected at least 13 providers");
        assert!(providers.iter().any(|p| p.id == "openrouter"));
        assert!(providers.iter().any(|p| p.id == "groq"));
    }

    #[test]
    fn substring_match_case_insensitive() {
        let p = find_by_name_substring("My-OpenRouter-Key").unwrap();
        assert_eq!(p.id, "openrouter");

        let p = find_by_name_substring("GROQ_KEY").unwrap();
        assert_eq!(p.id, "groq");
    }

    #[test]
    fn short_input_does_not_match() {
        // Regression: "hi" previously matched "Moonshot AI (China)" because
        // the display name contained "hi" (from "china"), and would also
        // match "zhipuai" by id substring. 1–2 char inputs are never a
        // useful provider hint — require 3+ chars.
        assert!(find_by_name_substring("hi").is_none());
        assert!(find_by_name_substring("ai").is_none());
        assert!(find_by_name_substring("x").is_none());
        assert!(find_by_name_substring("cn").is_none());
    }

    #[test]
    fn descriptive_name_words_do_not_match() {
        // Words that appear only in display names (not ids) should not
        // auto-detect a provider — they're too ambiguous.
        assert!(find_by_name_substring("china").is_none());
        assert!(find_by_name_substring("gateway").is_none());
    }

    #[test]
    fn short_exact_id_still_matches() {
        // 3-char ids are the shortest and must still be detectable.
        let p = find_by_name_substring("xai").unwrap();
        assert_eq!(p.id, "xai");
        let p = find_by_name_substring("poe").unwrap();
        assert_eq!(p.id, "poe");
    }
}
