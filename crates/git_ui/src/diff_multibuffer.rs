use crate::{
    conflict_view,
    git_panel::{GitPanel, GitStatusEntry},
    git_panel_settings::GitPanelSettings,
    gitlab_review::{
        self, Approvals, GitLabComment, RelatedIssue, RemoteDiscussion, ReviewerStanding,
        ReviewerState,
    },
    review_cache::{self, LastReview},
    review_order::{
        MAX_EXCERPT_BYTES_PER_FILE, NotesStreamParser, ReviewNotes, ReviewOrder, ReviewOrderFile,
        SummaryComment, ThreadNote, build_notes_prompt, build_order_prompt, build_reply_prompt,
        build_summary_prompt, link_file_references, order_from_description, parse_review_file_url,
        parse_review_order,
    },
};
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use collections::{HashMap, HashSet};
use editor::{
    Editor, EditorEvent, EditorSettings, ReviewCommentRecord, ReviewReply, ReviewThread,
    SelectionEffects, SplittableEditor,
    actions::GoToHunk,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
    multibuffer_context_lines,
    scroll::Autoscroll,
};
use futures::{FutureExt as _, StreamExt as _, stream};
use futures_lite::future::yield_now;
use git::{repository::RepoPath, status::FileStatus};
use gpui::{
    AnyElement, App, AppContext as _, AsyncWindowContext, DragMoveEvent, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Render,
    SharedString, Subscription, Task, WeakEntity,
};
use language::{Anchor, Buffer, BufferId, Capability, OffsetRangeExt, Point, ToPoint as _};
use language_model::{
    CompletionIntent, LanguageModel, LanguageModelProvider, LanguageModelRegistry,
    LanguageModelRequest, LanguageModelRequestMessage, Role,
};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownOptions,
};
use multi_buffer::{MultiBuffer, PathKey, ToPoint as _};
use project::{
    ConflictSet, Project, ProjectPath,
    git_store::{
        Repository,
        diff_buffer_list::{self, BranchDiffEvent, DiffBase},
    },
};
use settings::{GitPanelGroupBy, GitPanelSortBy, Settings, SettingsStore};
use std::{collections::BTreeMap, fmt::Write as _, sync::Arc};
use theme::ActiveTheme;
use ui::{CommonAnimationExt as _, KeyBinding, Tooltip, prelude::*};
use util::{ResultExt as _, rel_path::RelPath};
use workspace::{
    CloseActiveItem, ItemNavHistory, Toast, Workspace,
    item::{Item, SaveOptions},
    notifications::NotificationId,
};
use ztracing::instrument;

/// Loading every changed file at once makes the first one land no sooner than the
/// last, leaving a large diff on a spinner. Throughput flattens past this point.
const MAX_CONCURRENT_BUFFER_LOADS: usize = 16;

struct BufferSubscriptions {
    _diff: Entity<BufferDiff>,
    display_buffer: Entity<Buffer>,
    _diff_subscription: Subscription,
    _conflict_set: Option<Entity<ConflictSet>>,
    _conflict_set_subscription: Option<Subscription>,
}

pub struct DiffMultibuffer {
    multibuffer: Entity<MultiBuffer>,
    branch_diff: Entity<diff_buffer_list::DiffBufferList>,
    editor: Entity<SplittableEditor>,
    buffer_subscriptions: HashMap<RepoPath, BufferSubscriptions>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    pending_scroll: Option<PathKey>,
    empty_label: SharedString,
    review_order: Option<Arc<ReviewOrder>>,
    review_order_state: ReviewOrderState,
    review_order_blocks: Vec<CustomBlockId>,
    _review_order_task: Option<Task<()>>,
    _review_notes_task: Option<Task<()>>,
    /// Notes that arrived before the order they belong to.
    pending_notes: Option<ReviewNotes>,
    _review_order_wait_task: Option<Task<()>>,
    merge_request: Option<MergeRequestHeader>,
    _merge_request_task: Option<Task<()>>,
    _merge_request_details_task: Option<Task<()>>,
    _review_threads_task: Option<Task<()>>,
    /// The threads, asked for as soon as the merge request is known so they
    /// are ready by the time the files are.
    prefetched_discussions: Option<Task<Result<Vec<RemoteDiscussion>>>>,
    /// Re-runs the thread sync a while after the last one finished.
    _thread_refresh_task: Option<Task<()>>,
    /// A reply, resolve, or draft in flight; one at a time is plenty.
    _review_action_task: Option<Task<()>>,
    review_summary: Option<ReviewSummary>,
    /// A verdict is on its way to GitLab.
    finishing_review: bool,
    /// The base the diff had before it was switched to show only the
    /// changes since the last review, for switching back.
    base_before_since_review: Option<SharedString>,
    /// Comments lifted out of the editor while the excerpts are rebuilt for a
    /// new order, put back once the rebuild finishes.
    held_review_comments: Vec<ReviewCommentRecord>,
    posting_review: bool,
    _post_review_task: Option<Task<()>>,
    _task: Task<Result<()>>,
    _subscription: Subscription,
}

/// The merge request shown above the diff while reviewing.
struct MergeRequestHeader {
    iid: u64,
    title: SharedString,
    author: SharedString,
    web_url: SharedString,
    project_path: Option<String>,
    /// The tip of the source branch as GitLab last saw it.
    head_sha: Option<String>,
    description: Option<Entity<Markdown>>,
    description_text: Option<String>,
    expanded: bool,
    /// The height the user dragged the description to; the default until
    /// they do.
    description_height: Option<Pixels>,
    /// Where a drag of the description's bottom edge began: the pointer's
    /// y and the height at the time.
    description_drag: Option<(Pixels, Pixels)>,
    related_issues: Vec<RelatedIssue>,
    approvals: Option<Approvals>,
    reviewer: Option<ReviewerStanding>,
    last_review: Option<LastReview>,
}

impl MergeRequestHeader {
    /// What the prompts are told about the merge request.
    fn prompt_context(&self) -> String {
        let mut context = format!("Title: {}\n", self.title);
        if let Some(description) = &self.description_text {
            writeln!(context, "Description:\n{}", description.trim()).ok();
        }
        for issue in &self.related_issues {
            writeln!(context, "\nLinked issue #{}: {}", issue.iid, issue.title).ok();
            if let Some(description) = issue.description.as_deref().map(str::trim)
                && !description.is_empty()
            {
                writeln!(context, "{description}").ok();
            }
        }
        context
    }

    /// The commit the user last reviewed, when the branch has moved on
    /// since, so a diff against it shows what is new to them.
    fn stale_last_review(&self) -> Option<&LastReview> {
        let review = self.last_review.as_ref()?;
        let moved_on = self
            .head_sha
            .as_deref()
            .is_none_or(|head| head != review.head_sha);
        moved_on.then_some(review)
    }
}

/// The description's bottom edge while it is being dragged.
#[derive(Clone)]
struct DraggedDescriptionEdge;

impl Render for DraggedDescriptionEdge {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// How tall the description is until the user drags it.
const DEFAULT_DESCRIPTION_HEIGHT: gpui::Rems = rems(18.);
const MIN_DESCRIPTION_HEIGHT: gpui::Rems = rems(3.);
const DESCRIPTION_HANDLE_SIZE: Pixels = px(6.);

/// The summary comment being written to finish a review.
struct ReviewSummary {
    editor: Entity<Editor>,
    drafting: bool,
    posting: bool,
}

/// One review comment as the overview lists it.
#[derive(Clone, Debug)]
pub(crate) struct ReviewCommentEntry {
    pub id: usize,
    pub author: SharedString,
    pub path: String,
    /// One-based line in the file.
    pub line: u32,
    pub snippet: String,
    pub resolved: bool,
    pub posted: bool,
    pub replies: usize,
    anchor: multi_buffer::Anchor,
}

/// How a review is finished.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ReviewVerdict {
    /// Just the summary comment, if any.
    Comment,
    Approve,
    RequestChanges,
}

/// Where a request for a review order is up to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ReviewOrderState {
    Idle,
    /// Waiting for the files to load and a model to become available before
    /// the request can be sent.
    WaitingForModel,
    Suggesting,
    Failed(SharedString),
}

