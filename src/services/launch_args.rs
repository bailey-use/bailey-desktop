use anyhow::{Context, Result};
use serde_json::json;
use std::collections::HashMap;

use crate::cli_args::context_tag_to_tokens;
use crate::constants::PLACEHOLDER_LOOPBACK_URL;
use crate::services::ai_launcher::AIToolType;
use crate::services::codex_model_map::map_model_for_codex_cli;

pub(crate) struct RuntimeArgs {
    pub(crate) args: Vec<String>,
    pub(crate) codex_model_catalog_path: Option<String>,
}

pub(crate) fn merge_preview_env(
    tool_env: &HashMap<String, String>,
    manual_env: Option<&HashMap<String, String>>,
) -> HashMap<String, String> {
    let mut merged = tool_env.clone();
    if let Some(manual) = manual_env {
        for (key, value) in manual {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

pub(crate) fn preview_args(
    tool: AIToolType,
    raw_args: &[String],
    model: Option<&str>,
    env: &HashMap<String, String>,
) -> Vec<String> {
    let args = inject_claude_teammate_mode(tool, raw_args);
    if tool == AIToolType::Pi {
        return inject_pi_model(model, &args);
    }
    if tool == AIToolType::Amp {
        let args = inject_amp_no_ide(&args, env);
        let args = inject_amp_dangerously_allow_all(&args, env);
        return inject_amp_settings_file(&args, env);
    }
    if tool != AIToolType::Codex {
        return args;
    }

    let use_responses_router = uses_responses_to_chat_router(env);
    let args = inject_codex_model(model, &args, use_responses_router);
    let args = if should_preview_codex_model_catalog(model, use_responses_router) {
        let mut preview = vec![
            "--config".to_string(),
            "model_catalog_json=\"<temp:aivo-codex-model-catalog.json>\"".to_string(),
        ];
        preview.extend(args);
        preview
    } else {
        args
    };
    preview_codex_provider_config_args(env, args)
}

pub(crate) fn build_preview_notes(
    tool: AIToolType,
    raw_args: &[String],
    model: Option<&str>,
    env: &HashMap<String, String>,
) -> Vec<String> {
    let mut notes = Vec::new();

    if tool == AIToolType::Claude
        && !raw_args
            .iter()
            .any(|arg| arg == "--teammate-mode" || arg.starts_with("--teammate-mode="))
    {
        notes.push("injects `--teammate-mode in-process` for Claude".to_string());
    }

    maybe_push_router_note(
        &mut notes,
        env,
        &["AIVO_USE_ROUTER"],
        "starts an Anthropic compatibility router on a random local port",
    );
    maybe_push_router_note(
        &mut notes,
        env,
        &["AIVO_USE_ANTHROPIC_TO_OPENAI_ROUTER"],
        "starts an Anthropic-to-OpenAI compatibility router on a random local port",
    );
    maybe_push_router_note(
        &mut notes,
        env,
        &["AIVO_USE_COPILOT_ROUTER"],
        "starts a Copilot router on a random local port",
    );
    maybe_push_router_note(
        &mut notes,
        env,
        &["AIVO_USE_RESPONSES_TO_CHAT_ROUTER"],
        "starts a Responses-to-Chat router on a random local port",
    );
    maybe_push_router_note(
        &mut notes,
        env,
        &["AIVO_USE_RESPONSES_TO_CHAT_COPILOT_ROUTER"],
        "starts a Copilot-backed Responses-to-Chat router on a random local port",
    );
    maybe_push_router_note(
        &mut notes,
        env,
        &["AIVO_USE_GEMINI_ROUTER"],
        "starts a Gemini router on a random local port",
    );
    maybe_push_router_note(
        &mut notes,
        env,
        &["AIVO_USE_GEMINI_COPILOT_ROUTER"],
        "starts a Copilot-backed Gemini router on a random local port",
    );
    maybe_push_router_note(
        &mut notes,
        env,
        &["AIVO_USE_OPENCODE_ROUTER"],
        "starts an OpenCode compatibility router on a random local port",
    );
    maybe_push_router_note(
        &mut notes,
        env,
        &["AIVO_USE_OPENCODE_COPILOT_ROUTER"],
        "starts a Copilot-backed OpenCode router on a random local port",
    );
    maybe_push_router_note(
        &mut notes,
        env,
        &["AIVO_SETUP_PI_AGENT_DIR"],
        "writes a temporary Pi agent dir with custom provider config",
    );
    maybe_push_router_note(
        &mut notes,
        env,
        &["AIVO_USE_PI_COPILOT_ROUTER"],
        "starts a Copilot-backed Pi router on a random local port",
    );

    let use_responses_router = uses_responses_to_chat_router(env);
    if tool == AIToolType::Codex
        && model.is_some()
        && !raw_args.iter().any(|arg| {
            arg == "--model" || arg == "-m" || arg.starts_with("--model=") || arg.starts_with("-m=")
        })
    {
        notes.push("injects `-m <model>` for Codex".to_string());
    }
    if tool == AIToolType::Codex && should_preview_codex_model_catalog(model, use_responses_router)
    {
        notes.push("writes a temporary Codex model catalog file at launch time".to_string());
    }
    if tool == AIToolType::Codex && env.contains_key("OPENAI_BASE_URL") {
        notes.push("injects `--config model_provider=aivo` to bypass codex auth.json".to_string());
    }

    if tool == AIToolType::Pi
        && model.is_some()
        && !raw_args
            .iter()
            .any(|arg| arg == "--model" || arg.starts_with("--model="))
    {
        notes.push("injects `--model <model>` for Pi".to_string());
    }

    if tool == AIToolType::Amp && env.contains_key("AIVO_USE_AMP_BRIDGE") {
        notes.push(
            "starts an Amp bridge on a random local port — stubs the management plane locally \
             (auth/threads/telemetry) and translates LLM calls to the upstream"
                .to_string(),
        );
        if !raw_args.iter().any(|a| a == "--ide" || a == "--no-ide") {
            notes.push(
                "injects `--no-ide` so amp doesn't auto-prepend open IDE file/selection to \
                 messages going to the rerouted upstream (pass `--ide` to opt back in)"
                    .to_string(),
            );
        }
        if amp_runs_non_interactively(raw_args)
            && !raw_args.iter().any(|a| a == "--dangerously-allow-all")
        {
            notes.push(
                "injects `--dangerously-allow-all` because amp is in a non-interactive mode \
                 (`-x` / `--stream-json-input`); without it, tool-approval prompts would hang \
                 the run with no human to answer them"
                    .to_string(),
            );
        }
    }
    if tool == AIToolType::Amp
        && (env.contains_key("AIVO_AMP_INTERNAL_MODEL")
            || env.contains_key("AIVO_AMP_INTERNAL_MODEL_JSON"))
    {
        notes.push(
            "writes a temporary amp settings.json (merged from your ~/.config/amp/settings.json) \
             with the requested `internal.model` override and passes it via `--settings-file`"
                .to_string(),
        );
    }

    notes
}

pub(crate) async fn build_runtime_args(
    tool: AIToolType,
    raw_args: &[String],
    model: Option<&str>,
    env: &HashMap<String, String>,
) -> Result<RuntimeArgs> {
    let args = inject_claude_teammate_mode(tool, raw_args);
    if tool == AIToolType::Pi {
        return Ok(RuntimeArgs {
            args: inject_pi_model(model, &args),
            codex_model_catalog_path: None,
        });
    }
    if tool == AIToolType::Amp {
        let args = inject_amp_no_ide(&args, env);
        let args = inject_amp_dangerously_allow_all(&args, env);
        return Ok(RuntimeArgs {
            args: inject_amp_settings_file(&args, env),
            codex_model_catalog_path: None,
        });
    }
    if tool != AIToolType::Codex {
        return Ok(RuntimeArgs {
            args,
            codex_model_catalog_path: None,
        });
    }

    let use_responses_router = uses_responses_to_chat_router(env);
    let codex_model_catalog_path =
        maybe_write_codex_model_catalog(model, use_responses_router).await?;
    let args = inject_codex_model(model, &args, use_responses_router);
    let args = inject_codex_model_catalog(codex_model_catalog_path.as_deref(), &args);

    Ok(RuntimeArgs {
        args,
        codex_model_catalog_path,
    })
}

fn uses_responses_to_chat_router(env: &HashMap<String, String>) -> bool {
    env.contains_key("AIVO_USE_RESPONSES_TO_CHAT_ROUTER")
        || env.contains_key("AIVO_USE_RESPONSES_TO_CHAT_COPILOT_ROUTER")
}

/// Converts Codex `OPENAI_BASE_URL` + `OPENAI_API_KEY` env vars into
/// `--config model_provider` CLI flags so codex uses a custom provider
/// named "aivo" instead of its built-in auth flow.
///
/// Bypasses `~/.codex/auth.json` and avoids the deprecated `OPENAI_BASE_URL`
/// env var warning. Must be called after `prepare_runtime_env` (placeholder
/// URLs resolved) and before `spawn_child`.
pub(crate) fn inject_codex_provider_config(
    env: &mut HashMap<String, String>,
    args: &mut Vec<String>,
) {
    if args.iter().any(|a| a.contains("model_provider")) {
        return;
    }
    let base_url = match env.remove("OPENAI_BASE_URL") {
        Some(url) => url,
        None => return,
    };
    let api_key = match env.remove("OPENAI_API_KEY") {
        Some(key) => key,
        None => {
            env.insert("OPENAI_BASE_URL".to_string(), base_url);
            return;
        }
    };

    env.insert("AIVO_CODEX_API_KEY".to_string(), api_key);

    let escaped_url = base_url.replace('\\', "\\\\").replace('"', "\\\"");
    let mut config_args = vec![
        "--config".to_string(),
        "model_provider=\"aivo\"".to_string(),
        "--config".to_string(),
        "model_providers.aivo.name=\"aivo\"".to_string(),
        "--config".to_string(),
        format!("model_providers.aivo.base_url=\"{}\"", escaped_url),
        "--config".to_string(),
        "model_providers.aivo.env_key=\"AIVO_CODEX_API_KEY\"".to_string(),
    ];
    // Disable the built-in `codex_apps` MCP (OpenAI Connectors registry).
    // When aivo is routing codex to a non-OpenAI provider, the user is not
    // authed with ChatGPT, so codex_apps can't do anything useful — but it
    // still tries to fetch chatgpt.com/backend-api/connectors/directory on
    // startup, which costs 10s of wall-clock time and fails outright
    // without VPN. Disabling removes that tax; users who need apps should
    // run `codex` directly rather than going through aivo.
    if !args.iter().any(|a| a == "apps" || a == "connectors")
        && !args
            .windows(2)
            .any(|w| (w[0] == "--disable" || w[0] == "--enable") && w[1] == "apps")
    {
        config_args.push("--disable".to_string());
        config_args.push("apps".to_string());
    }
    config_args.append(args);
    *args = config_args;
}

/// Append `--config model_context_window=<tokens>` for codex when the user
/// asked for `--max-context=<N>m`. Codex clamps the value against the
/// model's advertised ceiling internally, so passing a high value on a
/// small model is silently a no-op rather than an error. We append (not
/// prepend) so the user's own `--config` flags, if any, parse first and
/// can win on conflict per codex's last-write-wins semantics.
pub(crate) fn inject_codex_max_context(args: &mut Vec<String>, max_context: Option<&str>) {
    let Some(tag) = max_context else {
        return;
    };
    let Some(tokens) = context_tag_to_tokens(tag) else {
        return;
    };
    args.push("--config".to_string());
    args.push(format!("model_context_window={tokens}"));
}

/// Rewrites env vars for the dry-run preview so it reflects what codex
/// will actually receive at runtime.
pub(crate) fn rewrite_codex_preview_env(env: &mut HashMap<String, String>) {
    if let Some(api_key) = env.remove("OPENAI_API_KEY") {
        env.insert("AIVO_CODEX_API_KEY".to_string(), api_key);
    }
    env.remove("OPENAI_BASE_URL");
}

/// Rewrites env vars for the dry-run preview so it reflects what amp will
/// actually see at runtime: `AMP_URL` and `AMP_API_KEY` are set by
/// `start_amp_bridge` after binding the bridge port, so they don't show up
/// in the env produced by `for_amp`. The preview adds placeholders here
/// (`http://127.0.0.1:<port>`, `aivo-bridge`) so the user can see at a
/// glance that amp will talk to a localhost bridge — not directly to
/// `AIVO_AMP_UPSTREAM_BASE_URL` like the bare env might suggest.
pub(crate) fn rewrite_amp_preview_env(env: &mut HashMap<String, String>) {
    if env.contains_key("AIVO_USE_AMP_BRIDGE") {
        env.insert("AMP_URL".to_string(), "http://127.0.0.1:<port>".to_string());
        env.insert("AMP_API_KEY".to_string(), "aivo-bridge".to_string());
    }
}

/// Preview-only: prepends model_provider `--config` flags for Codex args
/// without mutating the env map.
fn preview_codex_provider_config_args(
    env: &HashMap<String, String>,
    args: Vec<String>,
) -> Vec<String> {
    let base_url = match env.get("OPENAI_BASE_URL") {
        Some(url) => url.as_str(),
        None => return args,
    };

    let display_url = if base_url == PLACEHOLDER_LOOPBACK_URL {
        "http://127.0.0.1:<port>"
    } else {
        base_url
    };

    let mut prefix = vec![
        "--config".to_string(),
        "model_provider=\"aivo\"".to_string(),
        "--config".to_string(),
        "model_providers.aivo.name=\"aivo\"".to_string(),
        "--config".to_string(),
        format!("model_providers.aivo.base_url=\"{}\"", display_url),
        "--config".to_string(),
        "model_providers.aivo.env_key=\"AIVO_CODEX_API_KEY\"".to_string(),
    ];
    // Mirror the runtime behavior of inject_codex_provider_config: disable
    // the codex_apps MCP to avoid a startup call to chatgpt.com that would
    // hang without VPN and yield nothing useful under aivo's routing.
    if !args
        .windows(2)
        .any(|w| (w[0] == "--disable" || w[0] == "--enable") && w[1] == "apps")
    {
        prefix.push("--disable".to_string());
        prefix.push("apps".to_string());
    }
    prefix.extend(args);
    prefix
}

fn maybe_push_router_note(
    notes: &mut Vec<String>,
    env: &HashMap<String, String>,
    env_keys: &[&str],
    note: &str,
) {
    if env_keys.iter().any(|key| env.contains_key(*key)) {
        notes.push(note.to_string());
    }
}

fn should_preview_codex_model_catalog(model: Option<&str>, uses_non_openai_router: bool) -> bool {
    let model = match model {
        Some(model) if !model.is_empty() => model,
        _ => return false,
    };

    if !uses_non_openai_router {
        return false;
    }

    let model_lower = model.to_lowercase();
    let name_only = model_lower.split('/').next_back().unwrap_or(&model_lower);
    !(name_only.starts_with("gpt-")
        || name_only.starts_with("o1")
        || name_only.starts_with("o3")
        || name_only.starts_with("o4"))
}

fn inject_claude_teammate_mode(tool: AIToolType, args: &[String]) -> Vec<String> {
    if tool != AIToolType::Claude {
        return args.to_vec();
    }

    let has_teammate_mode = args
        .iter()
        .any(|a| a == "--teammate-mode" || a.starts_with("--teammate-mode="));
    if has_teammate_mode {
        return args.to_vec();
    }

    let mut new_args = vec!["--teammate-mode".to_string(), "in-process".to_string()];
    new_args.extend_from_slice(args);
    new_args
}

/// Prepends `--settings-file <path>` to amp's args when the bridge will
/// write a merged settings override. Triggered by any of: `--1m`, the
/// per-mode `--rush-model / --smart-model / --deep-model / --large-model`
/// flags, `--disable-tool`, or — in bridge mode — the always-on
/// auto-disable of unsupported tools (web_search/read_web_page/Task).
/// At runtime, the real path is in `AIVO_AMP_SETTINGS_FILE`; for dry-run
/// preview we substitute a `<temp:aivo-amp-settings.json>` placeholder
/// since the path is only known after `start_amp_bridge` runs. Skips if
/// the user already passed `--settings-file` themselves.
fn inject_amp_settings_file(args: &[String], env: &HashMap<String, String>) -> Vec<String> {
    let path = if let Some(p) = env.get("AIVO_AMP_SETTINGS_FILE") {
        p.clone()
    } else if env.contains_key("AIVO_AMP_INTERNAL_MODEL")
        || env.contains_key("AIVO_AMP_INTERNAL_MODEL_JSON")
        || env.contains_key("AIVO_AMP_TOOLS_DISABLE")
    {
        "<temp:aivo-amp-settings.json>".to_string()
    } else {
        return args.to_vec();
    };
    let already_set = args
        .iter()
        .any(|a| a == "--settings-file" || a.starts_with("--settings-file="));
    if already_set {
        return args.to_vec();
    }
    let mut new_args = vec!["--settings-file".to_string(), path];
    new_args.extend_from_slice(args);
    new_args
}

/// Prepends `--no-ide` to amp's args when the bridge is active. Amp's
/// IDE integration (default on) auto-prepends the open IDE file's path
/// and current text selection to every user message — useful when amp
/// is talking to ampcode.com, but a privacy leak when the bridge is
/// rerouting traffic to a third-party upstream (deepseek/openrouter/etc.).
/// Native-amp launches (`AIVO_USE_AMP_BRIDGE` unset) keep the default since
/// the user's data only goes back to Sourcegraph in that case.
///
/// Skipped if the user already passed `--ide` or `--no-ide` themselves —
/// explicit choice wins.
fn inject_amp_no_ide(args: &[String], env: &HashMap<String, String>) -> Vec<String> {
    if !env.contains_key("AIVO_USE_AMP_BRIDGE") {
        return args.to_vec();
    }
    let already_set = args.iter().any(|a| a == "--ide" || a == "--no-ide");
    if already_set {
        return args.to_vec();
    }
    let mut new_args = vec!["--no-ide".to_string()];
    new_args.extend_from_slice(args);
    new_args
}

/// True when amp's args put it in a non-interactive mode that can't surface
/// tool-approval prompts to a human:
/// - `-x` / `--execute "<prompt>"` — one-shot execution
/// - `--stream-json-input` — programmatic JSON-over-stdin
fn amp_runs_non_interactively(args: &[String]) -> bool {
    args.iter().any(|a| {
        a == "-x" || a == "--execute" || a.starts_with("--execute=") || a == "--stream-json-input"
    })
}

/// Prepends `--dangerously-allow-all` to amp's args when the bridge is
/// active AND amp is in a non-interactive mode (`-x`/`--execute`/
/// `--stream-json-input`). Without this, amp blocks on every tool-approval
/// prompt and the one-shot/programmatic call hangs forever — there's no
/// human at the other end to press a key.
///
/// Skipped if the user already passed the flag explicitly, and skipped
/// for native-amp launches (no bridge) so we don't widen permissions on
/// runs talking to ampcode.com itself.
fn inject_amp_dangerously_allow_all(args: &[String], env: &HashMap<String, String>) -> Vec<String> {
    if !env.contains_key("AIVO_USE_AMP_BRIDGE") {
        return args.to_vec();
    }
    if !amp_runs_non_interactively(args) {
        return args.to_vec();
    }
    let already_set = args.iter().any(|a| a == "--dangerously-allow-all");
    if already_set {
        return args.to_vec();
    }
    let mut new_args = vec!["--dangerously-allow-all".to_string()];
    new_args.extend_from_slice(args);
    new_args
}

fn inject_pi_model(model: Option<&str>, args: &[String]) -> Vec<String> {
    let model = match model {
        Some(m) if !m.is_empty() => m,
        _ => return args.to_vec(),
    };

    let has_model_flag = args
        .iter()
        .any(|a| a == "--model" || a.starts_with("--model="));
    if has_model_flag {
        return args.to_vec();
    }

    // Always prefix model with "aivo/" so pi selects
    // the custom provider from models.json.
    let pi_model = format!("aivo/{model}");

    let mut new_args = vec!["--model".to_string(), pi_model];
    new_args.extend_from_slice(args);
    new_args
}

fn inject_codex_model(model: Option<&str>, args: &[String], use_router: bool) -> Vec<String> {
    let model = match model {
        Some(m) if !m.is_empty() => m,
        _ => return args.to_vec(),
    };

    let has_model_flag = args
        .iter()
        .any(|a| a == "--model" || a == "-m" || a.starts_with("--model=") || a.starts_with("-m="));
    if has_model_flag {
        return args.to_vec();
    }

    let codex_model = if use_router {
        model.to_string()
    } else {
        map_model_for_codex_cli(model)
    };
    let mut new_args = vec!["-m".to_string(), codex_model];
    new_args.extend_from_slice(args);
    new_args
}

fn inject_codex_model_catalog(path: Option<&str>, args: &[String]) -> Vec<String> {
    let path = match path {
        Some(p) if !p.is_empty() => p,
        _ => return args.to_vec(),
    };

    if args.iter().any(|a| a.contains("model_catalog_json")) {
        return args.to_vec();
    }

    let escaped_path = path.replace('\\', "\\\\").replace('"', "\\\"");
    let mut new_args = vec![
        "--config".to_string(),
        format!("model_catalog_json=\"{}\"", escaped_path),
    ];
    new_args.extend_from_slice(args);
    new_args
}

async fn maybe_write_codex_model_catalog(
    model: Option<&str>,
    uses_non_openai_router: bool,
) -> Result<Option<String>> {
    let model = match model {
        Some(m) if !m.is_empty() => m,
        _ => return Ok(None),
    };

    if !uses_non_openai_router {
        return Ok(None);
    }

    let model_lower = model.to_lowercase();
    let name_only = model_lower.split('/').next_back().unwrap_or(&model_lower);
    if name_only.starts_with("gpt-")
        || name_only.starts_with("o1")
        || name_only.starts_with("o3")
        || name_only.starts_with("o4")
    {
        return Ok(None);
    }

    let catalog_json = build_codex_model_catalog_json(model)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = format!(
        "aivo-codex-model-catalog-{}-{}.json",
        std::process::id(),
        nonce
    );
    let path = std::env::temp_dir().join(file_name);

    tokio::fs::write(&path, catalog_json)
        .await
        .with_context(|| {
            format!(
                "Failed to write Codex model catalog override at {}",
                path.display()
            )
        })?;

    Ok(Some(path.to_string_lossy().to_string()))
}

fn build_codex_model_catalog_json(model: &str) -> Result<String> {
    let catalog = json!({
        "models": [{
            "slug": model,
            "display_name": model,
            "description": format!("Custom model metadata for {}", model),
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [
                {"effort": "low", "description": "low"},
                {"effort": "medium", "description": "medium"}
            ],
            "shell_type": "shell_command",
            "visibility": "list",
            "minimal_client_version": [0, 1, 0],
            "supported_in_api": true,
            "priority": 0,
            "upgrade": serde_json::Value::Null,
            "base_instructions": "base instructions",
            "supports_reasoning_summaries": false,
            "support_verbosity": false,
            "default_verbosity": serde_json::Value::Null,
            "apply_patch_tool_type": serde_json::Value::Null,
            "truncation_policy": {"mode": "bytes", "limit": 10000},
            "supports_parallel_tool_calls": false,
            "supports_image_detail_original": false,
            "context_window": 272000,
            "experimental_supported_tools": []
        }]
    });
    Ok(serde_json::to_string(&catalog)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inject_claude_teammate_mode_for_claude() {
        let args = vec!["--verbose".to_string(), "prompt".to_string()];
        let result = inject_claude_teammate_mode(AIToolType::Claude, &args);
        assert_eq!(
            result,
            vec!["--teammate-mode", "in-process", "--verbose", "prompt"]
        );
    }

    #[test]
    fn test_inject_claude_teammate_mode_skips_non_claude() {
        let args = vec!["--verbose".to_string()];
        let result = inject_claude_teammate_mode(AIToolType::Codex, &args);
        assert_eq!(result, vec!["--verbose"]);

        let result = inject_claude_teammate_mode(AIToolType::Gemini, &args);
        assert_eq!(result, vec!["--verbose"]);

        let result = inject_claude_teammate_mode(AIToolType::Opencode, &args);
        assert_eq!(result, vec!["--verbose"]);
    }

    #[test]
    fn test_inject_claude_teammate_mode_respects_user_flag() {
        let args = vec![
            "--teammate-mode".to_string(),
            "split".to_string(),
            "prompt".to_string(),
        ];
        let result = inject_claude_teammate_mode(AIToolType::Claude, &args);
        assert_eq!(result, vec!["--teammate-mode", "split", "prompt"]);

        let args = vec!["--teammate-mode=split".to_string(), "prompt".to_string()];
        let result = inject_claude_teammate_mode(AIToolType::Claude, &args);
        assert_eq!(result, vec!["--teammate-mode=split", "prompt"]);
    }

    #[test]
    fn test_inject_claude_teammate_mode_empty_args() {
        let args: Vec<String> = vec![];
        let result = inject_claude_teammate_mode(AIToolType::Claude, &args);
        assert_eq!(result, vec!["--teammate-mode", "in-process"]);
    }

    #[test]
    fn test_inject_codex_model_injects_when_provided() {
        let model = Some("o4-mini");
        let args = vec!["file.ts".to_string()];
        let result = inject_codex_model(model, &args, false);
        assert_eq!(result, vec!["-m", "o4-mini", "file.ts"]);
    }

    #[test]
    fn test_inject_codex_model_router_passes_original() {
        let model = Some("kimi-k2.5");
        let args = vec!["file.ts".to_string()];
        let result = inject_codex_model(model, &args, true);
        assert_eq!(result, vec!["-m", "kimi-k2.5", "file.ts"]);
    }

    #[test]
    fn test_inject_codex_model_router_passes_namespaced() {
        let model = Some("moonshot/kimi-k2.5");
        let args = vec!["file.ts".to_string()];
        let result = inject_codex_model(model, &args, true);
        assert_eq!(result, vec!["-m", "moonshot/kimi-k2.5", "file.ts"]);
    }

    #[test]
    fn test_inject_codex_model_skips_when_already_specified() {
        let model = Some("o4-mini");
        let args = vec![
            "--model".to_string(),
            "gpt-4o".to_string(),
            "file.ts".to_string(),
        ];
        let result = inject_codex_model(model, &args, false);
        assert_eq!(result, vec!["--model", "gpt-4o", "file.ts"]);
    }

    #[test]
    fn test_inject_codex_model_skips_shorthand_flag() {
        let model = Some("o4-mini");
        let args = vec![
            "-m".to_string(),
            "gpt-4o".to_string(),
            "file.ts".to_string(),
        ];
        let result = inject_codex_model(model, &args, false);
        assert_eq!(result, vec!["-m", "gpt-4o", "file.ts"]);
    }

    #[test]
    fn test_inject_codex_model_skips_equals_format() {
        let model = Some("o4-mini");
        let args = vec!["--model=gpt-4o".to_string(), "file.ts".to_string()];
        let result = inject_codex_model(model, &args, false);
        assert_eq!(result, vec!["--model=gpt-4o", "file.ts"]);
    }

    #[test]
    fn test_inject_codex_model_skips_empty_model() {
        let model = Some("");
        let args = vec!["file.ts".to_string()];
        let result = inject_codex_model(model, &args, false);
        assert_eq!(result, vec!["file.ts"]);
    }

    #[test]
    fn test_inject_codex_model_skips_none_model() {
        let model: Option<&str> = None;
        let args = vec!["file.ts".to_string()];
        let result = inject_codex_model(model, &args, false);
        assert_eq!(result, vec!["file.ts"]);
    }

    #[test]
    fn test_inject_codex_model_catalog_injects_when_path_provided() {
        let args = vec!["file.ts".to_string()];
        let result = inject_codex_model_catalog(Some("/tmp/catalog.json"), &args);
        assert_eq!(
            result,
            vec![
                "--config",
                "model_catalog_json=\"/tmp/catalog.json\"",
                "file.ts"
            ]
        );
    }

    #[test]
    fn test_inject_codex_model_catalog_skips_when_existing_setting_present() {
        let args = vec![
            "--config".to_string(),
            "model_catalog_json=\"/tmp/custom.json\"".to_string(),
            "file.ts".to_string(),
        ];
        let result = inject_codex_model_catalog(Some("/tmp/catalog.json"), &args);
        assert_eq!(result, args);
    }

    #[test]
    fn test_build_codex_model_catalog_json_includes_model_slug() {
        let model = "minimax/minimax-m2.5";
        let json = build_codex_model_catalog_json(model).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["models"][0]["slug"], model);
        assert_eq!(parsed["models"][0]["display_name"], model);
    }

    #[test]
    fn claude_prompt_after_teammate_mode() {
        let args = vec!["fix the login bug".to_string()];
        let result = inject_claude_teammate_mode(AIToolType::Claude, &args);
        assert_eq!(
            result,
            vec!["--teammate-mode", "in-process", "fix the login bug"]
        );
    }

    #[test]
    fn codex_prompt_after_model_flag() {
        let args = vec!["refactor this function".to_string()];
        let result = inject_codex_model(Some("gpt-4o"), &args, false);
        assert_eq!(result, vec!["-m", "gpt-4o", "refactor this function"]);
    }

    #[test]
    fn pi_prompt_after_model_flag() {
        let args = vec!["explain this code".to_string()];
        let result = inject_pi_model(Some("gpt-4o"), &args);
        assert_eq!(result, vec!["--model", "aivo/gpt-4o", "explain this code"]);
    }

    #[test]
    fn gemini_prompt_passes_through() {
        let args = vec!["explain this code".to_string()];
        let result = inject_claude_teammate_mode(AIToolType::Gemini, &args);
        assert_eq!(result, vec!["explain this code"]);
    }

    #[tokio::test]
    async fn opencode_prompt_passes_through_build_runtime_args() {
        let args = vec!["explain this code".to_string()];
        let env = HashMap::new();
        let result = build_runtime_args(AIToolType::Opencode, &args, None, &env)
            .await
            .unwrap();
        assert_eq!(result.args, vec!["explain this code"]);
    }

    #[test]
    fn inject_codex_max_context_appends_config_arg() {
        let mut args = vec!["-m".to_string(), "gpt-5".to_string()];
        inject_codex_max_context(&mut args, Some("1m"));
        assert_eq!(
            args,
            vec!["-m", "gpt-5", "--config", "model_context_window=1000000"]
        );
    }

    #[test]
    fn inject_codex_max_context_handles_multi_digit_tags() {
        let mut args: Vec<String> = vec![];
        inject_codex_max_context(&mut args, Some("12m"));
        assert_eq!(args, vec!["--config", "model_context_window=12000000"]);
    }

    #[test]
    fn inject_codex_max_context_noop_when_unset() {
        let mut args = vec!["existing".to_string()];
        inject_codex_max_context(&mut args, None);
        assert_eq!(args, vec!["existing"]);
    }

    #[test]
    fn inject_codex_max_context_noop_on_malformed_tag() {
        // Defensive: callers should pass canonical `<N>m`, but if junk slips
        // through (e.g. a future code path forgets to validate), we silently
        // skip rather than appending a garbage `--config` value.
        let mut args = vec!["existing".to_string()];
        inject_codex_max_context(&mut args, Some("foo"));
        assert_eq!(args, vec!["existing"]);
    }

    #[test]
    fn test_inject_codex_provider_config_direct_openai() {
        let mut env = HashMap::from([
            ("OPENAI_BASE_URL".into(), "https://api.openai.com/v1".into()),
            ("OPENAI_API_KEY".into(), "sk-test-key".into()),
        ]);
        let mut args = vec!["-m".into(), "o4-mini".into()];
        inject_codex_provider_config(&mut env, &mut args);

        assert!(!env.contains_key("OPENAI_BASE_URL"));
        assert!(!env.contains_key("OPENAI_API_KEY"));
        assert_eq!(env.get("AIVO_CODEX_API_KEY").unwrap(), "sk-test-key");
        assert_eq!(
            args,
            vec![
                "--config",
                "model_provider=\"aivo\"",
                "--config",
                "model_providers.aivo.name=\"aivo\"",
                "--config",
                "model_providers.aivo.base_url=\"https://api.openai.com/v1\"",
                "--config",
                "model_providers.aivo.env_key=\"AIVO_CODEX_API_KEY\"",
                "--disable",
                "apps",
                "-m",
                "o4-mini",
            ]
        );
    }

    #[test]
    fn test_inject_codex_provider_config_local_router() {
        let mut env = HashMap::from([
            ("OPENAI_BASE_URL".into(), "http://127.0.0.1:54321".into()),
            ("OPENAI_API_KEY".into(), "provider-key".into()),
        ]);
        let mut args = vec!["-m".into(), "claude-sonnet-4-6".into()];
        inject_codex_provider_config(&mut env, &mut args);

        assert_eq!(env.get("AIVO_CODEX_API_KEY").unwrap(), "provider-key");
        assert!(args[5].contains("http://127.0.0.1:54321"));
    }

    #[test]
    fn test_inject_codex_provider_config_ollama() {
        let mut env = HashMap::from([
            ("OPENAI_BASE_URL".into(), "http://127.0.0.1:12345".into()),
            ("OPENAI_API_KEY".into(), "ollama".into()),
        ]);
        let mut args = vec![];
        inject_codex_provider_config(&mut env, &mut args);

        assert_eq!(env.get("AIVO_CODEX_API_KEY").unwrap(), "ollama");
        assert!(args.contains(&"model_provider=\"aivo\"".to_string()));
    }

    #[test]
    fn test_inject_codex_provider_config_noop_without_base_url() {
        let mut env = HashMap::from([("OPENAI_API_KEY".into(), "sk-key".into())]);
        let mut args = vec!["prompt".into()];
        inject_codex_provider_config(&mut env, &mut args);

        assert_eq!(env.get("OPENAI_API_KEY").unwrap(), "sk-key");
        assert_eq!(args, vec!["prompt"]);
    }

    #[test]
    fn test_inject_codex_provider_config_noop_without_api_key() {
        let mut env =
            HashMap::from([("OPENAI_BASE_URL".into(), "https://api.openai.com/v1".into())]);
        let mut args = vec!["prompt".into()];
        inject_codex_provider_config(&mut env, &mut args);

        // base_url should be restored
        assert!(env.contains_key("OPENAI_BASE_URL"));
        assert_eq!(args, vec!["prompt"]);
    }

    #[test]
    fn test_inject_codex_provider_config_skips_if_model_provider_in_args() {
        let mut env = HashMap::from([
            ("OPENAI_BASE_URL".into(), "https://api.openai.com/v1".into()),
            ("OPENAI_API_KEY".into(), "sk-key".into()),
        ]);
        let mut args = vec![
            "--config".into(),
            "model_provider=\"custom\"".into(),
            "-m".into(),
            "gpt-4o".into(),
        ];
        inject_codex_provider_config(&mut env, &mut args);

        // Should not modify anything
        assert!(env.contains_key("OPENAI_BASE_URL"));
        assert!(env.contains_key("OPENAI_API_KEY"));
        assert!(!env.contains_key("AIVO_CODEX_API_KEY"));
    }

    #[test]
    fn test_inject_codex_provider_config_preserves_existing_args() {
        let mut env = HashMap::from([
            ("OPENAI_BASE_URL".into(), "https://api.openai.com/v1".into()),
            ("OPENAI_API_KEY".into(), "sk-key".into()),
        ]);
        let mut args = vec![
            "--config".into(),
            "model_catalog_json=\"/tmp/cat.json\"".into(),
            "-m".into(),
            "gpt-4o".into(),
            "fix bug".into(),
        ];
        inject_codex_provider_config(&mut env, &mut args);

        // Config flags + --disable apps prepended, original args at the end
        assert_eq!(args[8], "--disable");
        assert_eq!(args[9], "apps");
        assert_eq!(args[10], "--config");
        assert_eq!(args[11], "model_catalog_json=\"/tmp/cat.json\"");
        assert_eq!(args[12], "-m");
        assert_eq!(args[13], "gpt-4o");
        assert_eq!(args[14], "fix bug");
    }

    #[test]
    fn test_rewrite_codex_preview_env() {
        let mut env = HashMap::from([
            ("OPENAI_BASE_URL".into(), "https://api.openai.com/v1".into()),
            ("OPENAI_API_KEY".into(), "sk-key".into()),
            ("CODEX_MODEL".into(), "gpt-4o".into()),
        ]);
        rewrite_codex_preview_env(&mut env);

        assert!(!env.contains_key("OPENAI_BASE_URL"));
        assert!(!env.contains_key("OPENAI_API_KEY"));
        assert_eq!(env.get("AIVO_CODEX_API_KEY").unwrap(), "sk-key");
        assert_eq!(env.get("CODEX_MODEL").unwrap(), "gpt-4o");
    }

    #[test]
    fn test_preview_codex_provider_config_args_with_base_url() {
        let env = HashMap::from([("OPENAI_BASE_URL".into(), "https://api.openai.com/v1".into())]);
        let args = vec!["-m".into(), "gpt-4o".into()];
        let result = preview_codex_provider_config_args(&env, args);

        assert_eq!(result[0], "--config");
        assert_eq!(result[1], "model_provider=\"aivo\"");
        assert!(result[5].contains("https://api.openai.com/v1"));
        assert_eq!(result[8], "--disable");
        assert_eq!(result[9], "apps");
        assert_eq!(result[10], "-m");
        assert_eq!(result[11], "gpt-4o");
    }

    #[test]
    fn test_preview_codex_provider_config_args_placeholder_url() {
        let env = HashMap::from([("OPENAI_BASE_URL".into(), PLACEHOLDER_LOOPBACK_URL.into())]);
        let args = vec!["-m".into(), "model".into()];
        let result = preview_codex_provider_config_args(&env, args);

        assert!(result[5].contains("http://127.0.0.1:<port>"));
    }

    #[test]
    fn test_preview_codex_provider_config_args_noop_without_base_url() {
        let env = HashMap::new();
        let args = vec!["-m".into(), "gpt-4o".into()];
        let result = preview_codex_provider_config_args(&env, args);

        assert_eq!(result, vec!["-m", "gpt-4o"]);
    }

    #[test]
    fn test_inject_amp_no_ide_prepends_when_bridge_active() {
        // Bridge active + user didn't pick a side → prepend `--no-ide` so
        // amp doesn't auto-prefix open-IDE file content to messages going
        // through the bridge to a third-party upstream.
        let env = HashMap::from([("AIVO_USE_AMP_BRIDGE".into(), "1".into())]);
        let args = vec!["--mode".into(), "smart".into()];
        let result = inject_amp_no_ide(&args, &env);
        assert_eq!(result, vec!["--no-ide", "--mode", "smart"]);
    }

    #[test]
    fn test_inject_amp_no_ide_skips_for_native_amp() {
        // Native amp (no bridge) → user's data only goes back to
        // Sourcegraph, no leak risk. Leave `--ide` behavior at amp's
        // default rather than silently disabling a useful feature.
        let env = HashMap::new();
        let args = vec!["thread".into(), "list".into()];
        let result = inject_amp_no_ide(&args, &env);
        assert_eq!(result, vec!["thread", "list"]);
    }

    #[test]
    fn test_inject_amp_dangerously_allow_all_fires_for_one_shot_under_bridge() {
        // Bridge active + amp invoked non-interactively (`-x "..."`) → there
        // is no human to answer tool-approval prompts, so prepend the flag
        // so amp actually completes instead of hanging.
        let env = HashMap::from([("AIVO_USE_AMP_BRIDGE".into(), "1".into())]);
        let args = vec!["-x".into(), "fix the failing test".into()];
        let result = inject_amp_dangerously_allow_all(&args, &env);
        assert_eq!(
            result,
            vec!["--dangerously-allow-all", "-x", "fix the failing test"]
        );
    }

    #[test]
    fn test_inject_amp_dangerously_allow_all_fires_for_stream_json_input() {
        // `--stream-json-input` is amp's programmatic path — same story as
        // `-x`: no human in the loop, so auto-allow.
        let env = HashMap::from([("AIVO_USE_AMP_BRIDGE".into(), "1".into())]);
        let args = vec!["--stream-json-input".into()];
        let result = inject_amp_dangerously_allow_all(&args, &env);
        assert!(
            result
                .first()
                .is_some_and(|a| a == "--dangerously-allow-all")
        );
    }

    #[test]
    fn test_inject_amp_dangerously_allow_all_skips_interactive() {
        // No `-x` / `--execute` / `--stream-json-input` → user is interactive
        // and CAN answer tool prompts. Don't widen permissions silently.
        let env = HashMap::from([("AIVO_USE_AMP_BRIDGE".into(), "1".into())]);
        let args = vec!["--mode".into(), "smart".into()];
        let result = inject_amp_dangerously_allow_all(&args, &env);
        assert_eq!(result, vec!["--mode", "smart"]);
    }

    #[test]
    fn test_inject_amp_dangerously_allow_all_skips_native_amp() {
        // No bridge → user is talking directly to ampcode.com; they own the
        // permissions story, don't auto-widen.
        let env = HashMap::new();
        let args = vec!["-x".into(), "ship it".into()];
        let result = inject_amp_dangerously_allow_all(&args, &env);
        assert_eq!(result, vec!["-x", "ship it"]);
    }

    #[test]
    fn test_inject_amp_dangerously_allow_all_idempotent() {
        // User already passed the flag → don't double-inject.
        let env = HashMap::from([("AIVO_USE_AMP_BRIDGE".into(), "1".into())]);
        let args = vec!["--dangerously-allow-all".into(), "-x".into(), "go".into()];
        let result = inject_amp_dangerously_allow_all(&args, &env);
        assert_eq!(result, args);
        assert_eq!(
            result
                .iter()
                .filter(|a| *a == "--dangerously-allow-all")
                .count(),
            1
        );
    }

    #[test]
    fn test_inject_amp_dangerously_allow_all_handles_execute_with_equals() {
        // `--execute=hi` (single token) is the same non-interactive mode as
        // `-x hi` / `--execute hi` — must trigger.
        let env = HashMap::from([("AIVO_USE_AMP_BRIDGE".into(), "1".into())]);
        let args = vec!["--execute=hi".into()];
        let result = inject_amp_dangerously_allow_all(&args, &env);
        assert!(
            result
                .first()
                .is_some_and(|a| a == "--dangerously-allow-all")
        );
    }

    #[test]
    fn test_inject_amp_no_ide_respects_explicit_user_flag() {
        // User passed `--ide` explicitly even with the bridge active —
        // they've made a deliberate choice; don't override.
        let env = HashMap::from([("AIVO_USE_AMP_BRIDGE".into(), "1".into())]);
        let with_ide = vec!["--ide".into(), "prompt".into()];
        assert_eq!(inject_amp_no_ide(&with_ide, &env), with_ide);

        // Same with explicit `--no-ide` — don't double-inject.
        let with_no_ide = vec!["--no-ide".into(), "prompt".into()];
        let result = inject_amp_no_ide(&with_no_ide, &env);
        assert_eq!(result, vec!["--no-ide", "prompt"]);
        assert_eq!(result.iter().filter(|a| *a == "--no-ide").count(), 1);
    }
}
