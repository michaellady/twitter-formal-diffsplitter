//! Critical-diff alerting.
//!
//! Stream 2 Phase 3. When the diff comparator records a diff with
//! `severity == "critical"`, the proxy fires every configured `Notifier`
//! concurrently. Notifier failures are logged but never block the request
//! path or the diff record itself — alerting is strictly out-of-band.
//!
//! ## Notifiers
//!
//! * `LogNotifier` — always on. Emits a structured `tracing::warn!` line so
//!   operators tailing the diffsplitter logs see critical diffs even if no
//!   external sink is configured.
//! * `WebhookNotifier` — POSTs a JSON payload to a generic webhook URL.
//!   Compatible with Slack incoming webhooks, Discord, ntfy.sh, custom
//!   bridges, etc. Enabled by `ALERT_WEBHOOK_URL`.
//! * `GitHubPRCommentNotifier` — POSTs to the GitHub Issues comments API
//!   for a tracked PR. Enabled when `ALERT_GH_REPO`, `ALERT_GH_PR`, and
//!   `ALERT_GH_TOKEN` are all set.
//!
//! ## De-dup across restarts
//!
//! The `diffs` table has a `notified_at_ns` column (added in this phase via
//! an idempotent `ALTER TABLE` migration in `db::migrate`). Before firing,
//! the proxy claims the row by setting `notified_at_ns = now()` only when
//! it is currently NULL — that single-statement claim is atomic under
//! SQLite's writer-lock, so two proxy instances or a crash-and-restart
//! cannot double-fire for the same diff.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;

/// The payload handed to every notifier. Constructed by the proxy from the
/// row it just inserted into the `diffs` table.
#[derive(Clone, Debug, Serialize)]
pub struct AlertDiff {
    pub diff_id: i64,
    pub severity: String,
    pub request_path: String,
    pub method: String,
    pub primary_status: Option<u16>,
    pub shadow_status: Option<u16>,
    /// Wall-clock unix-nanos. Same value as `diffs.observed_at_ns`.
    pub observed_at_ns: i64,
}

#[async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, diff: &AlertDiff) -> Result<()>;
    fn name(&self) -> &'static str;
}

/// Always-on stderr fallback. Writes a structured `tracing::warn!` so it
/// shows up in the same log stream as the rest of the proxy.
pub struct LogNotifier;

#[async_trait]
impl Notifier for LogNotifier {
    async fn notify(&self, diff: &AlertDiff) -> Result<()> {
        tracing::warn!(
            target: "diffsplitter::alert",
            diff_id = diff.diff_id,
            severity = %diff.severity,
            method = %diff.method,
            path = %diff.request_path,
            primary_status = ?diff.primary_status,
            shadow_status = ?diff.shadow_status,
            observed_at_ns = diff.observed_at_ns,
            "CRITICAL diff observed"
        );
        Ok(())
    }
    fn name(&self) -> &'static str {
        "log"
    }
}

/// Generic webhook poster. Sends a small JSON document with the same shape
/// as `AlertDiff`. Works with Slack incoming webhooks, Discord, ntfy.sh,
/// and any custom HTTP sink that accepts `application/json`.
pub struct WebhookNotifier {
    pub url: String,
    pub client: reqwest::Client,
}

impl WebhookNotifier {
    pub fn new(url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { url, client }
    }
}

#[async_trait]
impl Notifier for WebhookNotifier {
    async fn notify(&self, diff: &AlertDiff) -> Result<()> {
        let resp = self.client.post(&self.url).json(diff).send().await?;
        let status = resp.status();
        if !status.is_success() {
            // Read at most a small snippet of the response body for the log.
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            anyhow::bail!("webhook returned {status}: {snippet}");
        }
        Ok(())
    }
    fn name(&self) -> &'static str {
        "webhook"
    }
}

/// POSTs a comment to an open GitHub PR via the Issues comments API. The
/// `repo` is "owner/name" (e.g. "michaellady/twitter_formal_diffsplitter")
/// and `pr_number` is the PR number that should accumulate alerts.
pub struct GitHubPRCommentNotifier {
    pub repo: String,
    pub pr_number: u64,
    pub gh_token: String,
    pub client: reqwest::Client,
}

impl GitHubPRCommentNotifier {
    pub fn new(repo: String, pr_number: u64, gh_token: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("diffsplitter-alerting/1")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            repo,
            pr_number,
            gh_token,
            client,
        }
    }

    fn comment_url(&self) -> String {
        format!(
            "https://api.github.com/repos/{}/issues/{}/comments",
            self.repo, self.pr_number
        )
    }

    fn body_markdown(diff: &AlertDiff) -> String {
        format!(
            "**Critical diff observed** (id `{}`)\n\n\
             - severity: `{}`\n\
             - method: `{}`\n\
             - path: `{}`\n\
             - primary_status: `{:?}`\n\
             - shadow_status: `{:?}`\n\
             - observed_at_ns: `{}`\n\n\
             _Posted automatically by the diffsplitter (Stream 2 Phase 3 alerting)._",
            diff.diff_id,
            diff.severity,
            diff.method,
            diff.request_path,
            diff.primary_status,
            diff.shadow_status,
            diff.observed_at_ns,
        )
    }
}

