//! Shared git helpers for worktree activation and module discovery.

use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{Context, eyre};

/// Run `git` in `cwd` and return stdout on success.
pub fn git_stdout(cwd: impl AsRef<Path>, args: &[&str]) -> color_eyre::Result<String> {
    let cwd = cwd.as_ref();
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .wrap_err_with(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!(
            "git {} failed (in {}): {}",
            args.join(" "),
            cwd.display(),
            stderr.trim()
        ));
    }

    String::from_utf8(output.stdout).wrap_err("git stdout was not valid UTF-8")
}

/// Resolve the git repository toplevel for `cwd`.
pub fn repo_toplevel(cwd: impl AsRef<Path>) -> color_eyre::Result<PathBuf> {
    let out = git_stdout(cwd.as_ref(), &["rev-parse", "--show-toplevel"])
        .wrap_err("resolving git repository root")?;
    Ok(PathBuf::from(out.trim()))
}

/// Basename of the repository root directory (main module name).
pub fn main_repo_name(root: impl AsRef<Path>) -> color_eyre::Result<String> {
    let root = root.as_ref();
    root.file_name()
        .ok_or_else(|| eyre!("git root has no directory name: {}", root.display()))
        .map(|n| n.to_string_lossy().into_owned())
}

/// `(namespace, repository)` parsed from `origin` remote URL (or any remote).
///
/// Accepts common GitHub-style forms:
/// - `git@host:namespace/repo.git`
/// - `https://host/namespace/repo.git`
/// - `ssh://git@host/namespace/repo.git`
pub fn remote_namespace_and_repo(
    repo: impl AsRef<Path>,
    remote: &str,
) -> color_eyre::Result<(String, String)> {
    let url = git_stdout(repo.as_ref(), &["remote", "get-url", remote])
        .wrap_err_with(|| format!("reading git remote `{remote}` URL"))?;
    parse_remote_namespace_and_repo(url.trim())
        .wrap_err_with(|| format!("parsing remote `{remote}` URL `{url}`"))
}

/// Parse `namespace` / `repository` from a git remote URL string.
pub fn parse_remote_namespace_and_repo(url: &str) -> color_eyre::Result<(String, String)> {
    let url = url.trim();
    if url.is_empty() {
        return Err(eyre!("empty remote URL"));
    }

    // SSH scp-like: git@host:namespace/repo[.git]
    if let Some(rest) = url.split_once(':').and_then(|(left, right)| {
        if left.contains("://") || right.starts_with('/') {
            None
        } else if left.contains('@') || !left.contains('/') {
            Some(right)
        } else {
            None
        }
    }) {
        return split_owner_repo(rest);
    }

    // https://host/namespace/repo[.git] or ssh://git@host/namespace/repo
    let path = if let Some(idx) = url.find("://") {
        let after = &url[idx + 3..];
        after
            .split_once('/')
            .map(|(_, path)| path)
            .ok_or_else(|| eyre!("remote URL has no path: {url}"))?
    } else {
        url
    };

    split_owner_repo(path)
}

fn split_owner_repo(path: &str) -> color_eyre::Result<(String, String)> {
    let path = path.trim_start_matches('/');
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let namespace = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre!("remote URL missing namespace: {path}"))?;
    let repository = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre!("remote URL missing repository: {path}"))?;
    Ok((namespace.to_string(), repository.to_string()))
}

/// Submodule `(name, path)` pairs from `.gitmodules` under `root`.
pub fn submodule_entries(root: impl AsRef<Path>) -> color_eyre::Result<Vec<(String, PathBuf)>> {
    let root = root.as_ref();
    let gitmodules = root.join(".gitmodules");
    if !gitmodules.is_file() {
        return Ok(Vec::new());
    }

    let out = match git_stdout(
        root,
        &[
            "config",
            "-f",
            ".gitmodules",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ],
    ) {
        Ok(out) => out,
        Err(_) => return Ok(Vec::new()),
    };

    let mut entries = Vec::new();
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        if path.is_empty() {
            continue;
        }
        if let Some(name) = key
            .strip_prefix("submodule.")
            .and_then(|rest| rest.strip_suffix(".path"))
            && !name.is_empty()
        {
            entries.push((name.to_string(), PathBuf::from(path)));
        }
    }
    Ok(entries)
}

/// True if local branch `name` exists in `repo`.
pub fn local_branch_exists(repo: impl AsRef<Path>, name: &str) -> color_eyre::Result<bool> {
    let refname = format!("refs/heads/{name}");
    let output = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", &refname])
        .current_dir(repo.as_ref())
        .output()
        .wrap_err("running git show-ref")?;
    Ok(output.status.success())
}

