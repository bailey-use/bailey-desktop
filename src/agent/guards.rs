//! Loop-safety and progress gates for the agent engine: cheap signatures to detect
//! no-progress (identical consecutive tool-call batches) and read-paging loops a plain
//! batch signature misses, plus the tool-failure and completion heuristics. Pure
//! functions over the tool-call list.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::agent::protocol::ToolCall;

/// Signature of a tool-call batch, ignoring call ids (which vary) but not args —
/// two consecutive identical signatures mean the model is looping.
pub(crate) fn batch_sig(calls: &[ToolCall]) -> String {
    calls
        .iter()
        .map(|c| format!("{}:{}", c.name, c.arguments))
        .collect::<Vec<_>>()
        .join("|")
}

/// Effective `(path, offset)` for a lone `read_file` call (`start_line` resolves to
/// `offset`). Held constant across steps → a paging loop `batch_sig` misses.
pub(crate) fn page_read_key(calls: &[ToolCall]) -> Option<(String, u64)> {
    let [call] = calls else { return None };
    if call.name != "read_file" {
        return None;
    }
    let path = call.arguments.get("path")?.as_str()?.to_string();
    let offset = call
        .arguments
        .get("offset")
        .or_else(|| call.arguments.get("start_line"))
        .and_then(Value::as_u64)
        .unwrap_or(1);
    Some((path, offset))
}

/// After this many same-signature tool failures, feed the tool's schema back so the
/// model self-corrects; after the higher bound, hard-stop the turn (a genuine loop).
pub(crate) const TOOL_FAILURE_HINT_AT: usize = 2;
pub(crate) const TOOL_FAILURE_STOP_AT: usize = 4;

/// Signature of a tool failure: the tool name + the error's leading text, so
/// cosmetically-varying args that fail the same way collapse to one streak.
pub(crate) fn failure_signature(tool: &str, error: &str) -> String {
    let head: String = error.trim().chars().take(80).collect();
    format!("{tool}\u{0}{head}")
}

/// Permission / plan-mode denials aren't malformed-call loops — a schema hint doesn't
/// apply, and the no-progress guard already stops identical retries. Don't count them.
pub(crate) fn is_policy_denial(error: &str) -> bool {
    let e = error.trim_start();
    e.starts_with("denied by user") || e.starts_with("Plan mode is read-only")
}

/// What to do about this batch's repeated tool failures.
pub(crate) enum FailureAction {
    None,
    /// Feed the tool's schema + the exact error back once, so the model self-corrects.
    Hint {
        tool: String,
        error: String,
    },
    /// The model kept failing the same way — stop the turn rather than burn steps.
    Stop,
}

/// Tracks per-signature tool-failure streaks across a turn.
#[derive(Default)]
pub(crate) struct FailureGuard {
    counts: HashMap<String, usize>,
    hinted: HashSet<String>,
}

impl FailureGuard {
    /// Fold this batch's `(tool, error)` failures in and return the strongest action:
    /// `Stop` as soon as any signature reaches [`TOOL_FAILURE_STOP_AT`], else `Hint`
    /// the first signature to reach [`TOOL_FAILURE_HINT_AT`] (once per signature).
    pub(crate) fn observe(&mut self, failures: &[(String, String)]) -> FailureAction {
        let mut action = FailureAction::None;
        for (tool, error) in failures {
            if is_policy_denial(error) {
                continue;
            }
            let sig = failure_signature(tool, error);
            let n = self.counts.entry(sig.clone()).or_insert(0);
            *n += 1;
            if *n >= TOOL_FAILURE_STOP_AT {
                return FailureAction::Stop;
            }
            if *n >= TOOL_FAILURE_HINT_AT
                && !matches!(action, FailureAction::Hint { .. })
                && self.hinted.insert(sig)
            {
                action = FailureAction::Hint {
                    tool: tool.clone(),
                    error: error.clone(),
                };
            }
        }
        action
    }
}

/// Whether a final (tool-less) answer admits the task ISN'T done — used only for
/// unattended `-e` runs to nudge the model to continue rather than stop short.
/// Deliberately narrow (first-person inability to *complete/finish*) and it excludes
/// the common false positive: "I couldn't find any …" is a valid result, not a stall.
pub(crate) fn is_incomplete_answer(text: &str) -> bool {
    let t = text.trim().to_ascii_lowercase();
    const INCOMPLETE: &[&str] = &[
        "i was unable to complete",
        "i wasn't able to complete",
        "i couldn't complete",
        "i could not complete",
        "unable to complete the task",
        "i didn't finish",
        "i did not finish",
        "i couldn't finish",
        "i could not finish",
        "i was unable to finish",
        "i ran out of",
        "this is incomplete",
        "the task is incomplete",
        "the implementation is incomplete",
        "i'll need to continue",
    ];
    INCOMPLETE.iter().any(|p| t.contains(p))
}

