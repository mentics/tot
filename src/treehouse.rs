//! Treehouse CLI integration: lease worktrees and launch Cursor.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use color_eyre::eyre::{Context, eyre};
use serde::Deserialize;

use crate::task::Worktree;

/// Result of `treehouse get --lease`.
#[derive(Debug, Clone)]
pub struct LeasedWorktree {
    pub number: i32,
    pub path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct LeaseJson {
    path: PathBuf,
    #[serde(default)]
    #[allow(dead_code)]
    lease_id: Option<String>,
}

/// Lease a worktree via Treehouse (`get --lease`), with submodules when supported.
///
/// Prefers `--json` for a structured path. Falls back to parsing path from stdout.
/// Always attempts `--submodules` (mentics fork / documented API).
pub fn lease_worktree(cwd: impl AsRef<Path>) -> color_eyre::Result<LeasedWorktree> {
    let cwd = cwd.as_ref();

    // Preferred: documented API with JSON + submodules.
    match run_lease(cwd, &["get", "--lease", "--submodules", "--json"]) {
        Ok(out) => return parse_lease_output(&out, true),
        Err(err) if is_unknown_flag_error(&err) => {
            // Retry without --json if that was the problem; still want --submodules.
        }
        Err(err) => {
            return Err(err).wrap_err("treehouse get --lease --submodules --json failed");
        }
    }

    // --json may be unavailable; try path-only stdout with --submodules.
    match run_lease(cwd, &["get", "--lease", "--submodules"]) {
        Ok(out) => return parse_lease_output(&out, false),
        Err(err) if is_unknown_flag_error(&err) => {}
        Err(err) => {
            return Err(err).wrap_err("treehouse get --lease --submodules failed");
        }
    }

    // Last resort: lease without --submodules (upstream without fork flag).
    let out = run_lease(cwd, &["get", "--lease", "--json"])
        .or_else(|_| run_lease(cwd, &["get", "--lease"]))
        .wrap_err(
            "treehouse get --lease failed (CLI may lack --lease / --submodules — \
             upgrade Treehouse or install a build with the lease API)",
        )?;
    parse_lease_output(&out, out.trim_start().starts_with('{'))
}

fn run_lease(cwd: &Path, args: &[&str]) -> color_eyre::Result<String> {
    let output = Command::new("treehouse")
        .args(args)
        .current_dir(cwd)
        .output()
        .wrap_err("failed to run `treehouse` — is it installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        return Err(eyre!("treehouse {} failed: {}", args.join(" "), detail));
    }

    String::from_utf8(output.stdout).wrap_err("treehouse stdout was not valid UTF-8")
}

fn is_unknown_flag_error(err: &color_eyre::Report) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    msg.contains("unknown flag") || msg.contains("unknown shorthand")
}

/// A Treehouse/git worktree path that blocked leasing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasePathConflict {
    pub path: PathBuf,
    pub kind: LeasePathConflictKind,
}

/// Why Treehouse could not create a worktree at `path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeasePathConflictKind {
    /// Directory gone, but git still has the worktree registered.
    MissingButRegistered,
    /// Directory (or file) already present where git wants to add a worktree.
    AlreadyExists,
}

/// Detect recoverable path conflicts inside a lease error.
pub fn parse_lease_path_conflict(err: &color_eyre::Report) -> Option<LeasePathConflict> {
    parse_lease_path_conflict_msg(&format!("{err:#}"))
}

fn parse_lease_path_conflict_msg(msg: &str) -> Option<LeasePathConflict> {
    parse_stale_registered_worktree_msg(msg).or_else(|| parse_path_already_exists_msg(msg))
}

/// Detect git's "missing but already registered worktree" failure inside a lease error.
fn parse_stale_registered_worktree_msg(msg: &str) -> Option<LeasePathConflict> {
    let lower = msg.to_lowercase();
    if !lower.contains("missing but already registered worktree") {
        return None;
    }

    const MARKER: &str = "is a missing but already registered worktree";
    let marker_pos = lower.find(MARKER)?;
    let before = &msg[..marker_pos];
    let path = extract_quoted_path_before(before).or_else(|| extract_last_absolute_path(before))?;

    Some(LeasePathConflict {
        path: PathBuf::from(path),
        kind: LeasePathConflictKind::MissingButRegistered,
    })
}

