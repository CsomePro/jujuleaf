use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Cursor, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::OverleafApi;
use crate::auth::{DEFAULT_PROFILE, Session};
use crate::jj::JjWorkspace;
use crate::operations::{
    BuildOptions, DocumentState, build_document_operations, minimal_text_changes,
    parse_document_snapshot,
};
use crate::project::{FileRef, collect_entities, connect_project_with_api};
use crate::socket::UpdateOptions;
use crate::store::{AssetCheckpoint, SyncStore, bytes_hash, content_hash};

const STATE_DIR: &str = ".jj/jujuleaf";
const METADATA_FILE: &str = ".jujuleaf/remote-metadata.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentMetadataSnapshot {
    path: String,
    remote_version: i64,
    ranges: Value,
    snapshot_metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteMetadataManifest {
    schema_version: u32,
    project_id: String,
    threads: Value,
    documents: BTreeMap<String, DocumentMetadataSnapshot>,
}

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
    pub binary_file_count: usize,
    pub metadata_path: String,
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
    pub binary_updated: Vec<String>,
    pub binary_added: Vec<String>,
    pub binary_local_only: Vec<String>,
    pub binary_unchanged: Vec<String>,
    pub binary_deleted: Vec<String>,
    pub binary_conflicts: Vec<String>,
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
    pub metadata_conflicts: Vec<String>,
    pub binary_pushed: Vec<String>,
    pub binary_added: Vec<String>,
    pub binary_unchanged: Vec<String>,
    pub binary_conflicts: Vec<String>,
    pub binary_unknown: Vec<String>,
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
    pub binary_modified: Vec<String>,
    pub binary_missing: Vec<String>,
    pub binary_clean: Vec<String>,
    pub binary_untracked: Vec<String>,
    pub unresolved_receipts: usize,
    pub jj_operation_id: String,
}

fn database(root: &Path) -> Result<SyncStore> {
    SyncStore::open(root.join(STATE_DIR).join("sync.sqlite3"))
}

fn local_path(root: &Path, remote_path: &str) -> Result<PathBuf> {
    let relative = Path::new(remote_path.trim_start_matches('/'));
    ensure!(!relative.as_os_str().is_empty(), "remote path is empty");
    ensure!(
        relative
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "unsafe remote path: {remote_path}"
    );
    Ok(root.join(relative))
}

fn snapshot_metadata(state: &DocumentState) -> Value {
    if let Value::Object(mut object) = state.raw.clone() {
        object.remove("content");
        Value::Object(object)
    } else {
        Value::Null
    }
}

fn metadata_parts(state: &DocumentState, ranges: &Value) -> Result<(Value, String)> {
    let snapshot = snapshot_metadata(state);
    let hash = bytes_hash(&serde_json::to_vec(&json!({
        "ranges": ranges,
        "snapshot": snapshot,
    }))?);
    Ok((snapshot, hash))
}

fn save_metadata_manifest(root: &Path, manifest: &RemoteMetadataManifest) -> Result<()> {
    let path = root.join(METADATA_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(manifest)?;
    if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
        std::fs::write(&path, bytes)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

fn cached_threads(root: &Path) -> Value {
    std::fs::read(root.join(METADATA_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<RemoteMetadataManifest>(&bytes).ok())
        .map(|manifest| manifest.threads)
        .unwrap_or(Value::Null)
}

fn zip_entries(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("invalid project zip")?;
    let mut entries = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("unsafe path in project zip: {}", entry.name()))?
            .to_string_lossy()
            .replace('\\', "/");
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut content)?;
        entries.insert(format!("/{}", relative.trim_start_matches('/')), content);
    }
    Ok(entries)
}

fn read_binary(root: &Path, remote_path: &str) -> Result<Option<Vec<u8>>> {
    let path = local_path(root, remote_path)?;
    if !path.exists() {
        return Ok(None);
    }
    std::fs::read(&path)
        .with_context(|| format!("failed to read {}", path.display()))
        .map(Some)
}

fn write_binary(root: &Path, remote_path: &str, content: &[u8]) -> Result<()> {
    let path = local_path(root, remote_path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))
}

