use std::sync::atomic::{AtomicU8, Ordering};

use super::provider_protocol::{ProviderProtocol, fallback_protocols};

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

/// Returns the ordered list of protocol candidates: active first, then fallbacks.
pub fn protocol_candidates(active_protocol: &AtomicU8) -> Vec<ProviderProtocol> {
    let current = ProviderProtocol::from_u8(active_protocol.load(Ordering::Relaxed));
    std::iter::once(current)
        .chain(fallback_protocols(current))
        .collect()
}

/// If this was a fallback attempt (attempt > 0), store the winning protocol and log.
pub fn commit_protocol_switch(
    active_protocol: &AtomicU8,
    protocol: ProviderProtocol,
    attempt: usize,
) {
    if attempt > 0 {
        active_protocol.store(protocol.to_u8(), Ordering::Relaxed);
        eprintln!("  • Protocol auto-switched to {}", protocol.as_str());
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
    fn protocol_candidates_starts_with_current() {
        let active = AtomicU8::new(ProviderProtocol::Google.to_u8());
        let candidates = protocol_candidates(&active);
        assert_eq!(candidates[0], ProviderProtocol::Google);
        assert!(candidates.len() > 1);
        assert!(!candidates[1..].contains(&ProviderProtocol::Google));
    }

    #[test]
    fn commit_switch_stores_on_fallback() {
        let active = AtomicU8::new(ProviderProtocol::Openai.to_u8());
        commit_protocol_switch(&active, ProviderProtocol::Google, 1);
        assert_eq!(
            ProviderProtocol::from_u8(active.load(Ordering::Relaxed)),
            ProviderProtocol::Google
        );
    }

    #[test]
    fn commit_switch_noop_on_first_attempt() {
        let active = AtomicU8::new(ProviderProtocol::Openai.to_u8());
        commit_protocol_switch(&active, ProviderProtocol::Google, 0);
        assert_eq!(
            ProviderProtocol::from_u8(active.load(Ordering::Relaxed)),
            ProviderProtocol::Openai
        );
    }

    #[test]
    fn protocol_candidates_anthropic_starts_with_anthropic() {
        let active = AtomicU8::new(ProviderProtocol::Anthropic.to_u8());
        let candidates = protocol_candidates(&active);
        assert_eq!(candidates[0], ProviderProtocol::Anthropic);
        assert!(!candidates[1..].contains(&ProviderProtocol::Anthropic));
    }

    #[test]
    fn protocol_candidates_openai_starts_with_openai() {
        let active = AtomicU8::new(ProviderProtocol::Openai.to_u8());
        let candidates = protocol_candidates(&active);
        assert_eq!(candidates[0], ProviderProtocol::Openai);
        assert!(!candidates[1..].contains(&ProviderProtocol::Openai));
    }

    #[test]
    fn commit_switch_stores_on_later_attempt() {
        let active = AtomicU8::new(ProviderProtocol::Openai.to_u8());
        commit_protocol_switch(&active, ProviderProtocol::Anthropic, 2);
        assert_eq!(
            ProviderProtocol::from_u8(active.load(Ordering::Relaxed)),
            ProviderProtocol::Anthropic
        );
    }
}
