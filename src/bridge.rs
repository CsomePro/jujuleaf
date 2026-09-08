use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{SecondsFormat, TimeZone, Utc};
use clap::{Args, Subcommand};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, sleep};
use uuid::Uuid;

use crate::api::OverleafApi;
use crate::auth::ProfileStore;
use crate::operations::{
    DocumentState, HISTORY_OT, parse_document_snapshot, slice_utf16, source_to_visible_position,
    utf16_len,
};
use crate::project::{DocumentRef, collect_documents, connect_project_with_api};
use crate::socket::{OverleafSocket, RealtimeEvent};
use crate::sync::discover_project_context;

const PROTOCOL_NAME: &str = "jujuleaf.bridge";
const PROTOCOL_VERSION: u32 = 1;
const CONTEXT_UNITS: usize = 160;

#[derive(Debug, Subcommand)]
pub enum BridgeCommand {
    /// Describe supported protocol versions and operations.
    Describe,
    /// Read normalized Overleaf comment data and events.
    Comments {
        #[command(subcommand)]
        command: BridgeCommentsCommand,
    },
}

impl BridgeCommand {
    fn operation(&self) -> &'static str {
        match self {
            Self::Describe => "describe",
            Self::Comments { command } => command.operation(),
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum BridgeCommentsCommand {
    /// Return every comment thread with normalized anchors and document snapshots.
    List(BridgeProtocolArgs),
    /// Return one authoritative comment thread context.
    Get {
        /// Stable Overleaf thread ID.
        thread_id: String,
        #[command(flatten)]
        protocol: BridgeProtocolArgs,
    },
    /// Stream normalized comment events with snapshots and automatic reconciliation.
    Watch {
        #[command(flatten)]
        protocol: BridgeProtocolArgs,
        /// Seconds between authoritative full snapshots.
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(5..))]
        reconcile_interval: u64,
    },
}

impl BridgeCommentsCommand {
    fn operation(&self) -> &'static str {
        match self {
            Self::List(_) => "comments.list",
            Self::Get { .. } => "comments.get",
            Self::Watch { .. } => "comments.watch",
        }
    }

    fn protocol(&self) -> u32 {
        match self {
            Self::List(args) => args.protocol,
            Self::Get { protocol, .. } | Self::Watch { protocol, .. } => protocol.protocol,
        }
    }
}

#[derive(Debug, Args)]
pub struct BridgeProtocolArgs {
    /// JujuLeaf bridge protocol version selected by the caller.
    #[arg(long, default_value_t = PROTOCOL_VERSION)]
    protocol: u32,
}

#[derive(Debug, thiserror::Error)]
#[error("bridge output already reported")]
pub struct ReportedBridgeExit {
    code: i32,
}

