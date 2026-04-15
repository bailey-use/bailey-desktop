//! Normalized thread type + age constant for the context pipeline.
//!
//! Context is stateless — extracted threads live in memory only, reconstructed
//! from claude/codex session files and `logs.db` on every invocation. This
//! module holds the shared primitives used across the ingest/render pipeline.

use chrono::{DateTime, Utc};

/// Default: threads older than this are filtered out at read time. Age
/// filtering is lazy (no persistent GC needed). Users can override with
/// `--last-days=<N>` or bypass entirely with `--all`.
pub const DEFAULT_THREAD_MAX_AGE_DAYS: i64 = 14;

/// A normalized conversational thread: one session summarized into a first
/// user "topic" and a last assistant "last_response". In-memory only.
#[derive(Debug, Clone)]
pub struct Thread {
    /// Which CLI produced the session: "claude" | "codex" | "chat" | ...
    pub cli: String,
    /// Native session id (Claude UUID, Codex rollout id, aivo chat session id).
    pub session_id: String,
    /// Provenance: JSONL path or `log://<session_id>` for chat-from-logs.
    pub source_path: String,
    /// First substantive user message in the session.
    pub topic: String,
    /// Last substantive assistant message.
    pub last_response: String,
    /// Session end timestamp (falls back to file mtime when the source lacks one).
    pub updated_at: DateTime<Utc>,
}
