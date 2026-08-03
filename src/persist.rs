use std::fs;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, eyre};
use rand::Rng;

use crate::task::Task;

const ENV_DATA_DIR: &str = "TOD_DATA_DIR";
const APP_DIR_NAME: &str = "tod";
const TASKS_SUBDIR: &str = "tasks";
const TITLE_STEM_MAX: usize = 40;
const RANDOM_SUFFIX_LEN: usize = 6;

/// Resolve the config/data directory: `TOD_DATA_DIR` or `$HOME/.config/tod/`.
pub fn config_dir() -> color_eyre::Result<PathBuf> {
    if let Ok(override_dir) = std::env::var(ENV_DATA_DIR) {
        let path = PathBuf::from(override_dir);
        if path.as_os_str().is_empty() {
            return Err(eyre!("{ENV_DATA_DIR} is set but empty"));
        }
        return Ok(path);
    }

    let home = dirs::home_dir().ok_or_else(|| eyre!("could not determine home directory"))?;
    Ok(home.join(".config").join(APP_DIR_NAME))
}

/// `{config}/tasks/`
pub fn tasks_dir() -> color_eyre::Result<PathBuf> {
    Ok(config_dir()?.join(TASKS_SUBDIR))
}

/// Ensure `{config}/tasks/` exists.
pub fn ensure_tasks_dir() -> color_eyre::Result<PathBuf> {
    let dir = tasks_dir()?;
    fs::create_dir_all(&dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Load every `*.json` task file from `{config}/tasks/`, sorted by `last_used` descending.
pub fn load_all_tasks() -> color_eyre::Result<Vec<Task>> {
    let dir = ensure_tasks_dir()?;
    let mut tasks = Vec::new();

    for entry in fs::read_dir(&dir).wrap_err_with(|| format!("reading {}", dir.display()))? {
        let entry = entry.wrap_err("reading tasks directory entry")?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let task = load_task_file(&path)
            .wrap_err_with(|| format!("loading task file {}", path.display()))?;
        tasks.push(task);
    }

    tasks.sort_by_key(|b| std::cmp::Reverse(b.last_used));
    Ok(tasks)
}

fn load_task_file(path: &Path) -> color_eyre::Result<Task> {
    let data = fs::read_to_string(path).wrap_err("reading file")?;
    let mut task: Task = serde_json::from_str(&data).wrap_err("parsing JSON")?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| eyre!("task file has no UTF-8 stem: {}", path.display()))?;
    task.file_stem = stem.to_string();
    task.migrate_legacy_pr();
    task.sort_notes();
    Ok(task)
}

/// Persist a task immediately to `{config}/tasks/{file_stem}.json`.
pub fn save_task(task: &Task) -> color_eyre::Result<()> {
    if task.file_stem.is_empty() {
        return Err(eyre!("cannot save task with empty file_stem"));
    }
    let dir = ensure_tasks_dir()?;
    let path = dir.join(format!("{}.json", task.file_stem));
    let json = serde_json::to_string_pretty(task).wrap_err("serializing task")?;
    fs::write(&path, json).wrap_err_with(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Permanently remove `{config}/tasks/{file_stem}.json`.
pub fn delete_task(file_stem: &str) -> color_eyre::Result<()> {
    if file_stem.is_empty() {
        return Err(eyre!("cannot delete task with empty file_stem"));
    }
    let dir = ensure_tasks_dir()?;
    let path = dir.join(format!("{file_stem}.json"));
    if !path.exists() {
        return Err(eyre!("task file not found: {}", path.display()));
    }
    fs::remove_file(&path).wrap_err_with(|| format!("removing {}", path.display()))?;
    Ok(())
}

/// Build a unique file stem: normalized truncated title + random suffix.
pub fn allocate_file_stem(title: &str) -> color_eyre::Result<String> {
    let dir = ensure_tasks_dir()?;
    let base = normalize_title_stem(title);
    loop {
        let suffix = random_alnum(RANDOM_SUFFIX_LEN);
        let stem = if base.is_empty() {
            suffix
        } else {
            format!("{base}-{suffix}")
        };
        let candidate = dir.join(format!("{stem}.json"));
        if !candidate.exists() {
            return Ok(stem);
        }
    }
}

fn normalize_title_stem(title: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true; // avoid leading dash
    for ch in title.chars() {
        if out.chars().count() >= TITLE_STEM_MAX {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn random_alnum(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| {
            let idx = rng.random_range(0..ALPHABET.len());
            ALPHABET[idx] as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Task;
    use std::sync::{Mutex, OnceLock};

    /// Serialize env-var tests that mutate `TOD_DATA_DIR`.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn normalize_strips_and_truncates() {
        assert_eq!(normalize_title_stem("Hello World!"), "hello-world");
        assert_eq!(normalize_title_stem("  --Foo--  "), "foo");
        let long = "a".repeat(100);
        assert_eq!(normalize_title_stem(&long).len(), TITLE_STEM_MAX);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let _guard = env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join(format!("tod-test-{}", random_alnum(8)));
        let _ = fs::remove_dir_all(&dir);
        // SAFETY: serialized by env_lock; restored below.
        unsafe {
            std::env::set_var(ENV_DATA_DIR, &dir);
        }

        let stem = allocate_file_stem("Hello World!").expect("stem");
        assert!(stem.starts_with("hello-world-"));
        assert_eq!(stem.len(), "hello-world-".len() + RANDOM_SUFFIX_LEN);

        let mut task = Task::new("Hello World!", stem);
        task.branch = Some("feat/x".into());
        task.modules = vec!["tod".into()];
        save_task(&task).expect("save");

        let loaded = load_all_tasks().expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].title, "Hello World!");
        assert_eq!(loaded[0].branch.as_deref(), Some("feat/x"));
        assert_eq!(loaded[0].file_stem, task.file_stem);
        assert!(!loaded[0].archived);

        delete_task(&task.file_stem).expect("delete");
        let loaded = load_all_tasks().expect("load after delete");
        assert!(loaded.is_empty());

        let _ = fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var(ENV_DATA_DIR);
        }
    }
}
