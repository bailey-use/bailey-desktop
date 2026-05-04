//! Audio (TTS) generation service.
//!
//! Handles OpenAI-compatible `/v1/audio/speech` (raw bytes back) and Google's
//! Gemini TTS surface (`:generateContent` with `responseModalities: ["AUDIO"]`,
//! base64 audio in the response). Output UX (path parsing, overwrite policy,
//! atomic writes, error extraction) lives in [`super::media_io`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use futures::StreamExt;
use serde_json::{Value, json};

use crate::services::http_utils::router_http_client;
use crate::services::media_io::{align_extension, atomic_write, extract_error_message};
use crate::services::provider_protocol::{ProviderProtocol, detect_provider_protocol};
use crate::services::session_store::ApiKey;

/// Sample rate of OpenAI's `response_format: "pcm"` output. Documented as
/// 24 kHz, 16-bit signed little-endian, mono — matches what `gpt-4o-mini-tts`
/// and `tts-1*` emit. Gemini's L16 audio uses the same shape after we strip
/// its `audio/L16; rate=24000` envelope.
pub const PCM_STREAM_RATE_HZ: u32 = 24_000;
pub const PCM_STREAM_CHANNELS: u16 = 1;
pub const PCM_STREAM_BITS: u16 = 16;

/// One TTS request.
#[derive(Debug, Clone)]
pub struct AudioRequest {
    pub prompt: String,
    pub model: String,
    pub voice: Option<String>,
    /// Provider-side `response_format`: mp3 | wav | opus | aac | flac | pcm.
    pub format: Option<String>,
    /// Speech rate. OpenAI accepts 0.25–4.0; other providers may vary.
    pub speed: Option<f32>,
}

/// One generated audio file (or in-memory bytes when no `path`).
#[derive(Debug, Clone)]
pub struct AudioArtifact {
    pub path: Option<PathBuf>,
    pub bytes: u64,
}

/// Maps an audio HTTP `Content-Type` header to a file extension. Falls
/// back to `"mp3"` for unrecognized values — the OpenAI default response
/// format. The PCM family includes Gemini's `audio/L16` and `audio/L24`.
pub fn ext_from_content_type(ct: Option<&str>) -> String {
    match ct.map(|c| {
        c.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    }) {
        Some(ref s) if s == "audio/mpeg" || s == "audio/mp3" => "mp3".into(),
        Some(ref s) if s == "audio/wav" || s == "audio/x-wav" || s == "audio/wave" => "wav".into(),
        Some(ref s) if s == "audio/opus" => "opus".into(),
        Some(ref s) if s == "audio/ogg" => "ogg".into(),
        Some(ref s) if s == "audio/aac" || s == "audio/x-aac" => "aac".into(),
        Some(ref s) if s == "audio/flac" || s == "audio/x-flac" => "flac".into(),
        Some(ref s)
            if s == "audio/pcm" || s.starts_with("audio/l16") || s.starts_with("audio/l24") =>
        {
            "pcm".into()
        }
        _ => "mp3".into(),
    }
}

/// Top-level TTS entry point. Picks the protocol from `key.base_url`, builds
/// the request, and writes the result atomically when `path` is set. When
/// `pinned_extension` is false, the caller chose the extension (e.g. the
/// default `.mp3`) and we may swap it to the server's actual content-type
/// suffix silently. When true, the user's extension is honored.
pub async fn generate(
    key: &ApiKey,
    request: &AudioRequest,
    path: Option<&Path>,
    pinned_extension: bool,
) -> Result<AudioArtifact> {
    let protocol = detect_provider_protocol(&key.base_url);
    match protocol {
        ProviderProtocol::Openai | ProviderProtocol::ResponsesApi => {
            generate_openai(key, request, path, pinned_extension).await
        }
        ProviderProtocol::Google => generate_google(key, request, path, pinned_extension).await,
        ProviderProtocol::Anthropic => bail!("Anthropic does not support text-to-speech"),
    }
}

