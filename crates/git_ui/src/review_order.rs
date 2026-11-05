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

    /// The same order with `pinned` files first, in their given order, and
    /// the rest following in this order. A pinned reason replaces the
    /// model's unless it is empty.
    pub fn with_pinned_first(&self, pinned: &[ReviewOrderEntry]) -> Self {
        let mut entries: Vec<ReviewOrderEntry> = pinned
            .iter()
            .map(|pin| {
                let model_entry = self.position(&pin.path).map(|index| &self.entries[index]);
                ReviewOrderEntry {
                    path: pin.path.clone(),
                    reason: if pin.reason.is_empty() {
                        model_entry
                            .map(|entry| entry.reason.clone())
                            .unwrap_or_default()
                    } else {
                        pin.reason.clone()
                    },
                    notes: model_entry
                        .map(|entry| entry.notes.clone())
                        .unwrap_or_default(),
                }
            })
            .collect();
        entries.extend(
            self.entries
                .iter()
                .filter(|entry| !pinned.iter().any(|pin| pin.path == entry.path))
                .cloned(),
        );
        Self::new(entries)
    }

    /// The same order with one file's notes replaced.
    pub fn with_file_notes(&self, path: &RepoPath, notes: Vec<ReviewNote>) -> Self {
        Self::new(self.entries.iter().map(|entry| ReviewOrderEntry {
            path: entry.path.clone(),
            reason: entry.reason.clone(),
            notes: if &entry.path == path {
                notes.clone()
            } else {
                entry.notes.clone()
            },
        }))
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

/// The order the author asks for in the merge request's description, when
/// it has a "review order" or "reading order" part naming files of the
/// diff, with the author's reasons. The files are matched among `known`
/// by full path, by unique suffix, or by a `…` in the middle.
pub(crate) fn order_from_description(
    description: &str,
    known: &[RepoPath],
) -> Vec<ReviewOrderEntry> {
    let Some(section) = order_section(description) else {
        return Vec::new();
    };
    // Each backtick token that names a file starts an entry; the text up to
    // the next such token is the author's reason for it.
    let mut entries: Vec<ReviewOrderEntry> = Vec::new();
    let mut reason = String::new();
    let mut rest = section;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else {
            break;
        };
        let token = &after[..end];
        let between = &rest[..start];
        match resolve_path(token, known) {
            Some(path) if !entries.iter().any(|entry| entry.path == path) => {
                if let Some(last) = entries.last_mut() {
                    reason.push_str(between);
                    last.reason = clean_reason(&reason).into();
                }
                reason.clear();
                entries.push(ReviewOrderEntry {
                    path,
                    reason: SharedString::default(),
                    notes: Vec::new(),
                });
            }
            _ => {
                reason.push_str(between);
                reason.push_str(token);
            }
        }
        rest = &after[end + 1..];
    }
    if let Some(last) = entries.last_mut() {
        reason.push_str(rest);
        last.reason = clean_reason(&reason).into();
    }
    entries
}

/// The part of `description` that gives the order: from the words "review
/// order" or "reading order" to the end of that paragraph or list.
fn order_section(description: &str) -> Option<&str> {
    let lower = description.to_ascii_lowercase();
    let start = [
        "review order",
        "reading order",
        "read in this order",
        "suggested order",
    ]
    .iter()
    .filter_map(|phrase| lower.find(phrase))
    .min()?;
    let section = &description[start..];
    // A paragraph ends at a blank line. A heading such as "## Review order"
    // names no file itself, so the blank line after it does not end the
    // section; the list below it does.
    let mut end = section.len();
    let mut seen_text_line = false;
    let mut offset = 0;
    for line in section.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if seen_text_line {
                end = offset;
                break;
            }
        } else if offset > 0 && trimmed.starts_with('#') {
            end = offset;
            break;
        } else if offset > 0 || trimmed.contains('`') {
            seen_text_line = true;
        }
        offset += line.len();
    }
    Some(&section[..end])
}

/// The scheme of the links that `link_file_references` writes: the path,
/// then `#L<line>` when the reference named one.
pub(crate) const REVIEW_FILE_SCHEME: &str = "review-file://";