impl DiffMultibuffer {
    pub(crate) fn new(
        branch_diff: Entity<diff_buffer_list::DiffBufferList>,
        multibuffer_capability: Capability,
        empty_label: impl Into<SharedString>,
        configure_editor: impl FnOnce(&mut SplittableEditor, &mut Context<SplittableEditor>) + 'static,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(multibuffer_capability);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let mut diff_display_editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer.clone(),
                project.clone(),
                workspace.clone(),
                window,
                cx,
            );
            configure_editor(&mut diff_display_editor, cx);
            diff_display_editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_show_diff_review_button(true, cx);
            });
            diff_display_editor
        });
        let editor_subscription = cx.subscribe_in(&editor, window, Self::handle_editor_event);

        let primary_editor = editor.read(cx).rhs_editor().clone();
        let review_comment_subscription = cx.subscribe_in(
            &primary_editor,
            window,
            |this, _editor, event: &EditorEvent, window, cx| match event {
                EditorEvent::ReviewCommentsChanged { .. } => cx.notify(),
                EditorEvent::ReviewReplySubmitted {
                    discussion_id,
                    body,
                    ..
                } => this.send_review_reply(discussion_id.clone(), body.clone(), window, cx),
                EditorEvent::ReviewThreadResolveRequested {
                    discussion_id,
                    resolved,
                } => this.resolve_review_thread(discussion_id.clone(), *resolved, window, cx),
                EditorEvent::ReviewReplyDraftRequested { comment_id, .. } => {
                    this.draft_review_reply(*comment_id, window, cx)
                }
                EditorEvent::ReviewCommentEdited {
                    discussion_id,
                    note_id,
                    body,
                } => this.update_remote_comment(
                    discussion_id.clone(),
                    *note_id,
                    body.clone(),
                    window,
                    cx,
                ),
                _ => {}
            },
        );

        let branch_diff_subscription = cx.subscribe_in(
            &branch_diff,
            window,
            move |this, _git_store, event, window, cx| match event {
                BranchDiffEvent::FileListChanged => {
                    this._task = window.spawn(cx, {
                        let this = cx.weak_entity();
                        async |cx| Self::refresh(this, cx).await
                    })
                }
                BranchDiffEvent::DiffBaseChanged => {
                    this.pending_scroll.take();
                    this._task = window.spawn(cx, {
                        let this = cx.weak_entity();
                        async |cx| Self::refresh(this, cx).await
                    })
                }
            },
        );

        let mut was_sort_by = GitPanelSettings::get_global(cx).sort_by;
        let mut was_group_by = GitPanelSettings::get_global(cx).group_by;
        let mut was_tree_view = GitPanelSettings::get_global(cx).tree_view;
        let mut was_collapse_untracked_diff =
            GitPanelSettings::get_global(cx).collapse_untracked_diff;
        cx.observe_global_in::<SettingsStore>(window, move |this, window, cx| {
            let settings = GitPanelSettings::get_global(cx);
            let sort_by = settings.sort_by;
            let group_by = settings.group_by;
            let tree_view = settings.tree_view;
            let is_collapse_untracked_diff = settings.collapse_untracked_diff;
            if sort_by != was_sort_by
                || group_by != was_group_by
                || tree_view != was_tree_view
                || is_collapse_untracked_diff != was_collapse_untracked_diff
            {
                this._task = {
                    window.spawn(cx, {
                        let this = cx.weak_entity();
                        async |cx| Self::refresh(this, cx).await
                    })
                }
            }
            was_sort_by = sort_by;
            was_group_by = group_by;
            was_tree_view = tree_view;
            was_collapse_untracked_diff = is_collapse_untracked_diff;
        })
        .detach();

        let task = window.spawn(cx, {
            let this = cx.weak_entity();
            async |cx| Self::refresh(this, cx).await
        });

        let mut this = Self {
            workspace: workspace.downgrade(),
            branch_diff,
            focus_handle,
            editor,
            multibuffer,
            buffer_subscriptions: Default::default(),
            pending_scroll: None,
            empty_label: empty_label.into(),
            review_order: None,
            review_order_state: ReviewOrderState::Idle,
            review_order_blocks: Vec::new(),
            _review_order_task: None,
            _review_notes_task: None,
            pending_notes: None,
            _review_order_wait_task: None,
            merge_request: None,
            _merge_request_task: None,
            _merge_request_details_task: None,
            _review_threads_task: None,
            prefetched_discussions: None,
            _thread_refresh_task: None,
            _review_action_task: None,
            review_summary: None,
            finishing_review: false,
            base_before_since_review: None,
            held_review_comments: Vec::new(),
            posting_review: false,
            _post_review_task: None,
            _task: task,
            _subscription: Subscription::join(
                branch_diff_subscription,
                Subscription::join(editor_subscription, review_comment_subscription),
            ),
        };
        if matches!(this.diff_base(cx), DiffBase::Merge { .. }) {
            this.load_threads_when_settled(window, cx);
        }
        this
    }

    /// Shows the branch's merge request and its threads once the files have
    /// loaded, so a branch diff of your own merge request carries the
    /// reviewers' comments too. A branch with no merge request logs and
    /// shows nothing.
    fn load_threads_when_settled(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        const POLL_INTERVAL: std::time::Duration = DiffMultibuffer::SETTLE_POLL;
        const MAX_POLLS: usize = 600;

        self._review_threads_task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut settled_count = None;
            for _ in 0..MAX_POLLS {
                let settled =
                    this.update(cx, |this, cx| this.files_settled(&mut settled_count, cx));
                match settled {
                    Ok(true) => break,
                    Ok(false) => cx.background_executor().timer(POLL_INTERVAL).await,
                    Err(_) => return,
                }
            }
            this.update_in(cx, |this, window, cx| {
                this.load_merge_request(None, cx);
                this.sync_review_threads(window, cx);
            })
            .ok();
        }));
    }

    /// Whether every file has loaded and the count has held still since the
    /// last call. The files arrive in batches, so one poll is not enough.
    fn files_settled(&self, settled_count: &mut Option<usize>, cx: &App) -> bool {
        let count = self.buffer_subscriptions.len();
        let loaded = count > 0 && !self.is_loading(cx);
        let settled = loaded && *settled_count == Some(count);
        *settled_count = loaded.then_some(count);
        settled
    }

    /// How often the loaders are asked whether the files have settled.
    const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(100);

    pub(crate) fn diff_base<'a>(&'a self, cx: &'a App) -> &'a DiffBase {
        self.branch_diff.read(cx).diff_base()
    }

    pub(crate) fn branch_diff(&self) -> &Entity<diff_buffer_list::DiffBufferList> {
        &self.branch_diff
    }

    pub(crate) fn repo(&self, cx: &App) -> Option<Entity<Repository>> {
        self.branch_diff.read(cx).repo().cloned()
    }

    pub(crate) fn set_repo(&mut self, repo: Option<Entity<Repository>>, cx: &mut Context<Self>) {
        self.branch_diff.update(cx, |branch_diff, cx| {
            branch_diff.set_repo(repo, cx);
        });
    }

    pub(crate) fn is_dirty(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).is_dirty(cx)
    }

    pub(crate) fn has_conflict(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).has_conflict(cx)
    }

    pub(crate) fn multibuffer(&self) -> &Entity<MultiBuffer> {
        &self.multibuffer
    }

    pub(crate) fn move_to_entry(
        &mut self,
        entry: GitStatusEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(git_repo) = self.branch_diff.read(cx).repo() else {
            return;
        };
        let repo = git_repo.read(cx);
        let path_key = project_diff_path_key(
            repo,
            &entry.repo_path,
            entry.status,
            self.review_order.as_deref(),
            cx,
        );

        self.move_to_path(path_key, window, cx)
    }

    pub(crate) fn move_to_project_path(
        &mut self,
        project_path: &ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(git_repo) = self.branch_diff.read(cx).repo() else {
            return;
        };
        let Some(repo_path) = git_repo
            .read(cx)
            .project_path_to_repo_path(project_path, cx)
        else {
            return;
        };
        let status = git_repo
            .read(cx)
            .status_for_path(&repo_path)
            .map(|entry| entry.status)
            .unwrap_or(FileStatus::Untracked);
        let path_key = project_diff_path_key(
            &git_repo.read(cx),
            &repo_path,
            status,
            self.review_order.as_deref(),
            cx,
        );
        self.move_to_path(path_key, window, cx)
    }

    pub(crate) fn move_to_beginning(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |s| {
                    s.select_ranges(vec![multi_buffer::Anchor::Min..multi_buffer::Anchor::Min]);
                });
            });
        });
    }

    pub(crate) fn move_to_path(
        &mut self,
        path_key: PathKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(position) = self.multibuffer.read(cx).location_for_path(&path_key, cx) {
            self.editor.update(cx, |editor, cx| {
                editor.rhs_editor().update(cx, |editor, cx| {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::focused()),
                        window,
                        cx,
                        |s| {
                            s.select_ranges([position..position]);
                        },
                    )
                })
            });
        } else {
            self.pending_scroll = Some(path_key);
        }
    }

    pub(crate) fn autoscroll(&self, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.request_autoscroll(Autoscroll::fit(), cx);
            })
        })
    }

    pub(crate) fn calculate_changed_lines(&self, cx: &App) -> (u32, u32) {
        self.multibuffer.read(cx).snapshot(cx).total_changed_lines()
    }

    pub(crate) fn review_order(&self) -> Option<&Arc<ReviewOrder>> {
        self.review_order.as_ref()
    }

    pub(crate) fn review_order_state(&self) -> &ReviewOrderState {
        &self.review_order_state
    }

    /// Sorts the excerpts by `order`, or back to the git panel's order for `None`.
    pub(crate) fn set_review_order(
        &mut self,
        order: Option<Arc<ReviewOrder>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._review_order_task.take();
        self.review_order_state = ReviewOrderState::Idle;
        let order = match (order, self.pending_notes.take()) {
            (Some(order), Some(notes)) => Some(Arc::new(order.with_notes(notes))),
            (order, _) => order,
        };
        if order.is_none() {
            self._review_notes_task.take();
        }
        if self.review_order == order {
            cx.notify();
            return;
        }
        self.review_order = order;
        self.pending_scroll.take();
        // Re-sorting rebuilds the excerpts, which would orphan the comments'
        // anchors, so hold them by file and row until the refresh is done.
        let records = self
            .editor
            .read(cx)
            .rhs_editor()
            .clone()
            .update(cx, |editor, cx| editor.take_review_comment_records(cx));
        self.held_review_comments.extend(records);
        self._task = window.spawn(cx, {
            let this = cx.weak_entity();
            async |cx| Self::refresh(this, cx).await
        });
        cx.notify();
    }

    pub(crate) fn clear_review_order(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_review_order(None, window, cx);
    }

    /// Forgets everything about the merge request being reviewed, for when
    /// the same diff view is reused for another branch: the header and its
    /// approvals, the order and notes, the comments and threads, and the
    /// summary box.
    pub(crate) fn reset_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._merge_request_task.take();
        self._merge_request_details_task.take();
        self._review_threads_task.take();
        self._thread_refresh_task.take();
        self._review_action_task.take();
        self._review_order_wait_task.take();
        self._review_notes_task.take();
        self.merge_request = None;
        self.review_summary = None;
        self.pending_notes = None;
        self.held_review_comments.clear();
        self.posting_review = false;
        self.finishing_review = false;
        self.editor
            .read(cx)
            .rhs_editor()
            .clone()
            .update(cx, |editor, cx| {
                editor.take_review_comment_records(cx);
            });
        self.set_review_order(None, window, cx);
        self.held_review_comments.clear();
        // The new branch's threads come in once its files have loaded.
        self.load_threads_when_settled(window, cx);
        cx.notify();
    }

    /// Shows the open merge request for the current branch above the diff.
    /// Reads it from `file` when the launcher already fetched it, else asks
    /// GitLab. No merge request is not an error.
    pub(crate) fn load_merge_request(
        &mut self,
        file: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.merge_request.is_some() || self._merge_request_task.is_some() {
            return;
        }
        let Some(repo) = self.branch_diff.read(cx).repo().cloned() else {
            return;
        };
        let (work_dir, branch) = {
            let repo = repo.read(cx);
            (
                repo.work_directory_abs_path.to_path_buf(),
                repo.branch.as_ref().map(|branch| branch.name().to_string()),
            )
        };
        let Some(branch) = branch else {
            return;
        };
        let fetch = cx.background_spawn(async move {
            match file {
                Some(file) => gitlab_review::merge_request_from_file(&file),
                None => gitlab_review::merge_request_for_branch(work_dir, branch).await,
            }
        });
        self._merge_request_task = Some(cx.spawn(async move |this, cx| {
            let merge_request = match fetch.await {
                Ok(merge_request) => merge_request,
                Err(error) => {
                    log::warn!("no merge request to show above the diff: {error:#}");
                    return;
                }
            };
            this.update(cx, |this, cx| {
                let languages = this
                    .workspace
                    .upgrade()
                    .map(|workspace| workspace.read(cx).project().read(cx).languages().clone());
                let project_path = merge_request.project_path();
                let head_sha = merge_request.head_sha().map(str::to_string);
                let description_text = merge_request
                    .description
                    .filter(|text| !text.trim().is_empty());
                let description = description_text.clone().map(|text| {
                    cx.new(|cx| {
                        // Descriptions carry diagrams and HTML details
                        // blocks, which the defaults leave as text.
                        Markdown::new_with_options(
                            text.into(),
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
                });
                let last_review = review_cache::load_last_review(&merge_request.web_url)
                    .log_err()
                    .flatten();
                this.merge_request = Some(MergeRequestHeader {
                    iid: merge_request.iid,
                    title: merge_request.title.into(),
                    author: merge_request
                        .author
                        .map(|author| author.name)
                        .unwrap_or_default()
                        .into(),
                    project_path,
                    head_sha,
                    web_url: merge_request.web_url.into(),
                    description,
                    description_text,
                    expanded: true,
                    description_height: None,
                    description_drag: None,
                    related_issues: Vec::new(),
                    approvals: None,
                    reviewer: None,
                    last_review,
                });
                this.load_merge_request_details(cx);
                this.prefetch_discussions(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Fetches what the header shows besides the description: the linked
    /// issues, who approved, and the user's own reviewer state. Each is
    /// optional, so a failure only logs.
    fn load_merge_request_details(&mut self, cx: &mut Context<Self>) {
        let Some((work_dir, iid)) = self.work_dir(cx).zip(self.merge_request_iid()) else {
            return;
        };
        let issues =
            cx.background_spawn(gitlab_review::fetch_related_issues(work_dir.clone(), iid));
        let approvals = cx.background_spawn(gitlab_review::fetch_approvals(work_dir.clone(), iid));
        let reviewer = cx.background_spawn(gitlab_review::fetch_reviewer_standing(work_dir, iid));
        self._merge_request_details_task = Some(cx.spawn(async move |this, cx| {
            let (issues, approvals, reviewer) = futures::join!(issues, approvals, reviewer);
            this.update(cx, |this, cx| {
                let Some(header) = this.merge_request.as_mut() else {
                    return;
                };
                match issues {
                    Ok(issues) => header.related_issues = issues,
                    Err(error) => log::warn!("no linked issues for the review: {error:#}"),
                }
                match approvals {
                    Ok(approvals) => header.approvals = Some(approvals),
                    Err(error) => log::warn!("no approvals for the review: {error:#}"),
                }
                match reviewer {
                    Ok(reviewer) => header.reviewer = Some(reviewer),
                    Err(error) => log::warn!("no reviewer state for the review: {error:#}"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Starts fetching the threads now; placing them waits for the files.
    fn prefetch_discussions(&mut self, cx: &mut Context<Self>) {
        let Some((work_dir, iid)) = self.work_dir(cx).zip(self.merge_request_iid()) else {
            return;
        };
        self.prefetched_discussions =
            Some(cx.background_spawn(gitlab_review::fetch_discussions(work_dir, iid)));
    }

    fn work_dir(&self, cx: &App) -> Option<std::path::PathBuf> {
        let repo = self.branch_diff.read(cx).repo()?;
        Some(repo.read(cx).work_directory_abs_path.to_path_buf())
    }

    fn merge_request_iid(&self) -> Option<u64> {
        self.merge_request.as_ref().map(|header| header.iid)
    }

    /// The tip of the checked out branch.
    fn head_sha(&self, cx: &App) -> Option<String> {
        let repo = self.branch_diff.read(cx).repo()?;
        repo.read(cx)
            .head_commit
            .as_ref()
            .map(|commit| commit.sha.to_string())
    }

    /// What the prompts are told about the merge request, once it is known.
    fn merge_request_context(&self) -> Option<String> {
        self.merge_request
            .as_ref()
            .map(MergeRequestHeader::prompt_context)
    }

    /// What a thread about this diff is told before the user's question:
    /// the merge request, then the reading order with its notes.
    pub(crate) fn ask_context(&self) -> String {
        let mut context = match self.merge_request.as_ref() {
            Some(header) => format!(
                "I am reviewing merge request !{} by {}.\n{}",
                header.iid,
                header.author,
                header.prompt_context()
            ),
            None => "I am reviewing this branch diff.\n".to_string(),
        };
        if let Some(order) = &self.review_order {
            context.push_str("\nSuggested reading order:\n");
            for (index, entry) in order.entries().iter().enumerate() {
                writeln!(
                    context,
                    "{}. {} — {}",
                    index + 1,
                    entry.path.as_unix_str(),
                    entry.reason
                )
                .ok();
                for note in &entry.notes {
                    writeln!(context, "   line {}: {}", note.line, note.text).ok();
                }
            }
        }
        context
    }

    /// Whether the merge request and its details have been looked up, or
    /// there is nothing to look up. The prompts wait for this so the notes
    /// can draw on the description.
    /// Whether the merge request itself has been read, which the prompts
    /// wait for since its description shapes them. Its details, the linked
    /// issues and approvals, are not waited for.
    fn merge_request_settled(&self) -> bool {
        self._merge_request_task
            .as_ref()
            .is_none_or(|task| task.is_ready())
    }

    pub(crate) fn has_merge_request(&self) -> bool {
        self.merge_request.is_some()
    }

    /// Whether the user has approved the merge request, once known, and
    /// whether a verdict is in flight.
    pub(crate) fn approval_state(&self) -> Option<(bool, bool)> {
        let approvals = self.merge_request.as_ref()?.approvals.as_ref()?;
        Some((approvals.user_has_approved, self.finishing_review))
    }

    /// The commit the user last reviewed, when the branch has moved on
    /// since, with whether the diff is currently against it.
    pub(crate) fn changes_since_review(&self, cx: &App) -> Option<(LastReview, bool)> {
        let review = self.merge_request.as_ref()?.stale_last_review()?.clone();
        let showing = matches!(
            self.diff_base(cx),
            DiffBase::Merge { base_ref } if base_ref.as_ref() == review.head_sha
        );
        Some((review, showing))
    }

    /// Diffs against the commit the user last reviewed, so only what changed
    /// since is shown, or back against the usual base.
    pub(crate) fn toggle_changes_since_review(&mut self, cx: &mut Context<Self>) {
        let Some((review, showing)) = self.changes_since_review(cx) else {
            return;
        };
        let base_ref = if showing {
            match self.base_before_since_review.take() {
                Some(base_ref) => base_ref,
                None => return,
            }
        } else {
            if let DiffBase::Merge { base_ref } = self.diff_base(cx) {
                self.base_before_since_review = Some(base_ref.clone());
            }
            SharedString::from(review.head_sha)
        };
        self.branch_diff.update(cx, |branch_diff, cx| {
            branch_diff.set_diff_base(DiffBase::Merge { base_ref }, cx);
        });
        cx.notify();
    }

    /// Records the current tip as reviewed, so the next visit can show only
    /// what changed after it.
    pub(crate) fn mark_reviewed(&mut self, cx: &mut Context<Self>) {
        let Some(head_sha) = self.head_sha(cx) else {
            return;
        };
        let Some(header) = self.merge_request.as_mut() else {
            return;
        };
        let review = LastReview {
            head_sha,
            at: time::OffsetDateTime::now_utc().unix_timestamp(),
        };
        review_cache::store_last_review(&header.web_url, &review).log_err();
        header.last_review = Some(review);
        cx.notify();
    }

    fn render_merge_request_header(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let header = self.merge_request.as_ref()?;
        let web_url = header.web_url.clone();
        let title_row = h_flex()
            .w_full()
            .gap_2()
            .px_2()
            .py_1()
            .child(
                IconButton::new(
                    "merge-request-toggle",
                    if header.expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    },
                )
                .icon_size(IconSize::Small)
                .on_click(cx.listener(|this, _, _, cx| this.toggle_merge_request_description(cx))),
            )
            .child(Label::new(header.title.clone()).weight(FontWeight::SEMIBOLD))
            .child(
                Label::new(format!("!{} · {}", header.iid, header.author))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(header.approvals.as_ref().and_then(|approvals| {
                (!approvals.approved_by.is_empty()).then(|| {
                    Label::new(format!("Approved by {}", approvals.approved_by.join(", ")))
                        .size(LabelSize::Small)
                        .color(Color::Success)
                })
            }))
            .children(header.reviewer.as_ref().and_then(|reviewer| {
                let (text, color) = match reviewer.state {
                    ReviewerState::RequestedChanges => ("You requested changes", Color::Warning),
                    ReviewerState::Approved => ("You approved", Color::Success),
                    ReviewerState::Reviewed => ("You reviewed", Color::Muted),
                    ReviewerState::Unreviewed => return None,
                };
                Some(Label::new(text).size(LabelSize::Small).color(color))
            }))
            .child(div().flex_1())
            .child(
                IconButton::new("merge-request-open", IconName::ArrowUpRight)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Open the merge request in the browser"))
                    .on_click(move |_, _, cx| cx.open_url(&web_url)),
            );
        let description_height = header
            .description_height
            .unwrap_or_else(|| DEFAULT_DESCRIPTION_HEIGHT.to_pixels(window.rem_size()));
        let description = header
            .expanded
            .then(|| header.description.clone())
            .flatten()
            .map(|markdown| {
                // The bottom edge drags to resize, and double-clicks back to
                // the default height.
                let handle = div()
                    .id("merge-request-description-handle")
                    .w_full()
                    .h(DESCRIPTION_HANDLE_SIZE)
                    .flex_shrink_0()
                    .cursor_row_resize()
                    .hover(|style| style.bg(cx.theme().colors().border_focused))
                    .on_drag(DraggedDescriptionEdge, |edge, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| edge.clone())
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            if let Some(header) = this.merge_request.as_mut() {
                                header.description_drag =
                                    Some((event.position.y, description_height));
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseUpEvent, _, cx| {
                            if event.click_count == 2
                                && let Some(header) = this.merge_request.as_mut()
                            {
                                header.description_height = None;
                                cx.notify();
                            }
                        }),
                    );
                v_flex()
                    .w_full()
                    .child(
                        div()
                            .id("merge-request-description")
                            .w_full()
                            .h(description_height)
                            .overflow_y_scroll()
                            .px_3()
                            .pb_2()
                            .text_sm()
                            .child(
                                MarkdownElement::new(
                                    markdown,
                                    editor::hover_markdown_style(window, cx),
                                )
                                .code_block_renderer(
                                    CodeBlockRenderer::Default {
                                        copy_button_visibility:
                                            CopyButtonVisibility::VisibleOnHover,
                                        wrap_button_visibility:
                                            markdown::WrapButtonVisibility::Hidden,
                                        border: true,
                                    },
                                ),
                            ),
                    )
                    .child(handle)
            });
        let summary = self.render_review_summary(cx);
        let colors = cx.theme().colors();
        Some(
            v_flex()
                .w_full()
                .bg(colors.panel_background)
                .border_b_1()
                .border_color(colors.border)
                .child(title_row)
                .children(description)
                .children(summary)
                .into_any_element(),
        )
    }

    /// Follows a drag of the description's bottom edge, keeping the height
    /// between a few lines and the window.
    fn drag_description_edge(
        &mut self,
        event: &DragMoveEvent<DraggedDescriptionEdge>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(header) = self.merge_request.as_mut() else {
            return;
        };
        let Some((start_y, start_height)) = header.description_drag else {
            return;
        };
        let min = MIN_DESCRIPTION_HEIGHT.to_pixels(window.rem_size());
        let max = window.viewport_size().height * 0.8;
        let height = (start_height + event.event.position.y - start_y).clamp(min, max);
        if header.description_height != Some(height) {
            header.description_height = Some(height);
            cx.notify();
        }
    }

    pub(crate) fn toggle_merge_request_description(&mut self, cx: &mut Context<Self>) {
        if let Some(header) = self.merge_request.as_mut() {
            header.expanded = !header.expanded;
            cx.notify();
        }
    }

    /// Opens the box for the summary comment that finishes the review, or
    /// closes it.
    pub(crate) fn toggle_review_summary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.review_summary.take().is_some() {
            cx.notify();
            return;
        }
        if self.merge_request.is_none() {
            return;
        }
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 12, window, cx);
            editor.set_placeholder_text(
                "Summarise your review, or draft one from your comments…",
                window,
                cx,
            );
            editor
        });
        let focus_handle = editor.focus_handle(cx);
        window.focus(&focus_handle, cx);
        self.review_summary = Some(ReviewSummary {
            editor,
            drafting: false,
            posting: false,
        });
        cx.notify();
    }

    fn render_review_summary(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let summary = self.review_summary.as_ref()?;
        let header = self.merge_request.as_ref()?;
        let colors = cx.theme().colors();
        let busy = summary.drafting || summary.posting;
        let approved = header
            .approvals
            .as_ref()
            .is_some_and(|approvals| approvals.user_has_approved);
        let changes_requested = header
            .reviewer
            .as_ref()
            .is_some_and(|reviewer| reviewer.state == ReviewerState::RequestedChanges);
        let status = if summary.drafting {
            "Drafting from your comments…"
        } else if summary.posting {
            "Sending to GitLab…"
        } else {
            "Finish your review"
        };
        let buttons = h_flex()
            .w_full()
            .gap_1()
            .child(
                Button::new("review-summary-draft", "Draft from Comments")
                    .start_icon(Icon::new(IconName::Sparkle).size(IconSize::Small))
                    .disabled(busy)
                    .tooltip(Tooltip::text(
                        "Ask the model to write a summary of your comments",
                    ))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.draft_review_summary(window, cx)),
                    ),
            )
            .child(div().flex_1())
            .child(
                Button::new("review-summary-comment", "Comment")
                    .disabled(busy)
                    .tooltip(Tooltip::text("Post the summary without a verdict"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.finish_review(ReviewVerdict::Comment, window, cx)
                    })),
            )
            .child(
                Button::new(
                    "review-summary-request-changes",
                    if changes_requested {
                        "Withdraw Request"
                    } else {
                        "Request Changes"
                    },
                )
                .disabled(busy)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.finish_review(ReviewVerdict::RequestChanges, window, cx)
                })),
            )
            .child(
                Button::new(
                    "review-summary-approve",
                    if approved { "Unapprove" } else { "Approve" },
                )
                .style(ButtonStyle::Filled)
                .disabled(busy)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.finish_review(ReviewVerdict::Approve, window, cx)
                })),
            )
            .child(
                IconButton::new("review-summary-close", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Close"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.toggle_review_summary(window, cx)),
                    ),
            );
        Some(
            v_flex()
                .w_full()
                .gap_1()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(colors.border)
                .child(
                    Label::new(status)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    div()
                        .w_full()
                        .border_1()
                        .border_color(colors.border)
                        .rounded_md()
                        .bg(colors.editor_background)
                        .px_2()
                        .py_1()
                        .child(summary.editor.clone()),
                )
                .child(buttons)
                .into_any_element(),
        )
    }

    /// The user's own comments, posted or not, with their place in the new
    /// version of the file, for the summary prompt.
    fn own_comments_for_summary(&self, cx: &App) -> Vec<SummaryComment> {
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        self.editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .review_comments()
            .into_iter()
            .filter(|comment| comment.author.is_none())
            .filter_map(|comment| {
                let end = comment.range.end.to_point(&snapshot);
                let (_, point) = snapshot.point_to_buffer_point(end)?;
                Some(SummaryComment {
                    path: comment.file_path.as_unix_str().to_string(),
                    line: point.row + 1,
                    text: comment.text,
                })
            })
            .collect()
    }

    /// Asks the model for a summary of the user's comments and puts it in
    /// the summary box for editing.
    fn draft_review_summary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let comments = self.own_comments_for_summary(cx);
        if comments.is_empty() {
            self.show_workspace_error(
                "Write some inline comments first; the summary is drawn from them.".into(),
                cx,
            );
            return;
        }
        let Some(summary) = self.review_summary.as_mut() else {
            return;
        };
        summary.drafting = true;
        cx.notify();
        let prompt = build_summary_prompt(self.merge_request_context().as_deref(), &comments);
        let request = self.model_request(prompt, cx);
        self._review_action_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match request {
                Ok((provider, model, request)) => complete_text(provider, model, request, cx).await,
                Err(error) => Err(error),
            };
            this.update_in(cx, |this, window, cx| {
                let Some(summary) = this.review_summary.as_mut() else {
                    return;
                };
                summary.drafting = false;
                match result {
                    Ok(text) => {
                        let editor = summary.editor.clone();
                        editor.update(cx, |editor, cx| {
                            editor.set_text(text.trim().to_string(), window, cx)
                        });
                    }
                    Err(error) => this.show_workspace_error(
                        format!("Could not draft the summary: {error:#}"),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Posts the summary, if any, then gives the verdict, and records the
    /// tip as reviewed.
    pub(crate) fn finish_review(
        &mut self,
        verdict: ReviewVerdict,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((work_dir, header)) = self.work_dir(cx).zip(self.merge_request.as_ref()) else {
            return;
        };
        let iid = header.iid;
        let project_path = header.project_path.clone();
        let approved = header
            .approvals
            .as_ref()
            .is_some_and(|approvals| approvals.user_has_approved);
        let changes_requested = header
            .reviewer
            .as_ref()
            .is_some_and(|reviewer| reviewer.state == ReviewerState::RequestedChanges);
        if self.finishing_review {
            return;
        }
        // From the toolbar there is no summary box; the verdict goes alone.
        let body = self
            .review_summary
            .as_mut()
            .map(|summary| {
                summary.posting = true;
                summary.editor.read(cx).text(cx).trim().to_string()
            })
            .unwrap_or_default();
        self.finishing_review = true;
        cx.notify();

        let request = cx.background_spawn(async move {
            if !body.is_empty() {
                gitlab_review::post_note(work_dir.clone(), iid, body).await?;
            }
            match verdict {
                ReviewVerdict::Comment => {}
                ReviewVerdict::Approve => {
                    gitlab_review::set_approval(work_dir, iid, !approved).await?
                }
                ReviewVerdict::RequestChanges => {
                    let project_path =
                        project_path.context("the merge request's project path is unknown")?;
                    gitlab_review::set_changes_requested(
                        work_dir,
                        project_path,
                        iid,
                        !changes_requested,
                    )
                    .await?
                }
            }
            anyhow::Ok(())
        });
        self._review_action_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| {
                this.finishing_review = false;
                if let Some(summary) = this.review_summary.as_mut() {
                    summary.posting = false;
                }
                match result {
                    Ok(()) => {
                        let what = match verdict {
                            ReviewVerdict::Comment => "Posted your summary",
                            ReviewVerdict::Approve if approved => "Withdrew your approval",
                            ReviewVerdict::Approve => "Approved",
                            ReviewVerdict::RequestChanges if changes_requested => {
                                "Withdrew your request for changes"
                            }
                            ReviewVerdict::RequestChanges => "Requested changes",
                        };
                        this.review_summary.take();
                        // Show the verdict at once; the details fetch confirms it.
                        if let Some(header) = this.merge_request.as_mut() {
                            match verdict {
                                ReviewVerdict::Approve => {
                                    if let Some(approvals) = header.approvals.as_mut() {
                                        approvals.user_has_approved = !approved;
                                    }
                                }
                                ReviewVerdict::RequestChanges => {
                                    if let Some(reviewer) = header.reviewer.as_mut() {
                                        reviewer.state = if changes_requested {
                                            ReviewerState::Reviewed
                                        } else {
                                            ReviewerState::RequestedChanges
                                        };
                                    }
                                }
                                ReviewVerdict::Comment => {}
                            }
                        }
                        this.mark_reviewed(cx);
                        this.load_merge_request_details(cx);
                        this.show_review_toast(format!("{what} on !{iid}"), cx);
                        this.sync_review_threads(window, cx);
                    }
                    Err(error) => this.show_workspace_error(
                        format!("Could not finish the review: {error:#}"),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn show_review_toast(&self, message: String, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let web_url = self
            .merge_request
            .as_ref()
            .map(|header| header.web_url.to_string());
        workspace.update(cx, |workspace, cx| {
            struct ReviewToast;
            let mut toast = Toast::new(NotificationId::unique::<ReviewToast>(), message).autohide();
            if let Some(web_url) = web_url {
                toast = toast.on_click("Open", move |_, cx| cx.open_url(&web_url));
            }
            workspace.show_toast(toast, cx);
        });
    }

    /// Sends `body` as a reply on the thread and shows it under the comment.
    fn send_review_reply(
        &mut self,
        discussion_id: String,
        body: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((work_dir, iid)) = self.work_dir(cx).zip(self.merge_request_iid()) else {
            self.show_workspace_error("No merge request is loaded to reply on.".into(), cx);
            return;
        };
        let request = cx.background_spawn(gitlab_review::reply_to_discussion(
            work_dir,
            iid,
            discussion_id.clone(),
            body,
        ));
        self._review_action_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(note) => {
                    this.editor
                        .read(cx)
                        .rhs_editor()
                        .clone()
                        .update(cx, |editor, cx| {
                            editor.append_review_reply(
                                &discussion_id,
                                ReviewReply {
                                    author: note.author.into(),
                                    body: note.body,
                                },
                                window,
                                cx,
                            );
                        });
                }
                Err(error) => {
                    this.show_workspace_error(format!("Could not send the reply: {error:#}"), cx)
                }
            })
            .ok();
        }));
    }

    /// Puts the edited text of one of the user's posted comments on its
    /// GitLab note. The editor already shows the new text; a failure says
    /// the remote still has the old one.
    fn update_remote_comment(
        &mut self,
        discussion_id: String,
        note_id: u64,
        body: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((work_dir, iid)) = self.work_dir(cx).zip(self.merge_request_iid()) else {
            return;
        };
        let request = cx.background_spawn(gitlab_review::update_note(
            work_dir,
            iid,
            discussion_id,
            note_id,
            body,
        ));
        self._review_action_task = Some(cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = request.await {
                this.update(cx, |this, cx| {
                    this.show_workspace_error(
                        format!("The comment changed here but not on GitLab: {error:#}"),
                        cx,
                    )
                })
                .ok();
            }
        }));
    }

    /// Resolves or reopens the thread on GitLab, then shows it so.
    fn resolve_review_thread(
        &mut self,
        discussion_id: String,
        resolved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((work_dir, iid)) = self.work_dir(cx).zip(self.merge_request_iid()) else {
            return;
        };
        let request = cx.background_spawn(gitlab_review::resolve_discussion(
            work_dir,
            iid,
            discussion_id.clone(),
            resolved,
        ));
        self._review_action_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    this.editor
                        .read(cx)
                        .rhs_editor()
                        .clone()
                        .update(cx, |editor, cx| {
                            editor.set_review_thread_resolved(&discussion_id, resolved, window, cx);
                        });
                }
                Err(error) => this.show_workspace_error(
                    format!("Could not change the thread's resolved state: {error:#}"),
                    cx,
                ),
            })
            .ok();
        }));
    }

    /// Asks the model for a reply to the thread under `comment_id` and puts
    /// it in the reply box for editing.
    fn draft_review_reply(
        &mut self,
        comment_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.editor.read(cx).rhs_editor().clone();
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let Some(comment) = editor
            .read(cx)
            .review_comments()
            .into_iter()
            .find(|comment| comment.id == comment_id)
        else {
            return;
        };
        let Some(thread) = comment.thread.as_ref() else {
            return;
        };
        let own_name = self
            .merge_request
            .as_ref()
            .and_then(|header| header.reviewer.as_ref())
            .map(|reviewer| reviewer.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "the reviewer".to_string());
        let mut notes = vec![ThreadNote {
            author: comment
                .author
                .as_ref()
                .map(|author| author.to_string())
                .unwrap_or_else(|| own_name.clone()),
            body: comment.text.clone(),
        }];
        notes.extend(thread.replies.iter().map(|reply| ThreadNote {
            author: reply.author.to_string(),
            body: reply.body.clone(),
        }));
        let code: String = snapshot.text_for_range(comment.range.clone()).collect();
        let prompt = build_reply_prompt(
            self.merge_request_context().as_deref(),
            comment.file_path.as_unix_str(),
            &code,
            &notes,
            &own_name,
        );
        let request = self.model_request(prompt, cx);
        self._review_action_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match request {
                Ok((provider, model, request)) => complete_text(provider, model, request, cx).await,
                Err(error) => Err(error),
            };
            this.update_in(cx, |this, window, cx| match result {
                Ok(text) => editor.update(cx, |editor, cx| {
                    editor.set_review_reply_draft(comment_id, text.trim().to_string(), window, cx)
                }),
                Err(error) => {
                    this.show_workspace_error(format!("Could not draft a reply: {error:#}"), cx)
                }
            })
            .ok();
        }));
    }

    /// A request for `prompt` to the commit message model, with its provider.
    fn model_request(
        &self,
        prompt: String,
        cx: &App,
    ) -> Result<(
        Arc<dyn LanguageModelProvider>,
        LanguageModel,
        LanguageModelRequest,
    )> {
        let registry = LanguageModelRegistry::read_global(cx);
        let model = registry
            .commit_message_model(cx)
            .or_else(|| registry.default_fast_model(cx))
            .context("no language model is available")?;
        let provider = registry.provider_for_model(&model)?;
        let request = text_request(prompt, &model, cx);
        Ok((provider, model, request))
    }

    /// Whether the file list or the file contents are still being loaded.
    fn is_loading(&self, cx: &App) -> bool {
        self.branch_diff.read(cx).is_tree_base_loading() || !self._task.is_ready()
    }

    /// Asks for a review order once every file has loaded and a model is
    /// available, which can take a moment right after the app starts. Keeps an
    /// order that is already set.
    pub(crate) fn suggest_review_order_when_loaded(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        const POLL_INTERVAL: std::time::Duration = DiffMultibuffer::SETTLE_POLL;
        const MAX_POLLS: usize = 300;

        if self.review_order.is_some()
            || matches!(
                self.review_order_state,
                ReviewOrderState::Suggesting | ReviewOrderState::WaitingForModel
            )
        {
            return;
        }
        self.review_order_state = ReviewOrderState::WaitingForModel;
        cx.notify();
        // Providers normally load their keys when the agent panel starts, which
        // does not happen when the panel is hidden, so load them here.
        let registry = LanguageModelRegistry::global(cx);
        let authentications: Vec<_> = registry
            .read(cx)
            .visible_providers()
            .into_iter()
            .map(|provider| provider.authenticate(cx))
            .collect();
        self._review_order_wait_task = Some(cx.spawn_in(window, async move |this, cx| {
            for authentication in authentications {
                authentication.await.ok();
            }
            cx.update(|_, cx| {
                registry.update(cx, |registry, cx| registry.refresh_fallback_model(cx))
            })
            .ok();
            // The files arrive in batches, so a request sent at the first one
            // would order a fraction of the diff. Wait until loading has
            // finished and the file count has held still. An order cached
            // for this commit is used the moment the files settle, with no
            // wait for a model.
            let mut settled_count = None;
            let mut settled_polls = 0;
            for _ in 0..MAX_POLLS {
                let ready = this.update_in(cx, |this, window, cx| {
                    if this.files_settled(&mut settled_count, cx) {
                        settled_polls += 1;
                    } else {
                        settled_polls = 0;
                    }
                    if settled_polls < 3 {
                        return false;
                    }
                    if this.apply_cached_review_order(window, cx) {
                        return true;
                    }
                    this.merge_request_settled()
                        && LanguageModelRegistry::read_global(cx)
                            .commit_message_model(cx)
                            .is_some()
                });
                match ready {
                    Ok(true) => break,
                    Ok(false) => cx.background_executor().timer(POLL_INTERVAL).await,
                    Err(_) => return,
                }
            }
            if this
                .read_with(cx, |this, _| this.review_order.is_some())
                .unwrap_or(true)
            {
                return;
            }
            // If still not ready, this reports why through the usual error path.
            this.update_in(cx, |this, window, cx| {
                this.review_order_state = ReviewOrderState::Idle;
                this.suggest_review_order(window, cx)
            })
            .ok();
        }));
    }

    /// Asks a model for a review order and applies it, then asks for guiding
    /// notes with a second, larger request so the order lands first.
    pub(crate) fn suggest_review_order(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.review_order_state == ReviewOrderState::Suggesting
            || !AgentSettings::get_global(cx).enabled(cx)
        {
            return;
        }
        let registry = LanguageModelRegistry::read_global(cx);
        // The order only needs a taste of each file, so a fast model does.
        let Some(order_model) = registry
            .default_fast_model(cx)
            .or_else(|| registry.commit_message_model(cx))
        else {
            self.fail_review_order(
                "no language model is available. Add an API key in the agent panel's settings, \
                 or sign in, or set `agent.default_model` in your settings"
                    .into(),
                cx,
            );
            return;
        };
        let notes_model = registry
            .commit_message_model(cx)
            .unwrap_or_else(|| order_model.clone());
        let (order_provider, notes_provider) = match (
            registry.provider_for_model(&order_model),
            registry.provider_for_model(&notes_model),
        ) {
            (Ok(order_provider), Ok(notes_provider)) => (order_provider, notes_provider),
            (Err(error), _) | (_, Err(error)) => {
                self.fail_review_order(error.to_string().into(), cx);
                return;
            }
        };
        let files = self.review_order_files(cx);
        if files.is_empty() {
            return;
        }
        // The same commit against the same base is the same diff, so an
        // order written for it before is as good as a new one, and instant.
        if self.apply_cached_review_order(window, cx) {
            return;
        }
        let cache_key = self.review_cache_key(cx);
        let known_paths: Vec<RepoPath> = files.iter().map(|file| file.path.clone()).collect();
        let context = self.merge_request_context();
        // The author's own reading order, when the description gives one,
        // comes first whatever the model says.
        let pinned = self
            .merge_request
            .as_ref()
            .and_then(|header| header.description_text.as_deref())
            .map(|description| order_from_description(description, &known_paths))
            .unwrap_or_default();
        let order_request = text_request(
            build_order_prompt(&files, context.as_deref()),
            &order_model,
            cx,
        );
        let notes_request = text_request(
            build_notes_prompt(&files, context.as_deref()),
            &notes_model,
            cx,
        );

        self.pending_notes.take();
        self.review_order_state = ReviewOrderState::Suggesting;
        cx.notify();
        self._review_order_task = Some(cx.spawn_in(window, {
            let known_paths = known_paths.clone();
            async move |this, cx| {
                let result = complete_text(order_provider, order_model, order_request, cx)
                    .await
                    .and_then(|text| parse_review_order(&text, &known_paths))
                    .map(|order| order.with_pinned_first(&pinned));
                this.update_in(cx, |this, window, cx| match result {
                    Ok(order) => this.set_review_order(Some(Arc::new(order)), window, cx),
                    Err(error) => this.fail_review_order(format!("{error:#}").into(), cx),
                })
                .ok();
            }
        }));
        self._review_notes_task = Some(cx.spawn_in(window, async move |this, cx| {
            // Each file's notes go up as soon as the model has written them,
            // so the first files of the order are annotated while the rest
            // are still on their way.
            let mut parser = NotesStreamParser::default();
            let result = stream_text(
                notes_provider,
                notes_model,
                notes_request,
                cx,
                |chunk, cx| {
                    for (path, notes) in parser.push(chunk, &known_paths) {
                        this.update(cx, |this, cx| this.apply_file_notes(path, notes, cx))
                            .ok();
                    }
                },
            )
            .await;
            let result = result.and_then(|_| {
                if parser.files_seen() == 0 {
                    Err(anyhow::anyhow!("the reply names no file from this diff"))
                } else {
                    Ok(())
                }
            });
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    if let Some((key, order)) = cache_key.as_deref().zip(this.review_order.clone())
                    {
                        review_cache::store_order(key, &order).log_err();
                    }
                }
                Err(error) => {
                    log::warn!("review notes failed: {error:#}");
                    this.show_workspace_error(
                        format!("The review order has no notes: {error:#}"),
                        cx,
                    );
                }
            })
            .ok();
        }));
    }

    /// Puts one file's notes on the current order, or keeps them for the
    /// order that is still on its way.
    fn apply_file_notes(
        &mut self,
        path: RepoPath,
        notes: Vec<crate::review_order::ReviewNote>,
        cx: &mut Context<Self>,
    ) {
        match self.review_order.clone() {
            Some(order) => {
                self.review_order = Some(Arc::new(order.with_file_notes(&path, notes)));
                self.update_review_order_blocks(cx);
                cx.notify();
            }
            None if self.review_order_state == ReviewOrderState::Suggesting => {
                self.pending_notes
                    .get_or_insert_with(Default::default)
                    .insert(path, notes);
            }
            None => {}
        }
    }

    /// Sets the order cached for this commit, if there is one.
    fn apply_cached_review_order(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(order) = self
            .review_cache_key(cx)
            .and_then(|key| review_cache::load_order(&key).log_err().flatten())
        else {
            return false;
        };
        self.review_order_state = ReviewOrderState::Idle;
        self.set_review_order(Some(Arc::new(order)), window, cx);
        true
    }

    /// Where the order for this diff is cached: by repository, base, and
    /// commit. None when the tip is unknown.
    fn review_cache_key(&self, cx: &App) -> Option<String> {
        let DiffBase::Merge { base_ref } = self.diff_base(cx).clone() else {
            return None;
        };
        let work_dir = self.work_dir(cx)?;
        let head_sha = self.head_sha(cx)?;
        Some(review_cache::order_key(&work_dir, &base_ref, &head_sha))
    }

    fn fail_review_order(&mut self, message: SharedString, cx: &mut Context<Self>) {
        self._review_order_task.take();
        self.review_order_state = ReviewOrderState::Failed(message.clone());
        self.show_workspace_error(format!("Failed to suggest a review order: {message}"), cx);
        cx.notify();
    }

    /// Shows an error on the workspace after the current update, since the
    /// caller may itself be running inside a workspace update.
    fn show_workspace_error(&self, message: String, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| workspace.show_error(message, cx));
            }
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn review_order_block_count(&self) -> usize {
        self.review_order_blocks.len()
    }

    pub(crate) fn is_posting_review(&self) -> bool {
        self.posting_review
    }

    /// The comments written here that are not on the merge request yet, as
    /// opposed to posted ones and threads pulled in from the review.
    pub(crate) fn unposted_review_comment_count(&self, cx: &App) -> usize {
        self.editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .review_comments()
            .iter()
            .filter(|comment| comment.thread.is_none())
            .count()
    }

    /// Posts the stored review comments that are not on the merge request yet
    /// as inline discussions, keeps them marked as posted, and then pulls the
    /// threads back so replies show up.
    pub(crate) fn post_review_to_gitlab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.posting_review {
            return;
        }
        let Some(repo) = self.branch_diff.read(cx).repo().cloned() else {
            return;
        };
        let (work_dir, branch) = {
            let repo = repo.read(cx);
            (
                repo.work_directory_abs_path.to_path_buf(),
                repo.branch.as_ref().map(|branch| branch.name().to_string()),
            )
        };
        let Some(branch) = branch else {
            self.fail_post_review("the repository has no current branch".into(), cx);
            return;
        };
        let comments = self.gitlab_comments(cx);
        if comments.is_empty() {
            return;
        }
        let total = comments.len();

        self.posting_review = true;
        cx.notify();
        let outcome = cx.background_spawn(gitlab_review::post_review(work_dir, branch, comments));
        self._post_review_task = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = outcome.await;
            this.update_in(cx, |this, window, cx| {
                this.posting_review = false;
                match outcome {
                    Ok(outcome) => {
                        this.editor
                            .read(cx)
                            .rhs_editor()
                            .clone()
                            .update(cx, |editor, cx| {
                                for posted in &outcome.posted {
                                    editor.mark_review_comment_posted(
                                        posted.comment_id,
                                        posted.discussion_id.clone(),
                                        posted.note_id,
                                        window,
                                        cx,
                                    );
                                }
                            });
                        match outcome.error {
                            Some(error) => this.fail_post_review(
                                format!(
                                    "{} of {total} comments posted. {error:#}",
                                    outcome.posted.len()
                                )
                                .into(),
                                cx,
                            ),
                            None => this.show_posted_review_toast(
                                outcome.posted.len(),
                                outcome.merge_request.iid,
                                outcome.merge_request.web_url,
                                cx,
                            ),
                        }
                        this.sync_review_threads(window, cx);
                    }
                    Err(error) => this.fail_post_review(format!("{error:#}").into(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Pulls the merge request's threads into the editor: replies for the
    /// comments posted from here, and threads others opened as comments by
    /// their authors.
    pub(crate) fn sync_review_threads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.branch_diff.read(cx).repo().cloned() else {
            return;
        };
        let (work_dir, branch) = {
            let repo = repo.read(cx);
            (
                repo.work_directory_abs_path.to_path_buf(),
                repo.branch.as_ref().map(|branch| branch.name().to_string()),
            )
        };
        let Some(branch) = branch else {
            return;
        };
        let iid = self.merge_request.as_ref().map(|header| header.iid);
        let fetch = match self.prefetched_discussions.take() {
            Some(prefetched) => prefetched,
            None => cx.background_spawn(async move {
                let iid = match iid {
                    Some(iid) => iid,
                    None => {
                        gitlab_review::merge_request_for_branch(work_dir.clone(), branch)
                            .await?
                            .iid
                    }
                };
                gitlab_review::fetch_discussions(work_dir, iid).await
            }),
        };
        self._review_threads_task = Some(cx.spawn_in(window, async move |this, cx| {
            match fetch.await {
                Ok(discussions) => {
                    this.update_in(cx, |this, window, cx| {
                        this.apply_remote_discussions(discussions, window, cx)
                    })
                    .ok();
                }
                Err(error) => log::warn!("could not load the merge request threads: {error:#}"),
            }
            this.update_in(cx, |this, window, cx| {
                this.schedule_thread_refresh(window, cx)
            })
            .ok();
        }));
    }

    /// Runs the thread sync again after a pause, so replies and new threads
    /// show up while the diff stays open.
    fn schedule_thread_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

        self._thread_refresh_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(REFRESH_INTERVAL).await;
            this.update_in(cx, |this, window, cx| {
                if !this.posting_review {
                    this.sync_review_threads(window, cx);
                }
            })
            .ok();
        }));
    }

    /// Puts the comments held over a re-sort back under their lines. A
    /// comment whose lines the diff no longer shows is dropped.
    fn restore_held_review_comments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.held_review_comments.is_empty() {
            return;
        }
        let records = std::mem::take(&mut self.held_review_comments);
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let buffers_by_file: HashMap<Arc<RelPath>, language::BufferSnapshot> = self
            .buffer_subscriptions
            .values()
            .filter_map(|subscription| {
                let buffer = subscription.display_buffer.read(cx);
                Some((buffer.file()?.path().clone(), buffer.snapshot()))
            })
            .collect();
        self.editor
            .read(cx)
            .rhs_editor()
            .clone()
            .update(cx, |editor, cx| {
                for record in records {
                    let Some(buffer) = buffers_by_file.get(&record.file_path) else {
                        continue;
                    };
                    let Some(range) =
                        anchor_range_for_rows(&snapshot, buffer, record.start_row, record.end_row)
                    else {
                        log::warn!(
                            "dropping a review comment on {}:{} that the diff no longer shows",
                            record.file_path.as_unix_str(),
                            record.start_row + 1
                        );
                        continue;
                    };
                    editor.add_review_comment_at(
                        record.file_path,
                        range,
                        record.text,
                        record.author,
                        record.thread,
                        window,
                        cx,
                    );
                }
            });
    }

    fn apply_remote_discussions(
        &mut self,
        discussions: Vec<RemoteDiscussion>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.editor.read(cx).rhs_editor().clone();
        let known: HashSet<String> = editor
            .read(cx)
            .review_comments()
            .into_iter()
            .filter_map(|comment| comment.thread)
            .chain(
                self.held_review_comments
                    .iter()
                    .filter_map(|record| record.thread.clone()),
            )
            .map(|thread| thread.discussion_id)
            .collect();
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let buffers_by_path: HashMap<
            String,
            (
                Arc<RelPath>,
                language::BufferSnapshot,
                buffer_diff::BufferDiffSnapshot,
            ),
        > = self
            .buffer_subscriptions
            .iter()
            .filter_map(|(path, subscription)| {
                let buffer = subscription.display_buffer.read(cx);
                Some((
                    path.as_unix_str().to_string(),
                    (
                        buffer.file()?.path().clone(),
                        buffer.snapshot(),
                        subscription._diff.read(cx).snapshot(cx),
                    ),
                ))
            })
            .collect();
        let last_review_at = self
            .merge_request
            .as_ref()
            .and_then(|header| header.last_review.as_ref())
            .map(|review| review.at);

        editor.update(cx, |editor, cx| {
            for discussion in discussions {
                let Some(first) = discussion.notes.first() else {
                    continue;
                };
                let replies: Vec<ReviewReply> = discussion
                    .notes
                    .iter()
                    .skip(1)
                    .map(|note| ReviewReply {
                        author: note.author.clone().into(),
                        body: note.body.clone(),
                    })
                    .collect();
                let since_last_review = last_review_at.is_some_and(|at| {
                    discussion
                        .created_at
                        .as_deref()
                        .and_then(parse_timestamp)
                        .is_some_and(|created| created > at)
                });
                if known.contains(&discussion.id) {
                    editor.set_review_thread_state(
                        &discussion.id,
                        first.id,
                        replies,
                        discussion.resolved,
                        since_last_review,
                        window,
                        cx,
                    );
                    continue;
                }
                // A thread on lines the diff does not show is left out.
                let Some((file_path, buffer, diff)) = buffers_by_path.get(&discussion.path) else {
                    continue;
                };
                // A thread on removed lines sits where they were removed
                // from, and says which old line it was on.
                let (start_row, end_row, old_line) = if discussion.old_side {
                    let row = new_row_for_old_row(diff, buffer, discussion.line.saturating_sub(1));
                    (row, row, Some(discussion.line))
                } else {
                    (
                        discussion.start_line.saturating_sub(1),
                        discussion.line.saturating_sub(1),
                        None,
                    )
                };
                // A thread opened on lines the diff no longer shows, because
                // the branch moved on, sits at the nearest shown line of its
                // file and says where it was.
                let (range, original_line) =
                    match anchor_range_for_rows(&snapshot, buffer, start_row, end_row) {
                        Some(range) => (range, None),
                        None => {
                            let Some(row) = nearest_shown_row(&snapshot, buffer, end_row) else {
                                continue;
                            };
                            let Some(range) = anchor_range_for_rows(&snapshot, buffer, row, row)
                            else {
                                continue;
                            };
                            (range, Some(discussion.line))
                        }
                    };
                editor.add_review_comment_at(
                    file_path.clone(),
                    range,
                    first.body.clone(),
                    Some(first.author.clone().into()),
                    Some(ReviewThread {
                        discussion_id: discussion.id.clone(),
                        note_id: first.id,
                        replies,
                        resolved: discussion.resolved,
                        original_line,
                        old_line,
                        since_last_review,
                    }),
                    window,
                    cx,
                );
            }
        });
    }

    /// The stored review comments, each resolved to its file and the last
    /// line of its range in the new version of the file.
    fn gitlab_comments(&self, cx: &App) -> Vec<GitLabComment> {
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let paths_by_buffer: HashMap<BufferId, &RepoPath> = self
            .buffer_subscriptions
            .iter()
            .map(|(path, subscription)| (subscription.display_buffer.read(cx).remote_id(), path))
            .collect();
        self.editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .review_comments()
            .into_iter()
            .filter(|comment| comment.thread.is_none())
            .filter_map(|comment| {
                let start = comment.range.start.to_point(&snapshot);
                let mut end = comment.range.end.to_point(&snapshot);
                // A range that ends at the start of a line does not include that line.
                if end.row > start.row && end.column == 0 {
                    end.row -= 1;
                }
                let (buffer, end_point) = snapshot.point_to_buffer_point(end)?;
                let path = paths_by_buffer.get(&buffer.remote_id())?;
                let buffer_id = buffer.remote_id();
                // The range starts in the same file unless it straddles two
                // excerpts, in which case only its last line is used.
                let start_row = snapshot
                    .point_to_buffer_point(start)
                    .filter(|(start_buffer, _)| start_buffer.remote_id() == buffer_id)
                    .map_or(end_point.row, |(_, start_point)| start_point.row);
                Some(GitLabComment {
                    id: comment.id,
                    path: path.as_unix_str().to_string(),
                    start_line: start_row.min(end_point.row) + 1,
                    line: end_point.row + 1,
                    body: comment.text,
                })
            })
            .collect()
    }

    fn fail_post_review(&mut self, message: SharedString, cx: &mut Context<Self>) {
        self._post_review_task.take();
        self.posting_review = false;
        self.show_workspace_error(
            format!("Failed to post the review to GitLab: {message}"),
            cx,
        );
        cx.notify();
    }

    fn show_posted_review_toast(
        &self,
        posted: usize,
        merge_request_iid: u64,
        web_url: String,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            struct PostedReviewToast;
            let noun = if posted == 1 { "comment" } else { "comments" };
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<PostedReviewToast>(),
                    format!("Posted {posted} {noun} to !{merge_request_iid}"),
                )
                .on_click("Open", move |_, cx| cx.open_url(&web_url))
                .autohide(),
                cx,
            );
        });
    }

    /// Each loaded file with its status, line counts, and first changed lines,
    /// in path order so the prompt is stable between runs.
    fn review_order_files(&self, cx: &App) -> Vec<ReviewOrderFile> {
        let branch_diff = self.branch_diff.read(cx);
        let mut files: Vec<ReviewOrderFile> = self
            .buffer_subscriptions
            .iter()
            .map(|(repo_path, subscription)| {
                let buffer = subscription.display_buffer.read(cx).snapshot();
                let diff = subscription._diff.read(cx).snapshot(cx);
                let base_text = diff.base_text();
                let mut added_lines = 0;
                let mut removed_lines = 0;
                let mut excerpt = String::new();
                for hunk in diff.hunks(&buffer) {
                    let removed: String = base_text
                        .text_for_range(hunk.diff_base_byte_range.clone())
                        .collect();
                    let added: String = buffer.text_for_range(hunk.buffer_range.clone()).collect();
                    removed_lines += removed.lines().count();
                    added_lines += added.lines().count();
                    if excerpt.len() >= MAX_EXCERPT_BYTES_PER_FILE {
                        continue;
                    }
                    for line in removed.lines() {
                        writeln!(excerpt, "-     |{line}").ok();
                    }
                    // Added lines carry their number in the new file, so the
                    // model's notes can name a line the editor can show.
                    for (offset, line) in added.lines().enumerate() {
                        let number = hunk.range.start.row as usize + offset + 1;
                        writeln!(excerpt, "+{number:>5} |{line}").ok();
                    }
                }
                ReviewOrderFile {
                    path: repo_path.clone(),
                    status: branch_diff
                        .status_for_path(repo_path, cx)
                        .unwrap_or(FileStatus::Untracked),
                    added_lines,
                    removed_lines,
                    excerpt,
                }
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files
    }

    /// Goes to the file, and line, that a link in a note points at.
    fn open_review_reference(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, line)) = parse_review_file_url(url) else {
            return;
        };
        let Some(subscription) = self.buffer_subscriptions.get(&path) else {
            return;
        };
        let buffer = subscription.display_buffer.read(cx).snapshot();
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let row = line
            .map(|line| line.saturating_sub(1))
            .unwrap_or(0)
            .min(buffer.max_point().row);
        // A line the diff does not show is reached through the nearest one
        // it does.
        let row = nearest_shown_row(&snapshot, &buffer, row).unwrap_or(row);
        let Some(anchor) = snapshot.anchor_in_excerpt(buffer.anchor_before(Point::new(row, 0)))
        else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::center()),
                    window,
                    cx,
                    |selections| selections.select_ranges([anchor..anchor]),
                );
                let focus_handle = editor.focus_handle(cx);
                window.focus(&focus_handle, cx);
            })
        });
    }

    /// A note or reason as markdown that can be selected, copied, and whose
    /// file mentions can be clicked.
    fn review_note_markdown(&self, text: &str, cx: &mut Context<Self>) -> Entity<Markdown> {
        let known: Vec<RepoPath> = self.buffer_subscriptions.keys().cloned().collect();
        let source: SharedString = link_file_references(text, &known).into();
        cx.new(|cx| Markdown::new(source, None, None, cx))
    }

    /// Puts a band above the first excerpt of each file the review order
    /// lists, with the file's reason, and one above each line a guiding
    /// note describes.
    fn update_review_order_blocks(&mut self, cx: &mut Context<Self>) {
        let old_blocks: HashSet<CustomBlockId> = self.review_order_blocks.drain(..).collect();
        let mut bands = Vec::new();
        if let Some(order) = self.review_order.clone()
            && let Some(repo) = self.branch_diff.read(cx).repo().cloned()
        {
            let snapshot = self.multibuffer.read(cx).snapshot(cx);
            for entry in order.entries() {
                let status = self
                    .branch_diff
                    .read(cx)
                    .status_for_path(&entry.path, cx)
                    .unwrap_or(FileStatus::Untracked);
                let path_key =
                    project_diff_path_key(repo.read(cx), &entry.path, status, Some(&*order), cx);
                if let Some(anchor) = self.multibuffer.read(cx).location_for_path(&path_key, cx) {
                    let reason = self.review_note_markdown(&entry.reason, cx);
                    bands.push((anchor, BlockStyle::Sticky, reason));
                }
                let Some(subscription) = self.buffer_subscriptions.get(&entry.path) else {
                    continue;
                };
                let buffer = subscription.display_buffer.read(cx).snapshot();
                let mut notes: Vec<_> = entry.notes.iter().collect();
                notes.sort_by_key(|note| note.line);
                for note in notes {
                    // A note past the end of the file lands on its last line.
                    let row = note.line.saturating_sub(1).min(buffer.max_point().row);
                    let text_anchor = buffer.anchor_before(Point::new(row, 0));
                    let Some(anchor) = snapshot.anchor_in_excerpt(text_anchor) else {
                        continue;
                    };
                    let markdown = self.review_note_markdown(&note.text, cx);
                    bands.push((anchor, BlockStyle::Flex, markdown));
                }
            }
        }
        // Two bands with no code between them read as one long note, so a
        // note that would sit right under another band is left out. A
        // file's band wins over a note at the end of the file before it.
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let mut kept: Vec<(multi_buffer::Anchor, BlockStyle, Entity<Markdown>)> = Vec::new();
        for band in bands {
            let row = band.0.to_point(&snapshot).row;
            let adjacent = kept
                .last()
                .is_some_and(|last| row <= last.0.to_point(&snapshot).row + 1);
            if adjacent {
                if band.1 == BlockStyle::Sticky
                    && kept.last().is_some_and(|last| last.1 != BlockStyle::Sticky)
                {
                    kept.pop();
                } else {
                    continue;
                }
            }
            kept.push(band);
        }
        let this = cx.weak_entity();
        let blocks: Vec<BlockProperties<multi_buffer::Anchor>> = kept
            .into_iter()
            .map(|(anchor, style, markdown)| {
                let this = this.clone();
                BlockProperties {
                    placement: BlockPlacement::Above(anchor),
                    height: Some(1),
                    style,
                    render: Arc::new(move |cx| {
                        // A tinted band with an accent edge, so it reads
                        // apart from the code around it.
                        let colors = cx.theme().colors();
                        h_flex()
                            .id(cx.block_id)
                            .h(cx.line_height)
                            .w_full()
                            .pl(cx.margins.gutter.width)
                            .pr_2()
                            .gap_2()
                            .overflow_hidden()
                            .border_l_2()
                            .border_color(colors.text_accent)
                            .bg(colors.text_accent.opacity(0.12))
                            .child(
                                Icon::new(IconName::Sparkle)
                                    .size(IconSize::Small)
                                    .color(Color::Accent),
                            )
                            .child(render_review_note(markdown.clone(), this.clone(), cx))
                            .into_any_element()
                    }),
                    priority: 0,
                }
            })
            .collect();
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                if !old_blocks.is_empty() {
                    editor.remove_blocks(old_blocks, None, cx);
                }
                if !blocks.is_empty() {
                    self.review_order_blocks = editor.insert_blocks(blocks, None, cx);
                }
            })
        });
    }

    /// The review comments in the buffer, top to bottom, with what an
    /// overview needs to show and jump to each.
    pub(crate) fn review_comment_entries(&self, cx: &App) -> Vec<ReviewCommentEntry> {
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let mut entries: Vec<(u32, ReviewCommentEntry)> = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .review_comments()
            .into_iter()
            .map(|comment| {
                let start = comment.range.start.to_point(&snapshot);
                let line = snapshot
                    .point_to_buffer_point(start)
                    .map_or(start.row, |(_, point)| point.row)
                    + 1;
                let first_line = comment.text.lines().next().unwrap_or_default().trim();
                let snippet: String = first_line.chars().take(70).collect();
                let snippet = if first_line.chars().count() > 70 {
                    format!("{snippet}…")
                } else {
                    snippet
                };
                (
                    start.row,
                    ReviewCommentEntry {
                        id: comment.id,
                        author: comment.author.clone().unwrap_or_else(|| "You".into()),
                        path: comment.file_path.as_unix_str().to_string(),
                        line,
                        snippet,
                        resolved: comment
                            .thread
                            .as_ref()
                            .is_some_and(|thread| thread.resolved),
                        posted: comment.thread.is_some(),
                        replies: comment
                            .thread
                            .as_ref()
                            .map_or(0, |thread| thread.replies.len()),
                        anchor: comment.range.start,
                    },
                )
            })
            .collect();
        entries.sort_by_key(|(row, entry)| (*row, entry.id));
        entries.into_iter().map(|(_, entry)| entry).collect()
    }

    /// Which comment the cursor is at or below, one-based, with how many
    /// there are; the position is None while the cursor is above the first.
    pub(crate) fn review_comment_position(&self, cx: &App) -> Option<(Option<usize>, usize)> {
        let entries = self.review_comment_entries(cx);
        if entries.is_empty() {
            return None;
        }
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let cursor = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .selections
            .newest_anchor()
            .head()
            .to_point(&snapshot)
            .row;
        let at_or_above = entries
            .iter()
            .filter(|entry| entry.anchor.to_point(&snapshot).row <= cursor)
            .count();
        Some(((at_or_above > 0).then_some(at_or_above), entries.len()))
    }

    /// Moves the cursor to the next comment, or the previous one, wrapping
    /// round at either end.
    pub(crate) fn go_to_review_comment(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries = self.review_comment_entries(cx);
        let Some((position, total)) = self.review_comment_position(cx) else {
            return;
        };
        let index = match (position, forward) {
            (None, true) => 0,
            (None, false) => total - 1,
            (Some(at), true) => at % total,
            (Some(at), false) => (at + total - 2) % total,
        };
        let Some(entry) = entries.get(index) else {
            return;
        };
        self.jump_to_review_comment(entry.anchor, window, cx);
    }

    /// Moves the cursor to the comment with `id`.
    pub(crate) fn go_to_review_comment_id(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .review_comment_entries(cx)
            .into_iter()
            .find(|entry| entry.id == id)
        else {
            return;
        };
        self.jump_to_review_comment(entry.anchor, window, cx);
    }

    fn jump_to_review_comment(
        &mut self,
        anchor: multi_buffer::Anchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::center()),
                    window,
                    cx,
                    |selections| selections.select_ranges([anchor..anchor]),
                );
                let focus_handle = editor.focus_handle(cx);
                window.focus(&focus_handle, cx);
            })
        });
        cx.notify();
    }

    /// Returns a reference to the splittable editor.
    pub(crate) fn editor(&self) -> &Entity<SplittableEditor> {
        &self.editor
    }

    pub(crate) fn selected_ranges(
        &self,
        cx: &App,
    ) -> (bool, Vec<std::ops::Range<multi_buffer::Anchor>>) {
        let editor = self.editor.read(cx).rhs_editor().read(cx);
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let mut selection = true;
        let mut ranges = editor
            .selections
            .disjoint_anchor_ranges()
            .collect::<Vec<_>>();
        if !ranges.iter().any(|range| range.start != range.end) {
            selection = false;
            let anchor = editor.selections.newest_anchor().head();
            if let Some((_, excerpt_range)) = snapshot.excerpt_containing(anchor..anchor)
                && let Some(range) = snapshot
                    .anchor_in_buffer(excerpt_range.context.start)
                    .zip(snapshot.anchor_in_buffer(excerpt_range.context.end))
                    .map(|(start, end)| start..end)
            {
                ranges = vec![range];
            } else {
                ranges = Vec::default();
            };
        }

        (selection, ranges)
    }

    /// Ranges for a toolbar stage/unstage action: the selection, or the cursor
    /// (a zero-width range that resolves to the single hunk under it) when
    /// there is no selection. Unlike [`Self::selected_ranges`], this never
    /// widens to the whole excerpt, so actions affect one hunk at a time.
    fn hunk_action_ranges(&self, cx: &App) -> Vec<std::ops::Range<multi_buffer::Anchor>> {
        self.editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .selections
            .disjoint_anchor_ranges()
            .collect()
    }

    pub(crate) fn stage_or_unstage_selected_hunks(
        &mut self,
        stage: bool,
        move_to_next: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.editor.read(cx).rhs_editor().clone();
        let ranges = self.hunk_action_ranges(cx);
        // Route through the editor's delegated stage path, the same path taken
        // by the hunk buttons (on either side of a split) and the keyboard.
        // For staging, dirty buffers are saved first, exactly as they are when
        // staging from the uncommitted diff or a normal editor.
        editor.update(cx, |editor, cx| {
            editor.stage_or_unstage_diff_hunks(stage, ranges, window, cx);
        });
        if move_to_next {
            editor
                .focus_handle(cx)
                .dispatch_action(&GoToHunk, window, cx);
        }
    }

    pub(crate) fn restore_selected_hunks(
        &mut self,
        move_to_next: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.editor.read(cx).rhs_editor().clone();
        let ranges = self.hunk_action_ranges(cx);
        editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let hunks: Vec<_> = editor.diff_hunks_in_ranges(&ranges, &snapshot).collect();
            if !hunks.is_empty() {
                editor.apply_restore(hunks, window, cx);
            }
        });
        if move_to_next {
            editor
                .focus_handle(cx)
                .dispatch_action(&GoToHunk, window, cx);
        }
    }

    fn handle_editor_event(
        &mut self,
        editor: &Entity<SplittableEditor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::SelectionsChanged { local: true } => {
                // Only follow the git panel selection from the view the user is
                // actually interacting with. Background (non-active) diff views
                // refresh on their own and must not hijack the panel selection.
                if !editor.focus_handle(cx).contains_focused(window, cx) {
                    return;
                }
                cx.emit(event.clone());
                let Some(project_path) = self.active_project_path(cx) else {
                    return;
                };
                self.workspace
                    .update(cx, |workspace, cx| {
                        if let Some(git_panel) = workspace.panel::<GitPanel>(cx) {
                            git_panel.update(cx, |git_panel, cx| {
                                git_panel.select_entry_by_path(project_path, window, cx)
                            })
                        }
                    })
                    .ok();
            }
            EditorEvent::Saved => {
                self._task =
                    cx.spawn_in(window, async move |this, cx| Self::refresh(this, cx).await);
            }

            _ => {}
        }
        if editor.focus_handle(cx).contains_focused(window, cx)
            && self.multibuffer.read(cx).is_empty()
        {
            self.focus_handle.focus(window, cx)
        }
    }

    #[instrument(skip_all)]
    fn register_buffer(
        &mut self,
        repo_path: RepoPath,
        path_key: PathKey,
        file_status: FileStatus,
        display_buffer: Entity<Buffer>,
        main_buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        conflict_set: Option<Entity<ConflictSet>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<BufferId> {
        let diff_subscription = cx.subscribe_in(&diff, window, {
            let repo_path = repo_path.clone();
            let path_key = path_key.clone();
            let display_buffer = display_buffer.clone();
            let main_buffer = main_buffer.clone();
            let diff = diff.clone();
            let conflict_set = conflict_set.clone();
            move |this, _, event, window, cx| match event {
                buffer_diff::BufferDiffEvent::DiffChanged(_) => {
                    this.buffer_ranges_changed(
                        repo_path.clone(),
                        path_key.clone(),
                        file_status,
                        display_buffer.clone(),
                        main_buffer.clone(),
                        diff.clone(),
                        conflict_set.clone(),
                        window,
                        cx,
                    );
                }
                buffer_diff::BufferDiffEvent::BaseTextChanged => {}
            }
        });
        let conflict_set_subscription = conflict_set.as_ref().map(|conflict_set| {
            cx.subscribe_in(conflict_set, window, {
                let repo_path = repo_path.clone();
                let path_key = path_key.clone();
                let display_buffer = display_buffer.clone();
                let main_buffer = main_buffer.clone();
                let diff = diff.clone();
                let conflict_set = Some(conflict_set.clone());
                move |this, _, _, window, cx| {
                    this.buffer_ranges_changed(
                        repo_path.clone(),
                        path_key.clone(),
                        file_status,
                        display_buffer.clone(),
                        main_buffer.clone(),
                        diff.clone(),
                        conflict_set.clone(),
                        window,
                        cx,
                    )
                }
            })
        });
        self.buffer_subscriptions.insert(
            repo_path,
            BufferSubscriptions {
                _diff: diff.clone(),
                display_buffer: display_buffer.clone(),
                _diff_subscription: diff_subscription,
                _conflict_set: conflict_set.clone(),
                _conflict_set_subscription: conflict_set_subscription,
            },
        );

        let snapshot = display_buffer.read(cx).snapshot();
        let diff_snapshot = diff.read(cx).snapshot(cx);

        let excerpt_ranges = {
            let diff_hunk_ranges = diff_snapshot
                .hunks_intersecting_range(
                    Anchor::min_max_range_for_buffer(snapshot.remote_id()),
                    &snapshot,
                )
                .map(|diff_hunk| diff_hunk.buffer_range.to_point(&snapshot));
            let conflict_ranges = conflict_set.as_ref().and_then(|conflict_set| {
                let conflicts = conflict_set.read(cx).snapshot();
                let conflicts = conflicts
                    .conflicts
                    .iter()
                    .map(|conflict| conflict.range.to_point(&snapshot))
                    .collect::<Vec<_>>();
                (!conflicts.is_empty()).then_some(conflicts)
            });

            conflict_ranges.unwrap_or_else(|| diff_hunk_ranges.collect())
        };

        let buffer_id = snapshot.text.remote_id();
        let mut needs_fold = false;

        let (was_empty, is_excerpt_newly_added) = self.editor.update(cx, |editor, cx| {
            let was_empty = editor.rhs_editor().read(cx).buffer().read(cx).is_empty();
            let is_newly_added = editor.update_excerpts_for_path(
                path_key.clone(),
                display_buffer,
                excerpt_ranges,
                multibuffer_context_lines(cx),
                diff,
                cx,
            );
            if let Some(conflict_set) = conflict_set {
                editor.rhs_editor().update(cx, |editor, cx| {
                    conflict_view::buffer_ranges_updated(editor, conflict_set, cx);
                });
            }
            (was_empty, is_newly_added)
        });

        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                if was_empty {
                    editor.change_selections(
                        SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| {
                            selections.select_ranges([
                                multi_buffer::Anchor::Min..multi_buffer::Anchor::Min
                            ])
                        },
                    );
                }
                if is_excerpt_newly_added
                    && (file_status.is_deleted()
                        || (file_status.is_untracked()
                            && GitPanelSettings::get_global(cx).collapse_untracked_diff))
                {
                    needs_fold = true;
                }
            })
        });

        if self.multibuffer.read(cx).is_empty()
            && self
                .editor
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        {
            self.focus_handle.focus(window, cx);
        } else if self.focus_handle.is_focused(window) && !self.multibuffer.read(cx).is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window, cx);
            });
        }
        if self.pending_scroll.as_ref() == Some(&path_key) {
            self.move_to_path(path_key, window, cx);
        }

        needs_fold.then_some(buffer_id)
    }

    fn buffer_ranges_changed(
        &mut self,
        repo_path: RepoPath,
        path_key: PathKey,
        file_status: FileStatus,
        display_buffer: Entity<Buffer>,
        main_buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        conflict_set: Option<Entity<ConflictSet>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if display_buffer.read(cx).is_dirty() {
            return;
        }
        self.register_buffer(
            repo_path,
            path_key,
            file_status,
            display_buffer,
            main_buffer,
            diff,
            conflict_set,
            window,
            cx,
        );
    }

    #[instrument(skip(this, cx))]
    pub(crate) async fn refresh(this: WeakEntity<Self>, cx: &mut AsyncWindowContext) -> Result<()> {
        let entries = this.update(cx, |this, cx| {
            let (repo, buffers_to_load) = this.branch_diff.update(cx, |branch_diff, cx| {
                let load_buffers = branch_diff.load_buffers(cx);
                (branch_diff.repo().cloned(), load_buffers)
            });
            let mut previous_paths = this
                .multibuffer
                .read(cx)
                .snapshot(cx)
                .buffers_with_paths()
                .map(|(buffer_snapshot, path_key)| (path_key.clone(), buffer_snapshot.remote_id()))
                .collect::<HashMap<_, _>>();

            let mut entries = BTreeMap::new();
            let mut live_repo_paths = HashSet::default();
            let review_order = this.review_order.clone();
            if let Some(repo) = repo {
                let repo = repo.read(cx);
                for diff_buffer in buffers_to_load {
                    live_repo_paths.insert(diff_buffer.repo_path.clone());
                    let path_key = project_diff_path_key(
                        &repo,
                        &diff_buffer.repo_path,
                        diff_buffer.file_status,
                        review_order.as_deref(),
                        cx,
                    );
                    previous_paths.remove(&path_key);
                    entries.insert(path_key, diff_buffer);
                }
            }

            let repo_path_by_display_id = this
                .buffer_subscriptions
                .iter()
                .map(|(repo_path, sub)| {
                    (sub.display_buffer.read(cx).remote_id(), repo_path.clone())
                })
                .collect::<HashMap<_, _>>();

            this.editor.update(cx, |editor, cx| {
                for (path, buffer_id) in previous_paths {
                    if let Some(repo_path) = repo_path_by_display_id.get(&buffer_id) {
                        this.buffer_subscriptions.remove(repo_path);
                    }
                    editor.rhs_editor().update(cx, |editor, cx| {
                        conflict_view::buffers_removed(editor, &[buffer_id], cx);
                    });
                    let _span = ztracing::info_span!("remove_excerpts_for_path");
                    _span.enter();
                    editor.remove_excerpts_for_path(path, cx);
                }
            });

            this.buffer_subscriptions
                .retain(|repo_path, _| live_repo_paths.contains(repo_path));

            entries
        })?;

        let mut buffers_to_fold = Vec::new();

        // Ordered, so excerpts don't shift under the reader as later files arrive.
        let mut loads = stream::iter(entries.into_iter().map(|(path_key, entry)| {
            let diff_buffer_list::DiffBuffer {
                repo_path,
                file_status,
                load,
            } = entry;
            load.map(move |loaded| (path_key, repo_path, file_status, loaded))
        }))
        .buffered(MAX_CONCURRENT_BUFFER_LOADS);

        while let Some((path_key, repo_path, file_status, loaded)) = loads.next().await {
            // Buffered loads can already be complete when we poll them, in which
            // case this loop never awaits and starves the foreground thread.
            yield_now().await;
            if let Some(loaded_buffer) = loaded.log_err() {
                cx.update(|window, cx| {
                    this.update(cx, |this, cx| {
                        if let Some(buffer_id) = this.register_buffer(
                            repo_path,
                            path_key,
                            file_status,
                            loaded_buffer.display_buffer,
                            loaded_buffer.main_buffer,
                            loaded_buffer.diff,
                            loaded_buffer.conflict_set,
                            window,
                            cx,
                        ) {
                            buffers_to_fold.push(buffer_id);
                        }
                    })
                    .ok();
                })?;
            }
        }
        this.update_in(cx, |this, window, cx| {
            if !buffers_to_fold.is_empty() {
                this.editor.update(cx, |editor, cx| {
                    editor
                        .rhs_editor()
                        .update(cx, |editor, cx| editor.fold_buffers(buffers_to_fold, cx));
                });
            }
            this.pending_scroll.take();
            this.update_review_order_blocks(cx);
            this.restore_held_review_comments(window, cx);
            cx.notify();
        })?;

        Ok(())
    }

    pub(crate) fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        let editor = self.editor.read(cx).focused_editor().read(cx);
        let focused_multibuffer = editor.buffer().read(cx);
        let position = editor.selections.newest_anchor().head();
        let focused_snapshot = focused_multibuffer.snapshot(cx);
        let (focused_anchor, _) = focused_snapshot.anchor_to_buffer_anchor(position)?;
        let path = focused_snapshot.path_for_buffer(focused_anchor.buffer_id)?;

        let multibuffer = self.multibuffer.read(cx);
        let position = multibuffer.location_for_path(path, cx)?;
        let snapshot = multibuffer.snapshot(cx);
        let (text_anchor, _) = snapshot.anchor_to_buffer_anchor(position)?;
        let buffer = multibuffer.buffer(text_anchor.buffer_id)?;
        let file = buffer.read(cx).file()?;
        Some(ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        })
    }

    pub(crate) fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    pub(crate) fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |primary_editor, cx| {
                primary_editor.deactivated(window, cx);
            })
        });
    }

    pub(crate) fn navigate(
        &mut self,
        data: Arc<dyn std::any::Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |primary_editor, cx| {
                primary_editor.navigate(data, window, cx)
            })
        })
    }

    pub(crate) fn set_nav_history(&mut self, nav_history: ItemNavHistory, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |primary_editor, _| {
                primary_editor.set_nav_history(Some(nav_history));
            })
        });
    }

    pub(crate) fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .for_each_project_item(cx, f)
    }

    pub(crate) fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |primary_editor, cx| {
                primary_editor.save(options, project, window, cx)
            })
        })
    }

    pub(crate) fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |primary_editor, cx| {
                primary_editor.reload(project, window, cx)
            })
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn excerpt_paths(&self, cx: &App) -> Vec<std::sync::Arc<util::rel_path::RelPath>> {
        let snapshot = self
            .editor()
            .read(cx)
            .rhs_editor()
            .read(cx)
            .buffer()
            .read(cx)
            .snapshot(cx);
        snapshot
            .excerpts()
            .map(|excerpt| {
                snapshot
                    .path_for_buffer(excerpt.context.start.buffer_id)
                    .unwrap()
                    .path
                    .clone()
            })
            .collect()
    }

    /// Returns the real (worktree-relative) path of each excerpted buffer, in
    /// the order the excerpts appear in the multibuffer. Unlike
    /// [`Self::excerpt_paths`], this resolves the buffer's actual `File` rather
    /// than the (possibly synthetic) `PathKey` path used for sorting.
    #[cfg(any(test, feature = "test-support"))]
    pub fn excerpt_file_paths(&self, cx: &App) -> Vec<String> {
        let multibuffer = self
            .editor()
            .read(cx)
            .rhs_editor()
            .read(cx)
            .buffer()
            .clone();
        let snapshot = multibuffer.read(cx).snapshot(cx);
        let mut result = Vec::new();
        let mut last_buffer_id = None;
        for excerpt in snapshot.excerpts() {
            let buffer_id = excerpt.context.start.buffer_id;
            if last_buffer_id == Some(buffer_id) {
                continue;
            }
            last_buffer_id = Some(buffer_id);
            if let Some(buffer) = multibuffer.read(cx).buffer(buffer_id)
                && let Some(file) = buffer.read(cx).file()
            {
                result.push(file.path().as_unix_str().to_string());
            }
        }
        result
    }
}

