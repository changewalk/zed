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
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
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

    // GitLab places a comment by its line in the merge request's own diff,
    // between base_sha and head_sha. The view shows the file as it is on
    // disk, which may be ahead of, behind, or edited since head_sha, so each
    // line is mapped onto head_sha and checked against that diff first.
    ensure_commits(&work_dir, &[&diff_refs.base_sha, &diff_refs.head_sha]).await?;
    let mut files: HashMap<String, FileOnGitLab> = HashMap::new();
    for comment in &comments {
        if !files.contains_key(&comment.path) {
            let file = FileOnGitLab::load(&work_dir, &diff_refs, &comment.path).await?;
            files.insert(comment.path.clone(), file);
        }
    }
    let mut placed = Vec::new();
    let mut unplaced = Vec::new();
    for comment in comments {
        match files[&comment.path].place(&comment, &diff_refs) {
            Ok(body) => placed.push((comment, body)),
            Err(reason) => unplaced.push(reason),
        }
    }

    let endpoint = format!(
        "projects/:id/merge_requests/{}/discussions",
        merge_request.iid
    );
    let results: Vec<Result<PostedComment>> = stream::iter(placed)
        .map(|(comment, body)| {
            let body = body.to_string();
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
    if !unplaced.is_empty() {
        let unplaced = anyhow!(
            "{} could not be placed on the merge request: {}",
            if unplaced.len() == 1 {
                "one comment".to_string()
            } else {
                format!("{} comments", unplaced.len())
            },
            unplaced.join("; ")
        );
        error = Some(match error {
            Some(error) => anyhow!("{error:#}. Also, {unplaced}"),
            None => unplaced,
        });
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
/// Runs git in `work_dir` and returns what it printed.
async fn git(work_dir: &Path, args: &[&str]) -> Result<String> {
    let output = new_command("git")
        .args(args)
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Fetches any of `shas` the clone does not have yet, so the merge request's
/// diff can be read locally.
async fn ensure_commits(work_dir: &Path, shas: &[&str]) -> Result<()> {
    for sha in shas {
        let commit = format!("{sha}^{{commit}}");
        if git(work_dir, &["cat-file", "-e", &commit]).await.is_ok() {
            continue;
        }
        git(work_dir, &["fetch", "--quiet", "origin", sha])
            .await
            .with_context(|| format!("the merge request's commit {sha} is not in this clone"))?;
    }
    Ok(())
}

/// A hunk header of a unified diff: the old and new line ranges it covers.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Hunk {
    old_start: u32,
    old_len: u32,
    new_start: u32,
    new_len: u32,
}

/// Reads `@@ -a,b +c,d @@`; a range without a count covers one line.
fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |text: &str| -> Option<(u32, u32)> {
        match text.split_once(',') {
            Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
            None => Some((text.parse().ok()?, 1)),
        }
    };
    let (old_start, old_len) = range(old)?;
    let (new_start, new_len) = range(new)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
    })
}

/// A line of the merge request's diff, keyed by its number in the new file.
#[derive(Clone, Copy, Debug, PartialEq)]
struct DiffLine {
    /// The line's number in the old file when it is unchanged; an added line
    /// has none.
    old_line: Option<u32>,
    /// The old side's line counter when the diff reached this line, which
    /// GitLab's line codes carry for added lines too.
    old_position: u32,
}

/// The lines of one file that GitLab's diff of the merge request shows.
#[derive(Clone, Debug, Default, PartialEq)]
struct DiffFile {
    /// The path before a rename, when the diff names one.
    old_path: Option<String>,
    lines: HashMap<u32, DiffLine>,
}

/// Reads the unified diff of one file, as `git diff -U3 base head -- path`
/// prints it, into the lines a comment may sit on.
fn parse_diff_file(text: &str) -> DiffFile {
    let mut file = DiffFile::default();
    let mut in_hunk = false;
    let (mut old, mut new) = (0u32, 0u32);
    for line in text.lines() {
        if let Some(hunk) = parse_hunk_header(line) {
            in_hunk = true;
            old = hunk.old_start;
            new = hunk.new_start;
            continue;
        }
        if !in_hunk {
            if let Some(path) = line.strip_prefix("--- a/") {
                file.old_path = Some(path.to_string());
            }
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => {
                file.lines.insert(
                    new,
                    DiffLine {
                        old_line: None,
                        old_position: old,
                    },
                );
                new += 1;
            }
            Some(b'-') => old += 1,
            Some(b' ') => {
                file.lines.insert(
                    new,
                    DiffLine {
                        old_line: Some(old),
                        old_position: old,
                    },
                );
                old += 1;
                new += 1;
            }
            // "\ No newline at end of file", or a header of the next file.
            _ => {}
        }
    }
    file
}

