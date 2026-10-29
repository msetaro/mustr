//! T3's agent worktrees inherit Zed's trust from their project's checkout.
//!
//! Zed's Restricted Mode keeps language servers and project settings out of a folder until the
//! user trusts it (`crates/project/src/trusted_worktrees.rs`). A T3 worktree is a checkout of its
//! project's repository, so when Zed trusts the project's checkout on this machine (T3's
//! `workspaceRoot`), mustr trusts the project's worktrees as it opens them. When the checkout is
//! trusted later (from Restricted Mode or anywhere), the worktrees already open follow at once.
//!
//! A folder is trusted this way only when all of these hold:
//! - T3 runs on this machine ([`worktrees_are_local`]), and the folder is open in a local project.
//! - T3's shell ([`ThreadShells`]) has a thread of the project whose `worktreePath` is the folder.
//! - Zed trusts the project's checkout: it, or a folder holding it, was trusted before launch
//!   (Zed's database) or since (Zed's trust events).
//! - The folder isn't the checkout or a folder holding it, and it's a linked git worktree (its
//!   `.git` is a file), as T3's worktrees are.
//!
//! As with any folder trusted in Zed, Zed then remembers the worktree as trusted.
//!
//! The review's Restricted Mode button ([`TrustThreadWorktree`]) asks whether to trust the
//! thread's project, which covers its agent worktrees, or only this worktree.

use crate::worktrees::{display_path, normalize_worktree_path, worktrees_are_local};
use anyhow::Context as _;
use collections::HashSet;
use fs::Fs;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, PromptButton, PromptLevel, Subscription,
    WeakEntity, Window,
};
use project::{
    Project, WorktreeId,
    project_settings::ProjectSettings,
    trusted_worktrees::{
        DbTrustedPaths, PathTrust, TrustedWorktrees, TrustedWorktreesEvent, TrustedWorktreesStore,
    },
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};
use review::TrustThreadWorktree;
use settings::Settings as _;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use thread_chat::{GlobalThreadBackend, Shell, ThreadShells};
use util::ResultExt as _;
use workspace::Workspace;

pub fn init(cx: &mut App) {
    let trusted = workspace::WorkspaceDb::global(cx)
        .fetch_trusted_worktrees()
        .context("fetching trusted worktrees")
        .log_err()
        .unwrap_or_default();
    init_with(trusted, cx);
}

/// Starts following Zed's trust and the projects' worktrees, with the folders Zed's database
/// trusted at launch. Call after `trusted_worktrees::init`.
fn init_with(db_trusted: DbTrustedPaths, cx: &mut App) {
    let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
        return;
    };
    // Kept alive by the observer, for the app's lifetime.
    let inheritance = cx.new(|cx| TrustInheritance::new(trusted_worktrees, db_trusted, cx));
    cx.observe_new(move |project: &mut Project, _, cx| {
        if project.is_local() {
            let worktree_store = project.worktree_store();
            let fs = project.fs().clone();
            inheritance.update(cx, |inheritance, cx| {
                inheritance.track(worktree_store, fs, cx)
            });
        }
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, action: &TrustThreadWorktree, window, cx| {
            ask_to_trust(workspace, &action.thread_id, window, cx);
        });
    })
    .detach();
}

/// Follows what Zed trusts and the worktrees local projects open, and trusts the T3 worktrees of
/// trusted projects.
struct TrustInheritance {
    trusted_worktrees: Entity<TrustedWorktreesStore>,
    /// Local folders Zed trusts, with everything in them: those trusted before launch, and those
    /// trusted since.
    trusted: HashSet<PathBuf>,
    stores: Vec<TrackedStore>,
    /// T3's shell, once the backend is installed and a worktree needed it.
    shells: Option<Entity<ThreadShells>>,
    /// Worktrees whose `.git` is being checked, and those it showed aren't linked git worktrees,
    /// by their store and id.
    checking: HashSet<(EntityId, WorktreeId)>,
    not_linked: HashSet<(EntityId, WorktreeId)>,
    _subscriptions: Vec<Subscription>,
}

struct TrackedStore {
    store: WeakEntity<WorktreeStore>,
    fs: Arc<dyn Fs>,
    _subscription: Subscription,
}

