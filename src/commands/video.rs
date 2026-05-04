//! `aivo video` — generate a video from a text prompt.
//!
//! Video generation is async on every supported provider, so this command
//! submits a job, prints the job ID up front (so a Ctrl+C'd session can
//! recover), spins on the polling loop, and downloads when the job is
//! `completed`. `--job-id <id>` skips submit and attaches to an in-flight
//! job — same downstream wait + download as the normal path.

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::json;
use tokio::task::JoinHandle;

use crate::cli::VideoArgs;
use crate::errors::ExitCode;
use crate::services::http_utils::router_http_client;
use crate::services::media_io::{self, OutputTarget, OverwritePolicy, human_bytes};
use crate::services::models_cache::ModelsCache;
use crate::services::session_store::{ApiKey, SessionStore};
use crate::services::video_gen::{self, PollOptions, VideoArtifact, VideoRequest};
use crate::style;

pub struct VideoCommand {
    session_store: SessionStore,
    cache: ModelsCache,
}

impl VideoCommand {
    pub fn new(session_store: SessionStore, cache: ModelsCache) -> Self {
        Self {
            session_store,
            cache,
        }
    }

    pub fn print_help() {
        println!(
            "{} {}",
            style::cyan("aivo video"),
            style::dim("— generate videos from a prompt (async)")
        );
        println!();
        println!("{} aivo video [OPTIONS] <PROMPT>", style::bold("Usage:"));
        println!();
        println!("{}", style::bold("Arguments:"));
        println!(
            "  {}{}",
            style::cyan(format!("{:<24}", "PROMPT")),
            style::dim("Text prompt for the video")
        );
        println!();
        println!("{}", style::bold("Options:"));
        let opt = |f: &str, d: &str| {
            println!("  {}{}", style::cyan(format!("{:<24}", f)), style::dim(d));
        };
        opt(
            "-m, --model <MODEL>",
            "Video model (e.g. sora-2, veo-3.0-generate-preview)",
        );
        opt("-k, --key <ID|NAME>", "API key to use");
        opt(
            "-o, --output <PATH>",
            "File, directory, or template ({ts}/{model})",
        );
        opt("-f, --force", "Overwrite existing files without prompting");
        opt(
            "-s, --size <WxH>",
            "1280x720 | 720x1280 | 1920x1080 | 16:9 | …",
        );
        opt(
            "    --seconds <N>",
            "Clip length, typically 4–20 (provider-dependent)",
        );
        opt("    --seed <N>", "Random seed for reproducibility");
        opt(
            "    --timeout <SECS>",
            "Polling timeout (default 600s); on timeout, recover with --job-id",
        );
        opt(
            "    --job-id <ID>",
            "Attach to an existing job instead of submitting a new one",
        );
        opt("-r, --refresh", "Bypass model-list cache");
        opt("    --json", "Emit JSON result (for scripting)");
        println!();
        println!("{}", style::bold("Examples:"));
        println!(
            "  {}",
            style::dim("aivo video \"a corgi running on a beach at sunset\"")
        );
        println!(
            "  {}",
            style::dim("aivo video \"city timelapse\" -m sora-2 --seconds 8 -s 1920x1080")
        );
        println!(
            "  {}",
            style::dim("aivo video --job-id video_abc123   # attach + download after a Ctrl+C")
        );
    }

    /// Prints the video-scope active key and model under the help output.
    pub async fn print_active_selection(session_store: &SessionStore) {
        let sel = session_store
            .get_last_video_selection()
            .await
            .ok()
            .flatten();
        crate::commands::print_active_selection_for(session_store, sel).await;
    }

    pub async fn execute(self, args: VideoArgs, key: ApiKey) -> ExitCode {
        // `--job-id` is the recovery path: we don't need a prompt or a model.
        // Without `--job-id`, an empty prompt prints help (mirrors image/audio).
        let prompt = if args.job_id.is_some() {
            None
        } else {
            match args
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(p) => Some(p.to_string()),
                None => {
                    Self::print_help();
                    Self::print_active_selection(&self.session_store).await;
                    return ExitCode::Success;
                }
            }
        };

        // Model is only needed to *submit*; recovery doesn't need it. We still
        // resolve it for the new-job path so the picker fires when the user
        // hasn't pinned a model yet.
        let model = if prompt.is_some() {
            match resolve_video_model(&self.session_store, &self.cache, &args, &key).await {
                Ok(Some(m)) => Some(m),
                Ok(None) => return ExitCode::Success, // picker cancelled
                Err(e) => {
                    eprintln!("{} {}", style::red("Error:"), e);
                    return ExitCode::UserError;
                }
            }
        } else {
            None
        };

