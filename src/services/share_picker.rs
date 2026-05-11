//! Interactive session picker for `aivo logs share` (no id passed).
//!
//! Shape: ratatui app with a left list of shareable sessions and a right
//! preview pane built from the summary data already on each row — no
//! per-item file reads or message decryption, so the picker stays snappy
//! across hundreds of sessions.
//!
//! Sources mirror what `aivo logs` lists by default:
//!   - chat sessions in the current cwd (from SessionStore index)
//!   - native CLI sessions in the current cwd (claude, codex, gemini, pi,
//!     opencode — via context_ingest)
//!   - amp threads (not cwd-keyed; included unfiltered, newest first)
//!
//! `run`/`serve` log rows are intentionally excluded — they aren't
//! shareable conversations on their own.
//!
//! Progressive loading: the TUI opens immediately and items stream in from
//! the background as each source finishes, so the terminal isn't blank
//! while the filesystem is being walked.

use std::io::{self, IsTerminal, Stdout};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::commands::chat::format_time_ago_short_dt;
use crate::commands::logs::trim_to_one_line;
use crate::services::amp_threads;
use crate::services::context_ingest::{self, IngestOptions};
use crate::services::id_compact::compact_id;
use crate::services::session_store::SessionStore;

/// One row in the picker list. The `id` is what gets handed to
/// `share_resolver::resolve_session` on Enter.
#[derive(Debug, Clone)]
struct PickerItem {
    id: String,
    source: &'static str,
    /// Short label shown in the left column. Includes source bracket,
    /// title/topic, and (where available) trailing model/cli context.
    short_label: String,
    updated_at: DateTime<Utc>,
    preview: Preview,
    /// Pre-lowercased `source + short_label + id` for search filtering.
    /// Built once at construction so per-keystroke filter passes don't
    /// re-allocate three lowercase strings per item per call.
    haystack: String,
}

impl PickerItem {
    fn new(
        id: String,
        source: &'static str,
        short_label: String,
        updated_at: DateTime<Utc>,
        preview: Preview,
    ) -> Self {
        let mut haystack = String::with_capacity(source.len() + short_label.len() + id.len() + 2);
        haystack.push_str(&source.to_lowercase());
        haystack.push(' ');
        haystack.push_str(&short_label.to_lowercase());
        haystack.push(' ');
        haystack.push_str(&id.to_lowercase());
        Self {
            id,
            source,
            short_label,
            updated_at,
            preview,
            haystack,
        }
    }
}

/// What the right pane renders when this item is selected. All fields
/// are pre-populated when items are gathered — no lazy file reads.
#[derive(Debug, Clone)]
struct Preview {
    /// Header line: `chat · gpt-4o`, `claude (native)`, `amp thread`.
    header: String,
    /// Full session id (the picker shows a compact form on the left).
    full_id: String,
    cwd: Option<String>,
    /// Pre-formatted body with section markers (`User:` / `Assistant:` etc.).
    body: String,
}