impl EventEmitter<EditorEvent> for DiffMultibuffer {}

impl Focusable for DiffMultibuffer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.multibuffer.read(cx).is_empty() {
            self.focus_handle.clone()
        } else {
            self.editor.focus_handle(cx)
        }
    }
}

impl Render for DiffMultibuffer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_empty = self.multibuffer.read(cx).is_empty();
        let is_loading = self.is_loading(cx);
        let empty_label = self.empty_label.clone();
        let merge_request_header = (!is_empty)
            .then(|| self.render_merge_request_header(window, cx))
            .flatten();

        div()
            .track_focus(&self.focus_handle)
            .key_context(if is_empty { "EmptyPane" } else { "GitDiff" })
            .bg(cx.theme().colors().editor_background)
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DraggedDescriptionEdge>, window, cx| {
                    this.drag_description_edge(event, window, cx)
                },
            ))
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .when(is_empty && is_loading, |el| {
                let rems = TextSize::Large.rems(cx);
                el.child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::Custom(rems))
                        .color(Color::Accent)
                        .with_rotate_animation(3)
                        .into_any_element(),
                )
            })
            .when(is_empty && !is_loading, |el| {
                let remote_button = if let Some(panel) = self
                    .workspace
                    .upgrade()
                    .and_then(|workspace| workspace.read(cx).panel::<GitPanel>(cx))
                {
                    panel.update(cx, |panel, cx| panel.render_remote_button(cx))
                } else {
                    None
                };
                let keybinding_focus_handle = self.focus_handle(cx);
                el.child(
                    v_flex()
                        .gap_1()
                        .child(h_flex().justify_around().child(Label::new(empty_label)))
                        .map(|el| match remote_button {
                            Some(button) => el.child(h_flex().justify_around().child(button)),
                            None => el.child(
                                h_flex()
                                    .justify_around()
                                    .child(Label::new("Remote up to date")),
                            ),
                        })
                        .child(
                            h_flex().justify_around().mt_1().child(
                                Button::new("project-diff-close-button", "Close")
                                    .key_binding(KeyBinding::for_action_in(
                                        &CloseActiveItem::default(),
                                        &keybinding_focus_handle,
                                        cx,
                                    ))
                                    .on_click(move |_, window, cx| {
                                        window.focus(&keybinding_focus_handle, cx);
                                        window.dispatch_action(
                                            Box::new(CloseActiveItem::default()),
                                            cx,
                                        );
                                    }),
                            ),
                        ),
                )
            })
            .when(!is_empty, |el| {
                el.child(
                    v_flex()
                        .size_full()
                        .children(merge_request_header)
                        .child(div().flex_1().min_h_0().w_full().child(self.editor.clone())),
                )
            })
    }
}

