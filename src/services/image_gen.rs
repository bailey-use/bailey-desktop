//! Image generation service.
//!
//! Handles OpenAI-compatible `/v1/images/generations` requests plus the output
//! path UX (default/exact/directory/template forms), overwrite policy,
//! and atomic file writes. Google Imagen is not yet implemented (returns a
//! clear error) and is tracked for a follow-up.

use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::services::http_utils::router_http_client;
use crate::services::provider_protocol::{ProviderProtocol, detect_provider_protocol};
use crate::services::session_store::ApiKey;

/// Options for a single image generation request.
#[derive(Debug, Clone)]
pub struct ImageRequest {
    pub prompt: String,
    pub model: String,
    pub size: Option<String>,
    pub quality: Option<String>,
}

/// One saved image (or URL, when `--url` was set).
#[derive(Debug, Clone)]
pub struct ImageArtifact {
    /// Path the file was written to, or `None` when `url_only` is set.
    pub path: Option<PathBuf>,
    /// Provider URL (OpenAI sometimes returns a URL, sometimes base64).
    pub url: Option<String>,
    /// Size of the written file in bytes (0 when `url_only`).
    pub bytes: u64,
}

/// How `-o`/`--output` was specified.
#[derive(Debug, Clone)]
pub enum OutputTarget {
    /// No `-o` given — use default timestamped filename in CWD.
    Default,
    /// `-o path` where the path ends with `/` or is an existing directory.
    Directory(PathBuf),
    /// `-o path` pointing at a specific file.
    File(PathBuf),
    /// `-o "tmpl.png"` — a template with `{ts}`/`{model}` tokens.
    Template(String),
}

impl OutputTarget {
    /// Parse the raw `-o` argument. Returns `Default` when `arg` is `None`.
    pub fn parse(arg: Option<&str>) -> Self {
        let raw = match arg {
            None => return Self::Default,
            Some(r) => r,
        };

        if raw.contains('{') && raw.contains('}') {
            return Self::Template(raw.to_string());
        }

        let path = PathBuf::from(raw);
        let looks_like_dir =
            raw.ends_with('/') || raw.ends_with(std::path::MAIN_SEPARATOR) || path.is_dir();
        if looks_like_dir {
            Self::Directory(path)
        } else {
            Self::File(path)
        }
    }

    /// True when the user's `-o` value pins a file extension. For Default
    /// and Directory the extension was chosen by us (currently "png"), so
    /// the caller is free to swap in the server's actual content-type.
    pub fn pins_extension(&self) -> bool {
        match self {
            Self::Default | Self::Directory(_) => false,
            Self::File(p) => p.extension().is_some(),
            Self::Template(s) => Path::new(s).extension().is_some(),
        }
    }
}

/// Resolves the concrete output path before any API call is made. Collision
/// resolution happens after the response, once the extension is known, via
/// [`apply_overwrite_policy`].
pub fn resolve_output_path(target: &OutputTarget, model: &str, ext: &str) -> Result<PathBuf> {
    let ts = Utc::now().format("%Y%m%d-%H%M%S").to_string();

    match target {
        OutputTarget::Default => Ok(PathBuf::from(format!("./aivo-{ts}.{ext}"))),
        OutputTarget::Directory(dir) => {
            verify_writable_dir(dir)?;
            Ok(dir.join(format!("aivo-{ts}.{ext}")))
        }
        OutputTarget::File(path) => {
            if let Some(parent) = parent_if_nonempty(path) {
                verify_writable_dir(parent)?;
            }
            let (stem, real_ext) = split_stem_ext(path, ext);
            let dir = path.parent().unwrap_or(Path::new("."));
            Ok(dir.join(format!("{stem}.{real_ext}")))
        }
        OutputTarget::Template(tmpl) => {
            let expanded = expand_template(tmpl, &ts, model);
            let path = PathBuf::from(&expanded);
            if let Some(parent) = parent_if_nonempty(&path) {
                verify_writable_dir(parent)?;
            }
            Ok(path)
        }
    }
}

