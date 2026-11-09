use crate::{
    branch_picker,
    diff_multibuffer::DiffMultibuffer,
    project_diff::{
        self, ApproveMergeRequest, AskAboutDiff, ClearReviewOrder, CompareWithBranch,
        DeployBranchDiff, FinishReview, GoToReviewComment, MarkReviewed, NextReviewComment,
        PostReviewToGitLab, PreviousReviewComment, ProjectDiff, RefreshReviewThreads, ReviewDiff,
        SuggestReviewOrder, ToggleChangesSinceReview, ToggleMergeRequestDescription,
        render_post_review_to_gitlab_button, render_review_buttons, render_review_order_button,
        render_send_review_to_agent_button,
    },
};
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result, anyhow};
use editor::{
    Addon, Editor, EditorEvent, HiddenUnstagedDiffHunkRenderer, SplittableEditor,
    actions::SendReviewToAgent,
};
use git::{repository::DiffType, status::FileStatus};
use gpui::{
    Action, App, AppContext as _, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, Task, WeakEntity,
};
use language::{BufferId, Capability};
use project::{
    Project, ProjectPath,
    git_store::{
        Repository,
        diff_buffer_list::{self, DiffBase},
    },
};
use settings::{GitDiffBaseSetting, Settings};
use std::{
    any::{Any, TypeId},
    sync::Arc,
};
use ui::{DiffStat, Divider, PopoverMenu, Tooltip, prelude::*};
use workspace::{
    CloseInactiveTabsAndPanes, ItemHandle, ItemNavHistory, SerializableItem, ToolbarItemEvent,
    ToolbarItemLocation, ToolbarItemView, Workspace,
    item::{Item, ItemEvent, SaveOptions},
    notifications::NotifyTaskExt,
    searchable::SearchableItemHandle,
};
use zed_actions::agent::{AskAboutBranchDiff, ReviewBranchDiff};

/// The workspace item for a branch (merge-base) diff: "Changes since {branch}".
/// It wraps a single [`DiffMultibuffer`] over [`DiffBase::Merge`] and delegates
/// the [`Item`] surface to it. The merge base can be changed in place via the
/// [`BranchDiffToolbar`]'s branch picker, which reloads without reconfiguring
/// the editor (the merge styling is identical for every base ref).
pub struct BranchDiff {
    diff: Entity<DiffMultibuffer>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    _diff_event_subscription: Subscription,
    /// A thread primed with this diff exists in the agent panel, so Ask
    /// only needs to show it.
    ask_thread_prepared: bool,
    _prepare_ask_thread_task: Option<Task<()>>,
}

struct BranchDiffAddon {
    branch_diff: Entity<diff_buffer_list::DiffBufferList>,
}

impl Addon for BranchDiffAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn override_status_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<FileStatus> {
        self.branch_diff
            .read(cx)
            .status_for_buffer_id(buffer_id, cx)
    }
}

/// What a `zed://git/review` launch asks the branch diff to do once open.
#[derive(Clone)]
struct ReviewLaunch {
    merge_request_file: Option<std::path::PathBuf>,
}

impl BranchDiff {
    pub(crate) fn register(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        workspace.register_action(|workspace, _: &DeployBranchDiff, window, cx| {
            Self::deploy_branch_diff(workspace, window, cx)
        });
        workspace.register_action(Self::compare_with_branch);
        workspace::register_serializable_item::<Self>(cx);
    }

    pub(crate) fn deploy_branch_diff(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::deploy_branch_diff_impl(workspace, None, None, window, cx);
    }

    /// Opens the branch diff and asks the model for a review order as soon as
    /// the files have loaded. Used by the `zed://git/review` URL. A `base_ref`
    /// replaces the default branch, for a merge request stacked on another.
    ///
    /// The URL often arrives before the project has scanned its repository,
    /// so this waits for one to appear before opening the diff.
    pub fn deploy_for_review(
        workspace: &mut Workspace,
        base_ref: Option<SharedString>,
        merge_request_file: Option<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
        const MAX_POLLS: usize = 120;

        let project = workspace.project().clone();
        let workspace = cx.entity();
        let workspace_weak = workspace.downgrade();
        window
            .spawn(cx, async move |cx| {
                for poll in 0..MAX_POLLS {
                    let deployed = workspace.update_in(cx, |workspace, window, cx| {
                        if project.read(cx).active_repository(cx).is_none() {
                            return false;
                        }
                        log::info!("review url: repository ready after {poll} polls");
                        Self::deploy_branch_diff_impl(
                            workspace,
                            base_ref.clone(),
                            Some(ReviewLaunch {
                                merge_request_file: merge_request_file.clone(),
                            }),
                            window,
                            cx,
                        );
                        true
                    })?;
                    if deployed {
                        return Ok(());
                    }
                    cx.background_executor().timer(POLL_INTERVAL).await;
                }
                Err(anyhow!("No git repository was found in the project"))
            })
            .detach_and_notify_err(workspace_weak, window, cx);
    }