/// Create `branch` if missing, then check it out in `repo`.
///
/// If checkout fails because the branch is locked by another worktree whose path is
/// **missing on disk**, forget that stale registration and retry once.
pub fn checkout_or_create_branch(repo: impl AsRef<Path>, branch: &str) -> color_eyre::Result<()> {
    match checkout_or_create_branch_once(repo.as_ref(), branch) {
        Ok(()) => Ok(()),
        Err(err) => {
            let Some(lock) = parse_branch_in_use_msg(&format!("{err:#}")) else {
                return Err(err);
            };
            if lock.conflicting_path.exists() {
                return Err(err);
            }
            // Stale registration after a crash / path move: forget and retry.
            forget_worktree_registration(repo.as_ref(), &lock.conflicting_path).wrap_err_with(
                || {
                    format!(
                        "branch `{branch}` locked by missing worktree {}; \
                         failed to forget registration",
                        lock.conflicting_path.display()
                    )
                },
            )?;
            checkout_or_create_branch_once(repo.as_ref(), branch)
        }
    }
}

fn checkout_or_create_branch_once(repo: &Path, branch: &str) -> color_eyre::Result<()> {
    if branch.is_empty() {
        return Err(eyre!("branch name is empty"));
    }

    if local_branch_exists(repo, branch)? {
        git_stdout(repo, &["checkout", branch])
            .wrap_err_with(|| format!("checking out branch `{branch}` in {}", repo.display()))?;
    } else {
        git_stdout(repo, &["checkout", "-b", branch]).wrap_err_with(|| {
            format!(
                "creating and checking out branch `{branch}` in {}",
                repo.display()
            )
        })?;
    }
    Ok(())
}

/// A branch cannot be checked out because another worktree already has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInUse {
    pub branch: String,
    pub conflicting_path: PathBuf,
}

/// Parse git's "`branch` is already used by worktree at `path`" from an error message.
pub fn parse_branch_in_use_msg(msg: &str) -> Option<BranchInUse> {
    let lower = msg.to_lowercase();
    const MARKER: &str = "is already used by worktree at";
    let marker_pos = lower.find(MARKER)?;
    let before = &msg[..marker_pos];
    let after = &msg[marker_pos + MARKER.len()..];

    let branch = extract_quoted_segment(before.trim_end())?
        .trim()
        .to_string();
    if branch.is_empty() {
        return None;
    }

    let path = extract_quoted_segment(after.trim_start())
        .or_else(|| {
            after.split_whitespace().next().map(|s| {
                s.trim_matches(|c| c == '.' || c == ';' || c == ',')
                    .to_string()
            })
        })?
        .trim()
        .to_string();
    if path.is_empty() {
        return None;
    }

    Some(BranchInUse {
        branch,
        conflicting_path: PathBuf::from(path),
    })
}

fn extract_quoted_segment(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    // Prefer the last quoted span in `s`.
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        let quote = bytes[i];
        if quote != b'\'' && quote != b'"' {
            continue;
        }
        let closer = i;
        let mut j = i;
        while j > 0 {
            j -= 1;
            if bytes[j] == quote {
                return Some(s[j + 1..closer].to_string());
            }
        }
    }
    None
}

/// Unregister a worktree path (`remove --force`), falling back to `prune`.
pub fn forget_worktree_registration(
    repo: impl AsRef<Path>,
    worktree_path: impl AsRef<Path>,
) -> color_eyre::Result<()> {
    let repo = repo.as_ref();
    let path = worktree_path.as_ref();
    match worktree_remove_force(repo, path) {
        Ok(()) => Ok(()),
        Err(remove_err) => worktree_prune(repo).wrap_err_with(|| {
            format!(
                "git worktree remove --force {} failed ({remove_err:#}); prune also failed",
                path.display()
            )
        }),
    }
}

/// Drop registrations for worktrees whose directories are gone (`git worktree prune`).
pub fn worktree_prune(repo: impl AsRef<Path>) -> color_eyre::Result<()> {
    let repo = repo.as_ref();
    git_stdout(repo, &["worktree", "prune"])
        .wrap_err_with(|| format!("git worktree prune failed in {}", repo.display()))?;
    Ok(())
}

