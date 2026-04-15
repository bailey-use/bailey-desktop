use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::commands::chat::ChatMessage;
use crate::commands::chat_response_parser::TokenUsage;

pub(super) fn format_request_elapsed(elapsed: Duration) -> String {
    format!("{}s", elapsed.as_secs())
}

pub(super) fn format_token_count(tokens: u64, usage: Option<TokenUsage>) -> String {
    if let Some(usage) = usage {
        let total = usage.total_tokens();
        let label = if total == 1 { "token" } else { "tokens" };
        return format!("{} {}", format_token_count_value(total), label);
    }
    if tokens == 0 {
        "0 tokens".to_string()
    } else {
        let label = if tokens == 1 { "token" } else { "tokens" };
        format!("~{} {}", format_token_count_value(tokens), label)
    }
}

fn format_token_count_value(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }

    let rounded_tenths = (tokens + 50) / 100;
    let thousands = rounded_tenths / 10;
    let tenths = rounded_tenths % 10;
    if tenths == 0 {
        format!("{thousands}k")
    } else {
        format!("{thousands}.{tenths}k")
    }
}

const ATTACHMENT_OVERHEAD_CHARS: usize = 64;
const MESSAGE_OVERHEAD_CHARS: usize = 20;

pub(super) fn estimate_context_tokens(history: &[ChatMessage]) -> u64 {
    let total_chars: usize = history
        .iter()
        .map(|m| {
            let attachment_chars = m
                .attachments
                .iter()
                .map(|a| a.name.len() + ATTACHMENT_OVERHEAD_CHARS)
                .sum::<usize>();
            m.role.len() + m.content.len() + attachment_chars + MESSAGE_OVERHEAD_CHARS
        })
        .sum();
    (total_chars / 4) as u64
}

pub(super) fn build_footer_text(model: &str, base_url: &str, cwd: &str, width: u16) -> String {
    let host = footer_host_label(base_url);
    let cwd_label = footer_cwd_label(cwd);
    let candidates = [
        format!("{model} · {host} · {cwd_label}"),
        format!("{model} · {host}"),
        model.to_string(),
    ];

    candidates
        .into_iter()
        .find(|candidate| candidate.chars().count() <= usize::from(width.max(1)))
        .unwrap_or_else(|| truncate_for_width(model, width))
}

pub(super) fn footer_host_label(base_url: &str) -> String {
    if base_url == "copilot" {
        return "copilot".to_string();
    }

    let trimmed = base_url.trim().trim_end_matches('/');
    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    without_scheme
        .split('/')
        .next()
        .filter(|host| !host.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

fn footer_cwd_label(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

pub(super) fn wrapped_text_line_count(text: impl Into<Text<'static>>, width: u16) -> usize {
    if width == 0 {
        return 0;
    }

    Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .line_count(width)
}

pub(super) fn format_session_group_label(updated_at: &str) -> String {
    let parsed = DateTime::parse_from_rfc3339(updated_at)
        .map(|value| value.with_timezone(&Local))
        .ok();
    let Some(parsed) = parsed else {
        return updated_at.to_string();
    };
    let today = Local::now().date_naive();
    if parsed.date_naive() == today {
        "Today".to_string()
    } else {
        parsed.format("%a %b %d %Y").to_string()
    }
}

pub(super) fn format_session_time(updated_at: &str) -> String {
    DateTime::parse_from_rfc3339(updated_at)
        .map(|value| value.with_timezone(&Local).format("%-I:%M %p").to_string())
        .unwrap_or_else(|_| updated_at.to_string())
}

pub(super) fn format_session_match_count(filtered: usize, total: usize) -> String {
    if total == 0 {
        return "0 chats".to_string();
    }
    if filtered == total {
        return format!("{total} chats");
    }
    format!("{filtered}/{total}")
}

pub(super) fn format_picker_match_count(filtered: usize, total: usize, noun: &str) -> String {
    if total == 0 {
        return format!("0 {noun}");
    }
    if filtered == total {
        return format!("{total} {noun}");
    }
    format!("{filtered}/{total}")
}

pub(super) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub(super) fn truncate_for_display_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if display_width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let mut result = String::new();
    let mut used = 0;
    let limit = max_width - 1;
    for ch in text.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > limit {
            break;
        }
        used += width;
        result.push(ch);
    }
    result.push('…');
    result
}

pub fn format_time_ago_short(updated_at: &str) -> String {
    let parsed = DateTime::parse_from_rfc3339(updated_at)
        .map(|value| value.with_timezone(&Utc))
        .ok();
    let Some(parsed) = parsed else {
        return updated_at.to_string();
    };
    format_time_ago_short_dt(parsed)
}

/// Same compact "now/5m/6h/2d/3w/4mo/5y" output as `format_time_ago_short`,
/// but takes a parsed `DateTime` so callers that already have one don't need
/// to round-trip through RFC-3339.
pub fn format_time_ago_short_dt(updated_at: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - updated_at).num_seconds().max(0);
    match seconds {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        86_400..=604_799 => format!("{}d", seconds / 86_400),
        604_800..=2_592_000 => format!("{}w", seconds / 604_800),
        2_592_001..=31_535_999 => format!("{}mo", seconds / 2_592_000),
        _ => format!("{}y", seconds / 31_536_000),
    }
}