/// Multibuffer anchors from the start of `start_row` to the end of `end_row`
/// in `buffer`, if the diff shows those lines.
fn anchor_range_for_rows(
    snapshot: &multi_buffer::MultiBufferSnapshot,
    buffer: &language::BufferSnapshot,
    start_row: u32,
    end_row: u32,
) -> Option<std::ops::Range<multi_buffer::Anchor>> {
    let max_row = buffer.max_point().row;
    let start_row = start_row.min(max_row);
    let end_row = end_row.min(max_row).max(start_row);
    let start = snapshot.anchor_in_excerpt(buffer.anchor_before(Point::new(start_row, 0)))?;
    let end = snapshot
        .anchor_in_excerpt(buffer.anchor_before(Point::new(end_row, buffer.line_len(end_row))))?;
    Some(start..end)
}

/// The row of the new version of the file that the old row `old_row` maps
/// to: the same line when the change kept it, or the first row of the hunk
/// that removed it.
fn new_row_for_old_row(
    diff: &buffer_diff::BufferDiffSnapshot,
    buffer: &language::BufferSnapshot,
    old_row: u32,
) -> u32 {
    let base_text = diff.base_text();
    let mut delta: i64 = 0;
    for hunk in diff.hunks(buffer) {
        let old_start = base_text
            .offset_to_point(hunk.diff_base_byte_range.start)
            .row;
        let old_end = base_text.offset_to_point(hunk.diff_base_byte_range.end).row;
        let new_start = hunk.range.start.row;
        let new_end = hunk.range.end.row;
        if old_row < old_start {
            break;
        }
        if old_row < old_end {
            return new_start.min(buffer.max_point().row);
        }
        delta = i64::from(new_end) - i64::from(old_end);
    }
    (i64::from(old_row) + delta)
        .clamp(0, i64::from(buffer.max_point().row))
        .try_into()
        .unwrap_or(0)
}