#[derive(Serialize)]
struct GhCommentBody<'a> {
    body: &'a str,
}

#[async_trait]
impl Notifier for GitHubPRCommentNotifier {
    async fn notify(&self, diff: &AlertDiff) -> Result<()> {
        let url = self.comment_url();
        let body = Self::body_markdown(diff);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.gh_token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&GhCommentBody { body: &body })
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let snippet: String = resp.text().await.unwrap_or_default().chars().take(200).collect();
            anyhow::bail!("github api returned {status}: {snippet}");
        }
        Ok(())
    }
    fn name(&self) -> &'static str {
        "github_pr_comment"
    }
}

/// Bundle of configured notifiers. Construction reads env vars exactly
/// once at startup; the proxy holds an `Arc<NotifierSet>` and fans out
/// concurrently per critical diff.
pub struct NotifierSet {
    pub notifiers: Vec<Arc<dyn Notifier>>,
}

impl NotifierSet {
    /// Build a notifier set from environment variables. `LogNotifier` is
    /// always present.
    pub fn from_env() -> Self {
        let mut notifiers: Vec<Arc<dyn Notifier>> = vec![Arc::new(LogNotifier)];

        if let Ok(url) = std::env::var("ALERT_WEBHOOK_URL") {
            if !url.is_empty() {
                tracing::info!(target: "diffsplitter::alert", "WebhookNotifier enabled");
                notifiers.push(Arc::new(WebhookNotifier::new(url)));
            }
        }

        match (
            std::env::var("ALERT_GH_REPO"),
            std::env::var("ALERT_GH_PR"),
            std::env::var("ALERT_GH_TOKEN"),
        ) {
            (Ok(repo), Ok(pr), Ok(tok))
                if !repo.is_empty() && !pr.is_empty() && !tok.is_empty() =>
            {
                if let Ok(pr_num) = pr.parse::<u64>() {
                    tracing::info!(
                        target: "diffsplitter::alert",
                        repo = %repo, pr = pr_num,
                        "GitHubPRCommentNotifier enabled"
                    );
                    notifiers.push(Arc::new(GitHubPRCommentNotifier::new(repo, pr_num, tok)));
                } else {
                    tracing::warn!(
                        target: "diffsplitter::alert",
                        "ALERT_GH_PR is not a valid u64; skipping GitHubPRCommentNotifier"
                    );
                }
            }
            _ => {}
        }

        Self { notifiers }
    }

    /// Fan out concurrently. Errors are logged; the function never returns
    /// an error itself so the caller (proxy hot path) cannot be tripped up.
    pub async fn fire(&self, diff: &AlertDiff) {
        let futs = self.notifiers.iter().map(|n| {
            let n = n.clone();
            let d = diff.clone();
            async move {
                let name = n.name();
                if let Err(e) = n.notify(&d).await {
                    tracing::warn!(
                        target: "diffsplitter::alert",
                        notifier = name,
                        diff_id = d.diff_id,
                        error = %e,
                        "notifier failed"
                    );
                }
            }
        });
        futures_util::future::join_all(futs).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_diff() -> AlertDiff {
        AlertDiff {
            diff_id: 42,
            severity: "critical".to_string(),
            request_path: "/tweets/123".to_string(),
            method: "GET".to_string(),
            primary_status: Some(200),
            shadow_status: Some(500),
            observed_at_ns: 1_700_000_000_000_000_000,
        }
    }

    #[tokio::test]
    async fn log_notifier_succeeds() {
        let n = LogNotifier;
        n.notify(&sample_diff()).await.expect("log notify ok");
        assert_eq!(n.name(), "log");
    }

    #[tokio::test]
    async fn notifier_set_default_only_log() {
        // Don't trust env var presence in CI; explicitly construct minimal set.
        let set = NotifierSet {
            notifiers: vec![Arc::new(LogNotifier)],
        };
        // Should not panic / not error.
        set.fire(&sample_diff()).await;
        assert_eq!(set.notifiers.len(), 1);
    }

    #[test]
    fn gh_comment_url_shape() {
        let n = GitHubPRCommentNotifier::new(
            "owner/repo".to_string(),
            7,
            "ghp_test".to_string(),
        );
        assert_eq!(
            n.comment_url(),
            "https://api.github.com/repos/owner/repo/issues/7/comments"
        );
    }

    #[test]
    fn gh_body_markdown_contains_fields() {
        let body = GitHubPRCommentNotifier::body_markdown(&sample_diff());
        assert!(body.contains("id `42`"));
        assert!(body.contains("severity: `critical`"));
        assert!(body.contains("`/tweets/123`"));
    }
}
