//! The `take_note` tool: a durable scratchpad the agent appends to during long,
//! multi-step work. Unlike ordinary tool output — which is summarized away (or
//! cleared) when the context is compacted — notes are pinned verbatim into every
//! compaction fold and rebuilt from the log on resume, so they survive to the end
//! of a long-horizon run. This is the agentic-memory pattern: keep the decisions,
//! findings, and dead-ends that must outlive the immediate steps.

use crate::agent::protocol::ToolSpec;
use serde_json::{Value, json};

/// The `take_note` function schema, offered on every turn (the system prompt
/// tells the model when to use it). Handled inline by the engine, like
/// `update_plan` — it isn't dispatched to `tools::execute`.
pub fn note_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "take_note".to_string(),
        description: "Save a short, durable note to your scratchpad during a long, multi-step \
task — a decision made, a finding, a dead-end to avoid, or what to do next. Notes persist \
verbatim even after older conversation is compacted away, so use this to keep track of progress \
and context that must outlive the immediate steps. One concise note per call. Skip it for quick \
work."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "note": {
                    "type": "string",
                    "description": "A concise note to remember (one fact, decision, finding, or next step)."
                }
            },
            "required": ["note"]
        }),
    }
}

/// Extract a trimmed, non-empty note from a `take_note` call's arguments.
pub fn parse_note(args: &Value) -> Result<String, String> {
    args.get("note")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "take_note: missing `note`".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_has_name_and_required_note() {
        let s = note_tool_spec();
        assert_eq!(s.name, "take_note");
        assert_eq!(s.parameters["required"][0], "note");
    }

    #[test]
    fn parse_note_trims_and_rejects_empty() {
        assert_eq!(parse_note(&json!({"note": "  hi  "})).unwrap(), "hi");
        assert!(parse_note(&json!({"note": "   "})).is_err());
        assert!(parse_note(&json!({})).is_err());
    }
}
