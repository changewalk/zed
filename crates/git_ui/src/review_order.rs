//! A suggested order to read the files of a diff in, written by a language model.
//!
//! The diff multibuffer sorts its excerpts by this order when one is set, and
//! shows each file's reason above its first excerpt. Each file may also carry
//! a few guiding notes, shown above the lines they describe.
//!
//! The order and the notes come from two separate requests, so the order can
//! be applied as soon as its small prompt is answered while the larger notes
//! prompt is still running.

use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use git::{repository::RepoPath, status::FileStatus};
use gpui::SharedString;
use serde::Deserialize;
use std::fmt::Write as _;

/// A short note that guides the reviewer through part of a file.
///
/// It says what a part does or how it connects to another file. It is not a
/// review finding.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewNote {
    /// One-based line in the new version of the file.
    pub line: u32,
    pub text: SharedString,
}

/// One file in a review order, with the model's reason for its place.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewOrderEntry {
    pub path: RepoPath,
    pub reason: SharedString,
    pub notes: Vec<ReviewNote>,
}

/// The most notes kept for one file, so the diff does not fill with prose.
pub(crate) const MAX_NOTES_PER_FILE: usize = 3;

/// The files of a diff in the order a reviewer should read them.
///
/// Files the model left out keep their usual sort order and come after the
/// listed ones.
#[derive(Debug, Default, PartialEq)]
pub struct ReviewOrder {
    entries: Vec<ReviewOrderEntry>,
    position_by_path: HashMap<RepoPath, usize>,
}

impl ReviewOrder {
    pub fn new(entries: impl IntoIterator<Item = ReviewOrderEntry>) -> Self {
        let mut order = Self::default();
        for entry in entries {
            if order.position_by_path.contains_key(&entry.path) {
                continue;
            }
            order
                .position_by_path
                .insert(entry.path.clone(), order.entries.len());
            order.entries.push(entry);
        }
        order
    }

    pub fn entries(&self) -> &[ReviewOrderEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The zero-based place of a file in the order, if the model listed it.
    pub fn position(&self, path: &RepoPath) -> Option<usize> {
        self.position_by_path.get(path).copied()
    }

    /// The same order with each file's notes replaced by `notes`. Files
    /// without an entry in `notes` lose theirs.
    pub fn with_notes(&self, mut notes: ReviewNotes) -> Self {
        Self::new(self.entries.iter().map(|entry| ReviewOrderEntry {
            path: entry.path.clone(),
            reason: entry.reason.clone(),
            notes: notes.remove(&entry.path).unwrap_or_default(),
        }))
    }
}

/// Guiding notes keyed by the file they belong to.
pub type ReviewNotes = HashMap<RepoPath, Vec<ReviewNote>>;

/// What the prompts tell the model about one changed file.
pub(crate) struct ReviewOrderFile {
    pub path: RepoPath,
    pub status: FileStatus,
    pub added_lines: usize,
    pub removed_lines: usize,
    /// The first changed lines of the file as `+` and `-` lines, already
    /// capped. Added lines carry their line number so notes can point at them.
    pub excerpt: String,
}

/// The most bytes of changed lines sent for one file in the order prompt,
/// which only needs a taste of each file to place it.
pub(crate) const ORDER_EXCERPT_BYTES_PER_FILE: usize = 800;
/// The most bytes of changed lines sent in the whole order prompt.
pub(crate) const ORDER_EXCERPT_BYTES_TOTAL: usize = 16_000;
/// The most bytes of changed lines sent for one file in the notes prompt.
pub(crate) const MAX_EXCERPT_BYTES_PER_FILE: usize = 6_000;
/// The most bytes of changed lines sent in the whole notes prompt.
pub(crate) const MAX_EXCERPT_BYTES_TOTAL: usize = 60_000;

/// The prompt that asks for the reading order, kept small so it answers fast.
pub(crate) fn build_order_prompt(files: &[ReviewOrderFile], instructions: Option<&str>) -> String {
    let mut prompt = include_str!("review_order_prompt.txt").to_string();
    append_instructions(&mut prompt, instructions);
    append_files(
        &mut prompt,
        files,
        ORDER_EXCERPT_BYTES_PER_FILE,
        ORDER_EXCERPT_BYTES_TOTAL,
    );
    prompt
}

/// The prompt that asks for guiding notes, with as much of each file as the
/// budget allows.
pub(crate) fn build_notes_prompt(files: &[ReviewOrderFile], instructions: Option<&str>) -> String {
    let mut prompt = include_str!("review_notes_prompt.txt").to_string();
    append_instructions(&mut prompt, instructions);
    append_files(
        &mut prompt,
        files,
        MAX_EXCERPT_BYTES_PER_FILE,
        MAX_EXCERPT_BYTES_TOTAL,
    );
    prompt
}

fn append_instructions(prompt: &mut String, instructions: Option<&str>) {
    if let Some(instructions) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
        write!(
            prompt,
            "\nExtra instructions from the reviewer:\n{instructions}\n"
        )
        .ok();
    }
}

