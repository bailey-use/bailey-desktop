//! `aivo speak` — generate speech (TTS) from a text prompt and play it.
//!
//! Resolves a key, takes the prompt from a positional arg / `--file` /
//! piped stdin, calls the provider, saves the result, and plays it. Every
//! invocation lands in the on-disk cache at `~/.config/aivo/audio/`,
//! keyed by `(prompt, voice, model, format, speed)`. Repeat calls with
//! identical inputs hit the cache and skip the provider entirely.

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::json;
use tokio::task::JoinHandle;

use crate::cli::AudioArgs;
use crate::errors::ExitCode;
use crate::services::audio_cache::{self, CacheKey};
use crate::services::audio_gen::{self, AudioArtifact, AudioRequest};
use crate::services::http_utils::router_http_client;
use crate::services::media_io::{self, OutputTarget, OverwritePolicy, human_bytes};
use crate::services::models_cache::ModelsCache;
use crate::services::playback;
use crate::services::session_store::{ApiKey, SessionStore};
use crate::style;

pub struct AudioCommand {
    session_store: SessionStore,
    cache: ModelsCache,
}

impl AudioCommand {
    pub fn new(session_store: SessionStore, cache: ModelsCache) -> Self {
        Self {
            session_store,
            cache,
        }
    }

    pub fn print_help() {
        let name = "aivo speak";
        println!(
            "{} {}",
            style::cyan(name),
            style::dim("— speak a prompt aloud (TTS, cached, plays by default)")
        );
        println!();
        println!("{} {} [OPTIONS] [<PROMPT>]", style::bold("Usage:"), name);
        println!();
        println!("{}", style::bold("Arguments:"));
        println!(
            "  {}{}",
            style::cyan(format!("{:<24}", "PROMPT")),
            style::dim("Text to read aloud (or use -f / pipe stdin)")
        );
        println!();
        println!("{}", style::bold("Options:"));
        let opt = |f: &str, d: &str| {
            println!("  {}{}", style::cyan(format!("{:<24}", f)), style::dim(d));
        };
        opt("-f, --file <PATH>", "Read prompt text from a file (UTF-8)");
        opt("-m, --model <MODEL>", "TTS model (e.g. tts-1, tts-1-hd)");
        opt("-k, --key <ID|NAME>", "API key to use");
        opt(
            "-o, --output <PATH>",
            "File, directory, or template ({ts}/{model}); default: cache dir",
        );
        opt(
            "    --overwrite",
            "Bypass cache and overwrite -o without prompting",
        );
        opt("    --voice <VOICE>", "alloy | nova | onyx | echo | …");
        opt(
            "    --format <FORMAT>",
            "mp3 (default) | wav | opus | aac | flac",
        );
        opt("    --speed <SPEED>", "Playback speed, typically 0.25–4.0");
        opt("    --no-play", "Save without playing");
        opt("-r, --refresh", "Bypass model-list cache");
        opt("    --json", "Emit JSON result (for scripting)");
        println!();
        println!("{}", style::bold("Examples:"));
        let ex = |s: &str| println!("  {}", style::dim(s));
        ex("aivo speak \"hello world\"");
        ex("aivo speak \"narration line\" -m tts-1-hd --voice nova");
        ex("aivo speak -f script.txt");
        ex("echo \"hi from pipe\" | aivo speak");
        ex("aivo speak \"...\" --no-play -o out.mp3   # save only");
        ex("aivo speak \"...\" --overwrite           # force regenerate");
    }

    /// Prints the audio-scope active key and model under the help output.
    /// Reads the audio-only `last_audio_selection` slot so it doesn't surface
    /// a chat key the user picked for `aivo chat`.
    pub async fn print_active_selection(session_store: &SessionStore) {
        let sel = session_store
            .get_last_audio_selection()
            .await
            .ok()
            .flatten();
        crate::commands::print_active_selection_for(session_store, sel).await;
    }

