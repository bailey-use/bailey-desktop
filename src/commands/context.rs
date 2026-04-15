//! `aivo context` command — stateless preview of what `--context`
//! would inject. Context has no durable storage of its own; data comes
//! from claude/codex session files and aivo's own logs.db on every call.

use serde_json::json;

use crate::cli::ContextArgs;
use crate::commands::chat_tui_format::format_time_ago_short_dt;
use crate::commands::trim_to_one_line;
use crate::errors::ExitCode;
use crate::services::context_ingest::{IngestOptions, ingest_project};
use crate::services::project_id::Thread;
use crate::services::system_env;
use crate::style;

#[derive(Default)]
pub struct ContextCommand;

impl ContextCommand {
    pub fn new() -> Self {
        Self
    }

    pub async fn execute(&self, args: ContextArgs) -> ExitCode {
        // Best-effort one-time cleanup: the file-based store from the prior
        // schema lives at `~/.config/aivo/context/`. Orphan that on first run
        // so we don't leave unused state on users' disks after this rename.
        cleanup_orphan_store().await;

        match args.action.as_deref().unwrap_or("") {
            // Bare `aivo context` → preview what --context would inject.
            "" => self.summary(&args).await,
            "clear" | "gc" => {
                eprintln!(
                    "{} `aivo context {}` was removed — context has no durable state to manage.",
                    style::yellow("!"),
                    args.action.as_deref().unwrap_or("")
                );
                eprintln!(
                    "{}",
                    style::dim(
                        "  Threads are derived from claude/codex session files on each run."
                    )
                );
                eprintln!(
                    "{}",
                    style::dim(
                        "  To 'forget' past work: remove the underlying session files directly (e.g. rm ~/.claude/projects/<project>/)."
                    )
                );
                ExitCode::UserError
            }
            other => {
                eprintln!(
                    "{} Unknown context action '{}'. Run without an action for the injection preview.",
                    style::red("Error:"),
                    other
                );
                ExitCode::UserError
            }
        }
    }

    /// Preview the exact slice `--context` would inject, plus resume hints.
    async fn summary(&self, args: &ContextArgs) -> ExitCode {
        let project_root = match system_env::current_dir() {
            Some(p) => p,
            None => {
                eprintln!(
                    "{} Could not determine current directory.",
                    style::red("Error:")
                );
                return ExitCode::UserError;
            }
        };

        let opts = if args.all {
            IngestOptions::unlimited()
        } else if let Some(days) = args.last_days {
            IngestOptions {
                max_age_days: Some(days),
                ..IngestOptions::default()
            }
        } else {
            IngestOptions::default()
        };

        let threads = match ingest_project(&project_root, opts).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{} {}", style::red("Error:"), e);
                return ExitCode::UserError;
            }
        };

        if args.json {
            let payload = json!({
                "project_root": project_root.to_string_lossy(),
                "thread_count": threads.len(),
                "threads": threads.iter().map(thread_to_json).collect::<Vec<_>>(),
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
            return ExitCode::Success;
        }

        if threads.is_empty() {
            println!(
                "{}",
                style::dim(format!(
                    "No context yet for this project ({}).",
                    project_root.display()
                ))
            );
            println!(
                "{}",
                style::dim(
                    "Run an AI CLI in this directory; aivo will pick it up automatically next time."
                )
            );
            return ExitCode::Success;
        }

        // Layout: age (right-aligned) · id (bold) · topic. CLI column appears
        // between id and topic only when the project has mixed sources.
        let distinct_clis: std::collections::BTreeSet<&str> =
            threads.iter().map(|t| t.cli.as_str()).collect();
        let show_cli_column = distinct_clis.len() > 1;
        let max_cli_len = if show_cli_column {
            threads.iter().map(|t| t.cli.len()).max().unwrap_or(6)
        } else {
            0
        };

        for t in threads.iter() {
            // Pad width BEFORE styling; ANSI escape codes would otherwise
            // break `{:>N}` alignment (the formatter counts escape bytes).
            let age = format!("{:>4}", format_time_ago_short_dt(t.updated_at));
            if show_cli_column {
                let cli = format!("{:<width$}", t.cli, width = max_cli_len);
                println!(
                    "{}  {}  {}  {}",
                    style::dim(age),
                    style::bold(short_id(&t.session_id)),
                    style::cyan(cli),
                    trim_to_one_line(&t.topic, 98)
                );
            } else {
                println!(
                    "{}  {}  {}",
                    style::dim(age),
                    style::bold(short_id(&t.session_id)),
                    trim_to_one_line(&t.topic, 106)
                );
            }
        }
        if !threads.is_empty() {
            println!(
                "{} {}",
                style::dim("→"),
                style::cyan("aivo run <tool> --context[=<id>]"),
            );
        }

        ExitCode::Success
    }

    pub fn print_help() {
        println!("{} aivo context [--json]", style::bold("Usage:"));
        println!();
        println!(
            "{}",
            style::dim(
                "Cross-CLI context — derived on demand from AI tool session files. No durable storage; each run reads fresh."
            )
        );
        println!();
        println!("{}", style::bold("Sources:"));
        let print_src = |name: &str, path: &str| {
            println!(
                "  {}{}",
                style::cyan(format!("{:<8}", name)),
                style::dim(path)
            );
        };
        print_src("claude", "~/.claude/projects/<encoded-cwd>/*.jsonl");
        print_src("codex", "~/.codex/sessions/**/*.jsonl (matched by cwd)");
        print_src("gemini", "~/.gemini/tmp/<sha256(cwd)>/chats/session-*.json");
        print_src("pi", "~/.pi/agent/sessions/--<encoded-cwd>--/*.jsonl");
        print_src("opencode", "~/.local/share/opencode/opencode.db (SQLite)");
        println!();
        println!("{}", style::bold("Options:"));
        let print_opt = |flag: &str, desc: &str| {
            println!(
                "  {}{}",
                style::cyan(format!("{:<18}", flag)),
                style::dim(desc)
            );
        };
        print_opt("-a, --all", "Show all sessions (no age or count caps)");
        print_opt("--last-days <N>", "Override the default 14-day age cap");
        print_opt("--json", "Dump every available thread as JSON");
        println!();
        println!("{}", style::bold("Examples:"));
        println!("  {}", style::dim("aivo context"));
        println!("  {}", style::dim("aivo context -a"));
        println!("  {}", style::dim("aivo context --last-days=30"));
        println!("  {}", style::dim("aivo context --json | jq '.threads'"));
        println!("  {}", style::dim("aivo run claude --context"));
    }
}

/// Remove the file-based store from the previous schema, if present. Silent
/// on failure — missing dir and permission errors both fall through to the
/// same effective "nothing to clean up" outcome.
async fn cleanup_orphan_store() {
    let Some(home) = system_env::home_dir() else {
        return;
    };
    let orphan = home.join(".config").join("aivo").join("context");
    if orphan.exists() {
        let _ = tokio::fs::remove_dir_all(&orphan).await;
    }
}

fn short_id(sid: &str) -> String {
    sid.chars().take(8).collect()
}

fn thread_to_json(t: &Thread) -> serde_json::Value {
    json!({
        "cli": t.cli,
        "session_id": t.session_id,
        "source_path": t.source_path,
        "topic": t.topic,
        "last_response": t.last_response,
        "updated_at": t.updated_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_caps_at_eight_chars() {
        assert_eq!(short_id("0127d1b8-cc27-422d").chars().count(), 8);
    }
}
