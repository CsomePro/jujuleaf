use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};

pub const IGNORE_FILE: &str = ".jujuleafignore";

/// Load root-relative gitignore-style rules used by JujuLeaf.
pub fn load(root: &Path) -> Result<Arc<GitIgnoreFile>> {
    let path = root.join(IGNORE_FILE);
    let contents = match std::fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    GitIgnoreFile::empty()
        .chain(RepoPath::root(), &path, &contents)
        .with_context(|| format!("invalid {}", path.display()))
}

pub fn repo_path(relative: &Path) -> Result<RepoPathBuf> {
    let normalized = relative.to_string_lossy().replace('\\', "/");
    RepoPathBuf::from_internal_string(normalized)
        .with_context(|| format!("invalid workspace path: {}", relative.display()))
}

pub fn is_private_path(relative: &Path) -> bool {
    relative.components().next().is_some_and(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(".jj" | ".jujuleaf" | ".git")
        )
    }) || relative == Path::new(IGNORE_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_gitignore_style_rules() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join(IGNORE_FILE),
            b"/build/\n*.aux\n!important.aux\n",
        )
        .unwrap();
        let ignore = load(temp.path()).unwrap();
        assert!(ignore.matches_dir(RepoPath::from_internal_string("build").unwrap()));
        assert!(ignore.matches_file(RepoPath::from_internal_string("paper.aux").unwrap()));
        assert!(!ignore.matches_file(RepoPath::from_internal_string("important.aux").unwrap()));
    }

    #[test]
    fn protects_internal_paths_from_uploads() {
        assert!(is_private_path(Path::new(".jj/repo/store")));
        assert!(is_private_path(Path::new(".git/config")));
        assert!(is_private_path(Path::new(".jujuleaf/project.json")));
        assert!(is_private_path(Path::new(IGNORE_FILE)));
        assert!(!is_private_path(Path::new("main.tex")));
    }
}
