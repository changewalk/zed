pub(super) mod blame;

use super::*;
use crate::hover_popover::hover_markdown_style;
use ::git::{
    Oid, Restore, blame::BlameEntry, commit::ParsedCommitMessage, repository::RepoPath,
    status::FileStatus,
};
use buffer_diff::{BufferDiff, DiffHunkStatus, DiffHunkStatusKind};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, MarkdownElement, MarkdownOptions, MarkdownStyle,
};
use project::git_store::Repository;

#[derive(Clone)]
struct ResolvedDiffHunk {
    buffer_range: Range<text::Anchor>,
    diff_base_byte_range: Range<usize>,
    status: DiffHunkStatus,
}

#[derive(Clone)]
struct ResolvedDiffHunks {
    diff: Entity<BufferDiff>,
    buffer: Option<Entity<Buffer>>,
    hunks: Vec<ResolvedDiffHunk>,
}

pub trait DiffHunkRenderer {
    fn render_hunk_controls(
        &self,
        row: u32,
        status: &DiffHunkStatus,
        hunk_range: Range<Anchor>,
        is_created_file: bool,
        line_height: Pixels,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement;

    fn render_hunk_as_staged(&self, status: &DiffHunkStatus, _cx: &App) -> bool {
        !status.has_secondary_hunk()
    }
}

pub struct DefaultDiffHunkRenderer;

impl DiffHunkRenderer for DefaultDiffHunkRenderer {
    fn render_hunk_controls(
        &self,
        row: u32,
        status: &DiffHunkStatus,
        hunk_range: Range<Anchor>,
        is_created_file: bool,
        line_height: Pixels,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        render_diff_hunk_controls(
            row,
            status,
            hunk_range,
            is_created_file,
            line_height,
            editor,
            window,
            cx,
        )
    }
}

pub struct HiddenDiffHunkRenderer;

impl DiffHunkRenderer for HiddenDiffHunkRenderer {
    fn render_hunk_controls(
        &self,
        _row: u32,
        _status: &DiffHunkStatus,
        _hunk_range: Range<Anchor>,
        _is_created_file: bool,
        _line_height: Pixels,
        _editor: &Entity<Editor>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        gpui::Empty.into_any_element()
    }
}

pub struct HiddenUnstagedDiffHunkRenderer;

impl DiffHunkRenderer for HiddenUnstagedDiffHunkRenderer {
    fn render_hunk_controls(
        &self,
        _row: u32,
        _status: &DiffHunkStatus,
        _hunk_range: Range<Anchor>,
        _is_created_file: bool,
        _line_height: Pixels,
        _editor: &Entity<Editor>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        gpui::Empty.into_any_element()
    }

    fn render_hunk_as_staged(&self, _status: &DiffHunkStatus, _cx: &App) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DisplayDiffHunk {
    Folded {
        display_row: DisplayRow,
    },
    Unfolded {
        is_created_file: bool,
        diff_base_byte_range: Range<usize>,
        display_row_range: Range<DisplayRow>,
        multi_buffer_range: Range<Anchor>,
        status: DiffHunkStatus,
        word_diffs: Vec<Range<MultiBufferOffset>>,
    },
}

#[derive(Clone)]
pub(super) struct InlineBlamePopoverState {
    pub(super) scroll_handle: ScrollHandle,
    pub(super) commit_message: Option<ParsedCommitMessage>,
    pub(super) markdown: Entity<Markdown>,
}

pub(super) struct InlineBlamePopover {
    pub(super) position: gpui::Point<Pixels>,
    pub(super) hide_task: Option<Task<()>>,
    pub(super) popover_bounds: Option<Bounds<Pixels>>,
    pub(super) popover_state: InlineBlamePopoverState,
    pub(super) keyboard_grace: bool,
}

/// Represents a diff review button indicator that shows up when hovering over lines in the gutter
/// in diff view mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PhantomDiffReviewIndicator {
    /// The starting anchor of the selection (or the only row if not dragging).
    pub(super) start: Anchor,
    /// The ending anchor of the selection. Equal to start_anchor for single-line selection.
    pub(super) end: Anchor,
    /// There's a small debounce between hovering over the line and showing the indicator.
    /// We don't want to show the indicator when moving the mouse from editor to e.g. project panel.
    pub(super) is_active: bool,
}

#[derive(Clone, Debug)]
pub(super) struct DiffReviewDragState {
    start_anchor: Anchor,
    current_anchor: Anchor,
}

/// Identifies a specific hunk in the diff buffer.
/// Used as a key to group comments by their location.
#[derive(Clone, Debug)]
pub(super) struct DiffHunkKey {
    /// The file path (relative to worktree) this hunk belongs to.
    pub(super) file_path: Arc<util::rel_path::RelPath>,
    /// An anchor at the start of the hunk. This tracks position as the buffer changes.
    pub(super) hunk_start_anchor: Anchor,
}

/// A reply on the remote thread a review comment was posted to.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewReply {
    pub author: SharedString,
    pub body: String,
}

/// The remote thread a review comment lives on, once posted.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ReviewThread {
    pub discussion_id: String,
    /// GitLab's id for the opening note, so the comment can be edited there.
    pub note_id: Option<u64>,
    pub replies: Vec<ReviewReply>,
    pub resolved: bool,
    /// The one-based line the thread was opened on, when the diff no longer
    /// shows that line and the thread sits at the nearest shown one instead.
    pub original_line: Option<u32>,
    /// The one-based line in the old version of the file, for a thread on
    /// lines the change removed; it sits at the place they were removed from.
    pub old_line: Option<u32>,
    /// The thread was opened after the user last reviewed the change.
    pub since_last_review: bool,
}

/// A review comment lifted out of the editor by file and row, so it can be
/// put back after the excerpts it was anchored in have been rebuilt.
#[derive(Clone, Debug)]
pub struct ReviewCommentRecord {
    /// The file path, relative to the worktree.
    pub file_path: Arc<util::rel_path::RelPath>,
    pub start_row: u32,
    pub end_row: u32,
    pub text: String,
    pub author: Option<SharedString>,
    pub thread: Option<ReviewThread>,
}

/// A review comment stored locally before being sent to the Agent panel.
#[derive(Clone)]
pub(super) struct StoredReviewComment {
    /// Unique identifier for this comment (for edit/delete operations).
    pub(super) id: usize,
    /// The comment text entered by the user.
    pub(super) comment: String,
    /// Anchors for the code range being reviewed.
    pub(super) range: Range<Anchor>,
    /// Whether this comment is currently being edited inline.
    pub(super) is_editing: bool,
    /// Who wrote the comment, when it came from a remote thread rather than
    /// the current user.
    pub(super) author: Option<SharedString>,
    /// The remote thread the comment was posted to, with its replies.
    pub(super) thread: Option<ReviewThread>,
    /// The comment text parsed as markdown, for rendering.
    pub(super) markdown: Entity<Markdown>,
    /// The replies parsed as markdown, in the same order as the thread's.
    pub(super) reply_markdown: Vec<Entity<Markdown>>,
}

/// About how many lines `text` takes once wrapped, so a comment's block can
/// be sized before it renders. Long comments are clipped rather than endless.
pub(super) fn estimated_body_lines(text: &str) -> u32 {
    const CHARS_PER_LINE: usize = 100;
    const MAX_LINES: u32 = 10;
    let mut lines = 0.0;
    for line in text.lines() {
        let chars = line.trim().chars().count();
        lines += if chars == 0 {
            0.5
        } else {
            chars.div_ceil(CHARS_PER_LINE) as f32
        };
    }
    (lines.ceil() as u32).clamp(1, MAX_LINES)
}

fn parse_markdown(editor: &Editor, text: &str, cx: &mut Context<Editor>) -> Entity<Markdown> {
    let text: SharedString = text.to_string().into();
    let languages = editor
        .project
        .as_ref()
        .map(|project| project.read(cx).languages().clone());
    cx.new(|cx| {
        // Review comments carry code blocks, diagrams, and HTML details
        // blocks, which the defaults leave as text.
        Markdown::new_with_options(
            text,
            languages,
            None,
            MarkdownOptions {
                parse_html: true,
                render_mermaid_diagrams: true,
                ..Default::default()
            },
            cx,
        )
    })
}

/// Represents an active diff review overlay that appears when clicking the "Add Review" button.
pub(super) struct DiffReviewOverlay {
    pub(super) anchor_range: Range<Anchor>,
    /// The block ID for the overlay.
    pub(super) block_id: CustomBlockId,
    /// The editor entity for the review input.
    pub(super) prompt_editor: Entity<Editor>,
    /// The hunk key this overlay belongs to.
    pub(super) hunk_key: DiffHunkKey,
    /// Whether the comments section is expanded.
    pub(super) comments_expanded: bool,
    /// Editors for comments currently being edited inline.
    /// Key: comment ID, Value: Editor entity for inline editing.
    pub(super) inline_edit_editors: HashMap<usize, Entity<Editor>>,
    /// Subscriptions for inline edit editors' action handlers.
    /// Key: comment ID, Value: Subscription keeping the Newline action handler alive.
    pub(super) inline_edit_subscriptions: HashMap<usize, Subscription>,
    /// Editors for replies being written under posted comments, by comment id.
    pub(super) reply_editors: HashMap<usize, Entity<Editor>>,
    pub(super) reply_subscriptions: HashMap<usize, Subscription>,
    /// The current user's avatar URI for display in comment rows.
    pub(super) user_avatar_uri: Option<SharedUri>,
    /// Subscription to keep the action handler alive.
    _subscription: Subscription,
}

impl DiffReviewDragState {
    pub(super) fn row_range(
        &self,
        snapshot: &DisplaySnapshot,
    ) -> std::ops::RangeInclusive<DisplayRow> {
        let start = self.start_anchor.to_display_point(snapshot).row();
        let current = self.current_anchor.to_display_point(snapshot).row();

        (start..=current).sorted()
    }
}

impl StoredReviewComment {
    fn new(
        id: usize,
        comment: String,
        anchor_range: Range<Anchor>,
        markdown: Entity<Markdown>,
    ) -> Self {
        Self {
            id,
            comment,
            range: anchor_range,
            is_editing: false,
            author: None,
            thread: None,
            markdown,
            reply_markdown: Vec::new(),
        }
    }

    /// The lines this comment and its replies take when expanded: a header
    /// line each, plus the body.
    fn estimated_lines(&self) -> u32 {
        let replies: u32 = self
            .thread
            .as_ref()
            .map(|thread| {
                thread
                    .replies
                    .iter()
                    .map(|reply| 1 + estimated_body_lines(&reply.body))
                    .sum()
            })
            .unwrap_or(0);
        1 + estimated_body_lines(&self.comment) + replies
    }
}

/// A stored review comment, with what a caller needs to post it elsewhere.
#[derive(Clone, Debug)]
pub struct ReviewCommentSnapshot {
    pub id: usize,
    /// The file path, relative to the worktree.
    pub file_path: Arc<util::rel_path::RelPath>,
    pub range: Range<Anchor>,
    pub text: String,
    /// Who wrote the comment, when it came from a remote thread rather
    /// than the current user.
    pub author: Option<SharedString>,
    /// The remote thread the comment is on, if it has been posted.
    pub thread: Option<ReviewThread>,
}

