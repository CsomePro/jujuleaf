use std::fs::File;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::api::OverleafApi;
use crate::auth::{DEFAULT_PROFILE, Session};
use crate::jj::JjWorkspace;
use crate::operations::{
    BuildOptions, InputChange, build_document_operations, parse_document_snapshot, utf16_len,
};
use crate::project::{collect_documents, connect_project};
use crate::socket::UpdateOptions;
use crate::store::{SyncStore, content_hash};

const STATE_DIR: &str = ".jj/jujuleaf";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectBinding {
    pub project_id: String,
    pub base_url: String,
    #[serde(default = "default_profile")]
    pub profile: String,
}

fn default_profile() -> String {
    DEFAULT_PROFILE.to_owned()
}

impl ProjectBinding {
    fn path(root: &Path) -> PathBuf {
        root.join(STATE_DIR).join("project.json")
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Self::path(root);
        std::fs::create_dir_all(path.parent().expect("binding has parent"))?;
        std::fs::write(&path, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = Self::path(root);
        serde_json::from_slice(
            &std::fs::read(&path)
                .with_context(|| format!("not a JujuLeaf clone: {}", root.display()))?,
        )
        .with_context(|| format!("invalid {}", path.display()))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneSummary {
    pub success: bool,
    pub project_id: String,
    pub profile: String,
    pub path: String,
    pub document_count: usize,
    pub jj_operation_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullSummary {
    pub success: bool,
    pub project_id: String,
    pub profile: String,
    pub updated: Vec<String>,
    pub added: Vec<String>,
    pub local_only: Vec<String>,
    pub unchanged: Vec<String>,
    pub conflicts: Vec<String>,
    pub jj_operation_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushSummary {
    pub success: bool,
    pub project_id: String,
    pub profile: String,
    pub pushed: Vec<String>,
    pub unchanged: Vec<String>,
    pub conflicts: Vec<String>,
    pub unknown: Vec<String>,
    pub jj_operation_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusSummary {
    pub project_id: String,
    pub profile: String,
    pub modified: Vec<String>,
    pub missing: Vec<String>,
    pub clean: Vec<String>,
    pub unresolved_receipts: usize,
    pub jj_operation_id: String,
}

fn database(root: &Path) -> Result<SyncStore> {
    SyncStore::open(root.join(STATE_DIR).join("sync.sqlite3"))
}

pub fn discover_root(start: impl AsRef<Path>) -> Result<PathBuf> {
    let mut current = start
        .as_ref()
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", start.as_ref().display()))?;
    loop {
        if ProjectBinding::path(&current).exists() {
            return Ok(current);
        }
        if !current.pop() {
            bail!("not inside a JujuLeaf clone")
        }
    }
}

fn write_document(root: &Path, remote_path: &str, content: &str) -> Result<()> {
    let relative = remote_path.trim_start_matches('/');
    ensure!(!relative.is_empty(), "remote document path is empty");
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))
}

fn read_document(root: &Path, remote_path: &str) -> Result<Option<String>> {
    let path = root.join(remote_path.trim_start_matches('/'));
    if !path.exists() {
        return Ok(None);
    }
    std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read UTF-8 document {}", path.display()))
        .map(Some)
}

fn extract_zip(bytes: &[u8], destination: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("invalid project zip")?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("unsafe path in project zip: {}", entry.name()))?
            .to_owned();
        let output = destination.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = File::create(&output)?;
        std::io::copy(&mut entry, &mut file)?;
        file.flush()?;
    }
    Ok(())
}

pub async fn clone_project(
    api: &mut OverleafApi,
    session: &Session,
    profile: &str,
    project_id: &str,
    destination: &Path,
) -> Result<CloneSummary> {
    if destination.exists() {
        ensure!(
            destination.read_dir()?.next().is_none(),
            "destination is not empty: {}",
            destination.display()
        );
    } else {
        std::fs::create_dir_all(destination)?;
    }

    let zip = api.download_zip(project_id).await?;
    extract_zip(&zip, destination)?;

    let (mut socket, project) = connect_project(session, project_id).await?;
    let documents = collect_documents(&project);
    let mut states = Vec::new();
    for document in &documents {
        let joined = socket.join_doc(&document.id).await?;
        let state = parse_document_snapshot(joined.snapshot, &joined.ot_type)?;
        write_document(destination, &document.path, &state.content)?;
        socket.leave_doc(&document.id).await.ok();
        states.push((
            document.clone(),
            joined.version,
            content_hash(&state.content),
        ));
    }
    socket.close().await.ok();

    let binding = ProjectBinding {
        project_id: project_id.to_owned(),
        base_url: session.base_url.clone(),
        profile: profile.to_owned(),
    };
    let mut workspace = JjWorkspace::init(destination).await?;
    binding.save(destination)?;
    let checkpoint = workspace
        .checkpoint(&format!("clone Overleaf project {project_id}"))
        .await?;
    let store = database(destination)?;
    for (document, version, hash) in states {
        store.upsert_document(
            project_id,
            &document.id,
            &document.path,
            version,
            &hash,
            Some(&checkpoint.operation_id),
        )?;
    }
    Ok(CloneSummary {
        success: true,
        project_id: project_id.to_owned(),
        profile: profile.to_owned(),
        path: destination.display().to_string(),
        document_count: documents.len(),
        jj_operation_id: checkpoint.operation_id,
    })
}

pub async fn pull_project(root: &Path, session: &Session, profile: &str) -> Result<PullSummary> {
    let binding = ProjectBinding::load(root)?;
    ensure!(
        binding.profile == profile,
        "clone is bound to profile '{}', but selected profile is '{profile}'",
        binding.profile
    );
    ensure!(
        binding.base_url == session.base_url,
        "clone belongs to {}, but current session uses {}",
        binding.base_url,
        session.base_url
    );
    let store = database(root)?;
    let mut workspace = JjWorkspace::open(root).await?;
    workspace.checkpoint("local state before pull").await?;
    let (mut socket, project) = connect_project(session, &binding.project_id).await?;
    let documents = collect_documents(&project);
    let mut updated = Vec::new();
    let mut added = Vec::new();
    let mut local_only = Vec::new();
    let mut unchanged = Vec::new();
    let mut conflicts = Vec::new();
    let mut accepted = Vec::new();

    for document in documents {
        let doc_id = document.id.clone();
        let joined = socket.join_doc(&document.id).await?;
        let state = parse_document_snapshot(joined.snapshot, &joined.ot_type)?;
        let remote_hash = content_hash(&state.content);
        store.reconcile_confirmed_hash(&binding.project_id, &document.id, &remote_hash)?;
        let local = read_document(root, &document.path)?;
        let previous = store.document(&binding.project_id, &document.id)?;
        let action = match (&local, &previous) {
            (None, _) => "added",
            (Some(local), Some(previous)) => {
                let local_hash = content_hash(local);
                if local_hash == remote_hash {
                    "unchanged"
                } else if local_hash == previous.remote_hash {
                    "updated"
                } else if remote_hash == previous.remote_hash {
                    "local_only"
                } else {
                    "conflict"
                }
            }
            (Some(local), None) if content_hash(local) == remote_hash => "unchanged",
            (Some(_), None) => "conflict",
        };
        match action {
            "added" => {
                write_document(root, &document.path, &state.content)?;
                added.push(document.path.clone());
            }
            "updated" => {
                write_document(root, &document.path, &state.content)?;
                updated.push(document.path.clone());
            }
            "local_only" => local_only.push(document.path.clone()),
            "unchanged" => unchanged.push(document.path.clone()),
            "conflict" => {
                let incoming = root.join(STATE_DIR).join("incoming").join(&document.id);
                if let Some(parent) = incoming.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(incoming, &state.content)?;
                conflicts.push(document.path.clone());
            }
            _ => unreachable!(),
        }
        if action != "conflict" {
            accepted.push((document, joined.version, remote_hash));
        }
        socket.leave_doc(&doc_id).await.ok();
    }
    socket.close().await.ok();
    let checkpoint = workspace
        .checkpoint(&format!("pull Overleaf project {}", binding.project_id))
        .await?;
    for (document, version, hash) in accepted {
        store.upsert_document(
            &binding.project_id,
            &document.id,
            &document.path,
            version,
            &hash,
            Some(&checkpoint.operation_id),
        )?;
    }
    Ok(PullSummary {
        success: conflicts.is_empty(),
        project_id: binding.project_id,
        profile: binding.profile,
        updated,
        added,
        local_only,
        unchanged,
        conflicts,
        jj_operation_id: checkpoint.operation_id,
    })
}

pub async fn push_project(
    root: &Path,
    session: &Session,
    profile: &str,
    options: &UpdateOptions,
) -> Result<PushSummary> {
    let binding = ProjectBinding::load(root)?;
    ensure!(
        binding.profile == profile,
        "clone is bound to profile '{}', but selected profile is '{profile}'",
        binding.profile
    );
    ensure!(
        binding.base_url == session.base_url,
        "clone belongs to {}, but current session uses {}",
        binding.base_url,
        session.base_url
    );
    let store = database(root)?;
    let mut workspace = JjWorkspace::open(root).await?;
    let checkpoint = workspace.checkpoint("local state before push").await?;
    let (mut socket, project) = connect_project(session, &binding.project_id).await?;
    let documents = collect_documents(&project);
    let mut pushed = Vec::new();
    let mut unchanged = Vec::new();
    let mut conflicts = Vec::new();
    let mut unknown = Vec::new();

    for document in documents {
        let Some(local) = read_document(root, &document.path)? else {
            conflicts.push(document.path);
            continue;
        };
        let joined = socket.join_doc(&document.id).await?;
        let state = parse_document_snapshot(joined.snapshot, &joined.ot_type)?;
        let local_hash = content_hash(&local);
        let remote_hash = content_hash(&state.content);
        store.reconcile_confirmed_hash(&binding.project_id, &document.id, &remote_hash)?;
        if local_hash == remote_hash {
            store.upsert_document(
                &binding.project_id,
                &document.id,
                &document.path,
                joined.version,
                &remote_hash,
                Some(&checkpoint.operation_id),
            )?;
            unchanged.push(document.path);
            socket.leave_doc(&document.id).await.ok();
            continue;
        }
        let Some(previous) = store.document(&binding.project_id, &document.id)? else {
            conflicts.push(document.path);
            socket.leave_doc(&document.id).await.ok();
            continue;
        };
        if remote_hash != previous.remote_hash {
            conflicts.push(document.path);
            socket.leave_doc(&document.id).await.ok();
            continue;
        }
        let built = build_document_operations(
            &state,
            &[InputChange {
                from: 0,
                to: Some(utf16_len(&state.content)),
                insert: Some(local.clone()),
                expect: Some(state.content.clone()),
            }],
            &BuildOptions::default(),
        )?;
        let operation_json = serde_json::to_string(&built.ops)?;
        let receipt = store.prepare_operation(
            &binding.project_id,
            &document.id,
            joined.version,
            &operation_json,
            &local_hash,
        )?;
        if let Some(public_id) = socket.public_id() {
            store.record_source_id(&receipt, public_id)?;
        }
        store.mark_inflight(&receipt)?;
        match socket
            .apply_update(&document.id, &built.ops, joined.version, None, options)
            .await
        {
            Ok(confirmation) => {
                store.mark_confirmed(&receipt)?;
                store.upsert_document(
                    &binding.project_id,
                    &document.id,
                    &document.path,
                    confirmation.version,
                    &local_hash,
                    Some(&checkpoint.operation_id),
                )?;
                pushed.push(document.path);
            }
            Err(error) => {
                store.mark_unknown(&receipt, &error.to_string())?;
                socket
                    .reconnect()
                    .await
                    .context("update result is unknown and reconnect failed")?;
                let refreshed = socket.join_doc(&document.id).await?;
                let refreshed_state =
                    parse_document_snapshot(refreshed.snapshot, &refreshed.ot_type)?;
                let refreshed_hash = content_hash(&refreshed_state.content);
                if refreshed_hash == local_hash {
                    store.mark_confirmed(&receipt)?;
                    store.upsert_document(
                        &binding.project_id,
                        &document.id,
                        &document.path,
                        refreshed.version,
                        &local_hash,
                        Some(&checkpoint.operation_id),
                    )?;
                    pushed.push(document.path);
                } else {
                    unknown.push(document.path);
                }
            }
        }
        socket.leave_doc(&document.id).await.ok();
    }
    socket.close().await.ok();
    Ok(PushSummary {
        success: conflicts.is_empty() && unknown.is_empty(),
        project_id: binding.project_id,
        profile: binding.profile,
        pushed,
        unchanged,
        conflicts,
        unknown,
        jj_operation_id: checkpoint.operation_id,
    })
}

pub async fn local_status(root: &Path) -> Result<StatusSummary> {
    let binding = ProjectBinding::load(root)?;
    let store = database(root)?;
    let jj_operation_id = JjWorkspace::open(root).await?.operation_id();
    let mut modified = Vec::new();
    let mut missing = Vec::new();
    let mut clean = Vec::new();

    let unresolved = store.unresolved_operations(&binding.project_id, None)?;
    for document in store.documents(&binding.project_id)? {
        match read_document(root, &document.path)? {
            None => missing.push(document.path),
            Some(content) if content_hash(&content) == document.remote_hash => {
                clean.push(document.path)
            }
            Some(_) => modified.push(document.path),
        }
    }
    Ok(StatusSummary {
        project_id: binding.project_id,
        profile: binding.profile,
        modified,
        missing,
        clean,
        unresolved_receipts: unresolved.len(),
        jj_operation_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zip_path_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        writer
            .start_file::<_, ()>("safe.tex", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"safe").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        extract_zip(&bytes, temp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.path().join("safe.tex")).unwrap(),
            "safe"
        );
    }

    #[test]
    fn discovers_clone_from_nested_directory() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(STATE_DIR)).unwrap();
        ProjectBinding {
            project_id: "p1".into(),
            base_url: "https://example.test".into(),
            profile: DEFAULT_PROFILE.into(),
        }
        .save(temp.path())
        .unwrap();
        let nested = temp.path().join("chapters");
        std::fs::create_dir(&nested).unwrap();
        assert_eq!(discover_root(&nested).unwrap(), temp.path());
    }

    #[test]
    fn old_project_bindings_default_to_the_default_profile() {
        let binding: ProjectBinding =
            serde_json::from_str(r#"{"projectId":"p1","baseUrl":"https://example.test"}"#).unwrap();
        assert_eq!(binding.profile, DEFAULT_PROFILE);
    }
}
