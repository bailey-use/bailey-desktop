use std::sync::atomic::{AtomicU8, Ordering};

use super::provider_protocol::{
    AttemptClassification, PathVariant, ProviderProtocol, decode_route, encode_route,
    fallback_path_variants, fallback_protocols,
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

/// Decide whether a fallback cascade should stop after a single mismatch
/// instead of continuing to the next `(protocol, path_variant)` candidate.
///
/// Centralises three break conditions so every router applies them
/// identically:
///
/// 1. **Semantic rejection** — a 4xx with a structured LLM-API error
///    envelope (`error.type` / `error.code` / `error.status`). The upstream
///    parsed our request and answered authoritatively; another candidate
///    cannot do better.
/// 2. **Terminal error past attempt 0** — auth/rate-limit/5xx from a
///    fallback candidate. Attempt 0 is exempted because a 401 there can
///    also mean "this host doesn't recognize my auth-header shape" (e.g.
///    DeepSeek seeing Google's `x-goog-api-key`); we probe one fallback
///    before bailing on that family of errors.
/// 3. **Any error on a proven route's attempt 0** — once the active route
///    has answered authoritatively at least once (`route_proven`), attempt
///    0 is the *proven* path, not a guess. Errors here are the request's
///    fault, not the route's; fanning out across 4 unrelated protocol
///    shapes just amplifies the same upstream failure into N gateway hits.
pub fn should_bail_on_mismatch(
    attempt: usize,
    classification: &AttemptClassification,
    route_proven: bool,
) -> bool {
    if classification.is_semantic_rejection {
        return true;
    }
    if classification.is_terminal && attempt > 0 {
        return true;
    }
    if attempt == 0 && route_proven {
        return true;
    }
    false
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

    fn cls(is_terminal: bool, is_semantic_rejection: bool) -> AttemptClassification {
        AttemptClassification {
            is_terminal,
            is_semantic_rejection,
            quirk_hint: None,
        }
    }

    #[test]
    fn bail_on_semantic_rejection_at_any_attempt() {
        let c = cls(false, true);
        for attempt in [0usize, 1, 5] {
            for proven in [false, true] {
                assert!(
                    should_bail_on_mismatch(attempt, &c, proven),
                    "expected bail for attempt={attempt}, proven={proven}"
                );
            }
        }
    }

    #[test]
    fn bail_on_terminal_only_after_attempt_zero() {
        let c = cls(true, false);
        // Attempt 0: a 401/403/429/5xx could still be the upstream rejecting
        // our auth-header *shape* (e.g. DeepSeek seeing `x-goog-api-key`).
        // Probe one fallback before bailing.
        assert!(!should_bail_on_mismatch(0, &c, false));
        assert!(should_bail_on_mismatch(1, &c, false));
        assert!(should_bail_on_mismatch(2, &c, false));
    }

    #[test]
    fn bail_on_attempt_zero_when_route_is_proven() {
        // Pin-trust: once the active route has answered authoritatively at
        // least once (2xx or semantic rejection), attempt 0 is the proven
        // path, not a guess. An error here is the request's fault — surface
        // it instead of fanning out across 4 unrelated protocol shapes.
        let c = cls(false, false);
        assert!(should_bail_on_mismatch(0, &c, true));
    }

    #[test]
    fn no_bail_on_attempt_zero_when_route_is_unproven() {
        // The "first request after launch" scenario: route is just a guess.
        // A masked gateway error like {"error":"Upstream request failed"}
        // (status 400) is neither terminal nor a semantic rejection, so
        // the cascade legitimately probes the next candidate.
        let c = cls(false, false);
        assert!(!should_bail_on_mismatch(0, &c, false));
    }

    #[test]
    fn no_bail_on_unproven_attempt_one_with_unstructured_error() {
        // Even past attempt 0, an unstructured error on an unproven route
        // shouldn't bail — we still haven't seen *any* candidate respond
        // authoritatively, so the cascade keeps walking.
        let c = cls(false, false);
        assert!(!should_bail_on_mismatch(1, &c, false));
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
