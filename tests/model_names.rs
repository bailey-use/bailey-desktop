use aivo::services::model_names::{
    anthropic_native_model_name, copilot_model_name, google_native_model_name,
    infer_provider_name_from_model, normalize_claude_version, transform_model_for_openrouter,
    transform_model_for_provider,
};

// ── normalize_claude_version ──────────────────────────────────────────

#[test]
fn normalize_claude_version_table() {
    let cases = [
        ("claude-sonnet-4-6", "claude-sonnet-4.6"),
        ("claude-haiku-4-5", "claude-haiku-4.5"),
        ("claude-opus-4-6", "claude-opus-4.6"),
        ("claude-haiku-4-5-20251001", "claude-haiku-4-5-20251001"),
        ("gpt-4o", "gpt-4o"),
        ("o4-mini", "o4-mini"),
        ("claude-sonnet-4", "claude-sonnet-4"),
        ("", ""),
    ];
    for (input, expected) in cases {
        assert_eq!(
            normalize_claude_version(input),
            expected,
            "normalize_claude_version({input:?})"
        );
    }
}

// ── transform_model_for_openrouter ────────────────────────────────────

#[test]
fn transform_model_for_openrouter_table() {
    let cases = [
        ("claude-sonnet-4-6", "anthropic/claude-sonnet-4.6"),
        ("anthropic/claude-sonnet-4.6", "anthropic/claude-sonnet-4.6"),
        ("gpt-4o", "gpt-4o"),
        ("gemini-2.5-pro", "gemini-2.5-pro"),
    ];
    for (input, expected) in cases {
        assert_eq!(
            transform_model_for_openrouter(input),
            expected,
            "transform_model_for_openrouter({input:?})"
        );
    }
}

// ── transform_model_for_provider ──────────────────────────────────────

#[test]
fn transform_model_for_provider_table() {
    let cases = [
        (
            "https://openrouter.ai/api/v1",
            "claude-sonnet-4-6",
            "anthropic/claude-sonnet-4.6",
        ),
        (
            "https://api.anthropic.com/v1",
            "claude-sonnet-4-6",
            "claude-sonnet-4-6",
        ),
    ];
    for (base_url, model, expected) in cases {
        assert_eq!(
            transform_model_for_provider(None, base_url, model),
            expected,
            "transform_model_for_provider({base_url:?}, {model:?})"
        );
    }
}

// ── copilot_model_name ────────────────────────────────────────────────

#[test]
fn copilot_model_name_table() {
    let cases = [
        ("claude-sonnet-4-6-20250603", "claude-sonnet-4.6"),
        ("claude-haiku-4-5-20250501", "claude-haiku-4.5"),
        ("claude-sonnet-4-6", "claude-sonnet-4.6"),
        ("claude-sonnet-4", "claude-sonnet-4"),
        ("gpt-4o", "gpt-4o"),
    ];
    for (input, expected) in cases {
        assert_eq!(
            copilot_model_name(input),
            expected,
            "copilot_model_name({input:?})"
        );
    }
}

// ── google_native_model_name ──────────────────────────────────────────

#[test]
fn google_native_model_name_table() {
    let cases = [
        ("google/gemini-2.5-pro", "gemini-2.5-pro"),
        ("gemini-2.5-pro", "gemini-2.5-pro"),
    ];
    for (input, expected) in cases {
        assert_eq!(
            google_native_model_name(input),
            expected,
            "google_native_model_name({input:?})"
        );
    }
}

// ── anthropic_native_model_name ───────────────────────────────────────

#[test]
fn anthropic_native_model_name_table() {
    let cases = [
        ("anthropic/claude-sonnet-4.6", "claude-sonnet-4-6"),
        ("claude-haiku-4.5-20251001", "claude-haiku-4-5-20251001"),
        ("MiniMax-M1", "MiniMax-M1"),
        ("claude-sonnet-4-6", "claude-sonnet-4-6"),
    ];
    for (input, expected) in cases {
        assert_eq!(
            anthropic_native_model_name(input),
            expected,
            "anthropic_native_model_name({input:?})"
        );
    }
}

// ── infer_provider_name_from_model ────────────────────────────────────

#[test]
fn infer_provider_name_from_model_table() {
    let cases: &[(&str, Option<&str>)] = &[
        ("claude-sonnet-4-6", Some("anthropic")),
        ("moonshot/kimi-k2.5", Some("moonshot")),
        ("gemini-2.5-pro", Some("google")),
        ("gpt-4o", Some("openai")),
        ("", None),
        ("llama-3.1-70b", None),
    ];
    for &(input, expected) in cases {
        assert_eq!(
            infer_provider_name_from_model(input).as_deref(),
            expected,
            "infer_provider_name_from_model({input:?})"
        );
    }
}
