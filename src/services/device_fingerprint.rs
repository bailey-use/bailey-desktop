//! Device fingerprinting and request signing for the aivo starter endpoint.
//!
//! Provides a privacy-preserving device identifier (SHA-256 of hardware UUID)
//! and per-request signatures that prevent trivial URL redistribution.

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::constants::AIVO_STARTER_SIGNING_KEY;
use crate::services::http_utils::current_unix_ts;
use crate::services::system_env;
use crate::version::VERSION;

type HmacSha256 = Hmac<Sha256>;

static DEVICE_ID: OnceLock<String> = OnceLock::new();

/// Stable per-install device identifier. Prefers a hardware machine ID
/// (macOS IOPlatformUUID, Linux /etc/machine-id, Windows MachineGuid). When
/// none is available — most notably on Termux, where /etc/machine-id is
/// absent — falls back to a random ID persisted at
/// `~/.config/aivo/device-id` so installs don't collide on a single hash.
pub fn device_id() -> &'static str {
    DEVICE_ID.get_or_init(|| {
        if let Some(raw) = system_env::machine_id() {
            return hex_sha256(raw.as_bytes());
        }
        load_or_create_persistent_device_id().unwrap_or_else(random_device_id)
    })
}

fn device_id_path() -> Option<PathBuf> {
    Some(
        system_env::home_dir()?
            .join(".config")
            .join("aivo")
            .join("device-id"),
    )
}

fn load_or_create_persistent_device_id() -> Option<String> {
    let path = device_id_path()?;
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(trimmed.to_string());
        }
    }
    let id = random_device_id();
    let parent = path.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    std::fs::write(&path, &id).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Some(id)
}

fn random_device_id() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// `HMAC-SHA256(signing_key, device_id:timestamp)` as lowercase hex.
pub fn sign_request(device_id: &str, timestamp: u64) -> String {
    let message = format!("{}:{}", device_id, timestamp);
    let mut mac =
        HmacSha256::new_from_slice(AIVO_STARTER_SIGNING_KEY.as_bytes()).expect("valid key length");
    mac.update(message.as_bytes());
    let result = mac.finalize().into_bytes();
    result.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Conditionally attaches device fingerprint headers when `is_starter` is true.
pub fn maybe_with_starter_headers(
    builder: reqwest::RequestBuilder,
    is_starter: bool,
) -> reqwest::RequestBuilder {
    if is_starter {
        with_starter_headers(builder)
    } else {
        builder
    }
}

/// Attaches device fingerprint headers to a request builder.
pub fn with_starter_headers(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let did = device_id();
    let ts = current_unix_ts();
    let sig = sign_request(did, ts);
    builder
        .header("X-Aivo-Device", did)
        .header("X-Aivo-Timestamp", ts.to_string())
        .header("X-Aivo-Signature", sig)
        .header("X-Aivo-Version", VERSION)
}

pub(crate) fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_64_char_hex() {
        let id = device_id();
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn device_id_is_stable() {
        assert_eq!(device_id(), device_id());
    }

    #[test]
    fn sign_request_produces_64_char_hex() {
        let sig = sign_request("abc123", 1700000000);
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sign_request_is_deterministic() {
        assert_eq!(
            sign_request("device1", 1700000000),
            sign_request("device1", 1700000000),
        );
    }

    #[test]
    fn sign_request_varies_with_timestamp() {
        assert_ne!(
            sign_request("device1", 1700000000),
            sign_request("device1", 1700000001),
        );
    }

    #[test]
    fn sign_request_varies_with_device() {
        assert_ne!(
            sign_request("device1", 1700000000),
            sign_request("device2", 1700000000),
        );
    }

    #[test]
    fn random_device_id_is_64_char_hex_and_varies() {
        let a = random_device_id();
        let b = random_device_id();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "32 random bytes should not collide");
    }

    #[test]
    fn random_device_id_is_not_unknown_hash() {
        // sha256("unknown") was the shared fallback that every Termux install
        // collided on. The random path must never produce it.
        assert_ne!(
            random_device_id(),
            "b23a6a8439c0dde5515893e7c90c1e3233b8616e634470f20dc4928bcf3609bc"
        );
    }
}
