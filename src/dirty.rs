//! Dirty worktree inspection before releasing a Treehouse lease.
//!
//! Inspects the worktree main repo and each submodule separately for staged,
//! unstaged, untracked, and unpushed-commit leftovers (local changes that
//! could be lost on release). Being behind the remote is ignored. Parent
//! gitlink / submodule-pointer changes are ignored in the main repo.

use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, eyre};

use crate::gitutil::{self, git_stdout};

const PATH_LIST_LIMIT: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyKind {
    Staged,
    Unstaged,
    Untracked,
    Remote,
}

impl DirtyKind {
    pub fn label(self) -> &'static str {
        match self {
            DirtyKind::Staged => "staged",
            DirtyKind::Unstaged => "unstaged",
            DirtyKind::Untracked => "untracked",
            DirtyKind::Remote => "unpushed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyGroup {
    /// Display name: main repo basename, or submodule name.
    pub location: String,
    pub kind: DirtyKind,
    /// File paths for staged/unstaged/untracked. Empty for remote.
    pub paths: Vec<String>,
    /// Only set for [`DirtyKind::Remote`] (unpushed local commits).
    pub ahead: usize,
    /// Only set for [`DirtyKind::Remote`]; informational when also diverged.
    pub behind: usize,
}

impl DirtyGroup {
    fn local(location: impl Into<String>, kind: DirtyKind, paths: Vec<String>) -> Self {
        Self {
            location: location.into(),
            kind,
            paths,
            ahead: 0,
            behind: 0,
        }
    }

    fn remote(location: impl Into<String>, ahead: usize, behind: usize) -> Self {
        Self {
            location: location.into(),
            kind: DirtyKind::Remote,
            paths: Vec::new(),
            ahead,
            behind,
        }
    }

    pub fn is_remote(&self) -> bool {
        self.kind == DirtyKind::Remote
    }

