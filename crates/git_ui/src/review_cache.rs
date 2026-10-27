//! What the review flow remembers between sessions: the order and notes a
//! model wrote for a given diff, and when the user last reviewed a merge
//! request.
//!
//! Everything is a small JSON file under Zed's data directory, so a diff
//! reopened at the same commit shows its order at once instead of asking the
//! model again.

use crate::review_order::{ReviewNote, ReviewOrder, ReviewOrderEntry};
use anyhow::{Context as _, Result};
use git::repository::RepoPath;
use serde::{Deserialize, Serialize};
use sha1::{Digest as _, Sha1};
use std::path::{Path, PathBuf};

fn cache_dir() -> PathBuf {
    paths::data_dir().join("review_cache")
}

/// A file name that is safe on every file system, from any key.
fn file_for(kind: &str, key: &str) -> PathBuf {
    let hash = format!("{:x}", Sha1::digest(key.as_bytes()));
    cache_dir().join(format!("{kind}-{hash}.json"))
}

fn read<T: for<'a> Deserialize<'a>>(file: &Path) -> Result<Option<T>> {
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("reading {}", file.display()))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", file.display())),
    }
}

fn write<T: Serialize>(file: &Path, value: &T) -> Result<()> {
    std::fs::create_dir_all(cache_dir())?;
    let text = serde_json::to_string(value)?;
    std::fs::write(file, text).with_context(|| format!("writing {}", file.display()))
}

#[derive(Serialize, Deserialize)]
struct CachedOrder {
    entries: Vec<CachedEntry>,
}

#[derive(Serialize, Deserialize)]
struct CachedEntry {
    path: String,
    reason: String,
    notes: Vec<CachedNote>,
}

#[derive(Serialize, Deserialize)]
struct CachedNote {
    line: u32,
    text: String,
}

/// The key an order is stored under: the repository, what the diff is
/// against, and the commit it is of. Any change to the files makes a new
/// commit, so a hit means the same diff.
pub(crate) fn order_key(work_dir: &Path, base_ref: &str, head_sha: &str) -> String {
    format!("{}\n{base_ref}\n{head_sha}", work_dir.display())
}

pub(crate) fn load_order(key: &str) -> Result<Option<ReviewOrder>> {
    let Some(cached): Option<CachedOrder> = read(&file_for("order", key))? else {
        return Ok(None);
    };
    let entries = cached
        .entries
        .into_iter()
        .map(|entry| {
            Ok(ReviewOrderEntry {
                path: RepoPath::new(&entry.path)?,
                reason: entry.reason.into(),
                notes: entry
                    .notes
                    .into_iter()
                    .map(|note| ReviewNote {
                        line: note.line,
                        text: note.text.into(),
                    })
                    .collect(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(ReviewOrder::new(entries)))
}

pub(crate) fn store_order(key: &str, order: &ReviewOrder) -> Result<()> {
    let cached = CachedOrder {
        entries: order
            .entries()
            .iter()
            .map(|entry| CachedEntry {
                path: entry.path.as_unix_str().to_string(),
                reason: entry.reason.to_string(),
                notes: entry
                    .notes
                    .iter()
                    .map(|note| CachedNote {
                        line: note.line,
                        text: note.text.to_string(),
                    })
                    .collect(),
            })
            .collect(),
    };
    write(&file_for("order", key), &cached)
}

/// When the user last reviewed a merge request, and at which commit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LastReview {
    pub head_sha: String,
    /// Seconds since the Unix epoch.
    pub at: i64,
}

pub(crate) fn load_last_review(merge_request_url: &str) -> Result<Option<LastReview>> {
    read(&file_for("last-review", merge_request_url))
}

pub(crate) fn store_last_review(merge_request_url: &str, review: &LastReview) -> Result<()> {
    write(&file_for("last-review", merge_request_url), review)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_order_survives_a_round_trip_through_the_cache() {
        let order = ReviewOrder::new([ReviewOrderEntry {
            path: RepoPath::new("src/a.rs").unwrap(),
            reason: "The main change.".into(),
            notes: vec![ReviewNote {
                line: 3,
                text: "A note.".into(),
            }],
        }]);
        let key = format!("test-{}", std::process::id());
        store_order(&key, &order).unwrap();
        let loaded = load_order(&key).unwrap().unwrap();
        assert_eq!(loaded, order);
        std::fs::remove_file(file_for("order", &key)).unwrap();
        assert_eq!(load_order(&key).unwrap(), None);
    }
}