/// A note's text as a markdown element: selectable, with file mentions as
/// links that move the diff to the file.
fn render_review_note(
    markdown: Entity<Markdown>,
    diff: WeakEntity<DiffMultibuffer>,
    cx: &mut editor::display_map::BlockContext,
) -> AnyElement {
    let mut style = editor::hover_markdown_style(cx.window, cx.app);
    style.link.color = Some(cx.theme().colors().text_accent);
    style.link.underline = Some(gpui::UnderlineStyle {
        thickness: px(1.),
        color: Some(cx.theme().colors().text_accent),
        wavy: false,
    });
    div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .child(
            MarkdownElement::new(markdown, style).on_url_click(move |url, window, cx| {
                diff.update(cx, |diff, cx| diff.open_review_reference(&url, window, cx))
                    .ok();
            }),
        )
        .into_any_element()
}

/// Seconds since the Unix epoch for one of GitLab's ISO 8601 timestamps.
fn parse_timestamp(text: &str) -> Option<i64> {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|time| time.unix_timestamp())
}

/// The shown row of `buffer` closest to `row`, when the diff shows any of it.
fn nearest_shown_row(
    snapshot: &multi_buffer::MultiBufferSnapshot,
    buffer: &language::BufferSnapshot,
    row: u32,
) -> Option<u32> {
    snapshot
        .excerpts_for_buffer(buffer.remote_id())
        .map(|excerpt| {
            let start = excerpt.context.start.to_point(buffer).row;
            let end = excerpt.context.end.to_point(buffer).row;
            row.clamp(start, end)
        })
        .min_by_key(|candidate| candidate.abs_diff(row))
}

