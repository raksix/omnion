//! Folders of one site's library: the tree, its paths, and the moves that keep it a tree.
//!
//! A folder is a row in `media_folders` with a *materialised* `path` — the chain of names from
//! the library root (`Campaigns/2026`) — so "every folder under `Campaigns`" and "is this move a
//! cycle?" are one indexed read instead of a recursive walk. The root of each site is a real row,
//! written by the migration, so the browser always has a stable id to address uploads to and to
//! deep-link.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{MediaError, Result};

/// Longest single folder name.
pub const MAX_FOLDER_NAME_LENGTH: usize = 120;

/// Longest materialised path, so a deep tree cannot grow an unbounded key.
pub const MAX_FOLDER_PATH_LENGTH: usize = 1024;

/// The name the materialised root carries.
pub const ROOT_FOLDER_NAME: &str = "Media";

/// One folder of one site.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Folder {
    /// Primary key.
    pub id: Uuid,
    /// Site the folder belongs to.
    pub site_id: Uuid,
    /// Parent folder, `None` for the library root.
    pub parent_id: Option<Uuid>,
    /// The name as the operator typed it.
    pub name: String,
    /// Materialised path from the library root, without leading or trailing slashes.
    pub path: String,
    /// Account that created the folder.
    pub created_by: Option<Uuid>,
    /// When the folder was created.
    pub created_at: OffsetDateTime,
    /// Last rename or move.
    pub updated_at: OffsetDateTime,
}

impl Folder {
    /// `true` when this folder is the library root of its site.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }

    /// The `path` of the folder that would contain a child called `name`.
    #[must_use]
    pub fn child_path(&self, name: &str) -> String {
        format!("{}/{}", self.path, name)
    }

    /// The names from the root down to this folder, in order — the breadcrumb the panel shows.
    #[must_use]
    pub fn segments(&self) -> Vec<&str> {
        self.path
            .split('/')
            .filter(|part| !part.is_empty())
            .collect()
    }
}

/// A folder that is about to be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFolder {
    /// Site the folder belongs to.
    pub site_id: Uuid,
    /// Parent folder id.
    pub parent_id: Uuid,
    /// Name as typed, before [`validate_folder_name`].
    pub name: String,
    /// Materialised path of the parent.
    pub parent_path: String,
    /// Account creating it.
    pub created_by: Option<Uuid>,
}

/// A folder that is about to be renamed or moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderMove {
    /// The new name.
    pub name: String,
    /// Materialised path of the new parent.
    pub parent_path: String,
}

/// Reduce a typed folder name to the form a path segment may take.
///
/// A folder name ends up inside a materialised path and inside object keys, so the same reduction
/// the file names get applies here: no separators, no control characters, no traversal, and a
/// bound on the length.
#[must_use]
pub fn sanitize_folder_name(raw: &str) -> String {
    let mut name = String::with_capacity(raw.len());
    for character in raw.trim().chars() {
        if character.is_control() {
            continue;
        }
        match character {
            '/' | '\\' | '\0' => name.push('-'),
            other => name.push(other),
        }
    }
    name.trim().trim_matches('.').to_owned()
}

/// Check a folder name, reporting which rule it broke.
pub fn validate_folder_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(MediaError::InvalidFolderName(
            "a folder needs a name".to_owned(),
        ));
    }
    if name.len() > MAX_FOLDER_NAME_LENGTH {
        return Err(MediaError::InvalidFolderName(format!(
            "a folder name is at most {MAX_FOLDER_NAME_LENGTH} characters"
        )));
    }
    if name == "." || name == ".." {
        return Err(MediaError::InvalidFolderName(
            "`.` and `..` are not folder names".to_owned(),
        ));
    }
    Ok(())
}

/// Build the materialised path of a child of `parent_path`.
///
/// The path is the one thing every subtree query and every cycle check depends on, so it is
/// derived here and nowhere else: a single rule, one implementation, one test surface.
pub fn child_path(parent_path: &str, name: &str) -> Result<String> {
    validate_folder_name(name)?;
    let path = if parent_path.trim_matches('/').is_empty() {
        name.to_owned()
    } else {
        format!("{}/{name}", parent_path.trim_matches('/'))
    };
    if path.len() > MAX_FOLDER_PATH_LENGTH {
        return Err(MediaError::InvalidFolderName(format!(
            "the folder path would be longer than {MAX_FOLDER_PATH_LENGTH} characters"
        )));
    }
    Ok(path)
}

/// The SQL predicate that matches a folder and everything under it.
///
/// Named here so the query and the test that proves a rename moves a whole subtree cannot drift
/// apart: `path = $prefix or path like $prefix || '/%'`. A plain `like 'Media/%'` would also
/// match `Media/Archive`, which is a different subtree.
#[must_use]
pub fn subtree_predicate() -> &'static str {
    "path = $1 or path like $2"
}

/// The `like` pattern that matches the descendants of a path (not the path itself).
#[must_use]
pub fn subtree_pattern(path: &str) -> String {
    format!("{}/%", path.trim_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(path: &str, parent: Option<Uuid>) -> Folder {
        Folder {
            id: Uuid::nil(),
            site_id: Uuid::nil(),
            parent_id: parent,
            name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            path: path.to_owned(),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_name_loses_its_separators_and_control_characters() {
        assert_eq!(
            sanitize_folder_name(" Campaigns / 2026 "),
            "Campaigns - 2026"
        );
        assert_eq!(sanitize_folder_name("a\nb\tc"), "abc");
        assert_eq!(sanitize_folder_name("..."), "");
    }

    #[test]
    fn a_name_is_refused_when_it_cannot_be_a_path_segment() {
        assert!(validate_folder_name("Campaigns").is_ok());
        assert!(validate_folder_name("").is_err());
        assert!(validate_folder_name("..").is_err());
        assert!(validate_folder_name(&"x".repeat(MAX_FOLDER_NAME_LENGTH + 1)).is_err());
    }

    #[test]
    fn a_child_path_joins_exactly_one_slash() {
        assert_eq!(child_path("Media", "Campaigns").unwrap(), "Media/Campaigns");
        assert_eq!(
            child_path("Media/Campaigns", "2026").unwrap(),
            "Media/Campaigns/2026"
        );
        // A parent path that already carries slashes on its edges is the caller's mistake, and
        // the path must not grow a double separator that every later query would have to strip.
        assert_eq!(child_path("/Media/", "2026").unwrap(), "Media/2026");
        assert!(child_path("Media", "../etc").is_ok());
        assert!(child_path("Media", "").is_err());
    }

    #[test]
    fn a_breadcrumb_lists_the_chain_from_the_root() {
        let deep = folder("Media/Campaigns/2026/Q1", Some(Uuid::new_v4()));
        assert_eq!(deep.segments(), vec!["Media", "Campaigns", "2026", "Q1"]);
        // The row with no parent is the library root; a folder with a parent is a real one.
        assert!(folder("Media", None).is_root());
        assert!(!deep.is_root());
    }

    #[test]
    fn a_folder_derives_the_path_of_its_children() {
        let parent = folder("Media/Campaigns", Some(Uuid::new_v4()));
        assert_eq!(parent.child_path("2026"), "Media/Campaigns/2026");
    }
}