    fn deploy_branch_diff_impl(
        workspace: &mut Workspace,
        base_ref_override: Option<SharedString>,
        review: Option<ReviewLaunch>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        telemetry::event!("Git Branch Diff Opened");
        let project = workspace.project().clone();
        let Some(intended_repo) = project.read(cx).active_repository(cx) else {
            let workspace = cx.entity().downgrade();
            window
                .spawn(cx, async |_cx| {
                    let result: Result<()> = Err(anyhow!("No active repository"));
                    result
                })
                .detach_and_notify_err(workspace, window, cx);
            return;
        };

        if let Some(base_ref) = base_ref_override {
            // A stacked merge request's own base is not the shared default
            // branch diff, so it gets a diff of its own.
            Self::deploy_branch_diff_with_base_ref_impl(
                workspace,
                project,
                intended_repo,
                base_ref,
                None,
                review,
                window,
                cx,
            );
            return;
        }

        let default_branch = intended_repo.update(cx, |repo, _| repo.default_branch(true));
        let workspace = cx.entity();
        let workspace_weak = workspace.downgrade();
        window
            .spawn(cx, async move |cx| {
                let base_ref = default_branch.await??;
                workspace.update_in(cx, |workspace, window, cx| {
                    let git_store = project.read(cx).git_store().clone();
                    let Some(base_ref) = base_ref else {
                        ProjectDiff::deploy_at(workspace, None, window, cx);
                        return;
                    };
                    let branch_diff =
                        if git_store.read(cx).diff_base() == GitDiffBaseSetting::DefaultBranch {
                            Some(git_store.update(cx, |git_store, cx| {
                                git_store.ensure_display_diff(
                                    intended_repo.clone(),
                                    base_ref.clone(),
                                    cx,
                                )
                            }))
                        } else {
                            None
                        };
                    Self::deploy_branch_diff_with_base_ref_impl(
                        workspace,
                        project,
                        intended_repo,
                        base_ref,
                        branch_diff,
                        review,
                        window,
                        cx,
                    );
                })?;

                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace_weak, window, cx);
    }

    fn compare_with_branch(
        workspace: &mut Workspace,
        _: &CompareWithBranch,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let Some(repository) = project.read(cx).active_repository(cx) else {
            let workspace = cx.entity().downgrade();
            window
                .spawn(cx, async |_cx| {
                    let result: Result<()> = Err(anyhow!("No active repository"));
                    result
                })
                .detach_and_notify_err(workspace, window, cx);
            return;
        };
        let selected_branch = workspace.active_item_as::<Self>(cx).and_then(|item| {
            match item.read(cx).diff_base(cx) {
                DiffBase::Merge { base_ref } => Some(base_ref.clone()),
                DiffBase::Head | DiffBase::Index | DiffBase::Staged => None,
            }
        });
        let workspace_handle = workspace.weak_handle();
        let on_select = Arc::new({
            let repository = repository.clone();
            let workspace = workspace_handle.clone();
            move |branch: git::repository::Branch, window: &mut Window, cx: &mut App| {
                let base_ref: SharedString = branch.name().to_owned().into();
                workspace
                    .update(cx, |workspace, cx| {
                        Self::deploy_branch_diff_with_base_ref(
                            workspace,
                            project.clone(),
                            repository.clone(),
                            base_ref,
                            None,
                            window,
                            cx,
                        );
                    })
                    .ok();
            }
        });

        workspace.toggle_modal(window, cx, |window, cx| {
            branch_picker::select_modal(
                workspace_handle,
                Some(repository),
                selected_branch,
                on_select,
                window,
                cx,
            )
        });
    }

    /// A review launch opens a workspace that may restore the tabs of an
    /// earlier session, which are noise next to the diff. Anything unsaved
    /// still asks before closing.
    fn close_other_tabs(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        workspace.close_inactive_items_and_panes(
            &CloseInactiveTabsAndPanes {
                save_intent: Some(workspace::SaveIntent::Close),
            },
            window,
            cx,
        );
    }

    pub(crate) fn deploy_branch_diff_with_base_ref(
        workspace: &mut Workspace,
        project: Entity<Project>,
        intended_repo: Entity<Repository>,
        base_ref: SharedString,
        branch_diff: Option<Entity<diff_buffer_list::DiffBufferList>>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::deploy_branch_diff_with_base_ref_impl(
            workspace,
            project,
            intended_repo,
            base_ref,
            branch_diff,
            None,
            window,
            cx,
        );
    }

    fn deploy_branch_diff_with_base_ref_impl(
        workspace: &mut Workspace,
        project: Entity<Project>,
        intended_repo: Entity<Repository>,
        base_ref: SharedString,
        branch_diff: Option<Entity<diff_buffer_list::DiffBufferList>>,
        review: Option<ReviewLaunch>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace.items_of_type::<Self>(cx).find(|item| {
            let item = item.read(cx);
            matches!(
                item.diff_base(cx),
                DiffBase::Merge { base_ref: existing_base_ref } if existing_base_ref == &base_ref
            ) && item
                .repo(cx)
                .is_some_and(|repo| repo.read(cx).id == intended_repo.read(cx).id)
                && branch_diff.as_ref().is_none_or(|expected| {
                    item.diff.read(cx).branch_diff().entity_id() == expected.entity_id()
                })
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            if let Some(review) = review {
                // The view is being reused for another branch, so nothing
                // of the previous review may linger: not the header, not the
                // order, not the threads, and not the primed thread.
                existing.update(cx, |item, cx| {
                    item.diff.update(cx, |diff, cx| {
                        diff.reset_review(window, cx);
                        diff.load_merge_request(review.merge_request_file, cx);
                        diff.suggest_review_order_when_loaded(window, cx)
                    });
                    item.ask_thread_prepared = false;
                    item._prepare_ask_thread_task.take();
                    item.prepare_ask_thread_when_ordered(window, cx);
                });
                Self::close_other_tabs(workspace, window, cx);
            }
            return;
        }

        let workspace = cx.entity();
        let workspace_weak = workspace.downgrade();
        window
            .spawn(cx, async move |cx| {
                let this = cx
                    .update(|window, cx| {
                        Self::new_with_branch_base(
                            project,
                            workspace.clone(),
                            base_ref,
                            intended_repo,
                            branch_diff,
                            window,
                            cx,
                        )
                    })?
                    .await?;
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.add_item_to_active_pane(
                            Box::new(this.clone()),
                            None,
                            true,
                            window,
                            cx,
                        );
                        if let Some(review) = review {
                            this.update(cx, |item, cx| {
                                item.diff.update(cx, |diff, cx| {
                                    diff.load_merge_request(review.merge_request_file, cx);
                                    diff.suggest_review_order_when_loaded(window, cx)
                                });
                                item.prepare_ask_thread_when_ordered(window, cx);
                            });
                            Self::close_other_tabs(workspace, window, cx);
                        }
                    })
                    .ok();
                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace_weak, window, cx);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_default_branch(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let Some(repo) = project.read(cx).git_store().read(cx).active_repository() else {
            return Task::ready(Err(anyhow!("No active repository")));
        };
        let main_branch = repo.update(cx, |repo, _| repo.default_branch(true));
        window.spawn(cx, async move |cx| {
            let base_ref = main_branch
                .await??
                .context("Could not determine default branch")?;
            cx.update(|window, cx| {
                let git_store = project.read(cx).git_store().clone();
                let branch_diff =
                    if git_store.read(cx).diff_base() == GitDiffBaseSetting::DefaultBranch {
                        Some(git_store.update(cx, |git_store, cx| {
                            git_store.ensure_display_diff(repo.clone(), base_ref.clone(), cx)
                        }))
                    } else {
                        None
                    };
                cx.new(|cx| {
                    Self::new_with_base_ref(
                        project,
                        workspace,
                        base_ref,
                        Some(repo),
                        branch_diff,
                        window,
                        cx,
                    )
                })
            })
        })
    }

    pub(crate) fn new_with_branch_base(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        base_ref: SharedString,
        repo: Entity<Repository>,
        branch_diff: Option<Entity<diff_buffer_list::DiffBufferList>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            cx.update(|window, cx| {
                cx.new(|cx| {
                    Self::new_with_base_ref(
                        project,
                        workspace,
                        base_ref,
                        Some(repo),
                        branch_diff,
                        window,
                        cx,
                    )
                })
            })
        })
    }

