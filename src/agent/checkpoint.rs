//! `/rewind` file-revert via tree-level checkpoints.
//!
//! Snapshots the whole working tree at each turn into a *private shadow git
//! object store* (a tempdir used only as `GIT_DIR`, with `GIT_WORK_TREE` pointed
//! at the project), and restores it on rewind. Because it operates on the tree —
//! not individual file edits — it reverts renames, moves, deletes, creates, and
//! `run_bash`-driven changes uniformly, which the old per-file byte snapshots
//! could not. The user's real repo (HEAD / index / stash) is never touched: we
//! only ever read/write our own shadow `GIT_DIR`, and write files into the work
//! tree on restore (the point of rewind).
//!
//! Plumbing only — `add`/`write-tree`/`read-tree`/`checkout-index`/`diff` — never
//! commits or branches. Honors the work tree's `.gitignore`; in a non-git dir it
//! seeds the shadow `info/exclude` with heavy/ephemeral defaults so snapshots
//! stay lean. A cheap size guard skips snapshotting a pathologically large tree.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// Cap on files in a single snapshot before it's skipped (turn = non-revertible).
const DEFAULT_MAX_FILES: usize = 20_000;
/// Cap on bytes in a single snapshot before it's skipped.
const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Patterns excluded from snapshots when the work tree is NOT a git repo (so it
/// has no `.gitignore` of its own). Inside a real repo we rely solely on the
/// repo's `.gitignore` and add none of these, so we never skip a path the repo
/// deliberately tracks.
const DEFAULT_EXCLUDES: &str = "node_modules/\n.venv/\nvenv/\ntarget/\ndist/\nbuild/\n.next/\n__pycache__/\n.mypy_cache/\n.pytest_cache/\n.gradle/\n.idea/\n.DS_Store\n";

/// What a restore did, for the rewind notice.
#[derive(Default)]
pub struct RestoreReport {
    /// Files rewritten or recreated to match the snapshot.
    pub restored: usize,
    /// Files removed because they were created after the snapshot point.
    pub deleted: usize,
    /// A git failure (the conversation still rewinds; files may be partial).
    pub error: Option<String>,
}

/// Per-session tree-checkpoint store backed by a shadow git dir.
pub struct CheckpointStore {
    cwd: PathBuf,
    /// The shadow `GIT_DIR`, created lazily on first snapshot. Dropping it (with
    /// the engine) removes the objects/index.
    dir: Option<tempfile::TempDir>,
    /// Cached `git --version` probe.
    git_ok: Option<bool>,
    max_files: usize,
    max_bytes: u64,
}

impl CheckpointStore {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            dir: None,
            git_ok: None,
            max_files: DEFAULT_MAX_FILES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// Tiny caps for tests that exercise the size guard.
    #[cfg(test)]
    fn with_caps(mut self, files: usize, bytes: u64) -> Self {
        self.max_files = files;
        self.max_bytes = bytes;
        self
    }

    /// Lazy, cached `git --version` probe. When git is absent, `/rewind` degrades
    /// to conversation-only (no file revert).
    pub async fn git_available(&mut self) -> bool {
        if let Some(v) = self.git_ok {
            return v;
        }
        let ok = Command::new("git")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        self.git_ok = Some(ok);
        ok
    }

    /// Snapshot the current work tree → tree SHA. `None` when git is unavailable,
    /// init fails, or the tree exceeds the size guard (turn = non-revertible).
    pub async fn snapshot(&mut self) -> Option<String> {
        if !self.git_available().await {
            return None;
        }
        let git_dir = self.ensure_init().await?;
        if self.exceeds_cap(&git_dir).await {
            return None;
        }
        let add = self.git(&git_dir).args(["add", "-A"]).output().await.ok()?;
        if !add.status.success() {
            return None;
        }
        let wt = self.git(&git_dir).arg("write-tree").output().await.ok()?;
        if !wt.status.success() {
            return None;
        }
        let sha = String::from_utf8_lossy(&wt.stdout).trim().to_string();
        (!sha.is_empty()).then_some(sha)
    }

    /// Restore the work tree to a snapshot tree SHA: rewrite/recreate its files
    /// and delete anything created since (incl. the new side of a rename), so the
    /// tree matches exactly. Reports counts for the notice.
    pub async fn restore(&mut self, tree: &str) -> RestoreReport {
        let err = |e: String| RestoreReport {
            error: Some(e),
            ..Default::default()
        };
        if !self.git_available().await {
            return err("git is unavailable".to_string());
        }
        let Some(git_dir) = self.ensure_init().await else {
            return err("could not init the checkpoint store".to_string());
        };
        // Capture the current tree first so we know what was added since `tree`.
        let _ = self.git(&git_dir).args(["add", "-A"]).output().await;
        let cur = match self.git(&git_dir).arg("write-tree").output().await {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            Ok(o) => return err(stderr_of(&o)),
            Err(e) => return err(e.to_string()),
        };
        if cur == tree {
            return RestoreReport::default(); // nothing changed since the snapshot
        }
        // Make the index `tree`, then force every file in it onto disk.
        match self.git(&git_dir).arg("read-tree").arg(tree).output().await {
            Ok(o) if o.status.success() => {}
            Ok(o) => return err(stderr_of(&o)),
            Err(e) => return err(e.to_string()),
        }
        match self
            .git(&git_dir)
            .args(["checkout-index", "-a", "-f"])
            .output()
            .await
        {
            Ok(o) if o.status.success() => {}
            Ok(o) => return err(stderr_of(&o)),
            Err(e) => return err(e.to_string()),
        }
        // Files present now but not in `tree` (creates / rename new-side) → delete.
        let to_delete = match self.diff_names(&git_dir, tree, &cur, "A").await {
            Ok(paths) => paths,
            Err(e) => return err(e),
        };
        let restored = self
            .diff_names(&git_dir, tree, &cur, "DM")
            .await
            .map(|v| v.len())
            .unwrap_or(0);
        let mut deleted = 0usize;
        for rel in &to_delete {
            let path = self.cwd.join(rel);
            if std::fs::remove_file(&path).is_ok() {
                deleted += 1;
            }
            self.rmdir_empty_parents(&path);
        }
        RestoreReport {
            restored,
            deleted,
            error: None,
        }
    }

    // --- internals ---

    /// A `git` command pre-wired with the shadow `GIT_DIR`, the project work tree,
    /// and config that neutralizes the user's environment (no CRLF rewrites on
    /// restore, preserve mode bits, verbatim path output).
    fn git(&self, git_dir: &Path) -> Command {
        let mut cmd = Command::new("git");
        cmd.env("GIT_DIR", git_dir)
            .env("GIT_WORK_TREE", &self.cwd)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .args([
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.eol=lf",
                "-c",
                "core.fileMode=true",
                "-c",
                "core.quotePath=false",
            ]);
        cmd
    }

    /// Create the shadow git dir on first use and return its path.
    async fn ensure_init(&mut self) -> Option<PathBuf> {
        if self.dir.is_none() {
            let td = tempfile::Builder::new()
                .prefix("aivo-rewind-")
                .tempdir()
                .ok()?;
            let git_dir = td.path().to_path_buf();
            let out = Command::new("git")
                .arg("--git-dir")
                .arg(&git_dir)
                .args(["init", "-q"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .ok()?;
            if !out.success() {
                return None;
            }
            // In a non-git work tree (no `.gitignore`), seed our own excludes so a
            // bare folder with `node_modules`/`target`/… snapshots leanly.
            if !self.cwd_is_git_repo().await {
                let _ = std::fs::write(git_dir.join("info").join("exclude"), DEFAULT_EXCLUDES);
            }
            self.dir = Some(td);
        }
        self.dir.as_ref().map(|d| d.path().to_path_buf())
    }

    async fn cwd_is_git_repo(&self) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(&self.cwd)
            .args(["rev-parse", "--is-inside-work-tree"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
            .unwrap_or(false)
    }

    /// True when the untracked files git would add exceed the size guard. Counts
    /// entries + sums their sizes without hashing (`ls-files --others`), so it's
    /// cheap even on a huge tree. A measurement failure does not block.
    async fn exceeds_cap(&self, git_dir: &Path) -> bool {
        let out = match self
            .git(git_dir)
            .args(["ls-files", "--others", "--exclude-standard", "-z"])
            .output()
            .await
        {
            Ok(o) if o.status.success() => o,
            _ => return false,
        };
        let mut files = 0usize;
        let mut bytes = 0u64;
        for rel in out.stdout.split(|&b| b == 0).filter(|s| !s.is_empty()) {
            files += 1;
            if files > self.max_files {
                return true;
            }
            let path = self.cwd.join(String::from_utf8_lossy(rel).as_ref());
            if let Ok(md) = std::fs::metadata(&path) {
                bytes = bytes.saturating_add(md.len());
                if bytes > self.max_bytes {
                    return true;
                }
            }
        }
        false
    }

    /// Paths changed between trees `from`→`to` matching `--diff-filter=<filter>`,
    /// relative to the work tree. NUL-delimited so spaces/unicode are verbatim.
    async fn diff_names(
        &self,
        git_dir: &Path,
        from: &str,
        to: &str,
        filter: &str,
    ) -> Result<Vec<PathBuf>, String> {
        let out = self
            .git(git_dir)
            .args(["diff", "--name-only", "-z"])
            .arg(format!("--diff-filter={filter}"))
            .arg(from)
            .arg(to)
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(stderr_of(&out));
        }
        Ok(out
            .stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(String::from_utf8_lossy(s).into_owned()))
            .collect())
    }

    /// Remove now-empty parent directories of a deleted file, up to (not incl.)
    /// the work tree root. Stops at the first non-empty / non-removable dir.
    fn rmdir_empty_parents(&self, file: &Path) {
        let mut cur = file.parent();
        while let Some(dir) = cur {
            if dir == self.cwd || !dir.starts_with(&self.cwd) {
                break;
            }
            if std::fs::remove_dir(dir).is_err() {
                break;
            }
            cur = dir.parent();
        }
    }
}

fn stderr_of(out: &std::process::Output) -> String {
    let e = String::from_utf8_lossy(&out.stderr);
    let e = e.trim();
    if e.is_empty() {
        "git command failed".to_string()
    } else {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Skip a test when git isn't installed (mirrors other env-gated tests).
    async fn git_or_skip(cwd: &Path) -> Option<CheckpointStore> {
        let mut store = CheckpointStore::new(cwd);
        if store.git_available().await {
            Some(store)
        } else {
            None
        }
    }

    fn write(cwd: &Path, rel: &str, body: &str) {
        let p = cwd.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn read(cwd: &Path, rel: &str) -> Option<String> {
        std::fs::read_to_string(cwd.join(rel)).ok()
    }

    #[tokio::test]
    async fn snapshot_then_restore_reverts_edit() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(cwd, "a.rs", "v0");
        let Some(mut store) = git_or_skip(cwd).await else {
            return;
        };
        let t0 = store.snapshot().await.expect("snapshot");
        write(cwd, "a.rs", "v1");
        let report = store.restore(&t0).await;
        assert_eq!(read(cwd, "a.rs").as_deref(), Some("v0"));
        assert!(report.error.is_none());
        assert_eq!(report.restored, 1);
    }

    #[tokio::test]
    async fn restore_recreates_renamed_file_and_removes_new() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(cwd, "a.rs", "A");
        let Some(mut store) = git_or_skip(cwd).await else {
            return;
        };
        let t0 = store.snapshot().await.expect("snapshot");
        // Simulate `mv a.rs b.rs` (a bash rename) + an edit of the new name.
        std::fs::rename(cwd.join("a.rs"), cwd.join("b.rs")).unwrap();
        write(cwd, "b.rs", "A edited");
        let report = store.restore(&t0).await;
        assert_eq!(read(cwd, "a.rs").as_deref(), Some("A"), "old name restored");
        assert!(!cwd.join("b.rs").exists(), "new name removed");
        assert_eq!((report.restored, report.deleted), (1, 1));
    }

    #[tokio::test]
    async fn restore_deletes_created_file_and_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(cwd, "keep.rs", "k");
        let Some(mut store) = git_or_skip(cwd).await else {
            return;
        };
        let t0 = store.snapshot().await.expect("snapshot");
        write(cwd, "sub/new.rs", "n");
        store.restore(&t0).await;
        assert!(!cwd.join("sub/new.rs").exists());
        assert!(!cwd.join("sub").exists(), "empty dir cleaned");
        assert_eq!(read(cwd, "keep.rs").as_deref(), Some("k"));
    }

    #[tokio::test]
    async fn restore_recreates_bash_rm_target() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(cwd, "a.rs", "A");
        let Some(mut store) = git_or_skip(cwd).await else {
            return;
        };
        let t0 = store.snapshot().await.expect("snapshot");
        std::fs::remove_file(cwd.join("a.rs")).unwrap(); // simulate `rm a.rs`
        let report = store.restore(&t0).await;
        assert_eq!(read(cwd, "a.rs").as_deref(), Some("A"));
        assert_eq!(report.restored, 1);
    }

    #[tokio::test]
    async fn gitignored_file_not_reverted() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        // A real repo so the repo's own .gitignore is honored.
        let inited = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .arg("init")
            .arg("-q")
            .output()
            .await;
        if inited.map(|o| !o.status.success()).unwrap_or(true) {
            return; // git missing
        }
        write(cwd, ".gitignore", "build/\n");
        write(cwd, "build/out", "v0");
        let mut store = CheckpointStore::new(cwd);
        let t0 = store.snapshot().await.expect("snapshot");
        write(cwd, "build/out", "v1");
        store.restore(&t0).await;
        // Ignored file is not snapshotted, so it keeps its edited content.
        assert_eq!(read(cwd, "build/out").as_deref(), Some("v1"));
    }

    #[tokio::test]
    async fn guard_skips_oversized_tree() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        for i in 0..5 {
            write(cwd, &format!("f{i}.rs"), "x");
        }
        let mut store = CheckpointStore::new(cwd).with_caps(2, u64::MAX);
        if !store.git_available().await {
            return;
        }
        assert!(
            store.snapshot().await.is_none(),
            "guard trips → no snapshot"
        );
    }

    #[tokio::test]
    async fn dedupe_identical_tree_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(cwd, "a.rs", "same");
        let Some(mut store) = git_or_skip(cwd).await else {
            return;
        };
        let t0 = store.snapshot().await.expect("snapshot");
        let t1 = store.snapshot().await.expect("snapshot");
        assert_eq!(t0, t1, "unchanged tree → identical SHA");
        let report = store.restore(&t0).await;
        assert_eq!((report.restored, report.deleted), (0, 0));
    }

    #[tokio::test]
    async fn works_in_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(cwd, "a.rs", "v0");
        let Some(mut store) = git_or_skip(cwd).await else {
            return;
        };
        // Not a git repo at all — the shadow store still snapshots/restores.
        let t0 = store.snapshot().await.expect("snapshot");
        write(cwd, "a.rs", "v1");
        store.restore(&t0).await;
        assert_eq!(read(cwd, "a.rs").as_deref(), Some("v0"));
    }

    #[tokio::test]
    async fn non_git_dir_excludes_heavy_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(cwd, "a.rs", "v0");
        write(cwd, "node_modules/pkg/index.js", "heavy");
        let Some(mut store) = git_or_skip(cwd).await else {
            return;
        };
        let t0 = store.snapshot().await.expect("snapshot");
        // node_modules is excluded → editing it is invisible to a restore.
        write(cwd, "node_modules/pkg/index.js", "changed");
        write(cwd, "a.rs", "v1");
        store.restore(&t0).await;
        assert_eq!(
            read(cwd, "a.rs").as_deref(),
            Some("v0"),
            "tracked file reverts"
        );
        assert_eq!(
            read(cwd, "node_modules/pkg/index.js").as_deref(),
            Some("changed"),
            "excluded file untouched"
        );
    }

    #[tokio::test]
    async fn real_repo_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let run = |args: &[&str]| {
            let mut c = std::process::Command::new("git");
            c.arg("-C").arg(cwd).args(args);
            c.env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t");
            c.output()
        };
        if run(&["init", "-q"])
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return; // git missing
        }
        write(cwd, "tracked.rs", "v0");
        let _ = run(&["add", "tracked.rs"]);
        let _ = run(&["commit", "-q", "-m", "init"]);
        let head_before = run(&["rev-parse", "HEAD"]).unwrap().stdout;

        // Drive a shadow snapshot + restore over the same work tree.
        let mut store = CheckpointStore::new(cwd);
        if !store.git_available().await {
            return;
        }
        let t0 = store.snapshot().await.expect("snapshot");
        write(cwd, "tracked.rs", "v1");
        store.restore(&t0).await;

        let head_after = run(&["rev-parse", "HEAD"]).unwrap().stdout;
        assert_eq!(head_before, head_after, "user HEAD unchanged");
        // The shadow store wrote nothing into the user's index.
        let status = run(&["status", "--porcelain"]).unwrap().stdout;
        assert!(
            String::from_utf8_lossy(&status).trim().is_empty(),
            "user worktree clean after restore"
        );
        assert_eq!(read(cwd, "tracked.rs").as_deref(), Some("v0"));
    }
}
