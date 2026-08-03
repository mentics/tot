use std::collections::BTreeMap;
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
    /// Pull request numbers keyed by module (repository) name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub module_prs: BTreeMap<String, u64>,
    /// Legacy single PR number from older task files; migrated into [`Self::module_prs`].
    #[serde(default, rename = "pr_number", skip_serializing)]
    pub legacy_pr_number: Option<u64>,
    #[serde(default)]
    pub modules: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<Worktree>,
    /// Free-form notes; newest first in the UI (see [`Task::sort_notes`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
    pub last_used: DateTime<Utc>,
    #[serde(default)]
    pub archived: bool,
}

/// A timestamped note attached to a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub body: String,
    pub created_at: DateTime<Utc>,
}

impl Note {
    pub fn new(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            created_at: Utc::now(),
        }
    }
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
            module_prs: BTreeMap::new(),
            legacy_pr_number: None,
            modules: Vec::new(),
            worktree: None,
            notes: Vec::new(),
            last_used: Utc::now(),
            archived: false,
        }
    }

    /// Keep notes ordered most-recent-first.
    pub fn sort_notes(&mut self) {
        self.notes.sort_by_key(|n| std::cmp::Reverse(n.created_at));
    }

    /// Insert a note and keep the list sorted newest-first.
    pub fn add_note(&mut self, body: impl Into<String>) {
        self.notes.push(Note::new(body));
        self.sort_notes();
    }

    /// Move a legacy top-level `pr_number` into [`Self::module_prs`] when possible.
    pub fn migrate_legacy_pr(&mut self) {
        let Some(pr) = self.legacy_pr_number.take() else {
            return;
        };
        if !self.module_prs.is_empty() {
            return;
        }
        if let Some(module) = self.modules.first() {
            self.module_prs.insert(module.clone(), pr);
        }
    }

    /// Update cognitive-recency timestamp to now.
    pub fn touch(&mut self) {
        self.last_used = Utc::now();
    }

    /// Modules that already have a known PR number.
    pub fn modules_with_prs(&self) -> Vec<(String, u64)> {
        self.modules
            .iter()
            .filter_map(|m| self.module_prs.get(m).map(|n| (m.clone(), *n)))
            .collect()
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
