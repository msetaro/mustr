//! A pull request's people and metadata, under its review's header (`mustr-docs/PARITY.md`
//! §2.11), as T3's `pullRequest/PullRequestSummaryTab.tsx` and `PullRequestDetailPanel.tsx`
//! show them:
//!
//! - **Reviewers**: who's asked to review, and a picker that asks or un-asks one at a time
//!   (`pullRequests.reviewerCandidates`, `pullRequests.requestReviewers`), as T3's
//!   `PullRequestReviewerPicker.tsx`: each pick applies at once, and the picker stays open.
//! - **Labels**: the pull request's labels, and a picker that adds or removes one at a time
//!   (`pullRequests.labelCandidates`, `pullRequests.setLabels`), as T3's
//!   `PullRequestLabelPicker.tsx`.
//! - **Edit title** (`pullRequests.update`), where the forge and the viewer allow it
//!   (`canEditPullRequestChangeRequest` in T3's `client-runtime`), as T3's
//!   `PullRequestTitleEditor.tsx`: Enter saves, Escape cancels, an empty or unchanged title
//!   saves nothing.
//! - **Stack**: the stacked pull requests it's part of (`pullRequests.stack`), top first, each
//!   opening its review, as T3's `PullRequestStackMenu.tsx` lists them.
//!
//! The reads need `orchestration:read`; the writes `source-control:write`, so dry runs refuse
//! them before they're sent.

use crate::{
    inbox::decode_each,
    pull_request_review,
    thread_git::{MenuSlot, menu_position},
};
use anyhow::Result;
use editor::Editor;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Task, WeakEntity, Window, div, px, rems,
};
use mustr_ui::{ButtonStyle, Chip, TextButton, palette, tokens, tokens::scaled};
use picker::{Picker, PickerDelegate};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use t3_client::{
    PullRequestRef,
    t3_api::{
        rpc,
        types::{
            PullRequestLabelChangeInput, PullRequestReviewerKind, PullRequestReviewerRequestInput,
            PullRequestReviewerRequestInputReviewersItem, PullRequestStack, PullRequestState,
            PullRequestUpdateInput,
        },
    },
};
use thread_chat::{Access, ThreadBackendExt as _};
use ui::{
    Color, ContextMenu, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize,
    ListItem, ListItemSpacing, Tooltip, prelude::*,
};
use workspace::{ModalView, Workspace};

// ---------------------------------------------------------------------------------------------
// What the details say