impl Editor {
    pub fn diff_hunks_in_ranges<'a>(
        &'a self,
        ranges: &'a [Range<Anchor>],
        buffer: &'a MultiBufferSnapshot,
    ) -> impl 'a + Iterator<Item = MultiBufferDiffHunk> {
        ranges.iter().flat_map(move |range| {
            let end_excerpt = buffer.excerpt_containing(range.end..range.end);
            let range = range.to_point(buffer);
            let mut peek_end = range.end;
            if range.end.row < buffer.max_row().0 {
                peek_end = Point::new(range.end.row + 1, 0);
            }
            buffer
                .diff_hunks_in_range(range.start..peek_end)
                .filter(move |hunk| {
                    if let Some((_, excerpt_range)) = &end_excerpt
                        && let Some(end_anchor) =
                            buffer.anchor_in_excerpt(excerpt_range.context.end)
                        && let Some(hunk_end_anchor) =
                            buffer.anchor_in_excerpt(hunk.excerpt_range.context.end)
                        && hunk_end_anchor.cmp(&end_anchor, buffer).is_gt()
                    {
                        false
                    } else {
                        true
                    }
                })
        })
    }

    fn resolve_diff_hunks(
        &self,
        hunks: Vec<MultiBufferDiffHunk>,
        cx: &App,
    ) -> Vec<ResolvedDiffHunks> {
        let multibuffer = self.buffer().read(cx);
        let chunk_by = hunks.into_iter().chunk_by(|hunk| hunk.buffer_id);
        let mut resolved = Vec::new();

        for (source_buffer_id, hunks) in &chunk_by {
            let Some(diff) = multibuffer.diff_for(source_buffer_id) else {
                continue;
            };
            let diff_snapshot = diff.read(cx).snapshot(cx);
            let main_buffer_id = diff_snapshot.buffer_id();
            let buffer = multibuffer.buffer(main_buffer_id).or_else(|| {
                self.project
                    .as_ref()
                    .and_then(|project| project.read(cx).buffer_for_id(main_buffer_id, cx))
            });
            let mut resolved_hunks = Vec::new();

            for hunk in hunks {
                if hunk.buffer_id == main_buffer_id {
                    resolved_hunks.push(ResolvedDiffHunk {
                        buffer_range: hunk.buffer_range,
                        diff_base_byte_range: hunk.diff_base_byte_range.start.0
                            ..hunk.diff_base_byte_range.end.0,
                        status: hunk.status,
                    });
                } else {
                    let diff_base_byte_range =
                        hunk.diff_base_byte_range.start.0..hunk.diff_base_byte_range.end.0;
                    let Some(hunk) = diff_snapshot
                        .hunks_intersecting_base_text_range(
                            diff_base_byte_range.clone(),
                            diff_snapshot.buffer_snapshot(),
                        )
                        .find(|hunk| hunk.diff_base_byte_range == diff_base_byte_range)
                    else {
                        continue;
                    };
                    let kind = if hunk.buffer_range.start == hunk.buffer_range.end {
                        DiffHunkStatusKind::Deleted
                    } else if hunk.diff_base_byte_range.is_empty() {
                        DiffHunkStatusKind::Added
                    } else {
                        DiffHunkStatusKind::Modified
                    };
                    resolved_hunks.push(ResolvedDiffHunk {
                        buffer_range: hunk.buffer_range,
                        diff_base_byte_range: hunk.diff_base_byte_range,
                        status: DiffHunkStatus {
                            kind,
                            secondary: hunk.secondary_status,
                        },
                    });
                }
            }

            if !resolved_hunks.is_empty() {
                resolved.push(ResolvedDiffHunks {
                    diff,
                    buffer,
                    hunks: resolved_hunks,
                });
            }
        }

        resolved
    }

    pub fn diff_hunk_renderer(&self) -> Arc<dyn DiffHunkRenderer> {
        self.diff_hunk_renderer
            .clone()
            .unwrap_or_else(|| Arc::new(DefaultDiffHunkRenderer))
    }

    pub fn set_diff_hunk_renderer(
        &mut self,
        renderer: Option<Arc<dyn DiffHunkRenderer>>,
        cx: &mut Context<Self>,
    ) {
        let had_renderer = self.diff_hunk_renderer.is_some();
        let has_renderer = renderer.is_some();
        self.diff_hunk_renderer = renderer;

        if !had_renderer && has_renderer {
            self.load_diff_task.take();
        } else if had_renderer && !has_renderer {
            self.buffer.update(cx, |buffer, cx| {
                buffer.set_all_diff_hunks_collapsed(cx);
            });

            if let Some(project) = self.project.clone() {
                self.load_diff_task = Some(
                    self.update_uncommitted_diff_for_buffer(
                        &project,
                        self.buffer.read(cx).all_buffers(),
                        cx,
                    )
                    .shared(),
                );
            }
        }

        cx.notify();
    }

    pub(crate) fn set_diff_hunk_action_target(&mut self, target: Option<WeakEntity<Editor>>) {
        self.diff_hunk_action_target = target;
    }

    pub fn git_blame_inline_enabled(&self) -> bool {
        self.git_blame_inline_enabled
    }

    pub fn blame(&self) -> Option<&Entity<GitBlame>> {
        self.blame.as_ref()
    }

    pub fn active_git_blame_entry(&self, cx: &mut App) -> Option<BlameEntry> {
        if !self.show_git_blame_inline
            || self.newest_selection_head_on_empty_line(cx)
            || !self.has_blame_entries(cx)
        {
            return None;
        }

        let blame = self.blame.as_ref()?;
        let snapshot = self.display_snapshot(cx);
        let cursor = self.selections.newest::<Point>(&snapshot).head();
        let (buffer, point) = snapshot.buffer_snapshot().point_to_buffer_point(cursor)?;

        blame
            .update(cx, |blame, cx| {
                blame
                    .blame_for_rows(
                        &[RowInfo {
                            buffer_id: Some(buffer.remote_id()),
                            buffer_row: Some(point.row),
                            ..Default::default()
                        }],
                        cx,
                    )
                    .next()
            })
            .flatten()
            .map(|(_, entry)| entry)
    }

    pub fn show_git_blame_gutter(&self) -> bool {
        self.show_git_blame_gutter
    }

    pub fn expand_selected_diff_hunks(&mut self, cx: &mut Context<Self>) {
        let ranges: Vec<_> = self
            .selections
            .disjoint_anchors()
            .iter()
            .map(|s| s.range())
            .collect();
        self.buffer
            .update(cx, |buffer, cx| buffer.expand_diff_hunks(ranges, cx))
    }

    pub fn toggle_git_blame(
        &mut self,
        _: &::git::Blame,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_git_blame_gutter = !self.show_git_blame_gutter;

        if self.show_git_blame_gutter && !self.has_blame_entries(cx) {
            self.start_git_blame(true, window, cx);
        }

        cx.notify();
    }

    pub fn toggle_git_blame_inline(
        &mut self,
        _: &ToggleGitBlameInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_git_blame_inline_internal(true, window, cx);
        cx.notify();
    }

    /// Hides the inline blame popover element, in case it's already visible, or
    /// interrupts the task meant to show it, in case the task is running.
    ///
    /// When `ignore_timeout` is set to `true`, the popover is hidden
    /// immediately, otherwise it'll be hidden after a short delay.
    ///
    /// Returns `true` if the popover was visible and was hidden, `false`
    /// otherwise.
    pub fn hide_blame_popover(&mut self, ignore_timeout: bool, cx: &mut Context<Self>) -> bool {
        self.inline_blame_popover_show_task.take();

        if let Some(state) = &mut self.inline_blame_popover {
            if ignore_timeout {
                self.inline_blame_popover.take();
                cx.notify();
            } else {
                state.hide_task = Some(cx.spawn(async move |editor, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(100))
                        .await;

                    editor
                        .update(cx, |editor, cx| {
                            editor.inline_blame_popover.take();
                            cx.notify();
                        })
                        .ok();
                }));
            }

            true
        } else {
            false
        }
    }

    pub fn git_restore(&mut self, _: &Restore, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only(cx) {
            return;
        }
        let selections = self
            .selections
            .all(&self.display_snapshot(cx))
            .into_iter()
            .map(|s| s.range())
            .collect();
        self.restore_hunks_in_ranges(selections, window, cx);
    }

    pub fn status_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<FileStatus> {
        if let Some(status) = self
            .addons
            .iter()
            .find_map(|(_, addon)| addon.override_status_for_buffer_id(buffer_id, cx))
        {
            return Some(status);
        }
        self.project
            .as_ref()?
            .read(cx)
            .status_for_buffer_id(buffer_id, cx)
    }

    pub fn go_to_hunk_before_or_after_position(
        &mut self,
        snapshot: &EditorSnapshot,
        position: Point,
        direction: Direction,
        wrap_around: bool,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let row = if direction == Direction::Next {
            self.hunk_after_position(snapshot, position, wrap_around)
                .map(|hunk| hunk.row_range.start)
        } else {
            self.hunk_before_position(snapshot, position, wrap_around)
        };

        if let Some(row) = row {
            let destination = Point::new(row.0, 0);
            let autoscroll = Autoscroll::center();

            self.unfold_ranges(&[destination..destination], false, false, cx);
            self.change_selections(SelectionEffects::scroll(autoscroll), window, cx, |s| {
                s.select_ranges([destination..destination]);
            });
        }
    }

    pub fn set_expand_all_diff_hunks(&mut self, cx: &mut App) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.set_all_diff_hunks_expanded(cx);
        });
    }

    pub fn expand_all_diff_hunks(
        &mut self,
        _: &ExpandAllDiffHunks,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.expand_diff_hunks(vec![Anchor::Min..Anchor::Max], cx)
        });
    }

    pub fn show_diff_review_overlay(
        &mut self,
        display_range: Range<DisplayRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Range { start, end } = display_range.sorted();
        let editor_snapshot = self.snapshot(window, cx);

        // Convert display rows to multibuffer points
        let start_point = editor_snapshot
            .display_snapshot
            .display_point_to_point(start.as_display_point(), Bias::Left);
        let end_point = editor_snapshot
            .display_snapshot
            .display_point_to_point(end.as_display_point(), Bias::Left);
        self.open_diff_review_overlay(start_point..end_point, None, true, true, window, cx);
    }

    /// Opens the review overlay below the lines in `points`, or finds the one
    /// already there, and returns the hunk key its comments are stored under.
    /// `file_path` names the file when the caller knows it, since the lookup
    /// by point fails at the first row of an excerpt.
    fn open_diff_review_overlay(
        &mut self,
        points: Range<Point>,
        file_path: Option<Arc<util::rel_path::RelPath>>,
        focus: bool,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DiffHunkKey {
        let Range {
            start: start_point,
            end: end_point,
        } = points;
        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let end_multi_buffer_row = MultiBufferRow(end_point.row);

        // Create anchor range for the selected lines (start of first line to end of last line)
        let line_end = Point::new(
            end_point.row,
            buffer_snapshot.line_len(end_multi_buffer_row),
        );
        let anchor_range =
            buffer_snapshot.anchor_after(start_point)..buffer_snapshot.anchor_before(line_end);

        // Compute the hunk key for this display row
        let file_path = file_path
            .or_else(|| {
                buffer_snapshot
                    .file_at(start_point)
                    .map(|file: &Arc<dyn language::File>| file.path().clone())
            })
            .unwrap_or_else(|| Arc::from(util::rel_path::RelPath::empty()));
        let hunk_start_anchor = buffer_snapshot.anchor_before(start_point);
        let new_hunk_key = DiffHunkKey {
            file_path,
            hunk_start_anchor,
        };

        // Check if we already have an overlay for this hunk
        if let Some(existing_overlay) = self.diff_review_overlays.iter().find(|overlay| {
            Self::hunk_keys_match(&overlay.hunk_key, &new_hunk_key, &buffer_snapshot)
        }) {
            // Just focus the existing overlay's prompt editor
            if focus {
                let focus_handle = existing_overlay.prompt_editor.focus_handle(cx);
                window.focus(&focus_handle, cx);
            }
            return new_hunk_key;
        }

        // Dismiss overlays that have no comments for their hunks
        if focus {
            self.dismiss_overlays_without_comments(cx);
        }

        // Get the current user's avatar URI from the project's user_store
        let user_avatar_uri = self.project.as_ref().and_then(|project| {
            let user_store = project.read(cx).user_store();
            user_store
                .read(cx)
                .current_user()
                .map(|user| user.avatar_uri.clone())
        });

        // Create anchor at the end of the last row so the block appears immediately below it
        // Use multibuffer coordinates for anchor creation
        let line_len = buffer_snapshot.line_len(end_multi_buffer_row);
        let anchor = buffer_snapshot.anchor_after(Point::new(end_multi_buffer_row.0, line_len));

        // Use the hunk key we already computed
        let hunk_key = new_hunk_key;

        // Create the prompt editor for the review input
        let prompt_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Add a review comment...", window, cx);
            editor
        });

        // Register the Newline action on the prompt editor to submit the review
        let parent_editor = cx.entity().downgrade();
        let subscription = prompt_editor.update(cx, |prompt_editor, _cx| {
            prompt_editor.register_action({
                let parent_editor = parent_editor.clone();
                move |_: &crate::actions::Newline, window, cx| {
                    if let Some(editor) = parent_editor.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.submit_diff_review_comment(window, cx);
                        });
                    }
                }
            })
        });

        // Calculate initial height based on existing comments for this hunk
        let initial_height = self.calculate_overlay_height(&hunk_key, expanded, &buffer_snapshot);

        // Create the overlay block
        let prompt_editor_for_render = prompt_editor.clone();
        let hunk_key_for_render = hunk_key.clone();
        let editor_handle = cx.entity().downgrade();
        let block = BlockProperties {
            style: BlockStyle::Sticky,
            placement: BlockPlacement::Below(anchor),
            height: Some(initial_height),
            render: Arc::new(move |cx| {
                Self::render_diff_review_overlay(
                    &prompt_editor_for_render,
                    &hunk_key_for_render,
                    &editor_handle,
                    cx,
                )
            }),
            priority: 0,
        };

        let block_ids = self.insert_blocks([block], None, cx);
        let Some(block_id) = block_ids.into_iter().next() else {
            log::error!("Failed to insert diff review overlay block");
            return hunk_key;
        };

        self.diff_review_overlays.push(DiffReviewOverlay {
            anchor_range,
            block_id,
            prompt_editor: prompt_editor.clone(),
            hunk_key: hunk_key.clone(),
            comments_expanded: expanded,
            inline_edit_editors: HashMap::default(),
            inline_edit_subscriptions: HashMap::default(),
            reply_editors: HashMap::default(),
            reply_subscriptions: HashMap::default(),
            user_avatar_uri,
            _subscription: subscription,
        });

        // Focus the prompt editor
        if focus {
            let focus_handle = prompt_editor.focus_handle(cx);
            window.focus(&focus_handle, cx);
        }
        cx.notify();
        hunk_key
    }

    /// Stores the diff review comment locally.
    /// Comments are stored per-hunk and can later be batch-submitted to the Agent panel.
    pub fn submit_diff_review_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Find the overlay that currently has focus
        let overlay_index = self
            .diff_review_overlays
            .iter()
            .position(|overlay| overlay.prompt_editor.focus_handle(cx).is_focused(window));
        let Some(overlay_index) = overlay_index else {
            return;
        };
        let overlay = &self.diff_review_overlays[overlay_index];

        let comment_text = overlay.prompt_editor.read(cx).text(cx).trim().to_string();
        if comment_text.is_empty() {
            return;
        }

        let anchor_range = overlay.anchor_range.clone();
        let hunk_key = overlay.hunk_key.clone();

        self.add_review_comment(hunk_key.clone(), comment_text, anchor_range, cx);

        // Clear the prompt editor but keep the overlay open
        if let Some(overlay) = self.diff_review_overlays.get(overlay_index) {
            overlay.prompt_editor.update(cx, |editor, cx| {
                editor.clear(window, cx);
            });
        }

        // Refresh the overlay to update the block height for the new comment
        self.refresh_diff_review_overlay_height(&hunk_key, window, cx);

        cx.notify();
    }

    /// Returns the prompt editor for the diff review overlay, if one is active.
    /// This is primarily used for testing.
    pub fn diff_review_prompt_editor(&self) -> Option<&Entity<Editor>> {
        self.diff_review_overlays
            .first()
            .map(|overlay| &overlay.prompt_editor)
    }

    /// Sets whether the comments section is expanded in the diff review overlay.
    /// This is primarily used for testing.
    pub fn set_diff_review_comments_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        for overlay in &mut self.diff_review_overlays {
            overlay.comments_expanded = expanded;
        }
        cx.notify();
    }

    /// Returns the total count of stored review comments across all hunks.
    pub(super) fn total_review_comment_count(&self) -> usize {
        self.stored_review_comments
            .iter()
            .map(|(_, v)| v.len())
            .sum()
    }

    /// Every stored review comment, in the order they were added within each hunk.
    pub fn review_comments(&self) -> Vec<ReviewCommentSnapshot> {
        self.stored_review_comments
            .iter()
            .flat_map(|(key, comments)| {
                comments.iter().map(|comment| ReviewCommentSnapshot {
                    id: comment.id,
                    file_path: key.file_path.clone(),
                    range: comment.range.clone(),
                    text: comment.comment.clone(),
                    author: comment.author.clone(),
                    thread: comment.thread.clone(),
                })
            })
            .collect()
    }

    /// Records that the comment with `id` is now on the remote thread
    /// `discussion_id`, so it stays in the editor and can gather replies.
    pub fn mark_review_comment_posted(
        &mut self,
        id: usize,
        discussion_id: String,
        note_id: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.update_review_comment_with(
            |comment| comment.id == id,
            |comment| {
                comment.thread = Some(ReviewThread {
                    discussion_id,
                    note_id,
                    ..ReviewThread::default()
                })
            },
            window,
            cx,
        )
    }

    /// Replaces the replies and the resolved state shown under the comment
    /// on thread `discussion_id`.
    pub fn set_review_thread_state(
        &mut self,
        discussion_id: &str,
        note_id: Option<u64>,
        replies: Vec<ReviewReply>,
        resolved: bool,
        since_last_review: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let reply_markdown: Vec<Entity<Markdown>> = replies
            .iter()
            .map(|reply| parse_markdown(self, &reply.body, cx))
            .collect();
        self.update_review_comment_with(
            |comment| {
                comment
                    .thread
                    .as_ref()
                    .is_some_and(|thread| thread.discussion_id == discussion_id)
            },
            |comment| {
                if let Some(thread) = comment.thread.as_mut() {
                    thread.note_id = note_id.or(thread.note_id);
                    thread.replies = replies;
                    thread.resolved = resolved;
                    thread.since_last_review = since_last_review;
                    comment.reply_markdown = reply_markdown;
                }
            },
            window,
            cx,
        )
    }

    /// Marks the thread `discussion_id` resolved or open, after GitLab did.
    pub fn set_review_thread_resolved(
        &mut self,
        discussion_id: &str,
        resolved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.update_review_comment_with(
            |comment| {
                comment
                    .thread
                    .as_ref()
                    .is_some_and(|thread| thread.discussion_id == discussion_id)
            },
            |comment| {
                if let Some(thread) = comment.thread.as_mut() {
                    thread.resolved = resolved;
                }
            },
            window,
            cx,
        )
    }

    /// Shows `reply` at the end of the thread on `discussion_id`, after it
    /// reached GitLab.
    pub fn append_review_reply(
        &mut self,
        discussion_id: &str,
        reply: ReviewReply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let markdown = parse_markdown(self, &reply.body, cx);
        self.update_review_comment_with(
            |comment| {
                comment
                    .thread
                    .as_ref()
                    .is_some_and(|thread| thread.discussion_id == discussion_id)
            },
            |comment| {
                if let Some(thread) = comment.thread.as_mut() {
                    thread.replies.push(reply);
                    comment.reply_markdown.push(markdown);
                }
            },
            window,
            cx,
        )
    }

    /// The hunk key of the overlay holding the comment `comment_id`.
    fn hunk_key_for_comment(&self, comment_id: usize) -> Option<DiffHunkKey> {
        self.stored_review_comments
            .iter()
            .find(|(_, comments)| comments.iter().any(|comment| comment.id == comment_id))
            .map(|(key, _)| key.clone())
    }

    fn review_comment(&self, comment_id: usize) -> Option<&StoredReviewComment> {
        self.stored_review_comments
            .iter()
            .flat_map(|(_, comments)| comments)
            .find(|comment| comment.id == comment_id)
    }

    /// Opens a reply box under the posted comment `comment_id` and focuses
    /// it, or focuses the one already open.
    pub fn open_review_reply(
        &mut self,
        comment_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hunk_key) = self.hunk_key_for_comment(comment_id) else {
            return;
        };
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let Some(overlay) = self
            .diff_review_overlays
            .iter_mut()
            .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
        else {
            return;
        };
        if let Some(editor) = overlay.reply_editors.get(&comment_id) {
            let focus_handle = editor.focus_handle(cx);
            window.focus(&focus_handle, cx);
            return;
        }
        let reply_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Reply…", window, cx);
            editor
        });
        let parent_editor = cx.entity().downgrade();
        let subscription = reply_editor.update(cx, |reply_editor, _cx| {
            reply_editor.register_action(move |_: &crate::actions::Newline, window, cx| {
                parent_editor
                    .update(cx, |editor, cx| {
                        editor.submit_review_reply(comment_id, window, cx);
                    })
                    .ok();
            })
        });
        let focus_handle = reply_editor.focus_handle(cx);
        overlay.reply_editors.insert(comment_id, reply_editor);
        overlay.reply_subscriptions.insert(comment_id, subscription);
        window.focus(&focus_handle, cx);
        self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        cx.notify();
    }

    /// Puts `text` in the reply box under `comment_id`, opening it first,
    /// for the user to edit before sending.
    pub fn set_review_reply_draft(
        &mut self,
        comment_id: usize,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_review_reply(comment_id, window, cx);
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let Some(hunk_key) = self.hunk_key_for_comment(comment_id) else {
            return;
        };
        let Some(editor) = self
            .diff_review_overlays
            .iter()
            .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            .and_then(|overlay| overlay.reply_editors.get(&comment_id))
            .cloned()
        else {
            return;
        };
        editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
    }

    /// Hands the reply under `comment_id` to whoever owns the editor for
    /// sending, and closes the reply box.
    pub fn submit_review_reply(
        &mut self,
        comment_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(discussion_id) = self
            .review_comment(comment_id)
            .and_then(|comment| comment.thread.as_ref())
            .map(|thread| thread.discussion_id.clone())
        else {
            return;
        };
        let Some(hunk_key) = self.hunk_key_for_comment(comment_id) else {
            return;
        };
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let Some(overlay) = self
            .diff_review_overlays
            .iter()
            .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
        else {
            return;
        };
        let body = overlay
            .reply_editors
            .get(&comment_id)
            .map(|editor| editor.read(cx).text(cx).trim().to_string())
            .unwrap_or_default();
        if body.is_empty() {
            return;
        }
        self.cancel_review_reply(comment_id, window, cx);
        cx.emit(EditorEvent::ReviewReplySubmitted {
            comment_id,
            discussion_id,
            body,
        });
    }

    /// Closes the reply box under `comment_id`, dropping what was typed.
    pub fn cancel_review_reply(
        &mut self,
        comment_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hunk_key) = self.hunk_key_for_comment(comment_id) else {
            return;
        };
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let Some(overlay) = self
            .diff_review_overlays
            .iter_mut()
            .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
        else {
            return;
        };
        let had_focus = overlay
            .reply_editors
            .get(&comment_id)
            .is_some_and(|editor| editor.focus_handle(cx).contains_focused(window, cx));
        overlay.reply_editors.remove(&comment_id);
        overlay.reply_subscriptions.remove(&comment_id);
        if had_focus {
            let focus_handle = self.focus_handle(cx);
            window.focus(&focus_handle, cx);
        }
        self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        cx.notify();
    }

    /// Asks whoever owns the editor to flip the resolved state of the
    /// thread under `comment_id` on the remote.
    pub fn request_review_thread_resolve(&mut self, comment_id: usize, cx: &mut Context<Self>) {
        let Some(thread) = self
            .review_comment(comment_id)
            .and_then(|comment| comment.thread.clone())
        else {
            return;
        };
        cx.emit(EditorEvent::ReviewThreadResolveRequested {
            discussion_id: thread.discussion_id,
            resolved: !thread.resolved,
        });
    }

    /// Asks whoever owns the editor to draft a reply to the thread under
    /// `comment_id`, to be put in its reply box.
    pub fn request_review_reply_draft(&mut self, comment_id: usize, cx: &mut Context<Self>) {
        let Some(discussion_id) = self
            .review_comment(comment_id)
            .and_then(|comment| comment.thread.as_ref())
            .map(|thread| thread.discussion_id.clone())
        else {
            return;
        };
        cx.emit(EditorEvent::ReviewReplyDraftRequested {
            comment_id,
            discussion_id,
        });
    }

    /// Shows a comment under the lines in `range` without taking focus: a
    /// thread from the remote review by its `author`, or one of the current
    /// user's own comments being put back after the excerpts changed.
    pub fn add_review_comment_at(
        &mut self,
        file_path: Arc<util::rel_path::RelPath>,
        range: Range<Anchor>,
        text: String,
        author: Option<SharedString>,
        thread: Option<ReviewThread>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let points = range.start.to_point(&snapshot)..range.end.to_point(&snapshot);
        // Threads from other people open collapsed; the user's own stay open.
        let expanded = author.is_none();
        let hunk_key =
            self.open_diff_review_overlay(points, Some(file_path), false, expanded, window, cx);
        let id = self.add_review_comment(hunk_key, text, range, cx);
        let reply_markdown: Vec<Entity<Markdown>> = thread
            .iter()
            .flat_map(|thread| thread.replies.iter())
            .map(|reply| parse_markdown(self, &reply.body, cx))
            .collect();
        self.update_review_comment_with(
            |comment| comment.id == id,
            |comment| {
                comment.author = author;
                comment.thread = thread;
                comment.reply_markdown = reply_markdown;
            },
            window,
            cx,
        );
    }

    /// Expands or collapses the comments of the overlay for `hunk_key`.
    fn toggle_overlay_comments(
        &mut self,
        hunk_key: &DiffHunkKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let Some(overlay) = self
            .diff_review_overlays
            .iter_mut()
            .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
        else {
            return;
        };
        overlay.comments_expanded = !overlay.comments_expanded;
        let hunk_key = overlay.hunk_key.clone();
        self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        cx.notify();
    }

    /// Takes every review comment out of the editor as file and row records
    /// and closes the overlays, for putting back with
    /// [`Editor::add_review_comment_at`] once the excerpts have been rebuilt.
    pub fn take_review_comment_records(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Vec<ReviewCommentRecord> {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let buffer_row = |anchor: &Anchor| {
            let point = anchor.to_point(&snapshot);
            snapshot
                .point_to_buffer_point(point)
                .map_or(point.row, |(_, buffer_point)| buffer_point.row)
        };
        let records: Vec<ReviewCommentRecord> = self
            .stored_review_comments
            .iter()
            .flat_map(|(key, comments)| {
                comments.iter().map(|comment| ReviewCommentRecord {
                    file_path: key.file_path.clone(),
                    start_row: buffer_row(&comment.range.start),
                    end_row: buffer_row(&comment.range.end),
                    text: comment.comment.clone(),
                    author: comment.author.clone(),
                    thread: comment.thread.clone(),
                })
            })
            .collect();
        if records.is_empty() {
            return records;
        }
        self.dismiss_all_diff_review_overlays(cx);
        self.stored_review_comments.clear();
        cx.emit(EditorEvent::ReviewCommentsChanged { total_count: 0 });
        cx.notify();
        records
    }

    /// Applies `update` to the first comment matching `find` and refreshes
    /// the overlay showing it.
    fn update_review_comment_with(
        &mut self,
        find: impl Fn(&StoredReviewComment) -> bool,
        update: impl FnOnce(&mut StoredReviewComment),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((hunk_key, comment)) =
            self.stored_review_comments
                .iter_mut()
                .find_map(|(key, comments)| {
                    comments
                        .iter_mut()
                        .find(|comment| find(comment))
                        .map(|comment| (key.clone(), comment))
                })
        else {
            return false;
        };
        update(comment);
        self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        cx.emit(EditorEvent::ReviewCommentsChanged {
            total_count: self.total_review_comment_count(),
        });
        cx.notify();
        true
    }

    /// Adds a new review comment to a specific hunk.
    pub(super) fn add_review_comment(
        &mut self,
        hunk_key: DiffHunkKey,
        comment: String,
        anchor_range: Range<Anchor>,
        cx: &mut Context<Self>,
    ) -> usize {
        let id = self.next_review_comment_id;
        self.next_review_comment_id += 1;

        let markdown = parse_markdown(self, &comment, cx);
        let stored_comment = StoredReviewComment::new(id, comment, anchor_range, markdown);

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let key_point = hunk_key.hunk_start_anchor.to_point(&snapshot);

        // Find existing entry for this hunk or add a new one
        if let Some((_, comments)) = self.stored_review_comments.iter_mut().find(|(k, _)| {
            k.file_path == hunk_key.file_path
                && k.hunk_start_anchor.to_point(&snapshot) == key_point
        }) {
            comments.push(stored_comment);
        } else {
            self.stored_review_comments
                .push((hunk_key, vec![stored_comment]));
        }

        cx.emit(EditorEvent::ReviewCommentsChanged {
            total_count: self.total_review_comment_count(),
        });
        cx.notify();
        id
    }

    pub(super) fn blame_hover(
        &mut self,
        _: &BlameHover,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let just_started = self.blame.is_none();
        if just_started {
            self.start_git_blame(true, window, cx);
        }
        let Some(blame) = self.blame.as_ref() else {
            return;
        };

        if just_started && !blame.read(cx).has_generated_entries() {
            let subscription = cx.observe_in(blame, window, |editor, blame, window, cx| {
                if blame.read(cx).has_generated_entries() {
                    editor.pending_blame_hover_observation.take();
                    editor.show_blame_hover_popover(window, cx);
                }
            });
            self.pending_blame_hover_observation = Some(subscription);
            return;
        }

        self.show_blame_hover_popover(window, cx);
    }

    fn show_blame_hover_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.snapshot(window, cx);
        let cursor = self
            .selections
            .newest::<Point>(&snapshot.display_snapshot)
            .head();
        let Some((buffer, point)) = snapshot.buffer_snapshot().point_to_buffer_point(cursor) else {
            return;
        };

        let Some(blame) = self.blame.as_ref() else {
            return;
        };

        let row_info = RowInfo {
            buffer_id: Some(buffer.remote_id()),
            buffer_row: Some(point.row),
            ..Default::default()
        };
        let Some((buffer, blame_entry)) = blame
            .update(cx, |blame, cx| blame.blame_for_rows(&[row_info], cx).next())
            .flatten()
        else {
            return;
        };

        let anchor = self.selections.newest_anchor().head();
        let position = self.to_pixel_point(anchor, &snapshot, window, cx);
        if let (Some(position), Some(last_bounds)) = (position, self.last_bounds) {
            self.show_blame_popover(
                buffer,
                &blame_entry,
                position + last_bounds.origin,
                true,
                cx,
            );
        };
    }

    pub(super) fn restore_file(
        &mut self,
        _: &::git::RestoreFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        let mut buffer_ids = HashSet::default();
        let snapshot = self.buffer().read(cx).snapshot(cx);
        for selection in self
            .selections
            .all::<MultiBufferOffset>(&self.display_snapshot(cx))
        {
            buffer_ids.extend(snapshot.buffer_ids_for_range(selection.range()))
        }

        let ranges = buffer_ids
            .into_iter()
            .flat_map(|buffer_id| snapshot.range_for_buffer(buffer_id))
            .collect::<Vec<_>>();

        self.restore_hunks_in_ranges(ranges, window, cx);
    }

    /// Restores the diff hunks in the editor's selections and moves the cursor
    /// to the next diff hunk. Wraps around to the beginning of the buffer if
    /// not all diff hunks are expanded.
    pub(super) fn restore_and_next(
        &mut self,
        _: &::git::RestoreAndNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        let selections = self
            .selections
            .all(&self.display_snapshot(cx))
            .into_iter()
            .map(|selection| selection.range())
            .collect();

        self.restore_hunks_in_ranges(selections, window, cx);

        let all_diff_hunks_expanded = self.buffer().read(cx).all_diff_hunks_expanded();
        let wrap_around = !all_diff_hunks_expanded;
        let snapshot = self.snapshot(window, cx);
        let position = self
            .selections
            .newest::<Point>(&snapshot.display_snapshot)
            .head();

        self.go_to_hunk_before_or_after_position(
            &snapshot,
            position,
            Direction::Next,
            wrap_around,
            window,
            cx,
        );
    }

    fn restore_diff_hunks(&mut self, hunks: Vec<ResolvedDiffHunks>, cx: &mut Context<Self>) {
        let mut revert_changes = Vec::new();
        for hunks in hunks {
            let Some(buffer) = hunks.buffer else {
                continue;
            };
            let diff_snapshot = hunks.diff.read(cx).snapshot(cx);
            let changes = hunks
                .hunks
                .into_iter()
                .filter_map(|hunk| {
                    if hunk.diff_base_byte_range == (0..0)
                        && hunk.buffer_range.start.is_min()
                        && hunk.buffer_range.end.is_max()
                    {
                        return None;
                    }
                    let original_text = diff_snapshot
                        .base_text()
                        .as_rope()
                        .slice(hunk.diff_base_byte_range.start..hunk.diff_base_byte_range.end);
                    Some((hunk.buffer_range, original_text))
                })
                .collect::<Vec<_>>();
            if !changes.is_empty() {
                revert_changes.push((buffer, changes));
            }
        }

        for (buffer, changes) in revert_changes {
            buffer.update(cx, |buffer, cx| {
                buffer.edit(
                    changes
                        .into_iter()
                        .map(|(range, text)| (range, text.to_string())),
                    None,
                    cx,
                );
            });
        }
    }

    pub(super) fn go_to_next_hunk(
        &mut self,
        _: &GoToHunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.snapshot(window, cx);
        let selection = self.selections.newest::<Point>(&self.display_snapshot(cx));
        self.go_to_hunk_before_or_after_position(
            &snapshot,
            selection.head(),
            Direction::Next,
            true,
            window,
            cx,
        );
    }

    pub(super) fn collapse_all_diff_hunks(
        &mut self,
        _: &CollapseAllDiffHunks,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.collapse_diff_hunks(vec![Anchor::Min..Anchor::Max], cx)
        });
    }

    pub fn toggle_all_diff_hunks(
        &mut self,
        _: &ToggleAllDiffHunks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_any_expanded_diff_hunks(cx) {
            self.collapse_all_diff_hunks(&CollapseAllDiffHunks, window, cx);
        } else {
            self.expand_all_diff_hunks(&ExpandAllDiffHunks, window, cx);
        }
    }

    pub(super) fn toggle_selected_diff_hunks(
        &mut self,
        _: &ToggleSelectedDiffHunks,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ranges: Vec<_> = self
            .selections
            .disjoint_anchors()
            .iter()
            .map(|s| s.range())
            .collect();
        self.toggle_diff_hunks_in_ranges(ranges, cx);
    }

    pub(super) fn show_diff_review_button(&self) -> bool {
        self.show_diff_review_button
    }

    pub(super) fn render_diff_review_button(
        &self,
        display_row: DisplayRow,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let text_color = cx.theme().colors().text;
        let icon_color = cx.theme().colors().icon_accent;

        h_flex()
            .id("diff_review_button")
            .cursor_pointer()
            .w(width - px(1.))
            .h(relative(0.9))
            .justify_center()
            .rounded_sm()
            .border_1()
            .border_color(text_color.opacity(0.1))
            .bg(text_color.opacity(0.15))
            .hover(|s| {
                s.bg(icon_color.opacity(0.4))
                    .border_color(icon_color.opacity(0.5))
            })
            .child(Icon::new(IconName::Plus).size(IconSize::Small))
            .tooltip(Tooltip::text("Add Review (drag to select multiple lines)"))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |editor, _event: &gpui::MouseDownEvent, window, cx| {
                    editor.start_diff_review_drag(display_row, window, cx);
                }),
            )
    }

    pub(super) fn start_diff_review_drag(
        &mut self,
        display_row: DisplayRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.snapshot(window, cx);
        let point = snapshot
            .display_snapshot
            .display_point_to_point(DisplayPoint::new(display_row, 0), Bias::Left);
        let anchor = snapshot.buffer_snapshot().anchor_before(point);
        self.diff_review_drag_state = Some(DiffReviewDragState {
            start_anchor: anchor,
            current_anchor: anchor,
        });
        cx.notify();
    }

    pub(super) fn update_diff_review_drag(
        &mut self,
        display_row: DisplayRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.diff_review_drag_state.is_none() {
            return;
        }
        let snapshot = self.snapshot(window, cx);
        let point = snapshot
            .display_snapshot
            .display_point_to_point(display_row.as_display_point(), Bias::Left);
        let anchor = snapshot.buffer_snapshot().anchor_before(point);
        if let Some(drag_state) = &mut self.diff_review_drag_state {
            drag_state.current_anchor = anchor;
            cx.notify();
        }
    }

    pub(super) fn end_diff_review_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(drag_state) = self.diff_review_drag_state.take() {
            let snapshot = self.snapshot(window, cx);
            let range = drag_state.row_range(&snapshot.display_snapshot);
            self.show_diff_review_overlay(*range.start()..*range.end(), window, cx);
        }
        cx.notify();
    }

    pub(super) fn cancel_diff_review_drag(&mut self, cx: &mut Context<Self>) {
        self.diff_review_drag_state = None;
        cx.notify();
    }

    /// Dismisses all diff review overlays.
    pub(super) fn dismiss_all_diff_review_overlays(&mut self, cx: &mut Context<Self>) {
        if self.diff_review_overlays.is_empty() {
            return;
        }
        let block_ids: HashSet<_> = self
            .diff_review_overlays
            .drain(..)
            .map(|overlay| overlay.block_id)
            .collect();
        self.remove_blocks(block_ids, None, cx);
        cx.notify();
    }

    /// Action handler for SubmitDiffReviewComment.
    pub(super) fn submit_diff_review_comment_action(
        &mut self,
        _: &SubmitDiffReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_diff_review_comment(window, cx);
    }

    /// Returns comments for a specific hunk, ordered by creation time.
    pub(super) fn comments_for_hunk<'a>(
        &'a self,
        key: &DiffHunkKey,
        snapshot: &MultiBufferSnapshot,
    ) -> &'a [StoredReviewComment] {
        let key_point = key.hunk_start_anchor.to_point(snapshot);
        self.stored_review_comments
            .iter()
            .find(|(k, _)| {
                k.file_path == key.file_path && k.hunk_start_anchor.to_point(snapshot) == key_point
            })
            .map(|(_, comments)| comments.as_slice())
            .unwrap_or(&[])
    }

    /// Returns the count of comments for a specific hunk.
    pub(super) fn hunk_comment_count(
        &self,
        key: &DiffHunkKey,
        snapshot: &MultiBufferSnapshot,
    ) -> usize {
        let key_point = key.hunk_start_anchor.to_point(snapshot);
        self.stored_review_comments
            .iter()
            .find(|(k, _)| {
                k.file_path == key.file_path && k.hunk_start_anchor.to_point(snapshot) == key_point
            })
            .map(|(_, v)| v.len())
            .unwrap_or(0)
    }

    /// Removes a review comment by ID from any hunk.
    pub fn remove_review_comment(&mut self, id: usize, cx: &mut Context<Self>) -> bool {
        for (_, comments) in self.stored_review_comments.iter_mut() {
            if let Some(index) = comments.iter().position(|c| c.id == id) {
                comments.remove(index);
                cx.emit(EditorEvent::ReviewCommentsChanged {
                    total_count: self.total_review_comment_count(),
                });
                cx.notify();
                return true;
            }
        }
        false
    }

    /// Updates a review comment's text by ID.
    pub(super) fn update_review_comment(
        &mut self,
        id: usize,
        new_comment: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let markdown = parse_markdown(self, &new_comment, cx);
        let mut edited_thread = None;
        for (_, comments) in self.stored_review_comments.iter_mut() {
            if let Some(comment) = comments.iter_mut().find(|c| c.id == id) {
                comment.markdown = markdown;
                comment.comment = new_comment.clone();
                comment.is_editing = false;
                // A comment of the user's own that is already on the remote
                // is changed there too.
                if comment.author.is_none()
                    && let Some(thread) = comment.thread.as_ref()
                    && let Some(note_id) = thread.note_id
                {
                    edited_thread = Some((thread.discussion_id.clone(), note_id));
                }
                break;
            }
        }
        let Some(_) = self.review_comment(id) else {
            return false;
        };
        cx.emit(EditorEvent::ReviewCommentsChanged {
            total_count: self.total_review_comment_count(),
        });
        if let Some((discussion_id, note_id)) = edited_thread {
            cx.emit(EditorEvent::ReviewCommentEdited {
                discussion_id,
                note_id,
                body: new_comment,
            });
        }
        cx.notify();
        true
    }

    /// Sets a comment's editing state.
    pub(super) fn set_comment_editing(
        &mut self,
        id: usize,
        is_editing: bool,
        cx: &mut Context<Self>,
    ) {
        for (_, comments) in self.stored_review_comments.iter_mut() {
            if let Some(comment) = comments.iter_mut().find(|c| c.id == id) {
                comment.is_editing = is_editing;
                cx.notify();
                return;
            }
        }
    }

    /// Removes review comments whose anchors are no longer valid or whose
    /// associated diff hunks no longer exist.
    ///
    /// This should be called when the buffer changes to prevent orphaned comments
    /// from accumulating.
    pub(super) fn cleanup_orphaned_review_comments(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let original_count = self.total_review_comment_count();

        // Remove comments with invalid hunk anchors
        self.stored_review_comments
            .retain(|(hunk_key, _)| hunk_key.hunk_start_anchor.is_valid(&snapshot));

        // Also clean up individual comments with invalid anchor ranges
        for (_, comments) in &mut self.stored_review_comments {
            comments.retain(|comment| {
                comment.range.start.is_valid(&snapshot) && comment.range.end.is_valid(&snapshot)
            });
        }

        // Remove empty hunk entries
        self.stored_review_comments
            .retain(|(_, comments)| !comments.is_empty());

        let new_count = self.total_review_comment_count();
        if new_count != original_count {
            cx.emit(EditorEvent::ReviewCommentsChanged {
                total_count: new_count,
            });
            cx.notify();
        }
    }

    /// Toggles the expanded state of the comments section in the overlay.
    pub(super) fn toggle_review_comments_expanded(
        &mut self,
        _: &ToggleReviewCommentsExpanded,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Find the overlay that currently has focus, or use the first one
        let overlay_info = self.diff_review_overlays.iter_mut().find_map(|overlay| {
            if overlay.prompt_editor.focus_handle(cx).is_focused(window) {
                overlay.comments_expanded = !overlay.comments_expanded;
                Some(overlay.hunk_key.clone())
            } else {
                None
            }
        });

        // If no focused overlay found, toggle the first one
        let hunk_key = overlay_info.or_else(|| {
            self.diff_review_overlays.first_mut().map(|overlay| {
                overlay.comments_expanded = !overlay.comments_expanded;
                overlay.hunk_key.clone()
            })
        });

        if let Some(hunk_key) = hunk_key {
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
            cx.notify();
        }
    }

    /// Handles the EditReviewComment action - sets a comment into editing mode.
    pub(super) fn edit_review_comment(
        &mut self,
        action: &EditReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let comment_id = action.id;

        // Set the comment to editing mode
        self.set_comment_editing(comment_id, true, cx);

        // Find the overlay that contains this comment and create an inline editor if needed
        // First, find which hunk this comment belongs to
        let hunk_key = self
            .stored_review_comments
            .iter()
            .find_map(|(key, comments)| {
                if comments.iter().any(|c| c.id == comment_id) {
                    Some(key.clone())
                } else {
                    None
                }
            });

        let snapshot = self.buffer.read(cx).snapshot(cx);
        if let Some(hunk_key) = hunk_key {
            if let Some(overlay) = self
                .diff_review_overlays
                .iter_mut()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    overlay.inline_edit_editors.entry(comment_id)
                {
                    // Find the comment text
                    let comment_text = self
                        .stored_review_comments
                        .iter()
                        .flat_map(|(_, comments)| comments)
                        .find(|c| c.id == comment_id)
                        .map(|c| c.comment.clone())
                        .unwrap_or_default();

                    // Create inline editor
                    let parent_editor = cx.entity().downgrade();
                    let inline_editor = cx.new(|cx| {
                        let mut editor = Editor::single_line(window, cx);
                        editor.set_text(&*comment_text, window, cx);
                        // Select all text for easy replacement
                        editor.select_all(&crate::actions::SelectAll, window, cx);
                        editor
                    });

                    // Register the Newline action to confirm the edit
                    let subscription = inline_editor.update(cx, |inline_editor, _cx| {
                        inline_editor.register_action({
                            let parent_editor = parent_editor.clone();
                            move |_: &crate::actions::Newline, window, cx| {
                                if let Some(editor) = parent_editor.upgrade() {
                                    editor.update(cx, |editor, cx| {
                                        editor.confirm_edit_review_comment(comment_id, window, cx);
                                    });
                                }
                            }
                        })
                    });

                    // Store the subscription to keep the action handler alive
                    overlay
                        .inline_edit_subscriptions
                        .insert(comment_id, subscription);

                    // Focus the inline editor
                    let focus_handle = inline_editor.focus_handle(cx);
                    window.focus(&focus_handle, cx);

                    entry.insert(inline_editor);
                }
            }
        }

        cx.notify();
    }

    /// Confirms an inline edit of a review comment.
    pub(super) fn confirm_edit_review_comment(
        &mut self,
        comment_id: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Get the new text from the inline editor
        // Find the overlay containing this comment's inline editor
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let hunk_key = self
            .stored_review_comments
            .iter()
            .find_map(|(key, comments)| {
                if comments.iter().any(|c| c.id == comment_id) {
                    Some(key.clone())
                } else {
                    None
                }
            });

        let new_text = hunk_key
            .as_ref()
            .and_then(|hunk_key| {
                self.diff_review_overlays
                    .iter()
                    .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
            })
            .as_ref()
            .and_then(|overlay| overlay.inline_edit_editors.get(&comment_id))
            .map(|editor| editor.read(cx).text(cx).trim().to_string());

        if let Some(new_text) = new_text {
            if !new_text.is_empty() {
                self.update_review_comment(comment_id, new_text, cx);
            }
        }

        // Remove the inline editor and its subscription
        if let Some(hunk_key) = hunk_key {
            if let Some(overlay) = self
                .diff_review_overlays
                .iter_mut()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            {
                overlay.inline_edit_editors.remove(&comment_id);
                overlay.inline_edit_subscriptions.remove(&comment_id);
            }
        }

        // Clear editing state
        self.set_comment_editing(comment_id, false, cx);
    }

    /// Cancels an inline edit of a review comment.
    pub(super) fn cancel_edit_review_comment(
        &mut self,
        comment_id: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Find which hunk this comment belongs to
        let hunk_key = self
            .stored_review_comments
            .iter()
            .find_map(|(key, comments)| {
                if comments.iter().any(|c| c.id == comment_id) {
                    Some(key.clone())
                } else {
                    None
                }
            });

        // Remove the inline editor and its subscription
        if let Some(hunk_key) = hunk_key {
            let snapshot = self.buffer.read(cx).snapshot(cx);
            if let Some(overlay) = self
                .diff_review_overlays
                .iter_mut()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            {
                overlay.inline_edit_editors.remove(&comment_id);
                overlay.inline_edit_subscriptions.remove(&comment_id);
            }
        }

        // Clear editing state
        self.set_comment_editing(comment_id, false, cx);
    }

    /// Action handler for ConfirmEditReviewComment.
    pub(super) fn confirm_edit_review_comment_action(
        &mut self,
        action: &ConfirmEditReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_edit_review_comment(action.id, window, cx);
    }

    /// Action handler for CancelEditReviewComment.
    pub(super) fn cancel_edit_review_comment_action(
        &mut self,
        action: &CancelEditReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_edit_review_comment(action.id, window, cx);
    }

    pub(super) fn reply_to_review_comment_action(
        &mut self,
        action: &ReplyToReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_review_reply(action.id, window, cx);
    }

    pub(super) fn confirm_review_reply_action(
        &mut self,
        action: &ConfirmReviewReply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_review_reply(action.id, window, cx);
    }

    pub(super) fn cancel_review_reply_action(
        &mut self,
        action: &CancelReviewReply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_review_reply(action.id, window, cx);
    }

    pub(super) fn toggle_review_thread_resolved_action(
        &mut self,
        action: &ToggleReviewThreadResolved,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_review_thread_resolve(action.id, cx);
    }

    pub(super) fn draft_review_reply_action(
        &mut self,
        action: &DraftReviewReply,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_review_reply_draft(action.id, cx);
    }

    /// Handles the DeleteReviewComment action - removes a comment.
    pub(super) fn delete_review_comment(
        &mut self,
        action: &DeleteReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Get the hunk key before removing the comment
        // Find the hunk key from the comment itself
        let comment_id = action.id;
        let hunk_key = self
            .stored_review_comments
            .iter()
            .find_map(|(key, comments)| {
                if comments.iter().any(|c| c.id == comment_id) {
                    Some(key.clone())
                } else {
                    None
                }
            });

        // Also get it from the overlay for refresh purposes
        let overlay_hunk_key = self
            .diff_review_overlays
            .first()
            .map(|o| o.hunk_key.clone());

        self.remove_review_comment(action.id, cx);

        // Refresh the overlay height after removing a comment
        if let Some(hunk_key) = hunk_key.or(overlay_hunk_key) {
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        }
    }

    pub(super) fn copy_permalink_to_line(
        &mut self,
        _: &CopyPermalinkToLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let permalink_task = self.get_permalink_to_line(cx);
        let workspace = self.workspace();

        cx.spawn_in(window, async move |_, cx| match permalink_task.await {
            Ok(permalink) => {
                cx.update(|_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(permalink.to_string()));
                })
                .ok();
            }
            Err(err) => {
                let message = format!("Failed to copy permalink to line: {err}");

                anyhow::Result::<()>::Err(err).log_err();

                if let Some(workspace) = workspace {
                    workspace
                        .update_in(cx, |workspace, _, cx| {
                            struct CopyPermalinkToLine;

                            workspace.show_toast(
                                Toast::new(
                                    NotificationId::unique::<CopyPermalinkToLine>(),
                                    message,
                                ),
                                cx,
                            )
                        })
                        .ok();
                }
            }
        })
        .detach();
    }

    pub(super) fn open_permalink_to_line(
        &mut self,
        _: &OpenPermalinkToLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let permalink_task = self.get_permalink_to_line(cx);
        let workspace = self.workspace();

        cx.spawn_in(window, async move |_, cx| match permalink_task.await {
            Ok(permalink) => {
                cx.update(|_, cx| {
                    cx.open_url(permalink.as_ref());
                })
                .ok();
            }
            Err(err) => {
                let message = format!("Failed to open permalink to line: {err}");

                anyhow::Result::<()>::Err(err).log_err();

                if let Some(workspace) = workspace {
                    workspace.update(cx, |workspace, cx| {
                        struct OpenPermalinkToLine;

                        workspace.show_toast(
                            Toast::new(NotificationId::unique::<OpenPermalinkToLine>(), message),
                            cx,
                        )
                    });
                }
            }
        })
        .detach();
    }

    pub(super) fn toggle_staged_selected_diff_hunks(
        &mut self,
        _: &::git::ToggleStaged,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ranges: Vec<_> = self
            .selections
            .disjoint_anchors()
            .iter()
            .map(|s| s.range())
            .collect();
        let task = self.save_buffers_for_ranges_if_needed(&ranges, cx);
        cx.spawn_in(window, async move |this, cx| {
            task.await?;
            this.update_in(cx, |this, window, cx| {
                let snapshot = this.buffer.read(cx).snapshot(cx);
                let hunks = this.diff_hunks_in_ranges(&ranges, &snapshot).collect();
                this.apply_toggle(hunks, window, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    pub(super) fn stage_and_next(
        &mut self,
        _: &::git::StageAndNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.do_stage_or_unstage_and_next(true, window, cx);
    }

    pub(super) fn unstage_and_next(
        &mut self,
        _: &::git::UnstageAndNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.do_stage_or_unstage_and_next(false, window, cx);
    }

    pub fn apply_toggle(
        &mut self,
        hunks: Vec<MultiBufferDiffHunk>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hunks = self.resolve_diff_hunks(hunks, cx);
        if hunks.is_empty() {
            return;
        }
        if let Some(target) = self.diff_hunk_action_target.clone() {
            target
                .update(cx, |editor, cx| {
                    editor.toggle_resolved_diff_hunks(hunks, cx);
                })
                .log_err();
            return;
        }
        self.toggle_resolved_diff_hunks(hunks, cx);
    }

    fn toggle_resolved_diff_hunks(
        &mut self,
        hunks: Vec<ResolvedDiffHunks>,
        cx: &mut Context<Self>,
    ) {
        for hunks in hunks {
            let Some(operations) = hunks.diff.read(cx).operations() else {
                continue;
            };
            let stage = if !operations.supports_unstaging() {
                true
            } else if !operations.supports_staging() {
                false
            } else {
                hunks
                    .hunks
                    .iter()
                    .any(|hunk| hunk.status.has_secondary_hunk())
            };
            let ranges = hunks
                .hunks
                .into_iter()
                .map(|hunk| hunk.buffer_range)
                .collect();
            if stage {
                operations.stage(hunks.diff, hunks.buffer, ranges, cx);
            } else {
                operations.unstage(hunks.diff, hunks.buffer, ranges, cx);
            }
        }
    }

    pub fn apply_stage_or_unstage(
        &mut self,
        stage: bool,
        hunks: Vec<MultiBufferDiffHunk>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hunks = self.resolve_diff_hunks(hunks, cx);
        if hunks.is_empty() {
            return;
        }
        if let Some(target) = self.diff_hunk_action_target.clone() {
            target
                .update(cx, |editor, cx| {
                    editor.stage_or_unstage_resolved_diff_hunks(stage, hunks, cx);
                })
                .log_err();
            return;
        }
        self.stage_or_unstage_resolved_diff_hunks(stage, hunks, cx);
    }

    fn stage_or_unstage_resolved_diff_hunks(
        &mut self,
        stage: bool,
        hunks: Vec<ResolvedDiffHunks>,
        cx: &mut Context<Self>,
    ) {
        for hunks in hunks {
            let Some(operations) = hunks.diff.read(cx).operations() else {
                continue;
            };
            if (stage && !operations.supports_staging())
                || (!stage && !operations.supports_unstaging())
            {
                continue;
            }
            let ranges = hunks
                .hunks
                .into_iter()
                .map(|hunk| hunk.buffer_range)
                .collect();
            if stage {
                operations.stage(hunks.diff, hunks.buffer, ranges, cx);
            } else {
                operations.unstage(hunks.diff, hunks.buffer, ranges, cx);
            }
        }
    }

    pub fn apply_restore(
        &mut self,
        hunks: Vec<MultiBufferDiffHunk>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hunks = self.resolve_diff_hunks(hunks, cx);
        if hunks.is_empty() {
            return;
        }
        if let Some(target) = self.diff_hunk_action_target.clone() {
            target
                .update(cx, |editor, cx| {
                    editor.restore_resolved_diff_hunks(hunks, window, cx);
                })
                .log_err();
            return;
        }
        self.restore_resolved_diff_hunks(hunks, window, cx);
    }

    fn restore_resolved_diff_hunks(
        &mut self,
        hunks: Vec<ResolvedDiffHunks>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        let mut restorable_hunks = Vec::new();
        for hunks in hunks {
            let Some(operations) = hunks.diff.read(cx).operations() else {
                continue;
            };
            if !operations.supports_restore() {
                continue;
            }
            if operations.supports_unstaging() {
                let ranges = hunks
                    .hunks
                    .iter()
                    .map(|hunk| hunk.buffer_range.clone())
                    .collect();
                operations.unstage(hunks.diff.clone(), hunks.buffer.clone(), ranges, cx);
            }
            restorable_hunks.push(hunks);
        }
        if restorable_hunks.is_empty() {
            return;
        }
        self.transact(window, cx, |editor, window, cx| {
            editor.restore_diff_hunks(restorable_hunks, cx);
            let selections = editor
                .selections
                .all::<MultiBufferOffset>(&editor.display_snapshot(cx));
            editor.change_selections(
                SelectionEffects::no_scroll(),
                window,
                cx,
                |selections_state| {
                    selections_state.select(selections);
                },
            );
        });
    }

    pub(super) fn clear_expanded_diff_hunks(&mut self, cx: &mut Context<Self>) -> bool {
        self.buffer.update(cx, |buffer, cx| {
            let ranges = vec![Anchor::Min..Anchor::Max];
            if !buffer.all_diff_hunks_expanded()
                && buffer.has_expanded_diff_hunks_in_ranges(&ranges, cx)
            {
                buffer.collapse_diff_hunks(ranges, cx);
                true
            } else {
                false
            }
        })
    }

    pub(super) fn has_any_expanded_diff_hunks(&self, cx: &App) -> bool {
        if self.buffer.read(cx).all_diff_hunks_expanded() {
            return true;
        }
        let ranges = vec![Anchor::Min..Anchor::Max];
        self.buffer
            .read(cx)
            .has_expanded_diff_hunks_in_ranges(&ranges, cx)
    }

    pub(super) fn toggle_single_diff_hunk(&mut self, range: Range<Anchor>, cx: &mut Context<Self>) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.toggle_single_diff_hunk(range, cx);
        })
    }

    pub(super) fn apply_all_diff_hunks(
        &mut self,
        _: &ApplyAllDiffHunks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }

        let buffers = self.buffer.read(cx).all_buffers();
        for branch_buffer in buffers {
            branch_buffer.update(cx, |branch_buffer, cx| {
                branch_buffer.merge_into_base(Vec::new(), cx);
            });
        }

        if let Some(project) = self.project.clone() {
            self.save(
                SaveOptions {
                    format: true,
                    force_format: false,
                    autosave: false,
                },
                project,
                window,
                cx,
            )
            .detach_and_log_err(cx);
        }
    }

    pub(super) fn apply_selected_diff_hunks(
        &mut self,
        _: &ApplyDiffHunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        let snapshot = self.snapshot(window, cx);
        let hunks = snapshot.hunks_for_ranges(
            self.selections
                .all(&snapshot.display_snapshot)
                .into_iter()
                .map(|selection| selection.range()),
        );
        let mut ranges_by_buffer = HashMap::default();
        self.transact(window, cx, |editor, _window, cx| {
            for hunk in hunks {
                if let Some(buffer) = editor.buffer.read(cx).buffer(hunk.buffer_id) {
                    ranges_by_buffer
                        .entry(buffer.clone())
                        .or_insert_with(Vec::new)
                        .push(hunk.buffer_range.to_offset(buffer.read(cx)));
                }
            }

            for (buffer, ranges) in ranges_by_buffer {
                buffer.update(cx, |buffer, cx| {
                    buffer.merge_into_base(ranges, cx);
                });
            }
        });

        if let Some(project) = self.project.clone() {
            self.save(
                SaveOptions {
                    format: true,
                    force_format: false,
                    autosave: false,
                },
                project,
                window,
                cx,
            )
            .detach_and_log_err(cx);
        }
    }

    pub(super) fn open_git_blame_commit(
        &mut self,
        _: &OpenGitBlameCommit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_git_blame_commit_internal(window, cx);
    }

    pub(super) fn toggle_git_blame_inline_internal(
        &mut self,
        user_triggered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.git_blame_inline_enabled {
            self.git_blame_inline_enabled = false;
            self.show_git_blame_inline = false;
            self.show_git_blame_inline_delay_task.take();
        } else {
            self.git_blame_inline_enabled = true;
            self.start_git_blame_inline(user_triggered, window, cx);
        }

        cx.notify();
    }

    pub(super) fn start_git_blame_inline(
        &mut self,
        user_triggered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_git_blame(user_triggered, window, cx);

        if ProjectSettings::get_global(cx)
            .git
            .inline_blame_delay()
            .is_some()
        {
            self.start_inline_blame_timer(window, cx);
        } else {
            self.show_git_blame_inline = true
        }
    }

    pub(super) fn render_git_blame_gutter(&self, cx: &App) -> bool {
        !self.mode().is_minimap() && self.show_git_blame_gutter && self.has_blame_entries(cx)
    }

    pub(super) fn render_git_blame_inline(&self, window: &Window, cx: &App) -> bool {
        ProjectSettings::get_global(cx).git.inline_blame.location
            == project::project_settings::InlineBlameLocation::Inline
            && self.show_git_blame_inline
            && (self.focus_handle.is_focused(window) || self.inline_blame_popover.is_some())
            && !self.newest_selection_head_on_empty_line(cx)
            && self.has_blame_entries(cx)
    }

    pub(super) fn start_inline_blame_timer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(delay) = ProjectSettings::get_global(cx).git.inline_blame_delay() {
            self.show_git_blame_inline = false;

            self.show_git_blame_inline_delay_task =
                Some(cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(delay).await;

                    this.update(cx, |this, cx| {
                        this.show_git_blame_inline = true;
                        cx.notify();
                    })
                    .log_err();
                }));
        }
    }

    pub(super) fn show_blame_popover(
        &mut self,
        buffer: BufferId,
        blame_entry: &BlameEntry,
        position: gpui::Point<Pixels>,
        ignore_timeout: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = &mut self.inline_blame_popover {
            state.hide_task.take();
        } else {
            let blame_popover_delay = EditorSettings::get_global(cx).hover_popover_delay.0;
            let blame_entry = blame_entry.clone();
            let show_task = cx.spawn(async move |editor, cx| {
                if !ignore_timeout {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(blame_popover_delay))
                        .await;
                }
                editor
                    .update(cx, |editor, cx| {
                        editor.inline_blame_popover_show_task.take();
                        let Some(blame) = editor.blame.as_ref() else {
                            return;
                        };
                        let blame = blame.read(cx);
                        let details = blame.details_for_entry(buffer, &blame_entry);
                        let markdown = cx.new(|cx| {
                            Markdown::new(
                                details
                                    .as_ref()
                                    .map(|message| message.message.clone())
                                    .unwrap_or_default(),
                                None,
                                None,
                                cx,
                            )
                        });
                        editor.inline_blame_popover = Some(InlineBlamePopover {
                            position,
                            hide_task: None,
                            popover_bounds: None,
                            popover_state: InlineBlamePopoverState {
                                scroll_handle: ScrollHandle::new(),
                                commit_message: details,
                                markdown,
                            },
                            keyboard_grace: ignore_timeout,
                        });
                        cx.notify();
                    })
                    .ok();
            });
            self.inline_blame_popover_show_task = Some(show_task);
        }
    }

    pub(super) fn go_to_prev_hunk(
        &mut self,
        _: &GoToPreviousHunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.snapshot(window, cx);
        let selection = self.selections.newest::<Point>(&snapshot.display_snapshot);
        self.go_to_hunk_before_or_after_position(
            &snapshot,
            selection.head(),
            Direction::Prev,
            true,
            window,
            cx,
        );
    }

    /// Calculates the appropriate block height for the diff review overlay.
    /// Height is in lines: 2 for input row, 1 for header when comments exist,
    /// and 2 lines per comment when expanded.
    pub(super) fn calculate_overlay_height(
        &self,
        hunk_key: &DiffHunkKey,
        comments_expanded: bool,
        snapshot: &MultiBufferSnapshot,
    ) -> u32 {
        let comments = self.comments_for_hunk(hunk_key, snapshot);
        let comment_count = comments.len();
        let base_height: u32 = 2; // Input row with avatar and buttons
        // An open reply box takes an input row of its own.
        let reply_boxes = self
            .diff_review_overlays
            .iter()
            .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, snapshot))
            .map_or(0, |overlay| overlay.reply_editors.len() as u32);

        if comment_count == 0 {
            base_height
        } else if comments_expanded {
            // Header (1 line) + each comment with its replies
            base_height
                + 1
                + comments
                    .iter()
                    .map(StoredReviewComment::estimated_lines)
                    .sum::<u32>()
                + reply_boxes * 2
        } else if Self::collapses_to_chip(comments) {
            1
        } else {
            // Just header when collapsed
            base_height + 1
        }
    }

    /// A thread that is already on the remote folds down to a one-line chip
    /// under its line, like a collapsed thread on GitLab, so it does not
    /// hide the diff. Unposted comments keep the full overlay, since the
    /// input box is where more of them are written.
    fn collapses_to_chip(comments: &[StoredReviewComment]) -> bool {
        !comments.is_empty() && comments.iter().all(|comment| comment.thread.is_some())
    }

    pub fn stage_or_unstage_diff_hunks(
        &mut self,
        stage: bool,
        ranges: Vec<Range<Anchor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = self.save_buffers_for_ranges_if_needed(&ranges, cx);
        cx.spawn_in(window, async move |this, cx| {
            task.await?;
            this.update_in(cx, |this, window, cx| {
                let snapshot = this.buffer.read(cx).snapshot(cx);
                let hunks = this.diff_hunks_in_ranges(&ranges, &snapshot).collect();
                this.apply_stage_or_unstage(stage, hunks, window, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    pub fn restore_diff_hunks_in_ranges(
        &mut self,
        ranges: Vec<Range<Anchor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let hunks = self.diff_hunks_in_ranges(&ranges, &snapshot).collect();
        self.apply_restore(hunks, window, cx);
    }

    fn toggle_diff_hunks_in_ranges(
        &mut self,
        ranges: Vec<Range<Anchor>>,
        cx: &mut Context<Editor>,
    ) {
        self.buffer.update(cx, |buffer, cx| {
            let expand = !buffer.has_expanded_diff_hunks_in_ranges(&ranges, cx);
            buffer.expand_or_collapse_diff_hunks(ranges, expand, cx);
        })
    }

    pub fn set_blame(
        &mut self,
        blame: Entity<GitBlame>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.blame_subscription = Some(cx.observe_in(&blame, window, |_, _, _, cx| cx.notify()));
        self.blame = Some(blame);
        self.show_git_blame_gutter = true;
        cx.notify();
    }

    fn start_git_blame(
        &mut self,
        user_triggered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .blame
            .as_ref()
            .is_some_and(|blame| blame.read(cx).is_static())
        {
            return;
        }
        if let Some(project) = self.project() {
            if let Some(buffer) = self.buffer().read(cx).as_singleton()
                && buffer.read(cx).file().is_none_or(|file| {
                    matches!(file.disk_state(), language::DiskState::Historic { .. })
                })
            {
                return;
            }

            let focused = self.focus_handle(cx).contains_focused(window, cx);

            let project = project.clone();
            let blame = cx
                .new(|cx| GitBlame::new(self.buffer.clone(), project, user_triggered, focused, cx));
            self.blame_subscription =
                Some(cx.observe_in(&blame, window, |_, _, _, cx| cx.notify()));
            self.blame = Some(blame);
        }
    }

    fn restore_hunks_in_ranges(
        &mut self,
        ranges: Vec<Range<Point>>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let hunks = self.snapshot(window, cx).hunks_for_ranges(ranges);
        self.apply_restore(hunks, window, cx);
    }

    fn save_buffers_for_ranges_if_needed(
        &mut self,
        ranges: &[Range<Anchor>],
        cx: &mut Context<Editor>,
    ) -> Task<Result<()>> {
        let multibuffer = self.buffer.read(cx);
        let snapshot = multibuffer.read(cx);
        let buffer_ids: HashSet<_> = ranges
            .iter()
            .flat_map(|range| snapshot.buffer_ids_for_range(range.clone()))
            .collect();
        drop(snapshot);

        let mut buffers = HashSet::default();
        for buffer_id in buffer_ids {
            if let Some(buffer_entity) = multibuffer.buffer(buffer_id) {
                let buffer = buffer_entity.read(cx);
                if buffer.file().is_some_and(|file| file.disk_state().exists()) && buffer.is_dirty()
                {
                    buffers.insert(buffer_entity);
                }
            }
        }

        if let Some(project) = &self.project {
            project.update(cx, |project, cx| project.save_buffers(buffers, cx))
        } else {
            Task::ready(Ok(()))
        }
    }

    fn do_stage_or_unstage_and_next(
        &mut self,
        stage: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ranges = self.selections.disjoint_anchor_ranges().collect::<Vec<_>>();

        if ranges.iter().any(|range| range.start != range.end) {
            self.stage_or_unstage_diff_hunks(stage, ranges, window, cx);
            return;
        }

        self.stage_or_unstage_diff_hunks(stage, ranges, window, cx);

        let all_diff_hunks_expanded = self.buffer().read(cx).all_diff_hunks_expanded();
        let wrap_around = !all_diff_hunks_expanded;
        let snapshot = self.snapshot(window, cx);
        let position = self
            .selections
            .newest::<Point>(&snapshot.display_snapshot)
            .head();

        self.go_to_hunk_before_or_after_position(
            &snapshot,
            position,
            Direction::Next,
            wrap_around,
            window,
            cx,
        );
    }

    fn open_git_blame_commit_internal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let (blame_entry, repo) = self.blame_entry_at_cursor(window, cx)?;
        let renderer = cx.global::<GlobalBlameRenderer>().0.clone();
        let workspace = self.workspace()?.downgrade();
        renderer.open_blame_commit(blame_entry, repo, workspace, window, cx);
        None
    }

    fn blame_entry_at_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(BlameEntry, Entity<Repository>)> {
        let blame = self.blame.clone()?;
        let snapshot = self.snapshot(window, cx);
        let cursor = self
            .selections
            .newest::<Point>(&snapshot.display_snapshot)
            .head();
        let (buffer, point) = snapshot.buffer_snapshot().point_to_buffer_point(cursor)?;
        let (_, blame_entry) = blame
            .update(cx, |blame, cx| {
                blame
                    .blame_for_rows(
                        &[RowInfo {
                            buffer_id: Some(buffer.remote_id()),
                            buffer_row: Some(point.row),
                            ..Default::default()
                        }],
                        cx,
                    )
                    .next()
            })
            .flatten()?;
        let repository = blame.read(cx).repository(cx, buffer.remote_id())?;
        Some((blame_entry, repository))
    }

    pub(crate) fn blame_revision_target(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(RepoPath, Oid, Entity<Repository>)> {
        let (blame_entry, repository) = self.blame_entry_at_cursor(window, cx)?;
        let highlighted_sha = self
            .blame
            .as_ref()
            .and_then(|blame| blame.read(cx).highlighted_sha());
        let (revision, path) = blame_entry.revision_target(highlighted_sha)?;
        Some((path, revision, repository))
    }

    pub(crate) fn blame_previous_revision_target(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(RepoPath, Oid, Entity<Repository>)> {
        let (blame_entry, repository) = self.blame_entry_at_cursor(window, cx)?;
        let (revision, path) = blame_entry.previous_revision_target()?;
        Some((path, revision, repository))
    }

    pub(super) fn blame_revision(
        &mut self,
        _: &BlameRevision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, revision, repository)) = self.blame_revision_target(window, cx) else {
            return;
        };
        self.open_blame_revision(path, revision, repository, window, cx);
    }

    pub(super) fn blame_previous_revision(
        &mut self,
        _: &BlamePreviousRevision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, revision, repository)) = self.blame_previous_revision_target(window, cx)
        else {
            return;
        };
        self.open_blame_revision(path, revision, repository, window, cx);
    }

    fn open_blame_revision(
        &mut self,
        path: RepoPath,
        revision: Oid,
        repository: Entity<Repository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace() else {
            return;
        };
        let renderer = cx.global::<GlobalBlameRenderer>().0.clone();
        renderer.open_blame_revision(
            path,
            revision,
            repository,
            workspace.downgrade(),
            window,
            cx,
        );
    }

    fn has_blame_entries(&self, cx: &App) -> bool {
        self.blame()
            .is_some_and(|blame| blame.read(cx).has_generated_entries())
    }

    fn newest_selection_head_on_empty_line(&self, cx: &App) -> bool {
        let cursor_anchor = self.selections.newest_anchor().head();

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let buffer_row = MultiBufferRow(cursor_anchor.to_point(&snapshot).row);

        snapshot.line_len(buffer_row) == 0
    }
    fn hunk_after_position(
        &mut self,
        snapshot: &EditorSnapshot,
        position: Point,
        wrap_around: bool,
    ) -> Option<MultiBufferDiffHunk> {
        let result = snapshot
            .buffer_snapshot()
            .diff_hunks_in_range(position..snapshot.buffer_snapshot().max_point())
            .find(|hunk| hunk.row_range.start.0 > position.row);

        if wrap_around {
            result.or_else(|| {
                snapshot
                    .buffer_snapshot()
                    .diff_hunks_in_range(Point::zero()..position)
                    .find(|hunk| hunk.row_range.end.0 < position.row)
            })
        } else {
            result
        }
    }

    fn hunk_before_position(
        &mut self,
        snapshot: &EditorSnapshot,
        position: Point,
        wrap_around: bool,
    ) -> Option<MultiBufferRow> {
        let result = snapshot.buffer_snapshot().diff_hunk_before(position);

        if wrap_around {
            result.or_else(|| snapshot.buffer_snapshot().diff_hunk_before(Point::MAX))
        } else {
            result
        }
    }

    /// Dismisses overlays that have no comments stored for their hunks.
    /// Keeps overlays that have at least one comment.
    fn dismiss_overlays_without_comments(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot(cx);

        // First, compute which overlays have comments (to avoid borrow issues with retain)
        let overlays_with_comments: Vec<bool> = self
            .diff_review_overlays
            .iter()
            .map(|overlay| self.hunk_comment_count(&overlay.hunk_key, &snapshot) > 0)
            .collect();

        // Now collect block IDs to remove and retain overlays
        let mut block_ids_to_remove = HashSet::default();
        let mut index = 0;
        self.diff_review_overlays.retain(|overlay| {
            let has_comments = overlays_with_comments[index];
            index += 1;
            if !has_comments {
                block_ids_to_remove.insert(overlay.block_id);
            }
            has_comments
        });

        if !block_ids_to_remove.is_empty() {
            self.remove_blocks(block_ids_to_remove, None, cx);
            cx.notify();
        }
    }

    /// Refreshes the diff review overlay block to update its height and render function.
    /// Uses resize_blocks and replace_blocks to avoid visual flicker from remove+insert.
    fn refresh_diff_review_overlay_height(
        &mut self,
        hunk_key: &DiffHunkKey,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Extract all needed data from overlay first to avoid borrow conflicts
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let (comments_expanded, block_id, prompt_editor) = {
            let Some(overlay) = self
                .diff_review_overlays
                .iter()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
            else {
                return;
            };

            (
                overlay.comments_expanded,
                overlay.block_id,
                overlay.prompt_editor.clone(),
            )
        };

        // Calculate new height
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let new_height = self.calculate_overlay_height(hunk_key, comments_expanded, &snapshot);

        // Update the block height using resize_blocks (avoids flicker)
        let mut heights = HashMap::default();
        heights.insert(block_id, new_height);
        self.resize_blocks(heights, None, cx);

        // Update the render function using replace_blocks (avoids flicker)
        let hunk_key_for_render = hunk_key.clone();
        let editor_handle = cx.entity().downgrade();
        let render: Arc<dyn Fn(&mut BlockContext) -> AnyElement + Send + Sync> =
            Arc::new(move |cx| {
                Self::render_diff_review_overlay(
                    &prompt_editor,
                    &hunk_key_for_render,
                    &editor_handle,
                    cx,
                )
            });

        let mut renderers = HashMap::default();
        renderers.insert(block_id, render);
        self.replace_blocks(renderers, None, cx);
    }

    /// Compares two DiffHunkKeys for equality by resolving their anchors.
    fn hunk_keys_match(a: &DiffHunkKey, b: &DiffHunkKey, snapshot: &MultiBufferSnapshot) -> bool {
        a.file_path == b.file_path
            && a.hunk_start_anchor.to_point(snapshot) == b.hunk_start_anchor.to_point(snapshot)
    }

    fn render_diff_review_overlay(
        prompt_editor: &Entity<Editor>,
        hunk_key: &DiffHunkKey,
        editor_handle: &WeakEntity<Editor>,
        cx: &mut BlockContext,
    ) -> AnyElement {
        fn format_line_ranges(ranges: &[(u32, u32)]) -> Option<String> {
            if ranges.is_empty() {
                return None;
            }
            let formatted: Vec<String> = ranges
                .iter()
                .map(|(start, end)| {
                    let start_line = start + 1;
                    let end_line = end + 1;
                    if start_line == end_line {
                        format!("Line {start_line}")
                    } else {
                        format!("Lines {start_line}-{end_line}")
                    }
                })
                .collect();
            // Don't show label for single line in single excerpt
            if ranges.len() == 1 && ranges[0].0 == ranges[0].1 {
                return None;
            }
            Some(formatted.join(" ⋯ "))
        }

        let theme = cx.theme();
        let colors = theme.colors();

        let (
            comments,
            comments_expanded,
            inline_editors,
            reply_editors,
            user_avatar_uri,
            line_ranges,
        ) = editor_handle
            .upgrade()
            .map(|editor| {
                let editor = editor.read(cx);
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let comments = editor.comments_for_hunk(hunk_key, &snapshot).to_vec();
                let (expanded, editors, reply_editors, avatar_uri, line_ranges) = editor
                    .diff_review_overlays
                    .iter()
                    .find(|overlay| Editor::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
                    .map(|o| {
                        let start_point = o.anchor_range.start.to_point(&snapshot);
                        let end_point = o.anchor_range.end.to_point(&snapshot);
                        // Get line ranges per excerpt to detect discontinuities
                        let buffer_ranges = snapshot.range_to_buffer_ranges(start_point..end_point);
                        let ranges: Vec<(u32, u32)> = buffer_ranges
                            .iter()
                            .map(|(buffer_snapshot, range, _)| {
                                let start = buffer_snapshot.offset_to_point(range.start.0).row;
                                let end = buffer_snapshot.offset_to_point(range.end.0).row;
                                (start, end)
                            })
                            .collect();
                        (
                            o.comments_expanded,
                            o.inline_edit_editors.clone(),
                            o.reply_editors.clone(),
                            o.user_avatar_uri.clone(),
                            if ranges.is_empty() {
                                None
                            } else {
                                Some(ranges)
                            },
                        )
                    })
                    .unwrap_or((true, HashMap::default(), HashMap::default(), None, None));
                (
                    comments,
                    expanded,
                    editors,
                    reply_editors,
                    avatar_uri,
                    line_ranges,
                )
            })
            .unwrap_or((
                Vec::new(),
                true,
                HashMap::default(),
                HashMap::default(),
                None,
                None,
            ));

        let comment_count = comments.len();
        let avatar_size = px(20.);
        let action_icon_size = IconSize::XSmall;
        let line_height = cx.line_height;
        let markdown_style = hover_markdown_style(cx.window, cx.app);

        if !comments_expanded && Self::collapses_to_chip(&comments) {
            return Self::render_collapsed_thread_chip(
                &comments,
                hunk_key.clone(),
                editor_handle.clone(),
                cx,
            );
        }

        v_flex()
            .w_full()
            .bg(colors.editor_background)
            .border_b_1()
            .border_color(colors.border)
            .px_2()
            .pb_2()
            .gap_2()
            // Line range indicator (only shown for multi-line selections or multiple excerpts)
            .when_some(line_ranges, |el, ranges| {
                let label = format_line_ranges(&ranges);
                if let Some(label) = label {
                    el.child(
                        h_flex()
                            .w_full()
                            .px_2()
                            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
                    )
                } else {
                    el
                }
            })
            // Top row: editable input with user's avatar
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .bg(colors.surface_background)
                    .child(
                        div()
                            .size(avatar_size)
                            .flex_shrink_0()
                            .rounded_full()
                            .overflow_hidden()
                            .child(if let Some(ref avatar_uri) = user_avatar_uri {
                                Avatar::new(avatar_uri.clone())
                                    .size(avatar_size)
                                    .into_any_element()
                            } else {
                                Icon::new(IconName::Person)
                                    .size(IconSize::Small)
                                    .color(ui::Color::Muted)
                                    .into_any_element()
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .border_1()
                            .border_color(colors.border)
                            .rounded_md()
                            .bg(colors.editor_background)
                            .px_2()
                            .py_1()
                            .child(prompt_editor.clone()),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_1()
                            .child(
                                IconButton::new("diff-review-close", IconName::Close)
                                    .icon_color(ui::Color::Muted)
                                    .icon_size(action_icon_size)
                                    .tooltip(Tooltip::text("Close"))
                                    .on_click(|_, window, cx| {
                                        window
                                            .dispatch_action(Box::new(crate::actions::Cancel), cx);
                                    }),
                            )
                            .child(
                                IconButton::new("diff-review-add", IconName::Return)
                                    .icon_color(ui::Color::Muted)
                                    .icon_size(action_icon_size)
                                    .tooltip(Tooltip::text("Add comment"))
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(
                                            Box::new(crate::actions::SubmitDiffReviewComment),
                                            cx,
                                        );
                                    }),
                            ),
                    ),
            )
            // Expandable comments section (only shown when there are comments)
            .when(comment_count > 0, |el| {
                el.child(Self::render_comments_section(
                    comments,
                    comments_expanded,
                    inline_editors,
                    reply_editors,
                    user_avatar_uri,
                    hunk_key.clone(),
                    editor_handle.clone(),
                    avatar_size,
                    action_icon_size,
                    line_height,
                    markdown_style,
                    colors,
                ))
            })
            .into_any_element()
    }

    /// A posted thread folded to one line: who opened it, whether it is
    /// resolved, how many replies, and its first words. Clicking opens it.
    fn render_collapsed_thread_chip(
        comments: &[StoredReviewComment],
        hunk_key: DiffHunkKey,
        editor_handle: WeakEntity<Editor>,
        cx: &mut BlockContext,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let Some(first) = comments.first() else {
            return div().into_any_element();
        };
        let author = first.author.clone().unwrap_or_else(|| "You".into());
        let thread = first.thread.as_ref();
        let resolved = thread.is_some_and(|thread| thread.resolved);
        let since_last_review = thread.is_some_and(|thread| thread.since_last_review);
        let replies: usize = comments
            .iter()
            .filter_map(|comment| comment.thread.as_ref())
            .map(|thread| thread.replies.len())
            .sum();
        let more_threads = comments.len().saturating_sub(1);
        let first_line = first.comment.lines().next().unwrap_or_default().trim();
        let preview: String = first_line.chars().take(80).collect();
        let ellipsis = if first_line.chars().count() > 80 {
            "…"
        } else {
            ""
        };
        let mut meta = Vec::new();
        if replies > 0 {
            meta.push(format!(
                "{replies} {}",
                if replies == 1 { "reply" } else { "replies" }
            ));
        }
        if more_threads > 0 {
            meta.push(format!("+{more_threads} more"));
        }
        let meta = meta.join(" · ");

        h_flex()
            .id(cx.block_id)
            .h(cx.line_height)
            .w_full()
            .pl(cx.margins.gutter.width)
            .pr_2()
            .items_center()
            .child(
                h_flex()
                    .id("collapsed-review-thread")
                    .h(cx.line_height - px(2.))
                    .max_w_full()
                    .px_2()
                    .gap_1p5()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(if resolved {
                        colors.border_variant
                    } else {
                        colors.border
                    })
                    .bg(colors.surface_background)
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .overflow_hidden()
                    .on_click(move |_, window: &mut Window, cx| {
                        editor_handle
                            .update(cx, |editor, cx| {
                                editor.toggle_overlay_comments(&hunk_key, window, cx)
                            })
                            .ok();
                    })
                    .child(
                        Icon::new(if resolved {
                            IconName::CheckDouble
                        } else {
                            IconName::Chat
                        })
                        .size(IconSize::XSmall)
                        .color(if resolved {
                            ui::Color::Success
                        } else {
                            ui::Color::Warning
                        }),
                    )
                    .child(
                        Label::new(author)
                            .size(LabelSize::Small)
                            .color(Color::Default),
                    )
                    .child(
                        Label::new(if resolved { "Resolved" } else { "Open" })
                            .size(LabelSize::XSmall)
                            .color(if resolved {
                                Color::Success
                            } else {
                                Color::Warning
                            }),
                    )
                    .when(since_last_review, |el| {
                        el.child(
                            Label::new("New")
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                    })
                    .when(!meta.is_empty(), |el| {
                        el.child(Label::new(meta).size(LabelSize::XSmall).color(Color::Muted))
                    })
                    .child(
                        Label::new(format!("{preview}{ellipsis}"))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .into_any_element()
    }

    fn render_comments_section(
        comments: Vec<StoredReviewComment>,
        expanded: bool,
        inline_editors: HashMap<usize, Entity<Editor>>,
        reply_editors: HashMap<usize, Entity<Editor>>,
        user_avatar_uri: Option<SharedUri>,
        hunk_key: DiffHunkKey,
        editor_handle: WeakEntity<Editor>,
        avatar_size: Pixels,
        action_icon_size: IconSize,
        line_height: Pixels,
        markdown_style: MarkdownStyle,
        colors: &theme::ThemeColors,
    ) -> impl IntoElement {
        let comment_count = comments.len();
        // Collapsed, the header carries who started the thread and how.
        let preview = (!expanded)
            .then(|| comments.first())
            .flatten()
            .map(|comment| {
                let author = comment.author.clone().unwrap_or_else(|| "You".into());
                let first_line = comment.comment.lines().next().unwrap_or_default().trim();
                let body: String = first_line.chars().take(90).collect();
                let ellipsis = if first_line.chars().count() > 90 {
                    "…"
                } else {
                    ""
                };
                format!(" · {author}: {body}{ellipsis}")
            })
            .unwrap_or_default();

        v_flex()
            .w_full()
            .gap_1()
            // Header with expand/collapse toggle
            .child(
                h_flex()
                    .id("review-comments-header")
                    .w_full()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .rounded_md()
                    .overflow_hidden()
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .on_click(move |_, window: &mut Window, cx| {
                        editor_handle
                            .update(cx, |editor, cx| {
                                editor.toggle_overlay_comments(&hunk_key, window, cx)
                            })
                            .ok();
                    })
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::Small)
                        .color(ui::Color::Muted),
                    )
                    .child(
                        Label::new(format!(
                            "{} Comment{}{preview}",
                            comment_count,
                            if comment_count == 1 { "" } else { "s" }
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            // Comments list (when expanded), each followed by its replies
            .when(expanded, |el| {
                el.children(comments.into_iter().flat_map(|comment| {
                    let inline_editor = inline_editors.get(&comment.id).cloned();
                    let reply_editor = reply_editors.get(&comment.id).cloned();
                    let comment_id = comment.id;
                    let replies: Vec<(ReviewReply, Entity<Markdown>)> = comment
                        .thread
                        .as_ref()
                        .map(|thread| thread.replies.clone())
                        .unwrap_or_default()
                        .into_iter()
                        .zip(comment.reply_markdown.clone())
                        .collect();
                    let row = Self::render_comment_row(
                        comment,
                        inline_editor,
                        user_avatar_uri.clone(),
                        avatar_size,
                        action_icon_size,
                        line_height,
                        markdown_style.clone(),
                        colors,
                    )
                    .into_any_element();
                    let markdown_style = markdown_style.clone();
                    let reply_box = reply_editor.map(|editor| {
                        Self::render_reply_box(comment_id, editor, action_icon_size, colors)
                            .into_any_element()
                    });
                    std::iter::once(row)
                        .chain(replies.into_iter().map(move |(reply, markdown)| {
                            Self::render_reply_row(
                                reply,
                                markdown,
                                line_height,
                                markdown_style.clone(),
                                colors,
                            )
                            .into_any_element()
                        }))
                        .chain(reply_box)
                }))
            })
    }

    /// The box a reply to the thread of `comment_id` is written in.
    fn render_reply_box(
        comment_id: usize,
        editor: Entity<Editor>,
        action_icon_size: IconSize,
        colors: &theme::ThemeColors,
    ) -> impl IntoElement {
        h_flex()
            .w_full()
            .pl_8()
            .pr_2()
            .py_1()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .border_1()
                    .border_color(colors.border)
                    .rounded_md()
                    .bg(colors.editor_background)
                    .px_2()
                    .py_1()
                    .child(editor),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_1()
                    .child(
                        IconButton::new(
                            format!("diff-review-reply-cancel-{comment_id}"),
                            IconName::Close,
                        )
                        .icon_color(ui::Color::Muted)
                        .icon_size(action_icon_size)
                        .tooltip(Tooltip::text("Discard reply"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::actions::CancelReviewReply { id: comment_id }),
                                cx,
                            );
                        }),
                    )
                    .child(
                        IconButton::new(
                            format!("diff-review-reply-send-{comment_id}"),
                            IconName::Send,
                        )
                        .icon_color(ui::Color::Muted)
                        .icon_size(action_icon_size)
                        .tooltip(Tooltip::text("Send reply to GitLab"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::actions::ConfirmReviewReply { id: comment_id }),
                                cx,
                            );
                        }),
                    ),
            )
    }

    /// The body of a comment or reply: markdown, wrapped, clipped at the
    /// height the block reserved for it.
    fn render_comment_body(
        markdown: Entity<Markdown>,
        lines: u32,
        line_height: Pixels,
        markdown_style: MarkdownStyle,
    ) -> impl IntoElement {
        div()
            .w_full()
            .max_h(line_height * lines as f32)
            .overflow_hidden()
            .text_sm()
            .child(
                MarkdownElement::new(markdown, markdown_style).code_block_renderer(
                    CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::VisibleOnHover,
                        wrap_button_visibility: markdown::WrapButtonVisibility::Hidden,
                        border: true,
                    },
                ),
            )
    }

    /// A reply from the remote thread, indented under its comment.
    fn render_reply_row(
        reply: ReviewReply,
        markdown: Entity<Markdown>,
        line_height: Pixels,
        markdown_style: MarkdownStyle,
        colors: &theme::ThemeColors,
    ) -> impl IntoElement {
        let lines = estimated_body_lines(&reply.body);
        v_flex()
            .w_full()
            .pl_8()
            .pr_2()
            .py_1()
            .gap_0p5()
            .child(
                h_flex().h(line_height).items_center().gap_2().child(
                    Label::new(reply.author)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                div()
                    .text_color(colors.text)
                    .child(Self::render_comment_body(
                        markdown,
                        lines,
                        line_height,
                        markdown_style,
                    )),
            )
    }

    /// The pencil that opens a comment for editing in place.
    fn render_edit_comment_button(comment_id: usize, action_icon_size: IconSize) -> IconButton {
        IconButton::new(format!("diff-review-edit-{comment_id}"), IconName::Pencil)
            .icon_color(ui::Color::Muted)
            .icon_size(action_icon_size)
            .tooltip(Tooltip::text("Edit comment"))
            .on_click(move |_, window, cx| {
                window.dispatch_action(
                    Box::new(crate::actions::EditReviewComment { id: comment_id }),
                    cx,
                );
            })
    }

    fn render_comment_row(
        comment: StoredReviewComment,
        inline_editor: Option<Entity<Editor>>,
        user_avatar_uri: Option<SharedUri>,
        avatar_size: Pixels,
        action_icon_size: IconSize,
        line_height: Pixels,
        markdown_style: MarkdownStyle,
        colors: &theme::ThemeColors,
    ) -> impl IntoElement {
        let comment_id = comment.id;
        let is_editing = inline_editor.is_some();
        let is_remote = comment.author.is_some();
        let author = comment.author.clone().unwrap_or_else(|| "You".into());
        let posted = comment.thread.is_some();
        let resolved = comment
            .thread
            .as_ref()
            .is_some_and(|thread| thread.resolved);
        let original_line = comment
            .thread
            .as_ref()
            .and_then(|thread| thread.original_line);
        let old_line = comment.thread.as_ref().and_then(|thread| thread.old_line);
        let since_last_review = comment
            .thread
            .as_ref()
            .is_some_and(|thread| thread.since_last_review);
        let lines = estimated_body_lines(&comment.comment);

        let header = h_flex()
            .w_full()
            .h(line_height)
            .items_center()
            .gap_2()
            .child(
                div()
                    .size(avatar_size)
                    .flex_shrink_0()
                    .rounded_full()
                    .overflow_hidden()
                    .child(match user_avatar_uri {
                        Some(ref avatar_uri) if !is_remote => Avatar::new(avatar_uri.clone())
                            .size(avatar_size)
                            .into_any_element(),
                        _ => Icon::new(IconName::Person)
                            .size(IconSize::Small)
                            .color(ui::Color::Muted)
                            .into_any_element(),
                    }),
            )
            .child(
                Label::new(author)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(if is_editing {
                // Editing mode: show close and confirm buttons
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new(
                            format!("diff-review-cancel-edit-{comment_id}"),
                            IconName::Close,
                        )
                        .icon_color(ui::Color::Muted)
                        .icon_size(action_icon_size)
                        .tooltip(Tooltip::text("Cancel"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::actions::CancelEditReviewComment {
                                    id: comment_id,
                                }),
                                cx,
                            );
                        }),
                    )
                    .child(
                        IconButton::new(
                            format!("diff-review-confirm-edit-{comment_id}"),
                            IconName::Return,
                        )
                        .icon_color(ui::Color::Muted)
                        .icon_size(action_icon_size)
                        .tooltip(Tooltip::text("Confirm"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::actions::ConfirmEditReviewComment {
                                    id: comment_id,
                                }),
                                cx,
                            );
                        }),
                    )
                    .into_any_element()
            } else if posted {
                h_flex()
                    .gap_1()
                    .items_center()
                    .when(!is_remote, |el| {
                        el.child(Self::render_edit_comment_button(
                            comment_id,
                            action_icon_size,
                        ))
                    })
                    .when(since_last_review, |el| {
                        el.child(
                            Label::new("New since your review")
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                    })
                    .when_some(old_line, |el, line| {
                        el.child(
                            Label::new(format!("on removed line {line}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .when_some(original_line, |el, line| {
                        el.child(
                            Label::new(format!("was line {line}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .when(resolved, |el| {
                        el.child(
                            Label::new("Resolved")
                                .size(LabelSize::XSmall)
                                .color(Color::Success),
                        )
                    })
                    .child(
                        IconButton::new(
                            format!("diff-review-draft-reply-{comment_id}"),
                            IconName::Sparkle,
                        )
                        .icon_color(ui::Color::Muted)
                        .icon_size(action_icon_size)
                        .tooltip(Tooltip::text("Draft a reply with the model"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::actions::DraftReviewReply { id: comment_id }),
                                cx,
                            );
                        }),
                    )
                    .child(
                        IconButton::new(
                            format!("diff-review-reply-{comment_id}"),
                            IconName::ReplyArrowRight,
                        )
                        .icon_color(ui::Color::Muted)
                        .icon_size(action_icon_size)
                        .tooltip(Tooltip::text("Reply"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::actions::ReplyToReviewComment { id: comment_id }),
                                cx,
                            );
                        }),
                    )
                    .child(
                        IconButton::new(
                            format!("diff-review-resolve-{comment_id}"),
                            if resolved {
                                IconName::CheckDouble
                            } else {
                                IconName::Check
                            },
                        )
                        .icon_color(ui::Color::Success)
                        .icon_size(action_icon_size)
                        .tooltip(Tooltip::text(if resolved {
                            "Reopen thread"
                        } else {
                            "Resolve thread"
                        }))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::actions::ToggleReviewThreadResolved {
                                    id: comment_id,
                                }),
                                cx,
                            );
                        }),
                    )
                    .into_any_element()
            } else {
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Label::new("Not posted")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(Self::render_edit_comment_button(
                        comment_id,
                        action_icon_size,
                    ))
                    .child(
                        IconButton::new(
                            format!("diff-review-delete-{comment_id}"),
                            IconName::Trash,
                        )
                        .icon_color(ui::Color::Muted)
                        .icon_size(action_icon_size)
                        .tooltip(Tooltip::text("Delete comment"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::actions::DeleteReviewComment { id: comment_id }),
                                cx,
                            );
                        }),
                    )
                    .into_any_element()
            });

        v_flex()
            .w_full()
            .gap_0p5()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(colors.surface_background)
            .child(header)
            .child(if let Some(editor) = inline_editor {
                // Inline edit mode: show an editable text field
                div()
                    .w_full()
                    .border_1()
                    .border_color(colors.border)
                    .rounded_md()
                    .bg(colors.editor_background)
                    .px_2()
                    .py_1()
                    .child(editor)
                    .into_any_element()
            } else {
                div()
                    .w_full()
                    .text_color(colors.text)
                    .child(Self::render_comment_body(
                        comment.markdown,
                        lines,
                        line_height,
                        markdown_style,
                    ))
                    .into_any_element()
            })
    }

    fn get_permalink_to_line(&self, cx: &mut Context<Self>) -> Task<Result<url::Url>> {
        let buffer_and_selection = maybe!({
            let selection = self.selections.newest::<Point>(&self.display_snapshot(cx));
            let selection_range = selection.range();

            let multi_buffer = self.buffer().read(cx);
            let multi_buffer_snapshot = multi_buffer.snapshot(cx);
            let buffer_ranges = multi_buffer_snapshot
                .range_to_buffer_ranges(selection_range.start..selection_range.end);

            let (buffer_snapshot, range, _) = if selection.reversed {
                buffer_ranges.first()
            } else {
                buffer_ranges.last()
            }?;

            let buffer_range = range.to_point(buffer_snapshot);
            let buffer = multi_buffer.buffer(buffer_snapshot.remote_id())?;

            let Some(buffer_diff) = multi_buffer.diff_for(buffer_snapshot.remote_id()) else {
                return Some((buffer, buffer_range.start.row..buffer_range.end.row));
            };

            let buffer_diff_snapshot = buffer_diff.read(cx).snapshot(cx);
            let start = buffer_diff_snapshot
                .buffer_point_to_base_text_point(buffer_range.start, &buffer_snapshot);
            let end = buffer_diff_snapshot
                .buffer_point_to_base_text_point(buffer_range.end, &buffer_snapshot);

            Some((buffer, start.row..end.row))
        });

        let Some((buffer, selection)) = buffer_and_selection else {
            return Task::ready(Err(anyhow!("failed to determine buffer and selection")));
        };

        let Some(project) = self.project() else {
            return Task::ready(Err(anyhow!("editor does not have project")));
        };

        project.update(cx, |project, cx| {
            project.get_permalink_to_line(&buffer, selection, cx)
        })
    }
}