/// `text` as markdown in which every mention of a file of the diff is a
/// link, so a note can be clicked to go there. A mention is a word that
/// names exactly one file, by full path or unique suffix, with an optional
/// `:line` after it.
pub(crate) fn link_file_references(text: &str, known: &[RepoPath]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, after) = rest.split_at(end);
        let lead = word.len() - word.trim_start_matches(['(', '`', '"', '\'', '[']).len();
        let core_end = word
            .trim_end_matches([')', '`', '"', '\'', ',', '.', ';', ':', ']'])
            .len();
        let core = &word[lead..core_end.max(lead)];
        let (name, line) = match core.rsplit_once(':') {
            Some((name, line)) if line.chars().all(|c| c.is_ascii_digit()) && !line.is_empty() => {
                (name, Some(line))
            }
            _ => (core, None),
        };
        match resolve_path(name, known) {
            Some(path) if !core.is_empty() => {
                out.push_str(&word[..lead]);
                out.push('[');
                out.push_str(core);
                out.push_str("](");
                out.push_str(REVIEW_FILE_SCHEME);
                out.push_str(path.as_unix_str());
                if let Some(line) = line {
                    out.push_str("#L");
                    out.push_str(line);
                }
                out.push(')');
                out.push_str(&word[core_end.max(lead)..]);
            }
            _ => out.push_str(word),
        }
        let space_end = after.len() - after.trim_start().len();
        out.push_str(&after[..space_end]);
        rest = &after[space_end..];
    }
    out
}

/// The file and line a link from `link_file_references` points at.
pub(crate) fn parse_review_file_url(url: &str) -> Option<(RepoPath, Option<u32>)> {
    let rest = url.strip_prefix(REVIEW_FILE_SCHEME)?;
    let (path, line) = match rest.split_once("#L") {
        Some((path, line)) => (path, line.parse().ok()),
        None => (rest, None),
    };
    Some((RepoPath::new(path).ok()?, line))
}

/// The file of the diff that `token` names, if exactly one does.
fn resolve_path(token: &str, known: &[RepoPath]) -> Option<RepoPath> {
    let token = token.trim().trim_start_matches("./");
    if token.is_empty() || !token.contains('.') {
        return None;
    }
    let matches: Vec<&RepoPath> = if let Some((prefix, suffix)) = token.split_once("...") {
        known
            .iter()
            .filter(|path| {
                let path = path.as_unix_str();
                path.starts_with(prefix.trim_end_matches('/'))
                    && path.ends_with(suffix.trim_start_matches('/'))
            })
            .collect()
    } else {
        let exact: Vec<&RepoPath> = known
            .iter()
            .filter(|path| path.as_unix_str() == token)
            .collect();
        if !exact.is_empty() {
            exact
        } else {
            known
                .iter()
                .filter(|path| {
                    let path = path.as_unix_str();
                    path.ends_with(token) && path[..path.len() - token.len()].ends_with('/')
                })
                .collect()
        }
    };
    match matches.as_slice() {
        [path] => Some((*path).clone()),
        _ => None,
    }
}

/// The author's words for a file, without the arrows and brackets that
/// joined the list, capped at a sentence.
fn clean_reason(text: &str) -> String {
    let joined = text.replace('→', " ").replace("->", " ");
    let joined = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned = joined
        .trim_matches(|c: char| c.is_whitespace() || "(),.:;-–".contains(c))
        .to_string();
    const MAX_CHARS: usize = 100;
    if cleaned.chars().count() > MAX_CHARS {
        let cut: String = cleaned.chars().take(MAX_CHARS).collect();
        format!("{}…", cut.trim_end())
    } else {
        cleaned
    }
}

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

/// The most bytes of merge request description or issue text put in a prompt.
const MAX_CONTEXT_BYTES: usize = 6_000;