/// A single user message asking the model for `prompt`.
fn text_request(prompt: String, model: &LanguageModel, cx: &App) -> LanguageModelRequest {
    LanguageModelRequest {
        thread_id: None,
        prompt_cache_key: None,
        prompt_id: None,
        intent: Some(CompletionIntent::UserPrompt),
        messages: vec![LanguageModelRequestMessage {
            role: Role::User,
            content: vec![prompt.into()],
            cache: false,
            reasoning_details: None,
        }],
        tools: Vec::new(),
        tool_choice: None,
        stop: Vec::new(),
        temperature: AgentSettings::temperature_for_model(model, cx),
        thinking_allowed: false,
        thinking_effort: None,
        speed: None,
        compact_at_tokens: None,
        max_output_tokens: None,
    }
}

/// Sends `request` to `model` and hands each chunk of the reply to
/// `on_chunk` as it arrives.
async fn stream_text(
    provider: Arc<dyn LanguageModelProvider>,
    model: LanguageModel,
    request: LanguageModelRequest,
    cx: &mut AsyncWindowContext,
    mut on_chunk: impl FnMut(&str, &mut AsyncWindowContext),
) -> Result<()> {
    if let Some(task) =
        cx.update(|_, cx| (!provider.is_authenticated(cx)).then(|| provider.authenticate(cx)))?
    {
        task.await.log_err();
    }
    let mut stream = provider
        .stream_completion_text(&model, request, cx)
        .await
        .context("the model request failed")?;
    while let Some(chunk) = stream.stream.next().await {
        let chunk = chunk.context("the model reply stopped early")?;
        on_chunk(&chunk, cx);
    }
    Ok(())
}