    pub async fn execute(self, args: AudioArgs, key: ApiKey, prompt: String) -> ExitCode {
        let model = match resolve_audio_model(&self.session_store, &self.cache, &args, &key).await {
            Ok(Some(m)) => m,
            Ok(None) => return ExitCode::Success, // picker cancelled
            Err(e) => {
                eprintln!("{} {}", style::red("Error:"), e);
                return ExitCode::UserError;
            }
        };

        let cache_key = CacheKey::from_inputs(
            &prompt,
            args.voice.as_deref(),
            &model,
            args.format.as_deref(),
            args.speed,
        );
        let ext = default_extension(args.format.as_deref());
        let cache_dir = audio_cache::audio_cache_dir(self.session_store.config_dir());
        let cache_file = audio_cache::cache_path(&cache_dir, &cache_key, &ext);

        // Resolve the user-visible save path. None → save *is* the cache file.
        // Some(path) → save into the user's chosen path; the cache file is
        // still populated for future hits.
        let user_output_path: Option<PathBuf> = match args.output.as_deref() {
            None => None,
            Some(_) => {
                let target = OutputTarget::parse(args.output.as_deref());
                let initial = match media_io::resolve_output_path(&target, &model, &ext) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("{} {}", style::red("Error:"), e);
                        return ExitCode::UserError;
                    }
                };
                let policy = OverwritePolicy::from_flags(args.overwrite, args.json);
                match crate::commands::resolve_final_path(&initial, policy, "--overwrite") {
                    Some(p) => Some(p),
                    None => return ExitCode::UserError,
                }
            }
        };

        // Try the cache first. ENOENT == cache miss; any other error is
        // surfaced (corrupt permissions, etc.). `--overwrite` skips the
        // lookup so the next branch always regenerates.
        let cache_metadata = if args.overwrite {
            None
        } else {
            match fs::metadata(&cache_file) {
                Ok(m) => Some(m),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    eprintln!(
                        "{} cached file unreadable ({}): {}",
                        style::red("Error:"),
                        cache_file.display(),
                        e
                    );
                    return ExitCode::UserError;
                }
            }
        };
        let cache_hit = cache_metadata.is_some();

        let mut elapsed = std::time::Duration::ZERO;
        let bytes = if let Some(meta) = cache_metadata {
            meta.len()
        } else {
            if let Some(parent) = cache_file.parent()
                && let Err(e) = fs::create_dir_all(parent)
            {
                eprintln!(
                    "{} cannot create cache dir '{}': {}",
                    style::red("Error:"),
                    parent.display(),
                    e
                );
                return ExitCode::UserError;
            }
            // Pin the extension on the cache path: the cache filename's
            // extension is derived from `args.format`, and we want the same
            // bytes mapped to the same path on every run regardless of the
            // server's content-type header.
            let request = AudioRequest {
                prompt: prompt.clone(),
                model: model.clone(),
                voice: args.voice.clone(),
                format: args.format.clone(),
                speed: args.speed,
            };
            let spinner = start_spinner_if_tty(&model);
            let start = std::time::Instant::now();
            let result = audio_gen::generate(&key, &request, Some(&cache_file), true).await;
            elapsed = start.elapsed();
            stop_spinner(spinner);
            match result {
                Ok(a) => a.bytes,
                Err(e) => {
                    eprintln!("{} {}", style::red("Error:"), e);
                    return ExitCode::NetworkError;
                }
            }
        };

        let final_path = match user_output_path {
            Some(dest) if dest != cache_file => {
                if let Err(e) = fs::copy(&cache_file, &dest) {
                    eprintln!(
                        "{} cannot write '{}': {}",
                        style::red("Error:"),
                        dest.display(),
                        e
                    );
                    return ExitCode::UserError;
                }
                dest
            }
            Some(dest) => dest,
            None => cache_file.clone(),
        };
        let artifact = AudioArtifact {
            path: Some(final_path.clone()),
            bytes,
        };

        let mut played = false;
        let mut playback_error: Option<String> = None;
        if !args.no_play {
            match playback::play_audio_blocking(&final_path) {
                Ok(()) => played = true,
                Err(e) => playback_error = Some(e.to_string()),
            }
        }

        let _ = self
            .session_store
            .set_last_audio_selection(&key, Some(&model))
            .await;

        if args.json {
            print_json(
                &artifact,
                &key,
                &model,
                args.voice.as_deref(),
                args.format.as_deref(),
                args.speed,
                elapsed,
                played,
                cache_hit,
            );
        } else {
            print_human(
                &artifact,
                &key,
                &model,
                args.voice.as_deref(),
                played,
                cache_hit,
                playback_error.as_deref(),
            );
        }
        ExitCode::Success
    }
}

async fn resolve_audio_model(
    session_store: &SessionStore,
    cache: &ModelsCache,
    args: &AudioArgs,
    key: &ApiKey,
) -> anyhow::Result<Option<String>> {
    match &args.model {
        Some(m) if !m.is_empty() => {
            let resolved = session_store.resolve_alias(m).await.unwrap_or(m.clone());
            Ok(Some(resolved))
        }
        Some(_) => pick_audio_model_interactively(cache, key, args.refresh).await,
        None => {
            if let Ok(Some(sel)) = session_store.get_last_audio_selection().await
                && sel.key_id == key.id
                && let Some(model) = sel.model
                && !model.is_empty()
            {
                return Ok(Some(model));
            }
            pick_audio_model_interactively(cache, key, args.refresh).await
        }
    }
}

