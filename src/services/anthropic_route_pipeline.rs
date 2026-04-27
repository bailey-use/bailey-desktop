//! Request patch pipeline for Aivo's Anthropic-compatible routing.
//!
//! This keeps provider-specific request quirks modular so routers stay focused on
//! transport and streaming.

use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

use crate::services::model_names::transform_model_for_provider;

pub struct RequestContext<'a> {
    pub upstream_base_url: &'a str,
}

pub trait RequestPatch: Send + Sync {
    fn patch_json(&self, _route: &str, _body: &mut Value, _ctx: &RequestContext<'_>) -> Result<()> {
        Ok(())
    }

    fn patch_headers(
        &self,
        _route: &str,
        _headers: &mut HeaderMap,
        _ctx: &RequestContext<'_>,
    ) -> Result<()> {
        Ok(())
    }
}

pub struct RouterPipeline {
    patches: Vec<Box<dyn RequestPatch>>,
}

impl RouterPipeline {
    pub fn new(patches: Vec<Box<dyn RequestPatch>>) -> Self {
        Self { patches }
    }

    pub fn for_openrouter() -> Self {
        Self::new(vec![
            Box::new(CacheControlPatch),
            Box::new(ModelNamePatch),
            Box::new(AnthropicVersionPatch),
        ])
    }

    pub fn patch_json(
        &self,
        route: &str,
        body: &mut Value,
        ctx: &RequestContext<'_>,
    ) -> Result<()> {
        for patch in &self.patches {
            patch.patch_json(route, body, ctx)?;
        }
        Ok(())
    }

    pub fn patch_headers(
        &self,
        route: &str,
        headers: &mut HeaderMap,
        ctx: &RequestContext<'_>,
    ) -> Result<()> {
        for patch in &self.patches {
            patch.patch_headers(route, headers, ctx)?;
        }
        Ok(())
    }
}

/// Normalizes provider model names (e.g. OpenRouter model prefix/version shape).
pub struct ModelNamePatch;

impl RequestPatch for ModelNamePatch {
    fn patch_json(&self, _route: &str, body: &mut Value, ctx: &RequestContext<'_>) -> Result<()> {
        if let Some(model) = body.get_mut("model")
            && let Some(model_str) = model.as_str()
        {
            *model = Value::String(transform_model_for_provider(
                ctx.upstream_base_url,
                model_str,
            ));
        }
        Ok(())
    }
}

/// Injects `cache_control` on the system prompt and last user message for Anthropic prompt caching.
pub struct CacheControlPatch;

impl RequestPatch for CacheControlPatch {
    fn patch_json(&self, route: &str, body: &mut Value, _ctx: &RequestContext<'_>) -> Result<()> {
        match route {
            "messages" => {
                if let Some(system) = body.get_mut("system") {
                    inject_cache_control_on_last_block(system);
                }
            }
            "chat/completions" => {
                inject_chat_completions_cache_control(body);
                return Ok(());
            }
            _ => return Ok(()),
        }

        if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for msg in messages.iter_mut().rev() {
                if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
                    continue;
                }
                if let Some(content) = msg.get_mut("content") {
                    inject_cache_control_on_last_block(content);
                }
                break;
            }
        }
        Ok(())
    }
}

/// Inject `cache_control` markers on an OpenAI Chat Completions request body.
/// Adds markers to the system message and last user message.
pub(crate) fn inject_chat_completions_cache_control(body: &mut Value) {
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut().rev() {
            if msg.get("role").and_then(|r| r.as_str()) == Some("system") {
                if let Some(content) = msg.get_mut("content") {
                    inject_cache_control_on_last_block(content);
                }
                break;
            }
        }
        for msg in messages.iter_mut().rev() {
            if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
                continue;
            }
            if let Some(content) = msg.get_mut("content") {
                inject_cache_control_on_last_block(content);
            }
            break;
        }
    }
}