fn is_binary_path(path: &str, content: &[u8]) -> bool {
    const BINARY_EXTENSIONS: &[&str] = &[
        "7z", "bmp", "doc", "docx", "eps", "gif", "gz", "ico", "jpeg", "jpg", "odp", "ods", "odt",
        "pdf", "png", "ppt", "pptx", "ps", "rar", "svgz", "tar", "tif", "tiff", "ttf", "webp",
        "xls", "xlsx", "zip",
    ];
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    BINARY_EXTENSIONS.contains(&extension.as_str())
        || content.contains(&0)
        || std::str::from_utf8(content).is_err()
}

fn collect_workspace_files(root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    fn walk(root: &Path, directory: &Path, files: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(root)?;
            if relative.components().next().is_some_and(|component| {
                component.as_os_str() == ".jj" || component.as_os_str() == ".jujuleaf"
            }) {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                walk(root, &path, files)?;
            } else if file_type.is_file() {
                let remote = format!("/{}", relative.to_string_lossy().replace('\\', "/"));
                files.insert(remote, std::fs::read(&path)?);
            }
        }
        Ok(())
    }

    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    Ok(files)
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
    let path = local_path(root, remote_path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))
}

fn read_document(root: &Path, remote_path: &str) -> Result<Option<String>> {
    let path = local_path(root, remote_path)?;
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
    let archive_entries = zip_entries(&zip)?;
    extract_zip(&zip, destination)?;

    let (mut socket, project) = connect_project_with_api(api, project_id).await?;
    let entities = collect_entities(&project);
    let documents = &entities.documents;
    let threads = api
        .threads(project_id)
        .await
        .unwrap_or_else(|_| cached_threads(destination));
    let mut states = Vec::new();
    let mut metadata_documents = BTreeMap::new();
    for document in documents {
        let joined = socket.join_doc(&document.id).await?;
        let state = parse_document_snapshot(joined.snapshot, &joined.ot_type)?;
        write_document(destination, &document.path, &state.content)?;
        let (snapshot_metadata, metadata_hash) = metadata_parts(&state, &joined.ranges)?;
        metadata_documents.insert(
            document.id.clone(),
            DocumentMetadataSnapshot {
                path: document.path.clone(),
                remote_version: joined.version,
                ranges: joined.ranges.clone(),
                snapshot_metadata: snapshot_metadata.clone(),
            },
        );
        socket.leave_doc(&document.id).await.ok();
        states.push((
            document.clone(),
            joined.version,
            content_hash(&state.content),
            metadata_hash,
            serde_json::to_string(&joined.ranges)?,
            serde_json::to_string(&snapshot_metadata)?,
        ));
    }
    socket.close().await.ok();

    save_metadata_manifest(
        destination,
        &RemoteMetadataManifest {
            schema_version: 1,
            project_id: project_id.to_owned(),
            threads,
            documents: metadata_documents,
        },
    )?;

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
    for (document, version, hash, metadata_hash, ranges_json, snapshot_metadata_json) in states {
        store.upsert_document(
            project_id,
            &document.id,
            &document.path,
            version,
            &hash,
            Some(&checkpoint.operation_id),
        )?;
        store.upsert_document_metadata(
            project_id,
            &document.id,
            version,
            &metadata_hash,
            &ranges_json,
            &snapshot_metadata_json,
        )?;
    }
    for file in &entities.files {
        let content = archive_entries
            .get(&file.path)
            .ok_or_else(|| anyhow!("project ZIP did not contain binary {}", file.path))?;
        store.upsert_asset(AssetCheckpoint {
            project_id,
            file_id: &file.id,
            path: &file.path,
            parent_folder_id: &file.parent_folder_id,
            remote_hash: &bytes_hash(content),
            size: content.len(),
            jj_operation_id: Some(&checkpoint.operation_id),
        })?;
    }
    Ok(CloneSummary {
        success: true,
        project_id: project_id.to_owned(),
        profile: profile.to_owned(),
        path: destination.display().to_string(),
        document_count: documents.len(),
        binary_file_count: entities.files.len(),
        metadata_path: METADATA_FILE.to_owned(),
        jj_operation_id: checkpoint.operation_id,
    })
}

pub async fn pull_project(root: &Path, session: &Session, profile: &str) -> Result<PullSummary> {
    let mut api = OverleafApi::new(session, None)?;
    pull_project_with_api(root, session, &mut api, profile).await
}

