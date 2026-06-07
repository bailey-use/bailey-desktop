use serde_json::{Value, json};

/// Builds a `/models` response body that satisfies both OpenAI-compatible
/// clients and codex 0.132+'s stricter models manager.
///
/// OpenAI-compatible clients read `{"object":"list","data":[...]}`. Codex's
/// `ModelsResponse` reads `{"models":[<ModelInfo>...]}` and ignores unknown
/// fields, so both shapes can live in one response.
pub(crate) fn build_models_response_body<I, S, O>(models: I) -> Value
where
    I: IntoIterator<Item = (S, O)>,
    S: AsRef<str>,
    O: AsRef<str>,
{
    let entries: Vec<(String, String)> = models
        .into_iter()
        .map(|(id, owned_by)| (id.as_ref().to_string(), owned_by.as_ref().to_string()))
        .collect();
    let openai_entries: Vec<Value> = entries
        .iter()
        .map(|(id, owned_by)| json!({"id": id, "object": "model", "owned_by": owned_by}))
        .collect();
    let codex_entries: Vec<Value> = entries.iter().map(|(id, _)| codex_model_info(id)).collect();
    json!({
        "object": "list",
        "data": openai_entries,
        "models": codex_entries,
    })
}

pub(crate) fn build_models_response_body_for_owner(models: &[String], owned_by: &str) -> Value {
    build_models_response_body(models.iter().map(|id| (id.as_str(), owned_by)))
}

/// Minimal valid `ModelInfo` for codex. Every field serde considers required
/// (no `#[serde(default)]`, no `Option` with default) is present; optional
/// fields are omitted so codex's defaults apply.
fn codex_model_info(id: &str) -> Value {
    json!({
        "slug": id,
        "display_name": id,
        "description": null,
        "supported_reasoning_levels": [],
        "shell_type": "default",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 0,
        "availability_nux": null,
        "upgrade": null,
        "base_instructions": "",
        "supports_reasoning_summaries": false,
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": null,
        "truncation_policy": {"mode": "tokens", "limit": 100_000},
        "supports_parallel_tool_calls": false,
        "experimental_supported_tools": [],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_response_body_satisfies_openai_and_codex_consumers() {
        let body = build_models_response_body([("composer-2.5", "cursor"), ("auto", "cursor")]);

        assert_eq!(body["object"], "list");
        let data = body["data"].as_array().expect("data must be an array");
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["id"], "composer-2.5");
        assert_eq!(data[0]["object"], "model");
        assert_eq!(data[0]["owned_by"], "cursor");

        let models = body["models"].as_array().expect("models must be an array");
        assert_eq!(models.len(), 2);
        let first = &models[0];
        for required in [
            "slug",
            "display_name",
            "description",
            "supported_reasoning_levels",
            "shell_type",
            "visibility",
            "supported_in_api",
            "priority",
            "availability_nux",
            "upgrade",
            "base_instructions",
            "supports_reasoning_summaries",
            "support_verbosity",
            "default_verbosity",
            "apply_patch_tool_type",
            "truncation_policy",
            "supports_parallel_tool_calls",
            "experimental_supported_tools",
        ] {
            assert!(
                first.get(required).is_some(),
                "codex requires field `{required}`: {first:?}"
            );
        }
        assert_eq!(first["slug"], "composer-2.5");
        assert_eq!(first["truncation_policy"]["mode"], "tokens");
    }
}
