//! Prompt-prefix drift detection and recovery from malformed model output.

use serde_json::Value;
use std::hash::{Hash, Hasher};

fn hash_json(value: &Value) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(value)
        .unwrap_or_default()
        .hash(&mut h);
    h.finish()
}

/// `(system, tools)` hash of the request prefix; split so a drift names the half.
pub(crate) fn prefix_fingerprint(system: &Value, tools: &[Value]) -> (u64, u64) {
    let mut th = std::collections::hash_map::DefaultHasher::new();
    for t in tools {
        serde_json::to_string(t).unwrap_or_default().hash(&mut th);
    }
    (hash_json(system), th.finish())
}

const LEAKED_TOOL_TAGS: &[&str] = &["tool_calls", "tool_call", "function_calls", "invoke"];

/// Stripped content if `content` is a tool call rendered as plain-text markup, else
/// `None`. Flagged only when the markup *dominates* the message, so prose that merely
/// quotes `<tool_calls>`-style markup as an inline example survives untouched.
pub(crate) fn strip_if_leaked(content: &str) -> Option<String> {
    if !content.contains('<') {
        return None;
    }
    let cleaned = strip_leaked_tool_calls(content);
    if cleaned == content {
        return None;
    }
    let removed = content.len().saturating_sub(cleaned.len());
    let remaining = cleaned.trim().len();
    (remaining == 0 || removed >= remaining).then_some(cleaned)
}

/// Strip balanced tool-call markup (nested goes as a unit; unbalanced is kept).
pub(crate) fn strip_leaked_tool_calls(content: &str) -> String {
    let mut out = content.to_string();
    for tag in LEAKED_TOOL_TAGS {
        out = strip_balanced_tag(&out, tag);
    }
    out
}

/// Next real `<tag` opener (boundary-checked, so `<tool_call` skips `<tool_callable>`).
fn find_open(hay: &str, open: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = hay[from..].find(open) {
        let pos = from + rel;
        let after = hay[pos + open.len()..].chars().next();
        if matches!(after, Some('>' | ' ' | '\t' | '\n' | '/') | None) {
            return Some(pos);
        }
        from = pos + open.len();
    }
    None
}

fn strip_balanced_tag(s: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::new();
    let mut rest = s;
    loop {
        let Some(start) = find_open(rest, &open) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let mut depth = 1usize;
        let mut scan = start + open.len();
        let mut end = None;
        while depth > 0 {
            let tail = &rest[scan..];
            let next_open = find_open(tail, &open).map(|p| scan + p);
            let next_close = tail.find(&close).map(|p| scan + p);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    depth += 1;
                    scan = o + open.len();
                }
                (_, Some(c)) => {
                    depth -= 1;
                    scan = c + close.len();
                    if depth == 0 {
                        end = Some(scan);
                    }
                }
                _ => break,
            }
        }
        match end {
            Some(e) => rest = &rest[e..],
            None => {
                out.push_str(&rest[start..]); // `rest[..start]` already pushed
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_leaked_tool_call_markup_keeping_prose() {
        let s = "Let me read it. <tool_calls>{\"name\":\"read_file\"}</tool_calls> done.";
        assert_eq!(
            strip_if_leaked(s).as_deref(),
            Some("Let me read it.  done.")
        );
        assert_eq!(strip_leaked_tool_calls(s), "Let me read it.  done.");
    }

    #[test]
    fn strips_invoke_with_attributes_and_nested_function_calls() {
        let s = "<function_calls><invoke name=\"grep\"><parameter>x</parameter></invoke></function_calls>";
        assert_eq!(strip_leaked_tool_calls(s), "");
        assert_eq!(strip_if_leaked(s).as_deref(), Some(""));
    }

    #[test]
    fn leaves_clean_prose_and_unbalanced_fragments_untouched() {
        assert!(strip_if_leaked("just a normal answer").is_none());
        assert!(strip_if_leaked("the <tool_callable> trait").is_none());
        assert!(strip_if_leaked("plain text, no markup").is_none());
        let frag = "<tool_calls>oops no close";
        assert_eq!(strip_leaked_tool_calls(frag), frag);
        assert!(strip_if_leaked(frag).is_none());
        let prose_close = "the </tool_calls> tag ends a block";
        assert_eq!(strip_leaked_tool_calls(prose_close), prose_close);
        assert!(strip_if_leaked(prose_close).is_none());
    }

    #[test]
    fn strips_nested_same_tag_without_leaving_an_orphan_close() {
        let s = "<tool_calls>a<tool_calls>b</tool_calls>c</tool_calls>";
        assert_eq!(strip_leaked_tool_calls(s), "");
        assert_eq!(strip_if_leaked(s).as_deref(), Some(""));
    }

    #[test]
    fn strips_sibling_blocks_keeping_prose_between() {
        let s = "<tool_calls>p</tool_calls>q<tool_calls>r</tool_calls>";
        assert_eq!(strip_leaked_tool_calls(s), "q");
    }

    #[test]
    fn keeps_prose_and_unbalanced_tail_after_a_balanced_block() {
        let s = "<tool_calls>a</tool_calls>b<tool_calls>c";
        assert_eq!(strip_leaked_tool_calls(s), "b<tool_calls>c");
    }

    #[test]
    fn keeps_an_inline_markup_example_inside_a_larger_explanation() {
        let answer = "When a weak model emits a tool call as text it looks like \
            <tool_calls>{\"name\":\"read_file\"}</tool_calls> and nothing runs.";
        assert!(strip_if_leaked(answer).is_none());
        // A markup-dominated message (brief lead-in + the call) is still a leak.
        let leak = "Let me read it. <tool_calls>{\"name\":\"read_file\"}</tool_calls>";
        assert_eq!(strip_if_leaked(leak).as_deref(), Some("Let me read it. "));
    }

    #[test]
    fn fingerprint_is_stable_for_identical_prefix() {
        let sys = json!({"role": "system", "content": "you are an agent"});
        let tools = vec![
            json!({"function": {"name": "a"}}),
            json!({"function": {"name": "b"}}),
        ];
        assert_eq!(
            prefix_fingerprint(&sys, &tools),
            prefix_fingerprint(&sys, &tools)
        );
    }

    #[test]
    fn fingerprint_pins_which_half_drifted() {
        let sys = json!({"role": "system", "content": "you are an agent"});
        let sys2 = json!({"role": "system", "content": "you are an agent. Current date: x."});
        let tools = vec![json!({"function": {"name": "a"}})];
        let tools2 = vec![
            json!({"function": {"name": "a"}}),
            json!({"function": {"name": "b"}}),
        ];

        let base = prefix_fingerprint(&sys, &tools);
        let sys_drift = prefix_fingerprint(&sys2, &tools);
        assert_ne!(base.0, sys_drift.0);
        assert_eq!(base.1, sys_drift.1);
        let tool_drift = prefix_fingerprint(&sys, &tools2);
        assert_eq!(base.0, tool_drift.0);
        assert_ne!(base.1, tool_drift.1);
    }
}