    pub(crate) fn new_with_base_ref(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        base_ref: SharedString,
        repo: Option<Entity<Repository>>,
        branch_diff: Option<Entity<diff_buffer_list::DiffBufferList>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch_diff = branch_diff.unwrap_or_else(|| {
            let git_store = project.read(cx).git_store().clone();
            cx.new(|cx| {
                diff_buffer_list::DiffBufferList::new(
                    DiffBase::Merge { base_ref },
                    git_store,
                    repo,
                    cx,
                )
            })
        });
        let branch_diff_for_addon = branch_diff.clone();
        let diff = cx.new(|cx| {
            DiffMultibuffer::new(
                branch_diff,
                Capability::ReadWrite,
                "No changes",
                move |editor, cx| {
                    // A branch's changes are all committed, so by the staged
                    // rule every hunk would draw hollow; a reviewer wants
                    // them filled, as the uncommitted diff draws them.
                    editor
                        .set_diff_hunk_renderer(Some(Arc::new(HiddenUnstagedDiffHunkRenderer)), cx);
                    editor.rhs_editor().update(cx, move |rhs_editor, _cx| {
                        rhs_editor.set_read_only(false);
                        rhs_editor.register_addon(BranchDiffAddon {
                            branch_diff: branch_diff_for_addon,
                        });
                    });
                },
                project.clone(),
                workspace.clone(),
                window,
                cx,
            )
        });
        Self::from_diff(diff, project, workspace, cx)
    }

    fn from_diff(
        diff: Entity<DiffMultibuffer>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let diff_event_subscription = cx.subscribe(&diff, |_, _, event: &EditorEvent, cx| {
            cx.emit(event.clone())
        });
        Self {
            diff,
            project,
            workspace: workspace.downgrade(),
            _diff_event_subscription: diff_event_subscription,
            ask_thread_prepared: false,
            _prepare_ask_thread_task: None,
        }
    }

    pub(crate) fn diff_base<'a>(&'a self, cx: &'a App) -> &'a DiffBase {
        self.diff.read(cx).diff_base(cx)
    }

