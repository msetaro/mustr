//! Adding a folder on this computer as a T3 project, as T3's "Add project" does
//! (`components/AddProjectToMachineDialog.tsx`, `CommandPaletteAddProject.tsx`).
//!
//! [`AddProject`] is in the palette and the threads sidebar's header. The app handles it
//! (`mustr/src/home.rs`): it knows which engine it's connected to, and mustr's own engine takes
//! new projects even in a dry run. This module builds what's sent, and reads T3's answer when the
//! folder is already a project ([`AlreadyAProject`]).

use anyhow::{Context as _, Result, anyhow};
use gpui::actions;
use serde_json::Value;
use std::path::Path;
use t3_api::{tagged::ProjectMutation, types};

actions!(
    thread_chat,
    [
        /// Adds a folder on this computer as a project that agents can work in.
        AddProject,
    ]
);

/// A folder to add as a project: T3's `workspaceRoot` and the project's title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewProject {
    pub workspace_root: String,
    pub title: String,
}

/// Adding a folder that's already a project: mustr says so, and selects that project, instead
/// of T3's "Failed to mutate project."
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlreadyAProject {
    /// The folder, as T3 or mustr resolved it.
    pub folder: String,
    /// The project that has the folder, when known.
    pub project_id: Option<String>,
}

impl std::fmt::Display for AlreadyAProject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} is already a project", self.folder)
    }
}

impl std::error::Error for AlreadyAProject {}