/// The prompt that asks for the reading order, kept small so it answers fast.
/// `context` describes the merge request: its title, description, and the
/// issues it is for.
pub(crate) fn build_order_prompt(files: &[ReviewOrderFile], context: Option<&str>) -> String {
    let mut prompt = include_str!("review_order_prompt.txt").to_string();
    append_context(&mut prompt, context);
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
pub(crate) fn build_notes_prompt(files: &[ReviewOrderFile], context: Option<&str>) -> String {
    let mut prompt = include_str!("review_notes_prompt.txt").to_string();
    append_context(&mut prompt, context);
    append_files(
        &mut prompt,
        files,
        MAX_EXCERPT_BYTES_PER_FILE,
        MAX_EXCERPT_BYTES_TOTAL,
    );
    prompt
}

/// One of the reviewer's comments, for the summary prompt.
pub(crate) struct SummaryComment {
    pub path: String,
    pub line: u32,
    pub text: String,
}

/// The prompt that asks for a summary comment on the merge request, drawn
/// from the reviewer's inline comments.
pub(crate) fn build_summary_prompt(context: Option<&str>, comments: &[SummaryComment]) -> String {
    let mut prompt = include_str!("review_summary_prompt.txt").to_string();
    append_context(&mut prompt, context);
    writeln!(
        prompt,
        "\nThe reviewer's inline comments ({}):",
        comments.len()
    )
    .ok();
    for comment in comments {
        writeln!(
            prompt,
            "\n{}:{}\n{}",
            comment.path,
            comment.line,
            comment.text.trim()
        )
        .ok();
    }
    prompt
}

/// One note of a thread, for the reply prompt.
pub(crate) struct ThreadNote {
    pub author: String,
    pub body: String,
}

/// The prompt that asks for a reply to a thread, given the code it is on
/// and what has been said so far. `own_name` is how the reviewer appears
/// in the thread, so the model knows which side it writes for.
pub(crate) fn build_reply_prompt(
    context: Option<&str>,
    path: &str,
    code: &str,
    notes: &[ThreadNote],
    own_name: &str,
) -> String {
    let mut prompt = include_str!("review_reply_prompt.txt").to_string();
    append_context(&mut prompt, context);
    writeln!(prompt, "\nYou write as {own_name}.").ok();
    writeln!(
        prompt,
        "\nThe code the thread is on, in {path}:\n```\n{}\n```",
        code.trim_end()
    )
    .ok();
    writeln!(prompt, "\nThe thread so far:").ok();
    for note in notes {
        writeln!(prompt, "\n{}:\n{}", note.author, note.body.trim()).ok();
    }
    prompt
}

fn append_context(prompt: &mut String, context: Option<&str>) {
    if let Some(context) = context.map(str::trim).filter(|context| !context.is_empty()) {
        let context = truncate_to_lines(context, MAX_CONTEXT_BYTES);
        write!(prompt, "\nAbout the merge request:\n{context}\n").ok();
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
    // The limit may fall inside a multi-byte character; back up to the
    // start of it before looking for the line break.
    let mut boundary = max_bytes;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let cut = text[..boundary].rfind('\n').unwrap_or(0);
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

/// Reads a notes reply as it streams in, giving out each file's notes as
/// soon as its JSON object is complete, so the first files of the order
/// get their notes while the model is still writing the rest.
#[derive(Default)]
pub(crate) struct NotesStreamParser {
    buffer: String,
    /// Where the next unread object can start.
    offset: usize,
    in_array: bool,
    seen: HashSet<RepoPath>,
}

impl NotesStreamParser {
    /// Adds `chunk` and returns the files whose notes are now complete.
    pub(crate) fn push(
        &mut self,
        chunk: &str,
        known_paths: &[RepoPath],
    ) -> Vec<(RepoPath, Vec<ReviewNote>)> {
        self.buffer.push_str(chunk);
        let mut ready = Vec::new();
        loop {
            if !self.in_array {
                match self.buffer[self.offset..].find('[') {
                    Some(index) => {
                        self.offset += index + 1;
                        self.in_array = true;
                    }
                    None => return ready,
                }
            }
            let Some((start, end)) = next_object(&self.buffer[self.offset..]) else {
                return ready;
            };
            let object = &self.buffer[self.offset + start..self.offset + end];
            self.offset += end;
            if let Ok(entry) = serde_json::from_str::<RawEntry>(object)
                && let Some(path) = known_paths
                    .iter()
                    .find(|path| path.as_unix_str() == entry.path.trim().trim_start_matches("./"))
                && self.seen.insert(path.clone())
            {
                let notes = notes_of(entry);
                if !notes.is_empty() {
                    ready.push((path.clone(), notes));
                }
            }
        }
    }

    /// How many files have had notes so far.
    pub(crate) fn files_seen(&self) -> usize {
        self.seen.len()
    }
}

/// The byte range of the first complete `{…}` object in `text`, if one
/// is there, skipping braces inside strings.
fn next_object(text: &str) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut start = None;
    let mut in_string = false;
    let mut escaped = false;
    for (index, c) in text.char_indices() {
        if in_string {
            match c {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => in_string = false,
                _ => escaped = false,
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(start) = start
                {
                    return Some((start, index + c.len_utf8()));
                }
            }
            _ => {}
        }
    }
    None
}

/// Reads a whole notes reply into notes keyed by file. Files with no
/// usable notes are left out. The live path streams instead; this stays
/// for the tests of the shared parsing.
#[cfg(test)]
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

    #[test]
    fn the_description_order_pins_its_files_first_with_the_authors_reasons() {
        let known: Vec<RepoPath> = [
            "docs/adrs/side-effects-run-on-temporal.md",
            "backend/src/app/temporal.py",
            "backend/src/app/modules/index/side_effects.py",
            "backend/src/app/modules/workspaces/side_effects.py",
            "helm/app/templates/seed-job.yaml",
            "backend/tests/support.py",
            "backend/src/app/worker.py",
        ]
        .iter()
        .map(|path| RepoPath::new(path).unwrap())
        .collect();
        let description = "Some intro.\n\n**Review order:** `docs/adrs/side-effects-run-on-temporal.md` (the decision) → `backend/src/app/temporal.py` (`RunActivity`, `defer_activity`) → `index/side_effects.py` (the shape every module repeats) → `helm/.../seed-job.yaml` → the tests (`tests/support.py` `eventually` first).\n\nMore text with `worker.py` later.";

        let pinned = order_from_description(description, &known);
        let pinned_paths: Vec<&str> = pinned.iter().map(|e| e.path.as_unix_str()).collect();
        assert_eq!(
            pinned_paths,
            vec![
                "docs/adrs/side-effects-run-on-temporal.md",
                "backend/src/app/temporal.py",
                "backend/src/app/modules/index/side_effects.py",
                "helm/app/templates/seed-job.yaml",
                "backend/tests/support.py",
            ]
        );
        assert_eq!(pinned[0].reason.as_ref(), "the decision");
        assert_eq!(pinned[1].reason.as_ref(), "RunActivity, defer_activity");
        assert_eq!(pinned[2].reason.as_ref(), "the shape every module repeats");
        assert_eq!(pinned[3].reason.as_ref(), "the tests");
        assert_eq!(pinned[4].reason.as_ref(), "eventually first");

        let model_order = ReviewOrder::new(known.iter().rev().map(|path| ReviewOrderEntry {
            path: path.clone(),
            reason: "model".into(),
            notes: Vec::new(),
        }));
        let combined = model_order.with_pinned_first(&pinned);
        let paths: Vec<&str> = combined
            .entries()
            .iter()
            .map(|e| e.path.as_unix_str())
            .collect();
        assert_eq!(
            paths,
            vec![
                "docs/adrs/side-effects-run-on-temporal.md",
                "backend/src/app/temporal.py",
                "backend/src/app/modules/index/side_effects.py",
                "helm/app/templates/seed-job.yaml",
                "backend/tests/support.py",
                "backend/src/app/worker.py",
                "backend/src/app/modules/workspaces/side_effects.py",
            ]
        );
        assert_eq!(combined.entries()[0].reason.as_ref(), "the decision");
        assert_eq!(combined.entries()[5].reason.as_ref(), "model");
    }

    #[test]
    fn file_mentions_in_notes_become_links_with_their_line() {
        let known: Vec<RepoPath> = ["src/client.rs", "src/loop.rs", "docs/a.md"]
            .iter()
            .map(|path| RepoPath::new(path).unwrap())
            .collect();
        let linked = link_file_references(
            "The callers in src/client.rs:41 rely on it (see `loop.rs`), not docs.",
            &known,
        );
        assert_eq!(
            linked,
            "The callers in [src/client.rs:41](review-file://src/client.rs#L41) rely on it \
             (see `[loop.rs](review-file://src/loop.rs)`), not docs."
        );
        assert_eq!(
            parse_review_file_url("review-file://src/client.rs#L41"),
            Some((RepoPath::new("src/client.rs").unwrap(), Some(41)))
        );
        assert_eq!(
            parse_review_file_url("review-file://src/loop.rs"),
            Some((RepoPath::new("src/loop.rs").unwrap(), None))
        );
        assert_eq!(parse_review_file_url("https://x"), None);
    }

    #[test]
    fn notes_stream_gives_each_file_as_its_object_completes() {
        let known = vec![
            RepoPath::new("a.rs").unwrap(),
            RepoPath::new("b.rs").unwrap(),
        ];
        let mut parser = NotesStreamParser::default();
        assert!(
            parser
                .push(
                    "Sure:\n[\n  {\"path\": \"a.rs\", \"notes\": [{\"line\": 3, \"no",
                    &known
                )
                .is_empty()
        );
        let first = parser.push("te\": \"A } brace in text\"}]},\n  {\"path\": \"b.", &known);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0.as_unix_str(), "a.rs");
        assert_eq!(first[0].1[0].text.as_ref(), "A } brace in text");
        let second = parser.push(
            "rs\", \"notes\": [{\"line\": 9, \"note\": \"B\"}]}\n]",
            &known,
        );
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].0.as_unix_str(), "b.rs");
        assert_eq!(parser.files_seen(), 2);
        assert!(parser.push("", &known).is_empty());
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let text = "first line\nsecond — dash\nthird";
        assert_eq!(truncate_to_lines(text, 19), "first line");
        assert_eq!(truncate_to_lines("short", 100), "short");
    }

    #[test]
    fn a_description_without_an_order_pins_nothing() {
        let known = vec![RepoPath::new("a.rs").unwrap()];
        assert!(order_from_description("Just `a.rs` changed.", &known).is_empty());
        assert!(order_from_description("", &known).is_empty());
    }
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
