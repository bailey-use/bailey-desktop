//! The `take_note` tool: a durable scratchpad pinned verbatim into every compaction
//! fold and rebuilt from the log on resume, so decisions/findings/dead-ends outlive the
//! immediate steps. Entries are `(optional id, text)` merged deterministically (ACE-style,
//! no LLM rewriting): reusing an `id` updates in place, and duplicate text refreshes
//! recency instead of stacking — so a long run doesn't collapse into near-duplicates.

use crate::agent::protocol::ToolSpec;
use serde_json::{Value, json};

/// One scratchpad entry; `id` is an optional slug reused to revise in place.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub id: Option<String>,
    pub text: String,
}

/// What [`merge_note`] did, for the model-facing confirmation.
pub enum MergeOutcome {
    Added(usize),
    Updated(String),
    Refreshed,
}

/// The `take_note` function schema, offered on every turn (the system prompt
/// tells the model when to use it). Handled inline by the engine, like
/// `update_plan` — it isn't dispatched to `tools::execute`.
pub fn note_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "take_note".to_string(),
        description: "Save a short, durable note to your scratchpad during a long, multi-step \
task — a decision made, a finding, a dead-end to avoid, or what to do next. Notes persist \
verbatim even after older conversation is compacted away, so use this to keep track of progress \
and context that must outlive the immediate steps. One concise note per call; reuse `id` to \
revise a note instead of repeating it. Skip it for quick work."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "note": {
                    "type": "string",
                    "description": "A concise note to remember (one fact, decision, finding, or next step)."
                },
                "id": {
                    "type": "string",
                    "description": "Optional short stable slug (e.g. 'auth-approach'). Reuse an id to UPDATE that note in place instead of adding a near-duplicate."
                }
            },
            "required": ["note"]
        }),
    }
}

/// Extract a trimmed, non-empty [`Note`] (with optional `id`) from a `take_note` call's arguments.
pub fn parse_note(args: &Value) -> Result<Note, String> {
    let text = args
        .get("note")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "take_note: missing `note`".to_string())?;
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(Note { id, text })
}

/// Deterministic merge: same id → update in place; same normalized text → refresh
/// recency; else append (capped oldest-first).
pub fn merge_note(notes: &mut Vec<Note>, new: Note, cap: usize) -> MergeOutcome {
    if let Some(id) = &new.id
        && let Some(pos) = notes.iter().position(|n| n.id.as_deref() == Some(id))
    {
        notes[pos].text = new.text;
        // Drop any OTHER note the update now duplicates, so the no-near-duplicates invariant holds.
        let id = id.clone();
        let norm = normalized(&notes[pos].text);
        notes.retain(|n| n.id.as_deref() == Some(&id) || normalized(&n.text) != norm);
        return MergeOutcome::Updated(id);
    }
    if let Some(pos) = notes
        .iter()
        .position(|n| normalized(&n.text) == normalized(&new.text))
    {
        let mut existing = notes.remove(pos);
        existing.id = existing.id.or(new.id); // adopt a new id if the old had none
        notes.push(existing);
        return MergeOutcome::Refreshed;
    }
    notes.push(new);
    while notes.len() > cap {
        notes.remove(0);
    }
    MergeOutcome::Added(notes.len())
}

/// Case/whitespace-insensitive key for duplicate detection.
fn normalized(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_has_name_and_required_note_with_optional_id() {
        let s = note_tool_spec();
        assert_eq!(s.name, "take_note");
        assert_eq!(s.parameters["required"][0], "note");
        // id is offered but not required.
        assert!(s.parameters["properties"].get("id").is_some());
        assert_eq!(s.parameters["required"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn parse_note_trims_and_rejects_empty() {
        assert_eq!(parse_note(&json!({"note": "  hi  "})).unwrap().text, "hi");
        assert!(parse_note(&json!({"note": "   "})).is_err());
        assert!(parse_note(&json!({})).is_err());
    }

    #[test]
    fn parse_note_with_id() {
        let n = parse_note(&json!({"note": "use JWT", "id": " auth "})).unwrap();
        assert_eq!(n.id.as_deref(), Some("auth"));
        assert_eq!(n.text, "use JWT");
        // Empty id → None.
        let n = parse_note(&json!({"note": "x", "id": "  "})).unwrap();
        assert!(n.id.is_none());
    }

    #[test]
    fn merge_updates_by_id() {
        let mut notes = vec![Note {
            id: Some("auth".into()),
            text: "use sessions".into(),
        }];
        let out = merge_note(
            &mut notes,
            Note {
                id: Some("auth".into()),
                text: "use JWT".into(),
            },
            50,
        );
        assert!(matches!(out, MergeOutcome::Updated(id) if id == "auth"));
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].text, "use JWT");
    }

    #[test]
    fn merge_by_id_dedups_a_note_it_now_duplicates() {
        let mut notes = vec![
            Note {
                id: None,
                text: "use JWT".into(),
            },
            Note {
                id: Some("auth".into()),
                text: "use sessions".into(),
            },
        ];
        // Updating "auth" to text that matches the other note collapses the duplicate.
        let out = merge_note(
            &mut notes,
            Note {
                id: Some("auth".into()),
                text: "use JWT".into(),
            },
            50,
        );
        assert!(matches!(out, MergeOutcome::Updated(id) if id == "auth"));
        assert_eq!(notes.len(), 1, "the duplicate note is dropped");
        assert_eq!(notes[0].id.as_deref(), Some("auth"));
        assert_eq!(notes[0].text, "use JWT");
    }

    #[test]
    fn merge_refreshes_exact_dup() {
        let mut notes = vec![
            Note {
                id: None,
                text: "first".into(),
            },
            Note {
                id: None,
                text: "Decided On X".into(),
            },
        ];
        // Case/whitespace variant of an existing note refreshes recency, not stacks.
        let out = merge_note(
            &mut notes,
            Note {
                id: None,
                text: "decided   on x".into(),
            },
            50,
        );
        assert!(matches!(out, MergeOutcome::Refreshed));
        assert_eq!(notes.len(), 2);
        assert_eq!(notes.last().unwrap().text, "Decided On X", "moved to back");
    }

    #[test]
    fn merge_refresh_adopts_id_when_old_had_none() {
        let mut notes = vec![Note {
            id: None,
            text: "finding".into(),
        }];
        merge_note(
            &mut notes,
            Note {
                id: Some("f1".into()),
                text: "finding".into(),
            },
            50,
        );
        assert_eq!(notes[0].id.as_deref(), Some("f1"));
    }

    #[test]
    fn merge_caps_oldest_first() {
        let mut notes = Vec::new();
        for i in 0..5 {
            merge_note(
                &mut notes,
                Note {
                    id: None,
                    text: format!("n{i}"),
                },
                3,
            );
        }
        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0].text, "n2", "oldest dropped");
        assert_eq!(notes[2].text, "n4");
    }
}