/// Streams OpenAI's `/v1/audio/speech` as raw PCM, calling `on_chunk` for
/// each network chunk so the caller can begin playback before the full
/// response arrives. Returns the accumulated PCM bytes (no WAV header).
///
/// The provider request always uses `response_format: "pcm"` — that's
/// what makes streaming worthwhile, since PCM has no decoder framing and
/// any chunk boundary is a safe playback boundary. Callers wrap the
/// returned bytes with `wrap_pcm_as_wav` before persisting to disk.
///
/// `on_chunk` returns `false` to signal the caller no longer wants more
/// audio (e.g. the user quit playback). On false, the function returns
/// early with whatever PCM has accumulated so far — partial bytes the
/// caller should *not* persist.
pub async fn stream_openai_pcm<F>(
    key: &ApiKey,
    request: &AudioRequest,
    mut on_chunk: F,
) -> Result<Vec<u8>>
where
    F: FnMut(&[u8]) -> bool,
{
    let base = key.base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/audio/speech")
    } else {
        format!("{base}/v1/audio/speech")
    };

    let mut body = json!({
        "model": request.model,
        "input": request.prompt,
        "voice": request
            .voice
            .clone()
            .unwrap_or_else(|| "alloy".to_string()),
        "response_format": "pcm",
        // `stream: true` is a no-op on legacy `tts-1` (server still buffers
        // before sending) but enables true server-side streaming on the
        // newer `gpt-4o*-tts` family. Sending it always is harmless.
        "stream": true,
    });
    if let Some(speed) = request.speed {
        body["speed"] = json!(speed);
    }

    let client = router_http_client();
    let response = client
        .post(&url)
        .bearer_auth(key.key.as_str())
        .json(&body)
        .send()
        .await
        .with_context(|| format!("audio request to {url} failed"))?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let detail = extract_error_message(&text).unwrap_or_else(|| text.clone());
        bail!("audio generation failed ({}): {}", status.as_u16(), detail);
    }

    // Pre-size for ~10 seconds of mono 16-bit / 24 kHz so a typical short
    // clip never reallocates. Long prompts will grow naturally.
    let mut accumulated = Vec::with_capacity(48_000 * 10);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading audio chunk failed")?;
        if chunk.is_empty() {
            continue;
        }
        let keep_going = on_chunk(&chunk);
        accumulated.extend_from_slice(&chunk);
        if !keep_going {
            // Caller no longer wants audio (e.g. user quit). Drop the
            // remaining stream — reqwest closes the connection on drop.
            break;
        }
    }
    Ok(accumulated)
}

async fn generate_openai(
    key: &ApiKey,
    request: &AudioRequest,
    path: Option<&Path>,
    pinned_extension: bool,
) -> Result<AudioArtifact> {
    let base = key.base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/audio/speech")
    } else {
        format!("{base}/v1/audio/speech")
    };

    let mut body = json!({
        "model": request.model,
        "input": request.prompt,
        // OpenAI requires `voice`; default to alloy if user didn't pick one.
        "voice": request
            .voice
            .clone()
            .unwrap_or_else(|| "alloy".to_string()),
    });
    if let Some(fmt) = &request.format {
        body["response_format"] = Value::String(fmt.clone());
    }
    if let Some(speed) = request.speed {
        body["speed"] = json!(speed);
    }

    let client = router_http_client();
    let response = client
        .post(&url)
        .bearer_auth(key.key.as_str())
        .json(&body)
        .send()
        .await
        .with_context(|| format!("audio request to {url} failed"))?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let detail = extract_error_message(&text).unwrap_or_else(|| text.clone());
        bail!("audio generation failed ({}): {}", status.as_u16(), detail);
    }

    let ct = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string);
    let raw_bytes = response
        .bytes()
        .await
        .context("reading audio body failed")?
        .to_vec();

    // OpenAI's `response_format: "pcm"` returns raw 24kHz/16-bit/mono LE
    // PCM with `Content-Type: audio/pcm` — same wrap-into-WAV path as
    // Gemini. Other formats pass through untouched.
    finalize_audio(raw_bytes, ct, path, pinned_extension)
}

