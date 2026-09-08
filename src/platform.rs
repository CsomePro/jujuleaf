use std::path::{Component, Path, PathBuf};

#[cfg(any(windows, target_os = "macos", test))]
use std::collections::BTreeMap;

use anyhow::{Result, ensure};

pub(crate) fn workspace_path(root: &Path, remote_path: &str) -> Result<PathBuf> {
    validate_remote_path(remote_path)?;
    Ok(root.join(remote_path.trim_start_matches('/')))
}

pub(crate) fn validate_materializable_paths<'a>(
    paths: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let paths = paths.into_iter().collect::<Vec<_>>();
    for path in &paths {
        validate_remote_path(path)?;
    }

    #[cfg(any(windows, target_os = "macos"))]
    validate_case_insensitive_collisions(&paths)?;

    Ok(())
}

fn validate_remote_path(remote_path: &str) -> Result<()> {
    ensure!(
        !remote_path.contains('\\'),
        "remote path contains a backslash and cannot be materialized safely: {remote_path}"
    );
    let relative = Path::new(remote_path.trim_start_matches('/'));
    ensure!(!relative.as_os_str().is_empty(), "remote path is empty");
    ensure!(
        relative
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "unsafe remote path: {remote_path}"
    );

    #[cfg(windows)]
    validate_windows_relative_path(remote_path.trim_start_matches('/'))?;

    Ok(())
}

#[cfg(any(windows, test))]
fn validate_windows_relative_path(relative: &str) -> Result<()> {
    for component in relative.split('/') {
        ensure!(
            !component.is_empty(),
            "remote path contains an empty Windows path component: {relative}"
        );
        ensure!(
            !component
                .chars()
                .any(|character| character <= '\u{1f}' || r#"<>:\"|?*"#.contains(character)),
            "remote path contains characters unsupported by Windows: {relative}"
        );
        ensure!(
            !component.ends_with([' ', '.']),
            "remote path ends a Windows path component with a space or period: {relative}"
        );
        let stem = component
            .split('.')
            .next()
            .unwrap_or(component)
            .to_ascii_uppercase();
        ensure!(
            !is_windows_device_name(&stem),
            "remote path uses a reserved Windows device name: {relative}"
        );
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn is_windows_device_name(stem: &str) -> bool {
    matches!(stem, "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
        })
}

#[cfg(any(windows, target_os = "macos", test))]
fn validate_case_insensitive_collisions(paths: &[&str]) -> Result<()> {
    let mut seen = BTreeMap::<String, &str>::new();
    for path in paths {
        let normalized = path.trim_start_matches('/');
        let key = normalized.to_lowercase();
        if let Some(existing) = seen.insert(key, normalized) {
            ensure!(
                existing == normalized,
                "remote paths differ only by case and cannot coexist on this filesystem: {existing} and {normalized}"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_remote_paths_on_every_platform() {
        assert!(validate_remote_path("/../outside.tex").is_err());
        assert!(validate_remote_path(r"/sections\\main.tex").is_err());
        assert!(validate_remote_path("/").is_err());
    }

    #[test]
    fn rejects_windows_incompatible_names() {
        assert!(validate_windows_relative_path("CON.tex").is_err());
        assert!(validate_windows_relative_path("figures/plot?.pdf").is_err());
        assert!(validate_windows_relative_path("trailing. ").is_err());
        assert!(validate_windows_relative_path("sections/main.tex").is_ok());
    }

    #[test]
    fn detects_case_insensitive_collisions() {
        assert!(validate_case_insensitive_collisions(&["Main.tex", "main.tex"]).is_err());
        assert!(validate_case_insensitive_collisions(&["main.tex", "sections/a.tex"]).is_ok());
    }
}
