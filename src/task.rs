use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use color_eyre::eyre::WrapErr;
use serde::{Deserialize, Serialize};

use crate::gitutil;

/// A managed coding task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Stem of the on-disk JSON filename (set on load/create; not part of the file body).
    #[serde(skip)]
    pub file_stem: String,

    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_id: Option<String>,
    /// Pull request number (e.g. GitHub PR #42), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(default)]
    pub modules: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<Worktree>,
    pub last_used: DateTime<Utc>,
    #[serde(default)]
    pub archived: bool,
}

/// Associated Treehouse worktree for a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worktree {
    pub number: i32,
    pub path: PathBuf,
}

impl Task {
    pub fn new(title: impl Into<String>, file_stem: impl Into<String>) -> Self {
        Self {
            file_stem: file_stem.into(),
            title: title.into(),
            branch: None,
            issue_id: None,
            pr_number: None,
            modules: Vec::new(),
            worktree: None,
            last_used: Utc::now(),
            archived: false,
        }
    }

    /// Update cognitive-recency timestamp to now.
    pub fn touch(&mut self) {
        self.last_used = Utc::now();
    }
}

/// Discover available module names for the git repo at `cwd`:
/// main repo directory name + each git submodule name.
pub fn available_modules(cwd: impl AsRef<Path>) -> color_eyre::Result<Vec<String>> {
    let root = gitutil::repo_toplevel(cwd).wrap_err("resolving git repository root")?;
    let main_name = gitutil::main_repo_name(&root)?;
    let mut modules = vec![main_name];
    for (name, _) in gitutil::submodule_entries(&root)? {
        modules.push(name);
    }
    Ok(modules)
}