async fn generate_google(
    key: &ApiKey,
    request: &AudioRequest,
    path: Option<&Path>,
    pinned_extension: bool,
) -> Result<AudioArtifact> {
    let url = google_endpoint(&key.base_url, &request.model);
    let body = build_google_audio_body(&request.prompt, request.voice.as_deref());

    let client = router_http_client();
    let response = client
        .post(&url)
        .header("x-goog-api-key", key.key.as_str())
        .json(&body)
        .send()
        .await
        .with_context(|| format!("audio request to {url} failed"))?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let detail = extract_error_message(&text).unwrap_or_else(|| text.clone());
        bail!("audio generation failed ({}): {}", status.as_u16(), detail);
    }

    let parsed: Value = response
        .json()
        .await
        .context("failed to decode Google audio response")?;
    let (raw_bytes, mime) = decode_google_audio_response(&parsed)?;

    // Gemini TTS replies with raw little-endian PCM (MIME `audio/L16;
    // codec=pcm; rate=24000`). That's not a self-contained audio file, so
    // afplay/aplay/SoundPlayer all reject it (`AudioFileOpen failed
    // ('typ?')` is afplay's flavour of "I can't tell what this is"). Wrap
    // it in a 44-byte WAV header so callers get something playable
    // regardless of `--format`.
    finalize_audio(raw_bytes, mime, path, pinned_extension)
}

/// Shared post-decode flow for both providers: optionally wrap raw PCM in
/// a WAV header, then either save the bytes to `path` (with extension
/// alignment) or return them in-memory.
fn finalize_audio(
    raw_bytes: Vec<u8>,
    mime: Option<String>,
    path: Option<&Path>,
    pinned_extension: bool,
) -> Result<AudioArtifact> {
    let (bytes, effective_mime) = maybe_wrap_pcm(raw_bytes, mime);
    let Some(target_path) = path else {
        return Ok(AudioArtifact {
            path: None,
            bytes: bytes.len() as u64,
        });
    };
    let server_ext = effective_mime
        .as_deref()
        .map(|m| ext_from_content_type(Some(m)));
    let final_path = align_extension(target_path, server_ext.as_deref(), pinned_extension);
    let written = atomic_write(&final_path, &bytes)?;
    Ok(AudioArtifact {
        path: Some(final_path),
        bytes: written,
    })
}

/// If the response is raw PCM (Gemini's `audio/L16` family, OpenAI's
/// `audio/pcm`), prepend a 44-byte WAV header so the saved file is a
/// self-contained audio file. Returns `(bytes, effective_mime)` — the
/// MIME shifts to `audio/wav` after wrapping so downstream extension
/// resolution lands on `.wav`.
///
/// Non-PCM responses (mp3, opus, aac, flac, an already-wrapped wav)
/// pass through unchanged.
fn maybe_wrap_pcm(bytes: Vec<u8>, mime: Option<String>) -> (Vec<u8>, Option<String>) {
    if let Some((rate, bits)) = parse_pcm_params(mime.as_deref()) {
        let wav = wrap_pcm_as_wav(&bytes, rate, 1, bits);
        return (wav, Some("audio/wav".to_string()));
    }
    (bytes, mime)
}