/// Public entrypoint. `Ok(Some(id))` = user selected, `Ok(None)` = cancelled,
/// `Err(_)` = setup or I/O failure (caller decides whether to print help).
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

    // Kick off four loaders concurrently (quick native, full native, chat,
    // amp); they send items via a channel as they complete so the picker can
    // open immediately.
    let (tx, rx) = mpsc::channel::<PickerItem>();

    let canonical_root = std::fs::canonicalize(project_root)
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .to_string();

    let store_clone = session_store.clone();
    let root_clone = canonical_root.clone();
    let project_path = project_root.to_path_buf();

    // The runtime is `current_thread`, so blocking the thread (inside the
    // TUI loop) prevents spawned tasks from running. Run the picker on a
    // `spawn_blocking` OS thread instead, then the runtime is free to drive
    // the loader tasks while the picker is showing.
    let picker_handle = tokio::task::spawn_blocking(move || run_picker(rx, all));

    // Two-phase native loading:
    //   Phase 1 (quick) — top 3 per CLI, no age limit.  Reads ~15 small
    //     file headers; the picker sees the most-recent sessions within one
    //     or two poll cycles (~50–100 ms).
    //   Phase 2 (full)  — top 50 per CLI, fills the rest of the list.
    //
    // Chat and amp are already fast (SQLite index + mtime-sorted dir scan)
    // and don't need a quick pass.
    let pp_quick = project_path.clone();
    let tx_nq = tx.clone();
    tokio::spawn(async move {
        let opts = IngestOptions {
            max_age_days: None,
            min_updated_at: None,
            max_per_source: Some(3),
        };
        load_native_items(&pp_quick, all, opts, tx_nq).await;
    });

    let tx1 = tx.clone();
    let tx2 = tx.clone();
    let tx3 = tx.clone();
    tokio::spawn(async move {
        load_chat_items(&store_clone, &root_clone, all, tx1).await;
    });
    tokio::spawn(async move {
        let opts = IngestOptions {
            max_age_days: None,
            min_updated_at: None,
            max_per_source: Some(50),
        };
        load_native_items(&project_path, all, opts, tx2).await;
    });
    tokio::spawn(async move {
        load_amp_items(tx3).await;
    });
    // Drop the original sender so the channel disconnects once all four
    // spawns have also dropped their clones.
    drop(tx);

    picker_handle.await.context("picker thread panicked")?
}

// ---------------------------------------------------------------------------
// Source loaders — each sends items via the channel and drops tx when done.
// ---------------------------------------------------------------------------

async fn load_chat_items(
    store: &SessionStore,
    canonical_root: &str,
    all: bool,
    tx: mpsc::Sender<PickerItem>,
) {
    let Ok(entries) = store.all_chat_sessions().await else {
        return;
    };
    for entry in entries {
        if !all && entry.cwd != canonical_root {
            continue;
        }
        let updated_at = parse_rfc3339(&entry.updated_at);
        let title = if entry.title.trim().is_empty() {
            entry.model.clone()
        } else {
            entry.title.clone()
        };
        let body = if entry.preview.trim().is_empty() {
            String::new()
        } else {
            entry.preview.clone()
        };
        let preview = Preview {
            header: format!("chat · {}", entry.model),
            full_id: entry.session_id.clone(),
            cwd: Some(entry.cwd),
            body,
        };
        let item = PickerItem::new(entry.session_id, "chat", title, updated_at, preview);
        if tx.send(item).is_err() {
            return;
        }
    }
}

async fn load_native_items(
    project_root: &Path,
    all: bool,
    opts: IngestOptions,
    tx: mpsc::Sender<PickerItem>,
) {
    let threads = if all {
        context_ingest::ingest_native_sessions_global(opts).await
    } else {
        context_ingest::ingest_project(project_root, opts).await
    };
    let Ok(threads) = threads else { return };
    for thread in threads {
        let title = first_nonempty_line(&thread.topic).unwrap_or_else(|| thread.cli.clone());
        let body = format_thread_body(&thread.topic, &thread.last_response);
        let preview = Preview {
            header: format!("{} (native)", thread.cli),
            full_id: thread.session_id.clone(),
            cwd: thread.cwd,
            body,
        };
        let item = PickerItem::new(
            thread.session_id,
            source_label_for_cli(&thread.cli),
            title,
            thread.updated_at,
            preview,
        );
        if tx.send(item).is_err() {
            return;
        }
    }
}

