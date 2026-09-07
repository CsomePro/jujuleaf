use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Cursor, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::TextDiff;

use crate::api::OverleafApi;
use crate::auth::{DEFAULT_PROFILE, Session};
use crate::ignore;
use crate::jj::JjWorkspace;
use crate::operations::{
    BuildOptions, DocumentState, build_document_operations, minimal_text_changes,
    parse_document_snapshot,
};
use crate::project::{FileRef, collect_entities, connect_project_with_api};
use crate::socket::UpdateOptions;
use crate::store::{AssetCheckpoint, SyncStore, bytes_hash, content_hash};

const STATE_DIR: &str = ".jj/jujuleaf";
const CONFLICTS_FILE: &str = ".jj/jujuleaf/conflicts.json";
const METADATA_FILE: &str = ".jujuleaf/remote-metadata.json";
const PROJECT_CONTEXT_FILE: &str = ".jujuleaf/project.json";

pub(crate) struct WorkspaceOperationLock {
    _file: File,
}

impl WorkspaceOperationLock {
    pub(crate) fn acquire(root: &Path, operation: &str) -> Result<Self> {
        let state_dir = root.join(STATE_DIR);
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("failed to create {}", state_dir.display()))?;
        let path = state_dir.join("workspace.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => bail!(
                "cannot run {operation}: another JujuLeaf operation is already running in {}",
                root.display()
            ),
            Err(TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("failed to lock {}", path.display()))
            }
        }
    }
}

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
struct ProjectContextManifest {
    schema_version: u32,
    project_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectContext {
    pub root: PathBuf,
    pub project_id: String,
    pub profile: Option<String>,
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

fn save_project_context(root: &Path, project_id: &str) -> Result<()> {
    let path = root.join(PROJECT_CONTEXT_FILE);
    std::fs::create_dir_all(path.parent().expect("context has parent"))?;
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&ProjectContextManifest {
            schema_version: 1,
            project_id: project_id.to_owned(),
        })?,
    )
    .with_context(|| format!("failed to write {}", path.display()))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    Document,
    Asset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictRemoteState {
    Present,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredConflict {
    project_id: String,
    kind: ConflictKind,
    entity_id: String,
    path: String,
    remote_state: ConflictRemoteState,
    #[serde(default)]
    remote_version: Option<i64>,
    #[serde(default)]
    remote_hash: Option<String>,
    #[serde(default)]
    base_hash: Option<String>,
    #[serde(default)]
    parent_folder_id: Option<String>,
    #[serde(default)]
    metadata_hash: Option<String>,
    #[serde(default)]
    ranges_json: Option<String>,
    #[serde(default)]
    snapshot_metadata_json: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConflictManifest {
    schema_version: u32,
    conflicts: Vec<StoredConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictSummary {
    pub kind: ConflictKind,
    pub path: String,
    pub remote_state: ConflictRemoteState,
    pub local_exists: bool,
    pub incoming_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictListSummary {
    pub project_id: String,
    pub conflict_count: usize,
    pub conflicts: Vec<ConflictSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictDetail {
    pub project_id: String,
    pub kind: ConflictKind,
    pub path: String,
    pub remote_state: ConflictRemoteState,
    pub base_hash: Option<String>,
    pub local_hash: Option<String>,
    pub remote_hash: Option<String>,
    pub binary: bool,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictResolution {
    Ours,
    Theirs,
    Merged(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictResolveSummary {
    pub success: bool,
    pub project_id: String,
    pub path: String,
    pub resolution: String,
    pub remaining_conflicts: usize,
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
    pub ignored: Vec<String>,
    pub conflicts: Vec<ConflictSummary>,
    pub unresolved_receipts: usize,
    pub jj_operation_id: String,
}

fn database(root: &Path) -> Result<SyncStore> {
    SyncStore::open(root.join(STATE_DIR).join("sync.sqlite3"))
}

fn conflict_manifest_path(root: &Path) -> PathBuf {
    root.join(CONFLICTS_FILE)
}

fn load_conflict_manifest(root: &Path) -> Result<ConflictManifest> {
    let path = conflict_manifest_path(root);
    if !path.exists() {
        return Ok(ConflictManifest {
            schema_version: 1,
            conflicts: Vec::new(),
        });
    }
    let manifest: ConflictManifest = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("invalid {}", path.display()))?;
    ensure!(
        manifest.schema_version == 1,
        "unsupported conflict manifest schema {}",
        manifest.schema_version
    );
    Ok(manifest)
}

fn save_conflict_manifest(root: &Path, manifest: &ConflictManifest) -> Result<()> {
    let path = conflict_manifest_path(root);
    if manifest.conflicts.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(manifest)?)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .with_context(|| format!("failed to replace {}", path.display()))
}

fn incoming_path(root: &Path, conflict: &StoredConflict) -> PathBuf {
    let directory = match conflict.kind {
        ConflictKind::Document => "incoming",
        ConflictKind::Asset => "incoming-assets",
    };
    root.join(STATE_DIR)
        .join(directory)
        .join(hex::encode(conflict.entity_id.as_bytes()))
}

fn upsert_conflict(manifest: &mut ConflictManifest, conflict: StoredConflict) {
    manifest.conflicts.retain(|existing| {
        !(existing.kind == conflict.kind
            && (existing.entity_id == conflict.entity_id || existing.path == conflict.path))
    });
    manifest.conflicts.push(conflict);
    manifest
        .conflicts
        .sort_by(|left, right| left.path.cmp(&right.path));
}

fn clear_conflict(
    root: &Path,
    manifest: &mut ConflictManifest,
    kind: ConflictKind,
    entity_id: &str,
    path: &str,
) {
    let removed: Vec<_> = manifest
        .conflicts
        .iter()
        .filter(|conflict| {
            conflict.kind == kind && (conflict.entity_id == entity_id || conflict.path == path)
        })
        .cloned()
        .collect();
    manifest.conflicts.retain(|conflict| {
        !(conflict.kind == kind && (conflict.entity_id == entity_id || conflict.path == path))
    });
    for conflict in removed {
        std::fs::remove_file(incoming_path(root, &conflict)).ok();
    }
}

fn record_conflict(
    root: &Path,
    manifest: &mut ConflictManifest,
    conflict: StoredConflict,
    incoming: Option<&[u8]>,
) -> Result<()> {
    clear_conflict(
        root,
        manifest,
        conflict.kind,
        &conflict.entity_id,
        &conflict.path,
    );
    match conflict.remote_state {
        ConflictRemoteState::Present => {
            let incoming =
                incoming.ok_or_else(|| anyhow!("present conflict has no incoming content"))?;
            let path = incoming_path(root, &conflict);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, incoming)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
        ConflictRemoteState::Deleted => {}
    }
    upsert_conflict(manifest, conflict);
    save_conflict_manifest(root, manifest)
}

fn find_conflict<'a>(manifest: &'a ConflictManifest, path: &str) -> Result<&'a StoredConflict> {
    let path = path.trim_start_matches('/');
    let matches: Vec<_> = manifest
        .conflicts
        .iter()
        .filter(|conflict| conflict.path.trim_start_matches('/') == path)
        .collect();
    ensure!(!matches.is_empty(), "no unresolved conflict for {path}");
    ensure!(
        matches.len() == 1,
        "multiple unresolved conflicts match {path}"
    );
    Ok(matches[0])
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

struct WorkspaceScan {
    files: BTreeMap<String, Vec<u8>>,
    ignored: Vec<String>,
}

fn scan_workspace_files(root: &Path) -> Result<WorkspaceScan> {
    fn walk(
        root: &Path,
        directory: &Path,
        rules: &jj_lib::gitignore::GitIgnoreFile,
        files: &mut BTreeMap<String, Vec<u8>>,
        ignored: &mut Vec<String>,
    ) -> Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(root)?;
            if ignore::is_private_path(relative) {
                continue;
            }
            let file_type = entry.file_type()?;
            let repo_path = ignore::repo_path(relative)?;
            if file_type.is_dir() {
                if rules.matches_dir(&repo_path) {
                    ignored.push(format!(
                        "{}/",
                        relative.to_string_lossy().replace('\\', "/")
                    ));
                } else {
                    walk(root, &path, rules, files, ignored)?;
                }
            } else if file_type.is_file() {
                let normalized = relative.to_string_lossy().replace('\\', "/");
                if rules.matches_file(&repo_path) {
                    ignored.push(normalized);
                } else {
                    files.insert(format!("/{normalized}"), std::fs::read(&path)?);
                }
            }
        }
        Ok(())
    }

    let rules = ignore::load(root)?;
    let mut files = BTreeMap::new();
    let mut ignored = Vec::new();
    walk(root, root, &rules, &mut files, &mut ignored)?;
    ignored.sort();
    Ok(WorkspaceScan { files, ignored })
}

pub(crate) fn collect_workspace_files(root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    Ok(scan_workspace_files(root)?.files)
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

pub fn discover_project_context(start: impl AsRef<Path>) -> Result<Option<ProjectContext>> {
    let mut current = start
        .as_ref()
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", start.as_ref().display()))?;
    loop {
        let binding_path = ProjectBinding::path(&current);
        if binding_path.exists() {
            let binding = ProjectBinding::load(&current)?;
            return Ok(Some(ProjectContext {
                root: current,
                project_id: binding.project_id,
                profile: Some(binding.profile),
            }));
        }

        let context_path = current.join(PROJECT_CONTEXT_FILE);
        if context_path.exists() {
            let context: ProjectContextManifest = serde_json::from_slice(
                &std::fs::read(&context_path)
                    .with_context(|| format!("failed to read {}", context_path.display()))?,
            )
            .with_context(|| format!("invalid {}", context_path.display()))?;
            ensure!(
                context.schema_version == 1,
                "unsupported schemaVersion {} in {}",
                context.schema_version,
                context_path.display()
            );
            ensure!(
                !context.project_id.trim().is_empty(),
                "projectId is empty in {}",
                context_path.display()
            );
            return Ok(Some(ProjectContext {
                root: current,
                project_id: context.project_id,
                profile: None,
            }));
        }

        let metadata_path = current.join(METADATA_FILE);
        if metadata_path.exists() {
            let metadata: Value = serde_json::from_slice(
                &std::fs::read(&metadata_path)
                    .with_context(|| format!("failed to read {}", metadata_path.display()))?,
            )
            .with_context(|| format!("invalid {}", metadata_path.display()))?;
            let project_id = metadata
                .get("projectId")
                .and_then(Value::as_str)
                .filter(|project_id| !project_id.trim().is_empty())
                .ok_or_else(|| anyhow!("projectId is missing in {}", metadata_path.display()))?;
            return Ok(Some(ProjectContext {
                root: current,
                project_id: project_id.to_owned(),
                profile: None,
            }));
        }

        if !current.pop() {
            return Ok(None);
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

fn conflict_summaries(root: &Path, manifest: &ConflictManifest) -> Result<Vec<ConflictSummary>> {
    manifest
        .conflicts
        .iter()
        .map(|conflict| {
            Ok(ConflictSummary {
                kind: conflict.kind,
                path: conflict.path.clone(),
                remote_state: conflict.remote_state,
                local_exists: local_path(root, &conflict.path)?.exists(),
                incoming_exists: incoming_path(root, conflict).exists(),
            })
        })
        .collect()
}

pub fn list_conflicts(root: &Path) -> Result<ConflictListSummary> {
    let binding = ProjectBinding::load(root)?;
    let manifest = load_conflict_manifest(root)?;
    let conflicts = conflict_summaries(root, &manifest)?;
    Ok(ConflictListSummary {
        project_id: binding.project_id,
        conflict_count: conflicts.len(),
        conflicts,
    })
}

pub fn show_conflict(root: &Path, path: &str) -> Result<ConflictDetail> {
    let binding = ProjectBinding::load(root)?;
    let manifest = load_conflict_manifest(root)?;
    let conflict = find_conflict(&manifest, path)?;
    ensure!(
        conflict.project_id == binding.project_id,
        "conflict belongs to a different project"
    );
    let local = std::fs::read(local_path(root, &conflict.path)?).ok();
    let remote =
        match conflict.remote_state {
            ConflictRemoteState::Present => {
                let path = incoming_path(root, conflict);
                Some(std::fs::read(&path).with_context(|| {
                    format!("missing incoming conflict copy {}", path.display())
                })?)
            }
            ConflictRemoteState::Deleted => None,
        };
    if let (Some(expected), Some(remote)) = (&conflict.remote_hash, &remote) {
        ensure!(
            bytes_hash(remote) == *expected,
            "incoming conflict copy for {} was modified; run pull again",
            conflict.path
        );
    }
    let binary = conflict.kind == ConflictKind::Asset;
    let diff = if binary {
        String::new()
    } else {
        let local = local
            .as_deref()
            .map(std::str::from_utf8)
            .transpose()
            .context("local conflict document is not UTF-8")?
            .unwrap_or_default();
        let remote = remote
            .as_deref()
            .map(std::str::from_utf8)
            .transpose()
            .context("incoming conflict document is not UTF-8")?
            .unwrap_or_default();
        TextDiff::from_lines(local, remote)
            .unified_diff()
            .context_radius(3)
            .header(
                &format!("local/{}", conflict.path.trim_start_matches('/')),
                &format!("remote/{}", conflict.path.trim_start_matches('/')),
            )
            .to_string()
    };
    Ok(ConflictDetail {
        project_id: binding.project_id,
        kind: conflict.kind,
        path: conflict.path.clone(),
        remote_state: conflict.remote_state,
        base_hash: conflict.base_hash.clone(),
        local_hash: local.as_deref().map(bytes_hash),
        remote_hash: conflict.remote_hash.clone(),
        binary,
        diff,
    })
}

pub async fn resolve_conflict(
    root: &Path,
    path: &str,
    resolution: ConflictResolution,
) -> Result<ConflictResolveSummary> {
    let binding = ProjectBinding::load(root)?;
    let mut manifest = load_conflict_manifest(root)?;
    let conflict = find_conflict(&manifest, path)?.clone();
    ensure!(
        conflict.project_id == binding.project_id,
        "conflict belongs to a different project"
    );
    let store = database(root)?;
    ensure!(
        store
            .unresolved_operations(&binding.project_id, None)?
            .is_empty(),
        "resolve uncertain remote writes before resolving conflicts"
    );
    match (conflict.kind, conflict.remote_state) {
        (ConflictKind::Document, ConflictRemoteState::Present) => {
            ensure!(
                conflict.remote_version.is_some()
                    && conflict.remote_hash.is_some()
                    && conflict.metadata_hash.is_some()
                    && conflict.ranges_json.is_some()
                    && conflict.snapshot_metadata_json.is_some(),
                "document conflict metadata is incomplete; run pull again"
            );
        }
        (ConflictKind::Document, ConflictRemoteState::Deleted) => {
            bail!("deleted remote document conflicts are not supported")
        }
        (ConflictKind::Asset, ConflictRemoteState::Present) => {
            ensure!(
                conflict.remote_hash.is_some() && conflict.parent_folder_id.is_some(),
                "asset conflict metadata is incomplete; run pull again"
            );
        }
        (ConflictKind::Asset, ConflictRemoteState::Deleted) => {}
    }
    let incoming = match conflict.remote_state {
        ConflictRemoteState::Present => {
            let path = incoming_path(root, &conflict);
            let contents = std::fs::read(&path)
                .with_context(|| format!("missing incoming conflict copy {}", path.display()))?;
            let expected = conflict
                .remote_hash
                .as_deref()
                .ok_or_else(|| anyhow!("conflict has no remote hash"))?;
            ensure!(
                bytes_hash(&contents) == expected,
                "incoming conflict copy for {} was modified; run pull again",
                conflict.path
            );
            Some(contents)
        }
        ConflictRemoteState::Deleted => None,
    };
    let local = local_path(root, &conflict.path)?;
    let (selected, resolution_name) = match &resolution {
        ConflictResolution::Ours => {
            (
                Some(std::fs::read(&local).with_context(|| {
                    format!("local conflict copy is missing: {}", local.display())
                })?),
                "ours",
            )
        }
        ConflictResolution::Theirs => (incoming.clone(), "theirs"),
        ConflictResolution::Merged(path) => (
            Some(
                std::fs::read(path)
                    .with_context(|| format!("failed to read merged file {}", path.display()))?,
            ),
            "merged",
        ),
    };

    match (&selected, conflict.kind) {
        (Some(contents), ConflictKind::Document) => {
            let contents =
                std::str::from_utf8(contents).context("resolved document must be UTF-8")?;
            write_document(root, &conflict.path, contents)?;
        }
        (Some(contents), ConflictKind::Asset) => {
            write_binary(root, &conflict.path, contents)?;
        }
        (None, _) => {
            if local.exists() {
                std::fs::remove_file(&local)
                    .with_context(|| format!("failed to remove {}", local.display()))?;
            }
        }
    }

    let mut workspace = JjWorkspace::open(root).await?;
    let checkpoint = workspace
        .checkpoint(&format!(
            "resolve {} conflict using {resolution_name}",
            conflict.path
        ))
        .await?;
    match (conflict.kind, conflict.remote_state) {
        (ConflictKind::Document, ConflictRemoteState::Present) => {
            store.upsert_document(
                &binding.project_id,
                &conflict.entity_id,
                &conflict.path,
                conflict
                    .remote_version
                    .ok_or_else(|| anyhow!("document conflict has no remote version"))?,
                conflict
                    .remote_hash
                    .as_deref()
                    .ok_or_else(|| anyhow!("document conflict has no remote hash"))?,
                Some(&checkpoint.operation_id),
            )?;
            store.upsert_document_metadata(
                &binding.project_id,
                &conflict.entity_id,
                conflict.remote_version.unwrap(),
                conflict
                    .metadata_hash
                    .as_deref()
                    .ok_or_else(|| anyhow!("document conflict has no metadata hash"))?,
                conflict
                    .ranges_json
                    .as_deref()
                    .ok_or_else(|| anyhow!("document conflict has no ranges"))?,
                conflict
                    .snapshot_metadata_json
                    .as_deref()
                    .ok_or_else(|| anyhow!("document conflict has no snapshot metadata"))?,
            )?;
        }
        (ConflictKind::Document, ConflictRemoteState::Deleted) => unreachable!(),
        (ConflictKind::Asset, ConflictRemoteState::Present) => {
            let remote = incoming
                .as_deref()
                .expect("present conflict has incoming data");
            store.upsert_asset(AssetCheckpoint {
                project_id: &binding.project_id,
                file_id: &conflict.entity_id,
                path: &conflict.path,
                parent_folder_id: conflict
                    .parent_folder_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("asset conflict has no parent folder"))?,
                remote_hash: conflict
                    .remote_hash
                    .as_deref()
                    .ok_or_else(|| anyhow!("asset conflict has no remote hash"))?,
                size: remote.len(),
                jj_operation_id: Some(&checkpoint.operation_id),
            })?;
        }
        (ConflictKind::Asset, ConflictRemoteState::Deleted) => {
            store.remove_asset(&binding.project_id, &conflict.entity_id)?;
        }
    }
    clear_conflict(
        root,
        &mut manifest,
        conflict.kind,
        &conflict.entity_id,
        &conflict.path,
    );
    save_conflict_manifest(root, &manifest)?;
    Ok(ConflictResolveSummary {
        success: true,
        project_id: binding.project_id,
        path: conflict.path,
        resolution: resolution_name.to_owned(),
        remaining_conflicts: manifest.conflicts.len(),
        jj_operation_id: checkpoint.operation_id,
    })
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
    save_project_context(destination, project_id)?;
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
    let mut conflict_manifest = load_conflict_manifest(root)?;
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
                if let Some(previous_path) = relocated_from.as_deref() {
                    write_document(root, &document.path, local.as_deref().unwrap_or_default())?;
                    std::fs::remove_file(local_path(root, previous_path)?).with_context(|| {
                        format!("failed to remove relocated document {previous_path}")
                    })?;
                }
                let conflict = StoredConflict {
                    project_id: binding.project_id.clone(),
                    kind: ConflictKind::Document,
                    entity_id: document.id.clone(),
                    path: document.path.clone(),
                    remote_state: ConflictRemoteState::Present,
                    remote_version: Some(joined.version),
                    remote_hash: Some(remote_hash.clone()),
                    base_hash: previous
                        .as_ref()
                        .map(|previous| previous.remote_hash.clone()),
                    parent_folder_id: None,
                    metadata_hash: Some(metadata_hash.clone()),
                    ranges_json: Some(ranges_json.clone()),
                    snapshot_metadata_json: Some(snapshot_metadata_json.clone()),
                };
                record_conflict(
                    root,
                    &mut conflict_manifest,
                    conflict,
                    Some(state.content.as_bytes()),
                )?;
                conflicts.push(document.path.clone());
            }
            _ => unreachable!(),
        }
        if action != "conflict" {
            clear_conflict(
                root,
                &mut conflict_manifest,
                ConflictKind::Document,
                &document.id,
                &document.path,
            );
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
                if let Some(previous_path) = relocated_from.as_deref() {
                    write_binary(root, &file.path, local.as_deref().unwrap_or_default())?;
                    std::fs::remove_file(local_path(root, previous_path)?).with_context(|| {
                        format!("failed to remove relocated asset {previous_path}")
                    })?;
                }
                let conflict = StoredConflict {
                    project_id: binding.project_id.clone(),
                    kind: ConflictKind::Asset,
                    entity_id: file.id.clone(),
                    path: file.path.clone(),
                    remote_state: ConflictRemoteState::Present,
                    remote_version: None,
                    remote_hash: Some(remote_hash.clone()),
                    base_hash: previous
                        .as_ref()
                        .map(|previous| previous.remote_hash.clone()),
                    parent_folder_id: Some(file.parent_folder_id.clone()),
                    metadata_hash: None,
                    ranges_json: None,
                    snapshot_metadata_json: None,
                };
                record_conflict(root, &mut conflict_manifest, conflict, Some(remote))?;
                binary_conflicts.push(file.path.clone());
            }
            _ => unreachable!(),
        }
        if action != "conflict" {
            clear_conflict(
                root,
                &mut conflict_manifest,
                ConflictKind::Asset,
                &file.id,
                &file.path,
            );
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
                let path = previous.path.clone();
                let conflict = StoredConflict {
                    project_id: binding.project_id.clone(),
                    kind: ConflictKind::Asset,
                    entity_id: previous.file_id.clone(),
                    path: path.clone(),
                    remote_state: ConflictRemoteState::Deleted,
                    remote_version: None,
                    remote_hash: None,
                    base_hash: Some(previous.remote_hash.clone()),
                    parent_folder_id: Some(previous.parent_folder_id.clone()),
                    metadata_hash: None,
                    ranges_json: None,
                    snapshot_metadata_json: None,
                };
                record_conflict(root, &mut conflict_manifest, conflict, None)?;
                binary_conflicts.push(path);
            }
            Some(_) => {
                std::fs::remove_file(local_path(root, &previous.path)?)?;
                binary_deleted.push(previous.path.clone());
                store.remove_asset(&binding.project_id, &previous.file_id)?;
                clear_conflict(
                    root,
                    &mut conflict_manifest,
                    ConflictKind::Asset,
                    &previous.file_id,
                    &previous.path,
                );
            }
            None => {
                binary_deleted.push(previous.path.clone());
                store.remove_asset(&binding.project_id, &previous.file_id)?;
                clear_conflict(
                    root,
                    &mut conflict_manifest,
                    ConflictKind::Asset,
                    &previous.file_id,
                    &previous.path,
                );
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
    save_conflict_manifest(root, &conflict_manifest)?;
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
    let mut conflict_manifest = load_conflict_manifest(root)?;
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
        let mut local = read_document(root, &document.path)?;
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
        if local.is_none()
            && let Some(previous) = &previous
            && previous.path != document.path
            && let Some(previous_local) = read_document(root, &previous.path)?
        {
            write_document(root, &document.path, &previous_local)?;
            std::fs::remove_file(local_path(root, &previous.path)?).with_context(|| {
                format!("failed to remove relocated document {}", previous.path)
            })?;
            local = Some(previous_local);
        }
        let observed_conflict = StoredConflict {
            project_id: binding.project_id.clone(),
            kind: ConflictKind::Document,
            entity_id: document.id.clone(),
            path: document.path.clone(),
            remote_state: ConflictRemoteState::Present,
            remote_version: Some(joined.version),
            remote_hash: Some(remote_hash.clone()),
            base_hash: previous
                .as_ref()
                .map(|previous| previous.remote_hash.clone()),
            parent_folder_id: None,
            metadata_hash: Some(metadata_hash.clone()),
            ranges_json: Some(ranges_json.clone()),
            snapshot_metadata_json: Some(snapshot_metadata_json.clone()),
        };
        let Some(local) = local else {
            record_conflict(
                root,
                &mut conflict_manifest,
                observed_conflict,
                Some(state.content.as_bytes()),
            )?;
            conflicts.push(document.path.clone());
            socket.leave_doc(&document.id).await.ok();
            continue;
        };
        let local_hash = content_hash(&local);
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
                &ranges_json,
                &snapshot_metadata_json,
            )?;
            clear_conflict(
                root,
                &mut conflict_manifest,
                ConflictKind::Document,
                &document.id,
                &document.path,
            );
            unchanged.push(document.path.clone());
            socket.leave_doc(&document.id).await.ok();
            continue;
        }
        let Some(previous) = previous else {
            record_conflict(
                root,
                &mut conflict_manifest,
                observed_conflict,
                Some(state.content.as_bytes()),
            )?;
            conflicts.push(document.path.clone());
            socket.leave_doc(&document.id).await.ok();
            continue;
        };
        if remote_hash != previous.remote_hash {
            record_conflict(
                root,
                &mut conflict_manifest,
                observed_conflict,
                Some(state.content.as_bytes()),
            )?;
            conflicts.push(document.path.clone());
            socket.leave_doc(&document.id).await.ok();
            continue;
        }
        let Some(previous_metadata) = store.document_metadata(&binding.project_id, &document.id)?
        else {
            record_conflict(
                root,
                &mut conflict_manifest,
                observed_conflict,
                Some(state.content.as_bytes()),
            )?;
            metadata_conflicts.push(document.path.clone());
            socket.leave_doc(&document.id).await.ok();
            continue;
        };
        if previous_metadata.metadata_hash != metadata_hash {
            record_conflict(
                root,
                &mut conflict_manifest,
                observed_conflict,
                Some(state.content.as_bytes()),
            )?;
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
                    clear_conflict(
                        root,
                        &mut conflict_manifest,
                        ConflictKind::Document,
                        &document.id,
                        &document.path,
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
                    clear_conflict(
                        root,
                        &mut conflict_manifest,
                        ConflictKind::Document,
                        &document.id,
                        &document.path,
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
        let previous = store.asset(&binding.project_id, &file.id)?;
        let mut local = read_binary(root, &file.path)?;
        if local.is_none()
            && let Some(previous) = &previous
            && previous.path != file.path
            && let Some(previous_local) = read_binary(root, &previous.path)?
        {
            write_binary(root, &file.path, &previous_local)?;
            std::fs::remove_file(local_path(root, &previous.path)?)
                .with_context(|| format!("failed to remove relocated asset {}", previous.path))?;
            local = Some(previous_local);
        }
        let observed_conflict = StoredConflict {
            project_id: binding.project_id.clone(),
            kind: ConflictKind::Asset,
            entity_id: file.id.clone(),
            path: file.path.clone(),
            remote_state: ConflictRemoteState::Present,
            remote_version: None,
            remote_hash: Some(remote_hash.clone()),
            base_hash: previous
                .as_ref()
                .map(|previous| previous.remote_hash.clone()),
            parent_folder_id: Some(file.parent_folder_id.clone()),
            metadata_hash: None,
            ranges_json: None,
            snapshot_metadata_json: None,
        };
        let Some(local) = local else {
            record_conflict(
                root,
                &mut conflict_manifest,
                observed_conflict,
                Some(&remote),
            )?;
            binary_conflicts.push(file.path.clone());
            continue;
        };
        let local_hash = bytes_hash(&local);
        if local_hash == remote_hash {
            clear_conflict(
                root,
                &mut conflict_manifest,
                ConflictKind::Asset,
                &file.id,
                &file.path,
            );
            accepted_assets.push((file.clone(), remote_hash, local.len()));
            binary_unchanged.push(file.path.clone());
            continue;
        }
        let Some(previous) = previous else {
            record_conflict(
                root,
                &mut conflict_manifest,
                observed_conflict,
                Some(&remote),
            )?;
            binary_conflicts.push(file.path.clone());
            continue;
        };
        if previous.remote_hash != remote_hash {
            record_conflict(
                root,
                &mut conflict_manifest,
                observed_conflict,
                Some(&remote),
            )?;
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
                clear_conflict(
                    root,
                    &mut conflict_manifest,
                    ConflictKind::Asset,
                    &file.id,
                    &file.path,
                );
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
    save_conflict_manifest(root, &conflict_manifest)?;
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
    let scan = scan_workspace_files(root)?;
    for (path, content) in scan.files {
        if !document_paths.contains(path.as_str())
            && !asset_paths.contains(path.as_str())
            && is_binary_path(&path, &content)
        {
            binary_untracked.push(path);
        }
    }
    let conflict_manifest = load_conflict_manifest(root)?;
    let conflicts = conflict_summaries(root, &conflict_manifest)?;
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
        ignored: scan.ignored,
        conflicts,
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
        assert_eq!(
            discover_project_context(&nested).unwrap(),
            Some(ProjectContext {
                root: temp.path().canonicalize().unwrap(),
                project_id: "p1".into(),
                profile: Some(DEFAULT_PROFILE.into()),
            })
        );
    }

    #[test]
    fn public_project_context_supports_discovery_without_private_state() {
        let temp = tempfile::tempdir().unwrap();
        let context_dir = temp.path().join(".jujuleaf");
        std::fs::create_dir_all(&context_dir).unwrap();
        std::fs::write(
            context_dir.join("project.json"),
            br#"{"schemaVersion":1,"projectId":"public-project"}"#,
        )
        .unwrap();
        let nested = temp.path().join("chapters");
        std::fs::create_dir(&nested).unwrap();

        assert_eq!(
            discover_project_context(&nested).unwrap(),
            Some(ProjectContext {
                root: temp.path().canonicalize().unwrap(),
                project_id: "public-project".into(),
                profile: None,
            })
        );
    }

    #[test]
    fn old_remote_metadata_can_supply_the_project_context() {
        let temp = tempfile::tempdir().unwrap();
        let context_dir = temp.path().join(".jujuleaf");
        std::fs::create_dir_all(&context_dir).unwrap();
        std::fs::write(
            temp.path().join(METADATA_FILE),
            br#"{"schemaVersion":1,"projectId":"legacy-project"}"#,
        )
        .unwrap();
        let nested = temp.path().join("chapters");
        std::fs::create_dir(&nested).unwrap();

        assert_eq!(
            discover_project_context(&nested).unwrap(),
            Some(ProjectContext {
                root: temp.path().canonicalize().unwrap(),
                project_id: "legacy-project".into(),
                profile: None,
            })
        );
    }

    #[test]
    fn public_project_context_contains_no_account_details() {
        let temp = tempfile::tempdir().unwrap();
        save_project_context(temp.path(), "p1").unwrap();

        let manifest: ProjectContextManifest =
            serde_json::from_slice(&std::fs::read(temp.path().join(PROJECT_CONTEXT_FILE)).unwrap())
                .unwrap();
        assert_eq!(manifest.schema_version, 1);
        assert_eq!(manifest.project_id, "p1");
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
    fn workspace_scan_honors_jujuleafignore() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("build")).unwrap();
        std::fs::create_dir_all(temp.path().join(".git")).unwrap();
        std::fs::write(temp.path().join(ignore::IGNORE_FILE), "/build/\n*.aux\n").unwrap();
        std::fs::write(temp.path().join("build/output.pdf"), [0, 1]).unwrap();
        std::fs::write(temp.path().join("paper.aux"), b"generated").unwrap();
        std::fs::write(temp.path().join("figure.png"), [0, 1, 2]).unwrap();
        std::fs::write(temp.path().join(".git/index"), [0, 1, 2]).unwrap();

        let scan = scan_workspace_files(temp.path()).unwrap();
        assert_eq!(
            scan.files.keys().cloned().collect::<Vec<_>>(),
            vec!["/figure.png"]
        );
        assert_eq!(scan.ignored, vec!["build/", "paper.aux"]);
    }

    #[tokio::test]
    async fn resolving_document_conflict_updates_remote_baseline() {
        let temp = tempfile::tempdir().unwrap();
        let mut workspace = JjWorkspace::init(temp.path()).await.unwrap();
        let binding = ProjectBinding {
            project_id: "p1".into(),
            base_url: "https://example.test".into(),
            profile: DEFAULT_PROFILE.into(),
        };
        binding.save(temp.path()).unwrap();
        write_document(temp.path(), "/main.tex", "base\n").unwrap();
        let base = workspace.checkpoint("base").await.unwrap();
        let store = database(temp.path()).unwrap();
        store
            .upsert_document(
                "p1",
                "doc-1",
                "/main.tex",
                1,
                &content_hash("base\n"),
                Some(&base.operation_id),
            )
            .unwrap();
        write_document(temp.path(), "/main.tex", "local\n").unwrap();

        let conflict = StoredConflict {
            project_id: "p1".into(),
            kind: ConflictKind::Document,
            entity_id: "doc-1".into(),
            path: "/main.tex".into(),
            remote_state: ConflictRemoteState::Present,
            remote_version: Some(2),
            remote_hash: Some(content_hash("remote\n")),
            base_hash: Some(content_hash("base\n")),
            parent_folder_id: None,
            metadata_hash: Some("metadata".into()),
            ranges_json: Some("[]".into()),
            snapshot_metadata_json: Some("{}".into()),
        };
        let incoming = incoming_path(temp.path(), &conflict);
        let mut manifest = ConflictManifest {
            schema_version: 1,
            conflicts: Vec::new(),
        };
        record_conflict(temp.path(), &mut manifest, conflict, Some(b"remote\n")).unwrap();
        assert!(conflict_manifest_path(temp.path()).exists());
        assert!(incoming.exists());

        let detail = show_conflict(temp.path(), "main.tex").unwrap();
        assert!(detail.diff.contains("-local"));
        assert!(detail.diff.contains("+remote"));
        let resolved = resolve_conflict(temp.path(), "main.tex", ConflictResolution::Ours)
            .await
            .unwrap();
        assert_eq!(resolved.remaining_conflicts, 0);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("main.tex")).unwrap(),
            "local\n"
        );
        let stored = database(temp.path())
            .unwrap()
            .document("p1", "doc-1")
            .unwrap()
            .unwrap();
        assert_eq!(stored.remote_version, 2);
        assert_eq!(stored.remote_hash, content_hash("remote\n"));
        assert!(!conflict_manifest_path(temp.path()).exists());
        assert!(!incoming.exists());
    }

    #[tokio::test]
    async fn accepting_remote_asset_deletion_removes_file_and_baseline() {
        let temp = tempfile::tempdir().unwrap();
        let mut workspace = JjWorkspace::init(temp.path()).await.unwrap();
        ProjectBinding {
            project_id: "p1".into(),
            base_url: "https://example.test".into(),
            profile: DEFAULT_PROFILE.into(),
        }
        .save(temp.path())
        .unwrap();
        write_binary(temp.path(), "/figure.png", b"local").unwrap();
        let base = workspace.checkpoint("base").await.unwrap();
        let store = database(temp.path()).unwrap();
        store
            .upsert_asset(AssetCheckpoint {
                project_id: "p1",
                file_id: "asset-1",
                path: "/figure.png",
                parent_folder_id: "root",
                remote_hash: &bytes_hash(b"base"),
                size: 4,
                jj_operation_id: Some(&base.operation_id),
            })
            .unwrap();
        let conflict = StoredConflict {
            project_id: "p1".into(),
            kind: ConflictKind::Asset,
            entity_id: "asset-1".into(),
            path: "/figure.png".into(),
            remote_state: ConflictRemoteState::Deleted,
            remote_version: None,
            remote_hash: None,
            base_hash: Some(bytes_hash(b"base")),
            parent_folder_id: Some("root".into()),
            metadata_hash: None,
            ranges_json: None,
            snapshot_metadata_json: None,
        };
        let mut manifest = ConflictManifest {
            schema_version: 1,
            conflicts: Vec::new(),
        };
        record_conflict(temp.path(), &mut manifest, conflict, None).unwrap();

        resolve_conflict(temp.path(), "figure.png", ConflictResolution::Theirs)
            .await
            .unwrap();
        assert!(!temp.path().join("figure.png").exists());
        assert!(
            database(temp.path())
                .unwrap()
                .asset("p1", "asset-1")
                .unwrap()
                .is_none()
        );
        assert!(!conflict_manifest_path(temp.path()).exists());
    }

    #[test]
    fn unsafe_remote_paths_never_escape_the_clone() {
        let temp = tempfile::tempdir().unwrap();
        assert!(local_path(temp.path(), "../outside").is_err());
        assert!(local_path(temp.path(), "/safe/file.png").is_ok());
    }

    #[test]
    fn workspace_operation_lock_serializes_writers() {
        let temp = tempfile::tempdir().unwrap();
        let first = WorkspaceOperationLock::acquire(temp.path(), "first").unwrap();
        assert!(WorkspaceOperationLock::acquire(temp.path(), "second").is_err());
        drop(first);
        WorkspaceOperationLock::acquire(temp.path(), "third").unwrap();
    }
}