/// Whether a final message trails off mid-step — ends on an action lead-in + colon,
/// e.g. "Let me run the tests:" — so the model promised the next action but stopped.
pub(crate) fn ends_with_continuation_cue(text: &str) -> bool {
    let t = text.trim_end();
    if !t.ends_with(':') {
        return false;
    }
    // The trailing clause: whatever follows the last sentence break or newline.
    let clause = t
        .rsplit(['\n', '.', '!', '?'])
        .find(|s| !s.trim().is_empty())
        .unwrap_or(t)
        .trim()
        .to_ascii_lowercase();
    const LEADS: &[&str] = &[
        "let me", "let's", "now i", "now let", "next, i", "i'll", "i will", "first, i", "first i",
    ];
    LEADS.iter().any(|p| clause.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn batch_sig_ignores_id_but_not_args() {
        let call = |id: &str, path: &str| ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: json!({ "path": path }),
        };
        assert_eq!(batch_sig(&[call("1", "a")]), batch_sig(&[call("2", "a")]));
        assert_ne!(batch_sig(&[call("1", "a")]), batch_sig(&[call("1", "b")]));
    }

    #[test]
    fn page_read_key_tracks_effective_region_not_junk_args() {
        let call = |args: Value| ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: args,
        };
        // Same region, varying ignored arg → same key (what `batch_sig` misses).
        assert_eq!(
            page_read_key(&[call(json!({"path":"a","offset":1,"limit":10}))]),
            page_read_key(&[call(json!({"path":"a","offset":1,"limit":99}))]),
        );
        assert_eq!(
            page_read_key(&[call(json!({"path":"a","start_line":7}))]),
            Some(("a".to_string(), 7)),
        );
        // Advancing offset → different key, so legit paging isn't flagged.
        assert_ne!(
            page_read_key(&[call(json!({"path":"a","offset":1}))]),
            page_read_key(&[call(json!({"path":"a","offset":31}))]),
        );
        assert_eq!(
            page_read_key(&[call(json!({"path":"a"})), call(json!({"path":"b"}))]),
            None
        );
        let grep = ToolCall {
            id: "1".into(),
            name: "grep".into(),
            arguments: json!({"path":"a"}),
        };
        assert_eq!(page_read_key(&[grep]), None);
    }

    #[test]
    fn failure_signature_collapses_varying_tails_not_tools() {
        // First 80 chars decide the signature: a differing tail past 80 collapses.
        let head = "e".repeat(80);
        assert_eq!(
            failure_signature("edit_file", &format!("{head}AAAA")),
            failure_signature("edit_file", &format!("{head}BBBB")),
        );
        // Different leading error, or different tool → different signature.
        assert_ne!(
            failure_signature("edit_file", "no match for old_string"),
            failure_signature("edit_file", "file not found"),
        );
        assert_ne!(
            failure_signature("edit_file", "same error"),
            failure_signature("read_file", "same error"),
        );
    }

    #[test]
    fn failure_guard_hints_once_then_stops_on_a_repeating_error() {
        let mut g = FailureGuard::default();
        let f = |e: &str| vec![("edit_file".to_string(), e.to_string())];
        assert!(matches!(g.observe(&f("no match")), FailureAction::None)); // 1
        assert!(matches!(
            g.observe(&f("no match")),
            FailureAction::Hint { .. }
        )); // 2 → hint
        assert!(matches!(g.observe(&f("no match")), FailureAction::None)); // 3 → already hinted
        assert!(matches!(g.observe(&f("no match")), FailureAction::Stop)); // 4 → stop
    }

    #[test]
    fn failure_guard_ignores_policy_denials() {
        let mut g = FailureGuard::default();
        let denied = vec![("run_bash".to_string(), "denied by user".to_string())];
        for _ in 0..6 {
            assert!(matches!(g.observe(&denied), FailureAction::None));
        }
    }

    #[test]
    fn failure_guard_keys_streaks_per_signature() {
        // Two different errors each need their own streak; neither alone trips stop.
        let mut g = FailureGuard::default();
        let a = vec![("edit_file".to_string(), "error A".to_string())];
        let b = vec![("edit_file".to_string(), "error B".to_string())];
        assert!(matches!(g.observe(&a), FailureAction::None));
        assert!(matches!(g.observe(&b), FailureAction::None));
        assert!(matches!(g.observe(&a), FailureAction::Hint { .. })); // A hits 2
        assert!(matches!(g.observe(&b), FailureAction::Hint { .. })); // B hits 2 independently
    }

    #[test]
    fn incomplete_answer_flags_admissions_not_negative_results() {
        assert!(is_incomplete_answer(
            "I was unable to complete the migration."
        ));
        assert!(is_incomplete_answer(
            "This is incomplete — I'll need to continue next time."
        ));
        assert!(is_incomplete_answer("I ran out of time before finishing."));
        // Not a stall: a negative *result* is a valid completion.
        assert!(!is_incomplete_answer(
            "I could not find any bugs in the code."
        ));
        assert!(!is_incomplete_answer("Done — all tests pass."));
        assert!(!is_incomplete_answer("There were no issues to fix."));
    }

    #[test]
    fn continuation_cue_flags_trailing_action_lead_ins() {
        assert!(ends_with_continuation_cue(
            "Sounds good. Let me run the tests:"
        ));
        assert!(ends_with_continuation_cue("First, I'll read the config:"));
        assert!(ends_with_continuation_cue("Now let me check the output:"));
        // Not a cue: a colon that isn't an action lead-in, or a normal closer.
        assert!(!ends_with_continuation_cue(
            "Here are the results: all green."
        ));
        assert!(!ends_with_continuation_cue("The files changed are:"));
        assert!(!ends_with_continuation_cue(
            "Fixed the bug and verified the tests pass."
        ));
    }
}