async fn load_amp_items(tx: mpsc::Sender<PickerItem>) {
    let amp_dir = amp_threads::default_threads_dir();
    for value in amp_threads::list_threads(&amp_dir, 100).await {
        let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
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
        let count = value
            .get("messageCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let preview = Preview {
            header: format!("amp · {} messages", count),
            full_id: id.to_string(),
            cwd: None,
            body: title.clone(),
        };
        let item = PickerItem::new(id.to_string(), "amp", title, updated_at, preview);
        if tx.send(item).is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// TUI
// ---------------------------------------------------------------------------

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

fn format_thread_body(topic: &str, last_response: &str) -> String {
    let mut out = String::new();
    if !topic.trim().is_empty() {
        out.push_str("User:\n");
        out.push_str(topic.trim());
    }
    if !last_response.trim().is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Assistant:\n");
        out.push_str(last_response.trim());
    }
    out
}

struct PickerState {
    items: Vec<PickerItem>,
    list_state: ListState,
    loading: bool,
    search_query: String,
    /// Set to true the first time the user presses a navigation key.
    /// While false, every incoming item pins the cursor to index 0
    /// (newest item always highlighted before user interacts).
    /// Once true, insertions adjust the selected index so the currently
    /// highlighted item stays highlighted — no cursor jump.
    user_navigated: bool,
}

impl PickerState {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            list_state: ListState::default(),
            loading: true,
            search_query: String::new(),
            user_navigated: false,
        }
    }

    fn visible_items(&self) -> Vec<&PickerItem> {
        if self.search_query.is_empty() {
            return self.items.iter().collect();
        }
        let q = self.search_query.to_lowercase();
        self.items
            .iter()
            .filter(|i| i.haystack.contains(&q))
            .collect()
    }

    fn push_item(&mut self, item: PickerItem) {
        if self.items.iter().any(|i| i.id == item.id) {
            return;
        }

        // Snapshot the selected item's id BEFORE inserting. After the insert
        // we re-find it in the new (possibly search-filtered) visible list, so
        // tracking works the same whether or not a search query is active.
        let selected_id_before: Option<String> = if self.user_navigated {
            let visible = self.visible_items();
            self.list_state
                .selected()
                .and_then(|i| visible.get(i))
                .map(|it| it.id.clone())
        } else {
            None
        };

        let pos = self
            .items
            .partition_point(|i| i.updated_at >= item.updated_at);
        self.items.insert(pos, item);

        if !self.user_navigated {
            // Pre-navigation: pin the cursor to the newest item. Reset offset
            // too so ratatui's diff never sees a stale scroll window.
            self.list_state = ListState::default().with_offset(0);
            self.list_state.select(Some(0));
            return;
        }

        // Post-navigation: re-find the previously-selected item by id. If it
        // dropped out of view (search filter no longer matches), fall back to
        // the top of the visible list. Offset is left alone — ratatui adjusts
        // it during render to keep `selected` visible.
        if let Some(id) = selected_id_before {
            let visible = self.visible_items();
            let new_sel = visible.iter().position(|it| it.id == id).unwrap_or(0);
            self.list_state.select(Some(new_sel));
        }
    }

    /// Called once the channel disconnects. Items are already sorted by
    /// `push_item`, and the cursor is wherever the user (or `push_item`'s
    /// pre-navigation pin) left it — just flip the loading flag.
    fn finish_loading(&mut self) {
        self.loading = false;
    }

    fn reset_selection(&mut self) {
        let visible = self.visible_items().len();
        if visible > 0 {
            self.list_state.select(Some(0));
        } else {
            self.list_state.select(None);
        }
    }
}

fn run_picker(rx: mpsc::Receiver<PickerItem>, all: bool) -> Result<Option<String>> {
    let mut terminal = setup_terminal()?;
    let result = picker_loop(&mut terminal, rx, all);
    teardown_terminal(&mut terminal);
    result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("failed to enable terminal raw mode for picker")?;
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        let _ = disable_raw_mode();
        return Err(e).context("failed to enter alternate screen for picker");
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to construct ratatui terminal")?;
    terminal.clear().ok();
    Ok(terminal)
}

fn teardown_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    let _ = terminal.show_cursor();
}

