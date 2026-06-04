//! Plugin self-description: the `--aivo-manifest` probe + the manifest schema.
//! A conforming `aivo-<name>` prints one JSON manifest on `--aivo-manifest` and
//! exits 0; legacy/non-conforming plugins fail the probe and are recorded without
//! a manifest. Frozen contract: `docs/PLUGIN-PROTOCOL.md`.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::style;

/// Plugin protocol version this host speaks; bumped only on a breaking change.
pub(crate) const PROTOCOL_VERSION: &str = "1";

/// Grace period for a plugin to print its manifest before the probe gives up.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// A plugin's self-description, captured at install/update and cached verbatim in
/// `.registry.json`. Unknown fields are ignored (forward-compatible). Capabilities
/// and hooks are disclosure-only in protocol v1 — see the spec for what's reserved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PluginManifest {
    pub name: String,
    pub version: String,
    /// Protocol the plugin targets; must equal `PROTOCOL_VERSION` to be honored.
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Requested capabilities — declared for disclosure; not yet enforced (P1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Reserved (P2 hooks); stored verbatim, not acted on in v1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
}

/// Parse a manifest from a plugin's `--aivo-manifest` stdout. Tolerant of leading
/// log noise: tries the whole trimmed output, then the last non-empty line. Yields
/// `None` unless the JSON parses *and* declares a supported protocol (an
/// unsupported protocol is treated as "no manifest" — the plugin still dispatches).
/// A name mismatch warns but is not fatal; the on-disk name stays authoritative.
pub(crate) fn parse_manifest(stdout: &str, expected_name: &str) -> Option<PluginManifest> {
    let manifest = serde_json::from_str::<PluginManifest>(stdout.trim())
        .ok()
        .or_else(|| {
            let last = stdout.lines().rev().find(|l| !l.trim().is_empty())?;
            serde_json::from_str::<PluginManifest>(last.trim()).ok()
        })?;

    if manifest.protocol != PROTOCOL_VERSION {
        eprintln!(
            "  {} plugin manifest targets protocol `{}` (this aivo speaks `{}`) — ignoring its declared roles/capabilities",
            style::yellow("!"),
            manifest.protocol,
            PROTOCOL_VERSION,
        );
        return None;
    }
    if manifest.name != expected_name {
        eprintln!(
            "  {} plugin manifest name `{}` differs from the installed name `{}` — keeping `{}`",
            style::yellow("!"),
            manifest.name,
            expected_name,
            expected_name,
        );
    }
    Some(manifest)
}

/// Run `bin --aivo-manifest` and parse its output. Best-effort: any spawn error,
/// timeout, non-zero exit, or unparseable output yields `None`, and the plugin is
/// then recorded without a manifest — a failed probe is never an install error.
pub(crate) async fn probe_manifest(bin: &Path, name: &str) -> Option<PluginManifest> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("--aivo-manifest")
        .env("AIVO_MANIFEST_PROBE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);

    // On timeout the future is dropped → kill_on_drop reaps the child.
    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_manifest(&String::from_utf8_lossy(&output.stdout), name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{"name":"amp","version":"0.1.0","protocol":"1",
        "roles":["subcommand"],"capabilities":["raw-key","spawn"]}"#;

    #[test]
    fn valid_manifest_parses() {
        let m = parse_manifest(VALID, "amp").expect("should parse");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.roles, ["subcommand"]);
        assert_eq!(m.capabilities, ["raw-key", "spawn"]);
    }

    #[test]
    fn unsupported_protocol_is_rejected() {
        let other = r#"{"name":"x","version":"1","protocol":"2"}"#;
        assert!(parse_manifest(other, "x").is_none());
    }

    #[test]
    fn garbage_is_none() {
        assert!(parse_manifest("not json at all", "x").is_none());
        assert!(parse_manifest("", "x").is_none());
        // Valid JSON, wrong shape (missing required fields) → None.
        assert!(parse_manifest(r#"{"hello":"world"}"#, "x").is_none());
    }

    #[test]
    fn name_mismatch_warns_but_parses() {
        let m = parse_manifest(VALID, "renamed").expect("name mismatch is non-fatal");
        assert_eq!(m.name, "amp");
    }

    #[test]
    fn manifest_after_log_noise_uses_last_line() {
        let noisy = format!("loading config...\nready\n{}", VALID.replace('\n', " "));
        let m = parse_manifest(&noisy, "amp").expect("last-line fallback");
        assert_eq!(m.version, "0.1.0");
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let extra = r#"{"name":"a","version":"1","protocol":"1","futureField":42}"#;
        assert!(parse_manifest(extra, "a").is_some());
    }
}
