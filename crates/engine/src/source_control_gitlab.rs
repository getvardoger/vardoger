//! GitLab merge requests for the pull request lists, through `glab api`
//! (the GitLab CLI, signed in on this device) — GitHub's `gh` counterpart.

use chrono::{DateTime, Utc};
use commutator_proto::change_request_assessment::{ChangeRequestCi, CiState};
use commutator_proto::{
    ChangeRequestActor, ChangeRequestSignals, ChangeRequestSummary, ChangeRequestFilter, ChangeRequestListItem, ChangeRequestMergeability,
    ChangeRequestLabel, ChangeRequestPage, ChangeRequestReviewDecision, ChangeRequestState,
    ChangeRequestStateFilter,
};
use serde::Deserialize;

use super::{
    ChangeRequestError, GITHUB_OUTPUT_LIMIT, GITHUB_TIMEOUT, GitHubCli, ProcessRequest,
    classify_run_error, host_from_authority,
};

/// A GitLab project: its host and full path (`group/sub/project`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLabProject {
    pub host: String,
    pub path: String,
}

/// The project behind a non-GitHub remote, nested groups included.
pub fn parse_remote(remote_url: &str) -> Option<GitLabProject> {
    let remote_url = remote_url.trim();
    let (authority, path) = match remote_url.split_once("://") {
        Some((scheme, rest)) => {
            if !matches!(
                scheme.to_ascii_lowercase().as_str(),
                "ssh" | "http" | "https"
            ) {
                return None;
            }
            rest.split_once('/')?
        }
        None => {
            let (authority, path) = remote_url.split_once(':')?;
            if authority.contains('/') || path.starts_with('/') {
                return None;
            }
            (authority, path)
        }
    };
    let host = host_from_authority(authority)?.to_ascii_lowercase();
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let valid = path.split('/').count() >= 2
        && path.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        });
    (valid && host != "github.com").then(|| GitLabProject {
        host,
        path: path.to_owned(),
    })
}

/// One page of a project's merge requests in `$state`, most recently updated
/// first, with what the board shows of each — as Shepherd's list asks it.
const MERGE_REQUESTS: &str = "query($path: ID!, $state: MergeRequestState, $after: String, $author: String, $reviewer: String) {
  currentUser { username }
  project(fullPath: $path) {
    mergeRequests(state: $state, first: 50, after: $after, sort: UPDATED_DESC, authorUsername: $author, reviewerUsername: $reviewer) {
      count
      pageInfo { hasNextPage endCursor }
      nodes {
        iid title webUrl state draft createdAt updatedAt approved conflicts detailedMergeStatus diffHeadSha sourceBranch
        author { username name avatarUrl }
        labels { nodes { title color } }
        reviewers { nodes { username mergeRequestInteraction { reviewState } } }
        headPipeline { status jobs { count } }
        diffStatsSummary { additions deletions }
      }
    }
  }
}";

/// A branch's merge requests, newest first: the checkout watch's question
/// (`gh pr list --head`'s counterpart).
const BRANCH_MERGE_REQUESTS: &str = "query($path: ID!, $branch: String!) {
  currentUser { username }
  project(fullPath: $path) {
    mergeRequests(sourceBranches: [$branch], state: all, first: 5, sort: UPDATED_DESC) {
      nodes {
        iid title webUrl state draft createdAt updatedAt approved conflicts detailedMergeStatus diffHeadSha sourceBranch targetBranch
        author { username name avatarUrl }
        reviewers { nodes { username mergeRequestInteraction { reviewState } } }
        headPipeline { status jobs { count } }
      }
    }
  }
}";

