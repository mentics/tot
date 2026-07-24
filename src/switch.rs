//! Switch-to-task workflow: activate worktree branches and open Cursor.

use std::path::{Path, PathBuf};

use color_eyre::eyre::eyre;

use crate::gitutil::{self, BranchInUse};
use crate::task::Worktree;
use crate::treehouse;

/// Branch is locked by another worktree whose directory still exists.
#[derive(Debug, Clone)]
pub struct BranchLockedError {
    pub branch: String,
    pub conflicting_path: PathBuf,
    /// Repo directory where checkout was attempted (main or submodule).
    pub checkout_repo: PathBuf,
    /// Current task worktree being activated.
    pub current_worktree: Worktree,
    /// Main Treehouse worktree that owns `conflicting_path`, when derivable.
    pub other_worktree: Option<Worktree>,
}

/// Failure from [`activate_worktree`].
#[derive(Debug)]
pub enum ActivateError {
    /// Task association points at a directory that no longer exists.
    PathMissing {
        worktree: Worktree,
    },
    /// Needs user choice: conflicting path exists on disk.
    BranchLocked(BranchLockedError),
    Other(color_eyre::Report),
}

impl std::fmt::Display for ActivateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActivateError::PathMissing { worktree } => write!(
                f,
                "worktree path does not exist: {}",
                worktree.path.display()
            ),
            ActivateError::BranchLocked(e) => write!(
                f,
                "branch `{}` is already used by worktree at {}",
                e.branch,
                e.conflicting_path.display()
            ),
            ActivateError::Other(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for ActivateError {}

impl From<color_eyre::Report> for ActivateError {
    fn from(value: color_eyre::Report) -> Self {
        ActivateError::Other(value)
    }
}

/// Check out the task branch (or `temp{N}`) in the worktree main repo and each submodule.
///
/// `on_progress` is called with a human-readable step label before each checkout.
/// Missing conflicting worktree registrations are forgotten automatically inside checkout.
/// If a conflicting path **exists**, returns [`ActivateError::BranchLocked`].
pub fn activate_worktree(
    worktree: &Worktree,
    task_modules: &[String],
    branch: &str,
    mut on_progress: impl FnMut(String) -> color_eyre::Result<()>,
) -> Result<(), ActivateError> {
    if branch.is_empty() {
        return Err(ActivateError::Other(eyre!(
            "cannot activate worktree without a branch name"
        )));
    }

    let root = &worktree.path;
    if !root.is_dir() {
        return Err(ActivateError::PathMissing {
            worktree: worktree.clone(),
        });
    }

    let main_name = gitutil::main_repo_name(root).map_err(ActivateError::Other)?;
    let temp_branch = format!("temp{}", worktree.number);

    let main_target = if task_modules.iter().any(|m| m == &main_name) {
        branch
    } else {
        temp_branch.as_str()
    };
    on_progress(format!(
        "Activate worktree: main `{main_name}` → `{main_target}`"
    ))
    .map_err(ActivateError::Other)?;
    checkout_for_activate(root, main_target, worktree)?;

    for (name, rel) in gitutil::submodule_entries(root).map_err(ActivateError::Other)? {
        let sub_path = root.join(&rel);
        if !sub_path.is_dir() {
            return Err(ActivateError::Other(eyre!(
                "submodule `{name}` path missing in worktree: {}",
                sub_path.display()
            )));
        }
        let target = if task_modules.iter().any(|m| m == &name) {
            branch
        } else {
            temp_branch.as_str()
        };
        on_progress(format!(
            "Activate worktree: submodule `{name}` → `{target}`"
        ))
        .map_err(ActivateError::Other)?;
        checkout_for_activate(&sub_path, target, worktree)
            .map_err(|err| annotate_submodule(err, &name))?;
    }

    Ok(())
}

fn annotate_submodule(err: ActivateError, name: &str) -> ActivateError {
    match err {
        locked @ ActivateError::BranchLocked(_) => locked,
        missing @ ActivateError::PathMissing { .. } => missing,
        ActivateError::Other(report) => {
            ActivateError::Other(report.wrap_err(format!("activating submodule `{name}`")))
        }
    }
}

fn checkout_for_activate(
    repo: &Path,
    branch: &str,
    current_worktree: &Worktree,
) -> Result<(), ActivateError> {
    match gitutil::checkout_or_create_branch(repo, branch) {
        Ok(()) => Ok(()),
        Err(err) => {
            let msg = format!("{err:#}");
            if let Some(BranchInUse {
                branch: locked_branch,
                conflicting_path,
            }) = gitutil::parse_branch_in_use_msg(&msg)
            {
                // Missing paths are already auto-forgotten + retried inside checkout.
                // If we still see this, the conflicting path exists (or forget failed).
                if conflicting_path.exists() {
                    let other_worktree = treehouse::main_worktree_from_pool_path(&conflicting_path)
                        .map(|(number, path)| Worktree { number, path });
                    return Err(ActivateError::BranchLocked(BranchLockedError {
                        branch: locked_branch,
                        conflicting_path,
                        checkout_repo: repo.to_path_buf(),
                        current_worktree: current_worktree.clone(),
                        other_worktree,
                    }));
                }
            }
            Err(ActivateError::Other(err.wrap_err(format!(
                "checking out branch `{branch}` in {}",
                repo.display()
            ))))
        }
    }
}

/// Lease a new Treehouse worktree from `cwd` (main repo).
pub fn lease_new_worktree(cwd: impl AsRef<Path>) -> color_eyre::Result<Worktree> {
    let leased = treehouse::lease_worktree(cwd.as_ref())?;
    Ok(leased.into())
}

/// Best-effort cleanup when a task's worktree directory is gone.
///
/// Tries Treehouse return (so the pool slot is freed) and forgets the git
/// worktree registration in the main repo. Failures are ignored — the caller
/// still clears the task association and can lease a fresh worktree.
pub fn cleanup_missing_worktree(path: impl AsRef<Path>) {
    let path = path.as_ref();
    let _ = treehouse::return_worktree(path);
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(main) = gitutil::repo_toplevel(&cwd) {
            let _ = gitutil::forget_worktree_registration(&main, path);
        }
    }
}

