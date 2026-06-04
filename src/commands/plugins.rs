//! `aivo plugins` — manage sibling-binary plugins (install/update/list/remove).
//! Plugins are `aivo-<name>` executables in `~/.config/aivo/plugins/`; dispatch
//! lives in `crate::plugin`.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::{
    PluginInstallArgs, PluginRemoveArgs, PluginUpdateArgs, PluginsArgs, PluginsSubcommand,
};
use crate::errors::ExitCode;
use crate::plugin::manifest::{PluginManifest, probe_manifest};
use crate::plugin::registry::{self, PluginRecord};
use crate::plugin::source::{self, SourceKind};
use crate::plugin::{
    PLUGIN_PREFIX, discover, infer_plugin_name, installed_plugins, is_reserved_plugin_name,
    plugins_dir,
};
use crate::style;
use chrono::Utc;

const INSTALL_HINT: &str =
    "Install one with `aivo plugins install <source>` (path, url, github:/npm:/cargo:).";

#[derive(Default)]
pub struct PluginsCommand;

impl PluginsCommand {
    pub fn new() -> Self {
        Self
    }

    pub async fn execute(&self, args: PluginsArgs) -> ExitCode {
        let cmd = args.command.unwrap_or(PluginsSubcommand::List);
        let result = match cmd {
            PluginsSubcommand::List => list_action(),
            PluginsSubcommand::Install(a) => install_action(a).await,
            PluginsSubcommand::Update(a) => update_action(a).await,
            PluginsSubcommand::Remove(a) => remove_action(a),
        };
        match result {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{} {:#}", style::red("Error:"), e);
                ExitCode::UserError
            }
        }
    }

    pub fn print_help() {
        println!("{} aivo plugins [SUBCOMMAND]", style::bold("Usage:"));
        println!();
        println!(
            "{}",
            style::dim(
                "Manage plugins — sibling `aivo-<name>` binaries under ~/.config/aivo/plugins.\n\
                 Once installed, `aivo <name> …` (or `aivo run <name> …`) runs the plugin."
            )
        );
        println!();
        println!("{}", style::bold("Subcommands:"));
        let row = |a: &str, b: &str| {
            println!("  {}  {}", style::cyan(format!("{:<26}", a)), style::dim(b));
        };
        row(
            "list",
            "Show installed plugins and where each resolves (default)",
        );
        row(
            "install <source> [--name N]",
            "Install from a path, URL, github:owner/repo, npm:pkg, or cargo:crate",
        );
        row(
            "update [name]",
            "Re-install from the recorded source (all plugins if no name)",
        );
        row("remove <name> [-y]", "Remove an installed plugin");
        println!();
        println!("{}", style::bold("Examples:"));
        for ex in [
            "aivo plugins",
            "aivo plugins install ./target/release/aivo-amp",
            "aivo plugins install github:owner/aivo-amp",
            "aivo plugins install npm:aivo-foo",
            "aivo plugins install cargo:aivo-bar",
            "aivo plugins update amp",
            "aivo plugins remove amp",
            "aivo amp --help        # run an installed plugin",
        ] {
            println!("  {}", style::dim(ex));
        }
    }
}

fn list_action() -> Result<ExitCode> {
    let plugins = installed_plugins();
    if plugins.is_empty() {
        eprintln!("  {} No plugins installed.", style::dim("·"));
        eprintln!("  {} {}", style::dim("·"), style::dim(INSTALL_HINT));
        if let Some(dir) = plugins_dir() {
            eprintln!(
                "  {} {}",
                style::dim("·"),
                style::dim(format!("Plugins live in {}", dir.display()))
            );
        }
        return Ok(ExitCode::Success);
    }

    let managed_dir = plugins_dir();
    let records = registry::load().plugins;
    let width = plugins
        .iter()
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(0)
        .min(24);
    for (name, path) in &plugins {
        let is_managed = managed_dir
            .as_deref()
            .is_some_and(|d| path.parent() == Some(d));
        let manifest = records.get(name).and_then(|r| r.manifest.as_ref());
        let tag = if !is_managed {
            " (external)"
        } else if manifest.is_none() {
            " (no manifest)"
        } else {
            ""
        };
        println!(
            "  {}  {}{}",
            style::cyan(format!("{name:<width$}")),
            style::dim(path.display().to_string()),
            style::dim(tag),
        );
        if let Some(m) = manifest {
            let mut bits = vec![format!("v{}", m.version)];
            if !m.roles.is_empty() {
                bits.push(m.roles.join("+"));
            }
            if !m.capabilities.is_empty() {
                bits.push(format!("caps: {}", m.capabilities.join(", ")));
            }
            println!(
                "  {}  {}",
                " ".repeat(width),
                style::dim(bits.join("  ·  "))
            );
        }
    }
    Ok(ExitCode::Success)
}

