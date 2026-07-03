//! Message and request shaping for the agent engine: converting tool specs and
//! assistant replies to OpenAI chat-wire JSON, and rendering the transcript to
//! plain text for the summarizer. Pure functions over serde_json values.

use crate::agent::protocol::{AssistantMessage, ToolSpec};
use serde_json::{Map, Value, json};

pub(crate) fn tool_to_openai(t: ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {"name": t.name, "description": t.description, "parameters": t.parameters}
    })
}

/// Convert an assistant reply to an OpenAI chat message for the history (`arguments`
/// as a string, `content` present when there are no tool calls).
pub(crate) fn assistant_to_openai(m: &AssistantMessage) -> Value {
    let mut msg = Map::new();
    msg.insert("role".into(), json!("assistant"));
    if let Some(c) = &m.content
        && !c.is_empty()
    {
        msg.insert("content".into(), json!(c));
    }
    if !m.tool_calls.is_empty() {
        let calls: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "arguments": serde_json::to_string(&t.arguments).unwrap_or_else(|_| "{}".into()),
                    }
                })
            })
            .collect();
        msg.insert("tool_calls".into(), json!(calls));
    } else if !msg.contains_key("content") {
        msg.insert("content".into(), json!(""));
    }
    Value::Object(msg)
}

pub(crate) fn role(m: &Value) -> &str {
    m.get("role").and_then(|r| r.as_str()).unwrap_or("")
}

pub(crate) fn content_str(m: &Value) -> String {
    m.get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string()
}

pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}… (+{} chars)", s.chars().count() - max)
}

/// Render messages to a plain transcript for the summarizer (tool results
/// capped at 2000 chars so the summarization request stays tractable).
pub(crate) fn serialize_transcript(messages: &[Value]) -> String {
    let mut out = String::new();
    for m in messages {
        match role(m) {
            "user" => out.push_str(&format!("[User]: {}\n", content_str(m))),
            "assistant" => {
                let c = content_str(m);
                if !c.is_empty() {
                    out.push_str(&format!("[Assistant]: {c}\n"));
                }
                if let Some(calls) = m.get("tool_calls").and_then(|t| t.as_array()) {
                    let rendered: Vec<String> = calls
                        .iter()
                        .filter_map(|tc| {
                            let f = tc.get("function")?;
                            let name = f.get("name")?.as_str()?;
                            let args = f.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
                            Some(format!("{name}({})", truncate_str(args, 200)))
                        })
                        .collect();
                    if !rendered.is_empty() {
                        out.push_str(&format!("[Tool calls]: {}\n", rendered.join("; ")));
                    }
                }
            }
            "tool" => out.push_str(&format!(
                "[Tool result]: {}\n",
                truncate_str(&content_str(m), 2000)
            )),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_transcript_renders_roles() {
        let messages = vec![
            json!({"role":"user","content":"do X"}),
            json!({"role":"assistant","content":"","tool_calls":[
                {"function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}
            ]}),
            json!({"role":"tool","content":"file contents"}),
        ];
        let t = serialize_transcript(&messages);
        assert!(t.contains("[User]: do X"));
        assert!(t.contains("[Tool calls]: read_file("));
        assert!(t.contains("[Tool result]: file contents"));
    }

    #[test]
    fn truncate_str_marks_overflow() {
        assert_eq!(truncate_str("abc", 5), "abc");
        let out = truncate_str("abcdefgh", 3);
        assert!(out.starts_with("abc…") && out.contains("+5 chars"));
    }
}