impl TrustInheritance {
    fn new(
        trusted_worktrees: Entity<TrustedWorktreesStore>,
        mut db_trusted: DbTrustedPaths,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(
                &trusted_worktrees,
                |this, _, event: &TrustedWorktreesEvent, cx| match event {
                    TrustedWorktreesEvent::Trusted(store, paths) => {
                        this.record_trusted(store, paths, cx);
                        this.refresh(cx);
                    }
                    // Zed restricts a worktree once, when it first checks it: T3's shell may
                    // have loaded since the worktree was added.
                    TrustedWorktreesEvent::Restricted(store, paths) => {
                        let Some(store) = store.upgrade().filter(|store| this.is_tracked(store))
                        else {
                            return;
                        };
                        for path in paths {
                            if let PathTrust::Worktree(worktree_id) = path {
                                this.consider(&store, *worktree_id, cx);
                            }
                        }
                    }
                },
            ),
            // The shell follows T3 only once the backend is installed, after the app's views.
            cx.observe_global::<GlobalThreadBackend>(|this, cx| this.refresh(cx)),
        ];
        Self {
            trusted_worktrees,
            // `None` is this machine; other hosts are remote projects', which T3 doesn't use.
            trusted: db_trusted.remove(&None).unwrap_or_default(),
            stores: Vec::new(),
            shells: None,
            checking: HashSet::default(),
            not_linked: HashSet::default(),
            _subscriptions: subscriptions,
        }
    }

    fn track(&mut self, store: Entity<WorktreeStore>, fs: Arc<dyn Fs>, cx: &mut Context<Self>) {
        self.stores.retain(|tracked| tracked.store.is_upgradable());
        let subscription = cx.subscribe(&store, |this, store, event, cx| match event {
            // Before Zed checks the new worktree's trust, so the check starts early.
            WorktreeStoreEvent::WorktreeAdded(worktree) => {
                let worktree_id = worktree.read(cx).id();
                this.consider(&store, worktree_id, cx);
            }
            WorktreeStoreEvent::WorktreeRemoved(_, worktree_id) => {
                let key = (store.entity_id(), *worktree_id);
                this.not_linked.remove(&key);
            }
            _ => {}
        });
        self.stores.push(TrackedStore {
            store: store.downgrade(),
            fs,
            _subscription: subscription,
        });
    }

    /// Notes the folders a local project's trust covers now.
    fn record_trusted(
        &mut self,
        store: &WeakEntity<WorktreeStore>,
        paths: &HashSet<PathTrust>,
        cx: &App,
    ) {
        let Some(store) = store.upgrade() else {
            return;
        };
        if !self.is_tracked(&store) {
            return;
        }
        for path in paths {
            match path {
                PathTrust::Worktree(worktree_id) => {
                    if let Some(worktree) = store.read(cx).worktree_for_id(*worktree_id, cx) {
                        let worktree = worktree.read(cx);
                        if worktree.is_local() && !worktree.is_single_file() {
                            self.trusted.insert(worktree.abs_path().to_path_buf());
                        }
                    }
                }
                PathTrust::AbsPath(path) => {
                    self.trusted.insert(path.clone());
                }
            }
        }
    }

    fn is_tracked(&self, store: &Entity<WorktreeStore>) -> bool {
        self.stores
            .iter()
            .any(|tracked| tracked.store == store.downgrade())
    }

    /// Looks again at the worktrees Zed restricts, after what's trusted or T3's shell changed.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.stores.retain(|tracked| tracked.store.is_upgradable());
        let restricted = self
            .stores
            .iter()
            .filter_map(|tracked| tracked.store.upgrade())
            .flat_map(|store| {
                self.trusted_worktrees
                    .read(cx)
                    .restricted_worktrees(&store, cx)
                    .into_iter()
                    .map(move |(worktree_id, _)| (store.clone(), worktree_id))
            })
            .collect::<Vec<_>>();
        for (store, worktree_id) in restricted {
            self.consider(&store, worktree_id, cx);
        }
    }

    /// Trusts a worktree when it's the T3 worktree of a trusted project, once its `.git` shows
    /// it's a linked git worktree.
    fn consider(
        &mut self,
        store: &Entity<WorktreeStore>,
        worktree_id: WorktreeId,
        cx: &mut Context<Self>,
    ) {
        let key = (store.entity_id(), worktree_id);
        if self.checking.contains(&key) || self.not_linked.contains(&key) {
            return;
        }
        let Some(folder) = self.inheriting_folder(store, worktree_id, cx) else {
            return;
        };
        let Some(fs) = self
            .stores
            .iter()
            .find(|tracked| tracked.store == store.downgrade())
            .map(|tracked| tracked.fs.clone())
        else {
            return;
        };
        self.checking.insert(key);
        let store = store.downgrade();
        cx.spawn(async move |this, cx| {
            let linked = fs.is_file(&folder.join(".git")).await;
            this.update(cx, |this, cx| {
                this.checking.remove(&key);
                if !linked {
                    log::info!(
                        "not trusting {}: it isn't a linked git worktree",
                        folder.display()
                    );
                    this.not_linked.insert(key);
                    return;
                }
                let Some(store) = store.upgrade() else {
                    return;
                };
                // What's trusted, or T3's shell, may have changed meanwhile.
                if this.inheriting_folder(&store, worktree_id, cx).as_ref() != Some(&folder) {
                    return;
                }
                log::info!(
                    "trusting {}: it's a T3 worktree of a trusted project",
                    folder.display()
                );
                this.trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                    trusted_worktrees.trust(
                        &store,
                        HashSet::from_iter([PathTrust::Worktree(worktree_id)]),
                        cx,
                    )
                });
            })
            .ok();
        })
        .detach();
    }

    /// The worktree's folder, when it should inherit its project's trust (all but the `.git`
    /// check).
    fn inheriting_folder(
        &mut self,
        store: &Entity<WorktreeStore>,
        worktree_id: WorktreeId,
        cx: &mut Context<Self>,
    ) -> Option<PathBuf> {
        // Zed trusts every folder already.
        if ProjectSettings::get_global(cx).session.trust_all_worktrees || !worktrees_are_local(cx) {
            return None;
        }
        let folder = {
            let worktree = store.read(cx).worktree_for_id(worktree_id, cx)?;
            let worktree = worktree.read(cx);
            // Zed doesn't restrict invisible worktrees, and single files follow their folders.
            if !worktree.is_visible() || worktree.is_single_file() || !worktree.is_local() {
                return None;
            }
            worktree.abs_path().to_path_buf()
        };
        let shells = self.shells(cx)?;
        let shell = shells.read(cx).shell()?;
        inherits_trust(&folder, shell, &self.trusted).then_some(folder)
    }

    fn shells(&mut self, cx: &mut Context<Self>) -> Option<Entity<ThreadShells>> {
        if self.shells.is_none() && thread_chat::backend(cx).is_some() {
            let shells = ThreadShells::global(cx);
            self._subscriptions
                .push(cx.observe(&shells, |this, _, cx| this.refresh(cx)));
            self.shells = Some(shells);
        }
        self.shells.clone()
    }
}

