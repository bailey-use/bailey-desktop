//! `aivo image` — generate images from a text prompt.
//!
//! Mirrors the chat one-shot pattern: resolve key, resolve prompt (from arg or
//! stdin), call the provider, save the result(s). Uses the shared
//! `services::image_gen` module for the actual HTTP + file work.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::json;
use tokio::task::JoinHandle;

use crate::cli::ImageArgs;
use crate::errors::ExitCode;
use crate::services::http_utils::router_http_client;
use crate::services::image_gen::{
    self, ImageArtifact, ImageRequest, OutputTarget, OverwriteDecision, OverwritePolicy,
};
use crate::services::models_cache::ModelsCache;
use crate::services::session_store::{ApiKey, SessionStore};
use crate::style;

pub struct ImageCommand {
    session_store: SessionStore,
    cache: ModelsCache,
}

impl ImageCommand {
    pub fn new(session_store: SessionStore, cache: ModelsCache) -> Self {
        Self {
            session_store,
            cache,
        }
    }

    pub fn print_help() {
        println!(
            "{} {}",
            style::cyan("aivo image"),
            style::dim("— generate images from a prompt")
        );
        println!();
        println!("{} aivo image [OPTIONS] [PROMPT]", style::bold("Usage:"));
        println!();
        println!("{}", style::bold("Arguments:"));
        println!(
            "  {}{}",
            style::cyan(format!("{:<24}", "PROMPT")),
            style::dim("Text prompt, or read from stdin when omitted")
        );
        println!();
        println!("{}", style::bold("Options:"));
        let opt = |f: &str, d: &str| {
            println!("  {}{}", style::cyan(format!("{:<24}", f)), style::dim(d));
        };
        opt(
            "-m, --model <MODEL>",
            "Image model (e.g. gpt-image-1, dall-e-3)",
        );
        opt("-k, --key <ID|NAME>", "API key to use");
        opt(
            "-o, --output <PATH>",
            "File, directory, or template ({ts}/{model})",
        );
        opt("-f, --force", "Overwrite existing files without prompting");
        opt("-s, --size <WxH>", "1024x1024 | 1792x1024 | 1024x1792");
        opt("-q, --quality <LEVEL>", "standard | hd | high | low");
        opt("-r, --refresh", "Bypass model-list cache");
        opt(
            "    --url",
            "Print provider URL only; skip download (URLs may expire)",
        );
        opt("    --json", "Emit JSON result (for scripting)");
        println!();
        println!("{}", style::bold("Examples:"));
        println!("  {}", style::dim("aivo image \"a red panda in space\""));
        println!(
            "  {}",
            style::dim("aivo image \"logo sketch\" -m dall-e-3 -o logo.png")
        );
        println!(
            "  {}",
            style::dim("echo \"a minimalist poster\" | aivo image --json")
        );
    }

    pub async fn execute(self, args: ImageArgs, key: ApiKey) -> ExitCode {
        // Resolve the model BEFORE reading stdin for the prompt. The fuzzy
        // picker reads keystrokes from stdin, so consuming stdin first (for
        // a piped prompt) would leave the picker at EOF. Matches
        // `aivo chat -x` which picks the model before reading stdin.
        let model = match resolve_image_model(&self.session_store, &self.cache, &args, &key).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                // Picker cancelled (ESC) — treat as clean exit, no error.
                return ExitCode::Success;
            }
            Err(e) => {
                eprintln!("{} {}", style::red("Error:"), e);
                return ExitCode::UserError;
            }
        };

        let prompt = match resolve_prompt(args.prompt.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{} {}", style::red("Error:"), e);
                return ExitCode::UserError;
            }
        };

        // Preflight: resolve target path *before* the API call so a slow
        // generation isn't wasted on an unwritable path.
        let target = OutputTarget::parse(args.output.as_deref());
        let ext = default_extension(args.size.as_deref());
        let initial_path = match image_gen::resolve_output_path(&target, &model, &ext) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{} {}", style::red("Error:"), e);
                return ExitCode::UserError;
            }
        };

        // Apply overwrite policy up front so we fail fast on collisions in
        // non-TTY / --json mode.
        let policy = OverwritePolicy::from_flags(args.force, args.json);
        let final_path = if args.url {
            // --url means no download, so no path to resolve against disk.
            None
        } else {
            match resolve_final_path(&initial_path, policy) {
                Some(p) => Some(p),
                None => return ExitCode::UserError,
            }
        };

        let request = ImageRequest {
            prompt,
            model: model.clone(),
            size: args.size.clone(),
            quality: args.quality.clone(),
        };

        let spinner = start_spinner_if_tty(&model);
        let start = std::time::Instant::now();
        let result = image_gen::generate(
            &key,
            &request,
            final_path.as_deref(),
            target.pins_extension(),
            args.url,
        )
        .await;
        let elapsed = start.elapsed();
        stop_spinner(spinner);

        let artifact = match result {
            Ok(a) => a,
            Err(e) => {
                eprintln!("{} {}", style::red("Error:"), e);
                return ExitCode::NetworkError;
            }
        };

        // Intentionally not persisting the chosen image model: aivo chat
        // reads from the same per-key slot unconditionally, so writing
        // `gpt-image-1` there would make the next `aivo chat` on this key
        // default to an image model. Until a dedicated per-key image slot
        // lands, the user re-selects (or re-passes `-m`) each time.

        if args.json {
            print_json(&artifact, &key, &model, &request, elapsed);
        } else {
            print_human(&artifact, &key, &model, request.size.as_deref());
        }
        ExitCode::Success
    }
}