/// Recursively remove `cache_control` keys from any object inside `body`.
/// Used for upstreams that reject Anthropic-specific cache_control on
/// system/message content (e.g., Bedrock-style shims). Walks the JSON tree
/// rather than enumerating known sites so future schema additions stay safe.
pub(crate) fn strip_cache_control(body: &mut Value) {
    match body {
        Value::Object(obj) => {
            obj.remove("cache_control");
            for (_k, v) in obj.iter_mut() {
                strip_cache_control(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                strip_cache_control(v);
            }
        }
        _ => {}
    }
}

pub(crate) fn inject_cache_control_on_last_block(value: &mut Value) {
    match value {
        Value::String(s) => {
            let text = s.clone();
            *value = json!([{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"}
            }]);
        }
        Value::Array(blocks) => {
            if let Some(last) = blocks.last_mut()
                && last.get("cache_control").is_none()
            {
                last["cache_control"] = json!({"type": "ephemeral"});
            }
        }
        _ => {}
    }
}

/// Adds Anthropic API version header where required by Anthropic-format endpoints.
pub struct AnthropicVersionPatch;

impl RequestPatch for AnthropicVersionPatch {
    fn patch_headers(
        &self,
        route: &str,
        headers: &mut HeaderMap,
        _ctx: &RequestContext<'_>,
    ) -> Result<()> {
        if matches!(route, "messages" | "messages/count_tokens") {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_name_patch_openrouter_transform() {
        let patch = ModelNamePatch;
        let mut body = serde_json::json!({"model":"claude-sonnet-4-6"});
        let ctx = RequestContext {
            upstream_base_url: "https://openrouter.ai/api/v1",
        };
        patch.patch_json("messages", &mut body, &ctx).unwrap();
        assert_eq!(body["model"], "anthropic/claude-sonnet-4.6");
    }

    #[test]
    fn test_model_name_patch_non_openrouter_passthrough() {
        let patch = ModelNamePatch;
        let mut body = serde_json::json!({"model":"claude-sonnet-4-6"});
        let ctx = RequestContext {
            upstream_base_url: "https://api.example.com/v1",
        };
        patch.patch_json("messages", &mut body, &ctx).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4-6");
    }

    #[test]
    fn test_anthropic_version_patch_only_messages_routes() {
        let patch = AnthropicVersionPatch;
        let ctx = RequestContext {
            upstream_base_url: "https://openrouter.ai/api/v1",
        };

        let mut headers = HeaderMap::new();
        patch.patch_headers("messages", &mut headers, &ctx).unwrap();
        assert!(headers.get("anthropic-version").is_some());

        let mut headers = HeaderMap::new();
        patch
            .patch_headers("chat/completions", &mut headers, &ctx)
            .unwrap();
        assert!(headers.get("anthropic-version").is_none());
    }

    #[test]
    fn test_cache_control_patch_converts_string_system_to_block() {
        let patch = CacheControlPatch;
        let mut body = serde_json::json!({
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "Hi"}]
        });
        let ctx = RequestContext {
            upstream_base_url: "https://api.anthropic.com/v1",
        };
        patch.patch_json("messages", &mut body, &ctx).unwrap();

        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "You are helpful.");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");

        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_cache_control_patch_adds_to_existing_blocks() {
        let patch = CacheControlPatch;
        let mut body = serde_json::json!({
            "system": [{"type": "text", "text": "First"}, {"type": "text", "text": "Second"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hello"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Hi"}]},
                {"role": "user", "content": [{"type": "text", "text": "Bye"}]}
            ]
        });
        let ctx = RequestContext {
            upstream_base_url: "https://api.anthropic.com/v1",
        };
        patch.patch_json("messages", &mut body, &ctx).unwrap();

        // Only last system block gets cache_control
        assert!(body["system"][0].get("cache_control").is_none());
        assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");

        // Only last user message gets cache_control
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert_eq!(
            body["messages"][2]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn test_cache_control_patch_preserves_existing_cache_control() {
        let patch = CacheControlPatch;
        let mut body = serde_json::json!({
            "system": [{"type": "text", "text": "Sys", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Hi", "cache_control": {"type": "ephemeral"}}]}]
        });
        let ctx = RequestContext {
            upstream_base_url: "https://api.anthropic.com/v1",
        };
        patch.patch_json("messages", &mut body, &ctx).unwrap();

        // Should not double-add
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn test_cache_control_patch_chat_completions_system_message() {
        let patch = CacheControlPatch;
        let mut body = serde_json::json!({
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hi"}
            ]
        });
        let ctx = RequestContext {
            upstream_base_url: "https://api.anthropic.com/v1",
        };
        patch
            .patch_json("chat/completions", &mut body, &ctx)
            .unwrap();

        // System message content converted to block with cache_control
        let sys_content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(sys_content[0]["cache_control"]["type"], "ephemeral");

        // Last user message also gets cache_control
        let user_content = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(user_content[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_cache_control_patch_skips_unknown_routes() {
        let patch = CacheControlPatch;
        let mut body = serde_json::json!({"system": "Hello", "messages": []});
        let ctx = RequestContext {
            upstream_base_url: "https://api.anthropic.com/v1",
        };
        patch
            .patch_json("messages/count_tokens", &mut body, &ctx)
            .unwrap();
        assert!(body["system"].is_string());
    }

    #[test]
    fn test_cache_control_chat_completions_multiple_system_messages() {
        let patch = CacheControlPatch;
        let mut body = serde_json::json!({
            "messages": [
                {"role": "system", "content": "First system."},
                {"role": "system", "content": "Second system."},
                {"role": "user", "content": "Hi"}
            ]
        });
        let ctx = RequestContext {
            upstream_base_url: "https://api.anthropic.com/v1",
        };
        patch
            .patch_json("chat/completions", &mut body, &ctx)
            .unwrap();

        // First system should NOT have cache_control
        assert!(
            body["messages"][0]["content"].is_string(),
            "first system message should remain a plain string"
        );
        // Last system message SHOULD have cache_control
        let last_sys = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(last_sys[0]["cache_control"]["type"], "ephemeral");
        // User message should also have cache_control
        let user = body["messages"][2]["content"].as_array().unwrap();
        assert_eq!(user[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn strip_cache_control_recursively_removes_nested_keys() {
        let mut body = serde_json::json!({
            "messages": [
                {"role": "system", "content": [
                    {"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}
                ]},
                {"role": "user", "content": [
                    {"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "trailing"}
                ]}
            ],
            "system": [
                {"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}
            ]
        });
        super::strip_cache_control(&mut body);
        // Top-level system block stripped.
        assert!(body["system"][0].get("cache_control").is_none());
        // Nested message-level blocks stripped.
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert!(
            body["messages"][1]["content"][0]
                .get("cache_control")
                .is_none()
        );
        // Other fields untouched.
        assert_eq!(body["messages"][0]["content"][0]["text"], "sys");
        assert_eq!(body["messages"][1]["content"][1]["text"], "trailing");
    }

    #[test]
    fn test_pipeline_applies_all_patches() {
        let pipeline = RouterPipeline::for_openrouter();
        let ctx = RequestContext {
            upstream_base_url: "https://openrouter.ai/api/v1",
        };
        let mut body = serde_json::json!({"model":"claude-haiku-4-5"});
        let mut headers = HeaderMap::new();

        pipeline.patch_json("messages", &mut body, &ctx).unwrap();
        pipeline
            .patch_headers("messages", &mut headers, &ctx)
            .unwrap();

        assert_eq!(body["model"], "anthropic/claude-haiku-4.5");
        assert_eq!(
            headers
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok()),
            Some("2023-06-01")
        );
    }
}
