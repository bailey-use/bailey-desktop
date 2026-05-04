//! Persistent on-disk cache for `aivo speak` TTS output.
//!
//! Cache files live under `<config_dir>/audio/<hash>.<ext>`. The hash is
//! derived from every input field that materially affects the generated
//! bytes — text, voice, model, format, speed — so a change in any one
//! produces a different cache entry. The leading `v1\n` lets us bump the
//! cache schema later without renaming existing files.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Subdirectory of the aivo config dir that holds cached TTS files.
const AUDIO_CACHE_SUBDIR: &str = "audio";
/// Schema version baked into every hash. Bump to invalidate the cache.
const HASH_SCHEMA: &str = "v1";

/// Inputs that determine TTS output bytes. Two requests with equal
/// `CacheKey`s should produce byte-identical audio from the same provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    pub text: String,
    pub voice: String,
    pub model: String,
    pub format: String,
    pub speed: String,
}

impl CacheKey {
    /// Builds a cache key from the same fields used by `AudioRequest`.
    /// `Option`s collapse to empty strings so "voice unset" is one cache
    /// slot rather than scattered across providers' default-voice names.
    pub fn from_inputs(
        text: &str,
        voice: Option<&str>,
        model: &str,
        format: Option<&str>,
        speed: Option<f32>,
    ) -> Self {
        Self {
            text: text.trim().to_string(),
            voice: voice.unwrap_or("").to_string(),
            model: model.to_string(),
            format: format.unwrap_or("").to_ascii_lowercase(),
            speed: speed.map(format_speed).unwrap_or_default(),
        }
    }
}

/// Stable f32 → string conversion. `{:?}` produces a round-trippable form
/// (e.g. `1.0` not `1`), so two equal floats always hash to the same key.
fn format_speed(s: f32) -> String {
    format!("{s:?}")
}

/// `<config_dir>/audio/`.
pub fn audio_cache_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(AUDIO_CACHE_SUBDIR)
}

/// `<config_dir>/audio/<hash>.<ext>`.
pub fn cache_path(cache_dir: &Path, key: &CacheKey, ext: &str) -> PathBuf {
    cache_dir.join(format!("{}.{}", hash_key(key), ext))
}

/// Hex-encoded SHA-256 over a versioned, newline-joined serialization of
/// the cache fields. Newlines inside `text` are fine — the field order is
/// fixed and every other field is a short identifier without newlines, so
/// the serialization is unambiguous.
pub fn hash_key(key: &CacheKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(HASH_SCHEMA.as_bytes());
    hasher.update(b"\n");
    hasher.update(key.text.as_bytes());
    hasher.update(b"\n");
    hasher.update(key.voice.as_bytes());
    hasher.update(b"\n");
    hasher.update(key.model.as_bytes());
    hasher.update(b"\n");
    hasher.update(key.format.as_bytes());
    hasher.update(b"\n");
    hasher.update(key.speed.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> CacheKey {
        CacheKey::from_inputs(text, Some("nova"), "tts-1", Some("mp3"), Some(1.0))
    }

    #[test]
    fn hash_is_deterministic() {
        assert_eq!(hash_key(&key("hello")), hash_key(&key("hello")));
    }

    #[test]
    fn hash_trims_text() {
        assert_eq!(hash_key(&key("  hello  ")), hash_key(&key("hello")));
    }

    #[test]
    fn hash_changes_with_text() {
        assert_ne!(hash_key(&key("hello")), hash_key(&key("hello world")));
    }

    #[test]
    fn hash_changes_with_voice() {
        let a = CacheKey::from_inputs("hi", Some("nova"), "tts-1", None, None);
        let b = CacheKey::from_inputs("hi", Some("alloy"), "tts-1", None, None);
        assert_ne!(hash_key(&a), hash_key(&b));
    }

    #[test]
    fn hash_changes_with_model() {
        let a = CacheKey::from_inputs("hi", None, "tts-1", None, None);
        let b = CacheKey::from_inputs("hi", None, "tts-1-hd", None, None);
        assert_ne!(hash_key(&a), hash_key(&b));
    }

    #[test]
    fn hash_changes_with_format() {
        let a = CacheKey::from_inputs("hi", None, "tts-1", Some("mp3"), None);
        let b = CacheKey::from_inputs("hi", None, "tts-1", Some("wav"), None);
        assert_ne!(hash_key(&a), hash_key(&b));
    }

    #[test]
    fn hash_changes_with_speed() {
        let a = CacheKey::from_inputs("hi", None, "tts-1", None, Some(1.0));
        let b = CacheKey::from_inputs("hi", None, "tts-1", None, Some(1.5));
        assert_ne!(hash_key(&a), hash_key(&b));
    }

    #[test]
    fn format_is_case_insensitive() {
        let a = CacheKey::from_inputs("hi", None, "tts-1", Some("MP3"), None);
        let b = CacheKey::from_inputs("hi", None, "tts-1", Some("mp3"), None);
        assert_eq!(hash_key(&a), hash_key(&b));
    }

    #[test]
    fn unset_voice_and_default_voice_differ() {
        // An empty voice (None) is its own slot, distinct from any
        // explicitly-named voice.
        let a = CacheKey::from_inputs("hi", None, "tts-1", None, None);
        let b = CacheKey::from_inputs("hi", Some("alloy"), "tts-1", None, None);
        assert_ne!(hash_key(&a), hash_key(&b));
    }

    #[test]
    fn cache_path_joins_dir_and_filename() {
        let dir = Path::new("/tmp/aivo-test/audio");
        let key = key("hello");
        let path = cache_path(dir, &key, "mp3");
        assert_eq!(path.parent(), Some(dir));
        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap();
        let hash = hash_key(&key);
        assert_eq!(file_name, format!("{hash}.mp3"));
    }

    #[test]
    fn audio_cache_dir_appends_subdir() {
        let dir = audio_cache_dir(Path::new("/tmp/aivo-test"));
        assert_eq!(dir, PathBuf::from("/tmp/aivo-test/audio"));
    }
}
