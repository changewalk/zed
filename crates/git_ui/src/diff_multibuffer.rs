use crate::{
    conflict_view,
    git_panel::{GitPanel, GitStatusEntry},
    git_panel_settings::GitPanelSettings,
    gitlab_review::{self, GitLabComment},
    review_order::{
        MAX_EXCERPT_BYTES_PER_FILE, ReviewNotes, ReviewOrder, ReviewOrderFile, build_notes_prompt,
        build_order_prompt, parse_review_notes, parse_review_order,
    },
};
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use collections::{HashMap, HashSet};
use editor::{
    EditorEvent, EditorSettings, SelectionEffects, SplittableEditor,
    actions::GoToHunk,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
    multibuffer_context_lines,
    scroll::Autoscroll,
};
use futures::{FutureExt as _, StreamExt as _, stream};
use futures_lite::future::yield_now;
use git::{repository::RepoPath, status::FileStatus};
use gpui::{
    AnyElement, App, AppContext as _, AsyncWindowContext, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, Render, SharedString, Subscription, Task, WeakEntity,
};
use language::{Anchor, Buffer, BufferId, Capability, OffsetRangeExt, Point};
use language_model::{
    CompletionIntent, LanguageModel, LanguageModelProvider, LanguageModelRegistry,
    LanguageModelRequest, LanguageModelRequestMessage, Role,
};
use markdown::{Markdown, MarkdownElement};
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
    review_comment_count: usize,
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
    description: Option<Entity<Markdown>>,
    expanded: bool,
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
        let review_comment_subscription =
            cx.subscribe(&primary_editor, |this, _editor, event: &EditorEvent, cx| {
                if let EditorEvent::ReviewCommentsChanged { total_count } = event {
                    this.review_comment_count = *total_count;
                    cx.notify();
                }
            });

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

        Self {
            workspace: workspace.downgrade(),
            branch_diff,
            focus_handle,
            editor,
            multibuffer,
            buffer_subscriptions: Default::default(),
            pending_scroll: None,
            review_comment_count: 0,
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
            posting_review: false,
            _post_review_task: None,
            _task: task,
            _subscription: Subscription::join(
                branch_diff_subscription,
                Subscription::join(editor_subscription, review_comment_subscription),
            ),
        }
    }

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

    /// Returns the total count of review comments across all hunks/files.
    pub(crate) fn total_review_comment_count(&self) -> usize {
        self.review_comment_count
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
        self._task = window.spawn(cx, {
            let this = cx.weak_entity();
            async |cx| Self::refresh(this, cx).await
        });
        cx.notify();
    }

    pub(crate) fn clear_review_order(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_review_order(None, window, cx);
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
                let description = merge_request
                    .description
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| cx.new(|cx| Markdown::new(text.into(), None, None, cx)));
                this.merge_request = Some(MergeRequestHeader {
                    iid: merge_request.iid,
                    title: merge_request.title.into(),
                    author: merge_request
                        .author
                        .map(|author| author.name)
                        .unwrap_or_default()
                        .into(),
                    web_url: merge_request.web_url.into(),
                    description,
                    expanded: true,
                });
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_merge_request_header(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let header = self.merge_request.as_ref()?;
        let web_url = header.web_url.clone();
        let colors = cx.theme().colors();
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
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(header) = this.merge_request.as_mut() {
                        header.expanded = !header.expanded;
                        cx.notify();
                    }
                })),
            )
            .child(Label::new(header.title.clone()).weight(FontWeight::SEMIBOLD))
            .child(
                Label::new(format!("!{} · {}", header.iid, header.author))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("merge-request-open", IconName::ArrowUpRight)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Open the merge request in the browser"))
                    .on_click(move |_, _, cx| cx.open_url(&web_url)),
            );
        let description = header
            .expanded
            .then(|| header.description.clone())
            .flatten()
            .map(|markdown| {
                div()
                    .id("merge-request-description")
                    .w_full()
                    .max_h(rems(18.))
                    .overflow_y_scroll()
                    .px_3()
                    .pb_2()
                    .text_sm()
                    .child(MarkdownElement::new(
                        markdown,
                        editor::hover_markdown_style(window, cx),
                    ))
            });
        Some(
            v_flex()
                .w_full()
                .bg(colors.panel_background)
                .border_b_1()
                .border_color(colors.border)
                .child(title_row)
                .children(description)
                .into_any_element(),
        )
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
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
        const MAX_POLLS: usize = 60;

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
            // finished and the file count has held still for one poll.
            let mut settled_count = None;
            for _ in 0..MAX_POLLS {
                let ready = this.update(cx, |this, cx| {
                    let count = this.buffer_subscriptions.len();
                    let loaded = count > 0 && !this.is_loading(cx);
                    let settled = loaded && settled_count == Some(count);
                    settled_count = loaded.then_some(count);
                    settled
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
        let known_paths: Vec<RepoPath> = files.iter().map(|file| file.path.clone()).collect();
        let order_request = text_request(build_order_prompt(&files, None), &order_model, cx);
        let notes_request = text_request(build_notes_prompt(&files, None), &notes_model, cx);

        self.pending_notes.take();
        self.review_order_state = ReviewOrderState::Suggesting;
        cx.notify();
        self._review_order_task = Some(cx.spawn_in(window, {
            let known_paths = known_paths.clone();
            async move |this, cx| {
                let result = complete_text(order_provider, order_model, order_request, cx)
                    .await
                    .and_then(|text| parse_review_order(&text, &known_paths));
                this.update_in(cx, |this, window, cx| match result {
                    Ok(order) => this.set_review_order(Some(Arc::new(order)), window, cx),
                    Err(error) => this.fail_review_order(format!("{error:#}").into(), cx),
                })
                .ok();
            }
        }));
        self._review_notes_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = complete_text(notes_provider, notes_model, notes_request, cx)
                .await
                .and_then(|text| parse_review_notes(&text, &known_paths));
            this.update(cx, |this, cx| match result {
                Ok(notes) => this.apply_review_notes(notes, cx),
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

    /// Puts `notes` on the current order, or keeps them for the order that
    /// is still on its way.
    fn apply_review_notes(&mut self, notes: ReviewNotes, cx: &mut Context<Self>) {
        match self.review_order.clone() {
            Some(order) => {
                self.review_order = Some(Arc::new(order.with_notes(notes)));
                self.update_review_order_blocks(cx);
                cx.notify();
            }
            None if self.review_order_state == ReviewOrderState::Suggesting => {
                self.pending_notes = Some(notes);
            }
            None => {}
        }
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

    /// Posts the stored review comments as inline discussions on the open
    /// merge request of the current branch, then drops the ones that posted.
    pub(crate) fn post_review_to_gitlab(&mut self, cx: &mut Context<Self>) {
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

        self.posting_review = true;
        cx.notify();
        let outcome = cx.background_spawn(gitlab_review::post_review(work_dir, branch, comments));
        self._post_review_task = Some(cx.spawn(async move |this, cx| {
            let outcome = outcome.await;
            this.update(cx, |this, cx| {
                this.posting_review = false;
                match outcome {
                    Ok(outcome) => {
                        this.editor
                            .read(cx)
                            .rhs_editor()
                            .clone()
                            .update(cx, |editor, cx| {
                                for id in &outcome.posted_ids {
                                    editor.remove_review_comment(*id, cx);
                                }
                            });
                        match outcome.error {
                            Some(error) => this.fail_post_review(
                                format!(
                                    "{} of the comments posted, then: {error:#}",
                                    outcome.posted_ids.len()
                                )
                                .into(),
                                cx,
                            ),
                            None => this.show_posted_review_toast(
                                outcome.posted_ids.len(),
                                outcome.merge_request.iid,
                                outcome.merge_request.web_url,
                                cx,
                            ),
                        }
                    }
                    Err(error) => this.fail_post_review(format!("{error:#}").into(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
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
            .filter_map(|comment| {
                let start = comment.range.start.to_point(&snapshot);
                let mut end = comment.range.end.to_point(&snapshot);
                // A range that ends at the start of a line does not include that line.
                if end.row > start.row && end.column == 0 {
                    end.row -= 1;
                }
                let (buffer, point) = snapshot.point_to_buffer_point(end)?;
                let path = paths_by_buffer.get(&buffer.remote_id())?;
                Some(GitLabComment {
                    id: comment.id,
                    path: path.as_unix_str().to_string(),
                    line: point.row + 1,
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

    /// Puts a one-line header with the file's place and reason above the first
    /// excerpt of each file the review order lists, and each of the file's
    /// guiding notes above the line it describes.
    fn update_review_order_blocks(&mut self, cx: &mut Context<Self>) {
        let old_blocks: HashSet<CustomBlockId> = self.review_order_blocks.drain(..).collect();
        let mut blocks = Vec::new();
        if let Some(order) = self.review_order.clone()
            && let Some(repo) = self.branch_diff.read(cx).repo().cloned()
        {
            let total = order.len();
            let snapshot = self.multibuffer.read(cx).snapshot(cx);
            for (index, entry) in order.entries().iter().enumerate() {
                if let Some(subscription) = self.buffer_subscriptions.get(&entry.path) {
                    let buffer = subscription.display_buffer.read(cx).snapshot();
                    for note in &entry.notes {
                        // A note past the end of the file lands on its last line.
                        let row = note.line.saturating_sub(1).min(buffer.max_point().row);
                        let text_anchor = buffer.anchor_before(Point::new(row, 0));
                        let Some(anchor) = snapshot.anchor_in_excerpt(text_anchor) else {
                            continue;
                        };
                        let text = note.text.clone();
                        blocks.push(BlockProperties {
                            placement: BlockPlacement::Above(anchor),
                            height: Some(1),
                            style: BlockStyle::Flex,
                            render: Arc::new(move |cx| {
                                h_flex()
                                    .id(cx.block_id)
                                    .h(cx.line_height)
                                    .w_full()
                                    .pl(cx.margins.gutter.width)
                                    .gap_1p5()
                                    .overflow_hidden()
                                    .bg(cx.theme().colors().editor_background)
                                    .child(
                                        Icon::new(IconName::Sparkle)
                                            .size(IconSize::Small)
                                            .color(Color::Accent),
                                    )
                                    .child(
                                        Label::new(text.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .italic(),
                                    )
                                    .into_any_element()
                            }),
                            priority: 0,
                        });
                    }
                }
                let status = self
                    .branch_diff
                    .read(cx)
                    .status_for_path(&entry.path, cx)
                    .unwrap_or(FileStatus::Untracked);
                let path_key =
                    project_diff_path_key(repo.read(cx), &entry.path, status, Some(&*order), cx);
                let Some(anchor) = self.multibuffer.read(cx).location_for_path(&path_key, cx)
                else {
                    continue;
                };
                let reason = entry.reason.clone();
                blocks.push(BlockProperties {
                    placement: BlockPlacement::Above(anchor),
                    height: Some(1),
                    style: BlockStyle::Sticky,
                    render: Arc::new(move |cx| {
                        h_flex()
                            .id(cx.block_id)
                            .h(cx.line_height)
                            .w_full()
                            .ml(cx.margins.gutter.width)
                            .gap_2()
                            .bg(cx.theme().colors().editor_background)
                            .child(
                                Label::new(format!("{} of {}", index + 1, total))
                                    .size(LabelSize::Small)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new(reason.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .into_any_element()
                    }),
                    priority: 0,
                });
            }
        }
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
        this.update(cx, |this, cx| {
            if !buffers_to_fold.is_empty() {
                this.editor.update(cx, |editor, cx| {
                    editor
                        .rhs_editor()
                        .update(cx, |editor, cx| editor.fold_buffers(buffers_to_fold, cx));
                });
            }
            this.pending_scroll.take();
            this.update_review_order_blocks(cx);
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