pub(super) fn truncate_for_width(text: &str, width: u16) -> String {
    truncate_for_display_width(text, usize::from(width))
}

#[cfg(test)]
mod tests {
    use super::{
        build_footer_text, display_width, format_request_elapsed, format_session_match_count,
        format_time_ago_short, format_token_count, truncate_for_display_width, truncate_for_width,
        wrapped_text_line_count,
    };
    use crate::commands::chat::TokenUsage;
    use chrono::{Duration as ChronoDuration, Utc};
    use std::time::Duration;

    #[test]
    fn test_wrapped_text_line_count_uses_ratatui_word_wrap() {
        assert_eq!(wrapped_text_line_count("", 10), 1);
        assert_eq!(wrapped_text_line_count("hello", 10), 1);
        assert_eq!(wrapped_text_line_count("abcdefghij", 5), 2);
        assert_eq!(wrapped_text_line_count("word word word", 8), 3);
    }

    #[test]
    fn test_truncate_for_width() {
        assert_eq!(truncate_for_width("hello", 10), "hello");
        assert_eq!(truncate_for_width("hello world", 6), "hello…");
    }

    #[test]
    fn test_build_footer_text_prefers_whole_segments() {
        assert_eq!(
            build_footer_text("gpt-4o", "https://openrouter.ai/api/v1", "/tmp/project", 80),
            "gpt-4o · openrouter.ai · project"
        );
        assert_eq!(
            build_footer_text("gpt-4o", "https://openrouter.ai/api/v1", "/tmp/project", 22),
            "gpt-4o · openrouter.ai"
        );
        assert_eq!(
            build_footer_text("gpt-4o", "https://openrouter.ai/api/v1", "/tmp/project", 6),
            "gpt-4o"
        );
    }

    #[test]
    fn test_format_token_count_with_usage_shows_total() {
        assert_eq!(
            format_token_count(
                999,
                Some(TokenUsage {
                    prompt_tokens: 129,
                    completion_tokens: 11,
                    cache_read_input_tokens: 90,
                    cache_creation_input_tokens: 15,
                }),
            ),
            "140 tokens"
        );
        assert_eq!(
            format_token_count(
                5_120,
                Some(TokenUsage {
                    prompt_tokens: 5_000,
                    completion_tokens: 120,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                }),
            ),
            "5.1k tokens"
        );
    }

    #[test]
    fn test_format_token_count_marks_estimates() {
        assert_eq!(format_token_count(0, None), "0 tokens");
        assert_eq!(format_token_count(105, None), "~105 tokens");
        assert_eq!(format_token_count(5_000, None), "~5k tokens");
        assert_eq!(format_token_count(12_345, None), "~12.3k tokens");
    }

    #[test]
    fn test_format_session_match_count() {
        assert_eq!(format_session_match_count(0, 0), "0 chats");
        assert_eq!(format_session_match_count(4, 4), "4 chats");
        assert_eq!(format_session_match_count(2, 5), "2/5");
    }

    #[test]
    fn test_truncate_for_display_width_handles_wide_text() {
        let truncated = truncate_for_display_width("你好🙂 hello", 8);
        assert!(display_width(&truncated) <= 8);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn test_format_time_ago_short() {
        let updated_at = (Utc::now() - ChronoDuration::minutes(5)).to_rfc3339();
        assert_eq!(format_time_ago_short(&updated_at), "5m");
    }

    #[test]
    fn test_format_request_elapsed() {
        assert_eq!(format_request_elapsed(Duration::from_secs(54)), "54s");
    }
}
