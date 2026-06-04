//! External-subcommand plugin dispatch: `aivo <name>` / `aivo run <name>` for a
//! name aivo doesn't own runs a sibling `aivo-<name>` binary (git/cargo style).
//! Checked before clap, and only when the sibling exists, so built-ins, tools,
//! and the chat shortcut always win.

pub(crate) mod manifest;
pub(crate) mod registry;
pub(crate) mod source;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::constants::{KNOWN_TOOLS, RESERVED_ALIAS_NAMES};
use crate::errors::ExitCode;
use crate::services::path_search::{collect_path_dirs, find_in_dirs, is_executable};
use crate::services::session_store::BundleAlias;
use crate::style;

pub(crate) const PLUGIN_PREFIX: &str = "aivo-";

/// Run the matching `aivo-<name>` plugin and return its exit code, or `None` if
/// none applies. Call before `Cli::parse_from` — clap rejects unknown subcommands.
pub async fn try_dispatch(
    raw_args: &[String],
    bundles: &HashMap<String, BundleAlias>,
    config_dir: &Path,
) -> Option<i32> {
    let (name, plugin_args) = resolve_invocation(raw_args, bundles)?;
    let bin = discover(name)?;
    Some(exec_plugin(&bin, plugin_args, config_dir).await)
}

/// Every installed plugin as `(name, path)` from one search-path sweep; first
/// match per name wins (managed dir → exe dir → `$PATH`), like `discover`.
pub fn installed_plugins() -> Vec<(String, PathBuf)> {
    let mut found: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    for dir in search_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_executable(&path) {
                continue;
            }
            // `file_stem` drops the `.exe`/`.cmd` extension on Windows.
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(name) = stem.strip_prefix(PLUGIN_PREFIX)
                && !name.is_empty()
            {
                found.entry(name.to_string()).or_insert(path);
            }
        }
    }
    found.into_iter().collect()
}

/// Bare plugin names for `--help` / `--help-json` listings.
pub fn installed_plugin_names() -> Vec<String> {
    installed_plugins()
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// argv → `(plugin_name, args_after_name)`, or `None` if no plugin applies.
/// `aivo amp …` and `aivo run amp …` both yield the same `aivo-amp …`.
fn resolve_invocation<'a>(
    raw_args: &'a [String],
    bundles: &HashMap<String, BundleAlias>,
) -> Option<(&'a str, &'a [String])> {
    let first = raw_args.get(1)?;
    if first == "run" {
        // `aivo run <name> …` — forward an unknown run-tool to its sibling.
        let name = raw_args.get(2)?;
        return dispatchable(name, bundles).then_some((name.as_str(), &raw_args[3..]));
    }
    dispatchable(first, bundles).then_some((first.as_str(), &raw_args[2..]))
}

/// True when `name` is eligible for plugin dispatch — aivo doesn't own it (not a
/// built-in, tool, chat ref, or user bundle).
fn dispatchable(name: &str, bundles: &HashMap<String, BundleAlias>) -> bool {
    if name.is_empty() || name.starts_with('-') {
        return false;
    }
    if is_reserved_plugin_name(name) {
        return false;
    }
    // Chat refs (mirrors rewrite_cli_args).
    if name.starts_with("hf:") || name.starts_with("http://") || name.starts_with("https://") {
        return false;
    }
    !bundles.contains_key(name)
}

/// `~/.config/aivo/plugins` — the directory `aivo plugins install` manages and
/// the first place discovery looks. `None` when the home dir can't be resolved.
pub fn plugins_dir() -> Option<PathBuf> {
    crate::services::system_env::home_dir().map(|h| h.join(".config").join("aivo").join("plugins"))
}

/// Locate `aivo-<name>` across the search path.
pub fn discover(name: &str) -> Option<PathBuf> {
    find_in_dirs(&format!("{PLUGIN_PREFIX}{name}"), &search_dirs())
}

/// Directories searched for `aivo-<name>`, in priority order: the managed
/// plugins dir, then next to the running executable, then every `$PATH` entry.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(p) = plugins_dir() {
        dirs.push(p);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        dirs.push(dir.to_path_buf());
    }
    dirs.extend(collect_path_dirs());
    dirs
}

/// True when `name` is a built-in/shortcut/tool aivo owns — `install` refuses
/// these since the built-in always shadows the plugin.
pub fn is_reserved_plugin_name(name: &str) -> bool {
    name == "help" || RESERVED_ALIAS_NAMES.contains(&name) || KNOWN_TOOLS.contains(&name)
}

/// Infer a plugin name from an install source. Scheme-aware (github:/npm:/cargo:/
/// URL/local) — delegates to the source classifier.
pub fn infer_plugin_name(source: &str) -> Option<String> {
    source::suggested_name(source)
}

