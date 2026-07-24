use std::env;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use color_eyre::eyre::{WrapErr, eyre};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::widgets::ListState;
use tui_textarea::{Input, Key, TextArea};

use crate::create::{self, ParsedCreateInput};
use crate::credentials;
use crate::dirty::{self, DirtyAction, DirtyReport};
use crate::gitutil;
use crate::linear;
use crate::persist;
use crate::switch;
use crate::task::{self, Task};
use crate::text_input;
use crate::treehouse;
use crate::ui;

/// Why the Linear credential prompt is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CredentialPromptKind {
    /// No key in the OS keyring yet.
    #[default]
    Missing,
    /// Linear rejected the key (401/403 or auth GraphQL error).
    Invalid,
}

/// Primary UI screen / mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    TaskList,
    Archive,
    Edit,
    /// Single-line input for create-new-task.
    CreatePrompt,
    /// Prompt for Linear API key when missing from the keyring.
    CredentialPrompt,
    /// Encrypted credential file exists but cannot be decrypted.
    CredentialFileBroken,
    /// Multiselect modules before leasing a worktree.
    SwitchModules,
    /// Branch name prompt before leasing a worktree.
    SwitchBranch,
    /// Dirty worktree warning before release (and optionally archive).
    DirtyWarning,
    /// Stale or leftover worktree path blocking a Treehouse lease.
    StaleWorktree,
    /// Branch locked by another worktree during activate.
    BranchLocked,
}

/// In-progress release (R alone, or as part of archive).
#[derive(Debug)]
pub struct ReleaseState {
    pub task_idx: usize,
    /// When true, archive the task after a successful release.
    pub then_archive: bool,
    pub report: DirtyReport,
    pub actions: Vec<DirtyAction>,
    pub action_cursor: usize,
}

/// Recovery choice when a lease hits a worktree path conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleWorktreeAction {
    /// Clear this path's registration, then retry (like allowing `git worktree add -f`).
    Override,
    /// `git worktree prune` — drop all missing registrations, then retry.
    Prune,
    /// `git worktree remove --force` on this path, then retry.
    Remove,
    /// Remove registration if any, then delete the worktree directory on disk.
    ClearPath,
    /// Delete the worktree directory on disk (`rm -rf` that path).
    DeleteDirectory,
    Cancel,
}

impl StaleWorktreeAction {
    pub fn label(self) -> &'static str {
        match self {
            StaleWorktreeAction::Override => "Override (force)",
            StaleWorktreeAction::Prune => "Prune",
            StaleWorktreeAction::Remove => "Remove",
            StaleWorktreeAction::ClearPath => "Clear path (deletes dir)",
            StaleWorktreeAction::DeleteDirectory => "Delete worktree directory",
            StaleWorktreeAction::Cancel => "Cancel",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            StaleWorktreeAction::Override => {
                "Unregister this pool slot in the main repo and every submodule \
                 (`git worktree remove --force`), delete leftover dirs under the \
                 slot if present, then retry the lease."
            }
            StaleWorktreeAction::Prune => {
                "Run `git worktree prune` in every submodule, then the main repo, \
                 to clear all missing-but-registered worktrees. Also deletes leftover \
                 dirs under this pool slot if present, then retries the lease."
            }
            StaleWorktreeAction::Remove => {
                "Unregister this pool slot in the main repo and every submodule \
                 (`git worktree remove --force`), delete leftover dirs under the \
                 slot if present, then retry the lease."
            }
            StaleWorktreeAction::ClearPath => {
                "Unregister this pool slot in the main repo and every submodule, \
                 then permanently delete leftover directories under the slot \
                 (`rm -rf`), then retry the lease."
            }
            StaleWorktreeAction::DeleteDirectory => {
                "Permanently delete the worktree directory shown above (and its \
                 main pool slot if this was a submodule path). Does not change \
                 git registrations by itself."
            }
            StaleWorktreeAction::Cancel => {
                "Abort the switch and leave the path / registrations unchanged."
            }
        }
    }

    pub fn shortcut(self) -> char {
        match self {
            StaleWorktreeAction::Override => 'o',
            StaleWorktreeAction::Prune => 'p',
            StaleWorktreeAction::Remove => 'r',
            StaleWorktreeAction::ClearPath => 'c',
            StaleWorktreeAction::DeleteDirectory => 'd',
            StaleWorktreeAction::Cancel => 'x',
        }
    }

    pub fn for_kind(kind: treehouse::LeasePathConflictKind) -> &'static [StaleWorktreeAction] {
        match kind {
            treehouse::LeasePathConflictKind::MissingButRegistered => &[
                StaleWorktreeAction::Override,
                StaleWorktreeAction::Prune,
                StaleWorktreeAction::Remove,
                StaleWorktreeAction::Cancel,
            ],
            treehouse::LeasePathConflictKind::AlreadyExists => &[
                StaleWorktreeAction::ClearPath,
                StaleWorktreeAction::DeleteDirectory,
                StaleWorktreeAction::Remove,
                StaleWorktreeAction::Cancel,
            ],
        }
    }
}

/// Prompt shown when Treehouse lease fails on a conflicting worktree path.
#[derive(Debug)]
pub struct StaleWorktreeState {
    /// Task file stem so we can resume after indices shift.
    pub task_stem: String,
    pub problem_path: PathBuf,
    /// Source main repo (cwd toplevel). Registration actions apply here and in
    /// every submodule under it.
    pub repo_root: PathBuf,
    /// Submodule path relative to the main repo, when the conflict path is under one.
    pub submodule_rel: Option<PathBuf>,
    pub kind: treehouse::LeasePathConflictKind,
    pub action_cursor: usize,
}

/// Prompt when activate cannot check out a branch locked by another worktree.
#[derive(Debug)]
pub struct BranchLockedState {
    pub task_stem: String,
    pub branch: String,
    pub conflicting_path: PathBuf,
    pub checkout_repo: PathBuf,
    pub current_worktree: crate::task::Worktree,
    pub other_worktree: Option<crate::task::Worktree>,
    pub action_cursor: usize,
}

/// Recovery choice when a branch is locked by another existing worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchLockedAction {
    /// Point the task at the other Treehouse worktree and continue.
    AssociateOther,
    /// Forget/remove the other worktree registration, then retry activate on the current one.
    RemoveOther,
    Cancel,
}

impl BranchLockedAction {
    pub fn label(self) -> &'static str {
        match self {
            BranchLockedAction::AssociateOther => "Use that worktree",
            BranchLockedAction::RemoveOther => "Remove other worktree",
            BranchLockedAction::Cancel => "Cancel",
        }
    }

    pub fn description(self, other_num: Option<i32>) -> String {
        match self {
            BranchLockedAction::AssociateOther => {
                let n = other_num
                    .map(|n| format!("worktree {n}"))
                    .unwrap_or_else(|| "that worktree".to_string());
                format!(
                    "Associate this task with {n} (where the branch is already checked out), \
                     return the current Treehouse lease if it differs, then continue activate."
                )
            }
            BranchLockedAction::RemoveOther => {
                "Run `git worktree remove --force` on the conflicting path so this branch \
                 can be checked out in the current worktree, then retry activate. \
                 Destructive if that other tree has real work."
                    .to_string()
            }
            BranchLockedAction::Cancel => {
                "Abort the switch and leave worktree associations unchanged.".to_string()
            }
        }
    }

    pub fn shortcut(self) -> char {
        match self {
            BranchLockedAction::AssociateOther => 'u',
            BranchLockedAction::RemoveOther => 'r',
            BranchLockedAction::Cancel => 'x',
        }
    }

    pub fn available(has_other_worktree: bool) -> Vec<BranchLockedAction> {
        let mut actions = Vec::new();
        if has_other_worktree {
            actions.push(BranchLockedAction::AssociateOther);
        }
        actions.push(BranchLockedAction::RemoveOther);
        actions.push(BranchLockedAction::Cancel);
        actions
    }
}

/// Which control is focused in the edit view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditFocus {
    Title,
    Branch,
    IssueId,
    Modules,
    /// Associated Treehouse worktree (clearable; not editable).
    Worktree,
}

#[derive(Debug)]
pub struct EditState {
    /// Index into `App::tasks`.
    pub task_idx: usize,
    pub return_to: View,
    pub focus: EditFocus,
    /// Highlighted row in the available-modules list.
    pub module_cursor: usize,
    pub available_modules: Vec<String>,
    pub title_input: TextArea<'static>,
    pub branch_input: TextArea<'static>,
    pub issue_input: TextArea<'static>,
}

/// In-progress switch prerequisites (modules / branch) for a task without a worktree.
#[derive(Debug)]
pub struct SwitchPrepState {
    pub task_idx: usize,
    pub module_cursor: usize,
    pub available_modules: Vec<String>,
    pub branch_input: TextArea<'static>,
}

/// Message shown in the footer status area.
#[derive(Debug, Clone)]
pub struct StatusMessage {
    pub text: String,
    pub is_error: bool,
    /// When true, the footer prefixes the text with an animated spinner.
    pub busy: bool,
}

/// Braille spinner frames for busy status.
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug)]
pub struct App {
    pub tasks: Vec<Task>,
    pub view: View,
    pub list_state: ListState,
    pub archive_state: ListState,
    pub edit: Option<EditState>,
    pub switch_prep: Option<SwitchPrepState>,
    pub release: Option<ReleaseState>,
    pub stale_worktree: Option<StaleWorktreeState>,
    pub branch_locked: Option<BranchLockedState>,
    /// Create-prompt input buffer.
    pub create_input: TextArea<'static>,
    /// Credential-prompt input buffer (Linear API key).
    pub credential_input: TextArea<'static>,
    /// Copy / reason for the credential prompt (missing vs invalid key).
    pub credential_prompt_kind: CredentialPromptKind,
    /// Path shown when [`View::CredentialFileBroken`] is active.
    pub credential_broken_path: Option<PathBuf>,
    /// Create input waiting while the user supplies Linear credentials.
    pending_create_input: Option<String>,
    /// After prompts, run lease/activate/cursor on the next loop tick (so the UI can redraw).
    pending_finish_switch: Option<usize>,
    /// After fixing a missing-but-registered conflict, the next "already exists"
    /// lease failure is cleared automatically once (partial lease leftover).
    lease_auto_clear_already_exists: bool,
    pub status: Option<StatusMessage>,
    /// Index into [`SPINNER_FRAMES`] while a busy status is showing.
    pub spinner_frame: usize,
    should_quit: bool,
}

