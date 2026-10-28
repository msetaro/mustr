//! Adding a folder on this computer as a T3 project, as T3's "Add project" does
//! (`components/AddProjectToMachineDialog.tsx`, `CommandPaletteAddProject.tsx`).
//!
//! [`AddProject`] is in the palette and the threads sidebar's header. The app handles it
//! (`mustr/src/home.rs`): it knows which engine it's connected to, and mustr's own engine takes
//! new projects even in a dry run. This module builds what's sent.

use anyhow::{Context as _, Result, anyhow};
use gpui::actions;
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

/// Checks that `path` is a folder that isn't a project yet, and names the project after it.
/// `existing` is each project's title and workspace root. It reads the file system.
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
    if let Some((title, _)) = existing
        .into_iter()
        .find(|(_, root)| *root == workspace_root)
    {
        return Err(anyhow!("{workspace_root} is already the project {title}"));
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
        let error = new_project(&folder, [("Hello", root.as_str())]).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("{root} is already the project Hello")
        );
        assert!(new_project(&dir.join("file.txt"), []).is_err());
        assert!(new_project(&dir.join("missing"), []).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
