use std::sync::atomic::{AtomicU8, Ordering};

use super::provider_protocol::{
    PathVariant, ProviderProtocol, decode_route, encode_route, fallback_path_variants,
    fallback_protocols,
};

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
/// Routers walk this list. They keep the first error to surface, but a
/// later terminal error (5xx/auth/rate-limit, see `is_terminal_upstream_error`)
/// overrides the earlier one — a 500 from a real handler is more diagnostic
/// than the leading 404 emitted while probing wrong paths.
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

/// Number of consecutive request-level failures after which the in-memory
/// pin is reset to the configured default route, forcing the next request
/// to re-probe protocols/path variants from scratch. Five is high enough to
/// avoid thrashing on transient network blips and low enough to recover
/// promptly when an upstream genuinely changes shape.
pub const CONSECUTIVE_FAILURES_BEFORE_RESET: u8 = 5;

/// Update the consecutive-failure counter and, if the threshold has been
/// reached, reset the active route to the default. Returns `true` if the
/// pin was reset, so callers can log if useful.
///
/// Pass `succeeded = true` after any 2xx upstream response; `false` after
/// any non-2xx (or transport error) that exits the fallback loop.
pub fn record_request_outcome(
    active_route: &AtomicU8,
    consecutive_failures: &AtomicU8,
    default_protocol: ProviderProtocol,
    default_variant: PathVariant,
    succeeded: bool,
) -> bool {
    if succeeded {
        consecutive_failures.store(0, Ordering::Relaxed);
        return false;
    }
    let prev = consecutive_failures.fetch_add(1, Ordering::Relaxed);
    if prev + 1 >= CONSECUTIVE_FAILURES_BEFORE_RESET {
        active_route.store(
            encode_route(default_protocol, default_variant),
            Ordering::Relaxed,
        );
        consecutive_failures.store(0, Ordering::Relaxed);
        true
    } else {
        false
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
    fn record_outcome_resets_counter_on_success() {
        let active = AtomicU8::new(encode_route(
            ProviderProtocol::Anthropic,
            PathVariant::Stripped,
        ));
        let failures = AtomicU8::new(3);
        let reset = record_request_outcome(
            &active,
            &failures,
            ProviderProtocol::Openai,
            PathVariant::Default,
            true,
        );
        assert!(!reset);
        assert_eq!(failures.load(Ordering::Relaxed), 0);
        // Active route untouched on success.
        assert_eq!(
            decode_route(active.load(Ordering::Relaxed)),
            (ProviderProtocol::Anthropic, PathVariant::Stripped)
        );
    }

    #[test]
    fn record_outcome_resets_pin_at_threshold() {
        let active = AtomicU8::new(encode_route(
            ProviderProtocol::Anthropic,
            PathVariant::Stripped,
        ));
        let failures = AtomicU8::new(0);
        // First N-1 failures bump the counter without resetting.
        for _ in 0..(CONSECUTIVE_FAILURES_BEFORE_RESET - 1) {
            let reset = record_request_outcome(
                &active,
                &failures,
                ProviderProtocol::Openai,
                PathVariant::Default,
                false,
            );
            assert!(!reset);
        }
        // The Nth failure resets the active route to the default.
        let reset = record_request_outcome(
            &active,
            &failures,
            ProviderProtocol::Openai,
            PathVariant::Default,
            false,
        );
        assert!(reset);
        assert_eq!(
            decode_route(active.load(Ordering::Relaxed)),
            (ProviderProtocol::Openai, PathVariant::Default)
        );
        // Counter zeroed so the next failure starts a fresh streak.
        assert_eq!(failures.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn record_outcome_success_after_partial_streak_resets_counter() {
        let active = AtomicU8::new(encode_route(
            ProviderProtocol::Anthropic,
            PathVariant::Default,
        ));
        let failures = AtomicU8::new(0);
        for _ in 0..(CONSECUTIVE_FAILURES_BEFORE_RESET - 2) {
            record_request_outcome(
                &active,
                &failures,
                ProviderProtocol::Openai,
                PathVariant::Default,
                false,
            );
        }
        record_request_outcome(
            &active,
            &failures,
            ProviderProtocol::Openai,
            PathVariant::Default,
            true,
        );
        assert_eq!(failures.load(Ordering::Relaxed), 0);
        // Pin not reset because the streak broke.
        assert_eq!(
            decode_route(active.load(Ordering::Relaxed)),
            (ProviderProtocol::Anthropic, PathVariant::Default)
        );
    }
}
