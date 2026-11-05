//! Posts the review comments of a diff view to the open merge request of the
//! current branch.
//!
//! The requests go through the `glab` CLI, which already holds the user's
//! GitLab login and knows which project the repository belongs to.

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{AsyncWriteExt as _, StreamExt as _, stream};
use serde::Deserialize;
use serde_json::json;
use sha1::{Digest as _, Sha1};
use std::path::{Path, PathBuf};
use util::command::{Stdio, new_command};

/// A review comment resolved to a path and a line in the new version of a file.
#[derive(Clone, Debug, PartialEq)]
pub struct GitLabComment {
    /// The editor's id for the comment, so it can be removed once posted.
    pub id: usize,
    /// The path relative to the repository root, with forward slashes.
    pub path: String,
    /// One-based first line of the range in the new version of the file.
    pub start_line: u32,
    /// One-based last line of the range in the new version of the file.
    pub line: u32,
    pub body: String,
}

/// The commits a merge request's diff is between.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct DiffRefs {
    pub base_sha: String,
    pub head_sha: String,
    pub start_sha: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct MergeRequest {
    pub iid: u64,
    pub web_url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<Author>,
    #[serde(default)]
    pub diff_refs: Option<DiffRefs>,
    /// The commit at the tip of the source branch.
    #[serde(default)]
    pub sha: Option<String>,
}

impl MergeRequest {
    /// The project's full path, such as `group/project`, read off the web
    /// URL since the API replies only carry the numeric project id.
    pub fn project_path(&self) -> Option<String> {
        project_path_from_url(&self.web_url)
    }

    /// The tip of the source branch, from `sha` or the diff refs.
    pub fn head_sha(&self) -> Option<&str> {
        self.sha
            .as_deref()
            .or_else(|| self.diff_refs.as_ref().map(|refs| refs.head_sha.as_str()))
    }
}

fn project_path_from_url(web_url: &str) -> Option<String> {
    let (_, rest) = web_url.split_once("://")?;
    let (_, path) = rest.split_once('/')?;
    let (project, _) = path.split_once("/-/")?;
    (!project.is_empty()).then(|| project.to_string())
}

/// An issue the merge request closes or mentions, for telling the model
/// what the change is for.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct RelatedIssue {
    pub iid: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub web_url: String,
}

/// Who has approved the merge request, and whether the current user has.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Approvals {
    pub approved_by: Vec<String>,
    pub user_has_approved: bool,
}

/// The current user's standing as a reviewer of the merge request.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ReviewerState {
    #[default]
    Unreviewed,
    Reviewed,
    RequestedChanges,
    Approved,
}

/// The current user, with their standing as a reviewer of a merge request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReviewerStanding {
    pub name: String,
    pub state: ReviewerState,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Author {
    pub name: String,
}

/// The open merge request whose source branch is `branch`, with its
/// description, for showing above the diff.
pub async fn merge_request_for_branch(work_dir: PathBuf, branch: String) -> Result<MergeRequest> {
    find_merge_request(&work_dir, &branch).await
}

/// A merge request the launcher already fetched and wrote as JSON, which
/// saves two round trips to GitLab at open time.
pub fn merge_request_from_file(file: &Path) -> Result<MergeRequest> {
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("reading the merge request file {}", file.display()))?;
    serde_json::from_str(&text).context("the merge request file is not the expected JSON")
}

/// What came of posting a review: the comments that reached the merge
/// request with the thread each is now on, and the first error among those
/// that did not.
pub struct PostOutcome {
    pub merge_request: MergeRequest,
    pub posted: Vec<PostedComment>,
    pub error: Option<anyhow::Error>,
}

/// A comment that reached the merge request: the editor's id for it, the
/// thread it opened, and the note it became, which editing needs.
#[derive(Clone, Debug, PartialEq)]
pub struct PostedComment {
    pub comment_id: usize,
    pub discussion_id: String,
    pub note_id: Option<u64>,
}

#[derive(Deserialize)]
struct PostedDiscussion {
    id: String,
    #[serde(default)]
    notes: Vec<PostedNote>,
}