fn resolve_prompt(arg: Option<&str>) -> anyhow::Result<String> {
    if let Some(p) = arg {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    // Mirrors `aivo chat -x`: on a TTY, show a hint and read until EOF;
    // on a pipe, read silently. Either way, fall through to stdin when
    // no positional prompt was supplied.
    if std::io::stdin().is_terminal() {
        eprintln!("{}", style::dim("Enter prompt, then press Ctrl-D to send."));
    }
    let stdin = image_gen::read_stdin_prompt()?;
    if stdin.is_empty() {
        anyhow::bail!("empty prompt on stdin");
    }
    Ok(stdin)
}

async fn resolve_image_model(
    session_store: &SessionStore,
    cache: &ModelsCache,
    args: &ImageArgs,
    key: &ApiKey,
) -> anyhow::Result<Option<String>> {
    match &args.model {
        // `-m <name>` → use it.
        Some(m) if !m.is_empty() => {
            let resolved = session_store.resolve_alias(m).await.unwrap_or(m.clone());
            Ok(Some(resolved))
        }
        // Bare `-m`, or no flag at all → picker. The shared chat-model slot
        // doesn't hold image models (see comment in `execute`), so there's
        // no safe auto-reuse candidate to try first.
        _ => pick_image_model_interactively(cache, key, args.refresh).await,
    }
}

/// Opens an interactive picker over the provider's full model list. Returns
/// `None` when the user cancels. We use `fetch_all_models_cached` so
/// providers like xai that advertise `grok-2-image` / `grok-imagine-image`
/// surface the image models the user is here to pick (the chat picker's
/// `is_text_chat_model` filter would strip them).
async fn pick_image_model_interactively(
    cache: &ModelsCache,
    key: &ApiKey,
    refresh: bool,
) -> anyhow::Result<Option<String>> {
    if !std::io::stderr().is_terminal() {
        anyhow::bail!(
            "no image model specified and no terminal available; pass -m <name> (e.g. gpt-image-1)"
        );
    }

    let client = router_http_client();
    let all_models = crate::commands::models::fetch_all_models_cached(&client, key, cache, refresh)
        .await
        .unwrap_or_default();

    if all_models.is_empty() {
        anyhow::bail!(
            "could not fetch a model list for this key; pass -m <name> explicitly (e.g. gpt-image-1, dall-e-3)"
        );
    }

    // Image picker shows every model the provider advertises and lets the
    // user decide. Heuristically flagging "text" models produced false
    // positives for providers like ideogram / recraft / flux; the provider
    // error on submit is a better signal than our guess.
    Ok(crate::commands::models::prompt_model_picker(
        all_models,
        None,
        Vec::new(),
    ))
}

fn default_extension(size: Option<&str>) -> String {
    // Size doesn't affect extension, but keep the signature future-proof for
    // quality-based format routing. Default is png everywhere.
    let _ = size;
    "png".into()
}

fn resolve_final_path(initial: &Path, policy: OverwritePolicy) -> Option<PathBuf> {
    let answer = if !policy.force && policy.interactive && initial.exists() {
        Some(image_gen::prompt_overwrite(initial))
    } else {
        None
    };
    match image_gen::apply_overwrite_policy(initial, policy, answer) {
        OverwriteDecision::Write(p) => Some(p),
        OverwriteDecision::Abort => {
            if !policy.interactive {
                eprintln!(
                    "{} '{}' already exists (pass -f to overwrite).",
                    style::red("Error:"),
                    initial.display()
                );
            }
            None
        }
    }
}

fn start_spinner_if_tty(model: &str) -> Option<(Arc<AtomicBool>, JoinHandle<()>)> {
    if std::io::stderr().is_terminal() {
        Some(style::start_spinner(Some(&format!(
            " Generating image with {}…",
            model
        ))))
    } else {
        None
    }
}

fn stop_spinner(spinner: Option<(Arc<AtomicBool>, JoinHandle<()>)>) {
    if let Some((flag, _handle)) = spinner {
        style::stop_spinner(&flag);
    }
}

fn print_human(artifact: &ImageArtifact, key: &ApiKey, model: &str, size: Option<&str>) {
    if let Some(path) = &artifact.path {
        println!(
            "{} saved {} ({}, {}) via {}/{}",
            style::success_symbol(),
            style::cyan(path.display().to_string()),
            size.unwrap_or("default size"),
            human_bytes(artifact.bytes),
            style::dim(key.display_name()),
            style::dim(model),
        );
    } else if let Some(url) = &artifact.url {
        println!("{} {}", style::arrow_symbol(), url);
    }
}

fn print_json(
    artifact: &ImageArtifact,
    key: &ApiKey,
    model: &str,
    request: &ImageRequest,
    elapsed: std::time::Duration,
) {
    let out = json!({
        "model": model,
        "key": key.display_name(),
        "size": request.size,
        "quality": request.quality,
        "duration_ms": elapsed.as_millis() as u64,
        "path": artifact.path.as_ref().map(|p| p.display().to_string()),
        "url": artifact.url,
        "bytes": artifact.bytes,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
}

fn human_bytes(b: u64) -> String {
    const K: u64 = 1024;
    if b < K {
        format!("{b}B")
    } else if b < K * K {
        format!("{:.1}KB", b as f64 / K as f64)
    } else {
        format!("{:.1}MB", b as f64 / (K * K) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_formats_ranges() {
        assert_eq!(human_bytes(500), "500B");
        assert_eq!(human_bytes(1024), "1.0KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0MB");
    }

    #[test]
    fn default_extension_is_png() {
        assert_eq!(default_extension(None), "png");
        assert_eq!(default_extension(Some("1024x1024")), "png");
    }
}