    pub(crate) fn repo(&self, cx: &App) -> Option<Entity<Repository>> {
        self.diff.read(cx).repo(cx)
    }

    fn set_merge_base(&mut self, base_ref: SharedString, cx: &mut Context<Self>) {
        self.diff.update(cx, |diff, cx| {
            diff.branch_diff().update(cx, |branch_diff, cx| {
                branch_diff.set_diff_base(DiffBase::Merge { base_ref }, cx);
            });
        });
    }

    fn review_diff(&mut self, _: &ReviewDiff, window: &mut Window, cx: &mut Context<Self>) {
        let DiffBase::Merge { base_ref } = self.diff_base(cx).clone() else {
            return;
        };
        let Some(repo) = self.repo(cx) else {
            return;
        };

        let diff_receiver = repo.update(cx, |repo, cx| {
            repo.diff(
                DiffType::MergeBase {
                    base_ref: base_ref.clone(),
                },
                cx,
            )
        });

        let workspace = self.workspace.clone();
        window
            .spawn(cx, {
                let workspace = workspace.clone();
                async move |cx| {
                    let diff_text = diff_receiver.await??;

                    if let Some(workspace) = workspace.upgrade() {
                        workspace.update_in(cx, |_workspace, window, cx| {
                            window.dispatch_action(
                                ReviewBranchDiff {
                                    diff_text: diff_text.into(),
                                    base_ref,
                                }
                                .boxed_clone(),
                                cx,
                            );
                        })?;
                    }

                    anyhow::Ok(())
                }
            })
            .detach_and_notify_err(workspace, window, cx);
    }

    /// Shows the thread primed with this diff, or primes one now if none
    /// was prepared in the background.
    fn ask_about_diff(&mut self, _: &AskAboutDiff, window: &mut Window, cx: &mut Context<Self>) {
        if self.ask_thread_prepared {
            window.dispatch_action(zed_actions::assistant::ToggleFocus.boxed_clone(), cx);
            return;
        }
        self.prepare_ask_thread(true, window, cx);
    }

    /// Primes the thread once the review order is in, so the context it
    /// carries is complete, or after a while if no order comes.
    fn prepare_ask_thread_when_ordered(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
        const MAX_POLLS: usize = 240;

        if self.ask_thread_prepared || self._prepare_ask_thread_task.is_some() {
            return;
        }
        self._prepare_ask_thread_task = Some(cx.spawn_in(window, async move |this, cx| {
            for _ in 0..MAX_POLLS {
                let ordered = this.update(cx, |this, cx| {
                    this.diff.read(cx).review_order().is_some()
                        || matches!(
                            this.diff.read(cx).review_order_state(),
                            crate::diff_multibuffer::ReviewOrderState::Failed(_)
                        )
                });
                match ordered {
                    Ok(true) => break,
                    Ok(false) => cx.background_executor().timer(POLL_INTERVAL).await,
                    Err(_) => return,
                }
            }
            this.update_in(cx, |this, window, cx| {
                if !this.ask_thread_prepared {
                    this.prepare_ask_thread(false, window, cx);
                }
            })
            .ok();
        }));
    }

    /// Opens a thread primed with the diff and what is known about the merge
    /// request, told to read rather than review, so questions can start at
    /// once. With `focus` the panel is shown; without, it is left as it is.
    fn prepare_ask_thread(&mut self, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let DiffBase::Merge { base_ref } = self.diff_base(cx).clone() else {
            return;
        };
        let Some(repo) = self.repo(cx) else {
            return;
        };
        self.ask_thread_prepared = true;
        let context: SharedString = self.diff.read(cx).ask_context().into();
        let diff_receiver = repo.update(cx, |repo, cx| {
            repo.diff(
                DiffType::MergeBase {
                    base_ref: base_ref.clone(),
                },
                cx,
            )
        });
        let workspace = self.workspace.clone();
        window
            .spawn(cx, {
                let workspace = workspace.clone();
                async move |cx| {
                    let diff_text = diff_receiver.await??;
                    if let Some(workspace) = workspace.upgrade() {
                        workspace.update_in(cx, |_workspace, window, cx| {
                            window.dispatch_action(
                                AskAboutBranchDiff {
                                    diff_text: diff_text.into(),
                                    base_ref,
                                    context,
                                    focus,
                                }
                                .boxed_clone(),
                                cx,
                            );
                        })?;
                    }
                    anyhow::Ok(())
                }
            })
            .detach_and_notify_err(workspace, window, cx);
    }

