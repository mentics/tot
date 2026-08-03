# Features learnings

Progress and decisions while implementing `doc/features.md`.

## Status

| Section | Status |
| --- | --- |
| Repository context | done (modules discovery helper) |
| Data model | done (includes `pr_number`) |
| Persistence | done (tasks + `settings.json`) |
| Views | done (includes Settings) |
| Workflows — Create | done |
| Workflows — Open issue / Open PR | done |
| Workflows — Switch | done |
| Workflows — Archive / Unarchive / Release / Dirty check | done |
| Integrations & credentials | done |

## Log

### 2026-08-03 — waiting on main list

**Completed**
- `Task.waiting` bool (`#[serde(default)]`, default false); stays active when set.
- Main list **W** toggles waiting: touch + persist + sort + reselect by stem (no worktree release).
- When any active task is waiting: non-waiting → dim `── waiting ──` divider → waiting; ↑/↓ skip divider. No divider when none waiting.
- Archive view unchanged; waiting ignored while archived.

**Decisions**
- No separate Waiting view, status enum, or per-row waiting badge (divider only).

**Blockers:** none.

### 2026-07-30 — task notes on edit view

**Completed**
- `Task.notes` list of `{ body, created_at }`; newest-first sort on load/add; persisted with the task JSON.
- Edit view Notes pane: one-line truncated previews; Enter opens full note; N/A compose (Ctrl+S save); Del/D delete.
- Full note view styles `http(s)` URLs; Ctrl+click opens in the browser (mouse capture enabled).

**Decisions**
- Notes are add/view/delete only (no in-place edit of existing notes yet).
- Composer uses Ctrl+S so Enter can insert newlines.

**Follow-ups**
- Optional: edit an existing note.

**Blockers:** none.

### 2026-07-30 — switch activate fast path

**Completed**
- Switch skips checkouts when the worktree is already on expected branches (main + submodules).
- Detection uses `git rev-parse --abbrev-ref HEAD` only (no `git status`); `checkout_or_create_branch` also no-ops when already on the target.

**Decisions**
- “Already activated” is the gate, not “Cursor window open” (no reliable close notification / path-accurate window list).
- Any HEAD read failure → treat as not activated and fall through to normal activate.

### 2026-07-30 — per-module PRs

**Completed**
- Replaced single `pr_number` with `module_prs` map (module name → PR number); legacy `pr_number` migrated on load when possible.
- `{repository}` in PR URL template is the module name; `{namespace}` still from git `origin`.
- Edit: PR shown beside each module; Enter/P edits that module’s PR.
- Open PR: Linear attachments matched by repo name to modules; auto-open when exactly one module has a PR; otherwise module picker.
- Clearing a module (Space) also clears its PR.

**Decisions**
- Do not overwrite existing module PR values when Linear fills.
- One candidate with a known PR → open immediately (no picker).

### 2026-07-29 — open issue / PR + settings

**Completed**
- `Task.pr_number` optional field; editable in Edit view.
- Settings view + `{config}/settings.json` with issue/PR URL templates (`{issue_id}`, `{namespace}`, `{repository}`, `{pr_number}`).
- Main list **I** opens issue (prompts for issue ID if missing); **P** opens PR (Linear attachment lookup when PR missing but issue ID present; else prompt); **S** opens settings.
- Linear `fetch_pr_number_for_issue` via issue attachments; git `origin` parsed for namespace/repo.
- Browser open via `xdg-open` / `open`.

**Decisions**
- PR template defaults to GitHub-style URL; issue template starts empty (must be set).
- Namespace/repository come from git remote at open time, not stored in settings.
- Credential prompt generalized to resume create or open-PR.

**Follow-ups**
- Optional: open issue/PR from archive/edit views.
- Optional: settings for non-`origin` remotes.

**Blockers:** none.

### 2026-07-23 — kickoff

- Starting from scaffold: placeholder `Task`/`TaskStatus`, single list UI, no persistence/integrations.
- Package name and UI branding are both `tod`.
- Work order: data model → persistence → credentials → views → create → switch → archive/release/dirty.

### 2026-07-23 — data model + persistence