fn append_files(
    prompt: &mut String,
    files: &[ReviewOrderFile],
    per_file_budget: usize,
    total_budget: usize,
) {
    writeln!(prompt, "\nChanged files ({}):", files.len()).ok();
    let mut budget = total_budget;
    for file in files {
        writeln!(
            prompt,
            "\n### {} ({}, +{} -{})",
            file.path.as_unix_str(),
            status_label(file.status),
            file.added_lines,
            file.removed_lines
        )
        .ok();
        if budget == 0 || file.excerpt.is_empty() {
            continue;
        }
        let excerpt = truncate_to_lines(&file.excerpt, budget.min(per_file_budget));
        budget = budget.saturating_sub(excerpt.len());
        writeln!(prompt, "```diff\n{excerpt}\n```").ok();
    }
}

fn status_label(status: FileStatus) -> &'static str {
    if status.is_conflicted() {
        "conflicted"
    } else if status.is_created() {
        "added"
    } else if status.is_deleted() {
        "deleted"
    } else {
        "modified"
    }
}

/// Cuts `text` at the last line break before `max_bytes`, so a line is never
/// split in the middle of a character or a word.
fn truncate_to_lines(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text.trim_end();
    }
    let cut = text[..max_bytes].rfind('\n').unwrap_or(0);
    text[..cut].trim_end()
}

#[derive(Deserialize)]
struct RawEntry {
    path: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    notes: Vec<RawNote>,
}

#[derive(Deserialize)]
struct RawNote {
    #[serde(default)]
    line: u32,
    #[serde(default)]
    note: String,
}

