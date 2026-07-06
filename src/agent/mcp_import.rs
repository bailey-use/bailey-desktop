//! Discover MCP servers configured in OTHER coding agents' config files —
//! read-only — so `aivo mcp import` can copy them into aivo's own mcp.json.
//! Codex/grok keep their servers in TOML; those are reported as unsupported
//! rather than parsed (no toml dependency).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// One importable server found in another tool's config, already normalized
/// to aivo's `{command,args,env}` / `{url,headers}` shape.
pub struct ImportedServer {
    pub name: String,
    pub config: Value,
    /// Human target for listings: the command line or the URL.
    pub display: String,
}

/// One foreign config file that exists and defines at least one importable
/// server.
pub struct ImportSource {
    pub tool: &'static str,
    pub path: PathBuf,
    pub servers: Vec<ImportedServer>,
}

/// The JSON-config tools aivo knows how to read, as
/// `(tool, path relative to $HOME, key holding the servers object)`.
const JSON_SOURCES: &[(&str, &str, &str)] = &[
    ("claude", ".claude.json", "mcpServers"),
    ("cursor", ".cursor/mcp.json", "mcpServers"),
    ("gemini", ".gemini/settings.json", "mcpServers"),
    ("copilot", ".copilot/mcp-config.json", "mcpServers"),
    ("amp", ".config/amp/settings.json", "amp.mcpServers"),
];

/// Tools whose MCP config aivo can't parse yet (TOML), listed so `import`
/// can say why they're absent instead of silently skipping them.
pub const UNSUPPORTED_TOML: &[(&str, &str)] = &[
    ("codex", ".codex/config.toml"),
    ("grok", ".grok/config.toml"),
];

/// Scan the known config locations under the real home directory.
pub fn discover() -> Vec<ImportSource> {
    match crate::services::system_env::home_dir() {
        Some(home) => discover_from(&home),
        None => Vec::new(),
    }
}

/// Inner with the home dir injected so tests can plant fixture files.
pub fn discover_from(home: &Path) -> Vec<ImportSource> {
    let mut out = Vec::new();
    for (tool, rel, key) in JSON_SOURCES {
        let path = home.join(rel);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(root) = serde_json::from_str::<Value>(&text) else {
            continue; // unreadable foreign config → not ours to complain about
        };
        let Some(servers) = root.get(*key).and_then(|s| s.as_object()) else {
            continue;
        };
        let mut found: Vec<ImportedServer> = servers
            .iter()
            .filter_map(|(name, cfg)| {
                normalize(cfg).map(|config| ImportedServer {
                    name: name.clone(),
                    display: display_of(&config),
                    config,
                })
            })
            .collect();
        if found.is_empty() {
            continue;
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        out.push(ImportSource {
            tool,
            path,
            servers: found,
        });
    }
    out
}

/// A foreign server config → aivo's shape, or `None` when there's nothing
/// spawnable/connectable in it. Keeps only the fields aivo understands:
/// tool filters, timeouts, and trust flags don't carry over.
fn normalize(cfg: &Value) -> Option<Value> {
    let cfg = cfg.as_object()?;
    // Remote first: gemini calls streamable HTTP `httpUrl` (its `url` is
    // legacy SSE, which aivo can't speak — still imported; the connect
    // error is clearer than silently dropping the server).
    let url = cfg
        .get("httpUrl")
        .or_else(|| cfg.get("url"))
        .and_then(|u| u.as_str());
    if let Some(url) = url {
        let mut out = Map::new();
        out.insert("url".to_string(), json!(url));
        if let Some(headers) = cfg.get("headers").filter(|h| h.is_object()) {
            out.insert("headers".to_string(), headers.clone());
        }
        return Some(Value::Object(out));
    }
    let command = cfg.get("command").and_then(|c| c.as_str())?;
    let mut out = Map::new();
    out.insert("command".to_string(), json!(command));
    if let Some(args) = cfg.get("args").filter(|a| a.is_array()) {
        out.insert("args".to_string(), args.clone());
    }
    if let Some(env) = cfg.get("env").filter(|e| e.is_object()) {
        out.insert("env".to_string(), env.clone());
    }
    Some(Value::Object(out))
}

/// Listing target for a server config `Value`: the URL or the command line.
/// Also used by the TUI's paste picker rows.
pub fn display_of(config: &Value) -> String {
    if let Some(url) = config.get("url").and_then(|u| u.as_str()) {
        return url.to_string();
    }
    let command = config
        .get("command")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    let args: Vec<&str> = config
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    if args.is_empty() {
        command.to_string()
    } else {
        format!("{command} {}", args.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_shapes_per_tool() {
        // claude/cursor: stdio with env, plus type fields to drop.
        let v = normalize(&json!({
            "type": "stdio", "command": "npx", "args": ["-y", "srv"],
            "env": {"K": "V"}, "timeout": 5000
        }))
        .unwrap();
        assert_eq!(
            v,
            json!({"command": "npx", "args": ["-y", "srv"], "env": {"K": "V"}})
        );
        // claude http: url + headers survive, type dropped.
        let v = normalize(&json!({
            "type": "http", "url": "https://h/mcp",
            "headers": {"Authorization": "Bearer ${TOK}"}
        }))
        .unwrap();
        assert_eq!(
            v,
            json!({"url": "https://h/mcp", "headers": {"Authorization": "Bearer ${TOK}"}})
        );
        // gemini streamable: httpUrl wins over legacy url.
        let v = normalize(&json!({"httpUrl": "https://h/mcp", "url": "https://h/sse"})).unwrap();
        assert_eq!(v, json!({"url": "https://h/mcp"}));
        // copilot local: tools filter dropped.
        let v = normalize(&json!({
            "type": "local", "command": "docker", "args": ["run", "x"], "tools": ["*"]
        }))
        .unwrap();
        assert_eq!(v, json!({"command": "docker", "args": ["run", "x"]}));
        // Nothing spawnable → skipped.
        assert!(normalize(&json!({"comment": "stub"})).is_none());
        assert!(normalize(&json!("not an object")).is_none());
    }

    #[test]
    fn discover_from_plants() {
        let dir = std::env::temp_dir().join(format!("aivo-mcp-import-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".cursor")).unwrap();
        std::fs::create_dir_all(dir.join(".config/amp")).unwrap();
        std::fs::write(
            dir.join(".claude.json"),
            r#"{"mcpServers":{"gh":{"command":"npx","args":["-y","@x/gh"]},"stub":{}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(".cursor/mcp.json"),
            r#"{"mcpServers":{"linear":{"url":"https://mcp.linear.app/mcp"}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(".config/amp/settings.json"),
            r#"{"amp.mcpServers":{"pw":{"command":"npx","args":["@playwright/mcp"]}}}"#,
        )
        .unwrap();
        // Present but empty/irrelevant → no source entry.
        std::fs::write(dir.join(".gemini-nope.json"), "{}").unwrap();

        let sources = discover_from(&dir);
        let tools: Vec<&str> = sources.iter().map(|s| s.tool).collect();
        assert_eq!(tools, ["claude", "cursor", "amp"]);
        let claude = &sources[0];
        assert_eq!(claude.servers.len(), 1, "empty stub entry is skipped");
        assert_eq!(claude.servers[0].name, "gh");
        assert_eq!(claude.servers[0].display, "npx -y @x/gh");
        assert_eq!(sources[1].servers[0].display, "https://mcp.linear.app/mcp");

        std::fs::remove_dir_all(&dir).ok();
    }
}