/// Whether `folder` is the worktree of one of the shell's threads whose project's checkout Zed
/// trusts (`trusted` holds it).
fn inherits_trust(folder: &Path, shell: &Shell, trusted: &HashSet<PathBuf>) -> bool {
    shell.threads.values().any(|thread| {
        normalize_worktree_path(thread.worktree_path.as_deref())
            .is_some_and(|path| Path::new(path) == folder)
            && shell
                .projects
                .get(&thread.project_id)
                .and_then(|project| checkout_of(&project.workspace_root))
                .is_some_and(|checkout| {
                    is_separate_worktree(folder, &checkout)
                        && trusted.iter().any(|path| checkout.starts_with(path))
                })
    })
}

/// A project's checkout, from T3's `workspaceRoot`, when it's an absolute path.
fn checkout_of(workspace_root: &str) -> Option<PathBuf> {
    normalize_worktree_path(Some(workspace_root))
        .map(PathBuf::from)
        .filter(|checkout| checkout.is_absolute())
}

/// Whether trusting `folder` can't take the project's checkout or anything around it along:
/// Zed trusts what's in a trusted folder too, so a filesystem root, the checkout, or a folder
/// that holds it never inherits.
fn is_separate_worktree(folder: &Path, checkout: &Path) -> bool {
    folder.is_absolute() && folder.parent().is_some() && !checkout.starts_with(folder)
}

