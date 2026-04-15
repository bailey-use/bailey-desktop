//! Command handlers module for the aivo CLI.
//! Provides implementations for all CLI commands.

use crate::services::ai_launcher::PreparedLaunch;
use crate::services::environment_injector::redact_env_value;
use crate::style;

/// Strips trailing slashes and a bare `/v1` suffix from a provider base URL.
pub(crate) fn normalize_base_url(url: &str) -> &str {
    let url = url.trim_end_matches('/');
    url.strip_suffix("/v1").unwrap_or(url)
}

/// Truncates `text` to its first line, then to `max_chars` with an ellipsis.
/// Used by `aivo context` and `--context` for one-line topic previews.
pub(crate) fn trim_to_one_line(text: &str, max_chars: usize) -> String {
    let one_line: String = text.lines().next().unwrap_or("").chars().collect();
    if one_line.chars().count() > max_chars {
        let prefix: String = one_line.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", prefix)
    } else {
        one_line
    }
}

/// Truncates a URL for display while preserving both the prefix and suffix.
pub(crate) fn truncate_url_for_display(url: &str, max_len: usize) -> String {
    let char_count = url.chars().count();
    if char_count <= max_len {
        return url.to_string();
    }
    let keep_suffix = 15.min(max_len / 3);
    let keep_prefix = max_len.saturating_sub(keep_suffix + 1);
    let prefix: String = url.chars().take(keep_prefix).collect();
    let suffix: String = url.chars().skip(char_count - keep_suffix).collect();
    format!("{prefix}…{suffix}")
}

pub mod alias;
pub mod chat;
pub(crate) mod chat_request_builder;
pub(crate) mod chat_response_parser;
pub(crate) mod chat_tui_format;
pub mod context;
pub mod info;
pub mod keys;
pub(crate) mod keys_ui;
pub mod logs;
pub mod models;
pub mod run;
pub mod serve;
pub mod start;
pub mod stats;
pub mod update;

pub use alias::AliasCommand;
pub use chat::ChatCommand;
pub use context::ContextCommand;
pub use info::InfoCommand;
pub use keys::KeysCommand;
pub use logs::LogsCommand;
pub use models::ModelsCommand;
pub use run::RunCommand;
pub use serve::{ServeCommand, ServeParams};
pub use start::{StartCommand, StartFlowArgs};
pub use stats::StatsCommand;
pub use update::UpdateCommand;

pub(crate) fn print_launch_preview(plan: &PreparedLaunch) {
    println!(
        "{} {}",
        style::bold("Tool:"),
        style::cyan(plan.tool.as_str())
    );
    println!(
        "{} {} {}",
        style::bold("Key:"),
        style::cyan(plan.key.display_name()),
        style::dim(format!("({})", plan.key.base_url))
    );
    println!(
        "{} {}",
        style::bold("Model:"),
        plan.model.as_deref().unwrap_or("(tool default)")
    );
    println!(
        "{} {}",
        style::bold("Command:"),
        format_shell_command(&plan.command, &plan.args)
    );
    println!();
    println!("{}", style::bold("Environment:"));
    if plan.env_vars.is_empty() {
        println!("  {}", style::dim("(none)"));
    } else {
        let mut keys: Vec<_> = plan.env_vars.keys().collect();
        keys.sort();
        for key in keys {
            println!("  {}={}", key, redact_env_value(key, &plan.env_vars[key]));
        }
    }

    if !plan.notes.is_empty() {
        println!();
        println!("{}", style::bold("Notes:"));
        for note in &plan.notes {
            println!("  {} {}", style::arrow_symbol(), note);
        }
    }
}

fn format_shell_command(command: &str, args: &[String]) -> String {
    let mut parts = vec![shell_quote(command)];
    parts.extend(args.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.' | ':' | '='))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::truncate_url_for_display;

    #[test]
    fn truncate_url_for_display_preserves_short_urls() {
        assert_eq!(
            truncate_url_for_display("https://api.example.com/v1", 50),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn truncate_url_for_display_shortens_long_urls() {
        let url = "https://very-long-provider-host.example.com/path/to/a/deeply/nested/resource/v1";
        let truncated = truncate_url_for_display(url, 32);

        assert_eq!(
            truncated,
            format!("{}…{}", &url[..21], &url[url.len() - 10..])
        );
    }
}