async fn install_action(args: PluginInstallArgs) -> Result<ExitCode> {
    let dir =
        plugins_dir().context("could not resolve the home directory for ~/.config/aivo/plugins")?;

    let name = match args.name {
        Some(n) => n,
        None => {
            // Surface a specific scheme error (e.g. `expected github:owner/repo`)
            // ahead of the generic name-inference failure.
            source::classify(&args.source)?;
            infer_plugin_name(&args.source)
                .context("could not infer a plugin name from the source — pass --name <name>")?
        }
    };
    validate_name(&name)?;
    if is_reserved_plugin_name(&name) {
        anyhow::bail!(
            "`{name}` collides with a built-in command or tool, so it would never run as a plugin. Choose a different --name."
        );
    }

    let target = dir.join(source::plugin_filename(&name));
    if target.exists() && !args.force {
        anyhow::bail!(
            "plugin `{name}` is already installed at {}. Pass --force to overwrite.",
            target.display()
        );
    }

    // Stable, re-fetchable source (absolute path for local files) for `update`.
    let source = canonical_source(&args.source);
    let outcome = reinstall(&name, &source, &dir).await?;
    record_install(&name, &source, &outcome);

    eprintln!(
        "  {} Installed plugin `{}` — run it with {}",
        style::success_symbol(),
        name,
        style::cyan(format!("aivo {name}")),
    );
    eprintln!(
        "  {} {}",
        style::dim("·"),
        style::dim(outcome.primary.display().to_string())
    );
    print_disclosure(&outcome);
    Ok(ExitCode::Success)
}

async fn update_action(args: PluginUpdateArgs) -> Result<ExitCode> {
    let dir = plugins_dir().context("could not resolve ~/.config/aivo/plugins")?;
    let records = registry::load().plugins;

    let targets: Vec<String> = match args.name {
        Some(n) => vec![n.strip_prefix(PLUGIN_PREFIX).unwrap_or(&n).to_string()],
        None => records.keys().cloned().collect(),
    };
    if targets.is_empty() {
        eprintln!(
            "  {} No plugins with a recorded source to update.",
            style::dim("·")
        );
        eprintln!("  {} {}", style::dim("·"), style::dim(INSTALL_HINT));
        return Ok(ExitCode::Success);
    }

    let mut any_failed = false;
    for name in &targets {
        let Some(source) = records.get(name).map(|r| r.source.clone()) else {
            any_failed = true;
            if discover(name).is_some() {
                eprintln!(
                    "  {} `{name}`: no recorded source (installed manually or externally) — reinstall with `aivo plugins install <source>`.",
                    style::yellow("!")
                );
            } else {
                eprintln!("  {} `{name}` is not installed.", style::yellow("!"));
            }
            continue;
        };
        match reinstall(name, &source, &dir).await {
            Ok(outcome) => {
                record_install(name, &source, &outcome);
                eprintln!(
                    "  {} Updated `{name}` from {}",
                    style::success_symbol(),
                    style::dim(&source)
                );
            }
            Err(e) => {
                any_failed = true;
                eprintln!("  {} `{name}`: {e:#}", style::red("✗"));
            }
        }
    }

    if any_failed {
        Ok(ExitCode::UserError)
    } else {
        Ok(ExitCode::Success)
    }
}

fn remove_action(args: PluginRemoveArgs) -> Result<ExitCode> {
    let dir = plugins_dir().context("could not resolve ~/.config/aivo/plugins")?;
    let name = args
        .name
        .strip_prefix(PLUGIN_PREFIX)
        .unwrap_or(&args.name)
        .to_string();
    // Binary plugins are `aivo-<name>`; an npm plugin's shim may be `.cmd` on Windows.
    let bin = dir.join(source::plugin_filename(&name));
    let target = if bin.exists() {
        bin
    } else {
        dir.join(format!("{PLUGIN_PREFIX}{name}.cmd"))
    };

    if !target.exists() {
        if let Some(found) = discover(&name) {
            anyhow::bail!(
                "`{name}` isn't managed by aivo — it's at {}. Remove it there (e.g. `cargo uninstall`).",
                found.display()
            );
        }
        anyhow::bail!("plugin `{name}` is not installed. See `aivo plugins list`.");
    }

    if !args.yes && !confirm(&format!("Remove plugin `{name}`?"))? {
        return Ok(ExitCode::Success);
    }

    std::fs::remove_file(&target).with_context(|| format!("removing {}", target.display()))?;
    // npm plugins also leave an `aivo-<name>.d/` payload directory.
    let bundle = dir.join(format!("{PLUGIN_PREFIX}{name}.d"));
    if bundle.is_dir() {
        let _ = std::fs::remove_dir_all(&bundle);
    }
    registry::forget(&name);
    eprintln!("  {} Removed plugin `{name}`", style::success_symbol());
    Ok(ExitCode::Success)
}