/// What the Restricted Mode button of a thread's review offers to trust.
struct TrustQuestion {
    project_title: String,
    checkout: PathBuf,
    store: Entity<WorktreeStore>,
    /// The restricted worktree the review shows.
    worktree_id: WorktreeId,
    /// Whether that's a T3 worktree, rather than the project's checkout.
    is_agent_worktree: bool,
}

impl TrustQuestion {
    fn new(workspace: &Workspace, thread_id: &str, cx: &mut App) -> Option<Self> {
        if !worktrees_are_local(cx) || thread_chat::backend(cx).is_none() {
            return None;
        }
        let shells = ThreadShells::global(cx);
        let shell = shells.read(cx).shell()?;
        let thread = shell.threads.get(thread_id)?;
        let project = shell.projects.get(&thread.project_id)?;
        let checkout = checkout_of(&project.workspace_root)?;
        let folder = match normalize_worktree_path(thread.worktree_path.as_deref()) {
            Some(path) => PathBuf::from(path),
            None => checkout.clone(),
        };
        let is_agent_worktree = folder != checkout;
        if is_agent_worktree && !is_separate_worktree(&folder, &checkout) {
            return None;
        }
        let local_project = workspace.project().read(cx);
        if !local_project.is_local() {
            return None;
        }
        let store = local_project.worktree_store();
        let trusted_worktrees = TrustedWorktrees::try_get_global(cx)?;
        let worktree_id = trusted_worktrees
            .read(cx)
            .restricted_worktrees(&store, cx)
            .into_iter()
            .find(|(_, path)| path.as_ref() == folder)
            .map(|(worktree_id, _)| worktree_id)?;
        Some(Self {
            project_title: project.title.clone(),
            checkout,
            store,
            worktree_id,
            is_agent_worktree,
        })
    }

    fn title(&self) -> String {
        format!("Trust “{}”?", self.project_title)
    }

    fn detail(&self) -> String {
        let checkout = display_path(&self.checkout);
        if self.is_agent_worktree {
            format!(
                "This agent worktree is in Restricted Mode: language servers and project settings \
                 are off in it. Trusting the project ({checkout}) also trusts its agent \
                 worktrees: this one now, and the others when you open them."
            )
        } else {
            format!(
                "{checkout} is in Restricted Mode: language servers and project settings are off \
                 in it. Trusting the project also trusts its agent worktrees when you open them."
            )
        }
    }

    fn answers(&self) -> Vec<PromptButton> {
        let mut answers = vec![PromptButton::ok("Trust Project")];
        if self.is_agent_worktree {
            answers.push(PromptButton::new("Trust Only This Worktree"));
        }
        answers.push(PromptButton::cancel("Stay in Restricted Mode"));
        answers
    }

    /// Trusts what the answer (by its index in [`Self::answers`]) chose.
    fn answer(self, answer: usize, cx: &mut App) {
        let worktree = PathTrust::Worktree(self.worktree_id);
        let paths = match answer {
            0 => HashSet::from_iter([PathTrust::AbsPath(self.checkout), worktree]),
            1 if self.is_agent_worktree => HashSet::from_iter([worktree]),
            _ => return,
        };
        if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
            trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                trusted_worktrees.trust(&self.store, paths, cx)
            });
        }
    }
}

/// Asks whether to trust the thread's project (and with it its agent worktrees) or only the
/// worktree. Zed's own dialog opens instead when the review's folder isn't one mustr can place.
fn ask_to_trust(
    workspace: &mut Workspace,
    thread_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(question) = TrustQuestion::new(workspace, thread_id, cx) else {
        workspace.show_worktree_trust_security_modal(true, window, cx);
        return;
    };
    let answer = window.prompt(
        PromptLevel::Warning,
        &question.title(),
        Some(&question.detail()),
        &question.answers(),
        cx,
    );
    cx.spawn(async move |_, cx| {
        if let Ok(answer) = answer.await {
            cx.update(|cx| question.answer(answer, cx));
        }
    })
    .detach();
}

#[cfg(test)]
#[path = "worktree_trust_tests.rs"]
mod tests;
