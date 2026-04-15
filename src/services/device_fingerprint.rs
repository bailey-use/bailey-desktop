//! Device fingerprinting and request signing for the aivo starter endpoint.
//!
//! Provides a privacy-preserving device identifier (SHA-256 of hardware UUID)
//! and per-request signatures that prevent trivial URL redistribution.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

use crate::constants::AIVO_STARTER_SIGNING_KEY;
use crate::services::http_utils::current_unix_ts;
use crate::services::system_env;
use crate::version::VERSION;

type HmacSha256 = Hmac<Sha256>;

static DEVICE_ID: OnceLock<String> = OnceLock::new();

/// Cached SHA-256 of `machine_id()`. Falls back to hash of "unknown" in VMs/containers.
pub fn device_id() -> &'static str {
    DEVICE_ID.get_or_init(|| {
        let raw = system_env::machine_id().unwrap_or_else(|| "unknown".to_string());
        hex_sha256(raw.as_bytes())
    })
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
}