#[cfg(test)]
impl Editor {
    /// Returns the line range for the first diff review overlay, if one is active.
    /// Returns (start_row, end_row) as physical line numbers in the underlying file.
    pub(super) fn diff_review_line_range(&self, cx: &App) -> Option<(u32, u32)> {
        let overlay = self.diff_review_overlays.first()?;
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let start_point = overlay.anchor_range.start.to_point(&snapshot);
        let end_point = overlay.anchor_range.end.to_point(&snapshot);
        let start_row = snapshot
            .point_to_buffer_point(start_point)
            .map(|(_, p)| p.row)
            .unwrap_or(start_point.row);
        let end_row = snapshot
            .point_to_buffer_point(end_point)
            .map(|(_, p)| p.row)
            .unwrap_or(end_point.row);
        Some((start_row, end_row))
    }

    /// Takes all stored comments from all hunks, clearing the storage.
    /// Returns a Vec of (hunk_key, comments) pairs.
    pub(super) fn take_all_review_comments(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Vec<(DiffHunkKey, Vec<StoredReviewComment>)> {
        // Dismiss all overlays when taking comments (e.g., when sending to agent)
        self.dismiss_all_diff_review_overlays(cx);
        let comments = std::mem::take(&mut self.stored_review_comments);
        // Reset the ID counter since all comments have been taken
        self.next_review_comment_id = 0;
        cx.emit(EditorEvent::ReviewCommentsChanged { total_count: 0 });
        cx.notify();
        comments
    }
}

impl EditorSnapshot {
    pub(super) fn display_diff_hunks_for_rows<'a>(
        &'a self,
        display_rows: Range<DisplayRow>,
        folded_buffers: &'a HashSet<BufferId>,
    ) -> impl 'a + Iterator<Item = DisplayDiffHunk> {
        let buffer_start = DisplayPoint::new(display_rows.start, 0).to_point(self);
        let buffer_end = DisplayPoint::new(display_rows.end, 0).to_point(self);