#[derive(Deserialize)]
struct PostedNote {
    id: u64,
}

/// How many comments post at once. GitLab copes with a few in flight, and
/// each call costs a round trip, so this hides most of the latency.
const CONCURRENT_POSTS: usize = 4;

/// Posts `comments` as inline discussions on the open merge request whose
/// source branch is `branch`, a few at a time. Every comment is attempted;
/// the ones GitLab rejects stay in the editor.
pub async fn post_review(
    work_dir: PathBuf,
    branch: String,
    comments: Vec<GitLabComment>,
) -> Result<PostOutcome> {
    let merge_request = find_merge_request(&work_dir, &branch).await?;
    let diff_refs = merge_request
        .diff_refs
        .clone()
        .context("the merge request has no diff refs yet; is the branch pushed?")?;

    let endpoint = format!(
        "projects/:id/merge_requests/{}/discussions",
        merge_request.iid
    );
    let results: Vec<Result<PostedComment>> = stream::iter(comments)
        .map(|comment| {
            let body = discussion_body(&comment, &diff_refs).to_string();
            let work_dir = work_dir.clone();
            let endpoint = endpoint.clone();
            async move {
                glab_api(&work_dir, "POST", &endpoint, Some(&body))
                    .await
                    .and_then(|reply| {
                        let discussion: PostedDiscussion = serde_json::from_str(&reply)
                            .context("the discussion reply is not the expected JSON")?;
                        Ok(PostedComment {
                            comment_id: comment.id,
                            discussion_id: discussion.id,
                            note_id: discussion.notes.first().map(|note| note.id),
                        })
                    })
                    .with_context(|| {
                        format!("posting the comment on {}:{}", comment.path, comment.line)
                    })
            }
        })
        .buffer_unordered(CONCURRENT_POSTS)
        .collect()
        .await;

    let mut posted = Vec::new();
    let mut error = None;
    for result in results {
        match result {
            Ok(comment) => posted.push(comment),
            Err(err) => {
                error.get_or_insert(err);
            }
        }
    }
    Ok(PostOutcome {
        merge_request,
        posted,
        error,
    })
}

/// A thread on the merge request's diff, with the lines it is on. The lines
/// count in the new version of the file, or in the old one for a thread on
/// removed lines.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteDiscussion {
    pub id: String,
    pub path: String,
    pub start_line: u32,
    pub line: u32,
    /// The thread is on lines the change removed, so `start_line` and
    /// `line` count in the old version of the file.
    pub old_side: bool,
    pub resolved: bool,
    /// The tip of the branch when the thread was opened.
    pub head_sha: Option<String>,
    /// When the thread was opened, as GitLab's ISO 8601 timestamp.
    pub created_at: Option<String>,
    /// The opening note first, then the replies, without system notes.
    pub notes: Vec<RemoteNote>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteNote {
    /// GitLab's id for the note, needed to edit it.
    pub id: Option<u64>,
    pub author: String,
    pub body: String,
}

/// Replaces the text of the note `note_id` on thread `discussion_id`.
pub async fn update_note(
    work_dir: PathBuf,
    iid: u64,
    discussion_id: String,
    note_id: u64,
    body: String,
) -> Result<()> {
    let endpoint =
        format!("projects/:id/merge_requests/{iid}/discussions/{discussion_id}/notes/{note_id}");
    let request = json!({ "body": body }).to_string();
    glab_api(&work_dir, "PUT", &endpoint, Some(&request)).await?;
    Ok(())
}

/// Adds `body` as a reply on the thread `discussion_id` and returns the
/// note as GitLab stored it.
pub async fn reply_to_discussion(
    work_dir: PathBuf,
    iid: u64,
    discussion_id: String,
    body: String,
) -> Result<RemoteNote> {
    #[derive(Deserialize)]
    struct CreatedNote {
        #[serde(default)]
        id: Option<u64>,
        #[serde(default)]
        body: String,
        #[serde(default)]
        author: Option<Author>,
    }
    let endpoint = format!("projects/:id/merge_requests/{iid}/discussions/{discussion_id}/notes");
    let request = json!({ "body": body }).to_string();
    let reply = glab_api(&work_dir, "POST", &endpoint, Some(&request)).await?;
    let note: CreatedNote =
        serde_json::from_str(&reply).context("the reply note is not the expected JSON")?;
    Ok(RemoteNote {
        id: note.id,
        author: note.author.map(|author| author.name).unwrap_or_default(),
        body: note.body,
    })
}