/// Returns `path.parent()` only when it's a non-empty path. `Path::parent()`
/// returns `Some("")` for bare filenames like `cat.png`, which isn't useful
/// for directory checks.
fn parent_if_nonempty(path: &Path) -> Option<&Path> {
    path.parent().filter(|p| !p.as_os_str().is_empty())
}

fn expand_template(tmpl: &str, ts: &str, model: &str) -> String {
    tmpl.replace("{ts}", ts)
        .replace("{model}", &sanitize_model(model))
}

/// Replaces filesystem-hostile characters in a model id with underscores.
pub fn sanitize_model(model: &str) -> String {
    model
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect()
}

fn split_stem_ext(path: &Path, default_ext: &str) -> (String, String) {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("aivo")
        .to_string();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| default_ext.to_string());
    (stem, ext)
}

fn verify_writable_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        bail!(
            "directory '{}' does not exist (create it first, or omit -o)",
            dir.display()
        );
    }
    if !dir.is_dir() {
        bail!("'{}' is not a directory", dir.display());
    }
    // Probe writability with a metadata check; a true write test would race.
    let meta = fs::metadata(dir)
        .with_context(|| format!("cannot access directory '{}'", dir.display()))?;
    if meta.permissions().readonly() {
        bail!("cannot write to '{}': permission denied", dir.display());
    }
    Ok(())
}

/// Decision made by [`apply_overwrite_policy`] for a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverwriteDecision {
    /// Write at this exact path (replacing any existing file, or writing a
    /// fresh file, or a `-1`/`-2`/… auto-suffix when the user chose skip).
    Write(PathBuf),
    /// Abort the whole run (non-TTY / JSON / explicit "no").
    Abort,
}

/// Controls how [`apply_overwrite_policy`] resolves existing-file collisions.
#[derive(Debug, Clone, Copy)]
pub struct OverwritePolicy {
    pub force: bool,
    pub interactive: bool,
}

impl OverwritePolicy {
    pub fn from_flags(force: bool, json_mode: bool) -> Self {
        Self {
            force,
            interactive: !json_mode && io::stdin().is_terminal() && io::stderr().is_terminal(),
        }
    }
}

/// Decides what to do with a single intended target path.
///
/// When `force` is set, always returns `Write`. When the path doesn't exist,
/// also `Write`. Otherwise prompts (if `interactive`) or aborts.
pub fn apply_overwrite_policy(
    path: &Path,
    policy: OverwritePolicy,
    prompt_answer: Option<char>,
) -> OverwriteDecision {
    if !path.exists() {
        return OverwriteDecision::Write(path.to_path_buf());
    }
    if policy.force {
        return OverwriteDecision::Write(path.to_path_buf());
    }
    if !policy.interactive {
        return OverwriteDecision::Abort;
    }
    let answer = prompt_answer.unwrap_or('n');
    match answer {
        'y' | 'Y' | 'a' | 'A' => OverwriteDecision::Write(path.to_path_buf()),
        's' | 'S' => {
            // Auto-suffix until we find a free path.
            OverwriteDecision::Write(next_free_path(path))
        }
        _ => OverwriteDecision::Abort,
    }
}