/// The hunks of `git diff -U0 head -- path`: what changed between the merge
/// request's head commit and the file on disk.
fn parse_hunks(text: &str) -> Vec<Hunk> {
    text.lines().filter_map(parse_hunk_header).collect()
}

/// Where line `local_line` of the file on disk sits in the merge request's
/// head commit, if the line is unchanged between the two.
fn line_in_head(hunks: &[Hunk], local_line: u32) -> Option<u32> {
    let mut shift: i64 = 0;
    for hunk in hunks {
        if hunk.new_len > 0 && (hunk.new_start..hunk.new_start + hunk.new_len).contains(&local_line)
        {
            return None;
        }
        let hunk_end = if hunk.new_len == 0 {
            hunk.new_start
        } else {
            hunk.new_start + hunk.new_len - 1
        };
        if local_line > hunk_end {
            shift += i64::from(hunk.old_len) - i64::from(hunk.new_len);
        }
    }
    u32::try_from(i64::from(local_line) + shift).ok()
}

/// One file of the merge request as GitLab holds it, next to how the file on
/// disk differs from it.
struct FileOnGitLab {
    path: String,
    diff: DiffFile,
    /// Changes from the merge request's head commit to the file on disk.
    since_head: Vec<Hunk>,
}

impl FileOnGitLab {
    async fn load(work_dir: &Path, diff_refs: &DiffRefs, path: &str) -> Result<Self> {
        let diff = git(
            work_dir,
            &[
                "diff",
                "-U3",
                &diff_refs.base_sha,
                &diff_refs.head_sha,
                "--",
                path,
            ],
        )
        .await?;
        let since_head = git(work_dir, &["diff", "-U0", &diff_refs.head_sha, "--", path]).await?;
        Ok(Self {
            path: path.to_string(),
            diff: parse_diff_file(&diff),
            since_head: parse_hunks(&since_head),
        })
    }

    /// The discussion to post for `comment`, or why GitLab would not take it.
    fn place(
        &self,
        comment: &GitLabComment,
        diff_refs: &DiffRefs,
    ) -> std::result::Result<serde_json::Value, String> {
        let head = &diff_refs.head_sha[..diff_refs.head_sha.len().min(7)];
        let in_head = |local_line: u32| {
            line_in_head(&self.since_head, local_line).ok_or_else(|| {
                format!(
                    "{}:{local_line} is not in the merge request's head commit {head}; \
                     the file on disk differs from what GitLab has, so update or push \
                     the branch and place the comment again",
                    self.path
                )
            })
        };
        let line = in_head(comment.line)?;
        let start_line = in_head(comment.start_line.min(comment.line))?;
        let on_diff = |new_line: u32| {
            self.diff.lines.get(&new_line).copied().ok_or_else(|| {
                format!(
                    "{}:{new_line} is outside the merge request's diff on GitLab, \
                     which only takes comments on changed lines and the three lines \
                     around them",
                    self.path
                )
            })
        };
        let end = on_diff(line)?;
        let start = on_diff(start_line)?;

        let path_hash = format!("{:x}", Sha1::digest(self.path.as_bytes()));
        let range_end = |new_line: u32, diff_line: DiffLine| {
            json!({
                "line_code": format!("{path_hash}_{}_{new_line}", diff_line.old_position),
                "type": if diff_line.old_line.is_some() { None } else { Some("new") },
                "old_line": diff_line.old_line,
                "new_line": new_line,
            })
        };
        let mut position = json!({
            "position_type": "text",
            "base_sha": diff_refs.base_sha,
            "head_sha": diff_refs.head_sha,
            "start_sha": diff_refs.start_sha,
            "new_path": self.path,
            "old_path": self.diff.old_path.as_deref().unwrap_or(&self.path),
            "new_line": line,
        });
        if let Some(old_line) = end.old_line {
            position["old_line"] = json!(old_line);
        }
        if start_line < line {
            position["line_range"] = json!({
                "start": range_end(start_line, start),
                "end": range_end(line, end),
            });
        }
        Ok(json!({ "body": comment.body, "position": position }))
    }
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

    const README_DIFF: &str = "\
diff --git a/docs/README.md b/docs/README.md
index 1111111..2222222 100644
--- a/docs/README.md
+++ b/docs/README.md
@@ -38,4 +38,6 @@ Intro
 context one
-gone
+fresh one
+fresh two
 context two
 context three
";

    fn refs() -> DiffRefs {
        DiffRefs {
            base_sha: "b".into(),
            head_sha: "h".into(),
            start_sha: "s".into(),
        }
    }

    fn file(since_head: &[Hunk]) -> FileOnGitLab {
        FileOnGitLab {
            path: "docs/README.md".into(),
            diff: parse_diff_file(README_DIFF),
            since_head: since_head.to_vec(),
        }
    }

