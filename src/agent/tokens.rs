//! Token-accounting primitives for the agent engine: conservative token
//! estimates (chars/4, with images counted flat rather than by base64 length),
//! `usage`-object parsing, and the measured/estimate calibration ratio. Pure
//! functions with no engine state — compaction and the loop call in.

use serde_json::{Value, json};

/// Flat per-image token cost — counting the base64 verbatim would blow the budget.
pub(crate) const IMAGE_TOKEN_ESTIMATE: usize = 1_500;

/// Default recent-window size held out of compaction.
pub(crate) const KEEP_RECENT_TOKENS: usize = 20_000;

/// Ceiling on the calibration multiplier — clamps a stray measurement.
pub(crate) const MAX_CALIBRATION: f64 = 2.5;

/// Below this estimate the measured/estimate ratio is too noisy to calibrate from.
pub(crate) const CALIBRATION_MIN_SAMPLE: usize = 2_000;

/// Recent-window size held out of compaction; `AIVO_AGENT_KEEP_RECENT` overrides.
pub(crate) fn keep_recent_tokens() -> usize {
    std::env::var("AIVO_AGENT_KEEP_RECENT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(KEEP_RECENT_TOKENS)
}

/// Measured/estimate ratio clamped to [1.0, [`MAX_CALIBRATION`]]; `.max(1)` keeps the division safe.
pub(crate) fn calibration_ratio(measured: u64, estimate: usize) -> f64 {
    (measured as f64 / estimate.max(1) as f64).clamp(1.0, MAX_CALIBRATION)
}

/// Total tokens from an OpenAI/Anthropic-style `usage` object (0 if absent).
pub(crate) fn usage_tokens(usage: &Option<Value>) -> u64 {
    let Some(u) = usage else {
        return 0;
    };
    if let Some(t) = u.get("total_tokens").and_then(|x| x.as_u64()) {
        return t;
    }
    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| u.get(*k).and_then(|x| x.as_u64()))
            .unwrap_or(0)
    };
    pick(&["input_tokens", "prompt_tokens"]) + pick(&["output_tokens", "completion_tokens"])
}

/// Flatten a user content value (string or multimodal array) into an array of parts.
pub(crate) fn content_to_parts(v: Value) -> Vec<Value> {
    match v {
        Value::Array(parts) => parts,
        Value::String(s) if s.is_empty() => Vec::new(),
        Value::String(s) => vec![json!({"type": "text", "text": s})],
        other => vec![other],
    }
}

pub(crate) fn is_image_part(part: &Value) -> bool {
    part.get("type").and_then(|t| t.as_str()) == Some("image_url")
}

/// Conservative token estimate: serialized JSON length / 4 (pi's heuristic).
pub(crate) fn estimate_tokens(messages: &[Value]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

/// chars/4, but each image part counts as a flat [`IMAGE_TOKEN_ESTIMATE`] — its base64
/// length would otherwise force needless compaction. Non-image messages are unchanged.
pub(crate) fn estimate_message_tokens(m: &Value) -> usize {
    if let Some(Value::Array(parts)) = m.get("content")
        && parts.iter().any(is_image_part)
    {
        let content: usize = parts
            .iter()
            .map(|p| {
                if is_image_part(p) {
                    IMAGE_TOKEN_ESTIMATE
                } else {
                    serde_json::to_string(p).map(|s| s.len()).unwrap_or(0) / 4
                }
            })
            .sum();
        return content + 4;
    }
    serde_json::to_string(m).map(|s| s.len()).unwrap_or(0) / 4
}

/// chars/4 token estimate for a plain string (same heuristic as [`estimate_tokens`]).
pub(crate) fn estimate_str_tokens(s: &str) -> usize {
    s.len() / 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_counts_image_flat_not_base64_length() {
        // A ~200KB base64 blob would be ~50k "tokens" at chars/4 — must count flat instead.
        let big = "A".repeat(200_000);
        let msg = json!({"role": "user", "content": [
            {"type": "text", "text": "hi"},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{big}")}},
        ]});
        let est = estimate_tokens(std::slice::from_ref(&msg));
        assert!(est < 3_000, "image bulk inflated the estimate: {est}");
    }

    #[test]
    fn usage_tokens_handles_both_shapes() {
        assert_eq!(usage_tokens(&Some(json!({"total_tokens": 42}))), 42);
        assert_eq!(
            usage_tokens(&Some(json!({"input_tokens": 10, "output_tokens": 5}))),
            15
        );
        assert_eq!(usage_tokens(&None), 0);
    }
}
