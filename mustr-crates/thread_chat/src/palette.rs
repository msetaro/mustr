//! What T3's command palette offers that Zed's palette didn't have yet
//! (`components/CommandPalette.tsx` in T3): "New thread in <project>" for every project, "New
//! thread without a project", "Find in current thread", and "Copy thread ID".
//!
//! The per-project entries can't be plain actions, since Zed's palette lists each action once, so
//! they're the palette's intercepted results ([`palette_results`]): the app shows them ahead of
//! Zed's commands (`mustr/src/hidden_actions.rs`) when the query names "new" or a project, the
//! project of the open thread first, as T3 lists it. The rest are actions.

use crate::{shell::ThreadShells, thread_view::ThreadView};
use gpui::{Action, App, ClipboardItem, Entity, WeakEntity, actions};
use schemars::JsonSchema;
use serde::Deserialize;
use workspace::Workspace;

actions!(
    thread_chat,
    [
        /// Opens a new agent thread without a project, in a folder of its own in T3's scratch
        /// directory.
        NewThreadWithoutProject,
        /// Finds text in the open agent thread.
        FindInThread,
        /// Copies the open agent thread's ID.
        CopyThreadId,
    ]
);

/// Opens a new agent thread in a project, working where T3's settings say new threads work.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema, Action)]
#[action(namespace = thread_chat)]
#[serde(deny_unknown_fields)]
pub struct NewThreadInProject {
    pub project_id: String,
}

/// One of mustr's own results in the command palette.
#[derive(Debug)]
pub struct PaletteResult {
    pub action: Box<dyn Action>,
    pub label: String,
    /// The byte offsets of the label's characters that match the query, to highlight.
    pub positions: Vec<usize>,
}

/// The fewest characters a query has before the palette offers new threads, so a first keystroke
/// doesn't list every project ahead of Zed's commands.
const MIN_QUERY_CHARS: usize = 2;

/// The labels' namespace, as the palette names mustr's other thread actions.
const NAMESPACE: &str = "thread chat: ";

/// mustr's results for the palette's `query`: "New thread in <project>" for each project the
/// query matches (each of its words starts a word of the label, in order, and one of them is
/// "new" or the project's), the project of `workspace`'s open thread first, then by name.
pub fn palette_results(
    query: &str,
    workspace: &WeakEntity<Workspace>,
    cx: &mut App,
) -> Vec<PaletteResult> {
    if query.trim().chars().count() < MIN_QUERY_CHARS || crate::backend(cx).is_none() {
        return Vec::new();
    }
    let shells = ThreadShells::global(cx);
    let current = workspace
        .upgrade()
        .and_then(|workspace| current_project(workspace.read(cx), &shells, cx));
    let Some(shell) = shells.read(cx).shell() else {
        return Vec::new();
    };
    let mut projects = shell
        .projects
        .values()
        .filter(|project| project.title != crate::new_thread::SCRATCH_PROJECT_TITLE)
        .map(|project| (project.id.clone(), project.title.clone()))
        .collect::<Vec<_>>();
    projects.sort_by_cached_key(|(id, title)| {
        (
            current.as_deref() != Some(id.as_str()),
            title.to_lowercase(),
            id.clone(),
        )
    });
    projects
        .into_iter()
        .filter_map(|(project_id, title)| {
            let label = format!("{NAMESPACE}new thread in {title}");
            let title_start = label.len() - title.len();
            let positions = match_words(&label, query, |start| {
                start == NAMESPACE.len() || start >= title_start
            })?;
            Some(PaletteResult {
                action: Box::new(NewThreadInProject { project_id }),
                label,
                positions,
            })
        })
        .collect()
}

/// The project of the thread or draft `workspace` shows.
fn current_project(
    workspace: &Workspace,
    shells: &Entity<ThreadShells>,
    cx: &App,
) -> Option<String> {
    let view = workspace.active_item(cx)?.downcast::<ThreadView>()?;
    let view = view.read(cx);
    if let Some(form) = &view.new_thread {
        return form.read(cx).selected_project_id().map(str::to_string);
    }
    let thread = shells.read(cx).thread(view.thread_id()?)?;
    Some(thread.project_id.clone())
}

/// Where `query` matches `label`: each of its words, in order, starts one of the label's words
/// (ignoring case), and at least one of those is a word `required` takes (by its byte offset),
/// so the label's namespace alone doesn't match. Gives the byte offsets of the matching
/// characters.
pub(crate) fn match_words(
    label: &str,
    query: &str,
    required: impl Fn(usize) -> bool,
) -> Option<Vec<usize>> {
    let words = label
        .split_whitespace()
        .map(|word| {
            let start = word.as_ptr() as usize - label.as_ptr() as usize;
            (start, word)
        })
        .collect::<Vec<_>>();
    let mut next = 0;
    let mut positions = Vec::new();
    let mut matched_required = false;
    for query_word in query.split_whitespace() {
        let query_word = query_word.to_lowercase();
        let (index, (start, word)) = words
            .iter()
            .enumerate()
            .skip(next)
            .find(|(_, (_, word))| word.to_lowercase().starts_with(&query_word))?;
        matched_required |= required(*start);
        positions.extend(
            word.char_indices()
                .take(query_word.chars().count())
                .map(|(offset, _)| start + offset),
        );
        next = index + 1;
    }
    (matched_required && !positions.is_empty()).then_some(positions)
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| register(workspace))
        .detach();
}

fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, action: &NewThreadInProject, window, cx| {
        let Some(view) = ThreadView::open_new(workspace, window, cx) else {
            log::error!("couldn't open a new thread: mustr isn't connected to T3");
            return;
        };
        if let Some(form) = view.read(cx).new_thread.clone() {
            let project_id = action.project_id.clone();
            form.update(cx, |form, cx| form.start_in_project(project_id, cx));
        }
    });
    workspace.register_action(|workspace, _: &NewThreadWithoutProject, window, cx| {
        let Some(view) = ThreadView::open_new(workspace, window, cx) else {
            log::error!("couldn't open a new thread: mustr isn't connected to T3");
            return;
        };
        if let Some(form) = view.read(cx).new_thread.clone() {
            form.update(cx, |form, cx| form.select_scratch(cx));
        }
    });
    workspace.register_action(|workspace, _: &FindInThread, window, cx| {
        let shows_thread = workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<ThreadView>())
            .is_some_and(|view| view.read(cx).thread_id().is_some());
        if shows_thread {
            window.dispatch_action(Box::new(zed_actions::buffer_search::Deploy::find()), cx);
        } else {
            cx.propagate();
        }
    });
    workspace.register_action(|workspace, _: &CopyThreadId, _, cx| {
        let thread_id = workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<ThreadView>())
            .and_then(|view| view.read(cx).thread_id().map(str::to_string));
        match thread_id {
            Some(thread_id) => cx.write_to_clipboard(ClipboardItem::new_string(thread_id)),
            None => cx.propagate(),
        }
    });
}