**Completed**
- Replaced placeholder `Task`/`TaskStatus` with real `Task` + `Worktree` (serde + chrono `DateTime<Utc>`).
- Added `task::available_modules(cwd)` via `git rev-parse --show-toplevel` + `.gitmodules` submodule names.
- Added `persist` module: `TOD_DATA_DIR` or `$HOME/.config/tod/tasks/`, load-all on startup, immediate `save_task`, filename = normalized truncated title + 6-char random suffix.
- App loads from disk (no hardcoded examples); UI shows title / branch / worktree number; non-archived only in the list.

**Decisions**
- `file_stem` is `#[serde(skip)]` and taken from the JSON filename on load (identity lives in the path, not the body).
- List sort by `last_used` descending happens in `load_all_tasks`; main list filters `archived == false`.
- Main module name = basename of git toplevel (not remote URL).
- Kept package name `tod`.

**Deps added:** `serde`, `serde_json`, `chrono` (serde), `rand`, `dirs`.

**Follow-ups**
- Wire `save_task` / `allocate_file_stem` / `touch` into create/edit/archive workflows.
- Views (Create row, archive view, edit view) and keybindings still pending.
- Dead-code warnings on helpers until workflows land (expected).

**Blockers:** none.

### 2026-07-23 — Views

**Completed**
- `View` enum: `TaskList`, `Archive`, `Edit`, `CreatePrompt` (stub).
- Task list: row 0 = Create new task; active tasks by `last_used` desc; keys ↑/↓, Enter (switch stub / create prompt), E edit, R stub, A archive (real if no worktree; stub if worktree), Shift+A archive view, Q quit.
- Archive view: archived tasks same row fields; U unarchives (persist + touch + resort); Esc → task list; Q quit.
- Edit view: title/branch/issue ID editable (persist + touch on each keystroke); modules multiselect from `available_modules(cwd)` with Space toggle; worktree read-only; Tab/↑/↓ focus; Enter advances from text fields; Esc returns to previous view.
- Status line for workflow stubs (switch / release / archive-with-worktree / create).

**Decisions**
- Keep drawing in `ui.rs` and event handling in `app.rs` (no `src/views/` split yet — still small).
- Shift+A: `KeyModifiers::SHIFT` on `a`/`A`, plus bare `A` fallback for terminals that omit the shift flag.
- While edit focus is title/branch/issue ID, `q` types into the field; quit with Q from modules focus or after Esc.
- Simple archive (no worktree) implemented; archive with worktree stays stubbed until Release/dirty-check workflows.
- Create prompt is UI-only: Enter shows stub status and returns to list.

**Follow-ups**
- Wire Create prompt parsing + Linear lookup (Workflows — Create).
- Switch / Release / Archive-with-release / Dirty check.
- Optional: open Edit from archive view.

**Blockers:** none.

### 2026-07-23 — Integrations + Create workflow

**Completed**
- OS keyring via `keyring`: service `tod`, account `linear` for the Linear API key (`credentials` module).
- On first Linear need: load key from keyring; if missing, `CredentialPrompt` view (masked input) → store → resume create.
- Create prompt parsing (`create` module): issue ID → branch (`git check-ref-format --branch`) → title; invalid branch shape that looks like `prefix/suffix` errors instead of silently becoming a title.
- Linear GraphQL (`linear` module, `ureq`): `issue(id:)` with identifier, fallback filter by team key + number; errors surface as TUI status (no panic).
- New task: allocate stem, persist immediately, non-archived, `last_used` now, select in main list.

**Decisions**
- Keyring naming: service=`tod`, user/account=`linear`.
- HTTP: `ureq` 3 (blocking) over reqwest — lighter for a TUI.
- `keyring` 3.x (stable `Entry` / `NoEntry` API).
- Credential UI is a dedicated view (not an overlay modal).
- When input matches the branch pattern but fails `check-ref-format`, reject with status rather than treating as title.

**Deps added:** `keyring`, `ureq` (json).

**Follow-ups**
- Switch / Release / Archive-with-release / Dirty check.
- Optional: re-prompt credentials on auth failure (currently only prompts when keyring entry is missing).
- Optional: open Edit from archive view.