/// `git worktree prune` in every submodule checkout, then the main repo.
///
/// Treehouse registers submodule worktrees against each submodule's backing
/// checkout — pruning only the main repo leaves those stale and the next lease
/// fails on another module.
pub fn worktree_prune_all(main_root: impl AsRef<Path>) -> color_eyre::Result<()> {
    let main_root = main_root.as_ref();
    let mut errors = Vec::new();

    for (name, rel) in submodule_entries(main_root)? {
        let checkout = main_root.join(&rel);
        if !checkout.is_dir() {
            continue;
        }
        if let Err(err) = worktree_prune(&checkout) {
            errors.push(format!(
                "submodule `{name}` ({}): {err:#}",
                checkout.display()
            ));
        }
    }

    if let Err(err) = worktree_prune(main_root) {
        errors.push(format!("main ({}): {err:#}", main_root.display()));
    }

    if !errors.is_empty() {
        return Err(eyre!(
            "git worktree prune failed in one or more repos:\n{}",
            errors.join("\n")
        ));
    }
    Ok(())
}

/// Unregister a worktree path, even if the directory is missing (`git worktree remove --force`).
pub fn worktree_remove_force(
    repo: impl AsRef<Path>,
    worktree_path: impl AsRef<Path>,
) -> color_eyre::Result<()> {
    let repo = repo.as_ref();
    let path = worktree_path.as_ref();
    let path_str = path
        .to_str()
        .ok_or_else(|| eyre!("worktree path is not valid UTF-8: {}", path.display()))?;
    git_stdout(repo, &["worktree", "remove", "--force", path_str]).wrap_err_with(|| {
        format!(
            "git worktree remove --force {} failed in {}",
            path.display(),
            repo.display()
        )
    })?;
    Ok(())
}

/// True when `git worktree remove` failed because the path is not registered.
fn is_not_a_worktree_err(err: &color_eyre::Report) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    msg.contains("not a working tree")
        || msg.contains("is not a valid path")
        || msg.contains("no such file or directory")
}

fn worktree_remove_force_if_registered(
    repo: impl AsRef<Path>,
    worktree_path: impl AsRef<Path>,
) -> color_eyre::Result<()> {
    match worktree_remove_force(repo, worktree_path) {
        Ok(()) => Ok(()),
        Err(err) if is_not_a_worktree_err(&err) => Ok(()),
        Err(err) => Err(err),
    }
}

/// `git worktree remove --force` for the pool slot in the main repo and every submodule.
///
/// Given a conflict at `.../<N>/<repo>` or `.../<N>/<repo>/<module>`, unregisters
/// the main worktree path and each `.../<N>/<repo>/<submodule_path>` against the
/// corresponding backing checkout. Missing registrations are ignored.
pub fn worktree_remove_slot_everywhere(
    main_root: impl AsRef<Path>,
    problem_path: impl AsRef<Path>,
) -> color_eyre::Result<()> {
    let main_root = main_root.as_ref();
    let problem_path = problem_path.as_ref();
    let main_wt =
        pool_main_worktree_dir(problem_path).unwrap_or_else(|| problem_path.to_path_buf());

    let mut errors = Vec::new();

    if let Err(err) = worktree_remove_force_if_registered(main_root, &main_wt) {
        errors.push(format!("main ({}): {err:#}", main_root.display()));
    }

    for (name, rel) in submodule_entries(main_root)? {
        let checkout = main_root.join(&rel);
        if !checkout.is_dir() {
            continue;
        }
        let child = main_wt.join(&rel);
        if let Err(err) = worktree_remove_force_if_registered(&checkout, &child) {
            errors.push(format!(
                "submodule `{name}` ({}): {err:#}",
                checkout.display()
            ));
        }
    }

    if !errors.is_empty() {
        return Err(eyre!(
            "git worktree remove failed in one or more repos:\n{}",
            errors.join("\n")
        ));
    }
    Ok(())
}

/// Delete a worktree directory on disk (`remove_dir_all`). No-op if already gone.
///
/// Callers must only pass the conflict / leftover worktree path shown to the user.
pub fn remove_worktree_dir(path: impl AsRef<Path>) -> color_eyre::Result<()> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(());
    }
    std::fs::remove_dir_all(path)
        .wrap_err_with(|| format!("failed to delete worktree directory {}", path.display()))?;
    Ok(())
}

/// Clear a blocking worktree path: try `git worktree remove --force`, then delete leftovers.
#[allow(dead_code)] // kept for single-repo recovery; slot-wide path uses clear_worktree_slot_everywhere
pub fn clear_worktree_path(
    repo: impl AsRef<Path>,
    worktree_path: impl AsRef<Path>,
) -> color_eyre::Result<()> {
    let path = worktree_path.as_ref();
    // Prefer the official git removal when the path is registered.
    match worktree_remove_force(repo.as_ref(), path) {
        Ok(()) => {}
        Err(_) if path.exists() => {
            // Not registered (or remove failed); fall through to directory delete.
        }
        Err(err) => {
            // Path already gone and remove failed — treat as cleared.
            if !path.exists() {
                return Ok(());
            }
            return Err(err);
        }
    }
    if path.exists() {
        remove_worktree_dir(path)?;
    }
    Ok(())
}

