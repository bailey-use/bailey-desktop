//! The opt-in edit-review gate: with the "review edits" toggle on, an edit-bearing
//! batch pauses to show its diffs before writing. Engine-side stays thin — collect
//! the edit calls into [`ReviewItem`]s and await [`AgentUi::review_edits`]; the TUI
//! computes and renders the diffs. Interactive `aivo code` only.

use serde_json::Value;

/// The file tools this gate governs; `run_bash` and other side effects are out of scope.
pub const EDIT_TOOLS: [&str; 4] = ["write_file", "edit_file", "multi_edit", "apply_patch"];

/// True when `name` is one of the edit tools the review gate intercepts.
pub fn is_edit_tool(name: &str) -> bool {
    EDIT_TOOLS.contains(&name)
}

/// One pending edit awaiting review: the raw call (`tool` + `args`) the TUI diffs,
/// `call_index` to map the verdict back onto the batch, and `paths` for the heading.
#[derive(Clone, Debug)]
pub struct ReviewItem {
    pub call_index: usize,
    pub tool: String,
    pub paths: Vec<String>,
    pub args: Value,
}

/// The user's verdict on a reviewed batch: run the edits, or drop them all (the
/// model gets [`REVIEW_REJECTED_DIRECTIVE`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    ApproveAll,
    Reject,
}

/// The tool result a rejected edit reports back to the model — a directive to stop
/// and ask, not to silently re-apply the same change.
pub const REVIEW_REJECTED_DIRECTIVE: &str = "The user reviewed this edit and chose NOT to apply it \
— nothing was written. Do not silently retry the same change. Stop and ask the user what they'd \
like different before editing this file again.";

/// Best-effort extraction of the file paths a tool call targets, for the review
/// heading. Mirrors how `tools::is_dangerous` reads paths; never touches disk.
pub fn edited_paths(name: &str, args: &Value) -> Vec<String> {
    let one = |key: &str| {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .into_iter()
            .collect::<Vec<_>>()
    };
    match name {
        "write_file" | "edit_file" | "multi_edit" => one("path"),
        "apply_patch" => args
            .get("input")
            .and_then(|v| v.as_str())
            .map(crate::agent::apply_patch::target_paths)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Build a [`ReviewItem`] for tool call `call_index` (assumed an edit tool).
pub fn review_item(call_index: usize, name: &str, args: &Value) -> ReviewItem {
    ReviewItem {
        call_index,
        tool: name.to_string(),
        paths: edited_paths(name, args),
        args: args.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edit_tool_membership() {
        assert!(is_edit_tool("write_file"));
        assert!(is_edit_tool("apply_patch"));
        assert!(!is_edit_tool("run_bash"));
        assert!(!is_edit_tool("read_file"));
    }

    #[test]
    fn paths_from_simple_edit_tools() {
        assert_eq!(
            edited_paths("write_file", &json!({"path": "a.txt", "content": "x"})),
            vec!["a.txt".to_string()]
        );
        assert_eq!(
            edited_paths("edit_file", &json!({"path": "src/lib.rs"})),
            vec!["src/lib.rs".to_string()]
        );
        assert!(edited_paths("write_file", &json!({"content": "x"})).is_empty());
    }

    #[test]
    fn review_item_captures_call() {
        let item = review_item(
            3,
            "write_file",
            &json!({"path": "out.txt", "content": "hi"}),
        );
        assert_eq!(item.call_index, 3);
        assert_eq!(item.tool, "write_file");
        assert_eq!(item.paths, vec!["out.txt".to_string()]);
        assert_eq!(item.args["content"], "hi");
    }
}