fn picker_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    rx: mpsc::Receiver<PickerItem>,
    all: bool,
) -> Result<Option<String>> {
    let mut state = PickerState::new();
    let mut dirty = true;

    loop {
        // Drain any incoming items from the background loaders.
        if state.loading {
            loop {
                match rx.try_recv() {
                    Ok(item) => {
                        state.push_item(item);
                        dirty = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        // All loaders finished — flip out of loading mode.
                        state.finish_loading();
                        dirty = true;
                        break;
                    }
                }
            }
        }
        if dirty {
            terminal.draw(|frame| draw(frame, &mut state, all))?;
            dirty = false;
        }

        // Poll with a short timeout so we re-drain the channel frequently.
        if event::poll(Duration::from_millis(50))? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match handle_key(key, &mut state) {
                Action::None => {
                    dirty = true;
                }
                Action::Cancel => return Ok(None),
                Action::Select => {
                    let visible = state.visible_items();
                    let id = state
                        .list_state
                        .selected()
                        .and_then(|i| visible.get(i))
                        .map(|i| i.id.clone());
                    return Ok(id);
                }
            }
        }
    }
}

enum Action {
    None,
    Select,
    Cancel,
}

fn handle_key(key: KeyEvent, state: &mut PickerState) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let visible_count = state.visible_items().len();

    match key.code {
        KeyCode::Esc => {
            if !state.search_query.is_empty() {
                state.search_query.clear();
                state.reset_selection();
                Action::None
            } else {
                Action::Cancel
            }
        }
        KeyCode::Char('c') if ctrl => Action::Cancel,
        KeyCode::Enter => Action::Select,
        KeyCode::Backspace => {
            state.search_query.pop();
            state.reset_selection();
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') if !ctrl => {
            state.user_navigated = true;
            move_selection(&mut state.list_state, visible_count, -1);
            Action::None
        }
        KeyCode::Char('p') if ctrl => {
            state.user_navigated = true;
            move_selection(&mut state.list_state, visible_count, -1);
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') if !ctrl => {
            state.user_navigated = true;
            move_selection(&mut state.list_state, visible_count, 1);
            Action::None
        }
        KeyCode::Char('n') if ctrl => {
            state.user_navigated = true;
            move_selection(&mut state.list_state, visible_count, 1);
            Action::None
        }
        KeyCode::PageUp => {
            state.user_navigated = true;
            move_selection(&mut state.list_state, visible_count, -10);
            Action::None
        }
        KeyCode::PageDown => {
            state.user_navigated = true;
            move_selection(&mut state.list_state, visible_count, 10);
            Action::None
        }
        KeyCode::Home => {
            state.user_navigated = true;
            if visible_count > 0 {
                state.list_state.select(Some(0));
            }
            Action::None
        }
        KeyCode::End => {
            state.user_navigated = true;
            if visible_count > 0 {
                state.list_state.select(Some(visible_count - 1));
            }
            Action::None
        }
        KeyCode::Char(c) if !ctrl => {
            state.search_query.push(c);
            state.reset_selection();
            Action::None
        }
        _ => Action::None,
    }
}

fn move_selection(state: &mut ListState, item_count: usize, delta: isize) {
    if item_count == 0 {
        return;
    }
    let current = state.selected().unwrap_or(0) as isize;
    let next = (current + delta).clamp(0, item_count as isize - 1);
    state.select(Some(next as usize));
}