**Blockers:** none. Live Linear lookup not exercised in automated tests (no API key in CI).

### 2026-07-23 — Workflows — Switch

**Completed**
- Enter on a task runs full switch: prerequisites → lease (if needed) → activate → `cursor --folder-uri vscode-remote://attached-container+…`.
- Prerequisites UI: `SwitchModules` multiselect (Space/Enter) and `SwitchBranch` text prompt when no worktree yet; persist + touch before continuing.
- `treehouse` module: `get --lease --submodules --json` (with fallbacks for missing flags / plain path stdout); derive worktree number from `.../<N>/<reponame>`; detached `cursor` spawn.
- `gitutil` + `switch`: checkout/create task branch on selected modules, `temp{N}` elsewhere (main + each submodule).
- Status messages for lease / activate / Cursor failures; list selection restored after touch/sort.

**Decisions**
- Modules prompt requires ≥1 selection before continuing (empty still means “not chosen”).
- Implement against documented API (`--lease`, `--json`, `--submodules`); degrade gracefully when flags missing.
- Cursor is fire-and-forget (`spawn`, null stdio) so the TUI is not blocked.
- Shared `gitutil` extracted; `task::available_modules` now uses it.

**Treehouse CLI findings (local)**
- Installed binary is **v1.7.0** (`~/.local/bin/treehouse`): **no** `--lease`, **no** `--json` on get/status, **no** `--submodules`.
- Documented / mentics fork API: `treehouse get --lease [--submodules] [--json]`; `--submodules` prepares managed submodule worktrees (mentics fork `get.go`).
- Lease against v1.7 surfaces a clear upgrade/fork error via status line (expected until CLI is updated).

**Follow-ups**
- Release / dirty check / archive-with-worktree.
- End-to-end switch once Treehouse ≥ lease + (optionally) mentics `--submodules` is installed.
- Optional: prompt for branch if an existing worktree association has no branch (spec only requires prompts before New worktree).

**Blockers:** none for code; live lease needs a newer Treehouse than local v1.7.

### 2026-07-23 — Workflows — Archive / Release / Dirty check

**Completed**
- **R** releases a task worktree: dirty check → `treehouse return` → clear association → touch + persist.
- **A** archives; if a worktree is associated, runs the same release path first (`then_archive`); cancel/block aborts archive and leaves the association unchanged.
- Unarchive left as-is (already correct).
- `DirtyWarning` view with selectable options (↑/↓ + Enter, shortcuts C/S/X, Esc cancel).
- Dirty inspection (`dirty` module): main repo + each submodule; staged / unstaged / untracked / unpushed (ahead only; behind ignored); parent gitlink changes ignored via `--ignore-submodules=all`.
- Warning UI groups by location + kind; ≤10 paths listed, else count; unpushed summarized in words.
- Stash option when local dirt exists: unstage then `git stash push -u` per dirty location; re-check; unpushed commits still block.
- Unit tests for classification, stash, gitlink ignore, formatting (temp git repos).

**Decisions**
- `treehouse return {path}` first with stdin null; on failure retry `treehouse return --force {path}`. Plain return can prompt on a TTY; the TUI cannot answer interactively, and dirty check already gates local leftovers, so `--force` is the safe non-interactive fallback.
- Stash menu label explicitly says untracked are included and staged files are unstaged first.
- No-worktree **R** shows status “No worktree associated with this task” (brief no-op message).
- Release state tracks `then_archive` so a successful clean check / stash / check-again continues into archive when started from **A**.

**Follow-ups**
- End-to-end release once Treehouse lease+return is available against a real worktree.
- Optional: open Edit from archive view (still open from Views).

**Blockers:** none for code; live return needs a leased Treehouse worktree.

### 2026-07-23 — Final verification

- `cargo test` — 19 passed
- `cargo clippy -- -D warnings` — clean (after small style fixes)
- All feature sections marked done in the status table above.
- Known runtime dependency: Treehouse with `get --lease` (local install is still v1.7 without lease); Linear live lookup needs a keyring API key.