/// The project `error` says has the folder, if it's T3's answer to adding a folder that's
/// already a project: `projects.mutate`'s `ProjectMutationError` caused by a
/// `ProjectConflictError` (`apps/server/src/ws.ts` and `project/ProjectService.ts` in T3). Its
/// cause comes as Effect encodes a defect, `{name, message}`, so the folder and the project are
/// read from the message ("Workspace <folder> already belongs to project <id>."); a
/// `ProjectConflictError` sent with its own fields is read too. `folder` is what was added.
pub fn project_conflict(error: &Value, folder: &str) -> Option<AlreadyAProject> {
    const CONFLICT: &str = "ProjectConflictError";
    let tag = |value: &Value| {
        ["_tag", "name"]
            .iter()
            .any(|key| value.get(*key).and_then(Value::as_str) == Some(CONFLICT))
    };
    let conflict = if tag(error) {
        error
    } else {
        error
            .get("cause")
            .filter(|cause| tag(cause) || conflict_message(cause).is_some())?
    };
    let from_message = conflict_message(conflict);
    let field = |key: &str| {
        conflict
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    Some(AlreadyAProject {
        folder: field("workspaceRoot")
            .or_else(|| from_message.as_ref().map(|(root, _)| root.clone()))
            .unwrap_or_else(|| folder.to_string()),
        project_id: field("conflictingProjectId").or(from_message.map(|(_, id)| id)),
    })
}

/// The folder and project of T3's `ProjectConflictError` message.
fn conflict_message(conflict: &Value) -> Option<(String, String)> {
    let message = conflict.get("message")?.as_str()?;
    let rest = message.strip_prefix("Workspace ")?;
    let (root, project) = rest.rsplit_once(" already belongs to project ")?;
    let project = project.trim_end_matches('.');
    (!root.is_empty() && !project.is_empty()).then(|| (root.to_string(), project.to_string()))
}

/// Checks that `path` is a folder that isn't a project yet, and names the project after it.
/// `existing` is each project's id and workspace root. It reads the file system. A folder that
/// is a project already fails with [`AlreadyAProject`].
pub fn new_project<'a>(
    path: &Path,
    existing: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<NewProject> {
    let path = path
        .canonicalize()
        .with_context(|| format!("{} isn't a folder mustr can read", path.display()))?;
    if !path.is_dir() {
        return Err(anyhow!("{} isn't a folder", path.display()));
    }
    let workspace_root = path
        .to_str()
        .with_context(|| format!("{} isn't a valid UTF-8 path", path.display()))?
        .to_string();
    if let Some((project_id, _)) = existing
        .into_iter()
        .find(|(_, root)| *root == workspace_root)
    {
        return Err(anyhow::Error::new(AlreadyAProject {
            folder: workspace_root,
            project_id: Some(project_id.to_string()),
        }));
    }
    let title = path.file_name().map_or_else(
        || workspace_root.clone(),
        |name| name.to_string_lossy().into_owned(),
    );
    Ok(NewProject {
        workspace_root,
        title,
    })
}

/// `projects.mutate` with `project.create`, as T3's client builds it (`createProject` in
/// `packages/client-runtime/src/operations/commands.ts`). The ids are fresh each time.
pub fn create_mutation(project: &NewProject) -> ProjectMutation {
    ProjectMutation::ProjectCreate(types::ProjectMutationProjectCreate {
        command_id: crate::commands::new_id(),
        project_id: crate::commands::new_id(),
        title: project.title.clone(),
        workspace_root: project.workspace_root.clone(),
        // The folder was picked, so it exists; never create one.
        create_workspace_root_if_missing: Some(false),
        default_model_selection: None,
        scripts: None,
        type_: types::ProjectMutationProjectCreateType::ProjectCreate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn adding_a_project_sends_t3s_project_create() {
        let mutation = create_mutation(&NewProject {
            workspace_root: "/src/hello".into(),
            title: "hello".into(),
        });
        let mut payload = serde_json::to_value(&mutation).unwrap();
        // Fresh ids each time.
        for id in ["commandId", "projectId"] {
            let value = payload[id].take();
            assert!(
                uuid::Uuid::parse_str(value.as_str().unwrap()).is_ok(),
                "{id}: {value}"
            );
        }
        assert_eq!(
            payload,
            json!({
                "type": "project.create",
                "commandId": null,
                "projectId": null,
                "title": "hello",
                "workspaceRoot": "/src/hello",
                "createWorkspaceRootIfMissing": false,
            })
        );
    }

    #[test]
    fn projects_are_folders_that_arent_projects_yet() {
        let dir = std::env::temp_dir().join(format!("mustr-new-project-{}", uuid::Uuid::new_v4()));
        let folder = dir.join("hello");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(dir.join("file.txt"), "").unwrap();
        let root = folder.canonicalize().unwrap().to_str().unwrap().to_string();

        assert_eq!(
            new_project(&folder, []).unwrap(),
            NewProject {
                workspace_root: root.clone(),
                title: "hello".into()
            }
        );
        // Named by the folder, however it was reached.
        assert_eq!(
            new_project(&folder.join("..").join("hello"), [])
                .unwrap()
                .title,
            "hello"
        );
        let error = new_project(&folder, [("project-1", root.as_str())]).unwrap_err();
        assert_eq!(error.to_string(), format!("{root} is already a project"));
        assert_eq!(
            error.downcast_ref::<AlreadyAProject>(),
            Some(&AlreadyAProject {
                folder: root,
                project_id: Some("project-1".into())
            })
        );
        assert!(new_project(&dir.join("file.txt"), []).is_err());
        assert!(new_project(&dir.join("missing"), []).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn t3s_conflict_says_the_folder_is_already_a_project() {
        // `projects.mutate`'s typed error, its cause encoded as Effect encodes a defect.
        let error = json!({
            "_tag": "ProjectMutationError",
            "commandId": "command-1",
            "message": "Failed to mutate project.",
            "cause": {
                "name": "ProjectConflictError",
                "message": "Workspace /src/my app already belongs to project project-7."
            }
        });
        let conflict = project_conflict(&error, "/src/my app").unwrap();
        assert_eq!(
            conflict,
            AlreadyAProject {
                folder: "/src/my app".into(),
                project_id: Some("project-7".into())
            }
        );
        assert_eq!(conflict.to_string(), "/src/my app is already a project");

        // The message alone is enough, and so is the error with its own fields.
        let unnamed = json!({ "cause": { "message":
            "Workspace /src/api already belongs to project project-1." } });
        assert_eq!(
            project_conflict(&unnamed, "/src/api").unwrap().project_id,
            Some("project-1".into())
        );
        let tagged = json!({
            "_tag": "ProjectConflictError", "projectId": "new",
            "workspaceRoot": "/src/api", "conflictingProjectId": "project-1"
        });
        assert_eq!(
            project_conflict(&tagged, "/src/api/").unwrap(),
            AlreadyAProject {
                folder: "/src/api".into(),
                project_id: Some("project-1".into())
            }
        );
        let named = json!({ "cause": { "name": "ProjectConflictError", "message": "" } });
        assert_eq!(
            project_conflict(&named, "/src/api").unwrap(),
            AlreadyAProject {
                folder: "/src/api".into(),
                project_id: None
            }
        );

        // Other failures aren't.
        let other = json!({
            "_tag": "ProjectMutationError",
            "commandId": "command-1",
            "message": "Failed to mutate project.",
            "cause": { "name": "ProjectOperationError",
                       "message": "Project operation 'normalize-workspace' failed." }
        });
        assert_eq!(project_conflict(&other, "/src/api"), None);
        assert_eq!(
            project_conflict(&json!({ "_tag": "ProjectMutationError" }), "/src/api"),
            None
        );
    }
}