/// Sends `request` to `model` and collects the whole reply as text.
async fn complete_text(
    provider: Arc<dyn LanguageModelProvider>,
    model: LanguageModel,
    request: LanguageModelRequest,
    cx: &mut AsyncWindowContext,
) -> Result<String> {
    if let Some(task) =
        cx.update(|_, cx| (!provider.is_authenticated(cx)).then(|| provider.authenticate(cx)))?
    {
        task.await.log_err();
    }
    let mut stream = provider
        .stream_completion_text(&model, request, cx)
        .await
        .context("the model request failed")?;
    let mut text = String::new();
    while let Some(chunk) = stream.stream.next().await {
        text.push_str(&chunk.context("the model reply stopped early")?);
    }
    Ok(text)
}

const CONFLICT_SORT_PREFIX: u64 = 1;
const TRACKED_SORT_PREFIX: u64 = 2;
const NEW_SORT_PREFIX: u64 = 3;
/// A file the review order lists sorts at this prefix plus its place in the order.
const REVIEW_ORDER_SORT_PREFIX: u64 = 1 << 32;
/// A file the review order leaves out sorts after every listed file.
const REVIEW_ORDER_UNLISTED_SORT_PREFIX: u64 = 1 << 33;

/// Computes a stable [`PathKey`] for a buffer in the project diff.
///
/// The key is an intrinsic function of the file's own repo path and status; it
/// never depends on which other buffers happen to be present in the
/// multibuffer. This is required because the multibuffer uses the path key both
/// to order excerpts and to identify which excerpts belong to a given buffer, so
/// a key that shifted as files were added or removed would break that identity.
///
/// Status grouping is encoded in the `sort_prefix`, and the within-group order
/// is encoded in the (possibly synthetic) path so that `PathKey`'s natural
/// ordering reproduces the git panel's order. The path here is only ever used
/// for sorting and multibuffer identity; the path shown in the UI comes from the
/// buffer's own `File`.
///
/// When a [`ReviewOrder`] is set, a file's place in that order replaces the
/// status grouping, and files the order leaves out keep the git panel's order
/// after the listed ones. The order is fixed for the life of the view, so the
/// key stays intrinsic to the file; changing the order rebuilds every excerpt.
pub(crate) fn project_diff_path_key(
    repo: &Repository,
    repo_path: &RepoPath,
    status: FileStatus,
    review_order: Option<&ReviewOrder>,
    cx: &App,
) -> PathKey {
    let settings = GitPanelSettings::get_global(cx);
    if let Some(order) = review_order {
        return match order.position(repo_path) {
            Some(position) => PathKey::with_sort_prefix(
                REVIEW_ORDER_SORT_PREFIX + position as u64,
                repo_path.into_arc(),
            ),
            None => PathKey::with_sort_prefix(
                REVIEW_ORDER_UNLISTED_SORT_PREFIX,
                project_diff_sort_path(repo_path, settings.tree_view, settings.sort_by),
            ),
        };
    }
    let sort_prefix = if settings.group_by != GitPanelGroupBy::Status {
        TRACKED_SORT_PREFIX
    } else if repo.had_conflict_on_last_merge_head_change(repo_path) {
        CONFLICT_SORT_PREFIX
    } else if status.is_created() {
        NEW_SORT_PREFIX
    } else {
        TRACKED_SORT_PREFIX
    };
    let path = project_diff_sort_path(repo_path, settings.tree_view, settings.sort_by);
    PathKey::with_sort_prefix(sort_prefix, path)
}