/// Spawn the plugin with stdio inherited and wait for it. Spawn-and-wait (not
/// `exec`) so aivo's own cleanup paths still run. Returns the child's exit code.
async fn exec_plugin(bin: &Path, args: &[String], config_dir: &Path) -> i32 {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args);
    // Minimal handoff; richer context (key, serve URL) added when a plugin needs it.
    cmd.env("AIVO_CONFIG_DIR", config_dir);
    if let Some(log) = debug_log_path(args) {
        cmd.env("AIVO_DEBUG_LOG", log);
    }
    match cmd.status().await {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!(
                "{} failed to launch plugin {}: {e}",
                style::red("Error:"),
                bin.display(),
            );
            ExitCode::UserError.code()
        }
    }
}

/// Resolve the debug-log path to hand a plugin, mirroring aivo's `--debug`
/// handling: `--debug=<path>` uses that path, bare `--debug` uses the shared
/// default. `None` when `--debug` isn't present.
fn debug_log_path(args: &[String]) -> Option<std::ffi::OsString> {
    for a in args {
        if a == "--debug" {
            return Some(crate::services::http_debug::default_log_path().into_os_string());
        }
        if let Some(rest) = a.strip_prefix("--debug=") {
            let path = if rest.is_empty() {
                crate::services::http_debug::default_log_path()
            } else {
                PathBuf::from(rest)
            };
            return Some(path.into_os_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn no_bundles() -> HashMap<String, BundleAlias> {
        HashMap::new()
    }

    #[test]
    fn unknown_top_level_name_dispatches_with_remaining_args() {
        let a = args(&["aivo", "amp", "trust", "list"]);
        let (name, rest) = resolve_invocation(&a, &no_bundles()).unwrap();
        assert_eq!(name, "amp");
        assert_eq!(rest, &["trust".to_string(), "list".to_string()]);
    }

    #[test]
    fn run_form_strips_run_and_name() {
        let a = args(&["aivo", "run", "amp", "-m", "x"]);
        let (name, rest) = resolve_invocation(&a, &no_bundles()).unwrap();
        assert_eq!(name, "amp");
        assert_eq!(rest, &["-m".to_string(), "x".to_string()]);
    }

    #[test]
    fn no_args_or_bare_run_is_not_a_plugin() {
        assert!(resolve_invocation(&args(&["aivo"]), &no_bundles()).is_none());
        assert!(resolve_invocation(&args(&["aivo", "run"]), &no_bundles()).is_none());
    }

    #[test]
    fn builtins_tools_and_flags_are_never_plugins() {
        for name in ["keys", "chat", "serve", "logs", "help", "image", "audio"] {
            assert!(
                resolve_invocation(&args(&["aivo", name]), &no_bundles()).is_none(),
                "{name} must not dispatch as a plugin"
            );
        }
        assert!(resolve_invocation(&args(&["aivo", "claude"]), &no_bundles()).is_none());
        assert!(resolve_invocation(&args(&["aivo", "run", "codex"]), &no_bundles()).is_none());
        assert!(resolve_invocation(&args(&["aivo", "--help"]), &no_bundles()).is_none());
        assert!(resolve_invocation(&args(&["aivo", "hf:owner/repo"]), &no_bundles()).is_none());
        assert!(
            resolve_invocation(&args(&["aivo", "https://example.com"]), &no_bundles()).is_none()
        );
    }

    #[test]
    fn user_bundle_wins_over_plugin() {
        let mut bundles = HashMap::new();
        bundles.insert(
            "myflow".to_string(),
            BundleAlias {
                tool: "claude".to_string(),
                args: vec![],
            },
        );
        assert!(resolve_invocation(&args(&["aivo", "myflow"]), &bundles).is_none());
    }

    #[test]
    fn infer_name_from_sources() {
        assert_eq!(infer_plugin_name("aivo-amp").as_deref(), Some("amp"));
        assert_eq!(infer_plugin_name("./bin/aivo-amp").as_deref(), Some("amp"));
        assert_eq!(
            infer_plugin_name("https://x.dev/dl/aivo-amp.exe?v=1").as_deref(),
            Some("amp")
        );
        assert_eq!(
            infer_plugin_name("/usr/local/bin/mytool").as_deref(),
            Some("mytool")
        );
        assert_eq!(infer_plugin_name(""), None);
    }

    #[test]
    fn reserved_names_are_rejected() {
        for n in ["keys", "chat", "run", "claude", "image", "help", "plugins"] {
            assert!(is_reserved_plugin_name(n), "{n} should be reserved");
        }
        assert!(!is_reserved_plugin_name("amp"));
    }

    #[test]
    fn debug_log_path_handling() {
        assert!(debug_log_path(&args(&["--debug"])).is_some());
        assert!(debug_log_path(&args(&["--debug="])).is_some());
        assert_eq!(
            debug_log_path(&args(&["--debug=/tmp/x.jsonl"])),
            Some(std::ffi::OsString::from("/tmp/x.jsonl"))
        );
        assert!(debug_log_path(&args(&["trust", "list"])).is_none());
    }
}