    pub fn is_local(&self) -> bool {
        !self.is_remote()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirtyReport {
    pub groups: Vec<DirtyGroup>,
}

impl DirtyReport {
    pub fn is_clean(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn has_local_changes(&self) -> bool {
        self.groups.iter().any(DirtyGroup::is_local)
    }

    pub fn has_remote_divergence(&self) -> bool {
        self.groups.iter().any(DirtyGroup::is_remote)
    }

    /// True when a stash of local changes could clear at least one blocking group.
    pub fn stash_would_help(&self) -> bool {
        self.has_local_changes()
    }
}

/// Inspect `worktree_root` (main worktree path) and each submodule under it.
pub fn inspect_worktree(worktree_root: impl AsRef<Path>) -> color_eyre::Result<DirtyReport> {
    let root = worktree_root.as_ref();
    if !root.is_dir() {
        return Err(eyre!("worktree path does not exist: {}", root.display()));
    }

    let main_name = gitutil::main_repo_name(root).unwrap_or_else(|_| "main".to_string());
    let mut groups = Vec::new();

    // Main repo: ignore gitlink / submodule-pointer changes.
    groups.extend(inspect_location(root, &main_name, true)?);

    for (name, rel) in gitutil::submodule_entries(root)? {
        let sub_path = root.join(&rel);
        if !sub_path.is_dir() {
            continue;
        }
        // Inside submodules, report all local dirt (no gitlink filter needed).
        groups.extend(inspect_location(&sub_path, &name, false)?);
    }

    Ok(DirtyReport { groups })
}

/// Inspect a single git directory. When `ignore_gitlinks` is true, submodule
/// pointer changes in the parent are excluded via `--ignore-submodules=all`.
pub fn inspect_location(
    repo: &Path,
    location: &str,
    ignore_gitlinks: bool,
) -> color_eyre::Result<Vec<DirtyGroup>> {
    let mut groups = Vec::new();

    let staged = list_paths(repo, &diff_name_only_args(true, ignore_gitlinks))?;
    if !staged.is_empty() {
        groups.push(DirtyGroup::local(location, DirtyKind::Staged, staged));
    }

    let unstaged = list_paths(repo, &diff_name_only_args(false, ignore_gitlinks))?;
    if !unstaged.is_empty() {
        groups.push(DirtyGroup::local(location, DirtyKind::Unstaged, unstaged));
    }

    let untracked = list_paths(repo, &["ls-files", "--others", "--exclude-standard"])?;
    if !untracked.is_empty() {
        groups.push(DirtyGroup::local(location, DirtyKind::Untracked, untracked));
    }

    // Only unpushed local commits can be lost on release. Being behind the
    // remote is fine — those changes are already stored upstream.
    if let Some((ahead, behind)) = remote_divergence(repo)?
        && ahead > 0
    {
        groups.push(DirtyGroup::remote(location, ahead, behind));
    }

    Ok(groups)
}

fn diff_name_only_args(cached: bool, ignore_gitlinks: bool) -> Vec<&'static str> {
    let mut args = vec!["diff", "--name-only"];
    if cached {
        args.insert(1, "--cached");
    }
    if ignore_gitlinks {
        args.push("--ignore-submodules=all");
    }
    args
}

fn list_paths(repo: &Path, args: &[&str]) -> color_eyre::Result<Vec<String>> {
    let out = git_stdout(repo, args)
        .wrap_err_with(|| format!("listing changes in {} ({})", repo.display(), args.join(" ")))?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Returns `(ahead, behind)` vs upstream when an upstream is configured.
fn remote_divergence(repo: &Path) -> color_eyre::Result<Option<(usize, usize)>> {
    let upstream = CommandQuiet::run(repo, &["rev-parse", "--abbrev-ref", "@{upstream}"])?;
    if upstream.is_none() {
        return Ok(None);
    }

    let out = git_stdout(
        repo,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    )
    .wrap_err("checking ahead/behind vs upstream")?;
    let mut parts = out.split_whitespace();
    let behind: usize = parts
        .next()
        .unwrap_or("0")
        .parse()
        .wrap_err("parsing behind count")?;
    let ahead: usize = parts
        .next()
        .unwrap_or("0")
        .parse()
        .wrap_err("parsing ahead count")?;
    Ok(Some((ahead, behind)))
}

/// Soft-fail wrapper for commands that may legitimately fail (e.g. no upstream).
struct CommandQuiet;

impl CommandQuiet {
    fn run(repo: &Path, args: &[&str]) -> color_eyre::Result<Option<String>> {
        use std::process::Command;
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .wrap_err_with(|| format!("failed to run git {}", args.join(" ")))?;
        if !output.status.success() {
            return Ok(None);
        }
        let stdout = String::from_utf8(output.stdout).wrap_err("git stdout was not valid UTF-8")?;
        Ok(Some(stdout.trim().to_string()))
    }
}

/// Stash local changes (including untracked) in every location that has them.
///
/// If a location has staged content, unstages first (`git reset HEAD`), then
/// `git stash push -u`. Does not attempt to fix remote divergence.
pub fn stash_local_changes(worktree_root: impl AsRef<Path>) -> color_eyre::Result<()> {
    let root = worktree_root.as_ref();
    let report = inspect_worktree(root)?;
    if !report.has_local_changes() {
        return Ok(());
    }

    // Collect unique locations with local dirt.
    let mut locations: Vec<(String, PathBuf, bool)> = Vec::new();
    let main_name = gitutil::main_repo_name(root).unwrap_or_else(|_| "main".to_string());
    let mut seen = std::collections::HashSet::new();

    for group in report.groups.iter().filter(|g| g.is_local()) {
        if !seen.insert(group.location.clone()) {
            continue;
        }
        if group.location == main_name {
            locations.push((group.location.clone(), root.to_path_buf(), true));
        } else {
            // Resolve submodule path by name.
            let entries = gitutil::submodule_entries(root)?;
            if let Some((_, rel)) = entries.into_iter().find(|(n, _)| n == &group.location) {
                locations.push((group.location.clone(), root.join(rel), false));
            }
        }
    }

    for (label, path, ignore_gitlinks) in locations {
        stash_one(&path, &label, ignore_gitlinks)?;
    }
    Ok(())
}

fn stash_one(repo: &Path, label: &str, ignore_gitlinks: bool) -> color_eyre::Result<()> {
    let staged = list_paths(repo, &diff_name_only_args(true, ignore_gitlinks))?;
    if !staged.is_empty() {
        // Unstage so stash -u can pick everything up as a single stash.
        let _ = git_stdout(repo, &["reset", "HEAD"])
            .wrap_err_with(|| format!("unstaging before stash in {label} ({})", repo.display()))?;
    }

    let unstaged = list_paths(repo, &diff_name_only_args(false, ignore_gitlinks))?;
    let untracked = list_paths(repo, &["ls-files", "--others", "--exclude-standard"])?;
    if unstaged.is_empty() && untracked.is_empty() && staged.is_empty() {
        return Ok(());
    }

    git_stdout(
        repo,
        &[
            "stash",
            "push",
            "-u",
            "-m",
            "tod: stash before worktree release",
        ],
    )
    .wrap_err_with(|| format!("stashing changes in {label} ({})", repo.display()))?;
    Ok(())
}

/// Format report lines for the DirtyWarning UI.
pub fn format_report_lines(report: &DirtyReport) -> Vec<String> {
    let mut lines = Vec::new();
    if report.is_clean() {
        lines.push("Worktree is clean.".to_string());
        return lines;
    }

    lines.push("Worktree has leftover changes — release blocked until clean.".to_string());
    lines.push(String::new());

    let mut current_loc: Option<&str> = None;
    for group in &report.groups {
        if current_loc != Some(group.location.as_str()) {
            current_loc = Some(&group.location);
            lines.push(format!("[{}]", group.location));
        }

        match group.kind {
            DirtyKind::Remote => {
                lines.push(format!(
                    "  unpushed: {}",
                    format_ahead_behind(group.ahead, group.behind)
                ));
            }
            kind => {
                let n = group.paths.len();
                if n > PATH_LIST_LIMIT {
                    lines.push(format!("  {n} {} files", kind.label()));
                } else {
                    lines.push(format!("  {} ({}):", kind.label(), n));
                    for p in &group.paths {
                        lines.push(format!("    - {p}"));
                    }
                }
            }
        }
    }
    lines
}

pub fn format_ahead_behind(ahead: usize, behind: usize) -> String {
    match (ahead, behind) {
        (0, 0) => "in sync".to_string(),
        (a, 0) => format!("ahead by {a}"),
        (0, b) => format!("behind by {b}"),
        (a, b) => format!("ahead by {a}, behind by {b}"),
    }
}

/// Option labels for the DirtyWarning menu (excluding stash when not helpful).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyAction {
    CheckAgain,
    StashChanges,
    Cancel,
}

impl DirtyAction {
    pub fn label(self) -> &'static str {
        match self {
            DirtyAction::CheckAgain => "Check again",
            DirtyAction::StashChanges => {
                "Stash changes (includes untracked; unstages staged first)"
            }
            DirtyAction::Cancel => "Cancel",
        }
    }

    pub fn shortcut(self) -> char {
        match self {
            DirtyAction::CheckAgain => 'c',
            DirtyAction::StashChanges => 's',
            DirtyAction::Cancel => 'x',
        }
    }
}

pub fn menu_actions(report: &DirtyReport) -> Vec<DirtyAction> {
    let mut actions = vec![DirtyAction::CheckAgain];
    if report.stash_would_help() {
        actions.push(DirtyAction::StashChanges);
    }
    actions.push(DirtyAction::Cancel);
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn init_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        assert!(
            Command::new("git")
                .args(["init"])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["config", "user.email", "test@example.com"])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["config", "user.name", "Test"])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
        // Avoid "master" vs "main" surprises in CI.
        let _ = Command::new("git")
            .args(["checkout", "-b", "main"])
            .current_dir(dir)
            .status();
        fs::write(dir.join("README"), "hi").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "README"])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "init"])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tod-dirty-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn clean_repo_is_clean() {
        let dir = temp_dir("clean");
        init_repo(&dir);
        let report = inspect_worktree(&dir).unwrap();
        assert!(report.is_clean());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classifies_staged_unstaged_untracked() {
        let dir = temp_dir("kinds");
        init_repo(&dir);

        fs::write(dir.join("staged.txt"), "s").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "staged.txt"])
                .current_dir(&dir)
                .status()
                .unwrap()
                .success()
        );

        fs::write(dir.join("README"), "changed").unwrap();
        fs::write(dir.join("untracked.txt"), "u").unwrap();

        let report = inspect_worktree(&dir).unwrap();
        assert!(report.has_local_changes());
        assert!(!report.has_remote_divergence());

        let kinds: Vec<_> = report.groups.iter().map(|g| g.kind).collect();
        assert!(kinds.contains(&DirtyKind::Staged));
        assert!(kinds.contains(&DirtyKind::Unstaged));
        assert!(kinds.contains(&DirtyKind::Untracked));

        let staged = report
            .groups
            .iter()
            .find(|g| g.kind == DirtyKind::Staged)
            .unwrap();
        assert_eq!(staged.paths, vec!["staged.txt".to_string()]);

        let untracked = report
            .groups
            .iter()
            .find(|g| g.kind == DirtyKind::Untracked)
            .unwrap();
        assert_eq!(untracked.paths, vec!["untracked.txt".to_string()]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_gitlink_changes_in_parent() {
        let parent = temp_dir("parent");
        let sub = temp_dir("sub");
        init_repo(&parent);
        init_repo(&sub);

        // Add submodule via git submodule add from a local path.
        // Use file:// URL for local clone compatibility.
        let sub_url = format!("file://{}", sub.display());
        let add = Command::new("git")
            .args(["submodule", "add", &sub_url, "vendor"])
            .current_dir(&parent)
            .output()
            .unwrap();
        if !add.status.success() {
            // Some environments lack submodule support; skip rather than fail CI.
            let _ = fs::remove_dir_all(&parent);
            let _ = fs::remove_dir_all(&sub);
            return;
        }
        assert!(
            Command::new("git")
                .args(["commit", "-m", "add submodule"])
                .current_dir(&parent)
                .status()
                .unwrap()
                .success()
        );

        // Move submodule HEAD so parent sees a dirty gitlink.
        assert!(
            Command::new("git")
                .args(["checkout", "-b", "other"])
                .current_dir(parent.join("vendor"))
                .status()
                .unwrap()
                .success()
        );
        fs::write(parent.join("vendor").join("extra"), "x").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "extra"])
                .current_dir(parent.join("vendor"))
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "sub change"])
                .current_dir(parent.join("vendor"))
                .status()
                .unwrap()
                .success()
        );

        let report = inspect_worktree(&parent).unwrap();
        // Parent should not report gitlink as staged/unstaged.
        let parent_name = parent.file_name().unwrap().to_string_lossy().to_string();
        let parent_local: Vec<_> = report
            .groups
            .iter()
            .filter(|g| g.location == parent_name && g.is_local())
            .collect();
        assert!(
            parent_local.is_empty(),
            "parent gitlink should be ignored, got {parent_local:?}"
        );
        // Submodule itself should still be inspected (clean after commit).
        let _ = fs::remove_dir_all(&parent);
        let _ = fs::remove_dir_all(&sub);
    }

    #[test]
    fn stash_clears_local_and_keeps_menu_logic() {
        let dir = temp_dir("stash");
        init_repo(&dir);
        fs::write(dir.join("staged.txt"), "s").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "staged.txt"])
                .current_dir(&dir)
                .status()
                .unwrap()
                .success()
        );
        fs::write(dir.join("untracked.txt"), "u").unwrap();

        let before = inspect_worktree(&dir).unwrap();
        assert!(before.stash_would_help());
        assert!(menu_actions(&before).contains(&DirtyAction::StashChanges));

        stash_local_changes(&dir).unwrap();
        let after = inspect_worktree(&dir).unwrap();
        assert!(after.is_clean());
        assert!(!menu_actions(&after).contains(&DirtyAction::StashChanges));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_summarizes_large_groups() {
        let mut paths = Vec::new();
        for i in 0..12 {
            paths.push(format!("f{i}.txt"));
        }
        let group = DirtyGroup::local("repo", DirtyKind::Untracked, paths);
        let report = DirtyReport {
            groups: vec![group],
        };
        let lines = format_report_lines(&report);
        assert!(lines.iter().any(|l| l.contains("12 untracked files")));
    }

    #[test]
    fn format_ahead_behind_words() {
        assert_eq!(format_ahead_behind(2, 0), "ahead by 2");
        assert_eq!(format_ahead_behind(0, 3), "behind by 3");
        assert_eq!(format_ahead_behind(1, 4), "ahead by 1, behind by 4");
    }

    /// Clone `origin` into `clone`, set upstream tracking on main.
    fn clone_with_upstream(origin: &Path, clone: &Path) {
        assert!(
            Command::new("git")
                .args([
                    "clone",
                    &origin.display().to_string(),
                    &clone.display().to_string()
                ])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["config", "user.email", "test@example.com"])
                .current_dir(clone)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["config", "user.name", "Test"])
                .current_dir(clone)
                .status()
                .unwrap()
                .success()
        );
    }

    fn init_bare_origin(tag: &str) -> (PathBuf, PathBuf) {
        let seed = temp_dir(&format!("{tag}-seed"));
        let origin = temp_dir(&format!("{tag}-origin"));
        init_repo(&seed);
        assert!(
            Command::new("git")
                .args([
                    "clone",
                    "--bare",
                    &seed.display().to_string(),
                    &origin.display().to_string(),
                ])
                .status()
                .unwrap()
                .success()
        );
        let _ = fs::remove_dir_all(&seed);
        (origin, temp_dir(&format!("{tag}-clone")))
    }

    #[test]
    fn behind_remote_alone_is_clean() {
        let (origin, clone) = init_bare_origin("behind");
        let remote_work = temp_dir("behind-remote-work");
        clone_with_upstream(&origin, &remote_work);
        clone_with_upstream(&origin, &clone);

        // Advance origin via remote_work so clone is behind after fetch.
        fs::write(remote_work.join("ahead.txt"), "remote").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "ahead.txt"])
                .current_dir(&remote_work)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "remote commit"])
                .current_dir(&remote_work)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["push", "origin", "main"])
                .current_dir(&remote_work)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["fetch", "origin"])
                .current_dir(&clone)
                .status()
                .unwrap()
                .success()
        );

        let report = inspect_worktree(&clone).unwrap();
        assert!(
            report.is_clean(),
            "behind-only should not block release, got {report:?}"
        );

        let _ = fs::remove_dir_all(&origin);
        let _ = fs::remove_dir_all(&clone);
        let _ = fs::remove_dir_all(&remote_work);
    }

    #[test]
    fn unpushed_commits_block_release() {
        let (origin, clone) = init_bare_origin("ahead");
        clone_with_upstream(&origin, &clone);

        fs::write(clone.join("local.txt"), "local").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "local.txt"])
                .current_dir(&clone)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "local only"])
                .current_dir(&clone)
                .status()
                .unwrap()
                .success()
        );

        let report = inspect_worktree(&clone).unwrap();
        assert!(report.has_remote_divergence());
        assert!(!report.has_local_changes());
        let remote = report
            .groups
            .iter()
            .find(|g| g.kind == DirtyKind::Remote)
            .unwrap();
        assert_eq!(remote.ahead, 1);
        assert_eq!(remote.behind, 0);

        let _ = fs::remove_dir_all(&origin);
        let _ = fs::remove_dir_all(&clone);
    }
}
