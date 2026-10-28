//! The file tree of a review: the changed files grouped by directory, with each file's change,
//! line counts, and the viewer's viewed mark.

use gpui::SharedString;
use std::collections::BTreeMap;

/// How a file changed in the review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileChange {
    Added,
    Deleted,
    Modified,
    Renamed,
}

impl FileChange {
    /// The one-letter code git and forges use.
    pub fn letter(self) -> &'static str {
        match self {
            FileChange::Added => "A",
            FileChange::Deleted => "D",
            FileChange::Modified => "M",
            FileChange::Renamed => "R",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            FileChange::Added => "Added",
            FileChange::Deleted => "Deleted",
            FileChange::Modified => "Modified",
            FileChange::Renamed => "Renamed",
        }
    }
}

/// The viewer's viewed mark for a file, as the forge reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileViewedState {
    #[default]
    Unviewed,
    Viewed,
    /// Marked viewed, then changed by a later push.
    Dismissed,
}

/// One changed file in the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTreeFile {
    /// The file's path in the head revision, or in the base revision for deleted files.
    pub path: String,
    /// The path before a rename.
    pub old_path: Option<String>,
    pub change: FileChange,
    /// Lines added and removed, when the review shows the file.
    pub line_counts: Option<(u32, u32)>,
    /// Why the review can't show the file (binary, too large for the forge), if it can't.
    pub unavailable: Option<String>,
    pub viewed: FileViewedState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileTreeRow {
    /// A directory. Chains of directories that hold nothing but one other directory are shown as
    /// one row (`crates/editor/src`), as most file trees do.
    Directory { name: SharedString, depth: usize },
    File {
        /// The index into [`FileTreeModel::files`].
        file: usize,
        name: SharedString,
        depth: usize,
    },
}

impl FileTreeRow {
    pub fn depth(&self) -> usize {
        match self {
            FileTreeRow::Directory { depth, .. } | FileTreeRow::File { depth, .. } => *depth,
        }
    }
}

/// The rows of a review's file tree: directories first, then files, each sorted by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileTreeModel {
    files: Vec<FileTreeFile>,
    rows: Vec<FileTreeRow>,
    tracks_viewed: bool,
}

impl FileTreeModel {
    /// `tracks_viewed` is `false` when the forge keeps no viewed marks, so the tree shows none.
    pub fn new(files: Vec<FileTreeFile>, tracks_viewed: bool) -> Self {
        let mut root = DirectoryNode::default();
        for (index, file) in files.iter().enumerate() {
            let mut components = file
                .path
                .split('/')
                .filter(|component| !component.is_empty())
                .collect::<Vec<_>>();
            let name = components.pop().unwrap_or(file.path.as_str());
            let mut node = &mut root;
            for component in components {
                node = node.directories.entry(component.to_string()).or_default();
            }
            node.files.push((name.to_string(), index));
        }
        let mut rows = Vec::with_capacity(files.len());
        root.push_rows(0, &mut rows);
        Self {
            files,
            rows,
            tracks_viewed,
        }
    }

    pub fn files(&self) -> &[FileTreeFile] {
        &self.files
    }

    pub fn rows(&self) -> &[FileTreeRow] {
        &self.rows
    }

    pub fn tracks_viewed(&self) -> bool {
        self.tracks_viewed
    }

    pub fn set_viewed(&mut self, file: usize, viewed: FileViewedState) {
        if let Some(file) = self.files.get_mut(file) {
            file.viewed = viewed;
        }
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn viewed_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| file.viewed == FileViewedState::Viewed)
            .count()
    }

    /// "3 of 12 viewed", or "12 files" when the forge keeps no viewed marks.
    pub fn summary(&self) -> String {
        if self.tracks_viewed {
            format!("{} of {} viewed", self.viewed_count(), self.file_count())
        } else if self.file_count() == 1 {
            "1 file".to_string()
        } else {
            format!("{} files", self.file_count())
        }
    }

    /// File indices in the order the tree shows them.
    pub fn file_order(&self) -> Vec<usize> {
        self.rows
            .iter()
            .filter_map(|row| match row {
                FileTreeRow::File { file, .. } => Some(*file),
                FileTreeRow::Directory { .. } => None,
            })
            .collect()
    }
}

#[derive(Default)]
struct DirectoryNode {
    directories: BTreeMap<String, DirectoryNode>,
    files: Vec<(String, usize)>,
}

impl DirectoryNode {
    fn push_rows(&self, depth: usize, rows: &mut Vec<FileTreeRow>) {
        for (name, directory) in &self.directories {
            let mut name = name.clone();
            let mut directory = directory;
            while directory.files.is_empty()
                && directory.directories.len() == 1
                && let Some((child_name, child)) = directory.directories.iter().next()
            {
                name = format!("{name}/{child_name}");
                directory = child;
            }
            rows.push(FileTreeRow::Directory {
                name: name.into(),
                depth,
            });
            directory.push_rows(depth + 1, rows);
        }
        let mut files = self.files.clone();
        files.sort();
        rows.extend(files.into_iter().map(|(name, file)| FileTreeRow::File {
            file,
            name: name.into(),
            depth,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, viewed: FileViewedState) -> FileTreeFile {
        FileTreeFile {
            path: path.to_string(),
            old_path: None,
            change: FileChange::Modified,
            line_counts: Some((1, 1)),
            unavailable: None,
            viewed,
        }
    }

    fn row_labels(model: &FileTreeModel) -> Vec<String> {
        model
            .rows()
            .iter()
            .map(|row| match row {
                FileTreeRow::Directory { name, depth } => format!("{}{name}/", "  ".repeat(*depth)),
                FileTreeRow::File { name, depth, file } => {
                    format!("{}{name} [{file}]", "  ".repeat(*depth))
                }
            })
            .collect()
    }

    #[test]
    fn groups_files_by_directory_and_compacts_single_child_chains() {
        let model = FileTreeModel::new(
            vec![
                file("README.md", FileViewedState::Viewed),
                file("crates/editor/src/editor.rs", FileViewedState::Unviewed),
                file("crates/editor/src/element.rs", FileViewedState::Viewed),
                file("crates/ui/button.rs", FileViewedState::Dismissed),
                file("crates/editor/Cargo.toml", FileViewedState::Unviewed),
                file("a.rs", FileViewedState::Unviewed),
                file("docs/guide/intro.md", FileViewedState::Unviewed),
            ],
            true,
        );

        assert_eq!(
            row_labels(&model),
            [
                "crates/",
                "  editor/",
                "    src/",
                "      editor.rs [1]",
                "      element.rs [2]",
                "    Cargo.toml [4]",
                "  ui/",
                "    button.rs [3]",
                "docs/guide/",
                "  intro.md [6]",
                "README.md [0]",
                "a.rs [5]",
            ]
        );
        assert_eq!(model.file_order(), [1, 2, 4, 3, 6, 0, 5]);
    }

    #[test]
    fn counts_only_files_marked_viewed() {
        let model = FileTreeModel::new(
            vec![
                file("a.rs", FileViewedState::Viewed),
                file("b.rs", FileViewedState::Dismissed),
                file("c.rs", FileViewedState::Unviewed),
                file("d/e.rs", FileViewedState::Viewed),
            ],
            true,
        );
        assert_eq!(model.viewed_count(), 2);
        assert_eq!(model.file_count(), 4);
        assert_eq!(model.summary(), "2 of 4 viewed");

        let untracked = FileTreeModel::new(vec![file("a.rs", FileViewedState::Unviewed)], false);
        assert_eq!(untracked.summary(), "1 file");
        assert_eq!(FileTreeModel::default().summary(), "0 files");
    }
}