#[derive(Deserialize)]
struct Actor {
    login: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PullRequestLabel {
    pub name: String,
    /// The forge's color, as hex without `#` (`0e8a16`).
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewerCapabilities {
    #[serde(default)]
    request: bool,
    #[serde(default)]
    list_candidates: bool,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditCapabilities {
    #[serde(default)]
    change_request: bool,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    #[serde(default)]
    reviewers: Option<ReviewerCapabilities>,
    #[serde(default)]
    edit: Option<EditCapabilities>,
    #[serde(default)]
    labels: Option<bool>,
    #[serde(default)]
    stacks: Option<bool>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewerPermissions {
    #[serde(default)]
    actions: Vec<String>,
    #[serde(default)]
    request_reviewers: Option<bool>,
    #[serde(default)]
    labels: Option<bool>,
    #[serde(default)]
    edit_change_request: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Detail {
    title: String,
    #[serde(default)]
    author: Option<Actor>,
    #[serde(default)]
    viewer: Option<String>,
    #[serde(default)]
    reviewers: Option<Value>,
    #[serde(default)]
    labels: Option<Value>,
    #[serde(default)]
    capabilities: Option<Capabilities>,
    #[serde(default)]
    viewer_permissions: Option<ViewerPermissions>,
}

/// Whether a control shows, and whether the viewer may use it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Offer {
    /// The forge can't do it, so nothing shows.
    #[default]
    Hidden,
    /// The forge can, but the viewer may not: it shows, off.
    Refused,
    Allowed,
}

impl Offer {
    fn of(supported: bool, allowed: bool) -> Self {
        match (supported, allowed) {
            (false, _) => Self::Hidden,
            (true, false) => Self::Refused,
            (true, true) => Self::Allowed,
        }
    }
}

/// The pull request's people and metadata, from `pullRequests.detail`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PullRequestMetadata {
    pub title: String,
    /// Who's asked to review it.
    pub reviewers: Vec<String>,
    pub labels: Vec<PullRequestLabel>,
    /// The reviewer picker: where the forge can request reviewers and list candidates.
    pub reviewer_picker: Offer,
    /// The label picker; the labels row shows when there are labels or the forge has them.
    pub label_picker: Offer,
    pub title_editable: bool,
    /// Whether the forge has stacks, for `pullRequests.stack`.
    pub stacks: bool,
}

impl PullRequestMetadata {
    pub fn parse(detail: &Value) -> Result<Self> {
        let detail: Detail = serde_json::from_value(detail.clone())?;
        let capabilities = detail.capabilities.unwrap_or_default();
        let viewer = detail.viewer_permissions.unwrap_or_default();
        let reviewer_capabilities = capabilities.reviewers.unwrap_or_default();
        let same_login = |one: Option<&str>, other: Option<&str>| match (one, other) {
            (Some(one), Some(other)) => one.trim().eq_ignore_ascii_case(other.trim()),
            _ => false,
        };
        let title_editable = capabilities.edit.is_some_and(|edit| edit.change_request)
            && viewer.edit_change_request.unwrap_or_else(|| {
                same_login(
                    detail.viewer.as_deref(),
                    detail.author.as_ref().map(|author| author.login.as_str()),
                ) || viewer.actions.iter().any(|action| action == "merge")
            });
        Ok(Self {
            title: detail.title,
            reviewers: decode_each::<Actor>(detail.reviewers.as_ref(), "reviewer")
                .into_iter()
                .map(|actor| actor.login)
                .collect(),
            labels: decode_each(detail.labels.as_ref(), "label"),
            reviewer_picker: Offer::of(
                reviewer_capabilities.request && reviewer_capabilities.list_candidates,
                viewer.request_reviewers.unwrap_or(false),
            ),
            label_picker: Offer::of(
                capabilities.labels == Some(true),
                viewer.labels != Some(false),
            ),
            title_editable,
            stacks: capabilities.stacks == Some(true),
        })
    }
}

/// Asks or un-asks one reviewer (`pullRequests.requestReviewers`, a change rather than the
/// whole list, as T3 sends it).
pub fn request_reviewers_input(
    reference: &PullRequestRef,
    id: &str,
    kind: PullRequestReviewerKind,
    requested: bool,
) -> PullRequestReviewerRequestInput {
    PullRequestReviewerRequestInput {
        allow_stale: reference.allow_stale,
        expected_account_id: reference.expected_account_id.clone(),
        host: reference.host.clone(),
        number: reference.number,
        project_id: reference.project_id.clone(),
        repository: reference.repository.clone(),
        requested,
        reviewers: vec![PullRequestReviewerRequestInputReviewersItem {
            id: id.to_string(),
            kind,
        }],
    }
}

/// Adds or removes one label (`pullRequests.setLabels`).
pub fn set_labels_input(
    reference: &PullRequestRef,
    label: &str,
    applied: bool,
) -> PullRequestLabelChangeInput {
    PullRequestLabelChangeInput {
        allow_stale: reference.allow_stale,
        applied,
        expected_account_id: reference.expected_account_id.clone(),
        host: reference.host.clone(),
        labels: vec![label.to_string()],
        number: reference.number,
        project_id: reference.project_id.clone(),
        repository: reference.repository.clone(),
    }
}

/// Retitles the pull request (`pullRequests.update` with only a title).
pub fn update_title_input(reference: &PullRequestRef, title: &str) -> PullRequestUpdateInput {
    PullRequestUpdateInput {
        allow_stale: reference.allow_stale,
        body: None,
        expected_account_id: reference.expected_account_id.clone(),
        host: reference.host.clone(),
        number: reference.number,
        project_id: reference.project_id.clone(),
        repository: reference.repository.clone(),
        title: Some(title.to_string()),
    }
}

/// The title to save from what was typed: trimmed, and `None` when it's empty or unchanged, as
/// T3's title editor decides.
pub fn title_to_save(typed: &str, current: &str) -> Option<String> {
    let typed = typed.trim();
    (!typed.is_empty() && typed != current.trim()).then(|| typed.to_string())
}

/// A forge's hex color (`0e8a16`, with or without `#`) as a color.
fn hex_color(hex: &str) -> Option<Hsla> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex, 16)
        .ok()
        .map(|rgb| gpui::rgb(rgb).into())
}

// ---------------------------------------------------------------------------------------------
// The details row

/// The pull request's reviewers, labels, stack, and title editing, under its review's header.
pub struct PullRequestDetails {
    reference: PullRequestRef,
    metadata: PullRequestMetadata,
    review: WeakEntity<review::ReviewView>,
    workspace: WeakEntity<Workspace>,
    stack: Option<PullRequestStack>,
    menu: MenuSlot,
    _stack: Task<()>,
}

impl PullRequestDetails {
    pub fn new(
        reference: PullRequestRef,
        metadata: PullRequestMetadata,
        review: WeakEntity<review::ReviewView>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let stack = if metadata.stacks {
            Self::load_stack(&reference, cx)
        } else {
            Task::ready(())
        };
        Self {
            reference,
            metadata,
            review,
            workspace,
            stack: None,
            menu: MenuSlot::default(),
            _stack: stack,
        }
    }

