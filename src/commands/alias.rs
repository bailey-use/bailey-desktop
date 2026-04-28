/**
 * AliasCommand handler — manage model aliases.
 *
 * Aliases map short names (e.g. "fast") to full model names
 * (e.g. "claude-haiku-4-5"). They are resolved at routing time
 * by any command that accepts --model.
 */
use anyhow::Result;

use crate::cli::AliasArgs;
use crate::errors::ExitCode;
use crate::services::session_store::SessionStore;
use crate::style;

pub struct AliasCommand {
    session_store: SessionStore,
}

impl AliasCommand {
    pub fn new(session_store: SessionStore) -> Self {
        Self { session_store }
    }

    pub async fn execute(&self, args: AliasArgs) -> ExitCode {
        match self.execute_internal(args).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{} {}", style::red("Error:"), e);
                ExitCode::UserError
            }
        }
    }

    async fn execute_internal(&self, args: AliasArgs) -> Result<ExitCode> {
        // `rm <name>` keyword form (matches what the help advertises) and
        // `--rm <name>` flag form. The keyword form only triggers when the
        // first positional is *exactly* "rm"; `rm=model` still creates an
        // alias literally named `rm`.
        let rm_keyword = args.assignment.as_deref() == Some("rm");
        if args.rm || rm_keyword {
            if args.json {
                anyhow::bail!("--json only applies to listing aliases");
            }
            let name = if rm_keyword {
                args.value.as_deref()
            } else {
                args.assignment.as_deref()
            };
            return self.remove_alias(name).await;
        }

        // `aivo models alias name=model` or `aivo models alias name model`
        if let Some(ref assignment) = args.assignment {
            if args.json {
                anyhow::bail!("--json only applies to listing aliases");
            }
            return self.set_alias(assignment, args.value.as_deref()).await;
        }

        // `aivo models alias` — list all
        self.list_aliases(args.json).await
    }

    async fn list_aliases(&self, json: bool) -> Result<ExitCode> {
        let aliases = self.session_store.get_aliases().await?;

        if json {
            let mut entries: Vec<_> = aliases.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let payload: serde_json::Map<String, serde_json::Value> = entries
                .into_iter()
                .map(|(name, model)| (name, serde_json::Value::String(model)))
                .collect();
            println!("{}", serde_json::to_string_pretty(&payload)?);
            return Ok(ExitCode::Success);
        }

        if aliases.is_empty() {
            println!("{}", style::dim("No aliases defined."));
            println!();
            println!(
                "{}",
                style::dim("Create one with: aivo models alias fast=claude-haiku-4-5")
            );
            return Ok(ExitCode::Success);
        }

        let mut entries: Vec<_> = aliases.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let max_name = entries.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        for (name, model) in &entries {
            println!(
                "{:width$} {} {}",
                style::cyan(name),
                style::dim("->"),
                model,
                width = max_name
            );
        }
        Ok(ExitCode::Success)
    }

    async fn set_alias(&self, assignment: &str, extra_value: Option<&str>) -> Result<ExitCode> {
        let (name, model) = if let Some((n, m)) = assignment.split_once('=') {
            (n.to_string(), m.to_string())
        } else if let Some(val) = extra_value {
            (assignment.to_string(), val.to_string())
        } else {
            eprintln!(
                "{} Expected format: aivo models alias name=model",
                style::red("Error:")
            );
            return Ok(ExitCode::UserError);
        };

        if name.is_empty() || model.is_empty() {
            eprintln!(
                "{} Both alias name and model must be non-empty",
                style::red("Error:")
            );
            return Ok(ExitCode::UserError);
        }

        // Check for self-reference
        if name == model {
            eprintln!(
                "{} Alias cannot point to itself: {}",
                style::red("Error:"),
                name
            );
            return Ok(ExitCode::UserError);
        }

        // Check for circular aliases
        let mut aliases = self.session_store.get_aliases().await?;
        aliases.insert(name.clone(), model.clone());
        if has_cycle(&aliases, &name) {
            eprintln!(
                "{} This would create a circular alias chain",
                style::red("Error:")
            );
            return Ok(ExitCode::UserError);
        }

        let prev = self
            .session_store
            .set_alias(name.clone(), model.clone())
            .await?;
        match prev {
            Some(old) => println!(
                "Updated {} {} {} (was {})",
                style::cyan(&name),
                style::dim("->"),
                model,
                style::dim(&old)
            ),
            None => println!(
                "Created {} {} {}",
                style::cyan(&name),
                style::dim("->"),
                model
            ),
        }
        Ok(ExitCode::Success)
    }

    async fn remove_alias(&self, name: Option<&str>) -> Result<ExitCode> {
        let name = match name {
            Some(n) => n,
            None => {
                eprintln!(
                    "{} Expected: aivo models alias rm <name>",
                    style::red("Error:")
                );
                return Ok(ExitCode::UserError);
            }
        };

        match self.session_store.remove_alias(name).await? {
            Some(model) => {
                println!(
                    "Removed {} {} {}",
                    style::cyan(name),
                    style::dim("->"),
                    style::dim(&model)
                );
                Ok(ExitCode::Success)
            }
            None => {
                eprintln!("{} No alias named '{}'", style::red("Error:"), name);
                Ok(ExitCode::UserError)
            }
        }
    }

    pub fn print_help() {
        println!("{} aivo models alias [name=model]", style::bold("Usage:"));
        println!();
        println!("{}", style::dim("Create, list, or remove model aliases."));
        println!();
        println!("{}", style::bold("Actions:"));
        let print_row = |label: &str, desc: &str| {
            println!(
                "  {}{}",
                style::cyan(format!("{:<14}", label)),
                style::dim(desc)
            );
        };
        print_row("(no args)", "List all aliases");
        print_row("name=model", "Create or update an alias");
        print_row("name model", "Create or update an alias");
        print_row("rm <name>", "Remove an alias");
        println!();
        println!("{}", style::bold("Options:"));
        let print_opt = |flag: &str, desc: &str| {
            println!(
                "  {}{}",
                style::cyan(format!("{:<14}", flag)),
                style::dim(desc)
            );
        };
        print_opt("--json", "Output alias list as JSON (listing only)");
        println!();
        println!("{}", style::bold("Examples:"));
        println!(
            "  {}",
            style::dim("aivo models alias fast=claude-haiku-4-5")
        );
        println!(
            "  {}",
            style::dim("aivo models alias best claude-sonnet-4-6")
        );
        println!("  {}", style::dim("aivo models alias rm fast"));
        println!("  {}", style::dim("aivo models alias"));
        println!("  {}", style::dim("aivo models alias --json"));
    }
}