fn draw(frame: &mut ratatui::Frame<'_>, state: &mut PickerState, all: bool) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(0),    // body
            Constraint::Length(1), // hint / search bar
        ])
        .split(area);

    let visible = state.visible_items();
    let count_label = if state.loading {
        format!("({} found, loading…)", visible.len())
    } else if !state.search_query.is_empty() {
        format!("({} of {} match)", visible.len(), state.items.len())
    } else {
        let scope = if all { "all projects" } else { "this project" };
        format!("({} found in {})", visible.len(), scope)
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " Share which session? ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(count_label, Style::default().fg(Color::DarkGray)),
    ]));
    frame.render_widget(header, layout[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(layout[1]);

    // Calculate how many chars the title column can use:
    //   inner width = pane width − 2 borders
    //   fixed cols  = 2 (highlight "▶ ") + 5 (age) + 1 + 10 (id) + 1 + 10 (src) + 1 = 30
    let inner_width = body[0].width.saturating_sub(2) as usize;
    let title_max = inner_width.saturating_sub(30).max(10);

    // Build owned widgets from `visible` before the borrow ends — avoids
    // cloning the whole selected `PickerItem` (including its multi-KB
    // preview body) just to dodge the borrow checker on `state.list_state`.
    let selected_idx = state.list_state.selected();
    let preview_paragraph: Option<Paragraph<'static>> = selected_idx
        .and_then(|i| visible.get(i))
        .map(|i| render_preview(i));
    let list_items: Vec<ListItem> = visible
        .iter()
        .map(|item| ListItem::new(format_list_line(item, title_max)))
        .collect();
    drop(visible);

    let mut ls = state.list_state.clone();
    let list = List::new(list_items)
        .block(Block::default().borders(Borders::ALL).title("Sessions"))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, body[0], &mut ls);
    state.list_state = ls;

    let loading = state.loading;
    let preview = preview_paragraph.unwrap_or_else(|| {
        Paragraph::new(if loading {
            "(loading…)"
        } else {
            "(no selection)"
        })
    });
    frame.render_widget(
        preview.block(Block::default().borders(Borders::ALL).title("Preview")),
        body[1],
    );

    let hint = if state.search_query.is_empty() {
        Line::from(vec![
            Span::styled(" ↑↓", Style::default().fg(Color::Cyan)),
            Span::raw(" navigate · "),
            Span::styled("Enter", Style::default().fg(Color::Cyan)),
            Span::raw(" share · "),
            Span::raw("type to search · "),
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
            Span::raw(" cancel "),
        ])
    } else {
        Line::from(vec![
            Span::styled(" /", Style::default().fg(Color::Yellow)),
            Span::raw(format!(" {}▌ ", state.search_query)),
            Span::styled("↑↓", Style::default().fg(Color::Cyan)),
            Span::raw(" navigate · "),
            Span::styled("Enter", Style::default().fg(Color::Cyan)),
            Span::raw(" share · "),
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
            Span::raw(" clear "),
        ])
    };
    let hint_widget = Paragraph::new(hint).style(Style::default().fg(Color::Gray));
    frame.render_widget(hint_widget, layout[2]);
}