    #[cfg(test)]
    pub fn metadata(&self) -> &PullRequestMetadata {
        &self.metadata
    }

    #[cfg(test)]
    pub fn stack(&self) -> Option<&PullRequestStack> {
        self.stack.as_ref()
    }

    fn load_stack(reference: &PullRequestRef, cx: &mut Context<Self>) -> Task<()> {
        let Some(backend) = thread_chat::backend(cx) else {
            return Task::ready(());
        };
        let call = backend.call::<rpc::PullRequestsStack>(reference.clone(), Access::Read);
        cx.spawn(async move |this, cx| {
            match call.await {
                Ok(stack) => {
                    this.update(cx, |this, cx| {
                        this.stack = stack.map(|stack| stack.0);
                        cx.notify();
                    })
                    .ok();
                }
                // Not in a stack, or a forge (or server) without stacks: nothing shows.
                Err(error) => log::info!("no stack for the pull request: {error:#}"),
            }
        })
    }

    /// Records a pick the forge took, so the row shows it without loading the details again.
    fn picked(&mut self, kind: CandidateKind, candidate: &Candidate, cx: &mut Context<Self>) {
        match kind {
            CandidateKind::Reviewers => {
                self.metadata
                    .reviewers
                    .retain(|login| !login.eq_ignore_ascii_case(&candidate.key));
                if candidate.chosen {
                    self.metadata.reviewers.push(candidate.key.clone());
                }
            }
            CandidateKind::Labels => {
                self.metadata
                    .labels
                    .retain(|label| label.name != candidate.key);
                if candidate.chosen {
                    self.metadata.labels.push(PullRequestLabel {
                        name: candidate.key.clone(),
                        color: candidate.color.clone(),
                    });
                }
            }
        }
        cx.notify();
    }

    fn set_title(&mut self, title: String, cx: &mut Context<Self>) {
        self.metadata.title = title.clone();
        self.review
            .update(cx, |review, cx| {
                review.update_pull_request_header(|header| header.title = title, cx)
            })
            .ok();
        cx.notify();
    }