#[derive(Deserialize)]
struct Graphql<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListData {
    current_user: Option<User>,
    project: Option<Project>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Project {
    merge_requests: Connection<MergeRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    #[serde(default)]
    count: Option<u64>,
    #[serde(default)]
    page_info: Option<PageInfo>,
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeRequest {
    iid: String,
    title: String,
    web_url: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    source_branch: String,
    #[serde(default)]
    target_branch: String,
    #[serde(default)]
    labels: Option<Connection<Label>>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    author: Option<User>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    approved: bool,
    #[serde(default)]
    conflicts: bool,
    #[serde(default)]
    detailed_merge_status: Option<String>,
    #[serde(default)]
    diff_head_sha: Option<String>,
    #[serde(default)]
    reviewers: Option<Connection<Reviewer>>,
    #[serde(default)]
    head_pipeline: Option<Pipeline>,
    #[serde(default)]
    diff_stats_summary: Option<Stats>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct User {
    username: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct Label {
    title: String,
    #[serde(default)]
    color: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reviewer {
    username: String,
    #[serde(default)]
    merge_request_interaction: Option<Interaction>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Interaction {
    #[serde(default)]
    review_state: Option<String>,
}

#[derive(Deserialize)]
struct Pipeline {
    status: String,
    #[serde(default)]
    jobs: Option<Count>,
}

#[derive(Deserialize)]
struct Count {
    #[serde(default)]
    count: u64,
}

#[derive(Deserialize)]
struct Stats {
    additions: u64,
    deletions: u64,
}

impl GitHubCli {
    /// Whether `project`'s host is a GitLab this device can reach: named so,
    /// or signed in to with `glab`.
    /// The `glab auth status` answer is kept with the list requests (5 min).
    pub async fn is_gitlab(&self, project: &GitLabProject) -> bool {
        if project.host.contains("gitlab") {
            return true;
        }
        let request = glab_request(vec![
            "auth".into(),
            "status".into(),
            "--hostname".into(),
            project.host.clone(),
        ]);
        let runner = self.runner.clone();
        self.cached_pr_request(format!("gitlab:auth:{}", project.host), false, async move {
            let signed_in = runner.run(request).await.is_ok_and(|output| output.success);
            Ok(serde_json::Value::Bool(signed_in))
        })
        .await
        .is_ok_and(|value| value == serde_json::Value::Bool(true))
    }

    /// The open merge requests of `project`, cached as GitHub's list is: in
    /// memory (shared in-flight reads, 5 min; 15 s after a refresh) and the
    /// first page on disk, which `cached` answers whatever its age.
    pub async fn list_gitlab_page(
        &self,
        project: &GitLabProject,
        filter: ChangeRequestFilter,
        state: ChangeRequestStateFilter,
        after: Option<&str>,
        refresh: bool,
        cached: bool,
    ) -> Result<ChangeRequestPage, ChangeRequestError> {
        if after.is_some_and(|cursor| !super::valid_page_cursor(cursor)) {
            return Err(ChangeRequestError::Decode);
        }
        // ponytail: stored per host, not per glab account; key by the viewer if accounts get switched.
        let account = format!("gitlab:{}", project.host);
        let store = self.store.clone().filter(|_| after.is_none());
        if cached
            && !refresh
            && let Some(store) = &store
            && let Some(page) = store.load(&account, &project.path, filter, state).await
        {
            return Ok(page);
        }
        let github = self.clone();
        let fetched = project.clone();
        let cursor = after.map(str::to_owned);
        // ponytail: a GitLab 429 also pauses GitHub reads for 15 min (one shared rate-limit flag).
        let value = self
            .cached_pr_request(
                format!(
                    "gitlab:list:{}/{}:{filter:?}:{state:?}:{}",
                    project.host,
                    project.path.to_ascii_lowercase(),
                    after.unwrap_or_default()
                ),
                refresh,
                async move {
                    let page = github.fetch_gitlab_page(&fetched, filter, state, cursor.as_deref()).await?;
                    serde_json::to_value(page).map_err(|_| ChangeRequestError::Decode)
                },
            )
            .await?;
        let page: ChangeRequestPage =
            serde_json::from_value(value).map_err(|_| ChangeRequestError::Decode)?;
        if let Some(store) = store {
            store.save(&account, &project.path, filter, state, &page).await;
        }
        Ok(page)
    }

    /// The signed-in `glab` user on `project`'s host, which Mine and
    /// Reviewing filter by; kept with the list requests (5 min).
    async fn gitlab_viewer(&self, project: &GitLabProject) -> Result<String, ChangeRequestError> {
        let github = self.clone();
        let project = project.clone();
        let key = format!("gitlab:viewer:{}", project.host);
        let value = self
            .cached_pr_request(key, false, async move {
                let data: ListData = github
                    .glab_graphql(&project, "query { currentUser { username } }", &[])
                    .await?;
                let me = data.current_user.ok_or(ChangeRequestError::Authentication)?;
                Ok(serde_json::Value::String(me.username))
            })
            .await?;
        value.as_str().map(str::to_owned).ok_or(ChangeRequestError::Decode)
    }

    async fn fetch_gitlab_page(
        &self,
        project: &GitLabProject,
        filter: ChangeRequestFilter,
        state: ChangeRequestStateFilter,
        after: Option<&str>,
    ) -> Result<ChangeRequestPage, ChangeRequestError> {
        let state = match state {
            ChangeRequestStateFilter::Open => "opened",
            ChangeRequestStateFilter::Merged => "merged",
            ChangeRequestStateFilter::Closed => "closed",
            ChangeRequestStateFilter::All => "all",
        };
        let mut vars = vec![("path", project.path.clone()), ("state", state.to_owned())];
        if let Some(after) = after {
            vars.push(("after", after.to_owned()));
        }
        match filter {
            ChangeRequestFilter::Authored => vars.push(("author", self.gitlab_viewer(project).await?)),
            ChangeRequestFilter::Reviewing => vars.push(("reviewer", self.gitlab_viewer(project).await?)),
            ChangeRequestFilter::All => {}
        }
        let data: ListData = self.glab_graphql(project, MERGE_REQUESTS, &vars).await?;
        page(project, data)
    }

    /// The merge request of a checkout's branch: the open one, else the
    /// latest. Asked fresh each time; the checkout watch keeps its own TTL.
    pub async fn find_gitlab_for_branch(
        &self,
        project: &GitLabProject,
        branches: &[String],
    ) -> Result<Option<ChangeRequestSummary>, ChangeRequestError> {
        for branch in branches {
            let vars = [("path", project.path.clone()), ("branch", branch.clone())];
            let data: ListData = self.glab_graphql(project, BRANCH_MERGE_REQUESTS, &vars).await?;
            if let Some(found) = branch_summary(project, data)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    async fn glab_graphql<T: serde::de::DeserializeOwned>(
        &self,
        project: &GitLabProject,
        query: &str,
        vars: &[(&str, String)],
    ) -> Result<T, ChangeRequestError> {
        let mut args: Vec<String> = vec![
            "api".into(),
            "graphql".into(),
            "--hostname".into(),
            project.host.clone(),
            "-f".into(),
            format!("query={query}"),
        ];
        for (name, value) in vars {
            args.extend(["-f".into(), format!("{name}={value}")]);
        }
        let response: Graphql<T> = self.glab_json(args).await?;
        if !response.errors.is_empty() {
            return Err(ChangeRequestError::CommandFailed);
        }
        response.data.ok_or(ChangeRequestError::Decode)
    }

    async fn glab_json<T: serde::de::DeserializeOwned>(
        &self,
        args: Vec<String>,
    ) -> Result<T, ChangeRequestError> {
        let request = glab_request(args);
        let output = self.runner.run(request).await.map_err(classify_run_error)?;
        if !output.success {
            return Err(classify_failure(&output.stderr));
        }
        if output.stdout_truncated {
            return Err(ChangeRequestError::Decode);
        }
        serde_json::from_slice(&output.stdout).map_err(|_| ChangeRequestError::Decode)
    }
}

fn glab_request(args: Vec<String>) -> ProcessRequest {
    ProcessRequest {
        program: "glab".into(),
        args,
        stdin: None,
        cwd: None,
        env: vec![("NO_PROMPT".into(), "1".into())],
        timeout: GITHUB_TIMEOUT,
        output_limit: GITHUB_OUTPUT_LIMIT,
    }
}

fn classify_failure(stderr: &[u8]) -> ChangeRequestError {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if stderr.contains("429") || stderr.contains("rate limit") {
        ChangeRequestError::RateLimited
    } else if stderr.contains("401")
        || stderr.contains("unauthorized")
        || stderr.contains("glab auth login")
        || stderr.contains("not logged")
    {
        ChangeRequestError::Authentication
    } else {
        ChangeRequestError::CommandFailed
    }
}

/// The board's page from a `MERGE_REQUESTS` answer.
fn page(project: &GitLabProject, data: ListData) -> Result<ChangeRequestPage, ChangeRequestError> {
    // No project: it doesn't exist, or this account can't see it.
    let requests = data.project.ok_or(ChangeRequestError::UnsupportedRepository)?.merge_requests;
    let me = data.current_user.map(|user| user.username);
    let next_cursor = requests
        .page_info
        .filter(|info| info.has_next_page)
        .and_then(|info| info.end_cursor);
    Ok(ChangeRequestPage {
        items: requests
            .nodes
            .into_iter()
            .filter_map(|mr| item(project, me.as_deref(), mr))
            .collect(),
        next_cursor,
        total_count: requests.count,
        fetched_at: Some(Utc::now()),
    })
}

/// The open merge request of a `BRANCH_MERGE_REQUESTS` answer, else its
/// latest, as a checkout's summary.
fn branch_summary(
    project: &GitLabProject,
    data: ListData,
) -> Result<Option<ChangeRequestSummary>, ChangeRequestError> {
    let mut nodes = data.project.ok_or(ChangeRequestError::UnsupportedRepository)?.merge_requests.nodes;
    let me = data.current_user.map(|user| user.username);
    let pick = nodes.iter().position(|mr| mr.state == "opened").unwrap_or(0);
    if nodes.is_empty() {
        return Ok(None);
    }
    let mr = nodes.swap_remove(pick);
    let base_ref = mr.target_branch.clone();
    let Some(item) = item(project, me.as_deref(), mr) else {
        return Ok(None);
    };
    let open = item.state == ChangeRequestState::Open;
    Ok(Some(ChangeRequestSummary {
        provider: item.provider,
        number: item.number,
        title: item.title,
        url: item.url,
        state: item.state,
        base_ref,
        head_ref: item.head_ref_name,
        signals: open.then(|| ChangeRequestSignals {
            review_decision: item.review_decision,
            review_requested_of_viewer: item.viewer_review_requested,
            ci: item.ci.state,
        }),
    }))
}

fn item(project: &GitLabProject, me: Option<&str>, mr: MergeRequest) -> Option<ChangeRequestListItem> {
    let (author, author_name, avatar) = mr
        .author
        .map(|a| (a.username, a.name.unwrap_or_default(), a.avatar_url.unwrap_or_default()))
        .unwrap_or_default();
    // A self-hosted GitLab gives its own avatars as a path.
    let avatar = if avatar.starts_with('/') { format!("https://{}{avatar}", project.host) } else { avatar };
    let reviewers = mr.reviewers.map(|r| r.nodes).unwrap_or_default();
    let changes_requested = reviewers.iter().any(|r| {
        r.merge_request_interaction
            .as_ref()
            .and_then(|i| i.review_state.as_deref())
            == Some("REQUESTED_CHANGES")
    });
    let review_decision = if changes_requested {
        ChangeRequestReviewDecision::ChangesRequested
    } else if mr.approved {
        ChangeRequestReviewDecision::Approved
    } else if !reviewers.is_empty() {
        ChangeRequestReviewDecision::ReviewRequired
    } else {
        ChangeRequestReviewDecision::Unknown
    };
    let ci = match mr.head_pipeline {
        None => ChangeRequestCi { state: CiState::NoChecks, total_count: 0 },
        Some(pipeline) => ChangeRequestCi {
            state: match pipeline.status.as_str() {
                "SUCCESS" => CiState::Passed,
                "FAILED" | "CANCELED" => CiState::Failed,
                "SKIPPED" => CiState::Skipped,
                "RUNNING" | "PENDING" | "CREATED" | "PREPARING" | "WAITING_FOR_RESOURCE"
                | "SCHEDULED" | "MANUAL" => CiState::Pending,
                _ => CiState::Unknown,
            },
            total_count: pipeline.jobs.map_or(0, |jobs| jobs.count),
        },
    };
    let stats = mr.diff_stats_summary;
    Some(ChangeRequestListItem {
        provider: "gitlab".into(),
        viewer_did_author: me.map(|me| me == author),
        viewer_review_requested: me.map(|me| reviewers.iter().any(|r| r.username == me)),
        author: ChangeRequestActor { login: author, avatar_url: avatar, name: author_name },
        repository: project.path.clone(),
        head_ref_oid: mr.diff_head_sha.unwrap_or_default(),
        ci,
        number: mr.iid.parse().ok()?,
        title: mr.title.split_whitespace().collect::<Vec<_>>().join(" "),
        url: mr.web_url,
        state: match mr.state.as_str() {
            "merged" => ChangeRequestState::Merged,
            "closed" => ChangeRequestState::Closed,
            _ => ChangeRequestState::Open,
        },
        is_draft: mr.draft,
        review_decision,
        additions: stats.as_ref().map_or(0, |s| s.additions),
        deletions: stats.as_ref().map_or(0, |s| s.deletions),
        // `conflicts` is the last check; the detailed status says CONFLICT once the target moves on.
        mergeability: if mr.conflicts || mr.detailed_merge_status.as_deref() == Some("CONFLICT") {
            ChangeRequestMergeability::Conflicting
        } else if mr.detailed_merge_status.as_deref() == Some("MERGEABLE") {
            ChangeRequestMergeability::Mergeable
        } else {
            ChangeRequestMergeability::Unknown
        },
        created_at: mr.created_at,
        updated_at: mr.updated_at,
        labels: mr
            .labels
            .map(|l| l.nodes)
            .unwrap_or_default()
            .into_iter()
            .map(|l| ChangeRequestLabel {
                name: l.title,
                color: l.color.unwrap_or_default().trim_start_matches('#').to_owned(),
            })
            .collect(),
        head_ref_name: mr.source_branch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_keep_nested_groups_and_skip_github() {
        let want = Some(GitLabProject {
            host: "gitlab.com".into(),
            path: "acme/platform/app".into(),
        });
        assert_eq!(parse_remote("git@gitlab.com:acme/platform/app.git"), want);
        assert_eq!(
            parse_remote("https://oauth2:t@gitlab.com/acme/platform/app/"),
            want
        );
        assert_eq!(parse_remote("https://github.com/acme/app"), None);
        assert_eq!(parse_remote("https://gitlab.com/app"), None);
    }

    #[test]
    fn merge_requests_become_list_items() {
        let data: ListData = serde_json::from_value(serde_json::json!({
            "currentUser": { "username": "me" },
            "project": { "mergeRequests": {
                "count": 51,
                "pageInfo": { "hasNextPage": true, "endCursor": "eyJpZCI6IjEifQ" },
                "nodes": [{
                    "iid": "7", "title": "Fix  it", "webUrl": "https://gitlab.com/a/b/-/merge_requests/7",
                    "state": "merged", "sourceBranch": "fix", "labels": { "nodes": [{ "title": "bug", "color": "#d9534f" }] },
                    "draft": false, "author": { "username": "ann", "avatarUrl": "/uploads/a.png" },
                    "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-02T00:00:00.000+03:00",
                    "approved": true, "conflicts": false, "detailedMergeStatus": "CONFLICT", "diffHeadSha": "abc",
                    "reviewers": { "nodes": [
                        { "username": "me", "mergeRequestInteraction": { "reviewState": "REQUESTED_CHANGES" } }
                    ] },
                    "headPipeline": { "status": "RUNNING", "jobs": { "count": 17 } },
                    "diffStatsSummary": { "additions": 3, "deletions": 1 }
                }]
            } }
        }))
        .unwrap();
        let project = parse_remote("git@gitlab.com:a/b.git").unwrap();
        let page = page(&project, data).unwrap();
        assert_eq!((page.next_cursor.as_deref(), page.total_count), (Some("eyJpZCI6IjEifQ"), Some(51)));
        let item = &page.items[0];
        assert_eq!(
            (item.number, item.title.as_str(), item.author.login.as_str(), item.repository.as_str()),
            (7, "Fix it", "ann", "a/b")
        );
        assert_eq!((item.viewer_did_author, item.viewer_review_requested), (Some(false), Some(true)));
        assert_eq!(item.mergeability, ChangeRequestMergeability::Conflicting);
        assert_eq!(item.review_decision, ChangeRequestReviewDecision::ChangesRequested);
        assert_eq!((item.ci.state, item.ci.total_count), (CiState::Pending, 17));
        assert_eq!((item.additions, item.deletions, item.head_ref_oid.as_str()), (3, 1, "abc"));
        assert_eq!((item.state, item.head_ref_name.as_str()), (ChangeRequestState::Merged, "fix"));
        assert_eq!(item.labels, [ChangeRequestLabel { name: "bug".into(), color: "d9534f".into() }]);
        assert_eq!(item.author.avatar_url, "https://gitlab.com/uploads/a.png");
    }

    #[test]
    fn a_branch_resolves_to_its_open_merge_request() {
        let project = GitLabProject { host: "gitlab.com".into(), path: "a/b/c".into() };
        let mr = |iid: &str, state: &str| serde_json::json!({
            "iid": iid, "title": "T", "webUrl": format!("https://gitlab.com/a/b/c/-/merge_requests/{iid}"),
            "state": state, "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            "sourceBranch": "feat", "targetBranch": "main", "headPipeline": { "status": "FAILED" }
        });
        let data: ListData = serde_json::from_value(serde_json::json!({
            "currentUser": { "username": "me" },
            "project": { "mergeRequests": { "nodes": [mr("9", "closed"), mr("4", "opened")] } }
        }))
        .unwrap();
        let found = branch_summary(&project, data).unwrap().unwrap();
        assert_eq!((found.provider.as_str(), found.number, found.base_ref.as_str()), ("gitlab", 4, "main"));
        assert_eq!(found.signals.unwrap().ci, CiState::Failed);
        let none: ListData = serde_json::from_value(serde_json::json!({
            "project": { "mergeRequests": { "nodes": [] } }
        }))
        .unwrap();
        assert_eq!(branch_summary(&project, none).unwrap(), None);
    }
}