/// Detect git's "`path` already exists" failure from `git worktree add`.
fn parse_path_already_exists_msg(msg: &str) -> Option<LeasePathConflict> {
    let lower = msg.to_lowercase();
    if lower.contains("missing but already registered") {
        return None;
    }
    if !lower.contains("already exists") {
        return None;
    }

    const MARKER: &str = "already exists";
    let marker_pos = lower.find(MARKER)?;
    let before = &msg[..marker_pos];
    let path = extract_quoted_path_before(before).or_else(|| extract_last_absolute_path(before))?;

    Some(LeasePathConflict {
        path: PathBuf::from(path),
        kind: LeasePathConflictKind::AlreadyExists,
    })
}

fn extract_quoted_path_before(before: &str) -> Option<String> {
    // Walk backward for the last '...' or "..." segment.
    let bytes = before.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        let quote = bytes[i];
        if quote != b'\'' && quote != b'"' {
            continue;
        }
        // Find matching opener before this closer.
        let closer_idx = i;
        let mut j = i;
        while j > 0 {
            j -= 1;
            if bytes[j] == quote {
                let candidate = &before[j + 1..closer_idx];
                if looks_like_path(candidate) {
                    return Some(candidate.to_string());
                }
                break;
            }
        }
    }
    None
}

fn extract_last_absolute_path(before: &str) -> Option<String> {
    // Fallback: last whitespace-separated token that looks absolute.
    before
        .split_whitespace()
        .rev()
        .find(|t| looks_like_path(t.trim_matches(|c| c == ':' || c == ',' || c == ';')))
        .map(|t| {
            t.trim_matches(|c| c == ':' || c == ',' || c == ';')
                .to_string()
        })
}

fn looks_like_path(s: &str) -> bool {
    !s.is_empty() && (s.starts_with('/') || s.starts_with('\\') || s.contains("/.treehouse/"))
}

fn parse_lease_output(stdout: &str, expect_json: bool) -> color_eyre::Result<LeasedWorktree> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(eyre!("treehouse lease returned empty stdout"));
    }

    let path = if expect_json || trimmed.starts_with('{') {
        let parsed: LeaseJson = serde_json::from_str(trimmed)
            .wrap_err_with(|| format!("parsing treehouse --json output: {trimmed}"))?;
        parsed.path
    } else {
        // Human banners go to stderr; path should be the only/last non-empty stdout line.
        PathBuf::from(
            trimmed
                .lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty())
                .ok_or_else(|| eyre!("treehouse lease stdout had no path line"))?,
        )
    };

    if !path.is_absolute() {
        return Err(eyre!(
            "treehouse lease path is not absolute: {}",
            path.display()
        ));
    }

    let number = worktree_number_from_path(&path).or_else(|| {
        // Optional: status --json if available (may fail on older CLIs).
        status_number_for_path(&path).ok().flatten()
    });

    let number = number.ok_or_else(|| {
        eyre!(
            "could not derive worktree number from path {} \
             (expected .../<N>/<reponame> under the treehouse root)",
            path.display()
        )
    })?;

    Ok(LeasedWorktree { number, path })
}

/// Derive worktree number from `.../<N>/<reponame>` path layout.
pub fn worktree_number_from_path(path: &Path) -> Option<i32> {
    let parent = path.parent()?;
    let num_str = parent.file_name()?.to_str()?;
    num_str.parse::<i32>().ok().filter(|&n| n > 0)
}

/// Resolve a Treehouse main worktree from a main or submodule path under the pool.
///
/// Accepts `.../<N>/<reponame>` or `.../<N>/<reponame>/<module>`.
pub fn main_worktree_from_pool_path(path: &Path) -> Option<(i32, PathBuf)> {
    if let Some(n) = worktree_number_from_path(path) {
        return Some((n, path.to_path_buf()));
    }
    let parent = path.parent()?;
    let n = worktree_number_from_path(parent)?;
    Some((n, parent.to_path_buf()))
}

