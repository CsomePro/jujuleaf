use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::api::OverleafApi;
use crate::auth::{ProfileStore, find_chrome};
use crate::jj::JjWorkspace;
use crate::sync::{ProjectBinding, discover_project_context, local_status};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warning,
    Error,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

impl DiagnosticCheck {
    fn new(name: &str, status: CheckStatus, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            status,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorSummary {
    pub success: bool,
    pub profile: String,
    pub base_url: Option<String>,
    pub workspace: Option<String>,
    pub checks: Vec<DiagnosticCheck>,
}

pub async fn inspect(
    profiles: &ProfileStore,
    explicit_profile: Option<&str>,
    path: &Path,
    online: bool,
) -> Result<DoctorSummary> {
    let mut checks = Vec::new();
    checks.push(DiagnosticCheck::new(
        "configuration",
        CheckStatus::Ok,
        format!("configuration directory: {}", profiles.root().display()),
    ));

    let context = match discover_project_context(path) {
        Ok(context) => context,
        Err(error) => {
            checks.push(DiagnosticCheck::new(
                "workspace-discovery",
                CheckStatus::Error,
                error.to_string(),
            ));
            None
        }
    };
    let profile = match explicit_profile {
        Some(profile) => profiles.resolve(Some(profile))?,
        None => match context
            .as_ref()
            .and_then(|context| context.profile.as_deref())
        {
            Some(profile) => profiles.resolve(Some(profile))?,
            None => profiles.active_name()?,
        },
    };

    let session_store = profiles.session_store(&profile)?;
    let session = match session_store.load() {
        Ok(Some(session)) if !session.cookie.is_empty() => {
            checks.push(DiagnosticCheck::new(
                "authentication",
                CheckStatus::Ok,
                format!("profile '{profile}' has stored credentials"),
            ));
            Some(session)
        }
        Ok(_) => {
            checks.push(DiagnosticCheck::new(
                "authentication",
                CheckStatus::Error,
                format!(
                    "profile '{profile}' is not authenticated; run: jujuleaf login --profile {profile}"
                ),
            ));
            None
        }
        Err(error) => {
            checks.push(DiagnosticCheck::new(
                "authentication",
                CheckStatus::Error,
                error.to_string(),
            ));
            None
        }
    };

    #[cfg(unix)]
    if session_store.path().exists() {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(session_store.path()) {
            Ok(metadata) if metadata.permissions().mode() & 0o077 == 0 => {
                checks.push(DiagnosticCheck::new(
                    "credential-permissions",
                    CheckStatus::Ok,
                    "credential file is private",
                ));
            }
            Ok(_) => checks.push(DiagnosticCheck::new(
                "credential-permissions",
                CheckStatus::Error,
                "credential file is accessible by other users",
            )),
            Err(error) => checks.push(DiagnosticCheck::new(
                "credential-permissions",
                CheckStatus::Error,
                error.to_string(),
            )),
        }
    }

    match find_chrome() {
        Some(path) => checks.push(DiagnosticCheck::new(
            "browser",
            CheckStatus::Ok,
            format!("found {}", path.display()),
        )),
        None => checks.push(DiagnosticCheck::new(
            "browser",
            CheckStatus::Warning,
            "Chrome/Chromium was not found; cookie-based login remains available",
        )),
    }

    if online {
        if let Some(session) = &session {
            match OverleafApi::new(session, None) {
                Ok(mut api) => match api.list_projects().await {
                    Ok(projects) => {
                        let count = projects
                            .get("projects")
                            .and_then(serde_json::Value::as_array)
                            .map(Vec::len)
                            .unwrap_or_default();
                        checks.push(DiagnosticCheck::new(
                            "endpoint",
                            CheckStatus::Ok,
                            format!("authenticated successfully; {count} project(s) visible"),
                        ));
                    }
                    Err(error) => checks.push(DiagnosticCheck::new(
                        "endpoint",
                        CheckStatus::Error,
                        error.to_string(),
                    )),
                },
                Err(error) => checks.push(DiagnosticCheck::new(
                    "endpoint",
                    CheckStatus::Error,
                    error.to_string(),
                )),
            }
        } else {
            checks.push(DiagnosticCheck::new(
                "endpoint",
                CheckStatus::Skipped,
                "no stored credentials to verify",
            ));
        }
    } else {
        checks.push(DiagnosticCheck::new(
            "endpoint",
            CheckStatus::Skipped,
            "online check disabled",
        ));
    }

    let workspace = if let Some(context) = context {
        let root = context.root;
        match ProjectBinding::load(&root) {
            Ok(binding) => checks.push(DiagnosticCheck::new(
                "project-binding",
                CheckStatus::Ok,
                format!("bound to project {}", binding.project_id),
            )),
            Err(error) => checks.push(DiagnosticCheck::new(
                "project-binding",
                CheckStatus::Error,
                error.to_string(),
            )),
        }
        match JjWorkspace::open(&root).await {
            Ok(_) => checks.push(DiagnosticCheck::new(
                "jujutsu",
                CheckStatus::Ok,
                "Jujutsu workspace opened successfully",
            )),
            Err(error) => checks.push(DiagnosticCheck::new(
                "jujutsu",
                CheckStatus::Error,
                error.to_string(),
            )),
        }
        match local_status(&root).await {
            Ok(status) if status.unresolved_receipts == 0 && status.conflicts.is_empty() => {
                checks.push(DiagnosticCheck::new(
                    "sync-state",
                    CheckStatus::Ok,
                    "no unresolved operation receipts or conflicts",
                ));
            }
            Ok(status) => checks.push(DiagnosticCheck::new(
                "sync-state",
                CheckStatus::Warning,
                format!(
                    "{} unresolved receipt(s), {} conflict(s)",
                    status.unresolved_receipts,
                    status.conflicts.len()
                ),
            )),
            Err(error) => checks.push(DiagnosticCheck::new(
                "sync-state",
                CheckStatus::Error,
                error.to_string(),
            )),
        }
        Some(root.display().to_string())
    } else {
        checks.push(DiagnosticCheck::new(
            "workspace",
            CheckStatus::Warning,
            "not inside a JujuLeaf clone",
        ));
        None
    };

    let success = !checks
        .iter()
        .any(|check| check.status == CheckStatus::Error);
    Ok(DoctorSummary {
        success,
        profile,
        base_url: session.map(|session| session.base_url),
        workspace,
        checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{DEFAULT_PROFILE, Session};

    #[tokio::test]
    async fn offline_check_reports_auth_without_exposing_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let profiles = ProfileStore::new(temp.path().join("config")).unwrap();
        profiles
            .session_store(DEFAULT_PROFILE)
            .unwrap()
            .save(&Session::new("private-cookie", "https://example.test"))
            .unwrap();

        let summary = inspect(&profiles, None, temp.path(), false).await.unwrap();
        assert!(summary.success);
        assert_eq!(summary.profile, DEFAULT_PROFILE);
        assert_eq!(summary.base_url.as_deref(), Some("https://example.test"));
        assert!(
            summary
                .checks
                .iter()
                .any(|check| { check.name == "authentication" && check.status == CheckStatus::Ok })
        );
        assert!(
            summary
                .checks
                .iter()
                .any(|check| { check.name == "endpoint" && check.status == CheckStatus::Skipped })
        );
        assert!(
            summary
                .checks
                .iter()
                .all(|check| !check.detail.contains("private-cookie"))
        );
    }
}