impl App {
    pub fn new() -> color_eyre::Result<Self> {
        let tasks = persist::load_all_tasks()?;
        let mut list_state = ListState::default();
        // Row 0 is always "Create new task".
        list_state.select(Some(0));
        let mut archive_state = ListState::default();
        if tasks.iter().any(|t| t.archived) {
            archive_state.select(Some(0));
        }
        Ok(Self {
            tasks,
            view: View::TaskList,
            list_state,
            archive_state,
            edit: None,
            switch_prep: None,
            release: None,
            stale_worktree: None,
            branch_locked: None,
            create_input: text_input::single_line(""),
            credential_input: text_input::single_line_masked(""),
            credential_prompt_kind: CredentialPromptKind::Missing,
            credential_broken_path: None,
            pending_create_input: None,
            pending_finish_switch: None,
            lease_auto_clear_already_exists: false,
            status: None,
            spinner_frame: 0,
            should_quit: false,
        })
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> color_eyre::Result<()> {
        while !self.should_quit {
            terminal.draw(|frame| ui::draw(frame, &mut self))?;
            if let Some(task_idx) = self.pending_finish_switch.take() {
                self.finish_switch(terminal, task_idx)?;
                continue;
            }
            self.handle_events(terminal)?;
        }
        Ok(())
    }

    pub fn spinner_char(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()]
    }

