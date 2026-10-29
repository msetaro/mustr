use super::*;
use crate::{
    thread_git::tests::{NOW, RecordingBackend, THREAD, WORKTREE, init_test, shell_thread},
    worktrees::LocalFolders,
};
use collections::HashMap;
use gpui::{TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use project::{FakeFs, trusted_worktrees};
use serde_json::{Value, json};
use workspace::{MultiWorkspace, security_modal::SecurityModal};

/// `project-1`'s checkout; `THREAD` works in its worktree, `WORKTREE`.
const CHECKOUT: &str = "/src/api";
/// `project-2`'s checkout and the worktree of its thread, `thread-2`.
const WEB_CHECKOUT: &str = "/src/web";
const WEB_WORKTREE: &str = "/tmp/worktrees/web-redesign";
/// A linked worktree of `project-1` that no thread works in.
const STRAY: &str = "/tmp/worktrees/stray";
/// `thread-5`'s folder, which isn't a linked git worktree.
const NOTES: &str = "/tmp/scratch/notes";

fn project(id: &str, title: &str, root: &str) -> Value {
    json!({
        "id": id, "title": title, "workspaceRoot": root, "scripts": [],
        "createdAt": NOW, "updatedAt": NOW, "defaultModelSelection": null
    })
}

fn shell_json() -> Value {
    json!({
        "schemaVersion": 1,
        "snapshotSequence": 7,
        "threads": [
            shell_thread(json!({})),
            shell_thread(json!({
                "id": "thread-2", "projectId": "project-2", "worktreePath": WEB_WORKTREE
            })),
            // In its project's checkout.
            shell_thread(json!({ "id": "thread-3", "worktreePath": null })),
            // In a folder that holds the checkout.
            shell_thread(json!({ "id": "thread-4", "worktreePath": "/src" })),
            shell_thread(json!({ "id": "thread-5", "worktreePath": NOTES })),
        ],
        "archivedThreads": [],
        "projects": [
            project("project-1", "api", CHECKOUT),
            project("project-2", "web", WEB_CHECKOUT),
        ]
    })
}

fn shell() -> Shell {
    Shell::from_snapshot(&serde_json::from_value(shell_json()).unwrap())
}

fn trusted(paths: &[&str]) -> HashSet<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

/// Sets up mustr with T3's shell and Zed's trust, with `trusted` as the folders Zed's database
/// trusted at launch. `local` says whether T3's folders are on this machine.
async fn setup(trusted_at_launch: &[&str], local: bool, cx: &mut TestAppContext) -> Arc<FakeFs> {
    init_test(RecordingBackend::with_shell(shell_json()), cx);
    let db_trusted: DbTrustedPaths = HashMap::from_iter([(None, trusted(trusted_at_launch))]);
    cx.update(|cx| {
        cx.set_global(LocalFolders(if local { |_| true } else { |_| false }));
        trusted_worktrees::init(db_trusted.clone(), cx);
        init_with(db_trusted, cx);
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/src",
        json!({
            "api": { ".git": {}, "src": { "lib.rs": "" } },
            "web": { ".git": {}, "index.ts": "" },
        }),
    )
    .await;
    fs.insert_tree(
        "/tmp",
        json!({
            "worktrees": {
                "retry-backoff": { ".git": "gitdir: /src/api/.git/worktrees/retry-backoff\n", "src": { "lib.rs": "" } },
                "web-redesign": { ".git": "gitdir: /src/web/.git/worktrees/web-redesign\n" },
                "stray": { ".git": "gitdir: /src/api/.git/worktrees/stray\n" },
            },
            "scratch": { "notes": { "todo.md": "" } },
        }),
    )
    .await;
    fs
}

async fn new_project(fs: &Arc<FakeFs>, cx: &mut TestAppContext) -> Entity<Project> {
    Project::test_with_worktree_trust(fs.clone(), [], cx).await
}

/// Opens a folder in the project, visible, as the review and the worktrees page do.
async fn open(project: &Entity<Project>, path: &str, cx: &mut TestAppContext) -> WorktreeId {
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path, true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    worktree.read_with(cx, |worktree, _| worktree.id())
}

/// Whether Zed lets language servers run in the worktree, as it asks before starting one.
fn is_trusted(project: &Entity<Project>, worktree_id: WorktreeId, cx: &mut TestAppContext) -> bool {
    let store = project.read_with(cx, |project, _| project.worktree_store());
    cx.update(|cx| {
        TrustedWorktrees::try_get_global(cx)
            .unwrap()
            .update(cx, |trusted_worktrees, cx| {
                trusted_worktrees.can_trust(&store, worktree_id, cx)
            })
    })
}

fn trust(project: &Entity<Project>, path: PathTrust, cx: &mut TestAppContext) {
    let store = project.read_with(cx, |project, _| project.worktree_store());
    cx.update(|cx| {
        TrustedWorktrees::try_get_global(cx)
            .unwrap()
            .update(cx, |trusted_worktrees, cx| {
                trusted_worktrees.trust(&store, HashSet::from_iter([path]), cx)
            })
    });
    cx.run_until_parked();
}

#[test]
fn only_a_trusted_projects_t3_worktrees_inherit() {
    let shell = shell();
    let inherits = |folder: &str, trusted_paths: &[&str]| {
        inherits_trust(Path::new(folder), &shell, &trusted(trusted_paths))
    };
    assert!(inherits(WORKTREE, &[CHECKOUT]));
    // T3 records paths with a trailing separator sometimes.
    assert!(inherits(&format!("{WORKTREE}/"), &[CHECKOUT]));
    // Trusting a folder that holds the checkout trusts the checkout, as in Zed.
    assert!(inherits(WORKTREE, &["/src"]));
    assert!(inherits(WEB_WORKTREE, &["/src"]));
    // Another project's trust doesn't count, nor does trusting a folder inside the checkout.
    assert!(!inherits(WORKTREE, &[WEB_CHECKOUT]));
    assert!(!inherits(WORKTREE, &["/src/api/src"]));
    assert!(!inherits(WORKTREE, &[]));
    assert!(!inherits(WEB_WORKTREE, &[CHECKOUT]));
    // A folder no thread of the project works in.
    assert!(!inherits(STRAY, &[CHECKOUT]));
    assert!(!inherits("/tmp/worktrees", &[CHECKOUT]));
    // The checkout itself, and a thread's folder that holds it, are Zed's to trust.
    assert!(!inherits(CHECKOUT, &[CHECKOUT]));
    assert!(!inherits("/src", &[CHECKOUT]));

    assert!(is_separate_worktree(
        Path::new(WORKTREE),
        Path::new(CHECKOUT)
    ));
    assert!(!is_separate_worktree(Path::new("/"), Path::new(CHECKOUT)));
    assert!(!is_separate_worktree(
        Path::new("/src"),
        Path::new(CHECKOUT)
    ));
    assert!(!is_separate_worktree(
        Path::new("worktree"),
        Path::new(CHECKOUT)
    ));
    assert_eq!(checkout_of(" /src/api/ "), Some(PathBuf::from(CHECKOUT)));
    assert_eq!(checkout_of("src/api"), None);
}

#[gpui::test]
async fn a_trusted_projects_worktree_opens_trusted(cx: &mut TestAppContext) {
    let fs = setup(&[CHECKOUT], true, cx).await;
    let project = new_project(&fs, cx).await;
    let worktree = open(&project, WORKTREE, cx).await;
    assert!(is_trusted(&project, worktree, cx));
    let store = project.read_with(cx, |project, _| project.worktree_store());
    assert!(
        !cx.update(|cx| TrustedWorktrees::has_restricted_worktrees(&store, cx)),
        "the review shows no Restricted Mode"
    );

    // With the shell loaded before the worktree opens, too.
    let project = new_project(&fs, cx).await;
    let worktree = open(&project, WORKTREE, cx).await;
    assert!(is_trusted(&project, worktree, cx));
}

#[gpui::test]
async fn an_untrusted_projects_worktree_stays_restricted(cx: &mut TestAppContext) {
    let fs = setup(&[CHECKOUT], true, cx).await;
    let project = new_project(&fs, cx).await;
    let web = open(&project, WEB_WORKTREE, cx).await;
    assert!(!is_trusted(&project, web, cx));
    // The trusted project's worktree in the same window doesn't change that.
    let api = open(&project, WORKTREE, cx).await;
    assert!(is_trusted(&project, api, cx));
    assert!(!is_trusted(&project, web, cx));
}

#[gpui::test]
async fn nothing_is_trusted_when_t3_is_on_another_machine(cx: &mut TestAppContext) {
    let fs = setup(&[CHECKOUT], false, cx).await;
    let project = new_project(&fs, cx).await;
    let worktree = open(&project, WORKTREE, cx).await;
    assert!(!is_trusted(&project, worktree, cx));
}

#[gpui::test]
async fn folders_that_arent_the_projects_t3_worktrees_stay_restricted(cx: &mut TestAppContext) {
    let fs = setup(&[CHECKOUT], true, cx).await;
    let project = new_project(&fs, cx).await;
    // A linked worktree of the project that no thread works in.
    let stray = open(&project, STRAY, cx).await;
    assert!(!is_trusted(&project, stray, cx));
    // A thread's folder that isn't a linked git worktree.
    let notes = open(&project, NOTES, cx).await;
    assert!(!is_trusted(&project, notes, cx));
    // A thread's folder that holds the project's checkout.
    let parent = open(&project, "/src", cx).await;
    assert!(!is_trusted(&project, parent, cx));
}

#[gpui::test]
async fn trusting_the_project_later_trusts_its_worktrees(cx: &mut TestAppContext) {
    let fs = setup(&[], true, cx).await;
    let project = new_project(&fs, cx).await;
    let worktree = open(&project, WORKTREE, cx).await;
    let web = open(&project, WEB_WORKTREE, cx).await;
    let checkout = open(&project, CHECKOUT, cx).await;
    assert!(!is_trusted(&project, worktree, cx));
    assert!(!is_trusted(&project, checkout, cx));

    // Trusting the checkout in Zed's dialog: its open worktree follows at once.
    trust(&project, PathTrust::Worktree(checkout), cx);
    assert!(is_trusted(&project, worktree, cx));
    assert!(!is_trusted(&project, web, cx));

    // And the next time it opens, in another window.
    let other_window = new_project(&fs, cx).await;
    let worktree = open(&other_window, WORKTREE, cx).await;
    assert!(is_trusted(&other_window, worktree, cx));

    // A folder trusted in another window counts too.
    let web_elsewhere = new_project(&fs, cx).await;
    trust(
        &web_elsewhere,
        PathTrust::AbsPath(PathBuf::from("/src")),
        cx,
    );
    assert!(is_trusted(&project, web, cx));
}

async fn workspace<'a>(
    fs: &Arc<FakeFs>,
    cx: &'a mut TestAppContext,
) -> (
    Entity<Workspace>,
    Entity<Project>,
    &'a mut VisualTestContext,
) {
    let project = new_project(fs, cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    (workspace, project, cx)
}

fn press_restricted_mode(
    workspace: &Entity<Workspace>,
    thread_id: &str,
    cx: &mut VisualTestContext,
) {
    workspace.update_in(cx, |workspace, window, cx| {
        ask_to_trust(workspace, thread_id, window, cx)
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn restricted_mode_asks_to_trust_the_project(cx: &mut TestAppContext) {
    let fs = setup(&[], true, cx).await;
    let (workspace, project, cx) = workspace(&fs, cx).await;
    let open = |path: &'static str, cx: &mut VisualTestContext| {
        let task = project.update(cx, |project, cx| {
            project.find_or_create_worktree(path, true, cx)
        });
        async move { task.await.unwrap().0 }
    };
    let worktree = open(WORKTREE, cx).await;
    let worktree = worktree.read_with(cx, |worktree, _| worktree.id());
    cx.run_until_parked();
    let is_trusted = |worktree: WorktreeId, cx: &mut VisualTestContext| {
        let store = project.read_with(cx, |project, _| project.worktree_store());
        cx.update(|_, cx| {
            TrustedWorktrees::try_get_global(cx)
                .unwrap()
                .update(cx, |trusted_worktrees, cx| {
                    trusted_worktrees.can_trust(&store, worktree, cx)
                })
        })
    };
    assert!(!is_trusted(worktree, cx));

    press_restricted_mode(&workspace, THREAD, cx);
    assert_eq!(
        cx.pending_prompt(),
        Some((
            "Trust “api”?".to_string(),
            "This agent worktree is in Restricted Mode: language servers and project settings \
             are off in it. Trusting the project (/src/api) also trusts its agent worktrees: \
             this one now, and the others when you open them."
                .to_string()
        ))
    );
    cx.simulate_prompt_answer("Stay in Restricted Mode");
    cx.run_until_parked();
    assert!(!is_trusted(worktree, cx));

    // Trusting only the worktree leaves the project untrusted.
    press_restricted_mode(&workspace, THREAD, cx);
    cx.simulate_prompt_answer("Trust Only This Worktree");
    cx.run_until_parked();
    assert!(is_trusted(worktree, cx));
    let stray = open(STRAY, cx).await;
    let checkout = open(CHECKOUT, cx).await;
    cx.run_until_parked();
    let checkout = checkout.read_with(cx, |worktree, _| worktree.id());
    assert!(!is_trusted(checkout, cx));

    // Trusting the project from a thread in its checkout.
    press_restricted_mode(&workspace, "thread-3", cx);
    assert_eq!(
        cx.pending_prompt(),
        Some((
            "Trust “api”?".to_string(),
            "/src/api is in Restricted Mode: language servers and project settings are off in \
             it. Trusting the project also trusts its agent worktrees when you open them."
                .to_string()
        ))
    );
    cx.simulate_prompt_answer("Trust Project");
    cx.run_until_parked();
    assert!(is_trusted(checkout, cx));
    // A folder no thread works in is still Zed's to trust.
    let stray = stray.read_with(cx, |worktree, _| worktree.id());
    assert!(!is_trusted(stray, cx));
}

#[gpui::test]
async fn trust_project_trusts_the_checkout_and_the_worktree(cx: &mut TestAppContext) {
    let fs = setup(&[], true, cx).await;
    let (workspace, project, cx) = workspace(&fs, cx).await;
    let task = project.update(cx, |project, cx| {
        project.find_or_create_worktree(WORKTREE, true, cx)
    });
    let (worktree, _) = task.await.unwrap();
    cx.run_until_parked();
    let worktree = worktree.read_with(cx, |worktree, _| worktree.id());

    press_restricted_mode(&workspace, THREAD, cx);
    cx.simulate_prompt_answer("Trust Project");
    cx.run_until_parked();
    let store = project.read_with(cx, |project, _| project.worktree_store());
    assert!(!cx.update(|_, cx| TrustedWorktrees::has_restricted_worktrees(&store, cx)));

    // The checkout opens trusted, and the worktree stays trusted.
    let task = project.update(cx, |project, cx| {
        project.find_or_create_worktree(CHECKOUT, true, cx)
    });
    let (checkout, _) = task.await.unwrap();
    cx.run_until_parked();
    let checkout = checkout.read_with(cx, |worktree, _| worktree.id());
    let can_trust = |id: WorktreeId, cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            TrustedWorktrees::try_get_global(cx)
                .unwrap()
                .update(cx, |trusted_worktrees, cx| {
                    trusted_worktrees.can_trust(&store, id, cx)
                })
        })
    };
    assert!(can_trust(checkout, cx));
    assert!(can_trust(worktree, cx));
}

#[gpui::test]
async fn restricted_mode_opens_zeds_dialog_for_other_folders(cx: &mut TestAppContext) {
    let fs = setup(&[], true, cx).await;
    // Zed's dialog has a text field.
    cx.update(editor::init);
    let (workspace, project, cx) = workspace(&fs, cx).await;
    let task = project.update(cx, |project, cx| {
        project.find_or_create_worktree(STRAY, true, cx)
    });
    task.await.unwrap();
    cx.run_until_parked();

    // The thread's worktree isn't open (another worktree is restricted), or the thread is gone.
    for thread_id in [THREAD, "thread-gone"] {
        press_restricted_mode(&workspace, thread_id, cx);
        assert!(!cx.has_pending_prompt());
        workspace.update_in(cx, |workspace, window, cx| {
            assert!(workspace.active_modal::<SecurityModal>(cx).is_some());
            workspace.show_worktree_trust_security_modal(true, window, cx);
        });
        cx.run_until_parked();
    }
}