/// Marks the thread `discussion_id` resolved, or open again.
pub async fn resolve_discussion(
    work_dir: PathBuf,
    iid: u64,
    discussion_id: String,
    resolved: bool,
) -> Result<()> {
    let endpoint = format!("projects/:id/merge_requests/{iid}/discussions/{discussion_id}");
    let request = json!({ "resolved": resolved }).to_string();
    glab_api(&work_dir, "PUT", &endpoint, Some(&request)).await?;
    Ok(())
}

/// Adds `body` as a comment on the merge request itself, not on a line.
pub async fn post_note(work_dir: PathBuf, iid: u64, body: String) -> Result<()> {
    let endpoint = format!("projects/:id/merge_requests/{iid}/notes");
    let request = json!({ "body": body }).to_string();
    glab_api(&work_dir, "POST", &endpoint, Some(&request)).await?;
    Ok(())
}

/// Gives or withdraws the current user's approval.
pub async fn set_approval(work_dir: PathBuf, iid: u64, approve: bool) -> Result<()> {
    let action = if approve { "approve" } else { "unapprove" };
    let endpoint = format!("projects/:id/merge_requests/{iid}/{action}");
    // An empty object keeps glab from sending a body with no content type.
    glab_api(&work_dir, "POST", &endpoint, Some("{}")).await?;
    Ok(())
}

/// Who has approved merge request `iid`.
pub async fn fetch_approvals(work_dir: PathBuf, iid: u64) -> Result<Approvals> {
    let endpoint = format!("projects/:id/merge_requests/{iid}/approvals");
    let text = glab_api(&work_dir, "GET", &endpoint, None).await?;
    parse_approvals(&text)
}

fn parse_approvals(text: &str) -> Result<Approvals> {
    #[derive(Deserialize)]
    struct RawApprovals {
        #[serde(default)]
        approved_by: Vec<RawApprover>,
        #[serde(default)]
        user_has_approved: bool,
    }
    #[derive(Deserialize)]
    struct RawApprover {
        #[serde(default)]
        user: Option<Author>,
    }
    let raw: RawApprovals =
        serde_json::from_str(text).context("the approvals reply is not the expected JSON")?;
    Ok(Approvals {
        approved_by: raw
            .approved_by
            .into_iter()
            .filter_map(|approver| approver.user.map(|user| user.name))
            .collect(),
        user_has_approved: raw.user_has_approved,
    })
}

/// The current user and their reviewer state on merge request `iid`, which
/// is `Unreviewed` when they are not a reviewer at all.
pub async fn fetch_reviewer_standing(work_dir: PathBuf, iid: u64) -> Result<ReviewerStanding> {
    #[derive(Deserialize)]
    struct CurrentUser {
        username: String,
        #[serde(default)]
        name: String,
    }
    #[derive(Deserialize)]
    struct RawReviewer {
        #[serde(default)]
        user: Option<RawUser>,
        #[serde(default)]
        state: String,
    }
    #[derive(Deserialize)]
    struct RawUser {
        username: String,
    }
    let user = glab_api(&work_dir, "GET", "user", None).await?;
    let user: CurrentUser =
        serde_json::from_str(&user).context("the user reply is not the expected JSON")?;
    let endpoint = format!("projects/:id/merge_requests/{iid}/reviewers");
    let text = glab_api(&work_dir, "GET", &endpoint, None).await?;
    let reviewers: Vec<RawReviewer> =
        serde_json::from_str(&text).context("the reviewers reply is not the expected JSON")?;
    let state = reviewers
        .into_iter()
        .find(|reviewer| {
            reviewer
                .user
                .as_ref()
                .is_some_and(|reviewer| reviewer.username == user.username)
        })
        .map(|reviewer| match reviewer.state.as_str() {
            "reviewed" => ReviewerState::Reviewed,
            "requested_changes" => ReviewerState::RequestedChanges,
            "approved" => ReviewerState::Approved,
            _ => ReviewerState::Unreviewed,
        })
        .unwrap_or_default();
    Ok(ReviewerStanding {
        name: user.name,
        state,
    })
}

