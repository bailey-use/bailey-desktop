//! Archive extraction shared by the GGUF/llama-server installer
//! (`huggingface.rs`) and the plugin source resolver (`plugin::source`). Shells
//! out to system `tar` — universal on Unix, and Windows 10+ ships `tar.exe`
//! (libarchive) which handles both `.tar.gz` and `.zip`, so no archive crate.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::services::path_search::is_executable;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveKind {
    TarGz,
    Zip,
}

/// Classify an archive by filename suffix. `None` = not an archive we unpack
/// (the caller treats it as a raw binary).
pub(crate) fn archive_kind_for(filename: &str) -> Option<ArchiveKind> {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Some(ArchiveKind::TarGz)
    } else if lower.ends_with(".zip") {
        Some(ArchiveKind::Zip)
    } else {
        None
    }
}

/// Extract `archive` into `dest_dir` via `tar` (`-xzf` for tar.gz, `-xf` for zip).
pub(crate) fn extract_archive(archive: &Path, dest_dir: &Path, kind: ArchiveKind) -> Result<()> {
    let mut cmd = Command::new("tar");
    match kind {
        ArchiveKind::TarGz => cmd.arg("-xzf"),
        ArchiveKind::Zip => cmd.arg("-xf"),
    };
    cmd.arg(archive).arg("-C").arg(dest_dir);
    let output = cmd.output().context(
        "Failed to invoke `tar` to extract the archive. On Windows ensure tar.exe is on PATH.",
    )?;
    if !output.status.success() {
        anyhow::bail!(
            "tar failed to extract {}: {}",
            archive.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    Ok(())
}

/// If `dest_dir` holds exactly one entry and it's a directory, hoist its
/// contents up one level. Release tarballs commonly wrap everything in a single
/// top-level folder.
pub(crate) fn flatten_single_subdir(dest_dir: &Path) -> Result<()> {
    let entries: Vec<_> = std::fs::read_dir(dest_dir)
        .with_context(|| format!("Failed to read {}", dest_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("Failed to enumerate {}", dest_dir.display()))?;

    let only = match entries.as_slice() {
        [e] if e.file_type().map(|t| t.is_dir()).unwrap_or(false) => e.path(),
        _ => return Ok(()),
    };

    for sub in
        std::fs::read_dir(&only).with_context(|| format!("Failed to read {}", only.display()))?
    {
        let sub = sub.with_context(|| format!("Failed to read entry in {}", only.display()))?;
        let from = sub.path();
        let to = dest_dir.join(sub.file_name());
        std::fs::rename(&from, &to)
            .with_context(|| format!("Failed to move {} -> {}", from.display(), to.display()))?;
    }
    std::fs::remove_dir(&only)
        .with_context(|| format!("Failed to remove now-empty {}", only.display()))?;
    Ok(())
}

/// Locate the plugin executable among files extracted under `root`. Tries, in
/// order: exact `aivo-<name>`, then `<name>`, then a sole executable; otherwise
/// errors listing the candidates. Hoists a single wrapper dir first, and drops
/// any candidate that (via a symlink) resolves outside `root` (zip-slip guard).
pub(crate) fn find_executable(root: &Path, name: &str) -> Result<PathBuf> {
    let _ = flatten_single_subdir(root);

    let mut candidates = Vec::new();
    collect_runnable(root, 0, &mut candidates)?;

    let root_canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    candidates.retain(|p| {
        std::fs::canonicalize(p)
            .map(|c| c.starts_with(&root_canon))
            .unwrap_or(false)
    });

    let matches = |p: &Path, target: &str| {
        p.file_name().and_then(|s| s.to_str()) == Some(target)
            || p.file_stem().and_then(|s| s.to_str()) == Some(target)
    };
    let prefixed = format!("aivo-{name}");
    if let Some(p) = candidates.iter().find(|p| matches(p, &prefixed)) {
        return Ok(p.clone());
    }
    if let Some(p) = candidates.iter().find(|p| matches(p, name)) {
        return Ok(p.clone());
    }
    match candidates.as_slice() {
        [only] => Ok(only.clone()),
        [] => anyhow::bail!("no executable file was found in the downloaded archive"),
        many => {
            let names: Vec<String> = many
                .iter()
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect();
            anyhow::bail!(
                "the archive contains several executables ({}); none is named `aivo-{name}`. \
                 Ask the author to ship the binary as `aivo-{name}`.",
                names.join(", "),
            )
        }
    }
}

fn collect_runnable(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
    if depth > 6 {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_runnable(&path, depth + 1, out)?;
        } else if is_executable(&path) {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // Both are used only by the `#[cfg(unix)]` executable-bit tests below.
    #[cfg(unix)]
    use tempfile::TempDir;

    #[cfg(unix)]
    fn put_runnable(path: &Path) {
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(path).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(path, p).unwrap();
        }
    }

    #[test]
    fn archive_kind_classification() {
        assert_eq!(archive_kind_for("x.tar.gz"), Some(ArchiveKind::TarGz));
        assert_eq!(archive_kind_for("X.TGZ"), Some(ArchiveKind::TarGz));
        assert_eq!(archive_kind_for("x.zip"), Some(ArchiveKind::Zip));
        assert_eq!(archive_kind_for("aivo-amp"), None);
        assert_eq!(archive_kind_for("x.tar.xz"), None);
    }

    #[cfg(unix)]
    #[test]
    fn finds_prefixed_then_falls_back() {
        let d = TempDir::new().unwrap();
        put_runnable(&d.path().join("README"));
        put_runnable(&d.path().join("aivo-foo"));
        let found = find_executable(d.path(), "foo").unwrap();
        assert_eq!(found.file_name().unwrap(), "aivo-foo");

        let d2 = TempDir::new().unwrap();
        put_runnable(&d2.path().join("whatever"));
        assert_eq!(
            find_executable(d2.path(), "foo")
                .unwrap()
                .file_name()
                .unwrap(),
            "whatever"
        );
    }

    #[cfg(unix)]
    #[test]
    fn hoists_single_subdir() {
        let d = TempDir::new().unwrap();
        let sub = d.path().join("aivo-foo-v1");
        std::fs::create_dir(&sub).unwrap();
        put_runnable(&sub.join("aivo-foo"));
        let found = find_executable(d.path(), "foo").unwrap();
        assert_eq!(found.file_name().unwrap(), "aivo-foo");
        assert_eq!(found.parent().unwrap(), d.path());
    }

    #[cfg(unix)]
    #[test]
    fn errors_when_ambiguous() {
        let d = TempDir::new().unwrap();
        put_runnable(&d.path().join("tool-a"));
        put_runnable(&d.path().join("tool-b"));
        assert!(find_executable(d.path(), "foo").is_err());
    }
}
