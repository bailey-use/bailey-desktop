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
use crate::plugin::manifest::{PluginManifest, grantable_capabilities, probe_manifest};
use crate::plugin::registry::{self, PluginRecord};
use crate::plugin::source::{self, SourceKind};
use crate::plugin::{
    PLUGIN_PREFIX, discover, infer_plugin_name, installed_plugins, is_reserved_plugin_name,
    plugins_dir, prompt_capability_grant,
};
use crate::services::system_env::collapse_tilde;
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
    let managed_dir = plugins_dir();
    let managed_dir_display = managed_dir
        .as_ref()
        .map(|d| collapse_tilde(&d.display().to_string()));

    if plugins.is_empty() {
        eprintln!("  {} No plugins installed.", style::dim("·"));
        eprintln!("  {} {}", style::dim("·"), style::dim(INSTALL_HINT));
        if let Some(dir) = &managed_dir_display {
            eprintln!(
                "  {} {}",
                style::dim("·"),
                style::dim(format!("Plugins live in {dir}"))
            );
        }
        return Ok(ExitCode::Success);
    }

    let records = registry::load().plugins;
    let width = plugins
        .iter()
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(0)
        .min(24);
    // Continuation lines align under the name: "  ● " (4) + name + "  " (2).
    let indent = " ".repeat(width + 6);
    let sep = style::dim("  ·  ");

    println!(
        "{} {}",
        style::bold("Installed plugins"),
        style::dim(format!("({})", plugins.len()))
    );
    println!();

    for (name, path) in &plugins {
        let is_managed = managed_dir
            .as_deref()
            .is_some_and(|d| path.parent() == Some(d));
        let manifest = records.get(name).and_then(|r| r.manifest.as_ref());

        // Resolve each required binary once (PATH scan), reused for both the
        // status bullet and the detail line.
        let reqs: Vec<(&str, bool)> = manifest
            .map(|m| {
                m.requires
                    .iter()
                    .map(|r| (r.bin.as_str(), bin_on_path(&r.bin)))
                    .collect()
            })
            .unwrap_or_default();

        // Bullet encodes readiness: green = ready to run, yellow = installed but
        // a required binary is missing, dim ○ = no manifest (can't tell).
        let bullet = if manifest.is_none() {
            style::empty_bullet_symbol()
        } else if reqs.iter().any(|(_, ok)| !ok) {
            style::yellow("●")
        } else {
            style::bullet_symbol()
        };

        // Line 1: bullet + name + identity (type · version) + provenance tag.
        let mut ident: Vec<String> = Vec::new();
        if let Some(m) = manifest {
            if let Some(kind) = &m.kind {
                ident.push(kind.clone());
            }
            ident.push(format!("v{}", m.version));
        }
        let tag = if !is_managed {
            " (external)"
        } else if manifest.is_none() {
            " (no manifest)"
        } else {
            ""
        };
        println!(
            "  {} {}  {}{}",
            bullet,
            style::cyan(format!("{name:<width$}")),
            style::dim(ident.join(" · ")),
            style::dim(tag),
        );

        // Description on its own line — tells you what the plugin actually is.
        if let Some(desc) = manifest.and_then(|m| m.description.as_deref())
            && !desc.is_empty()
        {
            println!("{indent}{}", style::dim(desc));
        }

        // Detail line: granted/declared caps, requirement status, and — for
        // external plugins only — where the binary resolves (managed ones all
        // live in the footer dir, so the path would be noise).
        let mut details: Vec<String> = Vec::new();
        if let Some(m) = manifest {
            let grantable = grantable_capabilities(&m.capabilities);
            let granted = records.get(name).map(|r| {
                grantable
                    .iter()
                    .filter(|c| r.granted_caps.contains(c))
                    .cloned()
                    .collect::<Vec<_>>()
            });
            match granted.as_deref() {
                Some(g) if !g.is_empty() => {
                    details.push(style::dim(format!("caps: {}", g.join(", "))))
                }
                _ if !m.capabilities.is_empty() => details.push(style::dim(format!(
                    "requests: {}",
                    m.capabilities.join(", ")
                ))),
                _ => {}
            }
            if !reqs.is_empty() {
                let marked = reqs
                    .iter()
                    .map(|(bin, ok)| {
                        let mark = if *ok {
                            style::green("✓")
                        } else {
                            style::red("✗")
                        };
                        format!("{} {mark}", style::dim(*bin))
                    })
                    .collect::<Vec<_>>()
                    .join(&style::dim(", "));
                details.push(format!("{} {marked}", style::dim("requires")));
            }
        }
        if !is_managed {
            details.push(style::dim(collapse_tilde(&path.display().to_string())));
        }
        if !details.is_empty() {
            println!("{indent}{}", details.join(&sep));
        }
        println!();
    }

    if let Some(dir) = &managed_dir_display {
        println!("  {}", style::dim(format!("Plugins live in {dir}")));
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
    // Any caps already granted to this name (a force-reinstall preserves them).
    let prior = registry::load()
        .plugins
        .get(&name)
        .map(|r| r.granted_caps.clone())
        .unwrap_or_default();
    let outcome = reinstall(&name, &source, &dir).await?;

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
    // Seek consent for any grantable caps the manifest requests, then
    // persist the grant alongside the record.
    let granted = resolve_grants(&name, outcome.manifest.as_ref(), &prior, args.trust);
    record_install(&name, &source, &outcome, granted.clone());
    print_disclosure(&outcome, &granted);
    ensure_requirements(outcome.manifest.as_ref()).await;
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
                let prior = records
                    .get(name)
                    .map(|r| r.granted_caps.clone())
                    .unwrap_or_default();
                // Update preserves prior grants; it never auto-escalates.
                let granted = resolve_grants(name, outcome.manifest.as_ref(), &prior, false);
                record_install(name, &source, &outcome, granted);
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
/// **local-path installs only** — aivo doesn't execute a freshly-fetched remote
/// binary at install time just to read its manifest; such plugins are recorded
/// manifest-less and probed lazily on first dispatch (see `crate::plugin::endpoint`).
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

/// Persist provenance (source + checksum + manifest + timestamp + granted caps)
/// so `update` can re-fetch and dispatch knows what to hand over. See
/// `crate::plugin::registry`.
fn record_install(name: &str, source: &str, outcome: &InstallOutcome, granted_caps: Vec<String>) {
    registry::record(
        name,
        PluginRecord {
            source: source.to_string(),
            checksum: outcome.checksum.clone(),
            manifest: outcome.manifest.clone(),
            installed_at: Some(Utc::now().to_rfc3339()),
            granted_caps,
        },
    );
}

/// Decide which grantable caps to grant. With `auto_grant` (`--trust`),
/// grants all requested without prompting; otherwise prompts (TTY only) for caps
/// not already approved, never escalating silently. A manifest-less plugin
/// (remote install) keeps its prior grant — the first dispatch probes + prompts
/// instead (see `crate::plugin::endpoint`).
fn resolve_grants(
    name: &str,
    manifest: Option<&PluginManifest>,
    prior: &[String],
    auto_grant: bool,
) -> Vec<String> {
    let Some(m) = manifest else {
        return prior.to_vec();
    };
    let requested = grantable_capabilities(&m.capabilities);
    if requested.is_empty() {
        return Vec::new();
    }
    if requested.iter().all(|c| prior.contains(c)) {
        return requested; // already consented to everything requested
    }
    if auto_grant || (std::io::stdin().is_terminal() && prompt_capability_grant(name, &requested)) {
        requested
    } else {
        // Keep only previously-granted caps still requested; no silent escalation.
        requested
            .into_iter()
            .filter(|c| prior.contains(c))
            .collect()
    }
}

/// True when `bin` resolves on `$PATH`.
fn bin_on_path(bin: &str) -> bool {
    crate::services::path_search::find_in_dirs(
        bin,
        &crate::services::path_search::collect_path_dirs(),
    )
    .is_some()
}

/// After install, check the plugin's declared `requires`: for each missing
/// executable, offer to run its (plugin-authored) install command — the same
/// consent-gated flow native tools get — or print a hint. aivo never invents the
/// command; it only runs what the plugin declared, after showing it.
async fn ensure_requirements(manifest: Option<&PluginManifest>) {
    let Some(m) = manifest else { return };
    for req in &m.requires {
        if bin_on_path(&req.bin) {
            continue;
        }
        eprintln!(
            "  {} this plugin needs `{}`, which isn't on your PATH.",
            style::yellow("!"),
            req.bin,
        );
        let Some(cmd) = &req.install else {
            eprintln!(
                "    {}",
                style::dim(format!("install `{}` and re-run.", req.bin))
            );
            continue;
        };
        eprintln!("    {}", style::dim(format!("install command: {cmd}")));
        // Non-interactive (CI) → just leave the hint; don't run installers blind.
        if !std::io::stdin().is_terminal() {
            continue;
        }
        if confirm(&format!("Run it to install `{}`?", req.bin)).unwrap_or(false) {
            run_install_command(cmd, &req.bin).await;
        }
    }
}

/// Run a plugin-declared install command with inherited stdio (consent already
/// given by the caller).
async fn run_install_command(cmd: &str, bin: &str) {
    eprintln!("  {} Installing `{bin}`...", style::arrow_symbol());
    let mut command = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    match command.status().await {
        Ok(s) if s.success() => eprintln!("  {} `{bin}` installed.", style::success_symbol()),
        Ok(_) => eprintln!(
            "  {} install command exited non-zero — install `{bin}` manually.",
            style::yellow("!")
        ),
        Err(e) => eprintln!(
            "  {} couldn't run the install command: {e}",
            style::yellow("!")
        ),
    }
}

/// Surface what a freshly-installed plugin declared, and what was granted.
fn print_disclosure(outcome: &InstallOutcome, granted: &[String]) {
    let Some(m) = &outcome.manifest else {
        eprintln!(
            "  {} {}",
            style::dim("·"),
            style::dim("no manifest — runs as a plain subcommand")
        );
        return;
    };
    let mut bits = vec![format!("v{}", m.version)];
    if let Some(t) = &m.kind {
        bits.push(format!("type: {t}"));
    }
    if !m.roles.is_empty() {
        bits.push(format!("roles: {}", m.roles.join(", ")));
    }
    if !m.capabilities.is_empty() {
        bits.push(format!("requests: {}", m.capabilities.join(", ")));
    }
    eprintln!("  {} {}", style::dim("·"), style::dim(bits.join("  ·  ")));
    if !granted.is_empty() {
        eprintln!(
            "    {}",
            style::dim(format!("granted: {}", granted.join(", ")))
        );
    } else if !grantable_capabilities(&m.capabilities).is_empty() {
        eprintln!(
            "    {}",
            style::dim("no capabilities granted — reinstall interactively to grant")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with_caps(caps: &[&str]) -> PluginManifest {
        PluginManifest {
            name: "amp".to_string(),
            version: "1".to_string(),
            protocol: "1".to_string(),
            description: None,
            kind: None,
            roles: Vec::new(),
            documents_aivo_flags: false,
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
            hooks: Vec::new(),
            homepage: None,
            transcripts: None,
            requires: Vec::new(),
        }
    }

    #[test]
    fn resolve_grants_ignores_reserved_capabilities() {
        let manifest = manifest_with_caps(&["config-read", "endpoint", "config-write"]);
        assert_eq!(
            resolve_grants("amp", Some(&manifest), &[], true),
            ["endpoint"]
        );

        let prior = vec!["config-read".to_string(), "endpoint".to_string()];
        assert_eq!(
            resolve_grants("amp", Some(&manifest), &prior, false),
            ["endpoint"]
        );
    }
}
