//! Inline session picker for `aivo logs share` (no id passed).
//!
//! Builds a unified list of shareable sessions (chat + native CLI + amp),
//! awaits all loaders in parallel, then drives an inline `FuzzySelect`
//! prompt. No alternate screen, no preview pane — the picker renders ~12
//! lines in place and the selection stays in scrollback after exit.
//!
//! Sources mirror what `aivo logs` lists by default, minus `run`/`serve`
//! events (which aren't shareable conversations on their own):
//!   - chat sessions in the current cwd (from SessionStore index)
//!   - native CLI sessions in the current cwd (claude, codex, gemini, pi,
//!     opencode — via context_ingest)
//!   - amp threads (not cwd-keyed; included unfiltered, newest first)

use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::commands::chat::format_time_ago_short_dt;
use crate::commands::logs::trim_to_one_line;
use crate::services::amp_threads;
use crate::services::context_ingest::{self, IngestOptions};
use crate::services::id_compact::compact_id;
use crate::services::session_store::SessionStore;
use crate::style;
use crate::tui::FuzzySelect;

/// Width of the id column. Matches `aivo logs`'s ID_COL_WIDTH so prefixes
/// copied from the listing align with prefixes shown here.
const ID_COL_WIDTH: usize = 8;
/// Padded width of the `[source]` bracket column. Fits `[opencode]` (10).
const BRACKET_COL_WIDTH: usize = 10;
/// Newest-N cap for chat sessions. `all_chat_sessions` returns the full
/// on-disk index (can be thousands on long-lived installs), and inline
/// FuzzySelect pages 10 at a time — handing it 4k rows leaves the user
/// staring at "↓ N more below" until they type a filter. Older chats are
/// still reachable via an explicit id prefix.
const CHAT_PICKER_LIMIT: usize = 100;

#[derive(Debug, Clone)]
struct PickerItem {
    id: String,
    source: &'static str,
    title: String,
    updated_at: DateTime<Utc>,
}

/// Public entrypoint. `Ok(Some(id))` = user selected, `Ok(None)` = cancelled
/// or no items, `Err(_)` = setup or I/O failure.
pub async fn pick_session_id(
    session_store: &SessionStore,
    project_root: &Path,
    all: bool,
) -> Result<Option<String>> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        anyhow::bail!(
            "`aivo logs share` needs a terminal to show the picker. Pass an explicit session id, e.g. `aivo logs share <id>`."
        );
    }

    // Delayed spinner: cheap loads finish before the spinner shows, slow
    // ones get feedback. Mirrors `aivo logs`'s list path.
    const SPINNER_DELAY: Duration = Duration::from_millis(250);
    let load = load_items(session_store, project_root, all);
    tokio::pin!(load);
    let items = tokio::select! {
        items = &mut load => items,
        _ = tokio::time::sleep(SPINNER_DELAY) => {
            let (spinning, handle) = style::start_spinner(Some(" loading sessions…"));
            let items = (&mut load).await;
            style::stop_spinner(&spinning);
            let _ = handle.await;
            items
        }
    };

    if items.is_empty() {
        let scope = if all { "any project" } else { "this project" };
        println!(
            "{}",
            style::dim(format!("No shareable sessions found in {scope}."))
        );
        return Ok(None);
    }

    let prompt = if all {
        "Share which session? (all projects)".to_string()
    } else {
        "Share which session?".to_string()
    };

    // `FuzzySelect::interact_opt` blocks on `event::read()`. The aivo
    // runtime is current-thread, so do it on a blocking thread to keep
    // the runtime free for other futures (e.g. Ctrl+C handling).
    tokio::task::spawn_blocking(move || -> std::io::Result<Option<String>> {
        let labels: Vec<String> = items.iter().map(format_label).collect();
        let selected = FuzzySelect::new()
            .with_prompt(&prompt)
            .items(&labels)
            .default(0)
            .interact_opt()?;
        Ok(selected.map(|idx| items[idx].id.clone()))
    })
    .await
    .context("picker thread panicked")?
    .context("picker I/O failed")
}

async fn load_items(store: &SessionStore, project_root: &Path, all: bool) -> Vec<PickerItem> {
    let canonical_root = std::fs::canonicalize(project_root)
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .to_string();

    let native_opts = IngestOptions {
        max_age_days: None,
        min_updated_at: None,
        max_per_source: Some(50),
    };

    let (chat, native, amp) = tokio::join!(
        load_chat_items(store, &canonical_root, all),
        load_native_items(project_root, all, native_opts),
        load_amp_items(),
    );

    let mut all_items: Vec<PickerItem> = chat.into_iter().chain(native).chain(amp).collect();
    all_items.sort_by_key(|i| std::cmp::Reverse(i.updated_at));
    all_items
}

