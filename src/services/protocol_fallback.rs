use std::sync::atomic::{AtomicU8, Ordering};

use super::provider_protocol::{
    PathVariant, ProviderProtocol, decode_route, encode_route, fallback_path_variants,
    fallback_protocols,
};

/// Per-candidate record captured during a fallback loop. The router uses it
/// only for diagnostic logging when every candidate fails — the real upstream
/// response that gets returned to the client is built separately from the
/// first attempt's body.
#[derive(Debug, Clone)]
pub struct AttemptRecord {
    pub protocol: ProviderProtocol,
    pub variant: PathVariant,
    pub status: u16,
    pub body_preview: String,
}

impl AttemptRecord {
    pub fn new(protocol: ProviderProtocol, variant: PathVariant, status: u16, body: &str) -> Self {
        Self {
            protocol,
            variant,
            status,
            body_preview: truncate_body_for_log(body),
        }
    }
}

fn truncate_body_for_log(body: &str) -> String {
    const LIMIT: usize = 200;
    let trimmed = body.trim();
    let mut iter = trimmed.chars();
    let head: String = iter.by_ref().take(LIMIT).collect();
    if iter.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn variant_label(v: PathVariant) -> &'static str {
    match v {
        PathVariant::Default => "default",
        PathVariant::Stripped => "stripped",
    }
}

/// Emit a multi-line diagnostic to stderr listing every fallback attempt and
/// the status it returned. Skipped when fewer than 2 attempts were tried (a
/// single failure is its own diagnostic — surfacing it in the response body
/// is enough). `context` is a short label for the router (e.g. `"claude"`).
pub fn log_exhausted_fallback(context: &str, attempts: &[AttemptRecord]) {
    if attempts.len() < 2 {
        return;
    }
    eprintln!(
        "  • {context} fallback exhausted, tried {} routes:",
        attempts.len()
    );
    for record in attempts {
        eprintln!(
            "      {:9} {:8} → {} {}",
            record.protocol.as_str(),
            variant_label(record.variant),
            record.status,
            record.body_preview,
        );
    }
}

/// Outcome of a single protocol attempt in the fallback loop.
pub enum AttemptOutcome<T> {
    Success(T),
    /// Non-success HTTP status — try the next candidate. Body is preserved so
    /// the router can surface the real upstream error after exhaustion.
    Mismatch {
        status: u16,
        body: String,
    },
}

/// Returns the ordered list of `(protocol, path_variant)` candidates: the
/// active route first, then the active protocol with its alternate path
/// variant, then each fallback protocol with the default variant, then each
/// fallback protocol with the stripped variant.
///
/// Routers iterate this list and capture only the FIRST `Mismatch` outcome —
/// the active/pinned route's response is the most informative error to
/// surface (genuine 401/429/5xx from the configured protocol, not the
/// trailing 404 from a probe of the wrong endpoint).
pub fn protocol_candidates(active_route: &AtomicU8) -> Vec<(ProviderProtocol, PathVariant)> {
    let (current_proto, current_variant) = decode_route(active_route.load(Ordering::Relaxed));

    let mut out: Vec<(ProviderProtocol, PathVariant)> = Vec::new();
    for variant in fallback_path_variants(current_proto, current_variant) {
        out.push((current_proto, variant));
    }
    let fallbacks: Vec<ProviderProtocol> = fallback_protocols(current_proto);
    for proto in &fallbacks {
        out.push((*proto, PathVariant::Default));
    }
    for proto in &fallbacks {
        if proto.supports_path_variants() {
            out.push((*proto, PathVariant::Stripped));
        }
    }
    out
}

/// If this was a fallback attempt (attempt > 0), store the winning route.
pub fn commit_protocol_switch(
    active_route: &AtomicU8,
    protocol: ProviderProtocol,
    variant: PathVariant,
    attempt: usize,
) {
    if attempt > 0 {
        active_route.store(encode_route(protocol, variant), Ordering::Relaxed);
    }
}

