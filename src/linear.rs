//! Linear GraphQL API client (issue lookup by identifier).

use color_eyre::eyre::{Context, eyre};
use serde::Deserialize;
use serde_json::json;
use std::fmt;

const LINEAR_GRAPHQL_URL: &str = "https://api.linear.app/graphql";

/// Issue fields needed when creating a task from a Linear identifier.
#[derive(Debug, Clone)]
pub struct LinearIssue {
    pub identifier: String,
    pub title: String,
}

/// Failure looking up a Linear issue.
#[derive(Debug)]
pub enum IssueLookupError {
    /// HTTP 401/403 — API key is missing, revoked, or wrong.
    Unauthorized,
    /// Any other lookup failure (not found, network, GraphQL, etc.).
    Other(color_eyre::Report),
}

impl fmt::Display for IssueLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "Linear API key was rejected (HTTP 401/403)"),
            Self::Other(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for IssueLookupError {}

impl From<color_eyre::Report> for IssueLookupError {
    fn from(err: color_eyre::Report) -> Self {
        Self::Other(err)
    }
}

#[derive(Debug, Deserialize)]
struct GraphqlResponse {
    data: Option<GraphqlData>,
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphqlData {
    issue: Option<IssueNode>,
    issues: Option<IssueConnection>,
}

#[derive(Debug, Deserialize)]
struct IssueConnection {
    nodes: Vec<IssueNode>,
}

#[derive(Debug, Deserialize)]
struct IssueNode {
    title: String,
    identifier: String,
    #[serde(default)]
    attachments: Option<AttachmentConnection>,
}

#[derive(Debug, Deserialize)]
struct AttachmentConnection {
    nodes: Vec<AttachmentNode>,
}

#[derive(Debug, Deserialize)]
struct AttachmentNode {
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
}

/// Look up a Linear issue by human identifier (`TEAM-123`).
///
/// Tries `issue(id:)` first (Linear accepts identifiers), then falls back to
/// filtering by team key + number.
pub fn fetch_issue_by_identifier(
    api_key: &str,
    identifier: &str,
) -> Result<LinearIssue, IssueLookupError> {
    let (team_key, number) = parse_identifier(identifier).map_err(IssueLookupError::Other)?;

    match fetch_via_issue_id(api_key, identifier) {
        Ok(issue) => Ok(issue),
        Err(IssueLookupError::Unauthorized) => Err(IssueLookupError::Unauthorized),
        Err(err) => {
            // Fall through to filter; keep the first error if filter also fails.
            match fetch_via_filter(api_key, &team_key, number) {
                Ok(issue) => Ok(issue),
                Err(IssueLookupError::Unauthorized) => Err(IssueLookupError::Unauthorized),
                Err(filter_err) => Err(IssueLookupError::Other(eyre!(
                    "Linear lookup for {identifier} failed ({err}); filter fallback also failed: {filter_err}"
                ))),
            }
        }
    }
}

/// A GitHub PR linked to a Linear issue (from attachments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedPr {
    /// Repository name segment from the PR URL (e.g. `widgets` in `org/widgets`).
    pub repository: String,
    pub number: u64,
}

/// Look up GitHub-style PRs linked to a Linear issue (via attachments).
///
/// Returns an empty vec when the issue exists but has no recognizable PR attachments.
pub fn fetch_linked_prs_for_issue(
    api_key: &str,
    identifier: &str,
) -> Result<Vec<LinkedPr>, IssueLookupError> {
    let (team_key, number) = parse_identifier(identifier).map_err(IssueLookupError::Other)?;

    match fetch_attachments_via_issue_id(api_key, identifier) {
        Ok(urls) => Ok(linked_prs_from_urls(&urls)),
        Err(IssueLookupError::Unauthorized) => Err(IssueLookupError::Unauthorized),
        Err(err) => match fetch_attachments_via_filter(api_key, &team_key, number) {
            Ok(urls) => Ok(linked_prs_from_urls(&urls)),
            Err(IssueLookupError::Unauthorized) => Err(IssueLookupError::Unauthorized),
            Err(filter_err) => Err(IssueLookupError::Other(eyre!(
                "Linear PR lookup for {identifier} failed ({err}); filter fallback also failed: {filter_err}"
            ))),
        },
    }
}

fn parse_identifier(identifier: &str) -> color_eyre::Result<(String, i64)> {
    let Some((team, num)) = identifier.rsplit_once('-') else {
        return Err(eyre!("invalid Linear identifier: {identifier}"));
    };
    let number: i64 = num
        .parse()
        .wrap_err_with(|| format!("invalid issue number in {identifier}"))?;
    Ok((team.to_string(), number))
}

fn fetch_via_issue_id(api_key: &str, identifier: &str) -> Result<LinearIssue, IssueLookupError> {
    let query = r#"
        query Issue($id: String!) {
            issue(id: $id) {
                title
                identifier
            }
        }
    "#;
    let body = json!({
        "query": query,
        "variables": { "id": identifier },
    });
    let response = graphql_post(api_key, &body)?;
    check_graphql_errors(&response)?;
    let issue = response
        .data
        .and_then(|d| d.issue)
        .ok_or_else(|| IssueLookupError::Other(eyre!("issue not found")))?;
    Ok(LinearIssue {
        identifier: issue.identifier,
        title: issue.title,
    })
}

fn fetch_via_filter(
    api_key: &str,
    team_key: &str,
    number: i64,
) -> Result<LinearIssue, IssueLookupError> {
    let query = r#"
        query IssueByTeamNumber($teamKey: String!, $number: Float!) {
            issues(
                filter: {
                    number: { eq: $number }
                    team: { key: { eq: $teamKey } }
                }
                first: 1
            ) {
                nodes {
                    title
                    identifier
                }
            }
        }
    "#;
    let body = json!({
        "query": query,
        "variables": {
            "teamKey": team_key,
            "number": number as f64,
        },
    });
    let response = graphql_post(api_key, &body)?;
    check_graphql_errors(&response)?;
    let nodes = response
        .data
        .and_then(|d| d.issues)
        .map(|c| c.nodes)
        .unwrap_or_default();
    let issue = nodes.into_iter().next().ok_or_else(|| {
        IssueLookupError::Other(eyre!("no issue matched team {team_key} number {number}"))
    })?;
    Ok(LinearIssue {
        identifier: issue.identifier,
        title: issue.title,
    })
}

fn fetch_attachments_via_issue_id(
    api_key: &str,
    identifier: &str,
) -> Result<Vec<String>, IssueLookupError> {
    let query = r#"
        query IssueAttachments($id: String!) {
            issue(id: $id) {
                title
                identifier
                attachments {
                    nodes {
                        url
                    }
                }
            }
        }
    "#;
    let body = json!({
        "query": query,
        "variables": { "id": identifier },
    });
    let response = graphql_post(api_key, &body)?;
    check_graphql_errors(&response)?;
    let issue = response
        .data
        .and_then(|d| d.issue)
        .ok_or_else(|| IssueLookupError::Other(eyre!("issue not found")))?;
    Ok(attachment_urls(&issue))
}

fn fetch_attachments_via_filter(
    api_key: &str,
    team_key: &str,
    number: i64,
) -> Result<Vec<String>, IssueLookupError> {
    let query = r#"
        query IssueAttachmentsByTeamNumber($teamKey: String!, $number: Float!) {
            issues(
                filter: {
                    number: { eq: $number }
                    team: { key: { eq: $teamKey } }
                }
                first: 1
            ) {
                nodes {
                    title
                    identifier
                    attachments {
                        nodes {
                            url
                        }
                    }
                }
            }
        }
    "#;
    let body = json!({
        "query": query,
        "variables": {
            "teamKey": team_key,
            "number": number as f64,
        },
    });
    let response = graphql_post(api_key, &body)?;
    check_graphql_errors(&response)?;
    let nodes = response
        .data
        .and_then(|d| d.issues)
        .map(|c| c.nodes)
        .unwrap_or_default();
    let issue = nodes.into_iter().next().ok_or_else(|| {
        IssueLookupError::Other(eyre!("no issue matched team {team_key} number {number}"))
    })?;
    Ok(attachment_urls(&issue))
}

fn attachment_urls(issue: &IssueNode) -> Vec<String> {
    issue
        .attachments
        .as_ref()
        .map(|c| {
            c.nodes
                .iter()
                .filter_map(|a| a.url.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn linked_prs_from_urls(urls: &[String]) -> Vec<LinkedPr> {
    urls.iter().filter_map(|url| parse_github_pr(url)).collect()
}

/// Extract `(repository, pr_number)` from a GitHub pull request URL.
pub fn parse_github_pr(url: &str) -> Option<LinkedPr> {
    // .../namespace/repository/pull/123
    let lower = url.to_ascii_lowercase();
    let idx = lower.find("/pull/")?;
    let before = &url[..idx];
    let after = &url[idx + "/pull/".len()..];
    let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if num.is_empty() {
        return None;
    }
    let number: u64 = num.parse().ok()?;
    let repository = before
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())?
        .to_string();
    Some(LinkedPr { repository, number })
}

fn check_graphql_errors(response: &GraphqlResponse) -> Result<(), IssueLookupError> {
    if let Some(errors) = &response.errors {
        let msg = errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        if looks_like_auth_graphql(&msg) {
            return Err(IssueLookupError::Unauthorized);
        }
        return Err(IssueLookupError::Other(eyre!("GraphQL errors: {msg}")));
    }
    Ok(())
}

fn looks_like_auth_graphql(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("authentication")
        || lower.contains("unauthorized")
        || lower.contains("not authenticated")
        || lower.contains("invalid api key")
        || lower.contains("api key")
}

fn graphql_post(
    api_key: &str,
    body: &serde_json::Value,
) -> Result<GraphqlResponse, IssueLookupError> {
    let response = match ureq::post(LINEAR_GRAPHQL_URL)
        .header("Authorization", api_key)
        .header("Content-Type", "application/json")
        .send_json(body)
    {
        Ok(response) => response,
        // ureq 3 treats non-2xx as Err(StatusCode) by default — that is how Linear 401 arrives.
        Err(ureq::Error::StatusCode(401 | 403)) => {
            return Err(IssueLookupError::Unauthorized);
        }
        Err(ureq::Error::StatusCode(code)) => {
            return Err(IssueLookupError::Other(eyre!(
                "calling Linear GraphQL API: http status: {code}"
            )));
        }
        Err(err) => {
            return Err(IssueLookupError::Other(eyre!(
                "calling Linear GraphQL API: {err:#}"
            )));
        }
    };

    let status = response.status();
    let code = status.as_u16();
    if code == 401 || code == 403 {
        return Err(IssueLookupError::Unauthorized);
    }
    if !status.is_success() {
        let text = response
            .into_body()
            .read_to_string()
            .unwrap_or_else(|_| String::new());
        return Err(IssueLookupError::Other(eyre!(
            "Linear HTTP {status}: {text}"
        )));
    }

    response
        .into_body()
        .read_json::<GraphqlResponse>()
        .map_err(|err| IssueLookupError::Other(eyre!("decoding Linear GraphQL response: {err:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_pr_urls() {
        assert_eq!(
            parse_github_pr("https://github.com/acme/widgets/pull/42"),
            Some(LinkedPr {
                repository: "widgets".into(),
                number: 42,
            })
        );
        assert_eq!(
            parse_github_pr("https://github.com/acme/widgets/pull/99/files"),
            Some(LinkedPr {
                repository: "widgets".into(),
                number: 99,
            })
        );
        assert_eq!(
            parse_github_pr("https://github.com/acme/widgets/issues/42"),
            None
        );
    }
}