/// Unregister the pool slot in main + all submodules, then delete leftover dirs on disk.
pub fn clear_worktree_slot_everywhere(
    main_root: impl AsRef<Path>,
    problem_path: impl AsRef<Path>,
) -> color_eyre::Result<()> {
    let main_root = main_root.as_ref();
    let problem_path = problem_path.as_ref();
    worktree_remove_slot_everywhere(main_root, problem_path)?;
    clear_leftover_pool_dirs_after_registration_fix(problem_path)?;
    Ok(())
}

/// After clearing a stale registration, delete leftover dirs at `path` (and the main
/// worktree slot when `path` is a submodule under `.../<N>/<repo>/...`).
///
/// Partial Treehouse leases often leave the main worktree on disk after a submodule
/// registration failure; prune alone then retries into "already exists".
pub fn clear_leftover_pool_dirs_after_registration_fix(
    problem_path: impl AsRef<Path>,
) -> color_eyre::Result<()> {
    let path = problem_path.as_ref();
    if path.exists() {
        remove_worktree_dir(path)?;
    }
    // If this was a submodule path (.../<N>/<repo>/<module>), also clear the main
    // slot when present so the next lease is not blocked by "already exists".
    if let Some(main) = pool_main_worktree_dir(path) {
        if main != path && main.exists() {
            remove_worktree_dir(&main)?;
        }
    }
    Ok(())
}

/// Derive `.../<N>/<repo>` from a pool path (main or nested submodule).
fn pool_main_worktree_dir(path: &Path) -> Option<PathBuf> {
    // Walk up until parent is numeric (the pool slot number).
    let mut current = path;
    loop {
        let parent = current.parent()?;
        let name = parent.file_name()?.to_str()?;
        if name.parse::<u32>().is_ok() {
            return Some(current.to_path_buf());
        }
        current = parent;
    }
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

    #[test]
    fn parses_common_remote_urls() {
        assert_eq!(
            parse_remote_namespace_and_repo("git@github.com:acme/widgets.git").unwrap(),
            ("acme".into(), "widgets".into())
        );
        assert_eq!(
            parse_remote_namespace_and_repo("https://github.com/acme/widgets.git").unwrap(),
            ("acme".into(), "widgets".into())
        );
        assert_eq!(
            parse_remote_namespace_and_repo("ssh://git@github.com/acme/widgets.git").unwrap(),
            ("acme".into(), "widgets".into())
        );
    }

    #[test]
    fn checkout_creates_and_switches() {
        let dir = std::env::temp_dir().join(format!("tod-gitutil-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        init_repo(&dir);

        checkout_or_create_branch(&dir, "feature/x").unwrap();
        let head = git_stdout(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head.trim(), "feature/x");

        checkout_or_create_branch(&dir, "temp1").unwrap();
        let head = git_stdout(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head.trim(), "temp1");

        checkout_or_create_branch(&dir, "feature/x").unwrap();
        let head = git_stdout(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head.trim(), "feature/x");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_branch_in_use_error() {
        let msg = "git checkout opt/inv-569 failed (in /wt/1/workspace/flagship): \
             fatal: 'opt/inv-569-recalculate_open_kit_requests' is already used by worktree at \
             '/home/vscode/.treehouse/workspace-df5f8e/3/workspace/flagship'";
        let lock = parse_branch_in_use_msg(msg).unwrap();
        assert_eq!(lock.branch, "opt/inv-569-recalculate_open_kit_requests");
        assert_eq!(
            lock.conflicting_path,
            PathBuf::from("/home/vscode/.treehouse/workspace-df5f8e/3/workspace/flagship")
        );
    }

    #[test]
    fn pool_main_dir_from_submodule_path() {
        assert_eq!(
            pool_main_worktree_dir(Path::new(
                "/home/vscode/worktrees/workspace-df5f8e/1/workspace/flagship"
            )),
            Some(PathBuf::from(
                "/home/vscode/worktrees/workspace-df5f8e/1/workspace"
            ))
        );
        assert_eq!(
            pool_main_worktree_dir(Path::new(
                "/home/vscode/.treehouse/workspace-df5f8e/2/workspace"
            )),
            Some(PathBuf::from(
                "/home/vscode/.treehouse/workspace-df5f8e/2/workspace"
            ))
        );
    }

    #[test]
    fn remove_worktree_dir_deletes_existing_path() {
        let dir = std::env::temp_dir().join(format!("tod-rm-wt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("file"), "x").unwrap();
        remove_worktree_dir(&dir).unwrap();
        assert!(!dir.exists());
        // Missing path is fine.
        remove_worktree_dir(&dir).unwrap();
    }
}