/// Recognize the raw-PCM MIME family Gemini TTS returns and pull the
/// `rate` parameter out of it. Returns `(sample_rate_hz, bits_per_sample)`
/// when the MIME is one we know how to wrap; `None` otherwise (callers
/// then save the bytes verbatim).
///
/// Examples:
/// - `audio/L16; codec=pcm; rate=24000` → `Some((24000, 16))`
/// - `audio/L24; rate=48000` → `Some((48000, 24))`
/// - `audio/pcm` (no rate) → `Some((24000, 16))` (Gemini's documented default)
/// - `audio/mpeg` → `None`
fn parse_pcm_params(mime: Option<&str>) -> Option<(u32, u16)> {
    let s = mime?;
    let mut iter = s.split(';');
    let head = iter.next()?.trim().to_ascii_lowercase();
    let bits = match head.as_str() {
        "audio/l16" | "audio/pcm" => 16u16,
        "audio/l24" => 24,
        _ => return None,
    };
    let mut rate: u32 = 24_000;
    for part in iter {
        let p = part.trim();
        if let Some(value) = p.strip_prefix("rate=")
            && let Ok(parsed) = value.trim().parse::<u32>()
        {
            rate = parsed;
        }
    }
    Some((rate, bits))
}

/// Prepends a minimal 44-byte WAV (RIFF/PCM) header to little-endian raw
/// PCM samples. Output is `pcm.len() + 44` bytes. Channels is the actual
/// channel count (1 = mono, 2 = stereo); Gemini's TTS output is always
/// mono so callers pass `1`.
pub(crate) fn wrap_pcm_as_wav(
    pcm: &[u8],
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
) -> Vec<u8> {
    let bytes_per_sample = bits_per_sample / 8;
    let byte_rate = sample_rate * u32::from(channels) * u32::from(bytes_per_sample);
    let block_align = channels * bytes_per_sample;
    let data_size = pcm.len() as u32;
    // RIFF chunk size = total file size - 8 (the "RIFF" + size fields).
    // Header (after "RIFF" + size) = 4 ("WAVE") + 8 ("fmt " + size) + 16
    // (fmt body) + 8 ("data" + size) = 36, plus the data payload.
    let chunk_size = 36u32.saturating_add(data_size);

    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&chunk_size.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt subchunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM format tag
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

/// Build `{base}/v1beta/models/{model}:generateContent`. Same shape as
/// the image side — tolerates trailing slash and `/v1beta` suffix on the
/// stored base URL.
fn google_endpoint(base_url: &str, model: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1beta").unwrap_or(trimmed);
    format!("{root}/v1beta/models/{model}:generateContent")
}

/// Default prebuilt Gemini TTS voice used when the user didn't pass
/// `--voice`. Gemini's TTS endpoint *requires* a voice — leaving
/// `speechConfig` out trips an `INVALID_ARGUMENT` on
/// `tts_voice_info.selected_voice_name`. `Kore` is one of the long-stable
/// prebuilt voices Google documents for the `gemini-*-tts` models; pick it
/// over `Aoede`/`Puck`/etc. as a defensible "no surprises" default.
const GEMINI_DEFAULT_VOICE: &str = "Kore";

/// Build the JSON body for Google's Gemini TTS. Always sets
/// `responseModalities = ["AUDIO"]` and *always* emits
/// `speechConfig.voiceConfig` — Gemini errors without one. Falls back to
/// [`GEMINI_DEFAULT_VOICE`] when `--voice` wasn't supplied.
fn build_google_audio_body(prompt: &str, voice: Option<&str>) -> Value {
    let voice_name = voice
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(GEMINI_DEFAULT_VOICE);
    let generation_config = json!({
        "responseModalities": ["AUDIO"],
        "speechConfig": {
            "voiceConfig": {
                "prebuiltVoiceConfig": {
                    "voiceName": voice_name,
                }
            }
        }
    });
    json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": generation_config,
    })
}