fn status_number_for_path(path: &Path) -> color_eyre::Result<Option<i32>> {
    let output = Command::new("treehouse")
        .args(["status", "--json"])
        .output()
        .wrap_err("running treehouse status --json")?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    #[derive(Deserialize)]
    struct StatusEntry {
        name: Option<String>,
        path: PathBuf,
    }
    let entries: Vec<StatusEntry> = serde_json::from_str(stdout.trim()).unwrap_or_default();
    for entry in entries {
        if entry.path == path {
            if let Some(name) = entry.name
                && let Ok(n) = name.parse::<i32>()
            {
                return Ok(Some(n));
            }
            // Fall back to path layout for this entry.
            return Ok(worktree_number_from_path(&entry.path));
        }
    }
    Ok(None)
}

impl From<LeasedWorktree> for Worktree {
    fn from(leased: LeasedWorktree) -> Self {
        Worktree {
            number: leased.number,
            path: leased.path,
        }
    }
}

/// Return a leased worktree to the Treehouse pool (`treehouse return {path}`).
///
/// Tries a plain return first (stdin closed so prompts cannot hang the TUI).
/// If that fails — typically because the CLI wants confirmation — retries with
/// `--force`. Callers must run the dirty-worktree check first so `--force` is
/// only used after local leftovers have been gated.
pub fn return_worktree(path: impl AsRef<Path>) -> color_eyre::Result<()> {
    let path = path.as_ref();
    let path_str = path
        .to_str()
        .ok_or_else(|| eyre!("worktree path is not valid UTF-8: {}", path.display()))?;

    match run_return(&["return", path_str]) {
        Ok(()) => Ok(()),
        Err(err) => {
            // Non-interactive TUI: plain return may refuse without a tty prompt.
            // Dirty check already ran; --force is the non-interactive path.
            run_return(&["return", "--force", path_str]).wrap_err_with(|| {
                format!(
                    "treehouse return failed for {} (plain return error was: {err:#})",
                    path.display()
                )
            })
        }
    }
}

fn run_return(args: &[&str]) -> color_eyre::Result<()> {
    let output = Command::new("treehouse")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .wrap_err("failed to run `treehouse return` — is treehouse installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        return Err(eyre!("treehouse {} failed: {}", args.join(" "), detail));
    }
    Ok(())
}

/// Open Cursor on `path` as `cd {path} && cursor .`, without waiting for the
/// editor window to close.
///
/// TEMP: IPC hook refresh (`VSCODE_IPC_HOOK_CLI` / `vscode-ipc-*.sock`) is
/// disabled; any inherited hook is cleared for the child process.
pub fn launch_cursor(path: impl AsRef<Path>) -> color_eyre::Result<()> {
    let path = path.as_ref();
    let display_path = path.display().to_string();

    let mut cmd = Command::new("cursor");
    cmd.arg(".")
        .current_dir(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // TEMP: disable IPC — do not refresh or inherit VSCODE_IPC_HOOK_CLI.
    // if let Some(hook) = newest_vscode_ipc_hook() {
    //     cmd.env("VSCODE_IPC_HOOK_CLI", &hook);
    // }
    cmd.env_remove("VSCODE_IPC_HOOK_CLI");

    // Waiting lets us surface silent failures (CLI often exits 0 while printing
    // an error).
    let output = cmd.output().wrap_err_with(|| {
        format!("failed to launch `cursor` on {display_path} — is the Cursor CLI on PATH?")
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");
    if !output.status.success()
        || combined.contains("only available in WSL or inside a Visual Studio Code terminal")
    {
        let detail = combined.trim();
        if detail.is_empty() {
            return Err(eyre!(
                "cursor launch failed for {display_path} (exit {})",
                output.status
            ));
        }
        return Err(eyre!("cursor launch failed for {display_path}: {detail}"));
    }
    Ok(())
}

/// Prefer the newest live `vscode-ipc-*.sock` so a long-lived process does not
/// keep using the hook it inherited at startup.
///
/// TEMP: unused while IPC hook refresh is disabled in `launch_cursor`.
#[allow(dead_code)]
fn newest_vscode_ipc_hook() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(xdg) = env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(xdg);
        if p.is_dir() {
            dirs.push(p);
        }
    }
    if let Some(uid) = current_uid() {
        let p = PathBuf::from(format!("/run/user/{uid}"));
        if p.is_dir() && !dirs.contains(&p) {
            dirs.push(p);
        }
    }
    let tmp = PathBuf::from("/tmp");
    if tmp.is_dir() && !dirs.contains(&tmp) {
        dirs.push(tmp);
    }
    // Also scan the directory of the inherited hook, if any.
    if let Ok(existing) = env::var("VSCODE_IPC_HOOK_CLI") {
        if let Some(parent) = Path::new(&existing).parent() {
            let p = parent.to_path_buf();
            if p.is_dir() && !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }

    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !(name.starts_with("vscode-ipc-") && name.ends_with(".sock")) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|(best_time, _)| modified > *best_time)
            {
                best = Some((modified, path));
            }
        }
    }
    best.map(|(_, path)| path)
}