    fn comment(start_line: u32, line: u32) -> GitLabComment {
        GitLabComment {
            id: 7,
            path: "docs/README.md".into(),
            start_line,
            line,
            body: "Why?".into(),
        }
    }

    #[test]
    fn hunk_headers_parse_with_and_without_counts() {
        assert_eq!(
            parse_hunk_header("@@ -246,6 +246,8 @@ uv run"),
            Some(Hunk {
                old_start: 246,
                old_len: 6,
                new_start: 246,
                new_len: 8
            })
        );
        assert_eq!(
            parse_hunk_header("@@ -1 +1,2 @@"),
            Some(Hunk {
                old_start: 1,
                old_len: 1,
                new_start: 1,
                new_len: 2
            })
        );
        assert_eq!(parse_hunk_header("+++ b/x"), None);
    }

    #[test]
    fn diff_file_tells_added_lines_from_unchanged_ones() {
        let diff = parse_diff_file(README_DIFF);
        assert_eq!(diff.old_path.as_deref(), Some("docs/README.md"));
        // 38 context, 39 and 40 added, 41 and 42 context; nothing beyond.
        assert_eq!(
            diff.lines[&38],
            DiffLine {
                old_line: Some(38),
                old_position: 38
            }
        );
        assert_eq!(
            diff.lines[&39],
            DiffLine {
                old_line: None,
                old_position: 40
            }
        );
        assert_eq!(
            diff.lines[&42],
            DiffLine {
                old_line: Some(41),
                old_position: 41
            }
        );
        assert!(!diff.lines.contains_key(&43));
    }

    #[test]
    fn lines_map_onto_the_head_commit_around_local_changes() {
        // On disk, two lines were added at 10 and one removed at 20 since head.
        let hunks = parse_hunks("@@ -9,0 +10,2 @@\n+a\n+b\n@@ -20 +21,0 @@\n-c\n");
        assert_eq!(line_in_head(&hunks, 5), Some(5));
        assert_eq!(line_in_head(&hunks, 10), None);
        assert_eq!(line_in_head(&hunks, 11), None);
        assert_eq!(line_in_head(&hunks, 12), Some(10));
        assert_eq!(line_in_head(&hunks, 30), Some(29));
        assert_eq!(line_in_head(&[], 318), Some(318));
    }

    #[test]
    fn a_comment_on_an_added_line_goes_to_the_new_side() {
        let body = file(&[]).place(&comment(39, 39), &refs()).unwrap();
        assert_eq!(
            body,
            json!({
                "body": "Why?",
                "position": {
                    "position_type": "text",
                    "base_sha": "b",
                    "head_sha": "h",
                    "start_sha": "s",
                    "new_path": "docs/README.md",
                    "old_path": "docs/README.md",
                    "new_line": 39,
                }
            })
        );
    }

    #[test]
    fn a_comment_on_an_unchanged_line_carries_both_sides() {
        let body = file(&[]).place(&comment(41, 41), &refs()).unwrap();
        assert_eq!(body["position"]["new_line"], 41);
        assert_eq!(body["position"]["old_line"], 40);
    }

    #[test]
    fn a_multi_line_comment_spans_with_gitlab_line_codes() {
        let body = file(&[]).place(&comment(38, 40), &refs()).unwrap();
        let path_hash = format!("{:x}", Sha1::digest(b"docs/README.md"));
        assert_eq!(
            body["position"]["line_range"],
            json!({
                "start": {
                    "line_code": format!("{path_hash}_38_38"),
                    "type": null,
                    "old_line": 38,
                    "new_line": 38,
                },
                "end": {
                    "line_code": format!("{path_hash}_40_40"),
                    "type": "new",
                    "old_line": null,
                    "new_line": 40,
                },
            })
        );
    }

    #[test]
    fn a_comment_off_the_diff_or_on_local_only_lines_says_why() {
        let outside = file(&[]).place(&comment(100, 100), &refs()).unwrap_err();
        assert!(outside.contains("docs/README.md:100"), "{outside}");
        assert!(
            outside.contains("outside the merge request's diff"),
            "{outside}"
        );

        let local_only = parse_hunks("@@ -38,0 +39,2 @@\n+x\n+y\n");
        let unpushed = file(&local_only)
            .place(&comment(39, 39), &refs())
            .unwrap_err();
        assert!(
            unpushed.contains("not in the merge request's head commit h"),
            "{unpushed}"
        );

        // A line after the local change maps back and is still placed.
        let shifted = file(&local_only).place(&comment(44, 44), &refs()).unwrap();
        assert_eq!(shifted["position"]["new_line"], 42);
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