pub async fn pull_project_with_api(
    root: &Path,
    session: &Session,
    api: &mut OverleafApi,
    profile: &str,
) -> Result<PullSummary> {
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
    let archive_entries = zip_entries(&api.download_zip(&binding.project_id).await?)?;
    let (mut socket, project) = connect_project_with_api(api, &binding.project_id).await?;
    let entities = collect_entities(&project);
    let threads = api
        .threads(&binding.project_id)
        .await
        .unwrap_or_else(|_| cached_threads(root));
    let mut updated = Vec::new();
    let mut added = Vec::new();
    let mut local_only = Vec::new();
    let mut unchanged = Vec::new();
    let mut conflicts = Vec::new();
    let mut accepted = Vec::new();
    let mut metadata_documents = BTreeMap::new();

    for document in &entities.documents {
        let doc_id = document.id.clone();
        let joined = socket.join_doc(&document.id).await?;
        let state = parse_document_snapshot(joined.snapshot, &joined.ot_type)?;
        let (snapshot_metadata, metadata_hash) = metadata_parts(&state, &joined.ranges)?;
        let ranges_json = serde_json::to_string(&joined.ranges)?;
        let snapshot_metadata_json = serde_json::to_string(&snapshot_metadata)?;
        metadata_documents.insert(
            document.id.clone(),
            DocumentMetadataSnapshot {
                path: document.path.clone(),
                remote_version: joined.version,
                ranges: joined.ranges.clone(),
                snapshot_metadata: snapshot_metadata.clone(),
            },
        );
        let remote_hash = content_hash(&state.content);
        store.reconcile_confirmed_hash(&binding.project_id, &document.id, &remote_hash)?;
        let previous = store.document(&binding.project_id, &document.id)?;
        let mut local = read_document(root, &document.path)?;
        let mut relocated_from = None;
        if local.is_none()
            && let Some(previous) = &previous
            && previous.path != document.path
            && let Some(previous_local) = read_document(root, &previous.path)?
        {
            local = Some(previous_local);
            relocated_from = Some(previous.path.clone());
        }
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
            "local_only" => {
                if relocated_from.is_some() {
                    write_document(root, &document.path, local.as_deref().unwrap_or_default())?;
                }
                local_only.push(document.path.clone());
            }
            "unchanged" => {
                if relocated_from.is_some() {
                    write_document(root, &document.path, &state.content)?;
                }
                unchanged.push(document.path.clone());
            }
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
            if let Some(previous_path) = relocated_from {
                std::fs::remove_file(local_path(root, &previous_path)?)?;
            }
            accepted.push((
                document.clone(),
                joined.version,
                remote_hash,
                metadata_hash,
                ranges_json,
                snapshot_metadata_json,
            ));
        }
        socket.leave_doc(&doc_id).await.ok();
    }
    socket.close().await.ok();

    let mut binary_updated = Vec::new();
    let mut binary_added = Vec::new();
    let mut binary_local_only = Vec::new();
    let mut binary_unchanged = Vec::new();
    let mut binary_deleted = Vec::new();
    let mut binary_conflicts = Vec::new();
    let mut accepted_assets = Vec::new();
    let remote_file_ids: BTreeSet<_> = entities.files.iter().map(|file| file.id.as_str()).collect();

    for file in &entities.files {
        let Some(remote) = archive_entries.get(&file.path) else {
            binary_conflicts.push(file.path.clone());
            continue;
        };
        let remote_hash = bytes_hash(remote);
        let previous = store.asset(&binding.project_id, &file.id)?;
        let mut local = read_binary(root, &file.path)?;
        let mut relocated_from = None;
        if local.is_none()
            && let Some(previous) = &previous
            && previous.path != file.path
            && let Some(previous_local) = read_binary(root, &previous.path)?
        {
            local = Some(previous_local);
            relocated_from = Some(previous.path.clone());
        }
        let action = match (&local, &previous) {
            (None, _) => "added",
            (Some(local), Some(previous)) => {
                let local_hash = bytes_hash(local);
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
            (Some(local), None) if bytes_hash(local) == remote_hash => "unchanged",
            (Some(_), None) => "conflict",
        };
        match action {
            "added" => {
                write_binary(root, &file.path, remote)?;
                binary_added.push(file.path.clone());
            }
            "updated" => {
                write_binary(root, &file.path, remote)?;
                binary_updated.push(file.path.clone());
            }
            "local_only" => {
                if relocated_from.is_some() {
                    write_binary(root, &file.path, local.as_deref().unwrap_or_default())?;
                }
                binary_local_only.push(file.path.clone());
            }
            "unchanged" => {
                if relocated_from.is_some() {
                    write_binary(root, &file.path, remote)?;
                }
                binary_unchanged.push(file.path.clone());
            }
            "conflict" => {
                let incoming = root.join(STATE_DIR).join("incoming-assets").join(&file.id);
                if let Some(parent) = incoming.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(incoming, remote)?;
                binary_conflicts.push(file.path.clone());
            }
            _ => unreachable!(),
        }
        if action != "conflict" {
            if let Some(previous_path) = relocated_from {
                std::fs::remove_file(local_path(root, &previous_path)?)?;
            }
            accepted_assets.push((file.clone(), remote_hash, remote.len()));
        }
    }

    for previous in store.assets(&binding.project_id)? {
        if remote_file_ids.contains(previous.file_id.as_str()) {
            continue;
        }
        match read_binary(root, &previous.path)? {
            Some(local) if bytes_hash(&local) != previous.remote_hash => {
                binary_conflicts.push(previous.path);
            }
            Some(_) => {
                std::fs::remove_file(local_path(root, &previous.path)?)?;
                binary_deleted.push(previous.path.clone());
                store.remove_asset(&binding.project_id, &previous.file_id)?;
            }
            None => {
                binary_deleted.push(previous.path.clone());
                store.remove_asset(&binding.project_id, &previous.file_id)?;
            }
        }
    }

    save_metadata_manifest(
        root,
        &RemoteMetadataManifest {
            schema_version: 1,
            project_id: binding.project_id.clone(),
            threads,
            documents: metadata_documents,
        },
    )?;
    let checkpoint = workspace
        .checkpoint(&format!("pull Overleaf project {}", binding.project_id))
        .await?;
    for (document, version, hash, metadata_hash, ranges_json, snapshot_metadata_json) in accepted {
        store.upsert_document(
            &binding.project_id,
            &document.id,
            &document.path,
            version,
            &hash,
            Some(&checkpoint.operation_id),
        )?;
        store.upsert_document_metadata(
            &binding.project_id,
            &document.id,
            version,
            &metadata_hash,
            &ranges_json,
            &snapshot_metadata_json,
        )?;
    }
    for (file, hash, size) in accepted_assets {
        store.upsert_asset(AssetCheckpoint {
            project_id: &binding.project_id,
            file_id: &file.id,
            path: &file.path,
            parent_folder_id: &file.parent_folder_id,
            remote_hash: &hash,
            size,
            jj_operation_id: Some(&checkpoint.operation_id),
        })?;
    }
    Ok(PullSummary {
        success: conflicts.is_empty() && binary_conflicts.is_empty(),
        project_id: binding.project_id,
        profile: binding.profile,
        updated,
        added,
        local_only,
        unchanged,
        conflicts,
        binary_updated,
        binary_added,
        binary_local_only,
        binary_unchanged,
        binary_deleted,
        binary_conflicts,
        jj_operation_id: checkpoint.operation_id,
    })
}