async fn load_chat_items(store: &SessionStore, canonical_root: &str, all: bool) -> Vec<PickerItem> {
    let Ok(entries) = store.all_chat_sessions().await else {
        return Vec::new();
    };
    // Filter → sort newest-first → cap. Sort/cap before mapping so we don't
    // allocate `PickerItem`s for entries that will be discarded.
    let mut filtered: Vec<_> = entries
        .into_iter()
        .filter(|e| all || e.cwd == canonical_root)
        .collect();
    filtered.sort_by_key(|e| std::cmp::Reverse(parse_rfc3339(&e.updated_at)));
    filtered.truncate(CHAT_PICKER_LIMIT);
    filtered
        .into_iter()
        .map(|entry| {
            let updated_at = parse_rfc3339(&entry.updated_at);
            let title = if entry.title.trim().is_empty() {
                entry.model
            } else {
                entry.title
            };
            PickerItem {
                id: entry.session_id,
                source: "chat",
                title,
                updated_at,
            }
        })
        .collect()
}

async fn load_native_items(project_root: &Path, all: bool, opts: IngestOptions) -> Vec<PickerItem> {
    let threads = if all {
        context_ingest::ingest_native_sessions_global(opts).await
    } else {
        context_ingest::ingest_project(project_root, opts).await
    };
    let Ok(threads) = threads else {
        return Vec::new();
    };
    threads
        .into_iter()
        .map(|thread| {
            let title = first_nonempty_line(&thread.topic).unwrap_or_else(|| thread.cli.clone());
            PickerItem {
                id: thread.session_id,
                source: source_label_for_cli(&thread.cli),
                title,
                updated_at: thread.updated_at,
            }
        })
        .collect()
}

async fn load_amp_items() -> Vec<PickerItem> {
    let amp_dir = amp_threads::default_threads_dir();
    amp_threads::list_threads(&amp_dir, 100)
        .await
        .into_iter()
        .filter_map(|value| {
            let id = value.get("id").and_then(|v| v.as_str())?.to_string();
            let updated_at = value
                .get("updatedAt")
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            let title = value
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "(amp thread)".to_string());
            Some(PickerItem {
                id,
                source: "amp",
                title,
                updated_at,
            })
        })
        .collect()
}

fn source_label_for_cli(cli: &str) -> &'static str {
    match cli {
        "claude" => "claude",
        "codex" => "codex",
        "gemini" => "gemini",
        "pi" => "pi",
        "opencode" => "opencode",
        _ => "native",
    }
}

fn parse_rfc3339(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn first_nonempty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|s| s.to_string())
}

/// Format one row as plain text. `FuzzySelect` paints the whole string in
/// cyan/dim based on highlight state, so no embedded ANSI here — fixed-width
/// columns carry the visual structure.
fn format_label(item: &PickerItem) -> String {
    let age = format_time_ago_short_dt(item.updated_at);
    let id_short = compact_id(&item.id, ID_COL_WIDTH);
    let bracket = format!("[{}]", item.source);
    // Title gets whatever room is left after age + id + bracket + 3 spaces.
    // Clamped so very wide terminals don't produce 200-char rows.
    let term_cols = console::Term::stdout().size().1 as usize;
    let prefix_width = 5 + 1 + ID_COL_WIDTH + 1 + BRACKET_COL_WIDTH + 1;
    let title_max = term_cols
        .saturating_sub(prefix_width + 4) // "> " + trailing headroom
        .clamp(20, 80);
    let title = trim_to_one_line(&item.title, title_max);
    format!(
        "{:>5} {:<id_w$} {:<br_w$} {}",
        age,
        id_short,
        bracket,
        title,
        id_w = ID_COL_WIDTH,
        br_w = BRACKET_COL_WIDTH,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_label_maps_known_clis() {
        assert_eq!(source_label_for_cli("claude"), "claude");
        assert_eq!(source_label_for_cli("opencode"), "opencode");
        assert_eq!(source_label_for_cli("unknown"), "native");
    }

    #[test]
    fn first_nonempty_line_skips_blanks() {
        assert_eq!(
            first_nonempty_line("\n  \nhello\nworld"),
            Some("hello".into())
        );
        assert_eq!(first_nonempty_line("   "), None);
        assert_eq!(first_nonempty_line(""), None);
    }

    #[test]
    fn format_label_aligns_columns() {
        let item = PickerItem {
            id: "abcdef1234".into(),
            source: "claude",
            title: "fix the auth bug".into(),
            updated_at: Utc::now(),
        };
        let line = format_label(&item);
        // Bracketed source is left-padded to 10 chars (`[claude]  ` = 10).
        assert!(line.contains("[claude]  "));
        assert!(line.contains("fix the auth bug"));
    }
}