/// Classify an HTTP response into an attempt outcome.
pub fn classify_attempt<T>(
    status: u16,
    response_text: String,
    success: Option<T>,
) -> AttemptOutcome<T> {
    match success {
        Some(val) => AttemptOutcome::Success(val),
        None => AttemptOutcome::Mismatch {
            status,
            body: response_text,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_attempt_success() {
        match classify_attempt(200, String::new(), Some(42)) {
            AttemptOutcome::Success(v) => assert_eq!(v, 42),
            _ => panic!("expected Success"),
        }
    }

    #[test]
    fn classify_attempt_any_error_is_mismatch() {
        for status in [400, 401, 403, 404, 405, 415, 422, 429, 500, 501, 502, 503] {
            match classify_attempt::<()>(status, "err".into(), None) {
                AttemptOutcome::Mismatch { status: s, .. } => assert_eq!(s, status),
                _ => panic!("expected Mismatch for {status}"),
            }
        }
    }

    #[test]
    fn classify_attempt_preserves_body() {
        let body = r#"{"error":{"code":"invalid_api_key","message":"Bad key"}}"#;
        match classify_attempt::<()>(401, body.into(), None) {
            AttemptOutcome::Mismatch { status, body: b } => {
                assert_eq!(status, 401);
                assert_eq!(b, body);
            }
            _ => panic!("expected Mismatch"),
        }
    }

    #[test]
    fn classify_attempt_success_ignores_status() {
        // When success is Some, status is irrelevant
        match classify_attempt(500, "error body".into(), Some("ok")) {
            AttemptOutcome::Success(v) => assert_eq!(v, "ok"),
            _ => panic!("expected Success even with error status"),
        }
    }

    #[test]
    fn protocol_candidates_starts_with_current_route() {
        let active = AtomicU8::new(ProviderProtocol::Google.to_u8());
        let candidates = protocol_candidates(&active);
        assert_eq!(
            candidates[0],
            (ProviderProtocol::Google, PathVariant::Default)
        );
        assert!(candidates.len() > 1);
    }

    #[test]
    fn protocol_candidates_includes_both_variants_for_active() {
        let active = AtomicU8::new(ProviderProtocol::Openai.to_u8());
        let candidates = protocol_candidates(&active);
        assert_eq!(
            candidates[0],
            (ProviderProtocol::Openai, PathVariant::Default)
        );
        assert_eq!(
            candidates[1],
            (ProviderProtocol::Openai, PathVariant::Stripped)
        );
    }

    #[test]
    fn protocol_candidates_skips_stripped_variant_for_google() {
        let active = AtomicU8::new(ProviderProtocol::Openai.to_u8());
        let candidates = protocol_candidates(&active);
        assert!(candidates.contains(&(ProviderProtocol::Google, PathVariant::Default)));
        assert!(!candidates.contains(&(ProviderProtocol::Google, PathVariant::Stripped)));
    }

    #[test]
    fn protocol_candidates_total_count_seven_for_three_variant_protocols() {
        // 3 variant-supporting protocols × 2 variants + Google × 1 = 7
        let active = AtomicU8::new(ProviderProtocol::Openai.to_u8());
        let candidates = protocol_candidates(&active);
        assert_eq!(candidates.len(), 7);
    }

    #[test]
    fn commit_switch_stores_route_on_fallback() {
        let active = AtomicU8::new(ProviderProtocol::Openai.to_u8());
        commit_protocol_switch(&active, ProviderProtocol::Google, PathVariant::Default, 1);
        let (proto, variant) = decode_route(active.load(Ordering::Relaxed));
        assert_eq!(proto, ProviderProtocol::Google);
        assert_eq!(variant, PathVariant::Default);
    }

    #[test]
    fn commit_switch_stores_stripped_variant() {
        let active = AtomicU8::new(ProviderProtocol::Openai.to_u8());
        commit_protocol_switch(
            &active,
            ProviderProtocol::Anthropic,
            PathVariant::Stripped,
            1,
        );
        let (proto, variant) = decode_route(active.load(Ordering::Relaxed));
        assert_eq!(proto, ProviderProtocol::Anthropic);
        assert_eq!(variant, PathVariant::Stripped);
    }

    #[test]
    fn commit_switch_noop_on_first_attempt() {
        let active = AtomicU8::new(ProviderProtocol::Openai.to_u8());
        commit_protocol_switch(&active, ProviderProtocol::Google, PathVariant::Stripped, 0);
        let (proto, variant) = decode_route(active.load(Ordering::Relaxed));
        assert_eq!(proto, ProviderProtocol::Openai);
        assert_eq!(variant, PathVariant::Default);
    }

    #[test]
    fn decode_route_backward_compatible_with_persisted_protocol_only() {
        // Pre-existing persisted values (0..=3) must decode as Default variant.
        for raw in 0u8..=3 {
            let (_, variant) = decode_route(raw);
            assert_eq!(variant, PathVariant::Default, "raw byte {raw}");
        }
    }

    #[test]
    fn attempt_record_preserves_short_body() {
        let r = AttemptRecord::new(
            ProviderProtocol::Openai,
            PathVariant::Default,
            401,
            "bad key",
        );
        assert_eq!(r.body_preview, "bad key");
    }

    #[test]
    fn attempt_record_truncates_long_body() {
        let body: String = "x".repeat(500);
        let r = AttemptRecord::new(ProviderProtocol::Openai, PathVariant::Default, 502, &body);
        // Truncated to 200 chars + ellipsis
        assert_eq!(r.body_preview.chars().count(), 201);
        assert!(r.body_preview.ends_with('…'));
    }

    #[test]
    fn attempt_record_handles_multibyte_truncation() {
        let body: String = "日本語テスト".repeat(100);
        let r = AttemptRecord::new(ProviderProtocol::Openai, PathVariant::Default, 500, &body);
        // Must not panic on char-boundary truncation (would have on byte slice)
        assert!(r.body_preview.ends_with('…'));
        assert!(r.body_preview.is_char_boundary(r.body_preview.len()));
    }

    #[test]
    fn attempt_record_trims_whitespace() {
        let r = AttemptRecord::new(
            ProviderProtocol::Openai,
            PathVariant::Default,
            400,
            "  oops  \n",
        );
        assert_eq!(r.body_preview, "oops");
    }
}