pub async fn push_project(
    root: &Path,
    session: &Session,
    profile: &str,
    options: &UpdateOptions,
) -> Result<PushSummary> {
    let mut api = OverleafApi::new(session, None)?;
    push_project_with_api(root, session, &mut api, profile, options).await
}

pub async fn push_project_with_api(
    root: &Path,
    session: &Session,
    api: &mut OverleafApi,
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
    let (mut socket, project) = connect_project_with_api(api, &binding.project_id).await?;
    let entities = collect_entities(&project);
    let threads = api
        .threads(&binding.project_id)
        .await
        .unwrap_or_else(|_| cached_threads(root));
    let mut pushed = Vec::new();
    let mut unchanged = Vec::new();
    let mut conflicts = Vec::new();
    let mut unknown = Vec::new();
    let mut metadata_conflicts = Vec::new();
    let mut metadata_documents = BTreeMap::new();

    for document in &entities.documents {
        let Some(local) = read_document(root, &document.path)? else {
            conflicts.push(document.path.clone());
            continue;
        };
        let joined = socket.join_doc(&document.id).await?;
        let state = parse_document_snapshot(joined.snapshot, &joined.ot_type)?;
        let (snapshot_metadata, metadata_hash) = metadata_parts(&state, &joined.ranges)?;
        metadata_documents.insert(
            document.id.clone(),
            DocumentMetadataSnapshot {
                path: document.path.clone(),
                remote_version: joined.version,
                ranges: joined.ranges.clone(),
                snapshot_metadata: snapshot_metadata.clone(),
            },
        );
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
            store.upsert_document_metadata(
                &binding.project_id,
                &document.id,
                joined.version,
                &metadata_hash,
                &serde_json::to_string(&joined.ranges)?,
                &serde_json::to_string(&snapshot_metadata)?,
            )?;
            unchanged.push(document.path.clone());
            socket.leave_doc(&document.id).await.ok();
            continue;
        }
        let Some(previous) = store.document(&binding.project_id, &document.id)? else {
            conflicts.push(document.path.clone());
            socket.leave_doc(&document.id).await.ok();
            continue;
        };
        if remote_hash != previous.remote_hash {
            conflicts.push(document.path.clone());
            socket.leave_doc(&document.id).await.ok();
            continue;
        }
        let Some(previous_metadata) = store.document_metadata(&binding.project_id, &document.id)?
        else {
            metadata_conflicts.push(document.path.clone());
            socket.leave_doc(&document.id).await.ok();
            continue;
        };
        if previous_metadata.metadata_hash != metadata_hash {
            metadata_conflicts.push(document.path.clone());
            socket.leave_doc(&document.id).await.ok();
            continue;
        }
        let changes = minimal_text_changes(&state.content, &local);
        let built = build_document_operations(&state, &changes, &BuildOptions::default())?;
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
            Ok(_confirmation) => {
                store.mark_confirmed(&receipt)?;
                socket.leave_doc(&document.id).await.ok();
                let refreshed = socket.join_doc(&document.id).await?;
                let refreshed_state =
                    parse_document_snapshot(refreshed.snapshot, &refreshed.ot_type)?;
                let refreshed_hash = content_hash(&refreshed_state.content);
                if refreshed_hash == local_hash {
                    let (refreshed_snapshot_metadata, refreshed_metadata_hash) =
                        metadata_parts(&refreshed_state, &refreshed.ranges)?;
                    store.upsert_document(
                        &binding.project_id,
                        &document.id,
                        &document.path,
                        refreshed.version,
                        &local_hash,
                        Some(&checkpoint.operation_id),
                    )?;
                    store.upsert_document_metadata(
                        &binding.project_id,
                        &document.id,
                        refreshed.version,
                        &refreshed_metadata_hash,
                        &serde_json::to_string(&refreshed.ranges)?,
                        &serde_json::to_string(&refreshed_snapshot_metadata)?,
                    )?;
                    metadata_documents.insert(
                        document.id.clone(),
                        DocumentMetadataSnapshot {
                            path: document.path.clone(),
                            remote_version: refreshed.version,
                            ranges: refreshed.ranges.clone(),
                            snapshot_metadata: refreshed_snapshot_metadata,
                        },
                    );
                    pushed.push(document.path.clone());
                } else {
                    unknown.push(document.path.clone());
                }
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
                    let (refreshed_snapshot_metadata, refreshed_metadata_hash) =
                        metadata_parts(&refreshed_state, &refreshed.ranges)?;
                    store.upsert_document(
                        &binding.project_id,
                        &document.id,
                        &document.path,
                        refreshed.version,
                        &local_hash,
                        Some(&checkpoint.operation_id),
                    )?;
                    store.upsert_document_metadata(
                        &binding.project_id,
                        &document.id,
                        refreshed.version,
                        &refreshed_metadata_hash,
                        &serde_json::to_string(&refreshed.ranges)?,
                        &serde_json::to_string(&refreshed_snapshot_metadata)?,
                    )?;
                    metadata_documents.insert(
                        document.id.clone(),
                        DocumentMetadataSnapshot {
                            path: document.path.clone(),
                            remote_version: refreshed.version,
                            ranges: refreshed.ranges.clone(),
                            snapshot_metadata: refreshed_snapshot_metadata,
                        },
                    );
                    pushed.push(document.path.clone());
                } else {
                    unknown.push(document.path.clone());
                }
            }
        }
        socket.leave_doc(&document.id).await.ok();
    }
    socket.close().await.ok();

    let mut binary_pushed = Vec::new();
    let mut binary_added = Vec::new();
    let mut binary_unchanged = Vec::new();
    let mut binary_conflicts = Vec::new();
    let mut binary_unknown = Vec::new();
    let mut accepted_assets = Vec::new();

    for file in &entities.files {
        let remote = api.download_file(&binding.project_id, &file.id).await?;
        let remote_hash = bytes_hash(&remote);
        let Some(local) = read_binary(root, &file.path)? else {
            binary_conflicts.push(file.path.clone());
            continue;
        };
        let local_hash = bytes_hash(&local);
        if local_hash == remote_hash {
            accepted_assets.push((file.clone(), remote_hash, local.len()));
            binary_unchanged.push(file.path.clone());
            continue;
        }
        let Some(previous) = store.asset(&binding.project_id, &file.id)? else {
            binary_conflicts.push(file.path.clone());
            continue;
        };
        if previous.remote_hash != remote_hash {
            binary_conflicts.push(file.path.clone());
            continue;
        }
        let path = local_path(root, &file.path)?;
        match api
            .replace_file_protected(
                &binding.project_id,
                &file.id,
                &file.parent_folder_id,
                &path,
                &file.name,
            )
            .await
        {
            Ok(response) => {
                let Some(new_id) = response.get("entity_id").and_then(Value::as_str) else {
                    binary_unknown.push(file.path.clone());
                    continue;
                };
                store.remove_asset(&binding.project_id, &file.id)?;
                accepted_assets.push((
                    FileRef {
                        id: new_id.to_owned(),
                        path: file.path.clone(),
                        name: file.name.clone(),
                        parent_folder_id: file.parent_folder_id.clone(),
                    },
                    local_hash,
                    local.len(),
                ));
                binary_pushed.push(file.path.clone());
            }
            Err(_) => binary_unknown.push(file.path.clone()),
        }
    }

    let document_paths: BTreeSet<_> = entities
        .documents
        .iter()
        .map(|document| document.path.as_str())
        .collect();
    let remote_file_paths: BTreeSet<_> = entities
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    let folders: BTreeMap<_, _> = entities
        .folders
        .iter()
        .map(|folder| (folder.path.as_str(), folder.id.as_str()))
        .collect();
    let new_file_candidates = if binary_conflicts.is_empty() {
        collect_workspace_files(root)?
    } else {
        BTreeMap::new()
    };
    for (path, content) in new_file_candidates {
        if document_paths.contains(path.as_str())
            || remote_file_paths.contains(path.as_str())
            || !is_binary_path(&path, &content)
        {
            continue;
        }
        let (parent_path, name) = path
            .rsplit_once('/')
            .map(|(parent, name)| (if parent.is_empty() { "/" } else { parent }, name))
            .ok_or_else(|| anyhow!("invalid local path: {path}"))?;
        let Some(folder_id) = folders.get(parent_path) else {
            binary_conflicts.push(path);
            continue;
        };
        let local_file = local_path(root, &path)?;
        match api
            .upload(&binding.project_id, folder_id, &local_file, name)
            .await
        {
            Ok(response) => {
                let Some(file_id) = response.get("entity_id").and_then(Value::as_str) else {
                    binary_unknown.push(path);
                    continue;
                };
                accepted_assets.push((
                    FileRef {
                        id: file_id.to_owned(),
                        path: path.clone(),
                        name: name.to_owned(),
                        parent_folder_id: (*folder_id).to_owned(),
                    },
                    bytes_hash(&content),
                    content.len(),
                ));
                binary_added.push(path);
            }
            Err(_) => binary_unknown.push(path),
        }
    }

    save_metadata_manifest(
        root,
        &RemoteMetadataManifest {
            schema_version: 1,
            project_id: binding.project_id.clone(),
            threads,
            documents: metadata_documents,
        },
    )?;
    let final_checkpoint = workspace
        .checkpoint(&format!("push Overleaf project {}", binding.project_id))
        .await?;
    for (file, hash, size) in accepted_assets {
        store.upsert_asset(AssetCheckpoint {
            project_id: &binding.project_id,
            file_id: &file.id,
            path: &file.path,
            parent_folder_id: &file.parent_folder_id,
            remote_hash: &hash,
            size,
            jj_operation_id: Some(&final_checkpoint.operation_id),
        })?;
    }
    Ok(PushSummary {
        success: conflicts.is_empty()
            && unknown.is_empty()
            && metadata_conflicts.is_empty()
            && binary_conflicts.is_empty()
            && binary_unknown.is_empty(),
        project_id: binding.project_id,
        profile: binding.profile,
        pushed,
        unchanged,
        conflicts,
        unknown,
        metadata_conflicts,
        binary_pushed,
        binary_added,
        binary_unchanged,
        binary_conflicts,
        binary_unknown,
        jj_operation_id: final_checkpoint.operation_id,
    })
}