/// Interactive y/N prompt; bails non-interactively (pass `--yes`).
fn confirm(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("{prompt} (non-interactive; pass --yes to confirm)");
    }
    eprint!("  {} {prompt} [y/N] ", style::yellow("?"));
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Reject names that can't dispatch: empty, flag-shaped, or containing a path
/// separator (which would escape the plugins dir / break the `aivo-<name>` map).
fn validate_name(name: &str) -> Result<()> {
    if name.starts_with('-') {
        anyhow::bail!("plugin name `{name}` must not start with `-`");
    }
    if name.contains('/') || name.contains('\\') {
        anyhow::bail!("plugin name `{name}` must not contain a path separator");
    }
    Ok(())
}

/// Outcome of materializing a source: where it installed, the integrity pin, and
/// the probed manifest (local installs only). Shared by install and update.
struct InstallOutcome {
    primary: PathBuf,
    checksum: Option<String>,
    manifest: Option<PluginManifest>,
}

/// Resolve the source (local path / URL / `github:` / `npm:` / `cargo:`), install
/// `aivo-<name>` into `dir`, and probe for a manifest. The probe runs for
/// **local-path installs only** — aivo won't execute a freshly-fetched remote
/// binary to read its manifest before an install-consent gate exists (a later
/// phase); such plugins are recorded manifest-less until reinstalled locally.
async fn reinstall(name: &str, source: &str, dir: &Path) -> Result<InstallOutcome> {
    let materialized = source::materialize(source, name, dir).await?;
    let manifest = if materialized.trusted_local {
        probe_manifest(&materialized.primary, name).await
    } else {
        None
    };
    Ok(InstallOutcome {
        primary: materialized.primary,
        checksum: materialized.checksum,
        manifest,
    })
}

/// A stable, re-fetchable form of the install source: scheme strings (`github:`,
/// `npm:`, `cargo:`, URLs) verbatim so `update` re-resolves; local paths made
/// absolute so `update` works regardless of the current directory.
fn canonical_source(source: &str) -> String {
    match source::classify(source) {
        Ok(SourceKind::LocalPath) | Err(_) => std::fs::canonicalize(source)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| source.to_string()),
        _ => source.to_string(),
    }
}

// ── Registry write + install disclosure ────────────────────────────────────

/// Persist provenance (source + checksum + manifest + timestamp) so `update`
/// can re-fetch and `list` can show what a plugin declared. See
/// `crate::plugin::registry`.
fn record_install(name: &str, source: &str, outcome: &InstallOutcome) {
    registry::record(
        name,
        PluginRecord {
            source: source.to_string(),
            checksum: outcome.checksum.clone(),
            manifest: outcome.manifest.clone(),
            installed_at: Some(Utc::now().to_rfc3339()),
        },
    );
}

/// Surface what a freshly-installed plugin declared about itself. Capabilities
/// are disclosure-only in protocol v1 — nothing is granted or enforced yet
/// (a consent gate + a scoped endpoint are a later phase), so this stays low-key
/// rather than implying a grant. See `docs/PLUGIN-PROTOCOL.md`.
fn print_disclosure(outcome: &InstallOutcome) {
    let Some(m) = &outcome.manifest else {
        eprintln!(
            "  {} {}",
            style::dim("·"),
            style::dim("no manifest — runs as a plain subcommand")
        );
        return;
    };
    let mut bits = vec![format!("v{}", m.version)];
    if !m.roles.is_empty() {
        bits.push(format!("roles: {}", m.roles.join(", ")));
    }
    if !m.capabilities.is_empty() {
        bits.push(format!("declares caps: {}", m.capabilities.join(", ")));
    }
    eprintln!("  {} {}", style::dim("·"), style::dim(bits.join("  ·  ")));
    if !m.capabilities.is_empty() {
        eprintln!(
            "    {}",
            style::dim(
                "(capabilities are disclosure-only here — not granted or enforced; see docs/PLUGIN-PROTOCOL.md)"
            )
        );
    }
}