/// Open Cursor on the worktree path (detached; does not wait).
pub fn launch_cursor(worktree: &Worktree) -> color_eyre::Result<()> {
    treehouse::launch_cursor(&worktree.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitutil;
    use crate::task::Worktree;
    use std::fs;
    use std::path::PathBuf;
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
    fn activate_checks_out_task_or_temp_branch() {
        let dir = std::env::temp_dir().join(format!("tod-switch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        init_repo(&dir);
        let main = gitutil::main_repo_name(&dir).unwrap();

        let wt = Worktree {
            number: 7,
            path: PathBuf::from(&dir),
        };

        // Main module selected → task branch.
        activate_worktree(&wt, &[main.clone()], "feat/switch-test", |_| Ok(())).unwrap();
        let head = gitutil::git_stdout(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head.trim(), "feat/switch-test");

        // Main not selected → temp7.
        activate_worktree(&wt, &[], "feat/switch-test", |_| Ok(())).unwrap();
        let head = gitutil::git_stdout(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head.trim(), "temp7");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn activate_reports_path_missing() {
        let missing = Worktree {
            number: 7,
            path: PathBuf::from("/tmp/tod-definitely-missing-worktree-path"),
        };
        let err = activate_worktree(&missing, &[], "feat/x", |_| Ok(())).unwrap_err();
        match err {
            ActivateError::PathMissing { worktree } => {
                assert_eq!(worktree.number, 7);
                assert_eq!(worktree.path, missing.path);
            }
            other => panic!("expected PathMissing, got {other}"),
        }
    }
}