pub async fn local_status(root: &Path) -> Result<StatusSummary> {
    let binding = ProjectBinding::load(root)?;
    let store = database(root)?;
    let jj_operation_id = JjWorkspace::open(root).await?.operation_id();
    let mut modified = Vec::new();
    let mut missing = Vec::new();
    let mut clean = Vec::new();
    let mut binary_modified = Vec::new();
    let mut binary_missing = Vec::new();
    let mut binary_clean = Vec::new();
    let mut binary_untracked = Vec::new();

    let unresolved = store.unresolved_operations(&binding.project_id, None)?;
    let documents = store.documents(&binding.project_id)?;
    let document_paths: BTreeSet<_> = documents
        .iter()
        .map(|document| document.path.clone())
        .collect();
    for document in documents {
        match read_document(root, &document.path)? {
            None => missing.push(document.path),
            Some(content) if content_hash(&content) == document.remote_hash => {
                clean.push(document.path)
            }
            Some(_) => modified.push(document.path),
        }
    }
    let assets = store.assets(&binding.project_id)?;
    let asset_paths: BTreeSet<_> = assets.iter().map(|asset| asset.path.clone()).collect();
    for asset in assets {
        match read_binary(root, &asset.path)? {
            None => binary_missing.push(asset.path),
            Some(content) if bytes_hash(&content) == asset.remote_hash => {
                binary_clean.push(asset.path)
            }
            Some(_) => binary_modified.push(asset.path),
        }
    }
    for (path, content) in collect_workspace_files(root)? {
        if !document_paths.contains(path.as_str())
            && !asset_paths.contains(path.as_str())
            && is_binary_path(&path, &content)
        {
            binary_untracked.push(path);
        }
    }
    Ok(StatusSummary {
        project_id: binding.project_id,
        profile: binding.profile,
        modified,
        missing,
        clean,
        binary_modified,
        binary_missing,
        binary_clean,
        binary_untracked,
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

    #[test]
    fn metadata_snapshot_excludes_document_text_but_keeps_tracked_changes() {
        let state = parse_document_snapshot(
            json!({
                "content": "abc",
                "trackedChanges": [{"range":{"pos":1,"length":1},"tracking":{"type":"delete"}}],
                "comments": []
            }),
            crate::operations::HISTORY_OT,
        )
        .unwrap();
        let metadata = snapshot_metadata(&state);
        assert!(metadata.get("content").is_none());
        assert!(metadata.get("trackedChanges").is_some());
        let (_, first_hash) = metadata_parts(&state, &json!([])).unwrap();
        let (_, second_hash) = metadata_parts(&state, &json!([{"id":"comment"}])).unwrap();
        assert_ne!(first_hash, second_hash);
    }

    #[test]
    fn workspace_binary_scan_ignores_private_state() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".jj/jujuleaf")).unwrap();
        std::fs::create_dir_all(temp.path().join(".jujuleaf")).unwrap();
        std::fs::create_dir_all(temp.path().join("figures")).unwrap();
        std::fs::write(temp.path().join(".jj/jujuleaf/state"), b"secret").unwrap();
        std::fs::write(temp.path().join(".jujuleaf/remote-metadata.json"), b"{}").unwrap();
        std::fs::write(temp.path().join("figures/plot.png"), [0, 1, 2]).unwrap();
        let files = collect_workspace_files(temp.path()).unwrap();
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            vec!["/figures/plot.png"]
        );
        assert!(is_binary_path(
            "/figures/plot.png",
            files.values().next().unwrap()
        ));
    }

    #[test]
    fn unsafe_remote_paths_never_escape_the_clone() {
        let temp = tempfile::tempdir().unwrap();
        assert!(local_path(temp.path(), "../outside").is_err());
        assert!(local_path(temp.path(), "/safe/file.png").is_ok());
    }
}