/// Asks the author for changes, or takes that request back. The REST API
/// has no call for this, so it goes through GraphQL.
pub async fn set_changes_requested(
    work_dir: PathBuf,
    project_path: String,
    iid: u64,
    requested: bool,
) -> Result<()> {
    let mutation = if requested {
        "mergeRequestRequestChanges"
    } else {
        "mergeRequestDestroyRequestedChanges"
    };
    let query = format!(
        "mutation {{ {mutation}(input: {{ projectPath: {}, iid: {} }}) {{ errors }} }}",
        json!(project_path),
        json!(iid.to_string())
    );
    let reply = glab_graphql(&work_dir, &query).await?;
    let reply: serde_json::Value =
        serde_json::from_str(&reply).context("the GraphQL reply is not JSON")?;
    let errors: Vec<String> = reply["data"][mutation]["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|error| error.as_str().map(str::to_string))
        .chain(
            reply["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|error| error["message"].as_str().map(str::to_string)),
        )
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

/// The issues merge request `iid` closes or mentions.
pub async fn fetch_related_issues(work_dir: PathBuf, iid: u64) -> Result<Vec<RelatedIssue>> {
    let endpoint = format!("projects/:id/merge_requests/{iid}/related_issues");
    let text = glab_api(&work_dir, "GET", &endpoint, None).await?;
    serde_json::from_str(&text).context("the related issues reply is not the expected JSON")
}

/// The threads on the diff of merge request `iid`, resolved ones included.
pub async fn fetch_discussions(work_dir: PathBuf, iid: u64) -> Result<Vec<RemoteDiscussion>> {
    let endpoint = format!("projects/:id/merge_requests/{iid}/discussions?per_page=100");
    let text = glab_api(&work_dir, "GET", &endpoint, None).await?;
    parse_discussions(&text)
}

fn parse_discussions(text: &str) -> Result<Vec<RemoteDiscussion>> {
    #[derive(Deserialize)]
    struct RawDiscussion {
        id: String,
        #[serde(default)]
        notes: Vec<RawNote>,
    }
    #[derive(Deserialize)]
    struct RawNote {
        #[serde(default)]
        id: Option<u64>,
        #[serde(default)]
        body: String,
        #[serde(default)]
        system: bool,
        #[serde(default)]
        resolved: bool,
        #[serde(default)]
        author: Option<Author>,
        #[serde(default)]
        created_at: Option<String>,
        #[serde(default)]
        position: Option<RawPosition>,
    }
    #[derive(Deserialize)]
    struct RawPosition {
        #[serde(default)]
        new_path: Option<String>,
        #[serde(default)]
        old_path: Option<String>,
        #[serde(default)]
        new_line: Option<u32>,
        #[serde(default)]
        old_line: Option<u32>,
        #[serde(default)]
        head_sha: Option<String>,
        #[serde(default)]
        line_range: Option<RawLineRange>,
    }
    #[derive(Deserialize)]
    struct RawLineRange {
        #[serde(default)]
        start: Option<RawLineReference>,
    }
    #[derive(Deserialize)]
    struct RawLineReference {
        #[serde(default)]
        new_line: Option<u32>,
        #[serde(default)]
        old_line: Option<u32>,
    }

    let raw: Vec<RawDiscussion> =
        serde_json::from_str(text).context("the discussion list is not the expected JSON")?;
    Ok(raw
        .into_iter()
        .filter_map(|discussion| {
            let notes: Vec<&RawNote> = discussion
                .notes
                .iter()
                .filter(|note| !note.system)
                .collect();
            let first = notes.first()?;
            let position = first.position.as_ref()?;
            let range_start = position
                .line_range
                .as_ref()
                .and_then(|range| range.start.as_ref());
            // A thread on a line the change kept or added has a new line; one
            // on a removed line has only an old line.
            let (path, line, start_line, old_side) = match (position.new_line, position.old_line) {
                (Some(line), _) => (
                    position.new_path.clone()?,
                    line,
                    range_start.and_then(|start| start.new_line),
                    false,
                ),
                (None, Some(line)) => (
                    position
                        .old_path
                        .clone()
                        .or_else(|| position.new_path.clone())?,
                    line,
                    range_start.and_then(|start| start.old_line),
                    true,
                ),
                (None, None) => return None,
            };
            let start_line = start_line.unwrap_or(line).min(line);
            Some(RemoteDiscussion {
                id: discussion.id,
                path,
                start_line,
                line,
                old_side,
                resolved: first.resolved,
                head_sha: position.head_sha.clone(),
                created_at: first.created_at.clone(),
                notes: notes
                    .into_iter()
                    .map(|note| RemoteNote {
                        id: note.id,
                        author: note
                            .author
                            .as_ref()
                            .map(|author| author.name.clone())
                            .unwrap_or_default(),
                        body: note.body.clone(),
                    })
                    .collect(),
            })
        })
        .collect())
}

async fn find_merge_request(work_dir: &Path, branch: &str) -> Result<MergeRequest> {
    let endpoint = format!(
        "projects/:id/merge_requests?state=opened&source_branch={}",
        urlencoding::encode(branch)
    );
    let listed = glab_api(work_dir, "GET", &endpoint, None).await?;
    let iid = first_merge_request_iid(&listed, branch)?;
    // The list omits diff refs, so fetch the merge request itself.
    let detail = glab_api(
        work_dir,
        "GET",
        &format!("projects/:id/merge_requests/{iid}"),
        None,
    )
    .await?;
    serde_json::from_str(&detail).context("the merge request reply is not the expected JSON")
}

fn first_merge_request_iid(listed: &str, branch: &str) -> Result<u64> {
    let merge_requests: Vec<MergeRequest> =
        serde_json::from_str(listed).context("the merge request list is not the expected JSON")?;
    merge_requests
        .first()
        .map(|merge_request| merge_request.iid)
        .ok_or_else(|| anyhow!("no open merge request has `{branch}` as its source branch"))
}

/// The JSON body that creates one inline discussion on the new side of the
/// diff, spanning the comment's lines when it covers more than one.
fn discussion_body(comment: &GitLabComment, diff_refs: &DiffRefs) -> serde_json::Value {
    let mut position = json!({
        "position_type": "text",
        "base_sha": diff_refs.base_sha,
        "head_sha": diff_refs.head_sha,
        "start_sha": diff_refs.start_sha,
        "new_path": comment.path,
        "old_path": comment.path,
        "new_line": comment.line,
    });
    if comment.start_line < comment.line {
        position["line_range"] = json!({
            "start": line_range_end(&comment.path, comment.start_line),
            "end": line_range_end(&comment.path, comment.line),
        });
    }
    json!({ "body": comment.body, "position": position })
}

/// One end of a multi-line position. GitLab wants its line code, which is the
/// SHA-1 of the path joined to the old and new line numbers; for a line on
/// the new side the old number is not checked, so the new one stands in.
fn line_range_end(path: &str, new_line: u32) -> serde_json::Value {
    let path_hash = format!("{:x}", Sha1::digest(path.as_bytes()));
    json!({
        "line_code": format!("{path_hash}_{new_line}_{new_line}"),
        "type": "new",
        "old_line": null,
        "new_line": new_line,
    })
}

/// Runs `glab api` in `work_dir` and returns the response body.
async fn glab_api(
    work_dir: &Path,
    method: &str,
    endpoint: &str,
    body: Option<&str>,
) -> Result<String> {
    let mut command = new_command("glab");
    command
        .args(["api", "--method", method, endpoint])
        .current_dir(work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if body.is_some() {
        // glab sends a raw body with no content type, which GitLab rejects.
        command
            .args(["--header", "Content-Type: application/json", "--input", "-"])
            .stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command
        .spawn()
        .context("could not run `glab`; install it and run `glab auth login`")?;
    if let Some(body) = body {
        let mut stdin = child
            .stdin
            .take()
            .context("could not write the request body to `glab`")?;
        stdin.write_all(body.as_bytes()).await?;
        stdin.close().await?;
    }
    let output = child.output().await.context("waiting for `glab`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "`glab api {endpoint}` failed: {}",
            [stderr.trim(), stdout.trim()]
                .into_iter()
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    String::from_utf8(output.stdout).context("`glab` replied with invalid UTF-8")
}

/// Runs a GraphQL `query` through `glab api graphql` and returns the reply.
async fn glab_graphql(work_dir: &Path, query: &str) -> Result<String> {
    let output = new_command("glab")
        .args(["api", "graphql", "-f"])
        .arg(format!("query={query}"))
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("could not run `glab`; install it and run `glab auth login`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`glab api graphql` failed: {}", stderr.trim());
    }
    String::from_utf8(output.stdout).context("`glab` replied with invalid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discussion_body_places_the_comment_on_the_new_side() {
        let comment = GitLabComment {
            id: 7,
            path: "src/loop.rs".into(),
            start_line: 41,
            line: 41,
            body: "Why not reuse the hunk key?".into(),
        };
        let diff_refs = DiffRefs {
            base_sha: "b".into(),
            head_sha: "h".into(),
            start_sha: "s".into(),
        };

        assert_eq!(
            discussion_body(&comment, &diff_refs),
            json!({
                "body": "Why not reuse the hunk key?",
                "position": {
                    "position_type": "text",
                    "base_sha": "b",
                    "head_sha": "h",
                    "start_sha": "s",
                    "new_path": "src/loop.rs",
                    "old_path": "src/loop.rs",
                    "new_line": 41,
                }
            })
        );
    }

    #[test]
    fn discussion_body_spans_a_multi_line_comment() {
        let comment = GitLabComment {
            id: 7,
            path: "src/loop.rs".into(),
            start_line: 40,
            line: 42,
            body: "This whole loop.".into(),
        };
        let diff_refs = DiffRefs {
            base_sha: "b".into(),
            head_sha: "h".into(),
            start_sha: "s".into(),
        };

        let body = discussion_body(&comment, &diff_refs);

        assert_eq!(body["position"]["new_line"], 42);
        let path_hash = "c494c1745f5fe1910594a89a8bd187439f7a6019";
        assert_eq!(
            body["position"]["line_range"],
            json!({
                "start": {
                    "line_code": format!("{path_hash}_40_40"),
                    "type": "new",
                    "old_line": null,
                    "new_line": 40,
                },
                "end": {
                    "line_code": format!("{path_hash}_42_42"),
                    "type": "new",
                    "old_line": null,
                    "new_line": 42,
                },
            })
        );
    }

    #[test]
    fn discussions_parse_to_threads_on_both_sides_without_system_notes() {
        let text = r#"[
          {"id": "d1", "notes": [
            {"id": 7, "body": "Why?", "author": {"name": "Ada"}, "created_at": "2026-10-19T10:00:00.000Z",
              "position": {"new_path": "src/a.rs", "old_path": "src/a.rs", "new_line": 12, "head_sha": "abc",
              "line_range": {"start": {"new_line": 10}, "end": {"new_line": 12}}}},
            {"body": "changed status", "system": true, "author": {"name": "GitLab"}},
            {"body": "Because.", "author": {"name": "Bob"}}
          ]},
          {"id": "general", "notes": [{"body": "Nice work", "author": {"name": "Ada"}}]},
          {"id": "resolved", "notes": [{"body": "Done", "resolved": true, "author": {"name": "Ada"},
            "position": {"new_path": "src/b.rs", "new_line": 3}}]},
          {"id": "old-side", "notes": [{"body": "Removed", "author": {"name": "Ada"},
            "position": {"new_path": "src/c.rs", "old_path": "src/old_c.rs", "old_line": 7,
            "line_range": {"start": {"old_line": 5}, "end": {"old_line": 7}}}}]}
        ]"#;

        let discussions = parse_discussions(text).unwrap();

        assert_eq!(
            discussions,
            vec![
                RemoteDiscussion {
                    id: "d1".into(),
                    path: "src/a.rs".into(),
                    start_line: 10,
                    line: 12,
                    old_side: false,
                    resolved: false,
                    head_sha: Some("abc".into()),
                    created_at: Some("2026-10-19T10:00:00.000Z".into()),
                    notes: vec![
                        RemoteNote {
                            id: Some(7),
                            author: "Ada".into(),
                            body: "Why?".into(),
                        },
                        RemoteNote {
                            id: None,
                            author: "Bob".into(),
                            body: "Because.".into(),
                        },
                    ],
                },
                RemoteDiscussion {
                    id: "resolved".into(),
                    path: "src/b.rs".into(),
                    start_line: 3,
                    line: 3,
                    old_side: false,
                    resolved: true,
                    head_sha: None,
                    created_at: None,
                    notes: vec![RemoteNote {
                        id: None,
                        author: "Ada".into(),
                        body: "Done".into(),
                    }],
                },
                RemoteDiscussion {
                    id: "old-side".into(),
                    path: "src/old_c.rs".into(),
                    start_line: 5,
                    line: 7,
                    old_side: true,
                    resolved: false,
                    head_sha: None,
                    created_at: None,
                    notes: vec![RemoteNote {
                        id: None,
                        author: "Ada".into(),
                        body: "Removed".into(),
                    }],
                },
            ]
        );
    }

    #[test]
    fn project_path_comes_off_the_web_url() {
        assert_eq!(
            project_path_from_url(
                "https://gitlab.com/example-group/product/platform/-/merge_requests/9"
            ),
            Some("example-group/product/platform".into())
        );
        assert_eq!(project_path_from_url("https://gitlab.com/"), None);
    }

    #[test]
    fn approvals_parse_names_and_the_current_user() {
        let approvals = parse_approvals(
            r#"{"approved_by": [{"user": {"name": "Ada"}}, {"user": {"name": "Bob"}}], "user_has_approved": true}"#,
        )
        .unwrap();
        assert_eq!(
            approvals,
            Approvals {
                approved_by: vec!["Ada".into(), "Bob".into()],
                user_has_approved: true,
            }
        );
    }

    #[test]
    fn first_merge_request_iid_reads_the_list_and_rejects_an_empty_one() {
        let listed = r#"[{"iid": 1565, "web_url": "https://gitlab.com/x/-/merge_requests/1565"}]"#;
        assert_eq!(first_merge_request_iid(listed, "feature").unwrap(), 1565);

        let error = first_merge_request_iid("[]", "feature").unwrap_err();
        assert!(error.to_string().contains("`feature`"));
        assert!(first_merge_request_iid("not json", "feature").is_err());
    }

    #[test]
    fn merge_request_detail_parses_diff_refs() {
        let detail = r#"{
            "iid": 3,
            "web_url": "https://gitlab.com/x/-/merge_requests/3",
            "diff_refs": {"base_sha": "b", "head_sha": "h", "start_sha": "s"},
            "title": "Add a thing",
            "description": "Why the thing.",
            "author": {"name": "Ada", "username": "ada"}
        }"#;
        let merge_request: MergeRequest = serde_json::from_str(detail).unwrap();
        assert_eq!(merge_request.title, "Add a thing");
        assert_eq!(merge_request.description.as_deref(), Some("Why the thing."));
        assert_eq!(
            merge_request.author.as_ref().map(|a| a.name.as_str()),
            Some("Ada")
        );
        assert_eq!(
            merge_request.diff_refs,
            Some(DiffRefs {
                base_sha: "b".into(),
                head_sha: "h".into(),
                start_sha: "s".into(),
            })
        );
    }
}