/// Detects cycles in the alias map starting from `start`.
fn has_cycle(aliases: &std::collections::HashMap<String, String>, start: &str) -> bool {
    let mut seen = std::collections::HashSet::new();
    let mut current = start;
    while let Some(target) = aliases.get(current) {
        if !seen.insert(current) {
            return true;
        }
        current = target;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn has_cycle_no_cycle() {
        let mut m = HashMap::new();
        m.insert("fast".to_string(), "claude-haiku".to_string());
        m.insert("best".to_string(), "claude-sonnet".to_string());
        assert!(!has_cycle(&m, "fast"));
        assert!(!has_cycle(&m, "best"));
    }

    #[test]
    fn has_cycle_self_reference() {
        let mut m = HashMap::new();
        m.insert("a".to_string(), "a".to_string());
        assert!(has_cycle(&m, "a"));
    }

    #[test]
    fn has_cycle_two_hop() {
        let mut m = HashMap::new();
        m.insert("a".to_string(), "b".to_string());
        m.insert("b".to_string(), "a".to_string());
        assert!(has_cycle(&m, "a"));
        assert!(has_cycle(&m, "b"));
    }

    #[test]
    fn has_cycle_three_hop() {
        let mut m = HashMap::new();
        m.insert("a".to_string(), "b".to_string());
        m.insert("b".to_string(), "c".to_string());
        m.insert("c".to_string(), "a".to_string());
        assert!(has_cycle(&m, "a"));
    }

    #[test]
    fn has_cycle_chain_no_cycle() {
        let mut m = HashMap::new();
        m.insert("a".to_string(), "b".to_string());
        m.insert("b".to_string(), "c".to_string());
        // c doesn't map to anything, so no cycle
        assert!(!has_cycle(&m, "a"));
    }

    #[tokio::test]
    async fn set_and_get_aliases() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionStore::with_path(dir.path().join("config.json"));

        assert!(store.get_aliases().await.unwrap().is_empty());

        store
            .set_alias("fast".to_string(), "claude-haiku".to_string())
            .await
            .unwrap();
        let aliases = store.get_aliases().await.unwrap();
        assert_eq!(aliases.get("fast").unwrap(), "claude-haiku");
    }

    #[tokio::test]
    async fn remove_alias_returns_old_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionStore::with_path(dir.path().join("config.json"));

        store
            .set_alias("fast".to_string(), "haiku".to_string())
            .await
            .unwrap();
        let removed = store.remove_alias("fast").await.unwrap();
        assert_eq!(removed, Some("haiku".to_string()));

        let removed_again = store.remove_alias("fast").await.unwrap();
        assert_eq!(removed_again, None);
    }

    #[tokio::test]
    async fn resolve_alias_follows_chain() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionStore::with_path(dir.path().join("config.json"));

        store
            .set_alias("quick".to_string(), "fast".to_string())
            .await
            .unwrap();
        store
            .set_alias("fast".to_string(), "claude-haiku".to_string())
            .await
            .unwrap();

        let resolved = store.resolve_alias("quick").await.unwrap();
        assert_eq!(resolved, "claude-haiku");
    }

    #[tokio::test]
    async fn resolve_alias_detects_cycle() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionStore::with_path(dir.path().join("config.json"));

        store
            .set_alias("a".to_string(), "b".to_string())
            .await
            .unwrap();
        store
            .set_alias("b".to_string(), "a".to_string())
            .await
            .unwrap();

        let result = store.resolve_alias("a").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn resolve_alias_passthrough_non_alias() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionStore::with_path(dir.path().join("config.json"));

        let resolved = store.resolve_alias("claude-sonnet-4-6").await.unwrap();
        assert_eq!(resolved, "claude-sonnet-4-6");
    }
}
