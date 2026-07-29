//! App settings (URL templates) persisted as `{config}/settings.json`.

use std::fs;
use std::path::PathBuf;

use color_eyre::eyre::{Context, eyre};
use serde::{Deserialize, Serialize};

use crate::persist;

const SETTINGS_FILE_NAME: &str = "settings.json";

/// Placeholders substituted into URL templates.
pub const ISSUE_ID_PLACEHOLDER: &str = "{issue_id}";
pub const NAMESPACE_PLACEHOLDER: &str = "{namespace}";
pub const REPOSITORY_PLACEHOLDER: &str = "{repository}";
pub const PR_NUMBER_PLACEHOLDER: &str = "{pr_number}";

/// User-configurable settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    /// Template for opening a tracker issue. Use `{issue_id}` where the ID goes.
    #[serde(default)]
    pub issue_url_template: String,
    /// Template for opening a PR. Use `{namespace}`, `{repository}`, `{pr_number}`.
    #[serde(default)]
    pub pr_url_template: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            issue_url_template: String::new(),
            pr_url_template: format!(
                "https://github.com/{NAMESPACE_PLACEHOLDER}/{REPOSITORY_PLACEHOLDER}/pull/{PR_NUMBER_PLACEHOLDER}"
            ),
        }
    }
}

impl Settings {
    /// `{config}/settings.json`
    pub fn path() -> color_eyre::Result<PathBuf> {
        Ok(persist::config_dir()?.join(SETTINGS_FILE_NAME))
    }

    /// Load settings, or defaults if the file is missing.
    pub fn load() -> color_eyre::Result<Self> {
        let path = Self::path()?;
        if !path.is_file() {
            return Ok(Self::default());
        }
        let data = fs::read_to_string(&path)
            .wrap_err_with(|| format!("reading settings {}", path.display()))?;
        serde_json::from_str(&data).wrap_err_with(|| format!("parsing settings {}", path.display()))
    }

    /// Persist settings immediately.
    pub fn save(&self) -> color_eyre::Result<()> {
        let dir = persist::config_dir()?;
        fs::create_dir_all(&dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
        let path = Self::path()?;
        let json = serde_json::to_string_pretty(self).wrap_err("serializing settings")?;
        fs::write(&path, json).wrap_err_with(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Build an issue URL from the template and issue ID.
    pub fn build_issue_url(&self, issue_id: &str) -> color_eyre::Result<String> {
        let tmpl = self.issue_url_template.trim();
        if tmpl.is_empty() {
            return Err(eyre!(
                "issue URL template is empty — set it in Settings (S)"
            ));
        }
        if !tmpl.contains(ISSUE_ID_PLACEHOLDER) {
            return Err(eyre!(
                "issue URL template must contain {ISSUE_ID_PLACEHOLDER}"
            ));
        }
        Ok(tmpl.replace(ISSUE_ID_PLACEHOLDER, issue_id))
    }

    /// Build a PR URL from the template and substitution values.
    pub fn build_pr_url(
        &self,
        namespace: &str,
        repository: &str,
        pr_number: u64,
    ) -> color_eyre::Result<String> {
        let tmpl = self.pr_url_template.trim();
        if tmpl.is_empty() {
            return Err(eyre!("PR URL template is empty — set it in Settings (S)"));
        }
        for ph in [
            NAMESPACE_PLACEHOLDER,
            REPOSITORY_PLACEHOLDER,
            PR_NUMBER_PLACEHOLDER,
        ] {
            if !tmpl.contains(ph) {
                return Err(eyre!("PR URL template must contain {ph}"));
            }
        }
        Ok(tmpl
            .replace(NAMESPACE_PLACEHOLDER, namespace)
            .replace(REPOSITORY_PLACEHOLDER, repository)
            .replace(PR_NUMBER_PLACEHOLDER, &pr_number.to_string()))
    }
}

/// Open a URL in the system default browser.
pub fn open_in_browser(url: &str) -> color_eyre::Result<()> {
    let openers: &[&str] = if cfg!(target_os = "macos") {
        &["open"]
    } else if cfg!(target_os = "windows") {
        &["cmd", "/C", "start", ""]
    } else {
        &["xdg-open"]
    };

    let status = if cfg!(target_os = "windows") {
        std::process::Command::new(openers[0])
            .args(&openers[1..])
            .arg(url)
            .status()
    } else {
        std::process::Command::new(openers[0]).arg(url).status()
    }
    .wrap_err_with(|| format!("launching browser for {url}"))?;

    if !status.success() {
        return Err(eyre!("browser opener exited with {status} for URL {url}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_issue_url() {
        let s = Settings {
            issue_url_template: "https://linear.app/acme/issue/{issue_id}".into(),
            ..Default::default()
        };
        assert_eq!(
            s.build_issue_url("ENG-42").unwrap(),
            "https://linear.app/acme/issue/ENG-42"
        );
    }

    #[test]
    fn builds_pr_url() {
        let s = Settings::default();
        assert_eq!(
            s.build_pr_url("acme", "widgets", 99).unwrap(),
            "https://github.com/acme/widgets/pull/99"
        );
    }

    #[test]
    fn rejects_empty_issue_template() {
        let s = Settings::default();
        assert!(s.build_issue_url("X-1").is_err());
    }
}