        self.buffer_snapshot()
            .diff_hunks_in_range(buffer_start..buffer_end)
            .filter_map(|hunk| {
                if folded_buffers.contains(&hunk.buffer_id)
                    || (hunk.row_range.is_empty() && self.buffer.all_diff_hunks_expanded())
                {
                    return None;
                }

                let hunk_start_point = Point::new(hunk.row_range.start.0, 0);
                let hunk_end_point = if hunk.row_range.end > hunk.row_range.start {
                    let last_row = MultiBufferRow(hunk.row_range.end.0 - 1);
                    let line_len = self.buffer_snapshot().line_len(last_row);
                    Point::new(last_row.0, line_len)
                } else {
                    Point::new(hunk.row_range.end.0, 0)
                };

                let hunk_display_start = self.point_to_display_point(hunk_start_point, Bias::Left);
                let hunk_display_end = self.point_to_display_point(hunk_end_point, Bias::Right);

                let display_hunk = if hunk_display_start.column() != 0 {
                    DisplayDiffHunk::Folded {
                        display_row: hunk_display_start.row(),
                    }
                } else {
                    let mut end_row = hunk_display_end.row();
                    if hunk.row_range.end > hunk.row_range.start || hunk_display_end.column() > 0 {
                        end_row.0 += 1;
                    }
                    let is_created_file = hunk.is_created_file();
                    let multi_buffer_range = hunk.multi_buffer_range.clone();

                    DisplayDiffHunk::Unfolded {
                        status: hunk.status(),
                        diff_base_byte_range: hunk.diff_base_byte_range.start.0
                            ..hunk.diff_base_byte_range.end.0,
                        word_diffs: hunk.word_diffs,
                        display_row_range: hunk_display_start.row()..end_row,
                        multi_buffer_range,
                        is_created_file,
                    }
                };

                Some(display_hunk)
            })
    }

    fn hunks_for_ranges(
        &self,
        ranges: impl IntoIterator<Item = Range<Point>>,
    ) -> Vec<MultiBufferDiffHunk> {
        let mut hunks = Vec::new();
        let mut processed_buffer_rows: HashMap<BufferId, HashSet<Range<text::Anchor>>> =
            HashMap::default();
        for query_range in ranges {
            let query_rows =
                MultiBufferRow(query_range.start.row)..MultiBufferRow(query_range.end.row + 1);
            for hunk in self.buffer_snapshot().diff_hunks_in_range(
                Point::new(query_rows.start.0, 0)..Point::new(query_rows.end.0, 0),
            ) {
                // Include deleted hunks that are adjacent to the query range, because
                // otherwise they would be missed.
                let mut intersects_range = hunk.row_range.overlaps(&query_rows);
                if hunk.status().is_deleted() {
                    intersects_range |= hunk.row_range.start == query_rows.end;
                    intersects_range |= hunk.row_range.end == query_rows.start;
                }
                if intersects_range {
                    if !processed_buffer_rows
                        .entry(hunk.buffer_id)
                        .or_default()
                        .insert(hunk.buffer_range.start..hunk.buffer_range.end)
                    {
                        continue;
                    }
                    hunks.push(hunk);
                }
            }
        }

        hunks
    }
}