        // Preflight: resolve the output path before submit so we fail fast
        // on bad paths instead of after a (possibly expensive) generation.
        let target = OutputTarget::parse(args.output.as_deref());
        let model_for_path = model.as_deref().unwrap_or("video");
        let initial_path = match media_io::resolve_output_path(&target, model_for_path, "mp4") {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{} {}", style::red("Error:"), e);
                return ExitCode::UserError;
            }
        };
        let policy = OverwritePolicy::from_flags(args.force, args.json);
        let final_path = match crate::commands::resolve_final_path(&initial_path, policy, "-f") {
            Some(p) => p,
            None => return ExitCode::UserError,
        };

        let poll = PollOptions {
            timeout: Duration::from_secs(args.timeout as u64),
            interval: Duration::from_secs(5),
        };

        let start = std::time::Instant::now();
        let result = if let Some(job_id) = args.job_id.as_deref() {
            let spinner = start_spinner_if_tty(&format!("Attaching to job {job_id}…"));
            let r = video_gen::attach(
                &key,
                job_id,
                Some(&final_path),
                target.pins_extension(),
                poll,
            )
            .await;
            stop_spinner(spinner);
            r
        } else {
            // model and prompt are both Some on this branch by construction.
            let request = VideoRequest {
                prompt: prompt.expect("prompt is set on the submit path"),
                model: model.clone().expect("model is resolved on the submit path"),
                size: args.size.clone(),
                seconds: args.seconds,
                seed: args.seed,
            };
            let label = format!("Generating video with {}…", request.model);
            let spinner = start_spinner_if_tty(&label);
            let r = video_gen::generate(
                &key,
                &request,
                Some(&final_path),
                target.pins_extension(),
                poll,
            )
            .await;
            stop_spinner(spinner);
            r
        };
        let elapsed = start.elapsed();

        let artifact = match result {
            Ok(a) => a,
            Err(e) => {
                eprintln!("{} {}", style::red("Error:"), e);
                return ExitCode::NetworkError;
            }
        };

        // Persist (key, model) to the video-only slot. Skip on the recovery
        // path — we don't have a model to remember there.
        if let Some(model_str) = model.as_deref() {
            let _ = self
                .session_store
                .set_last_video_selection(&key, Some(model_str))
                .await;
        }

        if args.json {
            print_json(&artifact, &key, model.as_deref(), &args, elapsed);
        } else {
            print_human(&artifact, &key, model.as_deref(), args.size.as_deref());
        }
        ExitCode::Success
    }
}

async fn resolve_video_model(
    session_store: &SessionStore,
    cache: &ModelsCache,
    args: &VideoArgs,
    key: &ApiKey,
) -> anyhow::Result<Option<String>> {
    match &args.model {
        Some(m) if !m.is_empty() => {
            let resolved = session_store.resolve_alias(m).await.unwrap_or(m.clone());
            Ok(Some(resolved))
        }
        Some(_) => pick_video_model_interactively(cache, key, args.refresh).await,
        None => {
            if let Ok(Some(sel)) = session_store.get_last_video_selection().await
                && sel.key_id == key.id
                && let Some(model) = sel.model
                && !model.is_empty()
            {
                return Ok(Some(model));
            }
            pick_video_model_interactively(cache, key, args.refresh).await
        }
    }
}

/// Opens a model picker over the provider's full model list. Same approach
/// as image/audio: don't filter heuristically; the provider error on submit
/// is a better signal than our guess about which models can do video.
async fn pick_video_model_interactively(
    cache: &ModelsCache,
    key: &ApiKey,
    refresh: bool,
) -> anyhow::Result<Option<String>> {
    if !std::io::stderr().is_terminal() {
        anyhow::bail!(
            "no video model specified and no terminal available; pass -m <name> (e.g. sora-2)"
        );
    }

    let client = router_http_client();
    let all_models = crate::commands::models::fetch_all_models_cached(&client, key, cache, refresh)
        .await
        .unwrap_or_default();

    if all_models.is_empty() {
        anyhow::bail!(
            "could not fetch a model list for this key; pass -m <name> explicitly (e.g. sora-2)"
        );
    }

    Ok(crate::commands::models::prompt_model_picker(
        all_models,
        None,
        Vec::new(),
        "Select model",
    ))
}

fn start_spinner_if_tty(label: &str) -> Option<(Arc<AtomicBool>, JoinHandle<()>)> {
    if std::io::stderr().is_terminal() {
        Some(style::start_spinner(Some(&format!(" {label}"))))
    } else {
        None
    }
}

fn stop_spinner(spinner: Option<(Arc<AtomicBool>, JoinHandle<()>)>) {
    if let Some((flag, _handle)) = spinner {
        style::stop_spinner(&flag);
    }
}

fn print_human(artifact: &VideoArtifact, key: &ApiKey, model: Option<&str>, size: Option<&str>) {
    if let Some(path) = &artifact.path {
        // Vercel's path has no real job id; suffix only when one exists.
        let job_suffix = artifact
            .job_id
            .as_deref()
            .map(|id| format!(" (job {})", style::dim(id)))
            .unwrap_or_default();
        println!(
            "{} saved {} ({}, {}) via {}/{}{}",
            style::success_symbol(),
            style::cyan(path.display().to_string()),
            size.unwrap_or("default size"),
            human_bytes(artifact.bytes),
            style::dim(key.display_name()),
            style::dim(model.unwrap_or("video")),
            job_suffix,
        );
    }
}

fn print_json(
    artifact: &VideoArtifact,
    key: &ApiKey,
    model: Option<&str>,
    args: &VideoArgs,
    elapsed: std::time::Duration,
) {
    let out = json!({
        "model": model,
        "key": key.display_name(),
        "size": args.size,
        "seconds": args.seconds,
        "seed": args.seed,
        "duration_ms": elapsed.as_millis() as u64,
        "path": artifact.path.as_ref().map(|p| p.display().to_string()),
        "url": artifact.url,
        "bytes": artifact.bytes,
        "job_id": artifact.job_id,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
}