/// Finds the next free path by appending `-1`, `-2`, … before the extension.
pub fn next_free_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(Path::new("."));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("aivo");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("png");
    for i in 1..=9999 {
        let candidate = parent.join(format!("{stem}-{i}.{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Extreme fallback — unlikely to ever hit.
    parent.join(format!("{stem}-overflow.{ext}"))
}

/// Prompts the user once about overwriting `path`. Returns the raw char the
/// user entered (lowercased) or `'n'` on empty/EOF.
pub fn prompt_overwrite(path: &Path) -> char {
    eprint!(
        "File '{}' exists. Overwrite? [y/N/s(kip-and-suffix)]: ",
        path.display()
    );
    let _ = io::stderr().flush();
    let mut buf = String::new();
    if io::stdin().read_line(&mut buf).is_err() {
        return 'n';
    }
    buf.trim()
        .chars()
        .next()
        .map(|c| c.to_ascii_lowercase())
        .unwrap_or('n')
}

/// Infers a file extension from an HTTP `Content-Type` header. Falls back to
/// `"png"` for anything unrecognized — OpenAI's default.
pub fn ext_from_content_type(ct: Option<&str>) -> String {
    match ct.map(|c| {
        c.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    }) {
        Some(ref s) if s == "image/jpeg" || s == "image/jpg" => "jpg".into(),
        Some(ref s) if s == "image/webp" => "webp".into(),
        Some(ref s) if s == "image/gif" => "gif".into(),
        _ => "png".into(),
    }
}

/// Writes bytes atomically via a `.part` sibling + rename. Cleans up the
/// partial file on failure so a failed run never leaves a half-written
/// file at the final name.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<u64> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("aivo");
    let suffix: u32 = rand::random();
    let tmp = parent.join(format!(".{stem}.aivo-tmp-{suffix:x}.part"));

    let write_result = (|| -> Result<u64> {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("cannot create temp file '{}'", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write failed for '{}'", tmp.display()))?;
        f.sync_all().ok();
        drop(f);
        replace_with_temp_file(&tmp, path)?;
        Ok(bytes.len() as u64)
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

fn replace_with_temp_file(tmp: &Path, path: &Path) -> Result<()> {
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("cannot replace existing file '{}'", path.display()))?;
    }

    fs::rename(tmp, path)
        .with_context(|| format!("rename '{}' -> '{}' failed", tmp.display(), path.display()))
}

// ---- Provider-level image generation ----

#[derive(Debug, Deserialize)]
struct OpenAIImageResponse {
    data: Vec<OpenAIImageItem>,
}

#[derive(Debug, Deserialize)]
struct OpenAIImageItem {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    b64_json: Option<String>,
}

/// Top-level generation entry point. Picks the protocol from `key.base_url`
/// and dispatches accordingly. `path` is the pre-resolved, overwrite-applied
/// target path (or `None` when `url_only` is set). When `pinned_extension`
/// is false, the caller chose the extension (e.g. the default `.png` for
/// `OutputTarget::Default`) and we may swap it to the server's actual
/// content-type suffix silently. When true, the user's extension is honored.
///
/// When `url_only` is true, skips the download step and only returns the URL
/// (fails for base64-only responses).
pub async fn generate(
    key: &ApiKey,
    request: &ImageRequest,
    path: Option<&Path>,
    pinned_extension: bool,
    url_only: bool,
) -> Result<ImageArtifact> {
    let protocol = detect_provider_protocol(&key.base_url);
    match protocol {
        ProviderProtocol::Openai | ProviderProtocol::ResponsesApi => {
            generate_openai(key, request, path, pinned_extension, url_only).await
        }
        ProviderProtocol::Google => {
            bail!(
                "Google Imagen support is not yet implemented; try an OpenAI-compatible key \
                 (e.g. openai, openrouter, xai)"
            )
        }
        ProviderProtocol::Anthropic => {
            bail!("Anthropic does not support image generation")
        }
    }
}

async fn generate_openai(
    key: &ApiKey,
    request: &ImageRequest,
    path: Option<&Path>,
    pinned_extension: bool,
    url_only: bool,
) -> Result<ImageArtifact> {
    let base = key.base_url.trim_end_matches('/');
    // Accept both "https://api.example.com" and "https://api.example.com/v1".
    let url = if base.ends_with("/v1") {
        format!("{base}/images/generations")
    } else {
        format!("{base}/v1/images/generations")
    };

    let mut body = json!({
        "model": request.model,
        "prompt": request.prompt,
    });
    if let Some(s) = &request.size {
        body["size"] = Value::String(s.clone());
    }
    if let Some(q) = &request.quality {
        body["quality"] = Value::String(q.clone());
    }
    if url_only {
        body["response_format"] = Value::String("url".into());
    }

    let client = router_http_client();
    let response = client
        .post(&url)
        .bearer_auth(key.key.as_str())
        .json(&body)
        .send()
        .await
        .with_context(|| format!("image request to {url} failed"))?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let detail = extract_error_message(&text).unwrap_or_else(|| text.clone());
        bail!("image generation failed ({}): {}", status.as_u16(), detail);
    }

    let parsed: OpenAIImageResponse = response
        .json()
        .await
        .context("failed to decode image response")?;

    let item = parsed
        .data
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("provider returned no images"))?;

    if url_only {
        let url = item
            .url
            .ok_or_else(|| anyhow!("--url requested but provider returned base64 only"))?;
        return Ok(ImageArtifact {
            path: None,
            url: Some(url),
            bytes: 0,
        });
    }

    let (bytes, maybe_url, ext_hint) = if let Some(b64) = item.b64_json {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .context("failed to decode base64 image payload")?;
        (decoded, None, None::<String>)
    } else if let Some(u) = item.url {
        let resp = client
            .get(&u)
            .send()
            .await
            .with_context(|| format!("downloading image from {u} failed"))?;
        let status = resp.status();
        if !status.is_success() {
            let host = reqwest::Url::parse(&u)
                .ok()
                .and_then(|parsed| parsed.host_str().map(str::to_string))
                .unwrap_or_else(|| u.clone());
            bail!(
                "image download failed: {} returned HTTP {} — the signed URL may have expired",
                host,
                status.as_u16()
            );
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);
        let body = resp.bytes().await.context("reading image body failed")?;
        (body.to_vec(), Some(u), ct)
    } else {
        bail!("provider response missing both url and b64_json");
    };

    let Some(target_path) = path else {
        return Ok(ImageArtifact {
            path: None,
            url: maybe_url,
            bytes: bytes.len() as u64,
        });
    };

    let final_path = align_extension(target_path, ext_hint.as_deref(), pinned_extension);
    let written = atomic_write(&final_path, &bytes)?;
    Ok(ImageArtifact {
        path: Some(final_path),
        url: maybe_url,
        bytes: written,
    })
}

/// When the user didn't pin an extension, silently swap to the server's
/// actual content-type. When they did, honor their choice. Falls through
/// unchanged when the server didn't report a content-type.
fn align_extension(path: &Path, content_type: Option<&str>, pinned: bool) -> PathBuf {
    if pinned {
        return path.to_path_buf();
    }
    let Some(ct) = content_type else {
        return path.to_path_buf();
    };
    let server_ext = ext_from_content_type(Some(ct));
    let current_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if current_ext == server_ext {
        return path.to_path_buf();
    }
    path.with_extension(server_ext)
}

fn extract_error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .or_else(|| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
}

/// Reads the full stdin into a string. Used as the fallback prompt source
/// when `aivo image` is invoked without a positional prompt.
pub fn read_stdin_prompt() -> Result<String> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .context("failed to read prompt from stdin")?;
    Ok(buf.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_none_returns_default() {
        assert!(matches!(OutputTarget::parse(None), OutputTarget::Default));
    }

    #[test]
    fn parse_file_path() {
        let t = OutputTarget::parse(Some("cat.png"));
        assert!(matches!(t, OutputTarget::File(_)));
    }

    #[test]
    fn parse_trailing_slash_is_directory() {
        let t = OutputTarget::parse(Some("out/"));
        assert!(matches!(t, OutputTarget::Directory(_)));
    }

    #[test]
    fn parse_template_with_braces() {
        let t = OutputTarget::parse(Some("cat-{n}.png"));
        assert!(matches!(t, OutputTarget::Template(_)));
    }

    #[test]
    fn resolve_default() {
        let path = resolve_output_path(&OutputTarget::Default, "gpt-image-1", "png").unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("aivo-"));
        assert!(name.ends_with(".png"));
    }

    #[test]
    fn resolve_exact_file() {
        let t = OutputTarget::File(PathBuf::from("cat.png"));
        let path = resolve_output_path(&t, "gpt-image-1", "png").unwrap();
        assert_eq!(path.to_string_lossy(), "cat.png");
    }

    #[test]
    fn resolve_directory_existing() {
        let tmp = TempDir::new().unwrap();
        let t = OutputTarget::Directory(tmp.path().to_path_buf());
        let path = resolve_output_path(&t, "gpt-image-1", "png").unwrap();
        assert!(path.starts_with(tmp.path()));
    }

    #[test]
    fn resolve_directory_missing_errors() {
        let t = OutputTarget::Directory(PathBuf::from("/definitely/does/not/exist/aivo-test"));
        let err = resolve_output_path(&t, "gpt-image-1", "png").unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn resolve_template_with_model_token() {
        let t = OutputTarget::Template("{model}.png".into());
        let path = resolve_output_path(&t, "gpt-image-1", "png").unwrap();
        assert_eq!(path.to_string_lossy(), "gpt-image-1.png");
    }

    #[test]
    fn resolve_template_with_ts_token() {
        let t = OutputTarget::Template("shot-{ts}.png".into());
        let path = resolve_output_path(&t, "gpt-image-1", "png").unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("shot-"));
        assert!(name.ends_with(".png"));
    }

    #[test]
    fn resolve_template_checks_parent_directory() {
        let t = OutputTarget::Template("/definitely/does/not/exist/cat.png".into());
        let err = resolve_output_path(&t, "gpt-image-1", "png").unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn parent_if_nonempty_skips_bare_filename() {
        // `Path::parent()` returns Some("") for a bare filename — the helper
        // filters that out so callers don't run directory checks against "".
        assert!(parent_if_nonempty(Path::new("cat.png")).is_none());
        assert_eq!(
            parent_if_nonempty(Path::new("out/cat.png")),
            Some(Path::new("out"))
        );
        assert_eq!(
            parent_if_nonempty(Path::new("/abs/dir/cat.png")),
            Some(Path::new("/abs/dir"))
        );
    }

    #[test]
    fn sanitize_model_replaces_slashes_and_colons() {
        assert_eq!(sanitize_model("org/model"), "org_model");
        assert_eq!(sanitize_model("name:tag"), "name_tag");
        assert_eq!(sanitize_model("normal-name_1"), "normal-name_1");
    }

    #[test]
    fn pins_extension_distinguishes_user_vs_auto() {
        assert!(!OutputTarget::Default.pins_extension());
        assert!(!OutputTarget::Directory(PathBuf::from("out")).pins_extension());
        assert!(OutputTarget::File(PathBuf::from("cat.png")).pins_extension());
        assert!(!OutputTarget::File(PathBuf::from("cat")).pins_extension());
        assert!(OutputTarget::Template("{model}-{ts}.png".into()).pins_extension());
        assert!(!OutputTarget::Template("{model}-{ts}".into()).pins_extension());
    }

    #[test]
    fn align_extension_swaps_when_unpinned_and_mismatched() {
        assert_eq!(
            align_extension(Path::new("aivo.png"), Some("image/jpeg"), false),
            PathBuf::from("aivo.jpg")
        );
        assert_eq!(
            align_extension(Path::new("out/aivo.png"), Some("image/webp"), false),
            PathBuf::from("out/aivo.webp")
        );
    }

    #[test]
    fn align_extension_keeps_user_pinned_path() {
        assert_eq!(
            align_extension(Path::new("cat.png"), Some("image/jpeg"), true),
            PathBuf::from("cat.png")
        );
    }

    #[test]
    fn align_extension_noop_when_matched_or_unknown_ct() {
        assert_eq!(
            align_extension(Path::new("aivo.png"), Some("image/png"), false),
            PathBuf::from("aivo.png")
        );
        assert_eq!(
            align_extension(Path::new("aivo.png"), None, false),
            PathBuf::from("aivo.png")
        );
    }

    #[test]
    fn next_free_path_finds_suffix() {
        let tmp = TempDir::new().unwrap();
        let existing = tmp.path().join("cat.png");
        fs::write(&existing, b"x").unwrap();
        let free = next_free_path(&existing);
        assert_eq!(free, tmp.path().join("cat-1.png"));

        fs::write(tmp.path().join("cat-1.png"), b"x").unwrap();
        assert_eq!(next_free_path(&existing), tmp.path().join("cat-2.png"));
    }

    #[test]
    fn ext_from_content_type_maps_known_types() {
        assert_eq!(ext_from_content_type(Some("image/jpeg")), "jpg");
        assert_eq!(ext_from_content_type(Some("image/jpg")), "jpg");
        assert_eq!(ext_from_content_type(Some("image/webp")), "webp");
        assert_eq!(ext_from_content_type(Some("image/gif")), "gif");
        assert_eq!(ext_from_content_type(Some("image/png")), "png");
        assert_eq!(ext_from_content_type(Some("application/json")), "png"); // fallback
        assert_eq!(ext_from_content_type(None), "png");
    }

    #[test]
    fn ext_from_content_type_ignores_charset_suffix() {
        assert_eq!(ext_from_content_type(Some("image/jpeg; charset=x")), "jpg");
    }

    #[test]
    fn overwrite_policy_force_writes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a.png");
        fs::write(&path, b"old").unwrap();
        let d = apply_overwrite_policy(
            &path,
            OverwritePolicy {
                force: true,
                interactive: false,
            },
            None,
        );
        assert_eq!(d, OverwriteDecision::Write(path));
    }

    #[test]
    fn overwrite_policy_nontty_aborts() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a.png");
        fs::write(&path, b"old").unwrap();
        let d = apply_overwrite_policy(
            &path,
            OverwritePolicy {
                force: false,
                interactive: false,
            },
            None,
        );
        assert_eq!(d, OverwriteDecision::Abort);
    }

    #[test]
    fn overwrite_policy_missing_file_writes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nope.png");
        let d = apply_overwrite_policy(
            &path,
            OverwritePolicy {
                force: false,
                interactive: false,
            },
            None,
        );
        assert_eq!(d, OverwriteDecision::Write(path));
    }

    #[test]
    fn overwrite_policy_yes_answer_writes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a.png");
        fs::write(&path, b"old").unwrap();
        let d = apply_overwrite_policy(
            &path,
            OverwritePolicy {
                force: false,
                interactive: true,
            },
            Some('y'),
        );
        assert_eq!(d, OverwriteDecision::Write(path));
    }

    #[test]
    fn overwrite_policy_no_answer_aborts() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a.png");
        fs::write(&path, b"old").unwrap();
        let d = apply_overwrite_policy(
            &path,
            OverwritePolicy {
                force: false,
                interactive: true,
            },
            Some('n'),
        );
        assert_eq!(d, OverwriteDecision::Abort);
    }

    #[test]
    fn overwrite_policy_skip_finds_free_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a.png");
        fs::write(&path, b"old").unwrap();
        let d = apply_overwrite_policy(
            &path,
            OverwritePolicy {
                force: false,
                interactive: true,
            },
            Some('s'),
        );
        match d {
            OverwriteDecision::Write(p) => {
                assert_eq!(p, tmp.path().join("a-1.png"));
            }
            _ => panic!("expected Write with suffixed path"),
        }
    }

    #[test]
    fn atomic_write_produces_final_file_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.png");
        let n = atomic_write(&path, b"hello").unwrap();
        assert_eq!(n, 5);
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        // No stray tmp files left behind
        let dir_entries: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(dir_entries.len(), 1);
        assert_eq!(dir_entries[0], "out.png");
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.png");
        fs::write(&path, b"old").unwrap();
        let n = atomic_write(&path, b"new bytes").unwrap();
        assert_eq!(n, 9);
        assert_eq!(fs::read(&path).unwrap(), b"new bytes");
    }

    #[test]
    fn extract_error_message_reads_openai_shape() {
        let body = r#"{"error":{"message":"bad prompt","type":"invalid"}}"#;
        assert_eq!(extract_error_message(body).as_deref(), Some("bad prompt"));
    }

    #[test]
    fn extract_error_message_reads_flat_shape() {
        let body = r#"{"message":"rate limited"}"#;
        assert_eq!(extract_error_message(body).as_deref(), Some("rate limited"));
    }

    #[test]
    fn extract_error_message_returns_none_for_plain_text() {
        assert!(extract_error_message("not json").is_none());
    }
}