fn project_diff_sort_path(
    repo_path: &RelPath,
    tree_view: bool,
    sort_by: GitPanelSortBy,
) -> Arc<RelPath> {
    if tree_view {
        tree_sort_path(repo_path)
    } else {
        match sort_by {
            GitPanelSortBy::Path => repo_path.into_arc(),
            GitPanelSortBy::Name => name_sort_path(repo_path),
        }
    }
}

/// Builds a synthetic path that sorts by file name first, falling back to the
/// full path to keep the key unique per file.
fn name_sort_path(repo_path: &RelPath) -> Arc<RelPath> {
    let Some(file_name) = repo_path.file_name() else {
        return repo_path.into_arc();
    };
    let synthetic = format!("{}/{}", file_name, repo_path.as_unix_str());
    RelPath::from_unix_str(&synthetic)
        .map(|path| path.into_arc())
        .unwrap_or_else(|_| repo_path.into_arc())
}

/// Builds a synthetic path whose natural component-wise ordering reproduces a
/// folder-first tree order. Each directory component is prefixed with a NUL
/// byte, which can never appear in a real path component and sorts before every
/// printable character, so at each level directories sort before files.
fn tree_sort_path(repo_path: &RelPath) -> Arc<RelPath> {
    let components: Vec<&str> = repo_path.components().collect();
    if components.len() <= 1 {
        return repo_path.into_arc();
    }
    let last = components.len() - 1;
    let mut synthetic = String::new();
    for (index, component) in components.into_iter().enumerate() {
        if index > 0 {
            synthetic.push('/');
        }
        if index < last {
            synthetic.push('\0');
        }
        synthetic.push_str(component);
    }
    RelPath::from_unix_str(&synthetic)
        .map(|path| path.into_arc())
        .unwrap_or_else(|_| repo_path.into_arc())
}