impl ReportedBridgeExit {
    pub fn code(&self) -> i32 {
        self.code
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct BridgeFault {
    code: &'static str,
    message: String,
    retryable: bool,
}

impl BridgeFault {
    fn new(code: &'static str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectDescriptor {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentSnapshot {
    id: String,
    path: String,
    remote_version: i64,
    content_hash: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct TextRange {
    from: usize,
    to: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommentAnchor {
    document_id: String,
    path: String,
    state: &'static str,
    source_range_utf16: TextRange,
    visible_range_utf16: TextRange,
    text: String,
    before: String,
    after: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Actor {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommentMessage {
    id: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<Actor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    edited_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommentThread {
    id: String,
    state: &'static str,
    messages: Vec<CommentMessage>,
    anchors: Vec<CommentAnchor>,
    anchor_state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detached_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_by: Option<Actor>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommentsSnapshot {
    project: ProjectDescriptor,
    documents: Vec<DocumentSnapshot>,
    threads: Vec<CommentThread>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadContext {
    project: ProjectDescriptor,
    documents: Vec<DocumentSnapshot>,
    thread: CommentThread,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommentEvent {
    r#type: &'static str,
    project_id: String,
    thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor: Option<Actor>,
    observed_at: String,
}

struct StreamEmitter {
    session_id: String,
    sequence: u64,
}

impl StreamEmitter {
    fn new() -> Self {
        Self {
            session_id: Uuid::new_v4().to_string(),
            sequence: 0,
        }
    }

    fn emit(&mut self, kind: &str, data: Value) -> Result<()> {
        self.sequence += 1;
        write_json(&json!({
            "protocol": protocol(PROTOCOL_VERSION),
            "kind": kind,
            "operation": "comments.watch",
            "ok": true,
            "sessionId": self.session_id,
            "sequence": self.sequence,
            "data": data
        }))
    }
}

enum ListenerItem {
    Event(RealtimeEvent),
    Closed(String),
}

pub async fn run(
    command: BridgeCommand,
    explicit_profile: Option<String>,
    project_override: Option<String>,
    current_dir: &Path,
    raw: bool,
    pretty: bool,
) -> Result<()> {
    let operation = command.operation();
    let result = execute(
        command,
        explicit_profile.as_deref(),
        project_override.as_deref(),
        current_dir,
        raw,
        pretty,
    )
    .await;
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let (code, message, retryable) = classify_error(&error);
            let exit_code = if code == "UNSUPPORTED_PROTOCOL" { 2 } else { 1 };
            let _ = write_json(&json!({
                "protocol": protocol(PROTOCOL_VERSION),
                "kind": "error",
                "operation": operation,
                "ok": false,
                "error": {
                    "code": code,
                    "message": message,
                    "retryable": retryable
                }
            }));
            Err(anyhow::Error::new(ReportedBridgeExit { code: exit_code }))
        }
    }
}

async fn execute(
    command: BridgeCommand,
    explicit_profile: Option<&str>,
    project_override: Option<&str>,
    current_dir: &Path,
    raw: bool,
    pretty: bool,
) -> Result<()> {
    ensure!(
        !raw && !pretty,
        BridgeFault::new(
            "UNSUPPORTED_PROTOCOL",
            "bridge output has a fixed JSON/NDJSON representation; do not use --raw or --pretty",
            false,
        )
    );

    match command {
        BridgeCommand::Describe => write_result("describe", describe()),
        BridgeCommand::Comments { command } => {
            ensure_protocol(command.protocol())?;
            let (project_id, context_profile) = resolve_project(project_override, current_dir)?;
            let profiles = ProfileStore::from_default_path()?;
            let profile = profiles.resolve(explicit_profile.or(context_profile.as_deref()))?;
            let session_store = profiles.session_store(&profile)?;
            let session = session_store.require().with_context(|| {
                format!("profile '{profile}' is not authenticated; run: jujuleaf login --profile {profile}")
            })?;
            let mut api = OverleafApi::new(&session, Some(session_store))?;

            match command {
                BridgeCommentsCommand::List(_) => {
                    let snapshot = build_snapshot(&mut api, &project_id).await?;
                    write_result("comments.list", snapshot)
                }
                BridgeCommentsCommand::Get { thread_id, .. } => {
                    let snapshot = build_snapshot(&mut api, &project_id).await?;
                    let context = thread_context(snapshot, &thread_id)?;
                    write_result("comments.get", context)
                }
                BridgeCommentsCommand::Watch {
                    reconcile_interval, ..
                } => {
                    watch_comments(
                        &mut api,
                        &project_id,
                        Duration::from_secs(reconcile_interval),
                    )
                    .await
                }
            }
        }
    }
}

fn protocol(version: u32) -> Value {
    json!({"name": PROTOCOL_NAME, "version": version})
}

fn describe() -> Value {
    json!({
        "protocol": {
            "name": PROTOCOL_NAME,
            "supportedVersions": [PROTOCOL_VERSION],
            "defaultVersion": PROTOCOL_VERSION
        },
        "binary": {"version": env!("CARGO_PKG_VERSION")},
        "operations": {
            "comments.list": {"versions": [1], "output": "json"},
            "comments.get": {"versions": [1], "output": "json"},
            "comments.watch": {
                "versions": [1],
                "output": "ndjson",
                "initialSnapshot": true,
                "automaticReconciliation": true
            }
        },
        "features": {
            "commentEvents": {
                "versions": [1],
                "types": [
                    "message.created",
                    "message.edited",
                    "message.deleted",
                    "thread.resolved",
                    "thread.reopened",
                    "thread.deleted",
                    "thread.changed"
                ]
            },
            "threadContext": {"versions": [1], "multipleAnchors": true},
            "skillInstall": {"targets": []}
        },
        "errors": [
            "JUJULEAF_NOT_INITIALIZED",
            "AUTH_REQUIRED",
            "PROJECT_NOT_FOUND",
            "THREAD_NOT_FOUND",
            "CONNECTION_LOST",
            "RATE_LIMITED",
            "CONFLICT",
            "UNSUPPORTED_PROTOCOL",
            "INTERNAL_ERROR"
        ]
    })
}

fn ensure_protocol(version: u32) -> Result<()> {
    ensure!(
        version == PROTOCOL_VERSION,
        BridgeFault::new(
            "UNSUPPORTED_PROTOCOL",
            format!(
                "unsupported bridge protocol {version}; supported versions: {PROTOCOL_VERSION}"
            ),
            false,
        )
    );
    Ok(())
}

fn resolve_project(
    project_override: Option<&str>,
    current_dir: &Path,
) -> Result<(String, Option<String>)> {
    if let Some(project_id) = project_override {
        ensure!(
            !project_id.trim().is_empty(),
            BridgeFault::new("PROJECT_NOT_FOUND", "project ID cannot be empty", false)
        );
        return Ok((project_id.to_owned(), None));
    }
    let context = discover_project_context(current_dir)?.ok_or_else(|| {
        BridgeFault::new(
            "JUJULEAF_NOT_INITIALIZED",
            "no JujuLeaf project was found; run inside a clone or pass --project-id",
            false,
        )
    })?;
    Ok((context.project_id, context.profile))
}

fn write_result(operation: &str, data: impl Serialize) -> Result<()> {
    write_json(&json!({
        "protocol": protocol(PROTOCOL_VERSION),
        "kind": "result",
        "operation": operation,
        "ok": true,
        "data": data
    }))
}

fn write_json(value: &Value) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn classify_error(error: &anyhow::Error) -> (&'static str, String, bool) {
    if let Some(fault) = error.downcast_ref::<BridgeFault>() {
        return (fault.code, fault.message.clone(), fault.retryable);
    }

    let message = format!("{error:#}");
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("not authenticated")
        || normalized.contains("session expired")
        || normalized.contains("401 unauthorized")
    {
        ("AUTH_REQUIRED", message, false)
    } else if normalized.contains("429") || normalized.contains("rate limit") {
        ("RATE_LIMITED", message, true)
    } else if normalized.contains("404")
        || normalized.contains("project not found")
        || normalized.contains("no project tree")
    {
        ("PROJECT_NOT_FOUND", message, false)
    } else if normalized.contains("conflict") {
        ("CONFLICT", message, false)
    } else if normalized.contains("socket.io")
        || normalized.contains("connection closed")
        || normalized.contains("timed out")
        || error
            .chain()
            .any(|cause| cause.downcast_ref::<reqwest::Error>().is_some())
    {
        ("CONNECTION_LOST", message, true)
    } else {
        ("INTERNAL_ERROR", message, false)
    }
}

async fn build_snapshot(api: &mut OverleafApi, project_id: &str) -> Result<CommentsSnapshot> {
    let raw_threads = api.threads(project_id).await?;
    let (mut socket, project) = connect_project_with_api(api, project_id).await?;
    let mut documents = collect_documents(&project);
    documents.sort_by(|left, right| left.path.cmp(&right.path));

    let mut document_snapshots = Vec::with_capacity(documents.len());
    let mut anchors: HashMap<String, Vec<CommentAnchor>> = HashMap::new();
    for document in &documents {
        let joined = socket
            .join_doc(&document.id)
            .await
            .with_context(|| format!("failed to read {}", document.path))?;
        let state = parse_document_snapshot(joined.snapshot, &joined.ot_type)
            .with_context(|| format!("failed to parse {}", document.path))?;
        collect_anchors(document, &state, &joined.ranges, &mut anchors);
        document_snapshots.push(DocumentSnapshot {
            id: document.id.clone(),
            path: clean_path(&document.path),
            remote_version: joined.version,
            content_hash: sha256(&state.content),
        });
        socket.leave_doc(&document.id).await.ok();
    }
    socket.close().await.ok();

    for thread_anchors in anchors.values_mut() {
        thread_anchors.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(
                    left.visible_range_utf16
                        .from
                        .cmp(&right.visible_range_utf16.from),
                )
                .then(
                    left.visible_range_utf16
                        .to
                        .cmp(&right.visible_range_utf16.to),
                )
        });
    }

    Ok(CommentsSnapshot {
        project: ProjectDescriptor {
            id: project_id.to_owned(),
            name: project
                .get("name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        },
        documents: document_snapshots,
        threads: normalize_threads(&raw_threads, anchors),
    })
}

fn thread_context(mut snapshot: CommentsSnapshot, thread_id: &str) -> Result<ThreadContext> {
    let index = snapshot
        .threads
        .iter()
        .position(|thread| thread.id == thread_id)
        .ok_or_else(|| {
            BridgeFault::new(
                "THREAD_NOT_FOUND",
                format!("comment thread '{thread_id}' was not found"),
                false,
            )
        })?;
    let thread = snapshot.threads.remove(index);
    let document_ids: HashSet<_> = thread
        .anchors
        .iter()
        .map(|anchor| anchor.document_id.as_str())
        .collect();
    snapshot
        .documents
        .retain(|document| document_ids.contains(document.id.as_str()));
    Ok(ThreadContext {
        project: snapshot.project,
        documents: snapshot.documents,
        thread,
    })
}

fn clean_path(path: &str) -> String {
    path.trim_start_matches('/').to_owned()
}

fn sha256(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn collect_anchors(
    document: &DocumentRef,
    state: &DocumentState,
    ranges: &Value,
    anchors: &mut HashMap<String, Vec<CommentAnchor>>,
) {
    if state.ot_type == HISTORY_OT
        && let Some(comments) = state.raw.get("comments").and_then(Value::as_array)
    {
        for comment in comments {
            let Some(thread_id) = comment.get("id").and_then(Value::as_str) else {
                continue;
            };
            for range in comment
                .get("ranges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some((position, length)) = raw_range(range) else {
                    continue;
                };
                if let Some(anchor) = make_anchor(document, state, position, length) {
                    anchors
                        .entry(thread_id.to_owned())
                        .or_default()
                        .push(anchor);
                }
            }
        }
        return;
    }

    for comment in ranges
        .get("comments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let operation = comment.get("op").unwrap_or(comment);
        let Some(thread_id) = comment
            .get("id")
            .or_else(|| operation.get("t"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(position) = operation.get("p").and_then(Value::as_u64) else {
            continue;
        };
        let length = operation
            .get("c")
            .and_then(Value::as_str)
            .map(utf16_len)
            .or_else(|| {
                operation
                    .get("length")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
            })
            .unwrap_or(0);
        if let Some(anchor) = make_anchor(document, state, position as usize, length) {
            anchors
                .entry(thread_id.to_owned())
                .or_default()
                .push(anchor);
        }
    }
}

fn raw_range(value: &Value) -> Option<(usize, usize)> {
    Some((
        value.get("pos")?.as_u64()? as usize,
        value.get("length")?.as_u64()? as usize,
    ))
}

fn make_anchor(
    document: &DocumentRef,
    state: &DocumentState,
    source_from: usize,
    source_length: usize,
) -> Option<CommentAnchor> {
    let source_to = source_from.checked_add(source_length)?;
    let visible_from = source_to_visible_position(source_from, state).ok()?;
    let visible_to = source_to_visible_position(source_to, state).ok()?;
    let text = slice_utf16(&state.content, visible_from, visible_to)
        .ok()?
        .to_owned();
    let before_from =
        floor_utf16_boundary(&state.content, visible_from.saturating_sub(CONTEXT_UNITS));
    let after_to = ceil_utf16_boundary(
        &state.content,
        (visible_to + CONTEXT_UNITS).min(utf16_len(&state.content)),
    );
    Some(CommentAnchor {
        document_id: document.id.clone(),
        path: clean_path(&document.path),
        state: if visible_from == visible_to {
            "collapsed"
        } else {
            "attached"
        },
        source_range_utf16: TextRange {
            from: source_from,
            to: source_to,
        },
        visible_range_utf16: TextRange {
            from: visible_from,
            to: visible_to,
        },
        text,
        before: slice_utf16(&state.content, before_from, visible_from)
            .unwrap_or_default()
            .to_owned(),
        after: slice_utf16(&state.content, visible_to, after_to)
            .unwrap_or_default()
            .to_owned(),
    })
}

fn floor_utf16_boundary(text: &str, target: usize) -> usize {
    let mut position = 0;
    for character in text.chars() {
        let next = position + character.len_utf16();
        if next > target {
            return position;
        }
        position = next;
    }
    position
}

fn ceil_utf16_boundary(text: &str, target: usize) -> usize {
    let mut position = 0;
    for character in text.chars() {
        let next = position + character.len_utf16();
        if position >= target {
            return position;
        }
        if next >= target {
            return next;
        }
        position = next;
    }
    position
}

fn normalize_threads(
    raw: &Value,
    mut anchors: HashMap<String, Vec<CommentAnchor>>,
) -> Vec<CommentThread> {
    let source = raw.get("threads").unwrap_or(raw);
    let Some(object) = source.as_object() else {
        return Vec::new();
    };
    let mut threads = object
        .iter()
        .map(|(thread_id, value)| {
            let thread_anchors = anchors.remove(thread_id).unwrap_or_default();
            let anchor_state = if thread_anchors
                .iter()
                .any(|anchor| anchor.state == "attached")
            {
                "attached"
            } else if !thread_anchors.is_empty() {
                "collapsed"
            } else {
                "detached"
            };
            let resolved = value
                .get("resolved")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            CommentThread {
                id: thread_id.clone(),
                state: if resolved { "resolved" } else { "open" },
                messages: value
                    .get("messages")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(normalize_message)
                    .collect(),
                anchors: thread_anchors,
                anchor_state,
                detached_reason: (anchor_state == "detached").then_some("range_missing"),
                resolved_at: value.get("resolved_at").and_then(timestamp),
                resolved_by: actor(
                    value.get("resolved_by_user"),
                    value.get("resolved_by_user_id"),
                ),
            }
        })
        .collect::<Vec<_>>();
    threads.sort_by(|left, right| left.id.cmp(&right.id));
    threads
}

fn normalize_message(value: &Value) -> Option<CommentMessage> {
    Some(CommentMessage {
        id: value.get("id")?.as_str()?.to_owned(),
        content: value
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        author: actor(value.get("user"), value.get("user_id")),
        created_at: value.get("timestamp").and_then(timestamp),
        edited_at: value.get("edited_at").and_then(timestamp),
    })
}

fn actor(user: Option<&Value>, explicit_id: Option<&Value>) -> Option<Actor> {
    let id = explicit_id
        .and_then(Value::as_str)
        .or_else(|| user.and_then(|user| user.get("id")).and_then(Value::as_str))
        .filter(|id| !id.is_empty())?;
    let name = user.and_then(|user| {
        user.get("name")
            .and_then(Value::as_str)
            .or_else(|| user.get("first_name").and_then(Value::as_str))
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
    });
    Some(Actor {
        id: id.to_owned(),
        name,
    })
}

fn timestamp(value: &Value) -> Option<String> {
    if let Some(value) = value.as_str() {
        return (!value.is_empty()).then(|| value.to_owned());
    }
    let milliseconds = value.as_i64()?;
    if milliseconds <= 0 {
        return None;
    }
    Utc.timestamp_millis_opt(milliseconds)
        .single()
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn spawn_listener(mut socket: OverleafSocket) -> (mpsc::Receiver<ListenerItem>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(256);
    let task = tokio::spawn(async move {
        loop {
            match socket.next_event().await {
                Ok(event) => {
                    if sender.send(ListenerItem::Event(event)).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender
                        .send(ListenerItem::Closed(format!("{error:#}")))
                        .await;
                    return;
                }
            }
        }
    });
    (receiver, task)
}

async fn open_listener(api: &mut OverleafApi, project_id: &str) -> Result<OverleafSocket> {
    let (socket, _) = connect_project_with_api(api, project_id).await?;
    Ok(socket)
}

async fn watch_comments(
    api: &mut OverleafApi,
    project_id: &str,
    reconcile_every: Duration,
) -> Result<()> {
    api.threads(project_id).await?;
    let socket = open_listener(api, project_id).await?;
    let (mut events, mut listener_task) = spawn_listener(socket);
    let mut emitter = StreamEmitter::new();
    emitter.emit(
        "stream.opened",
        json!({"projectId": project_id, "reconcileIntervalSeconds": reconcile_every.as_secs()}),
    )?;

    let snapshot = build_snapshot(api, project_id).await?;
    emitter.emit(
        "comments.snapshot",
        json!({"reason": "initial", "snapshot": snapshot}),
    )?;
    emitter.emit("stream.ready", json!({"projectId": project_id}))?;

    let mut reconcile = interval(reconcile_every);
    reconcile.set_missed_tick_behavior(MissedTickBehavior::Skip);
    reconcile.tick().await;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            item = events.recv() => {
                match item {
                    Some(ListenerItem::Event(event)) => {
                        for event in normalize_event(project_id, event) {
                            emitter.emit("comment.event", serde_json::to_value(event)?)?;
                        }
                    }
                    Some(ListenerItem::Closed(message)) => {
                        emitter.emit(
                            "stream.reset",
                            json!({"reason": "connection_lost", "message": message, "retryable": true}),
                        )?;
                        listener_task.abort();

                        let mut retry_seconds = 1u64;
                        let socket = loop {
                            match open_listener(api, project_id).await {
                                Ok(socket) => break socket,
                                Err(connection_error) => {
                                    if let Err(validation_error) = api.threads(project_id).await {
                                        let (_, _, retryable) = classify_error(&validation_error);
                                        if !retryable {
                                            return Err(validation_error);
                                        }
                                    }
                                    let (_, _, retryable) = classify_error(&connection_error);
                                    if !retryable {
                                        return Err(connection_error);
                                    }
                                    tokio::select! {
                                        _ = sleep(Duration::from_secs(retry_seconds)) => {}
                                        signal = &mut shutdown => {
                                            signal?;
                                            emitter.emit("stream.closed", json!({"reason": "interrupted"}))?;
                                            return Ok(());
                                        }
                                    }
                                    retry_seconds = (retry_seconds * 2).min(30);
                                }
                            }
                        };
                        (events, listener_task) = spawn_listener(socket);
                        let snapshot = build_snapshot(api, project_id).await?;
                        emitter.emit(
                            "comments.snapshot",
                            json!({"reason": "reconnect", "snapshot": snapshot}),
                        )?;
                        emitter.emit("stream.ready", json!({"projectId": project_id, "resumed": true}))?;
                    }
                    None => {
                        return Err(BridgeFault::new(
                            "CONNECTION_LOST",
                            "comment event stream closed without a reason",
                            true,
                        ).into());
                    }
                }
            }
            _ = reconcile.tick() => {
                let snapshot = build_snapshot(api, project_id).await?;
                emitter.emit(
                    "comments.snapshot",
                    json!({"reason": "reconcile", "snapshot": snapshot}),
                )?;
            }
            signal = &mut shutdown => {
                signal?;
                listener_task.abort();
                emitter.emit("stream.closed", json!({"reason": "interrupted"}))?;
                return Ok(());
            }
        }
    }
}

fn normalize_event(project_id: &str, event: RealtimeEvent) -> Vec<CommentEvent> {
    let observed_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let make = |event_type, thread_id: &str, message_id: Option<String>, actor| CommentEvent {
        r#type: event_type,
        project_id: project_id.to_owned(),
        thread_id: thread_id.to_owned(),
        message_id,
        actor,
        observed_at: observed_at.clone(),
    };
    let thread_id = || event.args.first().and_then(Value::as_str);

    match event.name.as_str() {
        "new-comment" => thread_id()
            .map(|thread_id| {
                let message = event.args.get(1);
                vec![make(
                    "message.created",
                    thread_id,
                    message
                        .and_then(|message| message.get("id"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    actor(
                        message.and_then(|message| message.get("user")),
                        message.and_then(|message| message.get("user_id")),
                    ),
                )]
            })
            .unwrap_or_default(),
        "edit-message" => thread_id()
            .map(|thread_id| {
                vec![make(
                    "message.edited",
                    thread_id,
                    event
                        .args
                        .get(1)
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    None,
                )]
            })
            .unwrap_or_default(),
        "delete-message" => thread_id()
            .map(|thread_id| {
                vec![make(
                    "message.deleted",
                    thread_id,
                    event
                        .args
                        .get(1)
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    None,
                )]
            })
            .unwrap_or_default(),
        "resolve-thread" => thread_id()
            .map(|thread_id| {
                vec![make(
                    "thread.resolved",
                    thread_id,
                    None,
                    actor(event.args.get(1), None),
                )]
            })
            .unwrap_or_default(),
        "reopen-thread" => thread_id()
            .map(|thread_id| vec![make("thread.reopened", thread_id, None, None)])
            .unwrap_or_default(),
        "delete-thread" => thread_id()
            .map(|thread_id| vec![make("thread.deleted", thread_id, None, None)])
            .unwrap_or_default(),
        "new-comment-threads" => event
            .args
            .first()
            .and_then(Value::as_object)
            .map(|threads| {
                threads
                    .keys()
                    .map(|thread_id| make("thread.changed", thread_id, None, None))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::DeleteRange;

    fn document() -> DocumentRef {
        DocumentRef {
            id: "doc-1".into(),
            path: "/main.tex".into(),
        }
    }

    #[test]
    fn history_anchors_keep_multiple_ranges_and_map_tracked_deletes() {
        let state = DocumentState {
            ot_type: HISTORY_OT.into(),
            content: "abcdef".into(),
            source_content: "abcXXdef".into(),
            tracked_delete_ranges: vec![DeleteRange { pos: 3, length: 2 }],
            raw: json!({
                "content": "abcXXdef",
                "comments": [{
                    "id": "thread-1",
                    "ranges": [{"pos": 2, "length": 4}, {"pos": 7, "length": 1}]
                }]
            }),
        };
        let mut anchors = HashMap::new();
        collect_anchors(&document(), &state, &json!({}), &mut anchors);
        let anchors = &anchors["thread-1"];
        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[0].source_range_utf16, TextRange { from: 2, to: 6 });
        assert_eq!(anchors[0].visible_range_utf16, TextRange { from: 2, to: 4 });
        assert_eq!(anchors[0].text, "cd");
        assert_eq!(anchors[1].text, "f");
    }

    #[test]
    fn legacy_empty_comment_range_is_a_collapsed_anchor() {
        let state = DocumentState {
            ot_type: "sharejs-text-ot".into(),
            content: "hello".into(),
            source_content: "hello".into(),
            tracked_delete_ranges: vec![],
            raw: json!(["hello"]),
        };
        let mut anchors = HashMap::new();
        collect_anchors(
            &document(),
            &state,
            &json!({"comments":[{"id":"thread-1","op":{"p":3,"c":"","t":"thread-1"}}]}),
            &mut anchors,
        );
        assert_eq!(anchors["thread-1"][0].state, "collapsed");
        assert_eq!(
            anchors["thread-1"][0].visible_range_utf16,
            TextRange { from: 3, to: 3 }
        );
    }

    #[test]
    fn thread_messages_are_normalized_without_private_overleaf_fields() {
        let threads = normalize_threads(
            &json!({"thread-1": {
                "resolved": true,
                "resolved_at": "2026-09-08T10:20:30Z",
                "messages": [{
                    "id":"message-1",
                    "content":"review this",
                    "timestamp":1788862830000i64,
                    "edited_at":0,
                    "user_id":"user-1",
                    "user":{"first_name":"Alice","email":"hidden@example.test"}
                }]
            }}),
            HashMap::new(),
        );
        let value = serde_json::to_value(&threads[0]).unwrap();
        assert_eq!(value["state"], "resolved");
        assert_eq!(value["anchorState"], "detached");
        assert_eq!(value["messages"][0]["author"]["name"], "Alice");
        assert!(value["messages"][0]["author"].get("email").is_none());
        assert!(value["messages"][0].get("editedAt").is_none());
    }

    #[test]
    fn private_socket_events_become_stable_wakeup_events() {
        let events = normalize_event(
            "project-1",
            RealtimeEvent {
                name: "new-comment".into(),
                args: vec![
                    json!("thread-1"),
                    json!({"id":"message-1","user_id":"user-1"}),
                ],
            },
        );
        let value = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(value["type"], "message.created");
        assert_eq!(value["threadId"], "thread-1");
        assert_eq!(value["messageId"], "message-1");

        assert!(
            normalize_event(
                "project-1",
                RealtimeEvent {
                    name: "otUpdate".into(),
                    args: vec![]
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn errors_are_classified_into_stable_codes() {
        let error = anyhow::anyhow!("profile 'work' is not authenticated");
        assert_eq!(classify_error(&error).0, "AUTH_REQUIRED");
        let error = anyhow::anyhow!("get threads failed (401 Unauthorized)");
        assert_eq!(classify_error(&error).0, "AUTH_REQUIRED");
        let error = anyhow::anyhow!(BridgeFault::new("THREAD_NOT_FOUND", "missing", false,));
        assert_eq!(classify_error(&error).0, "THREAD_NOT_FOUND");
    }
}