/// Opens a model picker over the provider's full model list. Same approach
/// as the image picker: don't filter heuristically, let the provider error
/// be the signal — TTS model naming varies wildly across providers.
async fn pick_audio_model_interactively(
    cache: &ModelsCache,
    key: &ApiKey,
    refresh: bool,
) -> anyhow::Result<Option<String>> {
    if !std::io::stderr().is_terminal() {
        anyhow::bail!(
            "no audio model specified and no terminal available; pass -m <name> (e.g. tts-1)"
        );
    }

    let client = router_http_client();
    let all_models = crate::commands::models::fetch_all_models_cached(&client, key, cache, refresh)
        .await
        .unwrap_or_default();

    if all_models.is_empty() {
        anyhow::bail!(
            "could not fetch a model list for this key; pass -m <name> explicitly (e.g. tts-1, tts-1-hd)"
        );
    }

    Ok(crate::commands::models::prompt_model_picker(
        all_models,
        None,
        Vec::new(),
        "Select model",
    ))
}

/// Maps the user-requested format flag to a default file extension. When
/// `--format` is missing, MP3 is used (matches OpenAI's server-side default).
fn default_extension(format: Option<&str>) -> String {
    match format.map(str::to_ascii_lowercase).as_deref() {
        Some("wav") => "wav".into(),
        Some("opus") => "opus".into(),
        Some("aac") => "aac".into(),
        Some("flac") => "flac".into(),
        Some("pcm") => "pcm".into(),
        // mp3, anything unknown, or missing → .mp3 default
        _ => "mp3".into(),
    }
}

fn start_spinner_if_tty(model: &str) -> Option<(Arc<AtomicBool>, JoinHandle<()>)> {
    if std::io::stderr().is_terminal() {
        let label = format!(" Speaking with {}…", model);
        Some(style::start_spinner(Some(&label)))
    } else {
        None
    }
}

fn stop_spinner(spinner: Option<(Arc<AtomicBool>, JoinHandle<()>)>) {
    if let Some((flag, _handle)) = spinner {
        style::stop_spinner(&flag);
    }
}

fn print_human(
    artifact: &AudioArtifact,
    key: &ApiKey,
    model: &str,
    voice: Option<&str>,
    played: bool,
    cached: bool,
    playback_error: Option<&str>,
) {
    let Some(path) = &artifact.path else {
        return;
    };
    let mut tags: Vec<String> = Vec::new();
    if cached {
        tags.push(style::dim("cached").to_string());
    }
    if played {
        tags.push(style::dim("played").to_string());
    } else if let Some(err) = playback_error {
        tags.push(format!("{}: {}", style::yellow("playback skipped"), err));
    }
    let suffix = if tags.is_empty() {
        String::new()
    } else {
        format!(" ({})", tags.join(", "))
    };
    println!(
        "{} saved {} ({}, {}) via {}/{}{}",
        style::success_symbol(),
        style::cyan(path.display().to_string()),
        voice.unwrap_or("default voice"),
        human_bytes(artifact.bytes),
        style::dim(key.display_name()),
        style::dim(model),
        suffix,
    );
}

#[allow(clippy::too_many_arguments)]
fn print_json(
    artifact: &AudioArtifact,
    key: &ApiKey,
    model: &str,
    voice: Option<&str>,
    format: Option<&str>,
    speed: Option<f32>,
    elapsed: std::time::Duration,
    played: bool,
    cached: bool,
) {
    let path = artifact.path.as_ref().map(|p| p.display().to_string());
    let out = json!({
        "model": model,
        "key": key.display_name(),
        "voice": voice,
        "format": format,
        "speed": speed,
        "duration_ms": elapsed.as_millis() as u64,
        "path": path,
        "bytes": artifact.bytes,
        "played": played,
        "cached": cached,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
}

/// Reads prompt text from a file path. Trims trailing whitespace; rejects
/// empty/whitespace-only files. Errors include the path for triage.
#[allow(dead_code)] // used by the binary's main.rs; lib build doesn't see it
pub fn read_prompt_file(path: &Path) -> anyhow::Result<String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read --file '{}': {}", path.display(), e))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("--file '{}' is empty", path.display());
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_extension_picks_mp3_for_default_or_unknown() {
        assert_eq!(default_extension(None), "mp3");
        assert_eq!(default_extension(Some("mp3")), "mp3");
        assert_eq!(default_extension(Some("garbage")), "mp3");
    }

    #[test]
    fn default_extension_picks_explicit_formats() {
        assert_eq!(default_extension(Some("wav")), "wav");
        assert_eq!(default_extension(Some("WAV")), "wav");
        assert_eq!(default_extension(Some("opus")), "opus");
        assert_eq!(default_extension(Some("aac")), "aac");
        assert_eq!(default_extension(Some("flac")), "flac");
        assert_eq!(default_extension(Some("pcm")), "pcm");
    }

    #[test]
    fn read_prompt_file_returns_trimmed_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt.txt");
        fs::write(&path, "  hello world  \n").unwrap();
        let out = read_prompt_file(&path).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn read_prompt_file_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        fs::write(&path, "   \n\t\n").unwrap();
        assert!(read_prompt_file(&path).is_err());
    }

    #[test]
    fn read_prompt_file_reports_missing_path() {
        let err = read_prompt_file(Path::new("/nonexistent/aivo-test.txt")).unwrap_err();
        assert!(err.to_string().contains("--file"));
    }
}
