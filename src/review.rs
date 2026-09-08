use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::OverleafApi;
use crate::auth::Session;
use crate::file_util::atomic_write;
use crate::jj::JjWorkspace;
use crate::operations::{
    BuildOptions, DocumentState, HISTORY_OT, build_document_operations, minimal_text_changes,
    parse_document_snapshot, utf16_len,
};
use crate::platform::workspace_path;
use crate::project::{DocumentRef, collect_entities, connect_project_with_api};
use crate::socket::UpdateOptions;
use crate::store::{OperationStatus, SyncStore, bytes_hash, content_hash};
use crate::sync::{
    ProjectBinding, PullSummary, collect_workspace_files, local_status, pull_project_with_api,
};

const STATE_DIR: &str = ".jj/jujuleaf";
const REVIEW_STATE_FILE: &str = "review.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPhase {
    Draft,
    Submitting,
    Submitted,
    Finishing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewDocument {
    path: String,
    base_version: i64,
    base_hash: String,
    base_metadata_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proposed_hash: Option<String>,
    #[serde(default)]
    change_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    submitted_version: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    receipt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    final_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewState {
    schema_version: u32,
    project_id: String,
    description: String,
    phase: ReviewPhase,
    started_at: String,
    base_operation_id: String,
    work_commit_id: String,
    parent_commit_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    submitted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    submitted_commit_id: Option<String>,
    documents: BTreeMap<String, ReviewDocument>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BeginSummary {
    success: bool,
    project_id: String,
    description: String,
    phase: ReviewPhase,
    operation_id: String,
    commit_id: String,
    parent_commit_id: String,
    document_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewAbortSummary {
    success: bool,
    project_id: String,
    description: String,
    restored_commit_id: String,
    jj_operation_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFileDiff {
    path: String,
    change_count: usize,
    removed: usize,
    inserted: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffSummary {
    success: bool,
    project_id: String,
    description: String,
    phase: ReviewPhase,
    files: Vec<ReviewFileDiff>,
    total_changes: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSubmitSummary {
    success: bool,
    project_id: String,
    description: String,
    phase: ReviewPhase,
    submitted: Vec<ReviewSubmittedFile>,
    unchanged: Vec<String>,
    total_change_ids: usize,
    jj_operation_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSubmittedFile {
    path: String,
    change_count: usize,
    change_ids: Vec<String>,
    acknowledged_version: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewStatusFile {
    path: String,
    change_ids: Vec<String>,
    pending_change_ids: Vec<String>,
    foreign_change_ids: Vec<String>,
    local_changed_after_submit: bool,
    remote_matches_proposal: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewStatusSummary {
    success: bool,
    project_id: String,
    description: String,
    phase: ReviewPhase,
    files: Vec<ReviewStatusFile>,
    pending_change_ids: Vec<String>,
    foreign_change_ids: Vec<String>,
    unsubmitted_files: Vec<String>,
    unresolved_receipts: usize,
    ready_to_finish: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFinishSummary {
    success: bool,
    project_id: String,
    description: String,
    reviewed_files: Vec<String>,
    unsubmitted_files: Vec<String>,
    jj_operation_id: String,
    pull: PullSummary,
}

struct RemoteReviewDocument {
    id: String,
    path: String,
    version: i64,
    content: String,
    hash: String,
    metadata_hash: String,
    ranges_json: String,
    snapshot_metadata_json: String,
    tracked_change_ids: BTreeSet<String>,
}

fn review_path(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join(REVIEW_STATE_FILE)
}

fn database(root: &Path) -> Result<SyncStore> {
    SyncStore::open(root.join(STATE_DIR).join("sync.sqlite3"))
}

fn read_document(root: &Path, remote_path: &str) -> Result<String> {
    let path = workspace_path(root, remote_path)?;
    std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read UTF-8 document {}", path.display()))
}

fn write_document(root: &Path, remote_path: &str, content: &str) -> Result<()> {
    let path = workspace_path(root, remote_path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))
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

fn collect_change_identities(kind: &str, array: Option<&Vec<Value>>, ids: &mut BTreeSet<String>) {
    for item in array.into_iter().flatten() {
        let explicit = item
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| item.pointer("/tracking/id").and_then(Value::as_str));
        let identity = explicit.map(str::to_owned).unwrap_or_else(|| {
            let encoded = serde_json::to_vec(item).expect("JSON values are serializable");
            format!("{kind}:{}", bytes_hash(&encoded))
        });
        ids.insert(identity);
    }
}

fn tracked_change_ids_from_values(ranges: &Value, snapshot: &Value) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    collect_change_identities(
        "legacy",
        ranges.get("changes").and_then(Value::as_array),
        &mut ids,
    );
    collect_change_identities(
        "history",
        snapshot.get("trackedChanges").and_then(Value::as_array),
        &mut ids,
    );
    ids
}

fn tracked_change_ids(state: &DocumentState, ranges: &Value) -> BTreeSet<String> {
    tracked_change_ids_from_values(ranges, &state.raw)
}

fn load_state(root: &Path) -> Result<Option<ReviewState>> {
    let path = review_path(root);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let state: ReviewState =
        serde_json::from_slice(&bytes).with_context(|| format!("invalid {}", path.display()))?;
    ensure!(
        state.schema_version == 1,
        "unsupported review state schema {}",
        state.schema_version
    );
    Ok(Some(state))
}

fn require_state(root: &Path) -> Result<ReviewState> {
    load_state(root)?.ok_or_else(|| anyhow!("no active review work; run: jujuleaf begin -m '...'"))
}

fn save_state(root: &Path, state: &ReviewState) -> Result<()> {
    let path = review_path(root);
    atomic_write(&path, &serde_json::to_vec_pretty(state)?)
}

fn clear_state(root: &Path) -> Result<()> {
    let path = review_path(root);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn document_submission_started(document: &ReviewDocument) -> bool {
    document.receipt_id.is_some()
        || document.submitted_version.is_some()
        || !document.change_ids.is_empty()
}

fn submission_started(state: &ReviewState) -> bool {
    state.documents.values().any(document_submission_started)
}

fn review_can_be_aborted(state: &ReviewState) -> bool {
    state.phase == ReviewPhase::Draft
        || (state.phase == ReviewPhase::Submitting && !submission_started(state))
}

fn review_can_be_finished(state: &ReviewState) -> bool {
    matches!(state.phase, ReviewPhase::Submitted | ReviewPhase::Finishing)
        || (state.phase == ReviewPhase::Submitting && submission_started(state))
}

fn unsubmitted_review_files(state: &ReviewState) -> Vec<String> {
    let mut files: Vec<String> = state
        .documents
        .values()
        .filter(|document| {
            document
                .proposed_hash
                .as_ref()
                .is_some_and(|hash| hash != &document.base_hash)
                && !document_submission_started(document)
        })
        .map(|document| document.path.clone())
        .collect();
    files.sort();
    files
}

fn proposed_or_base_hash(document: &ReviewDocument) -> &str {
    document
        .proposed_hash
        .as_deref()
        .unwrap_or(&document.base_hash)
}

fn finish_local_hash_is_valid(
    phase: ReviewPhase,
    document: &ReviewDocument,
    local_hash: &str,
) -> bool {
    if local_hash == proposed_or_base_hash(document) {
        return true;
    }
    phase == ReviewPhase::Finishing
        && document
            .final_hash
            .as_deref()
            .is_some_and(|hash| hash == local_hash)
}

pub fn ensure_sync_allowed(root: &Path, command: &str) -> Result<()> {
    if let Some(state) = load_state(root)? {
        bail!(
            "cannot run {command} while review work '{}' is {}; use the jujuleaf review commands (or 'jujuleaf review abort' before submission)",
            state.description,
            match state.phase {
                ReviewPhase::Draft => "draft",
                ReviewPhase::Submitting => "submitting",
                ReviewPhase::Submitted => "submitted",
                ReviewPhase::Finishing => "finishing",
            }
        );
    }
    Ok(())
}

fn validate_binding(
    root: &Path,
    session: &Session,
    profile: &str,
    state: &ReviewState,
) -> Result<ProjectBinding> {
    let binding = ProjectBinding::load(root)?;
    ensure!(
        binding.project_id == state.project_id,
        "review state belongs to project {}, not {}",
        state.project_id,
        binding.project_id
    );
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
    Ok(binding)
}

fn document_map(documents: Vec<DocumentRef>) -> BTreeMap<String, DocumentRef> {
    documents
        .into_iter()
        .map(|document| (document.id.clone(), document))
        .collect()
}

async fn read_remote_document(
    socket: &mut crate::socket::OverleafSocket,
    document: &DocumentRef,
) -> Result<RemoteReviewDocument> {
    let joined = socket.join_doc(&document.id).await?;
    let state = parse_document_snapshot(joined.snapshot, &joined.ot_type)?;
    let (snapshot, metadata_hash) = metadata_parts(&state, &joined.ranges)?;
    let tracked_change_ids = tracked_change_ids(&state, &joined.ranges);
    let result = RemoteReviewDocument {
        id: document.id.clone(),
        path: document.path.clone(),
        version: joined.version,
        hash: content_hash(&state.content),
        content: state.content,
        metadata_hash,
        ranges_json: serde_json::to_string(&joined.ranges)?,
        snapshot_metadata_json: serde_json::to_string(&snapshot)?,
        tracked_change_ids,
    };
    socket.leave_doc(&document.id).await.ok();
    Ok(result)
}

fn require_clean_binary_state(root: &Path, status: &crate::sync::StatusSummary) -> Result<()> {
    ensure!(
        status.binary_modified.is_empty()
            && status.binary_missing.is_empty()
            && status.binary_untracked.is_empty(),
        "review work currently supports text documents only; restore or publish binary changes before beginning/submitting review work in {}",
        root.display()
    );
    Ok(())
}

fn require_no_unresolved_receipts(status: &crate::sync::StatusSummary) -> Result<()> {
    ensure!(
        status.unresolved_receipts == 0,
        "resolve uncertain remote writes before review work"
    );
    Ok(())
}

fn require_no_sync_conflicts(status: &crate::sync::StatusSummary) -> Result<()> {
    ensure!(
        status.conflicts.is_empty(),
        "resolve synchronization conflicts before review work: {}",
        status
            .conflicts
            .iter()
            .map(|conflict| conflict.path.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

fn require_known_workspace_files(root: &Path, project_id: &str) -> Result<()> {
    let store = database(root)?;
    let known: BTreeSet<String> = store
        .documents(project_id)?
        .into_iter()
        .map(|document| document.path)
        .chain(
            store
                .assets(project_id)?
                .into_iter()
                .map(|asset| asset.path),
        )
        .collect();
    let untracked: Vec<String> = collect_workspace_files(root)?
        .into_keys()
        .filter(|path| !known.contains(path))
        .collect();
    ensure!(
        untracked.is_empty(),
        "review work cannot submit new local files; create them remotely before begin or remove them: {}",
        untracked.join(", ")
    );
    Ok(())
}

pub async fn begin_review(root: &Path, description: &str) -> Result<BeginSummary> {
    ensure!(load_state(root)?.is_none(), "review work is already active");
    let description = description.trim();
    ensure!(!description.is_empty(), "work description cannot be empty");

    let status = local_status(root).await?;
    require_clean_binary_state(root, &status)?;
    require_no_unresolved_receipts(&status)?;
    require_no_sync_conflicts(&status)?;
    ensure!(
        status.modified.is_empty() && status.missing.is_empty(),
        "begin requires a clean synchronized text baseline; run sync before begin"
    );

    let binding = ProjectBinding::load(root)?;
    require_known_workspace_files(root, &binding.project_id)?;
    let store = database(root)?;
    let mut documents = BTreeMap::new();
    for document in store.documents(&binding.project_id)? {
        let metadata = store
            .document_metadata(&binding.project_id, &document.doc_id)?
            .ok_or_else(|| anyhow!("missing metadata baseline for {}", document.path))?;
        let ranges: Value = serde_json::from_str(&metadata.ranges_json)?;
        let snapshot: Value = serde_json::from_str(&metadata.snapshot_metadata_json)?;
        let pending = tracked_change_ids_from_values(&ranges, &snapshot);
        ensure!(
            pending.is_empty(),
            "begin requires an accepted remote baseline; {} still has tracked changes: {}",
            document.path,
            pending.into_iter().collect::<Vec<_>>().join(", ")
        );
        documents.insert(
            document.doc_id,
            ReviewDocument {
                path: document.path,
                base_version: document.remote_version,
                base_hash: document.remote_hash,
                base_metadata_hash: metadata.metadata_hash,
                proposed_hash: None,
                change_ids: Vec::new(),
                submitted_version: None,
                receipt_id: None,
                final_hash: None,
            },
        );
    }
    ensure!(
        !documents.is_empty(),
        "synchronized clone has no text documents"
    );

    let mut workspace = JjWorkspace::open(root).await?;
    let base_operation_id = workspace.operation_id();
    let work = workspace.begin_change(description).await?;
    let state = ReviewState {
        schema_version: 1,
        project_id: binding.project_id.clone(),
        description: description.to_owned(),
        phase: ReviewPhase::Draft,
        started_at: Utc::now().to_rfc3339(),
        base_operation_id,
        work_commit_id: work.commit_id.clone(),
        parent_commit_id: work.parent_commit_id.clone(),
        submitted_at: None,
        submitted_commit_id: None,
        documents,
    };
    save_state(root, &state)?;

    Ok(BeginSummary {
        success: true,
        project_id: binding.project_id,
        description: description.to_owned(),
        phase: ReviewPhase::Draft,
        operation_id: work.operation_id,
        commit_id: work.commit_id,
        parent_commit_id: work.parent_commit_id,
        document_count: state.documents.len(),
    })
}

pub async fn abort_review(root: &Path) -> Result<ReviewAbortSummary> {
    let state = require_state(root)?;
    ensure!(
        review_can_be_aborted(&state),
        "review work can only be aborted before a remote submission attempt; resolve submitted tracked changes and run review finish"
    );
    let mut workspace = JjWorkspace::open(root).await?;
    let restored = workspace.abandon_change(&state.parent_commit_id).await?;
    clear_state(root)?;

    Ok(ReviewAbortSummary {
        success: true,
        project_id: state.project_id,
        description: state.description,
        restored_commit_id: restored.commit_id,
        jj_operation_id: restored.operation_id,
    })
}

pub async fn review_diff_with_api(
    root: &Path,
    session: &Session,
    profile: &str,
    api: &mut OverleafApi,
) -> Result<ReviewDiffSummary> {
    let state = require_state(root)?;
    ensure!(
        state.phase == ReviewPhase::Draft,
        "review diff is available before submit; use review status after submit"
    );
    let binding = validate_binding(root, session, profile, &state)?;
    let status = local_status(root).await?;
    require_clean_binary_state(root, &status)?;
    require_no_unresolved_receipts(&status)?;
    require_no_sync_conflicts(&status)?;
    require_known_workspace_files(root, &state.project_id)?;

    let (mut socket, project) = connect_project_with_api(api, &binding.project_id).await?;
    let documents = document_map(collect_entities(&project).documents);
    ensure!(
        documents.len() == state.documents.len(),
        "remote document set changed since begin; abort, synchronize, and begin review work again"
    );
    let mut files = Vec::new();
    for (doc_id, baseline) in &state.documents {
        let document = documents
            .get(doc_id)
            .ok_or_else(|| anyhow!("remote document disappeared: {}", baseline.path))?;
        ensure!(
            document.path == baseline.path,
            "remote document moved since begin: {} -> {}",
            baseline.path,
            document.path
        );
        let remote = read_remote_document(&mut socket, document).await?;
        ensure!(
            remote.version == baseline.base_version,
            "remote version changed since begin: {}",
            baseline.path
        );
        ensure!(
            remote.hash == baseline.base_hash,
            "remote content changed since begin: {}",
            baseline.path
        );
        ensure!(
            remote.metadata_hash == baseline.base_metadata_hash,
            "remote comments or tracked changes changed since begin: {}",
            baseline.path
        );
        let local = read_document(root, &baseline.path)?;
        let changes = minimal_text_changes(&remote.content, &local);
        if changes.is_empty() {
            continue;
        }
        let removed = changes
            .iter()
            .map(|change| utf16_len(change.expect.as_deref().unwrap_or_default()))
            .sum();
        let inserted = changes
            .iter()
            .map(|change| utf16_len(change.insert.as_deref().unwrap_or_default()))
            .sum();
        files.push(ReviewFileDiff {
            path: baseline.path.clone(),
            change_count: changes.len(),
            removed,
            inserted,
        });
    }
    socket.close().await.ok();
    let total_changes = files.iter().map(|file| file.change_count).sum();
    Ok(ReviewDiffSummary {
        success: true,
        project_id: binding.project_id,
        description: state.description,
        phase: state.phase,
        files,
        total_changes,
    })
}

pub async fn submit_review_with_api(
    root: &Path,
    session: &Session,
    profile: &str,
    api: &mut OverleafApi,
    options: &UpdateOptions,
) -> Result<ReviewSubmitSummary> {
    let mut state = require_state(root)?;
    ensure!(
        matches!(state.phase, ReviewPhase::Draft | ReviewPhase::Submitting),
        "review work has already been submitted"
    );
    let binding = validate_binding(root, session, profile, &state)?;
    let status = local_status(root).await?;
    require_clean_binary_state(root, &status)?;
    require_no_unresolved_receipts(&status)?;
    require_no_sync_conflicts(&status)?;
    require_known_workspace_files(root, &state.project_id)?;

    if state.phase == ReviewPhase::Draft {
        let preview = review_diff_with_api(root, session, profile, api).await?;
        ensure!(
            preview.total_changes > 0,
            "review work has no local text changes to submit"
        );
        let mut workspace = JjWorkspace::open(root).await?;
        let checkpoint = workspace.checkpoint(&state.description).await?;
        for document in state.documents.values_mut() {
            let local = read_document(root, &document.path)?;
            document.proposed_hash = Some(content_hash(&local));
        }
        state.phase = ReviewPhase::Submitting;
        state.submitted_commit_id = Some(checkpoint.commit_id);
        save_state(root, &state)?;
    }

    let store = database(root)?;
    let (mut socket, project) = connect_project_with_api(api, &binding.project_id).await?;
    let documents = document_map(collect_entities(&project).documents);
    let mut submitted = Vec::new();
    let mut unchanged = Vec::new();
    let mut history_user_id: Option<String> = None;

    let document_ids: Vec<String> = state.documents.keys().cloned().collect();
    for doc_id in document_ids {
        let baseline = state
            .documents
            .get(&doc_id)
            .cloned()
            .expect("document key exists");
        let document = documents
            .get(&doc_id)
            .ok_or_else(|| anyhow!("remote document disappeared: {}", baseline.path))?;
        ensure!(
            document.path == baseline.path,
            "remote document moved since begin: {} -> {}",
            baseline.path,
            document.path
        );

        if let Some(proposed_hash) = &baseline.proposed_hash {
            let local_hash = content_hash(&read_document(root, &baseline.path)?);
            ensure!(
                local_hash == *proposed_hash,
                "local file changed after review submission started: {}",
                baseline.path
            );
            let remote = read_remote_document(&mut socket, document).await?;

            if !baseline.change_ids.is_empty() {
                ensure!(
                    remote.hash == *proposed_hash
                        || baseline
                            .change_ids
                            .iter()
                            .all(|id| !remote.tracked_change_ids.contains(id)),
                    "partially submitted review changed unexpectedly: {}",
                    baseline.path
                );
                submitted.push(ReviewSubmittedFile {
                    path: baseline.path,
                    change_count: baseline.change_ids.len(),
                    change_ids: baseline.change_ids,
                    acknowledged_version: baseline.submitted_version.unwrap_or(remote.version),
                });
                continue;
            }

            let attempt = baseline
                .receipt_id
                .as_deref()
                .map(|receipt_id| {
                    store
                        .operation(receipt_id)?
                        .ok_or_else(|| anyhow!("review receipt disappeared: {receipt_id}"))
                })
                .transpose()?;
            let attempt_confirmed = match attempt.as_ref().map(|attempt| attempt.status) {
                Some(OperationStatus::Confirmed) => true,
                Some(OperationStatus::Inflight | OperationStatus::Unknown)
                    if remote.hash == *proposed_hash =>
                {
                    store.mark_confirmed(
                        baseline
                            .receipt_id
                            .as_deref()
                            .expect("loaded attempt has a receipt ID"),
                    )?;
                    true
                }
                Some(OperationStatus::Inflight | OperationStatus::Unknown)
                    if remote.tracked_change_ids.is_empty()
                        && (remote.version != baseline.base_version
                            || remote.hash != baseline.base_hash
                            || remote.metadata_hash != baseline.base_metadata_hash) =>
                {
                    store.mark_confirmed(
                        baseline
                            .receipt_id
                            .as_deref()
                            .expect("loaded attempt has a receipt ID"),
                    )?;
                    true
                }
                _ => false,
            };
            if attempt_confirmed {
                let change_ids: Vec<String> = remote.tracked_change_ids.iter().cloned().collect();
                let entry = state
                    .documents
                    .get_mut(&doc_id)
                    .expect("document key exists");
                entry.change_ids = change_ids.clone();
                entry.submitted_version = Some(remote.version);
                save_state(root, &state)?;
                submitted.push(ReviewSubmittedFile {
                    path: baseline.path,
                    change_count: change_ids.len(),
                    change_ids,
                    acknowledged_version: remote.version,
                });
                continue;
            }

            if proposed_hash == &baseline.base_hash {
                ensure!(
                    remote.version == baseline.base_version
                        && remote.hash == baseline.base_hash
                        && remote.metadata_hash == baseline.base_metadata_hash,
                    "unchanged review document changed remotely since begin: {}",
                    baseline.path
                );
                let entry = state
                    .documents
                    .get_mut(&doc_id)
                    .expect("document key exists");
                entry.proposed_hash = None;
                entry.submitted_version = None;
                save_state(root, &state)?;
                unchanged.push(baseline.path);
                continue;
            }

            ensure!(
                remote.version == baseline.base_version
                    && remote.hash == baseline.base_hash
                    && remote.metadata_hash == baseline.base_metadata_hash,
                "remote document changed while review submission was interrupted: {}",
                baseline.path
            );
            if let Some(attempt) = attempt
                && matches!(
                    attempt.status,
                    OperationStatus::Prepared
                        | OperationStatus::Inflight
                        | OperationStatus::Unknown
                )
            {
                store.mark_failed(
                    &attempt.receipt_id,
                    "remote remained at the review baseline; safe to retry",
                )?;
            }
        }

        let joined = socket.join_doc(&document.id).await?;
        let document_state = parse_document_snapshot(joined.snapshot, &joined.ot_type)?;
        let (_, metadata_hash) = metadata_parts(&document_state, &joined.ranges)?;
        let remote_hash = content_hash(&document_state.content);
        ensure!(
            joined.version == baseline.base_version,
            "remote version changed since begin: {}",
            baseline.path
        );
        ensure!(
            remote_hash == baseline.base_hash,
            "remote content changed since begin: {}",
            baseline.path
        );
        ensure!(
            metadata_hash == baseline.base_metadata_hash,
            "remote comments or tracked changes changed since begin: {}",
            baseline.path
        );
        let local = read_document(root, &baseline.path)?;
        let local_hash = content_hash(&local);
        ensure!(
            baseline
                .proposed_hash
                .as_ref()
                .is_none_or(|hash| hash == &local_hash),
            "local file changed after review submission started: {}",
            baseline.path
        );
        let changes = minimal_text_changes(&document_state.content, &local);
        if changes.is_empty() {
            unchanged.push(baseline.path);
            socket.leave_doc(&document.id).await.ok();
            continue;
        }

        let user_id = if document_state.ot_type == HISTORY_OT {
            if history_user_id.is_none() {
                history_user_id = Some(api.current_user_id(&binding.project_id).await?);
            }
            history_user_id.clone()
        } else {
            None
        };
        let built = build_document_operations(
            &document_state,
            &changes,
            &BuildOptions {
                tracked: true,
                user_id,
                timestamp: None,
            },
        )?;
        ensure!(
            built.expected_content == local,
            "generated review operations do not reproduce local content for {}",
            baseline.path
        );
        let before_ids = tracked_change_ids(&document_state, &joined.ranges);
        let operation_json = serde_json::to_string(&built.ops)?;
        for stale in store.unresolved_operations(&binding.project_id, Some(&document.id))? {
            ensure!(
                stale.status == OperationStatus::Prepared && stale.expected_hash == local_hash,
                "another unresolved remote write exists for {}; resolve it before retrying review",
                baseline.path
            );
            store.mark_failed(
                &stale.receipt_id,
                "receipt was prepared before review state recorded it; safe to retry",
            )?;
        }

        let receipt = store.prepare_operation(
            &binding.project_id,
            &document.id,
            joined.version,
            &operation_json,
            &local_hash,
        )?;
        {
            let entry = state
                .documents
                .get_mut(&doc_id)
                .expect("document key exists");
            entry.receipt_id = Some(receipt.clone());
        }
        save_state(root, &state)?;

        if let Some(public_id) = socket.public_id() {
            store.record_source_id(&receipt, public_id)?;
        }
        store.mark_inflight(&receipt)?;

        let result = if document_state.ot_type == crate::operations::LEGACY_OT {
            socket
                .apply_tracked_update(&document.id, &built.ops, joined.version, options)
                .await
        } else {
            socket
                .apply_update(&document.id, &built.ops, joined.version, None, options)
                .await
        };
        let outcome_was_unknown = result.is_err();
        if let Err(error) = result {
            store.mark_unknown(&receipt, &error.to_string())?;
            socket
                .reconnect()
                .await
                .context("review update result is unknown and reconnect failed")?;
        } else {
            store.mark_confirmed(&receipt)?;
            socket.leave_doc(&document.id).await.ok();
        }

        let refreshed = socket.join_doc(&document.id).await?;
        let refreshed_state = parse_document_snapshot(refreshed.snapshot, &refreshed.ot_type)?;
        let refreshed_hash = content_hash(&refreshed_state.content);
        ensure!(
            refreshed_hash == local_hash,
            "review update outcome is uncertain for {}; rerun review status",
            baseline.path
        );
        if outcome_was_unknown {
            store.mark_confirmed(&receipt)?;
        }
        let after_ids = tracked_change_ids(&refreshed_state, &refreshed.ranges);
        let change_ids: Vec<String> = after_ids.difference(&before_ids).cloned().collect();
        ensure!(
            !change_ids.is_empty(),
            "remote content changed but no tracked-change IDs were returned for {}",
            baseline.path
        );
        socket.leave_doc(&document.id).await.ok();

        let entry = state
            .documents
            .get_mut(&doc_id)
            .expect("document key exists");
        entry.proposed_hash = Some(local_hash);
        entry.change_ids = change_ids.clone();
        entry.submitted_version = Some(refreshed.version);
        save_state(root, &state)?;
        submitted.push(ReviewSubmittedFile {
            path: baseline.path,
            change_count: built.changes.len(),
            change_ids,
            acknowledged_version: refreshed.version,
        });
    }
    socket.close().await.ok();

    state.phase = ReviewPhase::Submitted;
    state.submitted_at = Some(Utc::now().to_rfc3339());
    save_state(root, &state)?;
    let total_change_ids = submitted.iter().map(|file| file.change_ids.len()).sum();
    Ok(ReviewSubmitSummary {
        success: true,
        project_id: binding.project_id,
        description: state.description,
        phase: state.phase,
        submitted,
        unchanged,
        total_change_ids,
        jj_operation_id: JjWorkspace::open(root).await?.operation_id(),
    })
}

pub async fn review_status_with_api(
    root: &Path,
    session: &Session,
    profile: &str,
    api: &mut OverleafApi,
) -> Result<ReviewStatusSummary> {
    let state = require_state(root)?;
    let binding = validate_binding(root, session, profile, &state)?;
    let status = local_status(root).await?;
    require_clean_binary_state(root, &status)?;
    require_no_sync_conflicts(&status)?;
    require_known_workspace_files(root, &state.project_id)?;
    let (mut socket, project) = connect_project_with_api(api, &binding.project_id).await?;
    let documents = document_map(collect_entities(&project).documents);
    let mut files = Vec::new();
    let mut pending = BTreeSet::new();
    let mut foreign = BTreeSet::new();
    let mut local_changed = false;

    for (doc_id, review_document) in &state.documents {
        let document = documents
            .get(doc_id)
            .ok_or_else(|| anyhow!("remote document disappeared: {}", review_document.path))?;
        ensure!(
            document.path == review_document.path,
            "remote document moved since begin: {} -> {}",
            review_document.path,
            document.path
        );
        let remote = read_remote_document(&mut socket, document).await?;
        let owned: BTreeSet<String> = review_document.change_ids.iter().cloned().collect();
        let pending_ids: Vec<String> = remote
            .tracked_change_ids
            .intersection(&owned)
            .cloned()
            .collect();
        let foreign_ids: Vec<String> = remote
            .tracked_change_ids
            .difference(&owned)
            .cloned()
            .collect();
        pending.extend(pending_ids.iter().cloned());
        foreign.extend(foreign_ids.iter().cloned());
        let local_hash = content_hash(&read_document(root, &review_document.path)?);
        let expected_hash = if state.phase == ReviewPhase::Finishing {
            review_document
                .final_hash
                .as_deref()
                .unwrap_or_else(|| proposed_or_base_hash(review_document))
        } else {
            proposed_or_base_hash(review_document)
        };
        let content_is_locked =
            review_document.proposed_hash.is_some() || state.phase != ReviewPhase::Draft;
        let changed_after_submit = content_is_locked
            && if state.phase == ReviewPhase::Finishing {
                !finish_local_hash_is_valid(state.phase, review_document, &local_hash)
            } else {
                local_hash != expected_hash
            };
        local_changed |= changed_after_submit;
        if review_document.proposed_hash.is_some()
            || changed_after_submit
            || !foreign_ids.is_empty()
        {
            files.push(ReviewStatusFile {
                path: review_document.path.clone(),
                change_ids: review_document.change_ids.clone(),
                pending_change_ids: pending_ids,
                foreign_change_ids: foreign_ids,
                local_changed_after_submit: changed_after_submit,
                remote_matches_proposal: remote.hash == expected_hash,
            });
        }
    }
    for (doc_id, document) in &documents {
        if state.documents.contains_key(doc_id) {
            continue;
        }
        let remote = read_remote_document(&mut socket, document).await?;
        let foreign_ids: Vec<String> = remote.tracked_change_ids.into_iter().collect();
        foreign.extend(foreign_ids.iter().cloned());
        if !foreign_ids.is_empty() {
            files.push(ReviewStatusFile {
                path: document.path.clone(),
                change_ids: Vec::new(),
                pending_change_ids: Vec::new(),
                foreign_change_ids: foreign_ids,
                local_changed_after_submit: false,
                remote_matches_proposal: false,
            });
        }
    }

    socket.close().await.ok();
    let unsubmitted_files = unsubmitted_review_files(&state);
    let ready_to_finish = review_can_be_finished(&state)
        && pending.is_empty()
        && foreign.is_empty()
        && !local_changed
        && status.conflicts.is_empty()
        && status.unresolved_receipts == 0;
    Ok(ReviewStatusSummary {
        success: true,
        project_id: binding.project_id,
        description: state.description,
        phase: state.phase,
        files,
        pending_change_ids: pending.into_iter().collect(),
        foreign_change_ids: foreign.into_iter().collect(),
        unsubmitted_files,
        unresolved_receipts: status.unresolved_receipts,
        ready_to_finish,
    })
}

pub async fn finish_review_with_api(
    root: &Path,
    session: &Session,
    profile: &str,
    api: &mut OverleafApi,
) -> Result<ReviewFinishSummary> {
    let mut state = require_state(root)?;
    ensure!(
        review_can_be_finished(&state),
        "review work must have at least one remote submission attempt before it can be finished"
    );
    let binding = validate_binding(root, session, profile, &state)?;
    let initial_status = local_status(root).await?;
    require_clean_binary_state(root, &initial_status)?;
    require_known_workspace_files(root, &state.project_id)?;

    let mut reviewed_files: Vec<String> = state
        .documents
        .values()
        .filter(|document| document_submission_started(document))
        .map(|document| document.path.clone())
        .collect();
    reviewed_files.sort();
    let unsubmitted_files = unsubmitted_review_files(&state);

    let (mut socket, project) = connect_project_with_api(api, &binding.project_id).await?;
    let documents = document_map(collect_entities(&project).documents);
    let mut reviewed_documents = Vec::new();
    let mut pending = BTreeSet::new();

    for (doc_id, review_document) in &state.documents {
        let local_hash = content_hash(&read_document(root, &review_document.path)?);
        ensure!(
            finish_local_hash_is_valid(state.phase, review_document, &local_hash),
            "local file changed after review submission: {}",
            review_document.path
        );
        let document = documents
            .get(doc_id)
            .ok_or_else(|| anyhow!("remote document disappeared: {}", review_document.path))?;
        ensure!(
            document.path == review_document.path,
            "remote document moved since begin: {} -> {}",
            review_document.path,
            document.path
        );
        let remote = read_remote_document(&mut socket, document).await?;
        pending.extend(remote.tracked_change_ids.iter().cloned());
        reviewed_documents.push((doc_id.clone(), remote));
    }
    for (doc_id, document) in &documents {
        if state.documents.contains_key(doc_id) {
            continue;
        }
        let remote = read_remote_document(&mut socket, document).await?;
        pending.extend(remote.tracked_change_ids);
    }

    socket.close().await.ok();
    ensure!(
        pending.is_empty(),
        "review still has pending tracked changes: {}",
        pending.into_iter().collect::<Vec<_>>().join(", ")
    );

    let store = database(root)?;
    for (doc_id, remote) in &reviewed_documents {
        if !remote.tracked_change_ids.is_empty() {
            continue;
        }
        let Some(receipt_id) = state
            .documents
            .get(doc_id)
            .and_then(|document| document.receipt_id.as_deref())
        else {
            continue;
        };
        let Some(operation) = store.operation(receipt_id)? else {
            continue;
        };
        match operation.status {
            OperationStatus::Prepared => store.mark_failed(
                receipt_id,
                "review finished before the prepared operation was sent",
            )?,
            OperationStatus::Inflight | OperationStatus::Unknown => {
                store.mark_confirmed(receipt_id)?;
            }
            OperationStatus::Confirmed | OperationStatus::Failed => {}
        }
    }
    require_no_unresolved_receipts(&local_status(root).await?)?;

    for (doc_id, remote) in &reviewed_documents {
        state
            .documents
            .get_mut(doc_id)
            .expect("document key exists")
            .final_hash = Some(remote.hash.clone());
    }
    state.phase = ReviewPhase::Finishing;
    save_state(root, &state)?;

    for (_, remote) in &reviewed_documents {
        write_document(root, &remote.path, &remote.content)?;
    }
    let mut workspace = JjWorkspace::open(root).await?;
    let checkpoint = workspace.checkpoint(&state.description).await?;
    for (_, remote) in &reviewed_documents {
        store.upsert_document(
            &binding.project_id,
            &remote.id,
            &remote.path,
            remote.version,
            &remote.hash,
            Some(&checkpoint.operation_id),
        )?;
        store.upsert_document_metadata(
            &binding.project_id,
            &remote.id,
            remote.version,
            &remote.metadata_hash,
            &remote.ranges_json,
            &remote.snapshot_metadata_json,
        )?;
    }

    let pull = pull_project_with_api(root, session, api, profile).await?;
    ensure!(
        pull.success,
        "review result was preserved locally, but final pull reported a conflict"
    );
    let mut workspace = JjWorkspace::open(root).await?;
    let described = workspace.describe_change(&state.description).await?;
    clear_state(root)?;

    Ok(ReviewFinishSummary {
        success: true,
        project_id: binding.project_id,
        description: state.description,
        reviewed_files,
        unsubmitted_files,
        jj_operation_id: described.operation_id,
        pull,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::content_hash;

    #[test]
    fn extracts_legacy_and_history_change_ids_without_comments() {
        let ranges = json!({
            "changes": [{"id":"legacy-1"}],
            "comments": [{"id":"comment-1"}]
        });
        let snapshot = json!({
            "trackedChanges": [
                {"id":"history-1","tracking":{"type":"insert"}},
                {"tracking":{"id":"history-2","type":"delete"}},
                {"range":{"pos":3,"length":2},"tracking":{"type":"delete"}}
            ]
        });
        let ids = tracked_change_ids_from_values(&ranges, &snapshot);
        assert!(ids.contains("history-1"));
        assert!(ids.contains("history-2"));
        assert!(ids.contains("legacy-1"));
        assert!(ids.iter().any(|id| id.starts_with("history:")));
        assert!(!ids.contains("comment-1"));
    }

    fn test_document(path: &str, proposed_hash: Option<&str>) -> ReviewDocument {
        ReviewDocument {
            path: path.into(),
            base_version: 1,
            base_hash: "base".into(),
            base_metadata_hash: "metadata".into(),
            proposed_hash: proposed_hash.map(str::to_owned),
            change_ids: Vec::new(),
            submitted_version: None,
            receipt_id: None,
            final_hash: None,
        }
    }

    fn test_state(phase: ReviewPhase) -> ReviewState {
        ReviewState {
            schema_version: 1,
            project_id: "p1".into(),
            description: "test review".into(),
            phase,
            started_at: "2026-01-01T00:00:00Z".into(),
            base_operation_id: "op-base".into(),
            work_commit_id: "commit-work".into(),
            parent_commit_id: "commit-parent".into(),
            submitted_at: None,
            submitted_commit_id: None,
            documents: BTreeMap::new(),
        }
    }

    #[test]
    fn partial_submission_has_an_explicit_finish_path() {
        let mut state = test_state(ReviewPhase::Submitting);
        state
            .documents
            .insert("d2".into(), test_document("/z.tex", Some("proposal-z")));
        state
            .documents
            .insert("d1".into(), test_document("/a.tex", Some("proposal-a")));

        assert!(review_can_be_aborted(&state));
        assert!(!review_can_be_finished(&state));
        assert_eq!(
            unsubmitted_review_files(&state),
            vec!["/a.tex".to_owned(), "/z.tex".to_owned()]
        );

        state.documents.get_mut("d1").unwrap().receipt_id = Some("receipt-1".into());
        assert!(!review_can_be_aborted(&state));
        assert!(review_can_be_finished(&state));
        assert_eq!(unsubmitted_review_files(&state), vec!["/z.tex"]);
    }

    #[test]
    fn finishing_accepts_both_sides_of_an_interrupted_local_write() {
        let mut document = test_document("/main.tex", Some("proposal"));
        document.final_hash = Some("reviewed".into());

        assert!(finish_local_hash_is_valid(
            ReviewPhase::Submitted,
            &document,
            "proposal"
        ));
        assert!(!finish_local_hash_is_valid(
            ReviewPhase::Submitted,
            &document,
            "reviewed"
        ));
        assert!(finish_local_hash_is_valid(
            ReviewPhase::Finishing,
            &document,
            "proposal"
        ));
        assert!(finish_local_hash_is_valid(
            ReviewPhase::Finishing,
            &document,
            "reviewed"
        ));
        assert!(!finish_local_hash_is_valid(
            ReviewPhase::Finishing,
            &document,
            "new local edit"
        ));
    }

    #[tokio::test]
    async fn begin_requires_clean_state_and_blocks_sync() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let mut workspace = JjWorkspace::init(root).await.unwrap();
        std::fs::write(root.join("main.tex"), "base").unwrap();
        let checkpoint = workspace.checkpoint("remote base").await.unwrap();
        ProjectBinding {
            project_id: "p1".into(),
            base_url: "https://example.test".into(),
            profile: "default".into(),
        }
        .save(root)
        .unwrap();
        let store = database(root).unwrap();
        store
            .upsert_document(
                "p1",
                "d1",
                "/main.tex",
                3,
                &content_hash("base"),
                Some(&checkpoint.operation_id),
            )
            .unwrap();
        store
            .upsert_document_metadata("p1", "d1", 3, &bytes_hash(b"meta"), "{}", "{}")
            .unwrap();

        let begun = begin_review(root, "rewrite introduction").await.unwrap();
        assert_eq!(begun.phase, ReviewPhase::Draft);
        assert!(ensure_sync_allowed(root, "sync").is_err());
        assert!(begin_review(root, "second work").await.is_err());

        std::fs::write(root.join("main.tex"), "local draft").unwrap();
        let state = require_state(root).unwrap();
        assert_eq!(state.description, "rewrite introduction");
        assert_eq!(state.documents.len(), 1);

        let aborted = abort_review(root).await.unwrap();
        assert_eq!(aborted.restored_commit_id, checkpoint.commit_id);
        assert_eq!(
            std::fs::read_to_string(root.join("main.tex")).unwrap(),
            "base"
        );
        assert!(load_state(root).unwrap().is_none());
        ensure_sync_allowed(root, "sync").unwrap();
    }
}