fn format_list_line(item: &PickerItem, title_max: usize) -> Line<'static> {
    let age = format_time_ago_short_dt(item.updated_at);
    let id_short = compact_id(&item.id, 10);
    Line::from(vec![
        Span::styled(format!("{:>5}", age), Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(
            format!("{:<10}", id_short),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(" "),
        Span::styled(
            format!("[{:<8}]", item.source),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw(" "),
        Span::raw(trim_to_one_line(&item.short_label, title_max)),
    ])
}

fn render_preview(item: &PickerItem) -> Paragraph<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        item.preview.header.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!("id: {}", item.preview.full_id),
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        format!("updated: {}", format_time_ago_short_dt(item.updated_at)),
        Style::default().fg(Color::DarkGray),
    )));
    if let Some(cwd) = &item.preview.cwd {
        lines.push(Line::from(Span::styled(
            format!("cwd: {}", cwd),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    if item.preview.body.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "(no preview text on this row)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for raw in item.preview.body.lines() {
            lines.push(Line::from(raw.to_string()));
        }
    }
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_thread_body_handles_either_side_missing() {
        assert_eq!(
            format_thread_body("question", "answer"),
            "User:\nquestion\n\nAssistant:\nanswer"
        );
        assert_eq!(format_thread_body("question", ""), "User:\nquestion");
        assert_eq!(format_thread_body("  ", "answer"), "Assistant:\nanswer");
        assert_eq!(format_thread_body("", ""), "");
    }

    #[test]
    fn push_item_maintains_newest_first_and_pins_selection() {
        let mut state = PickerState::new();
        let now = Utc::now();
        let make_item = |id: &str, hours_ago: i64| {
            PickerItem::new(
                id.to_string(),
                "chat",
                id.to_string(),
                now - chrono::Duration::hours(hours_ago),
                Preview {
                    header: "h".into(),
                    full_id: id.to_string(),
                    cwd: None,
                    body: "".into(),
                },
            )
        };
        // Push in arbitrary order (as loaders would deliver them).
        state.push_item(make_item("old", 5));
        state.push_item(make_item("new", 1));
        state.push_item(make_item("mid", 3));
        // Items should be sorted newest-first already.
        assert_eq!(state.items[0].id, "new");
        assert_eq!(state.items[1].id, "mid");
        assert_eq!(state.items[2].id, "old");
        // Before user navigates: selection always pinned to 0.
        assert_eq!(state.list_state.selected(), Some(0));
        assert_eq!(state.list_state.offset(), 0);

        // After user navigates: cursor tracks the highlighted item through insertions.
        state.user_navigated = true;
        state.list_state.select(Some(1)); // user selects "mid"
        state.push_item(make_item("newest", 0)); // inserted at position 0
        // "mid" was at index 1, insertion at 0 shifts it to index 2.
        assert_eq!(state.list_state.selected(), Some(2));
        assert_eq!(state.items[2].id, "mid");
    }

    #[test]
    fn push_item_dedupes_by_id() {
        let mut state = PickerState::new();
        let now = Utc::now();
        let make = |id: &str, hours_ago: i64| {
            PickerItem::new(
                id.to_string(),
                "chat",
                id.to_string(),
                now - chrono::Duration::hours(hours_ago),
                Preview {
                    header: "h".into(),
                    full_id: id.to_string(),
                    cwd: None,
                    body: "".into(),
                },
            )
        };
        state.push_item(make("abc", 1));
        // Quick-pass and full-pass loaders both emit the same id; second is a no-op.
        state.push_item(make("abc", 1));
        assert_eq!(state.items.len(), 1);
    }

    #[test]
    fn push_item_tracks_selection_by_id_under_search_filter() {
        let mut state = PickerState::new();
        let now = Utc::now();
        let make = |id: &str, label: &str, hours_ago: i64| {
            PickerItem::new(
                id.to_string(),
                "chat",
                label.to_string(),
                now - chrono::Duration::hours(hours_ago),
                Preview {
                    header: "h".into(),
                    full_id: id.to_string(),
                    cwd: None,
                    body: "".into(),
                },
            )
        };
        // Seed the picker with three items that match a future "auth" filter
        // and one that does not.
        state.push_item(make("a", "fix auth bug", 3));
        state.push_item(make("b", "refactor auth", 2));
        state.push_item(make("c", "dark mode", 4));

        // User searches for "auth" and selects the second visible match ("a",
        // since "refactor auth" is newer and sorts first).
        state.search_query = "auth".into();
        state.user_navigated = true;
        state.list_state.select(Some(1));
        let visible = state.visible_items();
        assert_eq!(visible[1].id, "a");
        drop(visible);

        // A non-matching item arrives during loading. Old logic (pos-based)
        // would corrupt the selected index; id-based tracking keeps the cursor
        // on "a".
        state.push_item(make("d", "unrelated note", 0));
        let visible = state.visible_items();
        assert_eq!(state.list_state.selected(), Some(1));
        assert_eq!(visible[state.list_state.selected().unwrap()].id, "a");
        drop(visible);

        // A new matching item arrives and is newer than "a" → it sorts above
        // "a" in the filtered view. The cursor must follow "a" downward.
        state.push_item(make("e", "auth tweak", 0));
        let visible = state.visible_items();
        let sel = state.list_state.selected().unwrap();
        assert_eq!(visible[sel].id, "a");
    }

    #[test]
    fn visible_items_filters_by_search_query() {
        let mut state = PickerState::new();
        let now = Utc::now();
        for label in &["fix login bug", "refactor auth", "add dark mode"] {
            state.push_item(PickerItem::new(
                label.to_string(),
                "chat",
                label.to_string(),
                now,
                Preview {
                    header: "h".into(),
                    full_id: label.to_string(),
                    cwd: None,
                    body: "".into(),
                },
            ));
        }
        state.search_query = "auth".into();
        let visible = state.visible_items();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "refactor auth");
    }
}