#[allow(dead_code)]
fn current_uid() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("Uid:") else {
            continue;
        };
        return rest.split_whitespace().next()?.parse().ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_number_from_typical_path() {
        let path = PathBuf::from("/home/u/.treehouse/myproject-a1b2c3/3/myproject");
        assert_eq!(worktree_number_from_path(&path), Some(3));
    }

    #[test]
    fn rejects_non_numeric_parent() {
        let path = PathBuf::from("/home/u/.treehouse/myproject/myproject");
        assert_eq!(worktree_number_from_path(&path), None);
    }

    #[test]
    fn parses_lease_json() {
        let json = r#"{"path":"/home/u/.treehouse/repo-abc/2/repo","lease_id":"x","lease_holder":"me","leased_at":"t"}"#;
        let leased = parse_lease_output(json, true).unwrap();
        assert_eq!(leased.number, 2);
        assert_eq!(
            leased.path,
            PathBuf::from("/home/u/.treehouse/repo-abc/2/repo")
        );
    }

    #[test]
    fn parses_plain_path_stdout() {
        let out = "/home/u/.treehouse/repo-abc/1/repo\n";
        let leased = parse_lease_output(out, false).unwrap();
        assert_eq!(leased.number, 1);
    }

    #[test]
    fn detects_stale_registered_worktree_error() {
        let msg = "treehouse get --lease --submodules --json failed: \
             🌳 Setting up worktree...\n\
             failed to create worktree: git worktree add --detach \
             /home/vscode/.treehouse/workspace-df5f8e/1/workspace refs/remotes/origin/main: \
             Preparing worktree (detached HEAD 147730e)\n\
             fatal: '/home/vscode/.treehouse/workspace-df5f8e/1/workspace' is a missing but already registered worktree;\n\
             use 'add -f' to override, or 'prune' or 'remove' to clear";
        let conflict = parse_lease_path_conflict_msg(msg).unwrap();
        assert_eq!(
            conflict.path,
            PathBuf::from("/home/vscode/.treehouse/workspace-df5f8e/1/workspace")
        );
        assert_eq!(conflict.kind, LeasePathConflictKind::MissingButRegistered);
    }

    #[test]
    fn detects_path_already_exists_error() {
        let msg = "treehouse get --lease --submodules --json failed: \
             🌳 Setting up worktree...\n\
             failed to create worktree: git worktree add --detach \
             /home/vscode/.treehouse/workspace-df5f8e/1/workspace refs/remotes/origin/main: \
             Preparing worktree (detached HEAD 147730e)\n\
             fatal: '/home/vscode/.treehouse/workspace-df5f8e/1/workspace' already exists";
        let conflict = parse_lease_path_conflict_msg(msg).unwrap();
        assert_eq!(
            conflict.path,
            PathBuf::from("/home/vscode/.treehouse/workspace-df5f8e/1/workspace")
        );
        assert_eq!(conflict.kind, LeasePathConflictKind::AlreadyExists);
    }

    #[test]
    fn derives_main_worktree_from_submodule_pool_path() {
        let (n, path) = main_worktree_from_pool_path(Path::new(
            "/home/vscode/.treehouse/workspace-df5f8e/3/workspace/flagship",
        ))
        .unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            path,
            PathBuf::from("/home/vscode/.treehouse/workspace-df5f8e/3/workspace")
        );
    }

    #[test]
    fn ignores_unrelated_lease_errors() {
        assert!(parse_lease_path_conflict_msg("treehouse get failed: pool empty").is_none());
    }
}
