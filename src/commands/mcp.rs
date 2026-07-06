//! `aivo mcp` — manage the coding agent's MCP servers from the CLI (list/add/rm).
//! The interactive twin is `/mcp` inside `aivo code`; both edit the user
//! `~/.config/aivo/mcp.json` and read the repo `.mcp.json` (project scope).

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};

use crate::agent::mcp;
use crate::cli::{McpAddArgs, McpArgs, McpRemoveArgs, McpSubcommand};
use crate::errors::ExitCode;
use crate::services::session_store::SessionStore;
use crate::style;

#[derive(Default)]
pub struct McpCommand;

impl McpCommand {
    pub fn new() -> Self {
        Self
    }

    pub async fn execute(&self, args: McpArgs) -> ExitCode {
        let cmd = args.command.unwrap_or(McpSubcommand::List);
        let result = match cmd {
            McpSubcommand::List => list_action().await,
            McpSubcommand::Add(a) => add_action(a).await,
            McpSubcommand::Remove(a) => remove_action(a).await,
        };
        match result {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{} {:#}", style::red("Error:"), e);
                crate::errors::exit_code_for_error(&e)
            }
        }
    }

    pub fn print_help() {
        println!("{} aivo mcp [SUBCOMMAND]", style::bold("Usage:"));
        println!();
        println!(
            "{}",
            style::dim(
                "Manage the coding agent's MCP servers. Interactive twin: /mcp inside `aivo code`."
            )
        );
        println!();
        println!("{}", style::bold("Subcommands:"));
        let row = |a: &str, b: &str| {
            println!("  {}  {}", style::cyan(format!("{:<26}", a)), style::dim(b));
        };
        row("list", "Show configured servers (default)");
        row("add <command> [args…]", "Add a stdio server (name derived)");
        row("add <https://url>", "Add a remote Streamable HTTP server");
        row("add '<json>'", "Add from a pasted mcpServers JSON block");
        row("rm <name>", "Remove a user-scope server");
        println!();
        println!("{}", style::bold("Files:"));
        println!(
            "  {}",
            style::dim("~/.config/aivo/mcp.json   user scope (managed here)")
        );
        println!(
            "  {}",
            style::dim("./.mcp.json               project scope (edit that file by hand)")
        );
    }
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

async fn list_action() -> Result<ExitCode> {
    let servers = mcp::configured_servers(&cwd());
    if servers.is_empty() {
        println!("No MCP servers configured.");
        println!(
            "{}",
            style::dim("Add one with `aivo mcp add <command|url|json>`.")
        );
        return Ok(ExitCode::Success);
    }
    let store = SessionStore::new();
    let disabled: HashSet<String> = store
        .get_disabled_mcp_servers()
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    let tool_optouts = store.get_disabled_mcp_tools().await.unwrap_or_default();
    // Char count, not byte len — `format!` pads by chars.
    let name_w = servers
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(4);
    for s in &servers {
        let scope = match s.scope {
            mcp::ServerScope::User => "user   ",
            mcp::ServerScope::Project => "project",
        };
        // Prefix match on the advertised (sanitized) form; display-only.
        let prefix = mcp::qualified_name(&s.name, "");
        let off_tools = tool_optouts
            .iter()
            .filter(|q| q.starts_with(&prefix))
            .count();
        let state = if disabled.contains(&s.name) {
            style::dim("off").to_string()
        } else if off_tools > 0 {
            format!("on {}", style::dim(format!("({off_tools} tools off)")))
        } else {
            "on".to_string()
        };
        println!(
            "{}  {}  {}  {}",
            style::cyan(format!("{:<name_w$}", s.name)),
            style::dim(scope),
            state,
            style::dim(&s.command),
        );
    }
    Ok(ExitCode::Success)
}

async fn add_action(args: McpAddArgs) -> Result<ExitCode> {
    const USAGE: &str = "usage: aivo mcp add <command [args…] | https://url | json>";
    let mut existing: HashSet<String> = mcp::configured_servers(&cwd())
        .into_iter()
        .map(|s| s.name)
        .collect();
    let store = SessionStore::new();

    // The shell already tokenized argv — multiple arguments are used verbatim
    // as `command args…` (re-joining and re-splitting would mangle args with
    // spaces the user quoted for their shell). A single argument is a JSON
    // block, a bare URL, or a full quoted command line (TUI-style), which
    // shlex splits.
    let (command, cmd_args) = match args.spec.as_slice() {
        [] => bail!("{USAGE}"),
        [single] => {
            let single = single.trim();
            if single.is_empty() {
                bail!("{USAGE}");
            }
            let json_input = if single.starts_with('{') {
                Some(single.to_string())
            } else {
                mcp::bare_url_to_config(single)
            };
            if let Some(json) = json_input {
                let parsed = mcp::parse_mcp_json(&json)
                    .map_err(|e| anyhow!("couldn't parse MCP config: {e}"))?;
                for (name_opt, value) in parsed {
                    let name = mcp::dedupe_name(
                        name_opt.unwrap_or_else(|| mcp::derive_name_from_value(&value)),
                        &existing,
                    );
                    mcp::add_user_server_value(&name, &value)
                        .await
                        .map_err(|e| anyhow!("failed to add `{name}`: {e}"))?;
                    // A freshly added server starts enabled, matching the TUI.
                    store.set_mcp_server_enabled(&name, true).await.ok();
                    println!("Added MCP server `{name}`");
                    if value.get("url").is_some() {
                        println!(
                            "{}",
                            style::dim(
                                "If it needs OAuth, authorize it inside `aivo code` → /mcp → Ctrl+O."
                            )
                        );
                    }
                    existing.insert(name);
                }
                return Ok(ExitCode::Success);
            }
            mcp::parse_mcp_add_input(single).map_err(|e| anyhow!(e))?
        }
        [command, rest @ ..] => {
            if command.starts_with("http://") || command.starts_with("https://") {
                bail!("unexpected arguments after a URL — {USAGE}");
            }
            (command.clone(), rest.to_vec())
        }
    };

    let name = mcp::dedupe_name(mcp::derive_server_name(&command, &cmd_args), &existing);
    mcp::add_user_server(&name, &command, &cmd_args)
        .await
        .map_err(|e| anyhow!("failed to add `{name}`: {e}"))?;
    store.set_mcp_server_enabled(&name, true).await.ok();
    println!("Added MCP server `{name}`");
    Ok(ExitCode::Success)
}

async fn remove_action(args: McpRemoveArgs) -> Result<ExitCode> {
    let name = args.name;
    let Some(server) = mcp::configured_servers(&cwd())
        .into_iter()
        .find(|s| s.name == name)
    else {
        eprintln!("No MCP server named `{name}`.");
        return Ok(ExitCode::UserError);
    };
    if server.scope == mcp::ServerScope::Project {
        // The merged view is project-wins, which can shadow a same-named user
        // entry — that one is still ours to remove.
        return match mcp::remove_user_server(&name).await {
            Ok(true) => {
                println!(
                    "Removed user-scope MCP server `{name}` (the project .mcp.json entry with this name still applies)"
                );
                Ok(ExitCode::Success)
            }
            Ok(false) => {
                eprintln!(
                    "`{name}` is defined in this repo's .mcp.json — edit that file to remove it."
                );
                Ok(ExitCode::UserError)
            }
            Err(e) => Err(anyhow!("failed to remove `{name}`: {e}")),
        };
    }
    match mcp::remove_user_server(&name).await {
        Ok(true) => {
            println!("Removed MCP server `{name}`");
            Ok(ExitCode::Success)
        }
        Ok(false) => {
            eprintln!("`{name}` was not in mcp.json.");
            Ok(ExitCode::UserError)
        }
        Err(e) => Err(anyhow!("failed to remove `{name}`: {e}")),
    }
}