/// The JSON array in a reply, with the paths resolved against the diff.
///
/// Prose around the array is tolerated. Paths that are not in `known_paths`
/// are dropped, as are repeats.
fn parse_entries(text: &str, known_paths: &[RepoPath]) -> Result<Vec<(RepoPath, RawEntry)>> {
    let start = text.find('[').context("the reply has no JSON array")?;
    let end = text.rfind(']').context("the reply has no JSON array")?;
    if end < start {
        return Err(anyhow!("the reply has no JSON array"));
    }
    let raw: Vec<RawEntry> =
        serde_json::from_str(&text[start..=end]).context("the reply is not a JSON array")?;

    let known: HashMap<&str, &RepoPath> = known_paths
        .iter()
        .map(|path| (path.as_unix_str(), path))
        .collect();
    let mut seen = HashSet::default();
    let entries = raw
        .into_iter()
        .filter_map(|entry| {
            let path = entry.path.trim().trim_start_matches("./");
            let path = *known.get(path)?;
            seen.insert(path.clone()).then(|| (path.clone(), entry))
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return Err(anyhow!("the reply names no file from this diff"));
    }
    Ok(entries)
}

fn notes_of(entry: RawEntry) -> Vec<ReviewNote> {
    let mut note_lines = HashSet::default();
    entry
        .notes
        .into_iter()
        .filter(|note| note.line > 0 && !note.note.trim().is_empty())
        .filter(|note| note_lines.insert(note.line))
        .take(MAX_NOTES_PER_FILE)
        .map(|note| ReviewNote {
            line: note.line,
            text: note.note.trim().to_string().into(),
        })
        .collect()
}

/// Reads the order reply into a [`ReviewOrder`]. Notes in the reply, if any,
/// are kept.
pub(crate) fn parse_review_order(text: &str, known_paths: &[RepoPath]) -> Result<ReviewOrder> {
    let entries = parse_entries(text, known_paths)?;
    Ok(ReviewOrder::new(entries.into_iter().map(
        |(path, entry)| ReviewOrderEntry {
            path,
            reason: entry.reason.trim().to_string().into(),
            notes: notes_of(entry),
        },
    )))
}

/// Reads the notes reply into notes keyed by file. Files with no usable
/// notes are left out.
pub(crate) fn parse_review_notes(text: &str, known_paths: &[RepoPath]) -> Result<ReviewNotes> {
    let entries = parse_entries(text, known_paths)?;
    Ok(entries
        .into_iter()
        .map(|(path, entry)| (path, notes_of(entry)))
        .filter(|(_, notes)| !notes.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::repo_path;

    #[test]
    fn parse_keeps_the_models_order_and_drops_unknown_and_repeated_paths() {
        let known = [
            repo_path("src/b.rs"),
            repo_path("src/a.rs"),
            repo_path("c.rs"),
        ];
        let reply = r#"Here is the order:
[
  {"path": "src/a.rs", "reason": "The main change."},
  {"path": "./c.rs", "reason": "Tests it."},
  {"path": "src/a.rs", "reason": "Repeated."},
  {"path": "nope.rs", "reason": "Not in the diff."}
]
Done."#;

        let order = parse_review_order(reply, &known).unwrap();

        assert_eq!(
            order
                .entries()
                .iter()
                .map(|entry| entry.path.as_unix_str())
                .collect::<Vec<_>>(),
            vec!["src/a.rs", "c.rs"]
        );
        assert_eq!(order.position(&repo_path("src/a.rs")), Some(0));
        assert_eq!(order.position(&repo_path("c.rs")), Some(1));
        assert_eq!(order.position(&repo_path("src/b.rs")), None);
        assert_eq!(order.entries()[1].reason.as_ref(), "Tests it.");
        assert!(order.entries().iter().all(|entry| entry.notes.is_empty()));
    }

    #[test]
    fn parse_notes_drops_bad_notes_caps_them_and_merges_into_an_order() {
        let known = [repo_path("src/a.rs"), repo_path("c.rs")];
        let reply = r#"[
  {"path": "src/a.rs", "notes": [
    {"line": 12, "note": "The retry loop; compare with the old one in c.rs."},
    {"line": 0, "note": "No line, dropped."},
    {"line": 12, "note": "Same line again, dropped."},
    {"line": 30, "note": "   "},
    {"line": 40, "note": "Second note."},
    {"line": 50, "note": "Third note."},
    {"line": 60, "note": "Fourth note, over the cap."}
  ]},
  {"path": "c.rs", "notes": []}
]"#;

        let notes = parse_review_notes(reply, &known).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[&repo_path("src/a.rs")],
            vec![
                ReviewNote {
                    line: 12,
                    text: "The retry loop; compare with the old one in c.rs.".into(),
                },
                ReviewNote {
                    line: 40,
                    text: "Second note.".into(),
                },
                ReviewNote {
                    line: 50,
                    text: "Third note.".into(),
                },
            ]
        );

        let order = ReviewOrder::new([
            ReviewOrderEntry {
                path: repo_path("c.rs"),
                reason: "First.".into(),
                notes: Vec::new(),
            },
            ReviewOrderEntry {
                path: repo_path("src/a.rs"),
                reason: "Second.".into(),
                notes: Vec::new(),
            },
        ]);
        let order = order.with_notes(notes);
        assert_eq!(order.position(&repo_path("c.rs")), Some(0));
        assert!(order.entries()[0].notes.is_empty());
        assert_eq!(order.entries()[1].notes.len(), 3);
    }

    #[test]
    fn parse_rejects_replies_without_known_files() {
        let known = [repo_path("src/a.rs")];
        assert!(parse_review_order("no json here", &known).is_err());
        assert!(parse_review_order(r#"[{"path": "x.rs"}]"#, &known).is_err());
        assert!(parse_review_order("[not json]", &known).is_err());
        assert!(parse_review_notes("no json here", &known).is_err());
    }

    #[test]
    fn prompts_list_every_file_and_cap_each_excerpt() {
        let long_excerpt = (0..400)
            .map(|i| format!("+{i:>5} |line {i} of a long change"))
            .collect::<Vec<_>>()
            .join("\n");
        let files = vec![
            ReviewOrderFile {
                path: repo_path("src/a.rs"),
                status: FileStatus::Untracked,
                added_lines: 400,
                removed_lines: 0,
                excerpt: long_excerpt,
            },
            ReviewOrderFile {
                path: repo_path("Cargo.lock"),
                status: FileStatus::Untracked,
                added_lines: 1,
                removed_lines: 1,
                excerpt: String::new(),
            },
        ];

        for (prompt, cap) in [
            (
                build_order_prompt(&files, Some("Read the lock file last.")),
                ORDER_EXCERPT_BYTES_PER_FILE,
            ),
            (build_notes_prompt(&files, None), MAX_EXCERPT_BYTES_PER_FILE),
        ] {
            assert!(prompt.contains("### src/a.rs (added, +400 -0)"));
            assert!(prompt.contains("### Cargo.lock (added, +1 -1)"));
            let excerpt_start = prompt.find("```diff\n").unwrap() + "```diff\n".len();
            let excerpt_end = prompt[excerpt_start..].find("\n```").unwrap();
            assert!(excerpt_end <= cap);
            assert!(
                prompt[excerpt_start..excerpt_start + excerpt_end].ends_with("of a long change")
            );
        }
        assert!(
            build_order_prompt(&files, Some("Read the lock file last."))
                .contains("Read the lock file last.")
        );
    }
}