    fn suggest_review_order(
        &mut self,
        _: &SuggestReviewOrder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.suggest_review_order(window, cx));
    }

    fn clear_review_order(
        &mut self,
        _: &ClearReviewOrder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.clear_review_order(window, cx));
    }

    fn post_review_to_gitlab(
        &mut self,
        _: &PostReviewToGitLab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.post_review_to_gitlab(window, cx));
    }

    fn refresh_review_threads(
        &mut self,
        _: &RefreshReviewThreads,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.sync_review_threads(window, cx));
    }

    fn finish_review(&mut self, _: &FinishReview, window: &mut Window, cx: &mut Context<Self>) {
        self.diff
            .update(cx, |diff, cx| diff.toggle_review_summary(window, cx));
    }

    fn approve_merge_request(
        &mut self,
        _: &ApproveMergeRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff.update(cx, |diff, cx| {
            diff.finish_review(crate::diff_multibuffer::ReviewVerdict::Approve, window, cx)
        });
    }

    fn mark_reviewed(&mut self, _: &MarkReviewed, _: &mut Window, cx: &mut Context<Self>) {
        self.diff.update(cx, |diff, cx| diff.mark_reviewed(cx));
    }

    fn toggle_merge_request_description(
        &mut self,
        _: &ToggleMergeRequestDescription,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.toggle_merge_request_description(cx));
    }

    fn next_review_comment(
        &mut self,
        _: &NextReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.go_to_review_comment(true, window, cx));
    }

    fn previous_review_comment(
        &mut self,
        _: &PreviousReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.go_to_review_comment(false, window, cx));
    }

    fn go_to_review_comment(
        &mut self,
        action: &GoToReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff.update(cx, |diff, cx| {
            diff.go_to_review_comment_id(action.id, window, cx)
        });
    }

    fn toggle_changes_since_review(
        &mut self,
        _: &ToggleChangesSinceReview,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.toggle_changes_since_review(cx));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn editor(&self, cx: &App) -> Entity<SplittableEditor> {
        self.diff.read(cx).editor().clone()
    }
}

impl EventEmitter<EditorEvent> for BranchDiff {}

impl Focusable for BranchDiff {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.diff.read(cx).focus_handle(cx)
    }
}

impl Item for BranchDiff {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch).color(Color::Muted))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.diff
            .update(cx, |diff, cx| diff.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.diff
            .update(cx, |diff, cx| diff.navigate(data, window, cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.tab_content_text(0, cx))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        match self.diff_base(cx) {
            DiffBase::Merge { base_ref } => {
                format!("Changes since {}", display_ref(base_ref)).into()
            }
            DiffBase::Head | DiffBase::Index | DiffBase::Staged => "Changes".into(),
        }
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Branch Diff Opened")
    }

    fn as_searchable(&self, _: &Entity<Self>, cx: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.diff.read(cx).editor().clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.diff.read(cx).for_each_project_item(cx, f);
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.diff.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.set_nav_history(nav_history, cx));
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let DiffBase::Merge { base_ref } = self.diff_base(cx).clone() else {
            return Task::ready(None);
        };
        let repo = self.repo(cx);
        let project = self.project.clone();
        let branch_diff = self.diff.read(cx).branch_diff().clone();
        Task::ready(Some(cx.new(|cx| {
            Self::new_with_base_ref(
                project,
                workspace,
                base_ref,
                repo,
                Some(branch_diff),
                window,
                cx,
            )
        })))
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.diff.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.diff.read(cx).has_conflict(cx)
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.diff
            .update(cx, |diff, cx| diff.save(options, project, window, cx))
    }

    fn save_as(
        &mut self,
        _: Entity<Project>,
        _: ProjectPath,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Task<Result<()>> {
        unreachable!()
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.diff
            .update(cx, |diff, cx| diff.reload(project, window, cx))
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<DiffMultibuffer>() {
            Some(self.diff.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(
                self.diff
                    .read(cx)
                    .editor()
                    .read(cx)
                    .rhs_editor()
                    .clone()
                    .into(),
            )
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.diff.read(cx).editor().clone().into())
        } else if type_id == TypeId::of::<diff_buffer_list::DiffBufferList>() {
            Some(self.diff.read(cx).branch_diff().clone().into())
        } else {
            None
        }
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff.update(cx, |diff, cx| {
            diff.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for BranchDiff {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .on_action(cx.listener(Self::review_diff))
            .on_action(cx.listener(Self::ask_about_diff))
            .on_action(cx.listener(Self::suggest_review_order))
            .on_action(cx.listener(Self::clear_review_order))
            .on_action(cx.listener(Self::post_review_to_gitlab))
            .on_action(cx.listener(Self::refresh_review_threads))
            .on_action(cx.listener(Self::finish_review))
            .on_action(cx.listener(Self::approve_merge_request))
            .on_action(cx.listener(Self::mark_reviewed))
            .on_action(cx.listener(Self::toggle_merge_request_description))
            .on_action(cx.listener(Self::toggle_changes_since_review))
            .on_action(cx.listener(Self::next_review_comment))
            .on_action(cx.listener(Self::previous_review_comment))
            .on_action(cx.listener(Self::go_to_review_comment))
            .child(self.diff.clone())
    }
}

impl SerializableItem for BranchDiff {
    fn serialized_item_kind() -> &'static str {
        "BranchDiff"
    }

    fn cleanup(
        _: workspace::WorkspaceId,
        _: Vec<workspace::ItemId>,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = project_diff::persistence::ProjectDiffDb::global(cx);
        window.spawn(cx, async move |cx| {
            let diff_base = db.get_project_diff_base(item_id, workspace_id)?;
            let DiffBase::Merge { base_ref } = diff_base else {
                anyhow::bail!("expected a merge base for a branch diff");
            };
            let workspace = workspace.upgrade().context("workspace gone")?;
            cx.update(|window, cx| {
                cx.new(|cx| {
                    Self::new_with_base_ref(project, workspace, base_ref, None, None, window, cx)
                })
            })
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let DiffBase::Merge { base_ref } = self.diff_base(cx).clone() else {
            return None;
        };
        let diff_base = DiffBase::Merge { base_ref };
        let db = project_diff::persistence::ProjectDiffDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_project_diff_base(item_id, workspace_id, diff_base)
                .await
        }))
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        false
    }
}

pub struct BranchDiffToolbar {
    branch_diff: Option<WeakEntity<BranchDiff>>,
    /// Repaints the buttons as the diff's review state changes.
    _diff_observation: Option<Subscription>,
}

impl BranchDiffToolbar {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            branch_diff: None,
            _diff_observation: None,
        }
    }

    fn branch_diff(&self, _: &App) -> Option<Entity<BranchDiff>> {
        self.branch_diff.as_ref()?.upgrade()
    }

    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(branch_diff) = self.branch_diff(cx) {
            branch_diff.focus_handle(cx).focus(window, cx);
        }
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        })
    }
}