    fn open_picker(&mut self, kind: CandidateKind, window: &mut Window, cx: &mut Context<Self>) {
        let delegate = CandidatePickerDelegate::new(kind, self.reference.clone(), cx.weak_entity());
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    Picker::uniform_list(delegate, window, cx)
                        .initial_width(tokens::modal_width(rems(28.), window))
                })
            })
            .ok();
    }

    fn open_title_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let reference = self.reference.clone();
        let title = self.metadata.title.clone();
        let details = cx.weak_entity();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    EditTitleDialog::new(reference, title, details, window, cx)
                })
            })
            .ok();
    }

    fn deploy_stack_menu(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(stack) = self.stack.clone() else {
            return;
        };
        let current = self.reference.number;
        let reference = self.reference.clone();
        let workspace = self.workspace.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            menu = menu.header(format!("Stack #{}", stack.number));
            // Top first, as T3 lists them.
            for layer in stack.layers.iter().rev() {
                let title = layer
                    .title
                    .clone()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| layer.head_branch.clone());
                let state = match &layer.state {
                    PullRequestState::Open if layer.is_draft == Some(true) => "draft",
                    PullRequestState::Open => "open",
                    PullRequestState::Merged => "merged",
                    PullRequestState::Closed => "closed",
                    PullRequestState::Unrecognized(_) => "open",
                };
                let label = format!("#{} {title} · {state}", layer.number);
                let workspace = workspace.clone();
                let mut layer_reference = reference.clone();
                layer_reference.number = layer.number;
                menu = menu.toggleable_entry(
                    label,
                    layer.number == current,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        let open = pull_request_review::open_review(
                            workspace.clone(),
                            layer_reference.clone(),
                            window,
                            cx,
                        );
                        open.detach_and_log_err(cx);
                    },
                );
            }
            menu.label(format!("↳ {}", stack.base))
        });
        self.menu
            .deploy(menu_position(event, window), menu, window, cx, |this| {
                &mut this.menu
            });
    }

    fn render_reviewers(&self, cx: &mut Context<Self>) -> AnyElement {
        let reviewers = self.metadata.reviewers.clone();
        row("Reviewers")
            .when(reviewers.is_empty(), |this| {
                this.child(
                    Label::new("None")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(reviewers.into_iter().enumerate().map(|(ix, login)| {
                Chip::new(("pull-request-reviewer", ix), format!("@{login}")).into_any_element()
            }))
            .children(self.picker_button(
                "pull-request-reviewers-edit",
                self.metadata.reviewer_picker,
                "Request or remove reviewers",
                "You can't request reviewers on this pull request",
                CandidateKind::Reviewers,
                cx,
            ))
            .into_any_element()
    }

    fn render_labels(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.metadata.labels.is_empty() && self.metadata.label_picker == Offer::Hidden {
            return None;
        }
        let palette = palette(cx);
        let labels = self.metadata.labels.clone();
        Some(
            row("Labels")
                .when(labels.is_empty(), |this| {
                    this.child(
                        Label::new("None")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .children(labels.into_iter().enumerate().map(|(ix, label)| {
                    let color = label
                        .color
                        .as_deref()
                        .and_then(hex_color)
                        .unwrap_or_else(|| palette.text_muted.hsla());
                    Chip::new(("pull-request-label", ix), label.name)
                        .dot(color)
                        .into_any_element()
                }))
                .children(self.picker_button(
                    "pull-request-labels-edit",
                    self.metadata.label_picker,
                    "Add or remove labels",
                    "You can't change this pull request's labels",
                    CandidateKind::Labels,
                    cx,
                ))
                .into_any_element(),
        )
    }

    fn picker_button(
        &self,
        id: &'static str,
        offer: Offer,
        tooltip: &'static str,
        refused: &'static str,
        kind: CandidateKind,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let blocked = thread_chat::backend(cx)
            .and_then(|backend| backend.blocked_reason(Access::SourceControlWrite));
        let (disabled, tooltip) = match (offer, blocked) {
            (Offer::Hidden, _) => return None,
            (Offer::Refused, _) => (true, refused.to_string()),
            (Offer::Allowed, Some(reason)) => (true, reason),
            (Offer::Allowed, None) => (false, tooltip.to_string()),
        };
        Some(
            IconButton::new(id, IconName::Plus)
                .icon_size(IconSize::XSmall)
                .disabled(disabled)
                .tooltip(Tooltip::text(tooltip))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.open_picker(kind, window, cx)),
                )
                .into_any_element(),
        )
    }

    fn render_stack(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let stack = self.stack.as_ref()?;
        let position = stack
            .layers
            .iter()
            .position(|layer| layer.number == self.reference.number)
            .map(|index| index + 1)?;
        Some(
            Chip::new(
                "pull-request-stack",
                format!("Stack {position} of {}", stack.layers.len()),
            )
            .menu_indicator(true)
            .tooltip(format!(
                "Stack #{}: the pull requests stacked on {}",
                stack.number, stack.base
            ))
            .on_click(
                cx.listener(|this, event, window, cx| this.deploy_stack_menu(event, window, cx)),
            )
            .into_any_element(),
        )
    }
}

/// One labeled group of the details row.
fn row(label: &'static str) -> gpui::Div {
    h_flex()
        .flex_none()
        .flex_wrap()
        .gap(scaled(px(6.)))
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
}

impl Render for PullRequestDetails {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let edit_title = self.metadata.title_editable.then(|| {
            let blocked = thread_chat::backend(cx)
                .and_then(|backend| backend.blocked_reason(Access::SourceControlWrite));
            TextButton::new("pull-request-edit-title", "Edit title")
                .disabled(blocked.is_some())
                .tooltip(blocked.unwrap_or_else(|| "Rename the pull request".to_string()))
                .on_click(cx.listener(|this, _, window, cx| this.open_title_editor(window, cx)))
                .into_any_element()
        });
        h_flex()
            .id("pull-request-details")
            .w_full()
            .flex_wrap()
            .gap_x(scaled(px(16.)))
            .gap_y(scaled(px(6.)))
            .child(self.render_reviewers(cx))
            .children(self.render_labels(cx))
            .children(self.render_stack(cx))
            .children(edit_title)
            .children(self.menu.render())
    }
}

// ---------------------------------------------------------------------------------------------
// The reviewer and label pickers

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateKind {
    Reviewers,
    Labels,
}

/// A reviewer or label the picker offers.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// The login or the label's name.
    pub key: String,
    /// The forge's id of a reviewer.
    pub id: String,
    pub reviewer_kind: Option<PullRequestReviewerKind>,
    /// A reviewer's name, or a label's description.
    pub detail: Option<String>,
    pub color: Option<String>,
    /// Asked to review, or applied.
    pub chosen: bool,
}

/// Picks reviewers or labels, one toggle at a time.
pub struct CandidatePickerDelegate {
    kind: CandidateKind,
    reference: PullRequestRef,
    details: WeakEntity<PullRequestDetails>,
    /// `None` until T3 has listed them.
    candidates: Option<Vec<Candidate>>,
    truncated: bool,
    matches: Vec<usize>,
    selected: usize,
    /// What's loading, or what went wrong.
    status: Option<SharedString>,
    /// The candidate being changed.
    in_flight: Option<usize>,
}

impl CandidatePickerDelegate {
    pub fn new(
        kind: CandidateKind,
        reference: PullRequestRef,
        details: WeakEntity<PullRequestDetails>,
    ) -> Self {
        Self {
            kind,
            reference,
            details,
            candidates: None,
            truncated: false,
            matches: Vec::new(),
            selected: 0,
            status: Some("Loading…".into()),
            in_flight: None,
        }
    }

    fn filter(&mut self, query: &str) {
        let query = query.trim().to_lowercase();
        self.matches = self
            .candidates
            .iter()
            .flatten()
            .enumerate()
            .filter(|(_, candidate)| {
                query.is_empty()
                    || candidate.key.to_lowercase().contains(&query)
                    || candidate
                        .detail
                        .as_ref()
                        .is_some_and(|detail| detail.to_lowercase().contains(&query))
            })
            .map(|(ix, _)| ix)
            .collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    fn load(&self, cx: &mut Context<Picker<Self>>) -> Task<Result<(Vec<Candidate>, bool)>> {
        let Some(backend) = thread_chat::backend(cx) else {
            return Task::ready(Err(anyhow::anyhow!("mustr isn't connected to T3")));
        };
        match self.kind {
            CandidateKind::Reviewers => {
                let call = backend.call::<rpc::PullRequestsReviewerCandidates>(
                    self.reference.clone(),
                    Access::Read,
                );
                cx.background_spawn(async move {
                    let list = call.await?;
                    let candidates = list
                        .candidates
                        .into_iter()
                        .map(|candidate| Candidate {
                            key: candidate.login,
                            id: candidate.id,
                            reviewer_kind: Some(candidate.kind),
                            detail: candidate.name,
                            color: None,
                            chosen: candidate.is_requested,
                        })
                        .collect();
                    Ok((candidates, list.truncated))
                })
            }
            CandidateKind::Labels => {
                let call = backend
                    .call::<rpc::PullRequestsLabelCandidates>(self.reference.clone(), Access::Read);
                cx.background_spawn(async move {
                    let list = call.await?;
                    let candidates = list
                        .candidates
                        .into_iter()
                        .map(|candidate| Candidate {
                            id: candidate.name.clone(),
                            key: candidate.name,
                            reviewer_kind: None,
                            detail: candidate.description,
                            color: candidate.color,
                            chosen: candidate.is_applied,
                        })
                        .collect();
                    Ok((candidates, list.truncated))
                })
            }
        }
    }

    /// Asks or un-asks the reviewer, or adds or removes the label, at `ix`.
    pub fn toggle(&mut self, ix: usize, cx: &mut Context<Picker<Self>>) {
        if self.in_flight.is_some() {
            return;
        }
        let Some(candidate) = self
            .candidates
            .as_ref()
            .and_then(|all| all.get(ix))
            .cloned()
        else {
            return;
        };
        let Some(backend) = thread_chat::backend(cx) else {
            return;
        };
        let choose = !candidate.chosen;
        let call = match (self.kind, candidate.reviewer_kind.clone()) {
            (CandidateKind::Reviewers, Some(kind)) => backend
                .call::<rpc::PullRequestsRequestReviewers>(
                    request_reviewers_input(&self.reference, &candidate.id, kind, choose),
                    Access::SourceControlWrite,
                ),
            (CandidateKind::Labels, _) => backend.call::<rpc::PullRequestsSetLabels>(
                set_labels_input(&self.reference, &candidate.key, choose),
                Access::SourceControlWrite,
            ),
            (CandidateKind::Reviewers, None) => return,
        };
        self.in_flight = Some(ix);
        self.status = None;
        cx.notify();
        let kind = self.kind;
        cx.spawn(async move |picker, cx| {
            let result = call.await;
            picker
                .update(cx, |picker, cx| {
                    let delegate = &mut picker.delegate;
                    delegate.in_flight = None;
                    match result {
                        Ok(()) => {
                            let Some(changed) =
                                delegate.candidates.as_mut().and_then(|all| all.get_mut(ix))
                            else {
                                return;
                            };
                            changed.chosen = choose;
                            let changed = changed.clone();
                            delegate
                                .details
                                .update(cx, |details, cx| details.picked(kind, &changed, cx))
                                .ok();
                        }
                        Err(error) => {
                            let what = match (kind, choose) {
                                (CandidateKind::Reviewers, true) => "request a review from",
                                (CandidateKind::Reviewers, false) => "remove",
                                (CandidateKind::Labels, true) => "add",
                                (CandidateKind::Labels, false) => "remove",
                            };
                            log::error!("couldn't {what} {}: {error:#}", candidate.key);
                            delegate.status = Some(
                                format!("Couldn't {what} {}: {error:#}", candidate.key).into(),
                            );
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    #[cfg(test)]
    pub fn candidates(&self) -> Option<&[Candidate]> {
        self.candidates.as_deref()
    }

    #[cfg(test)]
    pub fn status(&self) -> Option<&SharedString> {
        self.status.as_ref()
    }
}

impl PickerDelegate for CandidatePickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "pull request candidate picker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        match self.kind {
            CandidateKind::Reviewers => "Request a review from…".into(),
            CandidateKind::Labels => "Add or remove a label…".into(),
        }
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some(self.status.clone().unwrap_or_else(|| match self.kind {
            CandidateKind::Reviewers => "No one matches".into(),
            CandidateKind::Labels => "No labels match".into(),
        }))
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        if self.candidates.is_some() {
            self.filter(&query);
            return Task::ready(());
        }
        let load = self.load(cx);
        cx.spawn_in(window, async move |picker, cx| {
            let loaded = load.await;
            picker
                .update(cx, |picker, cx| {
                    let delegate = &mut picker.delegate;
                    match loaded {
                        Ok((candidates, truncated)) => {
                            delegate.candidates = Some(candidates);
                            delegate.truncated = truncated;
                            delegate.status = None;
                        }
                        Err(error) => {
                            delegate.candidates = Some(Vec::new());
                            delegate.status = Some(format!("Couldn't load them: {error:#}").into());
                        }
                    }
                    delegate.filter(&query);
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        // Each pick applies at once, and the picker stays open for the next, as in T3.
        if let Some(&ix) = self.matches.get(self.selected) {
            self.toggle(ix, cx);
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_header(&self, _: &mut Window, _: &mut Context<Picker<Self>>) -> Option<AnyElement> {
        let note = match (&self.status, self.candidates.as_ref()) {
            (Some(status), Some(candidates)) if !candidates.is_empty() => Some(status.clone()),
            _ if self.truncated => Some("T3 listed the first 100; type to narrow them".into()),
            _ => None,
        }?;
        Some(
            div()
                .px_2()
                .py_1()
                .child(Label::new(note).size(LabelSize::Small).color(Color::Muted))
                .into_any_element(),
        )
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let index = *self.matches.get(ix)?;
        let candidate = self.candidates.as_ref()?.get(index)?;
        let palette = palette(cx);
        let busy = self.in_flight == Some(index);
        let title = match self.kind {
            CandidateKind::Reviewers => format!("@{}", candidate.key),
            CandidateKind::Labels => candidate.key.clone(),
        };
        let dot = candidate
            .color
            .as_deref()
            .and_then(hex_color)
            .map(|color| div().size(scaled(px(8.))).rounded_full().bg(color));
        let check = if busy {
            Icon::new(IconName::ArrowCircle)
                .size(IconSize::Small)
                .color(Color::Muted)
        } else if candidate.chosen {
            Icon::new(IconName::Check)
                .size(IconSize::Small)
                .color(Color::Accent)
        } else {
            Icon::new(IconName::Check)
                .size(IconSize::Small)
                .color(Color::Hidden)
        };
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(check)
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .children(dot)
                        .child(Label::new(title).truncate())
                        .when(
                            candidate.reviewer_kind == Some(PullRequestReviewerKind::Team),
                            |this| {
                                this.child(
                                    Label::new("team")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            },
                        )
                        .children(candidate.detail.clone().map(|detail| {
                            div().min_w_0().flex_1().child(
                                Label::new(detail)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        }))
                        .when(candidate.chosen, |this| {
                            this.child(
                                div()
                                    .flex_none()
                                    .text_color(palette.text_muted.hsla())
                                    .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                                    .child(match self.kind {
                                        CandidateKind::Reviewers => "requested",
                                        CandidateKind::Labels => "applied",
                                    }),
                            )
                        }),
                ),
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Editing the title

/// Renames the pull request (T3's `PullRequestTitleEditor.tsx`, as a dialog).
pub struct EditTitleDialog {
    reference: PullRequestRef,
    current: String,
    details: WeakEntity<PullRequestDetails>,
    editor: Entity<Editor>,
    error: Option<SharedString>,
    saving: bool,
    _task: Option<Task<()>>,
}

impl EditTitleDialog {
    pub fn new(
        reference: PullRequestRef,
        current: String,
        details: WeakEntity<PullRequestDetails>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(current.clone(), window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });
        Self {
            reference,
            current,
            details,
            editor,
            error: None,
            saving: false,
            _task: None,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let Some(title) = title_to_save(&self.editor.read(cx).text(cx), &self.current) else {
            cx.emit(DismissEvent);
            return;
        };
        let Some(backend) = thread_chat::backend(cx) else {
            self.error = Some("mustr isn't connected to T3".into());
            cx.notify();
            return;
        };
        let call = backend.call::<rpc::PullRequestsUpdate>(
            update_title_input(&self.reference, &title),
            Access::SourceControlWrite,
        );
        self.saving = true;
        self.error = None;
        cx.notify();
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(()) => {
                        this.details
                            .update(cx, |details, cx| details.set_title(title, cx))
                            .ok();
                        cx.emit(DismissEvent);
                    }
                    Err(error) => {
                        log::error!("couldn't rename the pull request: {error:#}");
                        this.error = Some(format!("Couldn't save the title: {error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    #[cfg(test)]
    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }
}

impl EventEmitter<DismissEvent> for EditTitleDialog {}

impl ModalView for EditTitleDialog {}

impl Focusable for EditTitleDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for EditTitleDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        v_flex()
            .key_context("EditPullRequestTitle")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(tokens::modal_width(rems(34.), window))
            .p_4()
            .gap_3()
            .rounded(scaled(tokens::CORNER_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .shadow_lg()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new("Edit title")
                            .size(LabelSize::Large)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(pull_request_review::display_ref(&self.reference))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1p5()
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .border(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .bg(palette.background.hsla())
                    .child(self.editor.clone()),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        TextButton::new("edit-title-cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        TextButton::new(
                            "edit-title-save",
                            if self.saving { "Saving…" } else { "Save" },
                        )
                        .style(ButtonStyle::Primary)
                        .disabled(self.saving)
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
    }
}

#[cfg(test)]
#[path = "pull_request_details_tests.rs"]
mod tests;