pub fn set_blame_renderer(renderer: impl BlameRenderer + 'static, cx: &mut App) {
    cx.set_global(GlobalBlameRenderer(Arc::new(renderer)));
}

pub fn render_diff_hunk_controls(
    row: u32,
    status: &DiffHunkStatus,
    hunk_range: Range<Anchor>,
    is_created_file: bool,
    line_height: Pixels,
    editor: &Entity<Editor>,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let operations = hunk_range
        .start
        .buffer_id()
        .and_then(|buffer_id| editor.read(cx).buffer().read(cx).diff_for(buffer_id))
        .and_then(|diff| diff.read(cx).operations());
    let show_stage_restore = ProjectSettings::get_global(cx)
        .git
        .show_stage_restore_buttons;
    let supports_staging = operations
        .as_ref()
        .is_some_and(|operations| operations.supports_staging());
    let supports_unstaging = operations
        .as_ref()
        .is_some_and(|operations| operations.supports_unstaging());
    let supports_restore = operations
        .as_ref()
        .is_some_and(|operations| operations.supports_restore());
    let show_stage_or_unstage = show_stage_restore
        && ((status.has_secondary_hunk() && supports_staging)
            || (!status.has_secondary_hunk() && supports_unstaging));
    let show_restore = show_stage_restore && supports_restore;
    let show_hunk_navigation = !editor.read(cx).buffer().read(cx).all_diff_hunks_expanded();

    if !show_stage_or_unstage && !show_restore && !show_hunk_navigation {
        return gpui::Empty.into_any_element();
    }

    h_flex()
        .h(line_height)
        .mr_1()
        .gap_1()
        .px_0p5()
        .pb_1()
        .border_x_1()
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .rounded_b_lg()
        .bg(cx.theme().colors().editor_background)
        .gap_1()
        .block_mouse_except_scroll()
        .shadow_md()
        .when(show_stage_or_unstage, |el| {
            el.child(if status.has_secondary_hunk() {
                Button::new(("stage", row as u64), "Stage")
                    .alpha(if status.is_pending() { 0.66 } else { 1.0 })
                    .tooltip({
                        let focus_handle = editor.focus_handle(cx);
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                "Stage Hunk",
                                &::git::ToggleStaged,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click({
                        let editor = editor.clone();
                        move |_event, window, cx| {
                            editor.update(cx, |editor, cx| {
                                editor.stage_or_unstage_diff_hunks(
                                    true,
                                    vec![hunk_range.start..hunk_range.start],
                                    window,
                                    cx,
                                );
                            });
                        }
                    })
            } else {
                Button::new(("unstage", row as u64), "Unstage")
                    .alpha(if status.is_pending() { 0.66 } else { 1.0 })
                    .tooltip({
                        let focus_handle = editor.focus_handle(cx);
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                "Unstage Hunk",
                                &::git::ToggleStaged,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click({
                        let editor = editor.clone();
                        move |_event, window, cx| {
                            editor.update(cx, |editor, cx| {
                                editor.stage_or_unstage_diff_hunks(
                                    false,
                                    vec![hunk_range.start..hunk_range.start],
                                    window,
                                    cx,
                                );
                            });
                        }
                    })
            })
        })
        .when(show_restore, |el| {
            el.child(
                Button::new(("restore", row as u64), "Restore")
                    .tooltip({
                        let focus_handle = editor.focus_handle(cx);
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                "Restore Hunk",
                                &::git::Restore,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click({
                        let editor = editor.clone();
                        move |_event, window, cx| {
                            editor.update(cx, |editor, cx| {
                                let snapshot = editor.snapshot(window, cx);
                                let point = hunk_range.start.to_point(&snapshot.buffer_snapshot());
                                editor.restore_hunks_in_ranges(vec![point..point], window, cx);
                            });
                        }
                    })
                    .disabled(is_created_file),
            )
        })
        .when(show_hunk_navigation, |el| {
            el.child(
                IconButton::new(("next-hunk", row as u64), IconName::ArrowDown)
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    // .disabled(!has_multiple_hunks)
                    .tooltip({
                        let focus_handle = editor.focus_handle(cx);
                        move |_window, cx| {
                            Tooltip::for_action_in("Next Hunk", &GoToHunk, &focus_handle, cx)
                        }
                    })
                    .on_click({
                        let editor = editor.clone();
                        move |_event, window, cx| {
                            editor.update(cx, |editor, cx| {
                                let snapshot = editor.snapshot(window, cx);
                                let position = hunk_range.end.to_point(&snapshot.buffer_snapshot());
                                editor.go_to_hunk_before_or_after_position(
                                    &snapshot,
                                    position,
                                    Direction::Next,
                                    true,
                                    window,
                                    cx,
                                );
                                editor.expand_selected_diff_hunks(cx);
                            });
                        }
                    }),
            )
            .child(
                IconButton::new(("prev-hunk", row as u64), IconName::ArrowUp)
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    // .disabled(!has_multiple_hunks)
                    .tooltip({
                        let focus_handle = editor.focus_handle(cx);
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                "Previous Hunk",
                                &GoToPreviousHunk,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click({
                        let editor = editor.clone();
                        move |_event, window, cx| {
                            editor.update(cx, |editor, cx| {
                                let snapshot = editor.snapshot(window, cx);
                                let point = hunk_range.start.to_point(&snapshot.buffer_snapshot());
                                editor.go_to_hunk_before_or_after_position(
                                    &snapshot,
                                    point,
                                    Direction::Prev,
                                    true,
                                    window,
                                    cx,
                                );
                                editor.expand_selected_diff_hunks(cx);
                            });
                        }
                    }),
            )
        })
        .into_any_element()
}

impl Editor {
    pub(super) fn update_uncommitted_diff_for_buffer(
        &mut self,
        project: &Entity<Project>,
        buffers: impl IntoIterator<Item = Entity<Buffer>>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let mut tasks = Vec::new();
        project.update(cx, |project, cx| {
            let git_store = project.git_store().clone();
            git_store.update(cx, |git_store, cx| {
                for buffer in buffers {
                    if project::File::from_dyn(buffer.read(cx).file()).is_some() {
                        tasks.push(git_store.open_display_diff(buffer, cx));
                    }
                }
            });
        });

        let editor = cx.entity();
        let buffer = self.buffer.clone();
        cx.spawn(async move |_, cx| {
            let diffs = future::join_all(tasks).await;
            if editor.read_with(cx, |editor, _cx| editor.diff_hunk_renderer.is_some()) {
                return;
            }

            buffer.update(cx, |buffer, cx| {
                for diff in diffs.into_iter().flatten() {
                    buffer.add_diff(diff, cx);
                }
            });
        })
    }
}
