# Features

Spec for **tod**: a TUI to manage coding tasks, associate them with git worktrees (via Treehouse), and open Cursor on the right tree.

## Contents

- [Repository context](#repository-context)
- [Data model](#data-model)
- [Persistence](#persistence)
- [Views](#views)
- [Workflows](#workflows)
- [Integrations & credentials](#integrations--credentials)

---

## Repository context

tod always uses the **current working directory** as the main git repository. Available modules (main repo name + submodule names) and Treehouse operations are resolved relative to that repo. The user is expected to launch tod from the repo they intend to work in.

---

## Data model

### Task

| Field | Type | Notes |
| --- | --- | --- |
| Title | string | Required |
| Branch | string? | Optional branch name |
| Issue ID | string? | Optional tracker issue ID (e.g. from Linear) |
| Module PRs | map of string → integer | Optional PR number per associated module (module name = repository in the PR URL template). May be filled from Linear attachments matched by repo name |
| Modules | list of string | Required; may be empty. See [Modules](#modules) |
| Notes | list of Note | Optional; free-form notes on the task. Sorted **newest first** in the edit view. See [Notes](#notes) |
| Worktree | Worktree? | Optional; tracked automatically (not set in the edit view). See [Worktree](#worktree) |
| Last used | timestamp | **Cognitive recency**: updated on any interaction that involves this task (create, edit, switch, archive, unarchive, release worktree, etc.). List views sort by this **descending** (most recent first) so recently touched tasks stay near the top of the unarchived list |
| Archived | bool | When true, the task appears only in the [Archive view](#2-archive-view), not the main list |

### Notes

Each note has:

| Field | Type | Notes |
| --- | --- | --- |
| Body | string | Free-form text (may be multi-line; may contain `http://` / `https://` URLs) |
| Created at | timestamp | Used for newest-first ordering |

### Modules

Each task’s **modules** list is which parts of the repo matter when activating a worktree.

Available module names are **dynamic per repository** (from cwd):

- the name of the main repo itself
- plus the name of each git submodule

Stored values are those names. An empty list means none selected yet.

### Worktree

When a task has an associated worktree:

| Field | Type | Notes |
| --- | --- | --- |
| Number | integer | Displayed as `1`, `2`, `3`, … |
| Path | path | Filesystem path to the worktree |

Association is tracked automatically; the user does not set it by hand in the edit view. It is cleared when the worktree is [released](#release-worktree).

---

## Persistence

Tasks (and all associated fields) are persisted as **one JSON file per task**.

| Setting | Behavior |
| --- | --- |
| Config directory | From `TOD_DATA_DIR` if set; otherwise `$HOME/.config/tod/` (Linux XDG-style). Task files live under `{config}/tasks/`. Settings live at `{config}/settings.json`. |
| Load | On startup, load all task JSON files from `{config}/tasks/` and settings from `{config}/settings.json` (defaults if missing) |
| Save | Write to disk **immediately** whenever any task data or settings change |
| File names | Normalized, truncated title + random characters (avoids collisions) |

---

## Views

Primary UI screens. Multi-step behavior lives under [Workflows](#workflows).

Keybindings are fixed for now (not user-remappable).

### 1. Task list (main / initial view)

Active (non-archived) tasks, sorted by **last used descending**.

Each task row shows:

- title
- branch (if any)
- worktree number (if any)

| Key | Action |
| --- | --- |
| ↑ / ↓ | Move selection |
| Enter | [Switch](#switch-to-a-task) to the selected task |
| N | [Create new task](#create-new-task) |
| E | Open [Edit view](#3-edit-view) for the selected task |
| I | [Open issue](#open-issue) for the selected task in the browser |
| P | [Open PR](#open-pr) for the selected task in the browser |
| R | [Release worktree](#release-worktree) for the selected task (without archiving) |
| A | [Archive](#archive-task) the selected task |
| Shift+A | Open [Archive view](#2-archive-view) |
| S | Open [Settings view](#4-settings-view) |
| Q | Quit |

### 2. Archive view

Archived tasks, sorted by **last used descending**. Same row fields as the main list (title, branch, worktree number).

| Key | Action |
| --- | --- |
| ↑ / ↓ | Move selection |
| U | [Unarchive](#unarchive-task) the selected task |
| Esc | Return to the task list |
| Q | Quit |

### 3. Edit view

Edit a task’s editable fields; show read-only worktree info. Field changes update **last used** and persist immediately.

**Editable**

- title
- branch
- issue ID
- **modules** — multiselect of available modules (main repo name + each submodule name); each selected module may have a PR number (Enter / P while focused on the module)
- **notes** — list of notes, newest first; each row shows a one-line truncated preview

**Display only**

- worktree number and/or path (if associated)

| Key | Action |
| --- | --- |
| Tab / ↑ / ↓ | Move between fields (and within the modules / notes lists) |
| Enter | Confirm the current text field (title / branch / issue ID). On modules: set/edit that module’s PR number. On notes: open the full note |
| Space | Toggle the highlighted module on/off (clears that module’s PR when unchecked) |
| N / A | Add a new note (opens the note composer; Ctrl+S saves, Esc cancels) |
| Del / D | On notes: delete the highlighted note. On worktree: clear the association |
| Esc | Return to the previous view |
| Q | Quit |

#### Full note view

Opened with Enter on a note in the edit list. Shows the full body; URLs are underlined.

| Key / input | Action |
| --- | --- |
| ↑ / ↓ | Scroll |
| Ctrl+click | Open the clicked `http(s)` URL in the system browser |
| Esc | Return to the edit view |
| Q | Quit |

---

### 4. Settings view

Edit URL templates used when opening issues and PRs. Changes persist immediately to `{config}/settings.json`.

| Setting | Placeholders | Notes |
| --- | --- | --- |
| Issue URL template | `{issue_id}` | Required before **I** can open an issue |
| PR URL template | `{namespace}`, `{repository}`, `{pr_number}` | Default is a GitHub-style URL. `{namespace}` comes from the git `origin` remote; `{repository}` is the **module name** |

| Key | Action |
| --- | --- |
| Tab / ↑ / ↓ | Move between fields |
| Esc | Return to the task list |
| Q | Quit |

---

## Workflows

### Create new task

From the task list: press **N**, then enter a single line. Parse in order of specificity:

1. **Issue ID** — matches `^[A-Za-z]{1,32}-[0-9]{1,32}$` (e.g. `ABC-123`). Look up in Linear, use the issue title as the task title, store the issue ID. May prompt for Linear credentials ([Integrations](#integrations--credentials)).
2. **Branch name** — matches a prefix, a `/`, then a git-valid suffix:
   - **Prefix:** `^[A-Za-z]{1,64}` (up to 64 alphabetic characters)
   - **Then:** `/`
   - **Suffix:** 1–128 characters that form a valid git branch name segment per [`git check-ref-format --branch`](https://git-scm.com/docs/git-check-ref-format) (applied to the full `prefix/suffix` string). In practice the suffix must not contain ASCII control characters, space, `~`, `^`, `:`, `?`, `*`, `[`, `\`; must not contain `..` or `@{`; must not be `@` alone; must not start with `-` or `.`; must not end with `.` or `.lock`; and slash-separated components follow the same rules.
   - Store the full matched string as the **branch**.
   - If the suffix contains a strict issue id (`[A-Z]{1,8}-[0-9]{1,8}` anywhere in the suffix, not part of a longer letter/digit run — e.g. `ENG-123` in `feat/fix-ENG-123-add-login`), look that id up in Linear: use the issue **title**, store the **issue ID**, and keep the **branch**. Otherwise use the full branch string as the **title** as well.
3. **Title (default)** — otherwise treat the input as the task title.

The new task is non-archived, gets **last used** set to now, and appears in the main list.

### Open issue

From the task list, press **I** on a task:

1. If the task has no issue ID, prompt for one; store it on the task and continue.
2. Build a URL from the [settings](#4-settings-view) issue URL template by substituting `{issue_id}`.
3. Open the URL in the system browser (`xdg-open` / `open`).

If the issue URL template is empty or missing `{issue_id}`, show an error directing the user to Settings.

### Open PR

From the task list, press **P** on a task:

1. The task must have at least one associated **module**.
2. For any modules missing a PR number, if the task has an **issue ID**, query Linear attachments. For each GitHub PR URL whose repository segment matches a module name, store that PR number on the module (existing values are not overwritten).
3. If exactly **one** module has a PR number, open it immediately (no picker).
4. If **multiple** modules have PR numbers, show a module picker, then open the chosen one.
5. If **no** module has a PR yet: with one module, prompt for its PR number; with several, pick a module then prompt if needed.
6. Build the URL with `{namespace}` from git `origin`, `{repository}` = module name, `{pr_number}` = that module’s PR, and open it in the browser.

### Switch to a task

Pressing Enter on a task in the main list runs **switch**: put the user in that task’s environment and open Cursor on its worktree. Updates **last used**.

Worktrees are managed with **[Treehouse](https://github.com/mentics/treehouse)** (our fork with submodule support). Treehouse maintains a pool of reusable git worktrees; we do not manage worktrees by hand.

#### High-level flow

```
if task has no worktree:
    ensure modules + branch (prompt if missing; update the task)
    run New worktree steps
run Activate worktree steps   # always, including after New worktree
launch Cursor: cursor --folder-uri vscode-remote://attached-container+{hex(containerId)}{worktree path}
```

Always run the attached-container Cursor launch as the **last** step, whether or not a Cursor window already exists for that path.

#### Prerequisites (no worktree yet)

Before **New worktree**, if the task has no worktree yet:

1. **Modules** — if the modules list is empty, prompt with a multiselect of available modules. Persist the selection.
2. **Branch** — if there is no branch, prompt for a branch name. Persist it.

#### New worktree

When the task is **not** yet associated with a worktree. On success, continue with **Activate worktree**.

1. Call Treehouse **lease** (`treehouse get --lease`), always with **submodules** enabled.
2. Associate the leased worktree (number + path) with the task and persist.

#### Activate worktree

When the task **already** has a worktree, and also always after **New worktree**.

If main and every submodule are already on the expected branches (checked with `git rev-parse --abbrev-ref HEAD` only — not `git status`), skip checkouts and go straight to launching Cursor.

Otherwise, check out a branch in the main repository and in every submodule. Worktrees cannot all share the same branch name, so:

| Target | Branch to check out |
| --- | --- |
| Modules associated with the task | The task’s **branch** (create if missing, then check out) |
| Main repo and submodules **not** in the task’s modules list | `temp{N}` where `N` is the worktree number (e.g. `temp1`, `temp3`). Create if missing, then check out |

Then launch Cursor on the worktree path.

### Archive task

From the task list, press **A** on a task:

1. If the task has a worktree association, run [Release worktree](#release-worktree) first (including the dirty check). If release is cancelled or blocked, do **not** archive.
2. Set `Archived = true`, update **last used**, persist.
3. The task leaves the main list and appears in the archive view.

### Unarchive task

From the archive view, press **U** on a task:

1. Set `Archived = false`, update **last used**, persist.
2. The task returns to the main list (near the top, since last used was just updated).

Unarchive does **not** re-lease a worktree; the user switches to the task later if they need one.

### Release worktree

Frees a Treehouse worktree without necessarily archiving the task. Invoked by:

- **R** on a task that has a worktree (release only)
- **A** archive, when the task still has a worktree (release, then archive)

If the task has no worktree, **R** is a no-op (or a brief status message).

#### Flow

```
run Dirty worktree check   # block until clean or user cancels
treehouse return {worktree path}
clear worktree association on the task and persist
update last used
```

Use Treehouse **`treehouse return {path}`** to release the lease and return the worktree to the pool.

### Dirty worktree check

Run **before every release** (whether from **R** or as part of archive). The goal is to prevent forgotten work from being wiped when Treehouse resets the tree.

#### What to inspect

Inspect the worktree’s **main repo** and **each submodule** separately for leftovers:

| Kind | Meaning |
| --- | --- |
| Staged | Index changes not committed |
| Unstaged | Tracked file modifications not staged |
| Untracked | Untracked files (including ignored? **no** — only non-ignored untracked) |
| Unpushed commits | Local branch is ahead of its upstream (commits that exist only locally). Being *behind* the remote is fine — those changes are already stored upstream and will not be lost on release |

**Ignore submodule pointer / gitlink changes in the main repo.** Submodules often appear “modified” in the parent because they are on a different branch than the parent expects; that is expected and must **not** trigger the warning. Always look *inside* each submodule (and the main repo’s own non-gitlink changes) for real leftovers.

#### Warning UI

If anything is dirty or divergent, show a warning and **do not proceed** until the tree is clean (or the user cancels the whole release/archive).

- Group findings by location (main repo vs each submodule) and by kind (**staged** / **unstaged** / **untracked** / **unpushed**), so the user can see what is going on.
- If a group has **10 or fewer** paths, list them.
- If a group has **more than 10** paths, show a **count** instead of listing every file (e.g. “23 untracked files”).
- Unpushed commits may be summarized in words (ahead count; behind only shown when the branch has also diverged) rather than a file list.

#### User options on the warning

| Option | Behavior |
| --- | --- |
| **Check again** | Re-run the inspection. If clean, proceed with release. If still dirty, show the warning again with the current findings. |
| **Stash changes** | Offered when stashing would clear the blocking local changes. Run a stash that includes **untracked** files. If there is staged content, **unstage then stash** (make that clear in the UI: staged files are listed as staged before stash). After a successful stash, re-check; if clean (including no unpushed commits), proceed. If unpushed commits remain, keep blocking until the user pushes or otherwise reconciles (stash does not fix unpushed commits). |
| **Cancel** | Abort release (and therefore abort archive if this check was part of archive). Leave the worktree association unchanged. |

The user may leave the TUI, fix things manually in the worktree, return to the same prompt, and choose **Check again**. Repeat until clean or cancelled.

---

## Integrations & credentials

Integrations (Linear now; others later) resolve credentials in this order:

1. **Env override** — `TOD_LINEAR_API_KEY` if set (non-persisted; useful for CI / one-off use)
2. **OS keyring** — via the [`keyring`](https://crates.io/crates/keyring) crate (service `tod`, account `linear`)
3. **Encrypted config file** — `{config}/credentials/linear_api_key` (same config dir as tasks: `TOD_DATA_DIR` or `$HOME/.config/tod/`)

When code first needs to contact an integration:

1. Try to load the credential using the order above.
2. If it is missing, prompt the user for it.
3. Store the entered credential in the OS keyring when possible; if the keyring is unavailable (common in Linux containers), write an **encrypted** fallback file instead.

**File encryption:** ChaCha20-Poly1305; key derived from machine-bound material (`/etc/machine-id`, else hostname + uid) plus an app salt. File mode `0600`. This reduces casual disclosure; it is **not** equivalent to a real OS keychain against an attacker who can already read your home directory and run code as you.

**Messaging:** After storing, the UI status states where the secret was saved (OS keyring, or the full encrypted file path when falling back). The credential prompt also notes the fallback path.