/// Decode a Gemini TTS (`:generateContent`) response. Walks
/// `candidates[0].content.parts[]` and returns the first audio part's
/// `(decoded_bytes, mime_type)`. Tolerates both snake_case and camelCase
/// for `inline_data` / `inlineData` and `mime_type` / `mimeType`.
fn decode_google_audio_response(body: &Value) -> Result<(Vec<u8>, Option<String>)> {
    let parts = body
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow!("Google response missing candidates[0].content.parts"))?;

    for part in parts {
        let inline = part.get("inline_data").or_else(|| part.get("inlineData"));
        let Some(inline) = inline else { continue };
        let data = inline
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| anyhow!("Google inline_data/inlineData missing 'data' field"))?;
        let mime = inline
            .get("mime_type")
            .or_else(|| inline.get("mimeType"))
            .and_then(|m| m.as_str())
            .map(str::to_string);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("failed to decode base64 audio payload from Google response")?;
        return Ok((bytes, mime));
    }
    bail!("Google response contained no audio (no inline_data/inlineData part)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_from_content_type_maps_known_audio_types() {
        assert_eq!(ext_from_content_type(Some("audio/mpeg")), "mp3");
        assert_eq!(ext_from_content_type(Some("audio/mp3")), "mp3");
        assert_eq!(ext_from_content_type(Some("audio/wav")), "wav");
        assert_eq!(ext_from_content_type(Some("audio/x-wav")), "wav");
        assert_eq!(ext_from_content_type(Some("audio/wave")), "wav");
        assert_eq!(ext_from_content_type(Some("audio/opus")), "opus");
        assert_eq!(ext_from_content_type(Some("audio/ogg")), "ogg");
        assert_eq!(ext_from_content_type(Some("audio/aac")), "aac");
        assert_eq!(ext_from_content_type(Some("audio/flac")), "flac");
    }

    #[test]
    fn ext_from_content_type_handles_pcm_family() {
        assert_eq!(ext_from_content_type(Some("audio/pcm")), "pcm");
        // Gemini emits "audio/L16; codec=pcm; rate=24000"; we strip the
        // parameters via the `;` split before matching.
        assert_eq!(
            ext_from_content_type(Some("audio/L16; codec=pcm; rate=24000")),
            "pcm"
        );
        assert_eq!(ext_from_content_type(Some("audio/l24")), "pcm");
    }

    #[test]
    fn ext_from_content_type_falls_back_to_mp3() {
        assert_eq!(ext_from_content_type(None), "mp3");
        assert_eq!(ext_from_content_type(Some("application/json")), "mp3");
        assert_eq!(ext_from_content_type(Some("garbage")), "mp3");
    }

    #[test]
    fn ext_from_content_type_ignores_charset_suffix() {
        assert_eq!(ext_from_content_type(Some("audio/mpeg; charset=x")), "mp3");
    }

    #[test]
    fn google_endpoint_uses_v1beta_models_path() {
        assert_eq!(
            google_endpoint(
                "https://generativelanguage.googleapis.com",
                "gemini-2.5-flash-preview-tts",
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-preview-tts:generateContent"
        );
    }

    #[test]
    fn google_endpoint_strips_trailing_slash_and_v1beta_suffix() {
        let bare = google_endpoint("https://generativelanguage.googleapis.com/", "gemini-tts");
        let with_v1beta = google_endpoint(
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-tts",
        );
        let with_trailing = google_endpoint(
            "https://generativelanguage.googleapis.com/v1beta/",
            "gemini-tts",
        );
        let expected =
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-tts:generateContent";
        assert_eq!(bare, expected);
        assert_eq!(with_v1beta, expected);
        assert_eq!(with_trailing, expected);
    }

    #[test]
    fn build_google_audio_body_sets_audio_modality() {
        let body = build_google_audio_body("hello world", None);
        assert_eq!(
            body["contents"][0]["parts"][0]["text"],
            Value::String("hello world".into())
        );
        assert_eq!(
            body["generationConfig"]["responseModalities"],
            json!(["AUDIO"])
        );
    }

    #[test]
    fn build_google_audio_body_uses_default_voice_when_unset() {
        // Gemini TTS *requires* a voice — leaving speechConfig out trips
        // INVALID_ARGUMENT (`tts_voice_info.selected_voice_name must not be
        // empty`). Confirm the default voice is wired in.
        let body = build_google_audio_body("hi", None);
        assert_eq!(
            body["generationConfig"]["speechConfig"]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
            Value::String(GEMINI_DEFAULT_VOICE.into())
        );
    }

    #[test]
    fn build_google_audio_body_uses_default_when_voice_is_blank() {
        let body = build_google_audio_body("hi", Some("   "));
        assert_eq!(
            body["generationConfig"]["speechConfig"]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
            Value::String(GEMINI_DEFAULT_VOICE.into())
        );
    }

    #[test]
    fn build_google_audio_body_emits_voice_config_when_set() {
        let body = build_google_audio_body("hi", Some("Aoede"));
        assert_eq!(
            body["generationConfig"]["speechConfig"]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
            Value::String("Aoede".into())
        );
    }

    #[test]
    fn decode_google_audio_response_extracts_inline_data_snake_case() {
        let body = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"inline_data": {"mime_type": "audio/L16", "data": "aGVsbG8="}}
                    ]
                }
            }]
        });
        let (bytes, mime) = decode_google_audio_response(&body).unwrap();
        assert_eq!(bytes, b"hello");
        assert_eq!(mime.as_deref(), Some("audio/L16"));
    }

    #[test]
    fn decode_google_audio_response_handles_camel_case() {
        let body = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"inlineData": {"mimeType": "audio/mpeg", "data": "aGVsbG8="}}
                    ]
                }
            }]
        });
        let (bytes, mime) = decode_google_audio_response(&body).unwrap();
        assert_eq!(bytes, b"hello");
        assert_eq!(mime.as_deref(), Some("audio/mpeg"));
    }

    #[test]
    fn decode_google_audio_response_errors_when_no_audio_part() {
        let body = json!({
            "candidates": [{"content": {"parts": [{"text": "no audio for you"}]}}]
        });
        let err = decode_google_audio_response(&body).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("no audio"));
    }

    #[test]
    fn parse_pcm_params_reads_rate_from_gemini_mime() {
        // Real Gemini TTS Content-Type-like value.
        assert_eq!(
            parse_pcm_params(Some("audio/L16; codec=pcm; rate=24000")),
            Some((24000, 16))
        );
        assert_eq!(
            parse_pcm_params(Some("audio/L24; rate=48000")),
            Some((48000, 24))
        );
    }

    #[test]
    fn parse_pcm_params_defaults_rate_when_missing() {
        // Gemini's docs default to 24kHz; if the server omits `rate=`,
        // fall back to that rather than guessing wildly.
        assert_eq!(parse_pcm_params(Some("audio/L16")), Some((24000, 16)));
        assert_eq!(parse_pcm_params(Some("audio/pcm")), Some((24000, 16)));
    }

    #[test]
    fn parse_pcm_params_is_case_insensitive_on_head() {
        assert_eq!(
            parse_pcm_params(Some("AUDIO/L16; codec=pcm; rate=24000")),
            Some((24000, 16))
        );
    }

    #[test]
    fn parse_pcm_params_rejects_non_pcm_mimes() {
        assert!(parse_pcm_params(Some("audio/mpeg")).is_none());
        assert!(parse_pcm_params(Some("audio/wav")).is_none());
        assert!(parse_pcm_params(Some("audio/aac")).is_none());
        assert!(parse_pcm_params(None).is_none());
    }

    #[test]
    fn wrap_pcm_as_wav_produces_valid_riff_header() {
        let pcm: Vec<u8> = (0..100u8).collect();
        let wav = wrap_pcm_as_wav(&pcm, 24_000, 1, 16);

        // 44-byte header + payload.
        assert_eq!(wav.len(), 44 + pcm.len());
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");

        // RIFF chunk size = file_size - 8 = 36 + data_size.
        let chunk_size = u32::from_le_bytes(wav[4..8].try_into().unwrap());
        assert_eq!(chunk_size, 36 + pcm.len() as u32);

        // fmt subchunk: PCM=1, channels=1, sample_rate=24000.
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 24_000);
        // byte_rate = 24000 * 1 * 2 = 48000.
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 48_000);
        // block_align = 1 * 2 = 2.
        assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 2);
        // bits_per_sample = 16.
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);

        // data subchunk size = pcm.len().
        let data_size = u32::from_le_bytes(wav[40..44].try_into().unwrap());
        assert_eq!(data_size, pcm.len() as u32);

        // Payload preserved.
        assert_eq!(&wav[44..], pcm.as_slice());
    }

    #[test]
    fn maybe_wrap_pcm_wraps_openai_pcm_response() {
        // OpenAI returns `Content-Type: audio/pcm` (no rate parameter)
        // when `response_format: "pcm"`. We default to 24kHz/16-bit/mono
        // per OpenAI's documented PCM shape.
        let raw = vec![0xAB, 0xCDu8, 0xEF, 0x00];
        let (wrapped, mime) = maybe_wrap_pcm(raw.clone(), Some("audio/pcm".to_string()));
        assert_eq!(mime.as_deref(), Some("audio/wav"));
        assert_eq!(&wrapped[0..4], b"RIFF");
        assert_eq!(&wrapped[8..12], b"WAVE");
        assert_eq!(wrapped.len(), 44 + raw.len());
    }

    #[test]
    fn maybe_wrap_pcm_wraps_gemini_l16_response() {
        let raw = vec![0u8; 100];
        let (wrapped, mime) = maybe_wrap_pcm(
            raw.clone(),
            Some("audio/L16; codec=pcm; rate=24000".to_string()),
        );
        assert_eq!(mime.as_deref(), Some("audio/wav"));
        assert_eq!(wrapped.len(), 44 + raw.len());
    }

    #[test]
    fn maybe_wrap_pcm_passes_through_non_pcm() {
        // mp3, opus, aac, flac, and an already-wrapped wav must all be
        // returned untouched — their bytes are already self-contained.
        let raw = vec![0xFF, 0xFB, 0x90, 0x00]; // first bytes of an MP3 frame
        let (out, mime) = maybe_wrap_pcm(raw.clone(), Some("audio/mpeg".to_string()));
        assert_eq!(out, raw);
        assert_eq!(mime.as_deref(), Some("audio/mpeg"));

        let (out, mime) = maybe_wrap_pcm(raw.clone(), Some("audio/wav".to_string()));
        assert_eq!(out, raw);
        assert_eq!(mime.as_deref(), Some("audio/wav"));

        let (out, mime) = maybe_wrap_pcm(raw.clone(), None);
        assert_eq!(out, raw);
        assert_eq!(mime, None);
    }

    #[test]
    fn wrap_pcm_as_wav_handles_24bit_and_stereo() {
        let pcm = vec![0u8; 30]; // 5 stereo frames * (24/8 * 2) = 30 bytes.
        let wav = wrap_pcm_as_wav(&pcm, 48_000, 2, 24);
        // channels=2, bits=24 → block_align = 2 * 3 = 6, byte_rate = 48000*6 = 288000.
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 288_000);
        assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 6);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 24);
    }

    #[tokio::test]
    async fn generate_anthropic_protocol_bails_with_clear_message() {
        // Anthropic protocol keys have no TTS endpoint; fail before any HTTP.
        let key = ApiKey::new_with_protocol(
            "test".into(),
            "test".into(),
            "https://api.anthropic.com/v1".into(),
            None,
            "fake".into(),
        );
        let request = AudioRequest {
            prompt: "x".into(),
            model: "tts-1".into(),
            voice: None,
            format: None,
            speed: None,
        };
        let err = generate(&key, &request, None, false).await.unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("anthropic"),
            "got: {err}"
        );
    }
}