impl EventEmitter<ToolbarItemEvent> for BranchDiffToolbar {}

impl ToolbarItemView for BranchDiffToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.branch_diff = active_pane_item
            .and_then(|item| item.act_as::<BranchDiff>(cx))
            .map(|entity| entity.downgrade());
        self._diff_observation = self.branch_diff(cx).map(|branch_diff| {
            let diff = branch_diff.read(cx).diff.clone();
            cx.observe(&diff, |_, _, cx| cx.notify())
        });
        if self.branch_diff.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

/// A ref as a label: a full commit hash, which a review launched on
/// GitLab's base commit uses, is cut to its first eight characters.
fn display_ref(base_ref: &str) -> String {
    let is_hash = base_ref.len() >= 20 && base_ref.chars().all(|c| c.is_ascii_hexdigit());
    if is_hash {
        base_ref.chars().take(8).collect()
    } else {
        base_ref.to_string()
    }
}

impl Render for BranchDiffToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(branch_diff) = self.branch_diff(cx) else {
            return div();
        };
        let focus_handle = branch_diff.focus_handle(cx);
        let review_count = branch_diff
            .read(cx)
            .diff
            .read(cx)
            .unposted_review_comment_count(cx);
        let (additions, deletions) = branch_diff
            .read(cx)
            .diff
            .read(cx)
            .calculate_changed_lines(cx);
        let diff_base = branch_diff.read(cx).diff_base(cx).clone();
        let DiffBase::Merge { base_ref } = diff_base else {
            return div();
        };
        let selected_base_ref = base_ref.clone();
        let base_ref_label = format!("Base: {}", display_ref(&base_ref));
        let repository = branch_diff.read(cx).repo(cx);
        let workspace = branch_diff.read(cx).workspace.clone();
        let project = branch_diff.read(cx).project.clone();
        let buffers = branch_diff.read(cx).diff.read(cx).branch_diff().clone();
        let uses_display_diff = repository.as_ref().is_some_and(|repository| {
            project
                .read(cx)
                .git_store()
                .read(cx)
                .display_diff_for_repo(repository.read(cx).id)
                .is_some_and(|display_diff| display_diff.entity_id() == buffers.entity_id())
        });
        let view_for_picker = branch_diff.downgrade();

        let is_multibuffer_empty = branch_diff
            .read(cx)
            .diff
            .read(cx)
            .multibuffer()
            .read(cx)
            .is_empty();
        let is_ai_enabled = AgentSettings::get_global(cx).enabled(cx);

        let show_review_button = !is_multibuffer_empty && is_ai_enabled;
        let review_order_button = show_review_button
            .then(|| render_review_order_button(branch_diff.read(cx).diff.read(cx), &focus_handle));

        h_flex()
            .my_neg_1()
            .py_1()
            .gap_1p5()
            .flex_wrap()
            .justify_between()
            .when(!is_multibuffer_empty, |this| {
                this.child(DiffStat::new(
                    "branch-diff-stat",
                    additions as usize,
                    deletions as usize,
                ))
            })
            .child(Divider::vertical().ml_1())
            .child(
                PopoverMenu::new("branch-diff-base-branch-picker")
                    .menu(move |window, cx| {
                        let view_for_picker = view_for_picker.clone();
                        let workspace_for_select = workspace.clone();
                        let repository_for_select = repository.clone();
                        let project = project.clone();
                        let on_select = Arc::new(
                            move |branch: git::repository::Branch,
                                  window: &mut Window,
                                  cx: &mut App| {
                                let base_ref: SharedString = branch.name().to_owned().into();
                                if uses_display_diff {
                                    let Some(repository) = repository_for_select.clone() else {
                                        return;
                                    };
                                    workspace_for_select
                                        .update(cx, |workspace, cx| {
                                            BranchDiff::deploy_branch_diff_with_base_ref(
                                                workspace,
                                                project.clone(),
                                                repository,
                                                base_ref,
                                                None,
                                                window,
                                                cx,
                                            );
                                        })
                                        .ok();
                                } else {
                                    view_for_picker
                                        .update(cx, |branch_diff, cx| {
                                            branch_diff.set_merge_base(base_ref, cx);
                                            cx.notify();
                                        })
                                        .ok();
                                }
                            },
                        );

                        Some(branch_picker::select_popover(
                            workspace.clone(),
                            repository.clone(),
                            Some(selected_base_ref.clone()),
                            on_select,
                            window,
                            cx,
                        ))
                    })
                    .trigger_with_tooltip(
                        Button::new("branch-diff-base-branch", base_ref_label).end_icon(
                            Icon::new(IconName::ChevronDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        ),
                        Tooltip::text("Select Base Branch"),
                    ),
            )
            .when(show_review_button, |this| {
                let focus_handle = focus_handle.clone();
                this.child(Divider::vertical()).child(
                    Button::new("ask-about-diff", "Ask")
                        .start_icon(
                            Icon::new(IconName::ZedAssistant)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .tooltip(move |_, cx| {
                            Tooltip::with_meta_in(
                                "Ask About This Change",
                                Some(&AskAboutDiff),
                                "Show the thread primed with the diff, the merge request, \
                                 and the reading order, and ask away.",
                                &focus_handle,
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dispatch_action(&AskAboutDiff, window, cx);
                        })),
                )
            })
            .when_some(review_order_button, |this, (button, action)| {
                this.child(Divider::vertical())
                    .child(button.on_click(cx.listener(move |this, _, window, cx| {
                        this.dispatch_action(action.as_ref(), window, cx)
                    })))
            })
            .when(review_count > 0, |this| {
                let posting = branch_diff.read(cx).diff.read(cx).is_posting_review();
                this.child(Divider::vertical())
                    .child(
                        render_send_review_to_agent_button(review_count, &focus_handle).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&SendReviewToAgent, window, cx)
                            }),
                        ),
                    )
                    .child(
                        render_post_review_to_gitlab_button(review_count, posting, &focus_handle)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&PostReviewToGitLab, window, cx)
                            })),
                    )
            })
            .children(render_review_buttons(
                &branch_diff.read(cx).diff,
                &focus_handle,
                cx,
            ))
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use collections::HashMap;
    use editor::test::editor_test_context::assert_state_with_diff;
    use git::status::{FileStatus, TrackedStatus, UnmergedStatus, UnmergedStatusCode};
    use gpui::TestAppContext;
    use project::FakeFs;
    use serde_json::json;
    use settings::{DiffViewStyle, SettingsStore};
    use std::path::Path;
    use std::sync::Arc;
    use unindent::Unindent as _;
    use util::{
        path,
        rel_path::{RelPath, rel_path},
    };
    use workspace::MultiWorkspace;

    use super::*;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Unified);
                });
            });
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    #[gpui::test(iterations = 50)]
    async fn test_split_diff_conflict_path_transition_with_dirty_buffer_invalid_anchor_panics(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Split);
                });
            });
        });

        let build_conflict_text: fn(usize) -> String = |tag: usize| {
            let mut lines = (0..80)
                .map(|line_index| format!("line {line_index}"))
                .collect::<Vec<_>>();
            for offset in [5usize, 20, 37, 61] {
                lines[offset] = format!("base-{tag}-line-{offset}");
            }
            format!("{}\n", lines.join("\n"))
        };
        let initial_conflict_text = build_conflict_text(0);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "helper.txt": "same\n",
                "conflict.txt": initial_conflict_text,
            }),
        )
        .await;
        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state
                .refs
                .insert("MERGE_HEAD".into(), "conflict-head".into());
        })
        .unwrap();
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[(
                "conflict.txt",
                FileStatus::Unmerged(UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Updated,
                }),
            )],
        );
        fs.set_merge_base_content_for_repo(
            path!("/project/.git").as_ref(),
            &[
                ("conflict.txt", build_conflict_text(1)),
                ("helper.txt", "same\n".to_string()),
            ],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let _branch_diff = cx
            .update(|window, cx| {
                BranchDiff::new_with_default_branch(project.clone(), workspace, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/conflict.txt"), cx)
            })
            .await
            .unwrap();
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "dirty\n")], None, cx));
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        cx.run_until_parked();

        cx.update(|window, cx| {
            let fs = fs.clone();
            window
                .spawn(cx, async move |cx| {
                    cx.background_executor().simulate_random_delay().await;
                    fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
                        state.refs.insert("HEAD".into(), "head-1".into());
                        state.refs.remove("MERGE_HEAD");
                    })
                    .unwrap();
                    fs.set_status_for_repo(
                        path!("/project/.git").as_ref(),
                        &[
                            (
                                "conflict.txt",
                                FileStatus::Tracked(TrackedStatus {
                                    index_status: git::status::StatusCode::Modified,
                                    worktree_status: git::status::StatusCode::Modified,
                                }),
                            ),
                            (
                                "helper.txt",
                                FileStatus::Tracked(TrackedStatus {
                                    index_status: git::status::StatusCode::Modified,
                                    worktree_status: git::status::StatusCode::Modified,
                                }),
                            ),
                        ],
                    );
                    // FakeFs assigns deterministic OIDs by entry position; flipping order churns
                    // conflict diff identity without reaching into view internals.
                    fs.set_merge_base_content_for_repo(
                        path!("/project/.git").as_ref(),
                        &[
                            ("helper.txt", "helper-base\n".to_string()),
                            ("conflict.txt", build_conflict_text(2)),
                        ],
                    );
                })
                .detach();
        });

        cx.update(|window, cx| {
            let buffer = buffer.clone();
            window
                .spawn(cx, async move |cx| {
                    cx.background_executor().simulate_random_delay().await;
                    for edit_index in 0..10 {
                        if edit_index > 0 {
                            cx.background_executor().simulate_random_delay().await;
                        }
                        buffer.update(cx, |buffer, cx| {
                            let len = buffer.len();
                            if edit_index % 2 == 0 {
                                buffer.edit(
                                    [(0..0, format!("status-burst-head-{edit_index}\n"))],
                                    None,
                                    cx,
                                );
                            } else {
                                buffer.edit(
                                    [(len..len, format!("status-burst-tail-{edit_index}\n"))],
                                    None,
                                    cx,
                                );
                            }
                        });
                    }
                })
                .detach();
        });

        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_branch_diff(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "C",
                "b.txt": "new",
                "c.txt": "in-merge-base-and-work-tree",
                "d.txt": "created-in-head",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let diff = cx
            .update(|window, cx| {
                BranchDiff::new_with_default_branch(project.clone(), workspace, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "B".into()), ("d.txt", "created-in-head".into())],
            "sha",
        );
        fs.set_merge_base_content_for_repo(
            Path::new(path!("/project/.git")),
            &[
                ("a.txt", "A".into()),
                ("c.txt", "in-merge-base-and-work-tree".into()),
            ],
        );
        cx.run_until_parked();

        let editor = diff.read_with(cx, |diff, cx| diff.editor(cx).read(cx).rhs_editor().clone());

        let expected_diff = "
                - A
                + C
                + ˇnew
                + created-in-head"
            .unindent();
        assert_state_with_diff(&editor, cx, &expected_diff);

        cx.update(|window, cx| {
            editor.focus_handle(cx).focus(window, cx);
            window.dispatch_action(git::Restore.boxed_clone(), cx);
        });
        cx.run_until_parked();
        assert_state_with_diff(&editor, cx, &expected_diff);

        let statuses: HashMap<Arc<RelPath>, Option<FileStatus>> =
            editor.update(cx, |editor, cx| {
                editor
                    .buffer()
                    .read(cx)
                    .all_buffers()
                    .iter()
                    .map(|buffer| {
                        (
                            buffer.read(cx).file().unwrap().path().clone(),
                            editor.status_for_buffer_id(buffer.read(cx).remote_id(), cx),
                        )
                    })
                    .collect()
            });

        assert_eq!(
            statuses,
            HashMap::from_iter([
                (
                    rel_path("a.txt").into_arc(),
                    Some(FileStatus::Tracked(TrackedStatus {
                        index_status: git::status::StatusCode::Modified,
                        worktree_status: git::status::StatusCode::Modified
                    }))
                ),
                (rel_path("b.txt").into_arc(), Some(FileStatus::Untracked)),
                (
                    rel_path("d.txt").into_arc(),
                    Some(FileStatus::Tracked(TrackedStatus {
                        index_status: git::status::StatusCode::Added,
                        worktree_status: git::status::StatusCode::Added
                    }))
                )
            ])
        );
    }

    #[gpui::test]
    async fn test_branch_diff_action_matches_existing_item_by_base_ref(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "changed",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let target_branch_diff = cx
            .update(|window, cx| {
                let Some(repository) = project.read(cx).active_repository(cx) else {
                    return Task::ready(Err(anyhow!("No active repository")));
                };
                BranchDiff::new_with_branch_base(
                    project.clone(),
                    workspace.clone(),
                    "topic".into(),
                    repository,
                    None,
                    window,
                    cx,
                )
            })
            .await
            .unwrap();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(
                Box::new(target_branch_diff.clone()),
                None,
                true,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        cx.focus(&workspace);
        cx.update(|window, cx| {
            window.dispatch_action(DeployBranchDiff.boxed_clone(), cx);
        });
        cx.run_until_parked();

        let (active_base_ref, mut base_refs) = workspace.update(cx, |workspace, cx| {
            let active_item = workspace.active_item_as::<BranchDiff>(cx).unwrap();
            let active_base_ref = match active_item.read(cx).diff_base(cx) {
                DiffBase::Merge { base_ref } => base_ref.to_string(),
                DiffBase::Head | DiffBase::Index | DiffBase::Staged => {
                    panic!("expected active item to be a branch diff")
                }
            };
            let base_refs = workspace
                .items_of_type::<BranchDiff>(cx)
                .filter_map(|item| match item.read(cx).diff_base(cx) {
                    DiffBase::Merge { base_ref } => Some(base_ref.to_string()),
                    DiffBase::Head | DiffBase::Index | DiffBase::Staged => None,
                })
                .collect::<Vec<_>>();
            (active_base_ref, base_refs)
        });
        base_refs.sort();

        assert_eq!(active_base_ref, "origin/main");
        assert_eq!(base_refs, vec!["origin/main", "topic"]);
    }
}