    fn tick_spinner(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    /// Set a busy status, advance the spinner, and redraw immediately.
    fn report_progress(
        &mut self,
        terminal: &mut DefaultTerminal,
        msg: impl Into<String>,
    ) -> color_eyre::Result<()> {
        self.set_busy(msg);
        self.tick_spinner();
        terminal.draw(|frame| ui::draw(frame, self))?;
        Ok(())
    }

    /// Run `work` on a background thread while animating the busy spinner.
    fn run_busy<T, F>(
        &mut self,
        terminal: &mut DefaultTerminal,
        msg: impl Into<String>,
        work: F,
    ) -> color_eyre::Result<T>
    where
        F: FnOnce() -> color_eyre::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.set_busy(msg);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(work());
        });
        loop {
            self.tick_spinner();
            terminal.draw(|frame| ui::draw(frame, self))?;
            match rx.recv_timeout(Duration::from_millis(80)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(eyre!("background work thread disconnected"));
                }
            }
        }
    }

    fn handle_events(&mut self, terminal: &mut DefaultTerminal) -> color_eyre::Result<()> {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(terminal, key)?,
            _ => {}
        }
        Ok(())
    }

    fn handle_key(
        &mut self,
        terminal: &mut DefaultTerminal,
        key: KeyEvent,
    ) -> color_eyre::Result<()> {
        match self.view {
            View::TaskList => self.handle_task_list_key(terminal, key)?,
            View::Archive => self.handle_archive_key(key)?,
            View::Edit => self.handle_edit_key(terminal, key)?,
            View::CreatePrompt => self.handle_create_prompt_key(key)?,
            View::CredentialPrompt => self.handle_credential_prompt_key(key)?,
            View::CredentialFileBroken => self.handle_credential_file_broken_key(key)?,
            View::SwitchModules => self.handle_switch_modules_key(key)?,
            View::SwitchBranch => self.handle_switch_branch_key(key)?,
            View::DirtyWarning => self.handle_dirty_warning_key(terminal, key)?,
            View::StaleWorktree => self.handle_stale_worktree_key(key)?,
            View::BranchLocked => self.handle_branch_locked_key(key)?,
        }
        Ok(())
    }

    // --- Task list ---------------------------------------------------------

    fn handle_task_list_key(
        &mut self,
        terminal: &mut DefaultTerminal,
        key: KeyEvent,
    ) -> color_eyre::Result<()> {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
            // Shift+A opens archive view. Uppercase 'A' usually implies Shift even
            // when the terminal omits KeyModifiers::SHIFT.
            KeyCode::Char('a') | KeyCode::Char('A') if shift => self.open_archive_view(),
            KeyCode::Char('A') => self.open_archive_view(),
            KeyCode::Char('a') => self.archive_selected(terminal)?,
            KeyCode::Char('e') | KeyCode::Char('E') => self.open_edit_for_list_selection()?,
            KeyCode::Char('r') | KeyCode::Char('R') => self.release_selected(terminal)?,
            KeyCode::Down | KeyCode::Char('j') => self.select_next_list(),
            KeyCode::Up | KeyCode::Char('k') => self.select_previous_list(),
            KeyCode::Enter => self.activate_list_selection(),
            _ => {}
        }
        Ok(())
    }

    /// Rows: [Create new task] + active tasks. Index 0 = create.
    pub fn task_list_row_count(&self) -> usize {
        1 + self.active_tasks().count()
    }

    pub fn active_tasks(&self) -> impl Iterator<Item = (usize, &Task)> {
        self.tasks.iter().enumerate().filter(|(_, t)| !t.archived)
    }

    pub fn archived_tasks(&self) -> impl Iterator<Item = (usize, &Task)> {
        self.tasks.iter().enumerate().filter(|(_, t)| t.archived)
    }

    pub fn archived_count(&self) -> usize {
        self.tasks.iter().filter(|t| t.archived).count()
    }

    /// Map list UI index (1..) to `tasks` index. Index 0 is Create.
    fn tasks_index_for_list_row(&self, row: usize) -> Option<usize> {
        if row == 0 {
            return None;
        }
        self.active_tasks().nth(row - 1).map(|(i, _)| i)
    }

    pub fn selected_list_task(&self) -> Option<&Task> {
        let row = self.list_state.selected()?;
        let idx = self.tasks_index_for_list_row(row)?;
        self.tasks.get(idx)
    }

    fn select_next_list(&mut self) {
        let len = self.task_list_row_count();
        if len == 0 {
            return;
        }
        let i = self.list_state.selected().map_or(0, |i| (i + 1) % len);
        self.list_state.select(Some(i));
    }

    fn select_previous_list(&mut self) {
        let len = self.task_list_row_count();
        if len == 0 {
            return;
        }
        let i = self
            .list_state
            .selected()
            .map_or(0, |i| if i == 0 { len - 1 } else { i - 1 });
        self.list_state.select(Some(i));
    }

    fn activate_list_selection(&mut self) {
        match self.list_state.selected() {
            Some(0) => {
                self.create_input = text_input::single_line("");
                self.view = View::CreatePrompt;
                self.clear_status();
            }
            Some(row) => {
                if let Some(task_idx) = self.tasks_index_for_list_row(row)
                    && let Err(err) = self.start_switch(task_idx)
                {
                    self.set_error(format!("Switch failed: {err:#}"));
                }
            }
            None => {}
        }
    }

    fn open_archive_view(&mut self) {
        self.view = View::Archive;
        self.clear_status();
        let n = self.archived_count();
        if n == 0 {
            self.archive_state.select(None);
        } else if self.archive_state.selected().is_none_or(|i| i >= n) {
            self.archive_state.select(Some(0));
        }
    }

    fn open_edit_for_list_selection(&mut self) -> color_eyre::Result<()> {
        let Some(row) = self.list_state.selected() else {
            return Ok(());
        };
        if row == 0 {
            self.set_error("Select a task to edit");
            return Ok(());
        }
        let Some(task_idx) = self.tasks_index_for_list_row(row) else {
            return Ok(());
        };
        self.open_edit(task_idx, View::TaskList)
    }

    fn archive_selected(&mut self, terminal: &mut DefaultTerminal) -> color_eyre::Result<()> {
        let Some(row) = self.list_state.selected() else {
            return Ok(());
        };
        if row == 0 {
            self.set_error("Select a task to archive");
            return Ok(());
        }
        let Some(task_idx) = self.tasks_index_for_list_row(row) else {
            return Ok(());
        };

        if self.tasks[task_idx].worktree.is_some() {
            return self.begin_release(terminal, task_idx, true);
        }

        self.finish_archive(task_idx)
    }

    fn release_selected(&mut self, terminal: &mut DefaultTerminal) -> color_eyre::Result<()> {
        let Some(row) = self.list_state.selected() else {
            return Ok(());
        };
        if row == 0 {
            self.set_error("Select a task to release its worktree");
            return Ok(());
        }
        let Some(task_idx) = self.tasks_index_for_list_row(row) else {
            return Ok(());
        };

        if self.tasks[task_idx].worktree.is_none() {
            self.set_error("No worktree associated with this task");
            return Ok(());
        }

        self.begin_release(terminal, task_idx, false)
    }

    /// Start release: dirty-check first; show warning or proceed.
    fn begin_release(
        &mut self,
        terminal: &mut DefaultTerminal,
        task_idx: usize,
        then_archive: bool,
    ) -> color_eyre::Result<()> {
        let Some(path) = self
            .tasks
            .get(task_idx)
            .and_then(|t| t.worktree.as_ref())
            .map(|w| w.path.clone())
        else {
            self.set_error("No worktree to release");
            return Ok(());
        };

        // Directory already gone — clear the association without a dirty check.
        if !path.is_dir() {
            return self.complete_release(terminal, task_idx, then_archive);
        }

        let path_for_check = path.clone();
        let report =
            match self.run_busy(terminal, "Checking worktree for leftovers…", move || {
                dirty::inspect_worktree(&path_for_check)
            }) {
                Ok(report) => report,
                Err(err) => {
                    self.set_error(format!("Dirty check failed: {err:#}"));
                    return Ok(());
                }
            };

        if report.is_clean() {
            self.complete_release(terminal, task_idx, then_archive)
        } else {
            let actions = dirty::menu_actions(&report);
            self.release = Some(ReleaseState {
                task_idx,
                then_archive,
                report,
                actions,
                action_cursor: 0,
            });
            self.view = View::DirtyWarning;
            self.clear_status();
            Ok(())
        }
    }

    fn finish_archive(&mut self, task_idx: usize) -> color_eyre::Result<()> {
        let Some(task) = self.tasks.get_mut(task_idx) else {
            return Ok(());
        };
        task.archived = true;
        task.touch();
        persist::save_task(task)?;
        self.sort_tasks();
        self.clamp_list_selection();
        self.release = None;
        self.view = View::TaskList;
        self.set_status("Task archived");
        Ok(())
    }

    /// Return worktree to Treehouse, clear association, optionally archive.
    fn complete_release(
        &mut self,
        terminal: &mut DefaultTerminal,
        task_idx: usize,
        then_archive: bool,
    ) -> color_eyre::Result<()> {
        let (stem, path, wt_num) = {
            let Some(task) = self.tasks.get(task_idx) else {
                return Ok(());
            };
            let Some(wt) = task.worktree.as_ref() else {
                self.set_error("No worktree to release");
                return Ok(());
            };
            (task.file_stem.clone(), wt.path.clone(), wt.number)
        };

        // Leave any dirty-warning prompt; progress lives in the status bar.
        self.release = None;
        self.view = View::TaskList;

        let path_missing = !path.is_dir();
        if path_missing {
            // Directory already gone: best-effort return/forget, then clear association.
            let path_cleanup = path.clone();
            let _ = self.run_busy(
                terminal,
                format!("Worktree {wt_num} missing on disk; clearing association…"),
                move || {
                    switch::cleanup_missing_worktree(&path_cleanup);
                    Ok::<(), color_eyre::Report>(())
                },
            );
        } else if let Err(err) = self.run_busy(
            terminal,
            format!("Releasing worktree {wt_num}…"),
            move || treehouse::return_worktree(&path),
        ) {
            self.set_error(format!("Treehouse return failed: {err:#}"));
            return Ok(());
        }

        let task_idx = self
            .tasks
            .iter()
            .position(|t| t.file_stem == stem)
            .ok_or_else(|| color_eyre::eyre::eyre!("task disappeared during release"))?;

        {
            let task = &mut self.tasks[task_idx];
            task.worktree = None;
            task.touch();
            persist::save_task(task)?;
        }
        self.sort_tasks();

        let task_idx = self
            .tasks
            .iter()
            .position(|t| t.file_stem == stem)
            .ok_or_else(|| color_eyre::eyre::eyre!("task disappeared after release"))?;

        if then_archive {
            self.finish_archive(task_idx)?;
            self.set_status(if path_missing {
                format!("Cleared missing worktree {wt_num} and archived task")
            } else {
                format!("Released worktree {wt_num} and archived task")
            });
        } else {
            self.select_active_task_by_stem(&stem);
            self.set_status(if path_missing {
                format!("Cleared missing worktree {wt_num}")
            } else {
                format!("Released worktree {wt_num}")
            });
        }
        Ok(())
    }

    fn clamp_list_selection(&mut self) {
        let len = self.task_list_row_count();
        match self.list_state.selected() {
            Some(i) if i >= len => {
                if len == 0 {
                    self.list_state.select(None);
                } else {
                    self.list_state.select(Some(len - 1));
                }
            }
            None if len > 0 => self.list_state.select(Some(0)),
            _ => {}
        }
    }

    // --- Archive -----------------------------------------------------------

    fn handle_archive_key(&mut self, key: KeyEvent) -> color_eyre::Result<()> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
            KeyCode::Esc => {
                self.view = View::TaskList;
                self.clear_status();
            }
            KeyCode::Char('u') | KeyCode::Char('U') => self.unarchive_selected()?,
            KeyCode::Down | KeyCode::Char('j') => self.select_next_archive(),
            KeyCode::Up | KeyCode::Char('k') => self.select_previous_archive(),
            _ => {}
        }
        Ok(())
    }

    fn tasks_index_for_archive_row(&self, row: usize) -> Option<usize> {
        self.archived_tasks().nth(row).map(|(i, _)| i)
    }

    pub fn selected_archive_task(&self) -> Option<&Task> {
        let row = self.archive_state.selected()?;
        let idx = self.tasks_index_for_archive_row(row)?;
        self.tasks.get(idx)
    }

    fn select_next_archive(&mut self) {
        let len = self.archived_count();
        if len == 0 {
            return;
        }
        let i = self.archive_state.selected().map_or(0, |i| (i + 1) % len);
        self.archive_state.select(Some(i));
    }

    fn select_previous_archive(&mut self) {
        let len = self.archived_count();
        if len == 0 {
            return;
        }
        let i = self
            .archive_state
            .selected()
            .map_or(0, |i| if i == 0 { len - 1 } else { i - 1 });
        self.archive_state.select(Some(i));
    }

    fn unarchive_selected(&mut self) -> color_eyre::Result<()> {
        let Some(row) = self.archive_state.selected() else {
            self.set_error("No archived task selected");
            return Ok(());
        };
        let Some(task_idx) = self.tasks_index_for_archive_row(row) else {
            return Ok(());
        };

        self.tasks[task_idx].archived = false;
        self.tasks[task_idx].touch();
        persist::save_task(&self.tasks[task_idx])?;
        self.sort_tasks();
        self.clamp_archive_selection();
        self.set_status("Task unarchived");
        Ok(())
    }

    fn clamp_archive_selection(&mut self) {
        let len = self.archived_count();
        match self.archive_state.selected() {
            Some(i) if i >= len => {
                if len == 0 {
                    self.archive_state.select(None);
                } else {
                    self.archive_state.select(Some(len - 1));
                }
            }
            None if len > 0 => self.archive_state.select(Some(0)),
            _ => {}
        }
    }

    // --- Edit --------------------------------------------------------------

    fn open_edit(&mut self, task_idx: usize, return_to: View) -> color_eyre::Result<()> {
        let available_modules = match env::current_dir() {
            Ok(cwd) => match task::available_modules(&cwd) {
                Ok(mods) => mods,
                Err(err) => {
                    self.set_error(format!("Module discovery failed: {err:#}"));
                    Vec::new()
                }
            },
            Err(err) => {
                self.set_error(format!("Could not read cwd: {err}"));
                Vec::new()
            }
        };

        let (title_input, branch_input, issue_input) = {
            let task = &self.tasks[task_idx];
            (
                text_input::single_line(&task.title),
                text_input::single_line(task.branch.as_deref().unwrap_or("")),
                text_input::single_line(task.issue_id.as_deref().unwrap_or("")),
            )
        };

        self.edit = Some(EditState {
            task_idx,
            return_to,
            focus: EditFocus::Title,
            module_cursor: 0,
            available_modules,
            title_input,
            branch_input,
            issue_input,
        });
        self.view = View::Edit;
        self.clear_status();
        Ok(())
    }

    fn handle_edit_key(
        &mut self,
        terminal: &mut DefaultTerminal,
        key: KeyEvent,
    ) -> color_eyre::Result<()> {
        // Crossterm BackTab is not mapped by tui-textarea's Key enum.
        if matches!(key.code, KeyCode::BackTab) {
            self.edit_focus_prev();
            return Ok(());
        }

        let focus = self.edit.as_ref().map(|e| e.focus);
        let input = Input::from(key);

        // Keys that navigate the edit form (not passed into text fields).
        match (focus, &input) {
            (_, Input { key: Key::Esc, .. }) => {
                self.leave_edit();
                return Ok(());
            }
            (
                _,
                Input {
                    key: Key::Tab,
                    shift: false,
                    ..
                },
            ) => {
                self.edit_focus_next();
                return Ok(());
            }
            (
                _,
                Input {
                    key: Key::Tab,
                    shift: true,
                    ..
                },
            ) => {
                self.edit_focus_prev();
                return Ok(());
            }
            (Some(EditFocus::Modules), Input { key: Key::Down, .. })
            | (
                Some(EditFocus::Modules),
                Input {
                    key: Key::Char('j'),
                    ctrl: false,
                    alt: false,
                    ..
                },
            ) => {
                self.edit_module_move_or_leave(1);
                return Ok(());
            }
            (Some(EditFocus::Modules), Input { key: Key::Up, .. })
            | (
                Some(EditFocus::Modules),
                Input {
                    key: Key::Char('k'),
                    ctrl: false,
                    alt: false,
                    ..
                },
            ) => {
                self.edit_module_move_or_leave(-1);
                return Ok(());
            }
            (
                Some(EditFocus::Worktree),
                Input {
                    key: Key::Delete | Key::Backspace,
                    ..
                },
            )
            | (
                Some(EditFocus::Worktree),
                Input {
                    key: Key::Char('d' | 'D' | 'x' | 'X'),
                    ctrl: false,
                    alt: false,
                    ..
                },
            ) => {
                self.clear_edit_worktree_association(terminal)?;
                return Ok(());
            }
            // Up/Down move between fields even while a text input is focused.
            (_, Input { key: Key::Down, .. }) => {
                self.edit_focus_next();
                return Ok(());
            }
            (_, Input { key: Key::Up, .. }) => {
                self.edit_focus_prev();
                return Ok(());
            }
            (
                _,
                Input {
                    key: Key::Enter, ..
                },
            ) => {
                self.edit_confirm_field()?;
                return Ok(());
            }
            (
                Some(EditFocus::Modules),
                Input {
                    key: Key::Char(' '),
                    ctrl: false,
                    alt: false,
                    ..
                },
            ) => {
                self.toggle_selected_module()?;
                return Ok(());
            }
            (
                Some(EditFocus::Modules | EditFocus::Worktree),
                Input {
                    key: Key::Char('q' | 'Q'),
                    ctrl: false,
                    alt: false,
                    ..
                },
            ) => {
                self.should_quit = true;
                return Ok(());
            }
            (Some(EditFocus::Title | EditFocus::Branch | EditFocus::IssueId), _) => {
                // Fall through to textarea handling below.
            }
            (
                _,
                Input {
                    key: Key::Char('q' | 'Q'),
                    ctrl: false,
                    alt: false,
                    ..
                },
            ) => {
                self.should_quit = true;
                return Ok(());
            }
            _ => return Ok(()),
        }

        let Some(edit) = self.edit.as_mut() else {
            return Ok(());
        };
        let modified = match edit.focus {
            EditFocus::Title => edit.title_input.input(input),
            EditFocus::Branch => edit.branch_input.input(input),
            EditFocus::IssueId => edit.issue_input.input(input),
            EditFocus::Modules | EditFocus::Worktree => false,
        };
        if modified {
            self.sync_edit_inputs_to_task()?;
        }
        Ok(())
    }

    fn leave_edit(&mut self) {
        let return_to = self
            .edit
            .as_ref()
            .map(|e| e.return_to)
            .unwrap_or(View::TaskList);
        self.edit = None;
        self.view = return_to;
        self.clear_status();
    }

    fn edit_focus_next(&mut self) {
        let Some(edit) = self.edit.as_mut() else {
            return;
        };
        edit.focus = match edit.focus {
            EditFocus::Title => EditFocus::Branch,
            EditFocus::Branch => EditFocus::IssueId,
            EditFocus::IssueId => EditFocus::Modules,
            EditFocus::Modules => EditFocus::Worktree,
            EditFocus::Worktree => EditFocus::Title,
        };
        if edit.focus == EditFocus::Modules && !edit.available_modules.is_empty() {
            edit.module_cursor = edit
                .module_cursor
                .min(edit.available_modules.len().saturating_sub(1));
        }
    }

    fn edit_focus_prev(&mut self) {
        let Some(edit) = self.edit.as_mut() else {
            return;
        };
        let from_worktree = edit.focus == EditFocus::Worktree;
        edit.focus = match edit.focus {
            EditFocus::Title => EditFocus::Worktree,
            EditFocus::Branch => EditFocus::Title,
            EditFocus::IssueId => EditFocus::Branch,
            EditFocus::Modules => EditFocus::IssueId,
            EditFocus::Worktree => EditFocus::Modules,
        };
        if edit.focus == EditFocus::Modules && !edit.available_modules.is_empty() {
            if from_worktree {
                edit.module_cursor = edit.available_modules.len() - 1;
            } else {
                edit.module_cursor = edit
                    .module_cursor
                    .min(edit.available_modules.len().saturating_sub(1));
            }
        }
    }

    /// Move within the modules list; leave to adjacent fields at the edges.
    fn edit_module_move_or_leave(&mut self, delta: i32) {
        let Some(edit) = self.edit.as_ref() else {
            return;
        };
        let len = edit.available_modules.len();
        if len == 0 {
            if delta > 0 {
                self.edit_focus_next();
            } else {
                self.edit_focus_prev();
            }
            return;
        }
        let cur = edit.module_cursor;
        if delta > 0 && cur + 1 >= len {
            self.edit_focus_next();
            return;
        }
        if delta < 0 && cur == 0 {
            self.edit_focus_prev();
            return;
        }
        let Some(edit) = self.edit.as_mut() else {
            return;
        };
        edit.module_cursor = ((cur as i32) + delta) as usize;
    }

    fn edit_confirm_field(&mut self) -> color_eyre::Result<()> {
        let Some(edit) = self.edit.as_ref() else {
            return Ok(());
        };
        match edit.focus {
            EditFocus::Title | EditFocus::Branch | EditFocus::IssueId => {
                // Text already persisted on each keystroke; Enter advances focus.
                self.edit_focus_next();
            }
            EditFocus::Modules | EditFocus::Worktree => {
                // Enter on modules/worktree: no-op (Space toggles; D clears worktree).
            }
        }
        Ok(())
    }

    /// Drop the task's worktree association without returning it to Treehouse.
    ///
    /// Useful when the directory was deleted out from under the app and the
    /// stale path keeps blocking activate / lease flows.
    fn clear_edit_worktree_association(
        &mut self,
        terminal: &mut DefaultTerminal,
    ) -> color_eyre::Result<()> {
        let Some(edit) = self.edit.as_ref() else {
            return Ok(());
        };
        let task_idx = edit.task_idx;
        let Some(task) = self.tasks.get(task_idx) else {
            return Ok(());
        };
        let Some(wt) = task.worktree.clone() else {
            self.set_error("No worktree associated with this task");
            return Ok(());
        };

        let path_missing = !wt.path.is_dir();
        if path_missing {
            let path = wt.path.clone();
            let _ = self.run_busy(
                terminal,
                format!(
                    "Worktree {} missing on disk; clearing association…",
                    wt.number
                ),
                move || {
                    switch::cleanup_missing_worktree(&path);
                    Ok::<(), color_eyre::Report>(())
                },
            );
        }

        let Some(task) = self.tasks.get_mut(task_idx) else {
            return Ok(());
        };
        task.worktree = None;
        task.touch();
        persist::save_task(task)?;
        self.sort_tasks_preserving_edit(task_idx)?;
        self.set_status(format!(
            "Cleared worktree {} association{}",
            wt.number,
            if path_missing {
                " (path was missing)"
            } else {
                ""
            }
        ));
        Ok(())
    }

    /// Copy focused edit textareas into the underlying task and persist.
    fn sync_edit_inputs_to_task(&mut self) -> color_eyre::Result<()> {
        let (task_idx, title, branch, issue_id) = {
            let Some(edit) = self.edit.as_ref() else {
                return Ok(());
            };
            (
                edit.task_idx,
                text_input::value(&edit.title_input),
                text_input::value(&edit.branch_input),
                text_input::value(&edit.issue_input),
            )
        };
        let Some(task) = self.tasks.get_mut(task_idx) else {
            return Ok(());
        };
        task.title = title;
        task.branch = {
            let trimmed = branch.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(branch)
            }
        };
        task.issue_id = {
            let trimmed = issue_id.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(issue_id)
            }
        };
        task.touch();
        persist::save_task(task)?;
        self.sort_tasks_preserving_edit(task_idx)?;
        Ok(())
    }

    fn toggle_selected_module(&mut self) -> color_eyre::Result<()> {
        let (task_idx, module_name) = {
            let Some(edit) = self.edit.as_ref() else {
                return Ok(());
            };
            let Some(name) = edit.available_modules.get(edit.module_cursor) else {
                return Ok(());
            };
            (edit.task_idx, name.clone())
        };
        let Some(task) = self.tasks.get_mut(task_idx) else {
            return Ok(());
        };
        if let Some(pos) = task.modules.iter().position(|m| m == &module_name) {
            task.modules.remove(pos);
        } else {
            task.modules.push(module_name);
        }
        task.touch();
        persist::save_task(task)?;
        self.sort_tasks_preserving_edit(task_idx)?;
        Ok(())
    }

    /// Re-sort by last_used and keep `edit.task_idx` pointing at the same task.
    fn sort_tasks_preserving_edit(&mut self, old_idx: usize) -> color_eyre::Result<()> {
        let stem = self
            .tasks
            .get(old_idx)
            .map(|t| t.file_stem.clone())
            .ok_or_else(|| color_eyre::eyre::eyre!("edit task index out of range"))?;
        self.sort_tasks();
        if let Some(edit) = self.edit.as_mut() {
            edit.task_idx = self
                .tasks
                .iter()
                .position(|t| t.file_stem == stem)
                .ok_or_else(|| color_eyre::eyre::eyre!("edited task disappeared after sort"))?;
        }
        Ok(())
    }

    // --- Create prompt -----------------------------------------------------

    fn handle_create_prompt_key(&mut self, key: KeyEvent) -> color_eyre::Result<()> {
        let input = Input::from(key);
        match input {
            Input { key: Key::Esc, .. } => {
                self.create_input = text_input::single_line("");
                self.pending_create_input = None;
                self.view = View::TaskList;
                self.clear_status();
            }
            Input {
                key: Key::Enter, ..
            } => {
                let text = text_input::value(&self.create_input);
                self.submit_create(&text)?;
            }
            // Keep single-line: ignore bare Ctrl+M (treated as Enter by some terminals).
            Input {
                key: Key::Char('m'),
                ctrl: true,
                ..
            } => {}
            input => {
                self.create_input.input(input);
            }
        }
        Ok(())
    }

    fn handle_credential_prompt_key(&mut self, key: KeyEvent) -> color_eyre::Result<()> {
        let input = Input::from(key);
        match input {
            Input { key: Key::Esc, .. } => {
                self.credential_input = text_input::single_line_masked("");
                self.pending_create_input = None;
                self.credential_prompt_kind = CredentialPromptKind::Missing;
                self.create_input = text_input::single_line("");
                self.view = View::TaskList;
                self.set_status("Create cancelled (no Linear API key)");
            }
            Input {
                key: Key::Enter, ..
            } => {
                let key_text = text_input::value(&self.credential_input);
                let key_text = key_text.trim().to_string();
                if key_text.is_empty() {
                    self.set_error("Linear API key cannot be empty");
                    return Ok(());
                }
                let store_result = credentials::store_linear_api_key(&key_text);
                self.credential_input = text_input::single_line_masked("");
                let pending = self.pending_create_input.take();
                match pending {
                    Some(input) => {
                        self.create_input = text_input::single_line("");
                        // Use the key just entered; don't rely on an immediate reload.
                        self.submit_create_with_key(&input, Some(key_text.as_str()))?;
                        match store_result {
                            Ok(store) => {
                                // Prefer location message; keep create status if present by appending.
                                let where_msg = store.status_message();
                                if let Some(status) = &mut self.status {
                                    if !status.is_error {
                                        status.text = format!("{} — {}", status.text, where_msg);
                                    } else {
                                        status.text.push_str(&format!(" — {where_msg}"));
                                    }
                                } else {
                                    self.set_status(where_msg);
                                }
                            }
                            Err(err) => {
                                let suffix = format!(" (credential store failed: {err:#})");
                                if let Some(status) = &mut self.status {
                                    status.text.push_str(&suffix);
                                    status.is_error = true;
                                } else {
                                    self.set_error(format!("Linear API key not persisted{suffix}"));
                                }
                            }
                        }
                    }
                    None => {
                        self.view = View::TaskList;
                        match store_result {
                            Ok(store) => self.set_status(store.status_message()),
                            Err(err) => self.set_error(format!("Could not store API key: {err:#}")),
                        }
                    }
                }
            }
            Input {
                key: Key::Char('m'),
                ctrl: true,
                ..
            } => {}
            input => {
                self.credential_input.input(input);
            }
        }
        Ok(())
    }

    fn handle_credential_file_broken_key(&mut self, key: KeyEvent) -> color_eyre::Result<()> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                let resume = self.pending_create_input.clone().unwrap_or_default();
                match credentials::remove_linear_api_key_file() {
                    Ok(()) => {
                        self.credential_broken_path = None;
                        self.open_credential_prompt(&resume, CredentialPromptKind::Missing);
                        self.set_status("Old credential file removed — enter a new Linear API key");
                    }
                    Err(err) => {
                        self.set_error(format!("Could not remove credential file: {err:#}"));
                    }
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.credential_broken_path = None;
                if let Some(input) = self.pending_create_input.take() {
                    self.create_input = text_input::single_line(&input);
                    self.view = View::CreatePrompt;
                } else {
                    self.view = View::TaskList;
                }
                self.set_status("Left credential file unchanged");
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
            _ => {}
        }
        Ok(())
    }

    /// Parse input, optionally fetch Linear issue, persist task, return to list.
    fn submit_create(&mut self, input: &str) -> color_eyre::Result<()> {
        self.submit_create_with_key(input, None)
    }

    /// Like [`submit_create`], but may use a just-entered API key instead of the keyring.
    fn submit_create_with_key(
        &mut self,
        input: &str,
        api_key: Option<&str>,
    ) -> color_eyre::Result<()> {
        let parsed = match create::parse_create_input(input) {
            Ok(p) => p,
            Err(err) => {
                self.set_error(format!("Create failed: {err:#}"));
                // Stay on create prompt so the user can fix input.
                self.view = View::CreatePrompt;
                return Ok(());
            }
        };

        let (title, branch, issue_id) = match parsed {
            ParsedCreateInput::Title(title) => (title, None, None),
            ParsedCreateInput::Branch {
                name,
                issue_id: None,
            } => (name.clone(), Some(name), None),
            ParsedCreateInput::Branch {
                name,
                issue_id: Some(id),
            } => match self.resolve_linear_issue(&id, input, api_key)? {
                Some(issue) => (issue.title, Some(name), Some(issue.identifier)),
                None => {
                    // Credential prompt opened, or lookup failed (status already set).
                    return Ok(());
                }
            },
            ParsedCreateInput::IssueId(id) => {
                match self.resolve_linear_issue(&id, input, api_key)? {
                    Some(issue) => (issue.title, None, Some(issue.identifier)),
                    None => {
                        // Credential prompt opened, or lookup failed (status already set).
                        return Ok(());
                    }
                }
            }
        };

        self.finish_create_task(title, branch, issue_id)
    }

    /// Returns `None` when switching to the credential prompt, or when lookup fails.
    ///
    /// `resume_input` is the original create-prompt text to re-submit after credentials.
    fn resolve_linear_issue(
        &mut self,
        issue_id: &str,
        resume_input: &str,
        api_key_override: Option<&str>,
    ) -> color_eyre::Result<Option<linear::LinearIssue>> {
        let api_key = if let Some(key) = api_key_override {
            key.to_string()
        } else {
            match credentials::load_linear_api_key() {
                Ok(credentials::LoadLinearApiKey::Found(key)) => key,
                Ok(credentials::LoadLinearApiKey::Missing) => {
                    self.open_credential_prompt(resume_input, CredentialPromptKind::Missing);
                    return Ok(None);
                }
                Ok(credentials::LoadLinearApiKey::UnreadableFile { path }) => {
                    self.open_credential_file_broken(resume_input, path);
                    return Ok(None);
                }
                Err(err) => {
                    // Errors from credentials already name the failing store (keyring vs file).
                    self.set_error(format!("{err:#}"));
                    self.view = View::CreatePrompt;
                    return Ok(None);
                }
            }
        };

        match linear::fetch_issue_by_identifier(&api_key, issue_id) {
            Ok(issue) => Ok(Some(issue)),
            Err(linear::IssueLookupError::Unauthorized) => {
                self.open_credential_prompt(resume_input, CredentialPromptKind::Invalid);
                Ok(None)
            }
            Err(linear::IssueLookupError::Other(err)) => {
                self.set_error(format!("Linear lookup failed: {err:#}"));
                self.view = View::CreatePrompt;
                Ok(None)
            }
        }
    }

    fn open_credential_prompt(&mut self, resume_input: &str, kind: CredentialPromptKind) {
        self.pending_create_input = Some(resume_input.to_string());
        self.credential_input = text_input::single_line_masked("");
        self.credential_prompt_kind = kind;
        self.credential_broken_path = None;
        self.view = View::CredentialPrompt;
        match kind {
            CredentialPromptKind::Missing => {
                self.set_status(
                    "Enter your Linear API key (OS keyring, or encrypted config file if unavailable)",
                );
            }
            CredentialPromptKind::Invalid => {
                self.set_error("Previous Linear API key looks invalid — enter a new one");
            }
        }
    }

    fn open_credential_file_broken(&mut self, resume_input: &str, path: PathBuf) {
        self.pending_create_input = Some(resume_input.to_string());
        self.credential_broken_path = Some(path);
        self.view = View::CredentialFileBroken;
        self.set_error(
            "Credential file cannot be decrypted (wrong machine or corrupt) — recreate?",
        );
    }

    fn finish_create_task(
        &mut self,
        title: String,
        branch: Option<String>,
        issue_id: Option<String>,
    ) -> color_eyre::Result<()> {
        let stem = persist::allocate_file_stem(&title)?;
        let mut task = Task::new(title.clone(), stem);
        task.branch = branch;
        task.issue_id = issue_id;
        task.touch();
        persist::save_task(&task)?;

        let stem = task.file_stem.clone();
        self.tasks.push(task);
        self.sort_tasks();
        self.select_active_task_by_stem(&stem);

        self.create_input = text_input::single_line("");
        self.pending_create_input = None;
        self.credential_prompt_kind = CredentialPromptKind::Missing;
        self.view = View::TaskList;
        self.set_status(format!("Created task: {title}"));
        Ok(())
    }

    fn select_active_task_by_stem(&mut self, stem: &str) {
        let row = self
            .active_tasks()
            .enumerate()
            .find(|(_, (_, task))| task.file_stem == stem)
            .map(|(row_offset, _)| row_offset + 1); // +1: row 0 is Create
        if let Some(row) = row {
            self.list_state.select(Some(row));
        }
    }

    // --- Switch --------------------------------------------------------------

    /// Enter on a task: ensure worktree (prompt / lease if needed), activate, open Cursor.
    fn start_switch(&mut self, task_idx: usize) -> color_eyre::Result<()> {
        self.switch_prep = None;
        self.clear_status();

        if self.tasks.get(task_idx).is_none() {
            self.set_error("Task missing");
            return Ok(());
        }

        if self.tasks[task_idx].worktree.is_some() {
            self.queue_finish_switch(task_idx);
            return Ok(());
        }

        // No worktree yet: ensure modules + branch, then lease.
        if self.tasks[task_idx].modules.is_empty() {
            return self.open_switch_modules_prompt(task_idx);
        }
        if self.tasks[task_idx]
            .branch
            .as_ref()
            .map(|b| b.trim().is_empty())
            .unwrap_or(true)
        {
            return self.open_switch_branch_prompt(task_idx);
        }

        self.queue_finish_switch(task_idx);
        Ok(())
    }

    /// Schedule lease/activate/cursor after the next redraw (keeps the status bar live).
    fn queue_finish_switch(&mut self, task_idx: usize) {
        self.set_busy("Switching to task…");
        self.pending_finish_switch = Some(task_idx);
    }

    fn open_switch_modules_prompt(&mut self, task_idx: usize) -> color_eyre::Result<()> {
        let available_modules = self.discover_modules_or_status();
        self.switch_prep = Some(SwitchPrepState {
            task_idx,
            module_cursor: 0,
            available_modules,
            branch_input: text_input::single_line(""),
        });
        self.view = View::SwitchModules;
        self.set_status("Select modules for this task (Space toggle, Enter confirm)");
        Ok(())
    }

    fn open_switch_branch_prompt(&mut self, task_idx: usize) -> color_eyre::Result<()> {
        let available_modules = self
            .switch_prep
            .as_ref()
            .map(|s| s.available_modules.clone())
            .unwrap_or_else(|| self.discover_modules_or_status());
        self.switch_prep = Some(SwitchPrepState {
            task_idx,
            module_cursor: 0,
            available_modules,
            branch_input: text_input::single_line(
                self.tasks[task_idx].branch.as_deref().unwrap_or(""),
            ),
        });
        self.view = View::SwitchBranch;
        self.set_status("Enter a branch name for this task");
        Ok(())
    }

    fn discover_modules_or_status(&mut self) -> Vec<String> {
        match env::current_dir() {
            Ok(cwd) => match task::available_modules(&cwd) {
                Ok(mods) => mods,
                Err(err) => {
                    self.set_error(format!("Module discovery failed: {err:#}"));
                    Vec::new()
                }
            },
            Err(err) => {
                self.set_error(format!("Could not read cwd: {err}"));
                Vec::new()
            }
        }
    }

    fn handle_switch_modules_key(&mut self, key: KeyEvent) -> color_eyre::Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.switch_prep = None;
                self.view = View::TaskList;
                self.set_status("Switch cancelled");
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.switch_module_move(1),
            KeyCode::Up | KeyCode::Char('k') => self.switch_module_move(-1),
            KeyCode::Char(' ') => self.switch_toggle_module()?,
            KeyCode::Enter => self.confirm_switch_modules()?,
            _ => {}
        }
        Ok(())
    }

    fn handle_switch_branch_key(&mut self, key: KeyEvent) -> color_eyre::Result<()> {
        let input = Input::from(key);
        match input {
            Input { key: Key::Esc, .. } => {
                self.switch_prep = None;
                self.view = View::TaskList;
                self.set_status("Switch cancelled");
            }
            Input {
                key: Key::Enter, ..
            } => self.confirm_switch_branch()?,
            Input {
                key: Key::Char('m'),
                ctrl: true,
                ..
            } => {}
            input => {
                if let Some(prep) = self.switch_prep.as_mut() {
                    prep.branch_input.input(input);
                }
            }
        }
        Ok(())
    }

    fn switch_module_move(&mut self, delta: i32) {
        let Some(prep) = self.switch_prep.as_mut() else {
            return;
        };
        let len = prep.available_modules.len();
        if len == 0 {
            return;
        }
        let cur = prep.module_cursor as i32;
        prep.module_cursor = (cur + delta).rem_euclid(len as i32) as usize;
    }

    fn switch_toggle_module(&mut self) -> color_eyre::Result<()> {
        let (task_idx, module_name) = {
            let Some(prep) = self.switch_prep.as_ref() else {
                return Ok(());
            };
            let Some(name) = prep.available_modules.get(prep.module_cursor) else {
                return Ok(());
            };
            (prep.task_idx, name.clone())
        };
        let Some(task) = self.tasks.get_mut(task_idx) else {
            return Ok(());
        };
        if let Some(pos) = task.modules.iter().position(|m| m == &module_name) {
            task.modules.remove(pos);
        } else {
            task.modules.push(module_name);
        }
        task.touch();
        persist::save_task(task)?;
        self.sort_tasks_preserving_switch(task_idx)?;
        Ok(())
    }

    fn confirm_switch_modules(&mut self) -> color_eyre::Result<()> {
        let Some(task_idx) = self.switch_prep.as_ref().map(|p| p.task_idx) else {
            return Ok(());
        };
        if self.tasks[task_idx].modules.is_empty() {
            self.set_error("Select at least one module (Space), then Enter");
            return Ok(());
        }
        // Persist already happened on toggle; continue to branch or finish.
        if self.tasks[task_idx]
            .branch
            .as_ref()
            .map(|b| b.trim().is_empty())
            .unwrap_or(true)
        {
            return self.open_switch_branch_prompt(task_idx);
        }
        self.queue_finish_switch(task_idx);
        Ok(())
    }

    fn confirm_switch_branch(&mut self) -> color_eyre::Result<()> {
        let Some(prep) = self.switch_prep.as_ref() else {
            return Ok(());
        };
        let task_idx = prep.task_idx;
        let branch = text_input::value(&prep.branch_input);
        let branch = branch.trim().to_string();
        if branch.is_empty() {
            self.set_error("Branch name cannot be empty");
            return Ok(());
        }
        {
            let Some(task) = self.tasks.get_mut(task_idx) else {
                return Ok(());
            };
            task.branch = Some(branch);
            task.touch();
            persist::save_task(task)?;
        }
        self.sort_tasks_preserving_switch(task_idx)?;
        let task_idx = self
            .switch_prep
            .as_ref()
            .map(|p| p.task_idx)
            .unwrap_or(task_idx);
        self.queue_finish_switch(task_idx);
        Ok(())
    }

    /// Re-sort and keep `switch_prep.task_idx` pointing at the same task.
    fn sort_tasks_preserving_switch(&mut self, old_idx: usize) -> color_eyre::Result<()> {
        let stem = self
            .tasks
            .get(old_idx)
            .map(|t| t.file_stem.clone())
            .ok_or_else(|| color_eyre::eyre::eyre!("switch task index out of range"))?;
        self.sort_tasks();
        if let Some(prep) = self.switch_prep.as_mut() {
            prep.task_idx = self
                .tasks
                .iter()
                .position(|t| t.file_stem == stem)
                .ok_or_else(|| color_eyre::eyre::eyre!("switch task disappeared after sort"))?;
        }
        Ok(())
    }

    /// Lease if needed, activate branches, launch Cursor, touch + persist.
    fn finish_switch(
        &mut self,
        terminal: &mut DefaultTerminal,
        task_idx: usize,
    ) -> color_eyre::Result<()> {
        // Leave any switch prompts; progress lives in the status bar.
        self.switch_prep = None;
        self.view = View::TaskList;

        let stem = self
            .tasks
            .get(task_idx)
            .map(|t| t.file_stem.clone())
            .ok_or_else(|| color_eyre::eyre::eyre!("switch task missing"))?;

        // Stale association: directory gone — clear and lease a fresh worktree.
        if let Some(wt) = self.tasks[task_idx].worktree.clone() {
            if !wt.path.is_dir() {
                self.clear_missing_worktree_association(terminal, &stem, &wt)?;
            }
        }

        let task_idx = self
            .tasks
            .iter()
            .position(|t| t.file_stem == stem)
            .ok_or_else(|| color_eyre::eyre::eyre!("task disappeared after clearing worktree"))?;

        // Lease when no worktree yet.
        if self.tasks[task_idx].worktree.is_none() {
            let cwd = env::current_dir().wrap_err("reading cwd for treehouse lease")?;
            match self.run_busy(
                terminal,
                "New worktree: leasing from Treehouse…",
                move || switch::lease_new_worktree(&cwd),
            ) {
                Ok(wt) => {
                    let task = &mut self.tasks[task_idx];
                    task.worktree = Some(wt);
                    task.touch();
                    persist::save_task(task)?;
                }
                Err(err) => {
                    if let Some(conflict) = treehouse::parse_lease_path_conflict(&err) {
                        if conflict.kind == treehouse::LeasePathConflictKind::AlreadyExists
                            && self.lease_auto_clear_already_exists
                        {
                            self.lease_auto_clear_already_exists = false;
                            match self.auto_clear_lease_path_and_retry(
                                terminal,
                                &stem,
                                conflict.path.clone(),
                            ) {
                                Ok(()) => return Ok(()),
                                Err(clear_err) => {
                                    // Fall through to the normal recovery UI.
                                    self.set_status(format!(
                                        "Auto-clear after prune failed ({clear_err:#}); choose a fix"
                                    ));
                                }
                            }
                        }
                        match self.open_stale_worktree_recovery(&stem, conflict.path, conflict.kind)
                        {
                            Ok(()) => return Ok(()),
                            Err(open_err) => {
                                self.set_error(format!(
                                    "Treehouse lease failed (path conflict; could not open recovery): \
                                     {err:#} — {open_err:#}"
                                ));
                                return Ok(());
                            }
                        }
                    }
                    self.lease_auto_clear_already_exists = false;
                    self.switch_prep = None;
                    self.view = View::TaskList;
                    self.set_error(format!("Treehouse lease failed: {err:#}"));
                    return Ok(());
                }
            }
            self.lease_auto_clear_already_exists = false;
            self.sort_tasks();
        }

        let task_idx = self
            .tasks
            .iter()
            .position(|t| t.file_stem == stem)
            .ok_or_else(|| color_eyre::eyre::eyre!("task disappeared after lease"))?;

        let (worktree, modules, branch) = {
            let task = &self.tasks[task_idx];
            let worktree = task
                .worktree
                .clone()
                .ok_or_else(|| color_eyre::eyre::eyre!("worktree missing after lease"))?;
            let branch = task
                .branch
                .clone()
                .filter(|b| !b.trim().is_empty())
                .ok_or_else(|| color_eyre::eyre::eyre!("branch missing before activate"))?;
            (worktree, task.modules.clone(), branch)
        };

        if let Err(err) = switch::activate_worktree(&worktree, &modules, &branch, |step| {
            self.report_progress(terminal, step)
        }) {
            match err {
                switch::ActivateError::PathMissing { worktree: missing } => {
                    // Race / path vanished between lease check and activate.
                    self.clear_missing_worktree_association(terminal, &stem, &missing)?;
                    self.set_busy("Worktree missing; leasing a new one…");
                    self.pending_finish_switch =
                        self.tasks.iter().position(|t| t.file_stem == stem);
                    return Ok(());
                }
                switch::ActivateError::BranchLocked(locked) => {
                    self.open_branch_locked_recovery(&stem, locked);
                    return Ok(());
                }
                switch::ActivateError::Other(err) => {
                    self.switch_prep = None;
                    self.view = View::TaskList;
                    self.set_error(format!("Activate worktree failed: {err:#}"));
                    return Ok(());
                }
            }
        }

        self.report_progress(terminal, "Opening Cursor…")?;
        if let Err(err) = switch::launch_cursor(&worktree) {
            self.switch_prep = None;
            self.view = View::TaskList;
            self.set_error(format!("Opened worktree but Cursor launch failed: {err:#}"));
            // Still touch — switch mostly succeeded.
            self.tasks[task_idx].touch();
            persist::save_task(&self.tasks[task_idx])?;
            self.sort_tasks();
            self.select_active_task_by_stem(&stem);
            return Ok(());
        }

        {
            let task = &mut self.tasks[task_idx];
            task.touch();
            persist::save_task(task)?;
        }
        self.sort_tasks();
        self.select_active_task_by_stem(&stem);

        self.switch_prep = None;
        self.view = View::TaskList;
        let wt_num = self
            .tasks
            .iter()
            .find(|t| t.file_stem == stem)
            .and_then(|t| t.worktree.as_ref())
            .map(|w| w.number);
        let msg = match wt_num {
            Some(n) => format!("Switched to task (worktree {n}); opened Cursor"),
            None => "Switched to task; opened Cursor".to_string(),
        };
        self.set_status(msg);
        Ok(())
    }

    /// Clear a task's worktree association when its directory is gone.
    fn clear_missing_worktree_association(
        &mut self,
        terminal: &mut DefaultTerminal,
        stem: &str,
        worktree: &crate::task::Worktree,
    ) -> color_eyre::Result<()> {
        let path = worktree.path.clone();
        let wt_num = worktree.number;
        let _ = self.run_busy(
            terminal,
            format!("Worktree {wt_num} missing on disk; clearing association…"),
            move || {
                switch::cleanup_missing_worktree(&path);
                Ok::<(), color_eyre::Report>(())
            },
        );

        let task_idx = self
            .tasks
            .iter()
            .position(|t| t.file_stem == stem)
            .ok_or_else(|| color_eyre::eyre::eyre!("task disappeared while clearing worktree"))?;
        {
            let task = &mut self.tasks[task_idx];
            task.worktree = None;
            task.touch();
            persist::save_task(task)?;
        }
        self.sort_tasks();
        Ok(())
    }

    // --- Worktree path-conflict recovery -----------------------------------

    fn open_stale_worktree_recovery(
        &mut self,
        task_stem: &str,
        problem_path: PathBuf,
        kind: treehouse::LeasePathConflictKind,
    ) -> color_eyre::Result<()> {
        let cwd = env::current_dir().wrap_err("reading cwd for worktree path recovery")?;
        let main_root = gitutil::repo_toplevel(&cwd)?;
        // Label which submodule triggered the error (if any); recovery actions
        // always apply to main + every submodule.
        let owner = treehouse::registration_repo_for_conflict(&main_root, &problem_path)?;
        self.stale_worktree = Some(StaleWorktreeState {
            task_stem: task_stem.to_string(),
            problem_path,
            repo_root: main_root,
            submodule_rel: owner.submodule_rel,
            kind,
            action_cursor: 0,
        });
        self.view = View::StaleWorktree;
        self.clear_status();
        Ok(())
    }

    fn stale_actions(&self) -> &'static [StaleWorktreeAction] {
        self.stale_worktree
            .as_ref()
            .map(|s| StaleWorktreeAction::for_kind(s.kind))
            .unwrap_or(&[])
    }

    fn handle_stale_worktree_key(&mut self, key: KeyEvent) -> color_eyre::Result<()> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
            KeyCode::Esc => self.cancel_stale_worktree_recovery(),
            KeyCode::Down | KeyCode::Char('j') => self.move_stale_worktree_action(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_stale_worktree_action(-1),
            KeyCode::Enter => self.confirm_stale_worktree_action()?,
            KeyCode::Char(c) => {
                let lower = c.to_ascii_lowercase();
                let actions = self.stale_actions();
                if let Some(idx) = actions.iter().position(|a| a.shortcut() == lower) {
                    if let Some(stale) = self.stale_worktree.as_mut() {
                        stale.action_cursor = idx;
                    }
                    self.confirm_stale_worktree_action()?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn move_stale_worktree_action(&mut self, delta: i32) {
        let len = self.stale_actions().len() as i32;
        if len == 0 {
            return;
        }
        let Some(stale) = self.stale_worktree.as_mut() else {
            return;
        };
        let cur = stale.action_cursor as i32;
        stale.action_cursor = (cur + delta).rem_euclid(len) as usize;
    }

    fn confirm_stale_worktree_action(&mut self) -> color_eyre::Result<()> {
        let (action, task_stem, problem_path, main_root) = {
            let Some(stale) = self.stale_worktree.as_ref() else {
                return Ok(());
            };
            let actions = StaleWorktreeAction::for_kind(stale.kind);
            let Some(action) = actions.get(stale.action_cursor).copied() else {
                return Ok(());
            };
            (
                action,
                stale.task_stem.clone(),
                stale.problem_path.clone(),
                stale.repo_root.clone(),
            )
        };

        match action {
            StaleWorktreeAction::Cancel => {
                self.cancel_stale_worktree_recovery();
            }
            StaleWorktreeAction::Override | StaleWorktreeAction::Remove => {
                match gitutil::worktree_remove_slot_everywhere(&main_root, &problem_path) {
                    Ok(()) => {
                        if let Err(err) =
                            gitutil::clear_leftover_pool_dirs_after_registration_fix(&problem_path)
                        {
                            self.set_error(format!(
                                "Cleared registrations but could not delete leftover pool dirs \
                                 at {}: {err:#}",
                                problem_path.display()
                            ));
                            return Ok(());
                        }
                        self.stale_worktree = None;
                        self.lease_auto_clear_already_exists = true;
                        self.resume_switch_after_stale_fix(&task_stem, action)?;
                    }
                    Err(err) => {
                        self.set_error(format!(
                            "Could not clear worktree registrations for {}: {err:#}",
                            problem_path.display()
                        ));
                    }
                }
            }
            StaleWorktreeAction::Prune => match gitutil::worktree_prune_all(&main_root) {
                Ok(()) => {
                    if let Err(err) =
                        gitutil::clear_leftover_pool_dirs_after_registration_fix(&problem_path)
                    {
                        self.set_error(format!(
                            "Pruned registrations but could not delete leftover pool dirs \
                             at {}: {err:#}",
                            problem_path.display()
                        ));
                        return Ok(());
                    }
                    self.stale_worktree = None;
                    self.lease_auto_clear_already_exists = true;
                    self.resume_switch_after_stale_fix(&task_stem, action)?;
                }
                Err(err) => {
                    self.set_error(format!("git worktree prune failed: {err:#}"));
                }
            },
            StaleWorktreeAction::ClearPath => {
                match gitutil::clear_worktree_slot_everywhere(&main_root, &problem_path) {
                    Ok(()) => {
                        self.stale_worktree = None;
                        self.lease_auto_clear_already_exists = false;
                        self.resume_switch_after_stale_fix(&task_stem, action)?;
                    }
                    Err(err) => {
                        self.set_error(format!(
                            "Could not clear path {}: {err:#}",
                            problem_path.display()
                        ));
                    }
                }
            }
            StaleWorktreeAction::DeleteDirectory => {
                match gitutil::clear_leftover_pool_dirs_after_registration_fix(&problem_path) {
                    Ok(()) => {
                        self.stale_worktree = None;
                        self.lease_auto_clear_already_exists = false;
                        self.resume_switch_after_stale_fix(&task_stem, action)?;
                    }
                    Err(err) => {
                        self.set_error(format!(
                            "Could not delete {}: {err:#}",
                            problem_path.display()
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// After a prune/override left a leftover pool dir, clear it and retry the lease once.
    fn auto_clear_lease_path_and_retry(
        &mut self,
        terminal: &mut DefaultTerminal,
        task_stem: &str,
        problem_path: PathBuf,
    ) -> color_eyre::Result<()> {
        let cwd = env::current_dir().wrap_err("reading cwd for auto-clear after prune")?;
        let main_root = gitutil::repo_toplevel(&cwd)?;
        let path_for_clear = problem_path.clone();
        self.run_busy(
            terminal,
            format!(
                "Partial lease leftover at {}; clearing before retry…",
                problem_path.display()
            ),
            move || {
                gitutil::clear_worktree_slot_everywhere(&main_root, &path_for_clear)?;
                Ok(())
            },
        )?;

        let Some(task_idx) = self.tasks.iter().position(|t| t.file_stem == task_stem) else {
            self.view = View::TaskList;
            self.set_error("Task disappeared while auto-clearing leftover worktree path");
            return Ok(());
        };
        self.set_busy("Cleared leftover worktree path; retrying lease…");
        self.view = View::TaskList;
        self.pending_finish_switch = Some(task_idx);
        Ok(())
    }

    fn resume_switch_after_stale_fix(
        &mut self,
        task_stem: &str,
        action: StaleWorktreeAction,
    ) -> color_eyre::Result<()> {
        let Some(task_idx) = self.tasks.iter().position(|t| t.file_stem == task_stem) else {
            self.view = View::TaskList;
            self.set_error("Task disappeared while recovering worktree path conflict");
            return Ok(());
        };
        let label = match action {
            StaleWorktreeAction::Override => {
                "Cleared stale registrations (main + all submodules, override)".to_string()
            }
            StaleWorktreeAction::Prune => {
                "Pruned missing worktree registrations (main + all submodules)".to_string()
            }
            StaleWorktreeAction::Remove => {
                "Removed worktree registrations (main + all submodules)".to_string()
            }
            StaleWorktreeAction::ClearPath => {
                "Cleared blocking worktree path (main + all submodules)".to_string()
            }
            StaleWorktreeAction::DeleteDirectory => {
                "Deleted leftover worktree directory".to_string()
            }
            StaleWorktreeAction::Cancel => unreachable!(),
        };
        self.set_busy(format!("{label}; retrying lease…"));
        self.view = View::TaskList;
        self.pending_finish_switch = Some(task_idx);
        Ok(())
    }

    fn cancel_stale_worktree_recovery(&mut self) {
        self.stale_worktree = None;
        self.lease_auto_clear_already_exists = false;
        self.view = View::TaskList;
        self.set_status("Switch cancelled (worktree path left unchanged)");
    }

    // --- Branch locked by another worktree ---------------------------------

    fn open_branch_locked_recovery(&mut self, task_stem: &str, locked: switch::BranchLockedError) {
        self.branch_locked = Some(BranchLockedState {
            task_stem: task_stem.to_string(),
            branch: locked.branch,
            conflicting_path: locked.conflicting_path,
            checkout_repo: locked.checkout_repo,
            current_worktree: locked.current_worktree,
            other_worktree: locked.other_worktree,
            action_cursor: 0,
        });
        self.view = View::BranchLocked;
        self.clear_status();
    }

    fn branch_locked_actions(&self) -> Vec<BranchLockedAction> {
        self.branch_locked
            .as_ref()
            .map(|s| BranchLockedAction::available(s.other_worktree.is_some()))
            .unwrap_or_default()
    }

    fn handle_branch_locked_key(&mut self, key: KeyEvent) -> color_eyre::Result<()> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
            KeyCode::Esc => self.cancel_branch_locked_recovery(),
            KeyCode::Down | KeyCode::Char('j') => self.move_branch_locked_action(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_branch_locked_action(-1),
            KeyCode::Enter => self.confirm_branch_locked_action()?,
            KeyCode::Char(c) => {
                let lower = c.to_ascii_lowercase();
                let actions = self.branch_locked_actions();
                if let Some(idx) = actions.iter().position(|a| a.shortcut() == lower) {
                    if let Some(state) = self.branch_locked.as_mut() {
                        state.action_cursor = idx;
                    }
                    self.confirm_branch_locked_action()?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn move_branch_locked_action(&mut self, delta: i32) {
        let len = self.branch_locked_actions().len() as i32;
        if len == 0 {
            return;
        }
        let Some(state) = self.branch_locked.as_mut() else {
            return;
        };
        let cur = state.action_cursor as i32;
        state.action_cursor = (cur + delta).rem_euclid(len) as usize;
    }

    fn confirm_branch_locked_action(&mut self) -> color_eyre::Result<()> {
        let (action, task_stem, checkout_repo, conflicting_path, current, other) = {
            let Some(state) = self.branch_locked.as_ref() else {
                return Ok(());
            };
            let actions = BranchLockedAction::available(state.other_worktree.is_some());
            let Some(action) = actions.get(state.action_cursor).copied() else {
                return Ok(());
            };
            (
                action,
                state.task_stem.clone(),
                state.checkout_repo.clone(),
                state.conflicting_path.clone(),
                state.current_worktree.clone(),
                state.other_worktree.clone(),
            )
        };

        match action {
            BranchLockedAction::Cancel => self.cancel_branch_locked_recovery(),
            BranchLockedAction::AssociateOther => {
                let Some(other) = other else {
                    self.set_error(
                        "Could not derive the other Treehouse worktree from the conflicting path",
                    );
                    return Ok(());
                };
                if other.number != current.number {
                    // Best-effort return of the unused / wrong lease.
                    if let Err(err) = treehouse::return_worktree(&current.path) {
                        self.set_status(format!(
                            "Associated other worktree; return of previous lease failed: {err:#}"
                        ));
                    }
                }
                let Some(task_idx) = self.tasks.iter().position(|t| t.file_stem == task_stem)
                else {
                    self.branch_locked = None;
                    self.view = View::TaskList;
                    self.set_error("Task disappeared while recovering branch lock");
                    return Ok(());
                };
                {
                    let task = &mut self.tasks[task_idx];
                    task.worktree = Some(other.clone());
                    task.touch();
                    persist::save_task(task)?;
                }
                self.sort_tasks();
                let task_idx = self
                    .tasks
                    .iter()
                    .position(|t| t.file_stem == task_stem)
                    .ok_or_else(|| color_eyre::eyre::eyre!("task disappeared after associate"))?;
                self.branch_locked = None;
                self.set_busy(format!(
                    "Associated worktree {}; continuing activate…",
                    other.number
                ));
                self.view = View::TaskList;
                self.pending_finish_switch = Some(task_idx);
            }
            BranchLockedAction::RemoveOther => {
                match gitutil::forget_worktree_registration(&checkout_repo, &conflicting_path) {
                    Ok(()) => {
                        self.branch_locked = None;
                        let Some(task_idx) =
                            self.tasks.iter().position(|t| t.file_stem == task_stem)
                        else {
                            self.view = View::TaskList;
                            self.set_error("Task disappeared while recovering branch lock");
                            return Ok(());
                        };
                        self.set_busy("Removed other worktree registration; retrying activate…");
                        self.view = View::TaskList;
                        self.pending_finish_switch = Some(task_idx);
                    }
                    Err(err) => {
                        self.set_error(format!(
                            "Could not remove conflicting worktree {}: {err:#}",
                            conflicting_path.display()
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn cancel_branch_locked_recovery(&mut self) {
        self.branch_locked = None;
        self.view = View::TaskList;
        self.set_status("Switch cancelled (branch still locked by other worktree)");
    }

    // --- Release / dirty warning -------------------------------------------

    fn handle_dirty_warning_key(
        &mut self,
        terminal: &mut DefaultTerminal,
        key: KeyEvent,
    ) -> color_eyre::Result<()> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
            KeyCode::Esc => self.cancel_release(),
            KeyCode::Down | KeyCode::Char('j') => self.move_dirty_action(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_dirty_action(-1),
            KeyCode::Enter => self.confirm_dirty_action(terminal)?,
            KeyCode::Char(c) => {
                let lower = c.to_ascii_lowercase();
                if let Some(rel) = self.release.as_ref()
                    && let Some(idx) = rel.actions.iter().position(|a| a.shortcut() == lower)
                {
                    if let Some(rel) = self.release.as_mut() {
                        rel.action_cursor = idx;
                    }
                    self.confirm_dirty_action(terminal)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn move_dirty_action(&mut self, delta: i32) {
        let Some(rel) = self.release.as_mut() else {
            return;
        };
        let len = rel.actions.len();
        if len == 0 {
            return;
        }
        let cur = rel.action_cursor as i32;
        rel.action_cursor = (cur + delta).rem_euclid(len as i32) as usize;
    }

    fn confirm_dirty_action(&mut self, terminal: &mut DefaultTerminal) -> color_eyre::Result<()> {
        let (action, task_idx, then_archive, path) = {
            let Some(rel) = self.release.as_ref() else {
                return Ok(());
            };
            let Some(action) = rel.actions.get(rel.action_cursor).copied() else {
                return Ok(());
            };
            let path = self
                .tasks
                .get(rel.task_idx)
                .and_then(|t| t.worktree.as_ref())
                .map(|w| w.path.clone());
            (action, rel.task_idx, rel.then_archive, path)
        };

        match action {
            DirtyAction::Cancel => {
                self.cancel_release();
            }
            DirtyAction::CheckAgain => {
                let Some(path) = path else {
                    self.cancel_release();
                    self.set_error("Worktree association missing; release cancelled");
                    return Ok(());
                };
                let path_for_check = path.clone();
                let report =
                    match self.run_busy(terminal, "Checking worktree for leftovers…", move || {
                        dirty::inspect_worktree(&path_for_check)
                    }) {
                        Ok(report) => report,
                        Err(err) => {
                            self.set_error(format!("Dirty check failed: {err:#}"));
                            return Ok(());
                        }
                    };
                if report.is_clean() {
                    self.complete_release(terminal, task_idx, then_archive)?;
                } else {
                    let actions = dirty::menu_actions(&report);
                    if let Some(rel) = self.release.as_mut() {
                        rel.report = report;
                        rel.actions = actions;
                        rel.action_cursor = 0;
                    }
                    self.set_error("Still dirty — fix leftovers or stash, then check again");
                }
            }
            DirtyAction::StashChanges => {
                let Some(path) = path else {
                    self.cancel_release();
                    self.set_error("Worktree association missing; release cancelled");
                    return Ok(());
                };
                let path_for_stash = path.clone();
                if let Err(err) = self.run_busy(terminal, "Stashing local changes…", move || {
                    dirty::stash_local_changes(&path_for_stash)
                }) {
                    self.set_error(format!("Stash failed: {err:#}"));
                    return Ok(());
                }
                let path_for_check = path.clone();
                let report =
                    match self.run_busy(terminal, "Checking worktree for leftovers…", move || {
                        dirty::inspect_worktree(&path_for_check)
                    }) {
                        Ok(report) => report,
                        Err(err) => {
                            self.set_error(format!("Re-check after stash failed: {err:#}"));
                            return Ok(());
                        }
                    };
                if report.is_clean() {
                    self.complete_release(terminal, task_idx, then_archive)?;
                } else {
                    let msg = if report.has_remote_divergence() && !report.has_local_changes() {
                        "Stashed local changes; unpushed commits still block release".to_string()
                    } else {
                        "Stashed; still dirty — check again after fixing".to_string()
                    };
                    let actions = dirty::menu_actions(&report);
                    if let Some(rel) = self.release.as_mut() {
                        rel.report = report;
                        rel.actions = actions;
                        rel.action_cursor = 0;
                    }
                    self.set_error(msg);
                }
            }
        }
        Ok(())
    }

    fn cancel_release(&mut self) {
        let then_archive = self
            .release
            .as_ref()
            .map(|r| r.then_archive)
            .unwrap_or(false);
        self.release = None;
        self.view = View::TaskList;
        if then_archive {
            self.set_status("Archive cancelled (worktree not released)");
        } else {
            self.set_status("Release cancelled");
        }
    }

    // --- Shared helpers ----------------------------------------------------

    fn sort_tasks(&mut self) {
        self.tasks.sort_by_key(|b| std::cmp::Reverse(b.last_used));
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some(StatusMessage {
            text: msg.into(),
            is_error: false,
            busy: false,
        });
    }

    fn set_busy(&mut self, msg: impl Into<String>) {
        self.status = Some(StatusMessage {
            text: msg.into(),
            is_error: false,
            busy: true,
        });
    }

    fn set_error(&mut self, msg: impl Into<String>) {
        self.status = Some(StatusMessage {
            text: msg.into(),
            is_error: true,
            busy: false,
        });
    }

    fn clear_status(&mut self) {
        self.status = None;
    }
}
