//! Posts the review comments of a diff view to the open merge request of the
//! current branch.
//!
//! The requests go through the `glab` CLI, which already holds the user's
//! GitLab login and knows which project the repository belongs to.

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{AsyncWriteExt as _, StreamExt as _, stream};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use util::command::{Stdio, new_command};

/// A review comment resolved to a path and a line in the new version of a file.
#[derive(Clone, Debug, PartialEq)]
pub struct GitLabComment {
    /// The editor's id for the comment, so it can be removed once posted.
    pub id: usize,
    /// The path relative to the repository root, with forward slashes.
    pub path: String,
    /// One-based line in the new version of the file.
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
/// request, and the first error among those that did not.
pub struct PostOutcome {
    pub merge_request: MergeRequest,
    pub posted_ids: Vec<usize>,
    pub error: Option<anyhow::Error>,
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
    let results: Vec<(usize, Result<()>)> = stream::iter(comments)
        .map(|comment| {
            let body = discussion_body(&comment, &diff_refs).to_string();
            let work_dir = work_dir.clone();
            let endpoint = endpoint.clone();
            async move {
                let result = glab_api(&work_dir, "POST", &endpoint, Some(&body))
                    .await
                    .map(|_| ())
                    .with_context(|| {
                        format!("posting the comment on {}:{}", comment.path, comment.line)
                    });
                (comment.id, result)
            }
        })
        .buffer_unordered(CONCURRENT_POSTS)
        .collect()
        .await;

    let mut posted_ids = Vec::new();
    let mut error = None;
    for (id, result) in results {
        match result {
            Ok(()) => posted_ids.push(id),
            Err(err) => {
                error.get_or_insert(err);
            }
        }
    }
    Ok(PostOutcome {
        merge_request,
        posted_ids,
        error,
    })
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

/// The JSON body that creates one inline discussion on the new side of the diff.
fn discussion_body(comment: &GitLabComment, diff_refs: &DiffRefs) -> serde_json::Value {
    json!({
        "body": comment.body,
        "position": {
            "position_type": "text",
            "base_sha": diff_refs.base_sha,
            "head_sha": diff_refs.head_sha,
            "start_sha": diff_refs.start_sha,
            "new_path": comment.path,
            "old_path": comment.path,
            "new_line": comment.line,
        }
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
        command.args(["--input", "-"]).stdin(Stdio::piped());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discussion_body_places_the_comment_on_the_new_side() {
        let comment = GitLabComment {
            id: 7,
            path: "src/loop.rs".into(),
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
