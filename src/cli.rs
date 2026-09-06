use std::collections::HashMap;
use std::io::{Cursor, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::DEFAULT_BASE_URL;
use crate::api::OverleafApi;
use crate::auth::{Session, SessionStore, interactive_login};
use crate::jj::JjWorkspace;
use crate::operations::{
    BuildOptions, Change, HISTORY_OT, InputChange, LEGACY_OT, TextSelector,
    apply_legacy_operations, build_document_operations, changes_for_text, locate_text,
    parse_document_snapshot, slice_utf16, utf16_len, validate_history_operations,
    visible_to_source_position,
};
use crate::project::{collect_documents, connect_project, find_document, root_folder_id};
use crate::socket::UpdateOptions;
use crate::sync::{clone_project, discover_root, local_status, pull_project, push_project};

#[derive(Parser)]
#[command(
    name = "jujuleaf",
    version,
    about = "Local-first Overleaf collaboration, powered by Jujutsu",
    long_about = "JujuLeaf translates editor changes into Overleaf OT events and gives every local project a native Jujutsu history."
)]
struct Cli {
    #[arg(long, global = true, help = "Pretty-print JSON output")]
    pretty: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in with Chrome/Chromium or an existing session cookie.
    Login {
        #[arg(long)]
        cookie: Option<String>,
        #[arg(long, default_value = DEFAULT_BASE_URL)]
        base_url: String,
    },
    /// List Overleaf projects.
    #[command(visible_alias = "ls-projects")]
    Projects,
    /// Create a blank Overleaf project.
    CreateProject {
        name: String,
    },
    /// Rename an Overleaf project.
    RenameProject {
        project_id: String,
        new_name: String,
    },
    /// List project entities.
    Files {
        project_id: String,
    },
    /// Read a document through the live collaboration channel.
    Read {
        project_id: String,
        path: String,
        #[arg(long)]
        raw: bool,
        #[arg(long)]
        meta: bool,
    },
    /// Find all overlapping exact-text matches in CodeMirror UTF-16 coordinates.
    Locate {
        project_id: String,
        path: String,
        #[arg(long, alias = "old")]
        text: String,
    },
    /// Make an exact-text or full-document edit.
    Edit(EditorArgs),
    /// Submit an edit as Overleaf tracked changes.
    Suggest(EditorArgs),
    /// Insert text at a UTF-16 position.
    Insert(EditorArgs),
    /// Delete an exact match or UTF-16 range.
    Delete(EditorArgs),
    /// Replace an exact match or UTF-16 range.
    Replace(EditorArgs),
    /// Apply ordered CodeMirror-style changes.
    ApplyChanges(EditorArgs),
    /// Validate and submit raw OT wire operations.
    ApplyOps(EditorArgs),
    /// Accept tracked changes by ID.
    AcceptChanges {
        project_id: String,
        doc_id: String,
        #[arg(required = true)]
        change_ids: Vec<String>,
    },
    CreateDoc {
        project_id: String,
        name: String,
        #[arg(long)]
        parent: Option<String>,
    },
    DeleteDoc {
        project_id: String,
        doc_id: String,
    },
    CreateFolder {
        project_id: String,
        name: String,
        #[arg(long)]
        parent: Option<String>,
    },
    DeleteFolder {
        project_id: String,
        folder_id: String,
    },
    Rename {
        project_id: String,
        entity_id: String,
        name: String,
        #[arg(long, value_enum, default_value_t = EntityType::Doc)]
        r#type: EntityType,
    },
    Move {
        project_id: String,
        entity_id: String,
        folder_id: String,
        #[arg(long, value_enum, default_value_t = EntityType::Doc)]
        r#type: EntityType,
    },
    Upload {
        project_id: String,
        local_path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        parent: Option<String>,
    },
    Download {
        project_id: String,
        path: String,
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
    },
    Compile {
        project_id: String,
        #[arg(long)]
        draft: bool,
    },
    Pdf {
        project_id: String,
        #[arg(short = 'o', long, default_value = "output.pdf")]
        output: PathBuf,
    },
    Zip {
        project_id: String,
        #[arg(short = 'o', long, default_value = "project.zip")]
        output: PathBuf,
    },
    Threads {
        project_id: String,
    },
    Comment {
        project_id: String,
        thread_id: String,
        #[arg(required = true, trailing_var_arg = true)]
        text: Vec<String>,
    },
    AddComment {
        project_id: String,
        path: String,
        #[arg(required = true, trailing_var_arg = true)]
        text: Vec<String>,
        #[arg(long)]
        at_text: Option<String>,
        #[arg(long)]
        position: Option<usize>,
        #[arg(long)]
        length: Option<usize>,
        #[arg(long)]
        occurrence: Option<usize>,
    },
    ResolveThread {
        project_id: String,
        doc_id: String,
        thread_id: String,
    },
    ReopenThread {
        project_id: String,
        doc_id: String,
        thread_id: String,
    },
    DeleteThread {
        project_id: String,
        doc_id: String,
        thread_id: String,
    },
    EditComment {
        project_id: String,
        thread_id: String,
        message_id: String,
        #[arg(required = true, trailing_var_arg = true)]
        text: Vec<String>,
    },
    DeleteComment {
        project_id: String,
        thread_id: String,
        message_id: String,
    },
    Diff {
        project_id: String,
        path: String,
        #[arg(long, default_value_t = 0)]
        from: i64,
        #[arg(long)]
        to: Option<i64>,
    },
    Search {
        project_id: String,
        #[arg(required = true, trailing_var_arg = true)]
        query: Vec<String>,
    },
    Watch {
        project_id: String,
    },
    History {
        project_id: String,
        #[arg(long, default_value_t = 10)]
        min_count: usize,
    },
    Wordcount {
        project_id: String,
    },

    /// Clone an Overleaf project into a native .jj workspace.
    #[command(visible_alias = "init")]
    Clone {
        project_id: String,
        #[arg(default_value = ".")]
        destination: PathBuf,
    },
    /// Pull remote documents without overwriting concurrent local edits.
    #[command(visible_alias = "fetch")]
    Pull {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Push local documents with version/hash conflict protection.
    #[command(visible_alias = "publish")]
    Push {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[command(flatten)]
        retry: RetryArgs,
    },
    /// Pull then push when no conflicts are present.
    Sync {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[command(flatten)]
        retry: RetryArgs,
    },
    /// Compare local files with the last confirmed remote checkpoint.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Record the current local files as a Jujutsu checkpoint.
    Checkpoint {
        #[arg(short, long, default_value = "manual checkpoint")]
        message: String,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Restore the previous Jujutsu operation.
    Undo {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Restore the operation most recently undone by JujuLeaf.
    Redo {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Args, Debug, Clone)]
struct EditorArgs {
    project_id: String,
    path: String,
    #[arg(long)]
    content: Option<String>,
    #[arg(long)]
    text: Option<String>,
    #[arg(long)]
    old: Option<String>,
    #[arg(long)]
    new: Option<String>,
    #[arg(long)]
    from: Option<usize>,
    #[arg(long)]
    to: Option<usize>,
    #[arg(long)]
    position: Option<usize>,
    #[arg(long)]
    length: Option<usize>,
    #[arg(long)]
    occurrence: Option<usize>,
    #[arg(long)]
    all: bool,
    #[arg(long)]
    changes: Option<String>,
    #[arg(long)]
    ops: Option<String>,
    #[arg(long)]
    tracked: bool,
    #[arg(long)]
    dry_run: bool,
    #[command(flatten)]
    retry: RetryArgs,
}

#[derive(Args, Debug, Clone)]
struct RetryArgs {
    #[arg(long, default_value_t = 45_000)]
    timeout: u64,
    #[arg(long, default_value_t = 5_000)]
    retry_after: u64,
}

impl RetryArgs {
    fn options(&self) -> Result<UpdateOptions> {
        ensure!(self.timeout > 0, "--timeout must be positive");
        ensure!(self.retry_after > 0, "--retry-after must be positive");
        Ok(UpdateOptions {
            timeout: Duration::from_millis(self.timeout),
            retry_after: Duration::from_millis(self.retry_after),
        })
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EntityType {
    Doc,
    File,
    Folder,
}

impl EntityType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Doc => "doc",
            Self::File => "file",
            Self::Folder => "folder",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorCommand {
    Edit,
    Suggest,
    Insert,
    Delete,
    Replace,
    ApplyChanges,
    ApplyOps,
}

fn output(value: impl Serialize, pretty: bool) -> Result<()> {
    if pretty {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", serde_json::to_string(&value)?);
    }
    Ok(())
}

async fn stdin_text() -> Result<Option<String>> {
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut text = String::new();
    tokio::io::stdin().read_to_string(&mut text).await?;
    (!text.is_empty()).then_some(text).pipe(Ok)
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

fn selector(args: &EditorArgs) -> TextSelector {
    TextSelector {
        position: args.position,
        occurrence: args.occurrence,
        all: args.all,
    }
}

fn changes_to_inputs(changes: Vec<Change>) -> Vec<InputChange> {
    changes
        .into_iter()
        .map(|change| InputChange {
            from: change.from,
            to: Some(change.to),
            insert: Some(change.insert),
            expect: Some(change.removed),
        })
        .collect()
}

fn explicit_range(args: &EditorArgs, content: &str) -> Result<(usize, usize)> {
    let content_len = utf16_len(content);
    let (from, to) = if args.from.is_some() || args.to.is_some() {
        ensure!(
            args.position.is_none() && args.length.is_none(),
            "--from/--to and --position/--length are mutually exclusive"
        );
        (
            args.from.ok_or_else(|| anyhow!("--from is required"))?,
            args.to.ok_or_else(|| anyhow!("--to is required"))?,
        )
    } else {
        let from = args
            .position
            .ok_or_else(|| anyhow!("provide --from/--to or --position/--length"))?;
        let length = args
            .length
            .ok_or_else(|| anyhow!("--length is required with --position"))?;
        (from, from.saturating_add(length))
    };
    ensure!(
        from <= to && to <= content_len,
        "range {from}..{to} is outside document range 0..{content_len}"
    );
    Ok((from, to))
}

async fn requested_changes(
    command: EditorCommand,
    args: &EditorArgs,
    content: &str,
) -> Result<Vec<InputChange>> {
    match command {
        EditorCommand::ApplyChanges => {
            let raw = match &args.changes {
                Some(raw) => raw.clone(),
                None => stdin_text()
                    .await?
                    .ok_or_else(|| anyhow!("provide --changes JSON or JSON on stdin"))?,
            };
            serde_json::from_str(&raw).context("invalid --changes JSON")
        }
        EditorCommand::Insert => {
            let position = args
                .position
                .ok_or_else(|| anyhow!("--position is required"))?;
            let insert = args
                .text
                .clone()
                .or_else(|| args.content.clone())
                .or(stdin_text().await?)
                .ok_or_else(|| anyhow!("provide --text, --content, or stdin"))?;
            Ok(vec![InputChange {
                from: position,
                to: Some(position),
                insert: Some(insert),
                expect: None,
            }])
        }
        EditorCommand::Delete
            if args.old.is_some()
                && args.from.is_none()
                && args.to.is_none()
                && args.length.is_none() =>
        {
            Ok(changes_to_inputs(changes_for_text(
                content,
                args.old.as_deref().unwrap(),
                "",
                &selector(args),
            )?))
        }
        EditorCommand::Delete => {
            let (from, to) = explicit_range(args, content)?;
            Ok(vec![InputChange {
                from,
                to: Some(to),
                insert: Some(String::new()),
                expect: args.old.clone(),
            }])
        }
        EditorCommand::Replace
            if args.old.is_some()
                && args.from.is_none()
                && args.to.is_none()
                && args.length.is_none() =>
        {
            let insert = args
                .new
                .as_deref()
                .or(args.text.as_deref())
                .or(args.content.as_deref())
                .ok_or_else(|| anyhow!("provide --new, --text, or --content"))?;
            Ok(changes_to_inputs(changes_for_text(
                content,
                args.old.as_deref().unwrap(),
                insert,
                &selector(args),
            )?))
        }
        EditorCommand::Replace => {
            let (from, to) = explicit_range(args, content)?;
            let insert = args
                .new
                .clone()
                .or_else(|| args.text.clone())
                .or_else(|| args.content.clone())
                .or(stdin_text().await?)
                .ok_or_else(|| anyhow!("provide replacement text"))?;
            Ok(vec![InputChange {
                from,
                to: Some(to),
                insert: Some(insert),
                expect: args.old.clone(),
            }])
        }
        EditorCommand::Edit | EditorCommand::Suggest if args.old.is_some() => {
            let new = args
                .new
                .as_deref()
                .ok_or_else(|| anyhow!("provide --new with --old"))?;
            Ok(changes_to_inputs(changes_for_text(
                content,
                args.old.as_deref().unwrap(),
                new,
                &selector(args),
            )?))
        }
        EditorCommand::Edit | EditorCommand::Suggest => {
            let insert = args
                .content
                .clone()
                .or_else(|| args.text.clone())
                .or(stdin_text().await?)
                .ok_or_else(|| {
                    anyhow!("provide --old/--new for a targeted edit or full content on stdin")
                })?;
            Ok(vec![InputChange {
                from: 0,
                to: Some(utf16_len(content)),
                insert: Some(insert),
                expect: None,
            }])
        }
        EditorCommand::ApplyOps => unreachable!(),
    }
}

async fn authenticated() -> Result<(SessionStore, Session, OverleafApi)> {
    let store = SessionStore::from_default_path()?;
    let session = store.require()?;
    let api = OverleafApi::new(&session, Some(store.clone()))?;
    Ok((store, session, api))
}

async fn read_remote_document(session: &Session, project_id: &str, path: &str) -> Result<Value> {
    let (mut socket, project) = connect_project(session, project_id).await?;
    let document =
        find_document(&project, path).ok_or_else(|| anyhow!("file not found: {path}"))?;
    let joined = socket.join_doc(&document.id).await?;
    let state = parse_document_snapshot(joined.snapshot, &joined.ot_type)?;
    socket.leave_doc(&document.id).await.ok();
    socket.close().await.ok();
    Ok(json!({
        "path": path,
        "content": state.content,
        "length": utf16_len(&state.content),
        "sourceLength": utf16_len(&state.source_content),
        "version": joined.version,
        "docId": document.id,
        "otType": joined.ot_type,
        "ranges": joined.ranges
    }))
}

async fn root_folder(session: &Session, project_id: &str) -> Result<String> {
    let (socket, project) = connect_project(session, project_id).await?;
    socket.close().await.ok();
    root_folder_id(&project).ok_or_else(|| anyhow!("could not determine root folder ID"))
}

async fn edit_remote(
    command: EditorCommand,
    args: EditorArgs,
    session: &Session,
    api: &mut OverleafApi,
    pretty: bool,
) -> Result<()> {
    let (mut socket, project) = connect_project(session, &args.project_id).await?;
    let document = find_document(&project, &args.path)
        .ok_or_else(|| anyhow!("file not found: {}", args.path))?;
    let joined = socket.join_doc(&document.id).await?;
    let state = parse_document_snapshot(joined.snapshot, &joined.ot_type)?;
    let tracked = command == EditorCommand::Suggest || args.tracked;

    let (ops, changes, expected_content) = if command == EditorCommand::ApplyOps {
        let raw = match &args.ops {
            Some(raw) => raw.clone(),
            None => stdin_text()
                .await?
                .ok_or_else(|| anyhow!("provide --ops JSON or JSON on stdin"))?,
        };
        let ops: Vec<Value> = serde_json::from_str(&raw).context("invalid --ops JSON")?;
        let expected = if state.ot_type == LEGACY_OT {
            Value::String(apply_legacy_operations(&state.content, &ops)?)
        } else {
            validate_history_operations(&state, &ops)?;
            ensure!(
                !tracked,
                "encode tracking inside raw history-ot operations or use apply-changes --tracked"
            );
            Value::Null
        };
        (ops, None, expected)
    } else {
        let requested = requested_changes(command, &args, &state.content).await?;
        let user_id = if tracked && state.ot_type == HISTORY_OT {
            Some(api.current_user_id(&args.project_id).await?)
        } else {
            None
        };
        let built = build_document_operations(
            &state,
            &requested,
            &BuildOptions {
                tracked,
                user_id,
                timestamp: None,
            },
        )?;
        (
            built.ops,
            Some(built.changes),
            Value::String(built.expected_content),
        )
    };

    let removed: usize = changes
        .as_ref()
        .map(|changes| {
            changes
                .iter()
                .map(|change| utf16_len(&change.removed))
                .sum()
        })
        .unwrap_or_default();
    let inserted: usize = changes
        .as_ref()
        .map(|changes| changes.iter().map(|change| utf16_len(&change.insert)).sum())
        .unwrap_or_default();
    let mut summary = json!({
        "success": true,
        "path": args.path,
        "docId": document.id,
        "otType": state.ot_type,
        "baseVersion": joined.version,
        "tracked": tracked,
        "operationCount": ops.len(),
        "changeCount": changes.as_ref().map(Vec::len),
        "removed": removed,
        "inserted": inserted
    });

    if args.dry_run {
        summary["dryRun"] = json!(true);
        summary["operations"] = json!(ops);
        summary["changes"] = json!(changes);
        summary["expectedContent"] = expected_content;
        socket.leave_doc(&document.id).await.ok();
        socket.close().await.ok();
        return output(summary, pretty);
    }
    if ops.is_empty() {
        summary["noop"] = json!(true);
        socket.leave_doc(&document.id).await.ok();
        socket.close().await.ok();
        return output(summary, pretty);
    }
    let update_options = args.retry.options()?;
    let confirmation = if tracked && state.ot_type == LEGACY_OT {
        socket
            .apply_tracked_update(&document.id, &ops, joined.version, &update_options)
            .await?
    } else {
        socket
            .apply_update(&document.id, &ops, joined.version, None, &update_options)
            .await?
    };
    socket.leave_doc(&document.id).await.ok();
    socket.close().await.ok();
    summary["confirmed"] = json!(confirmation.confirmed);
    summary["acknowledgedVersion"] = json!(confirmation.version);
    summary["attempts"] = json!(confirmation.attempts);
    output(summary, pretty)
}

fn search_zip(zip: &[u8], query: &str) -> Result<Value> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip)).context("invalid project zip")?;
    let mut matches = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        let extension = Path::new(&name)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if !["tex", "bib", "sty", "cls", "txt"].contains(&extension) {
            continue;
        }
        let mut content = String::new();
        if entry.read_to_string(&mut content).is_err() {
            continue;
        }
        for (line_index, line) in content.lines().enumerate() {
            if line.contains(query) {
                matches.push(json!({
                    "file": name,
                    "line": line_index + 1,
                    "text": line
                }));
            }
        }
    }
    Ok(json!({"query": query, "matchCount": matches.len(), "matches": matches}))
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let pretty = cli.pretty;
    match cli.command {
        Command::Login { cookie, base_url } => {
            let cookie = match cookie {
                Some(cookie) => cookie,
                None => {
                    eprintln!("Opening Chrome for Overleaf sign-in…");
                    interactive_login(&base_url).await?
                }
            };
            let store = SessionStore::from_default_path()?;
            let session = Session::new(cookie, base_url);
            store.save(&session)?;
            let mut api = OverleafApi::new(&session, Some(store))?;
            let projects = api.list_projects().await?;
            output(
                json!({
                    "success": true,
                    "projectCount": projects.get("projects").and_then(Value::as_array).map(Vec::len).unwrap_or(0)
                }),
                pretty,
            )
        }
        Command::Status { path } => {
            let root = discover_root(path)?;
            output(local_status(&root).await?, pretty)
        }
        Command::Checkpoint { message, path } => {
            let root = discover_root(path)?;
            let mut workspace = JjWorkspace::open(root).await?;
            output(workspace.checkpoint(&message).await?, pretty)
        }
        Command::Undo { path } => {
            let root = discover_root(path)?;
            let mut workspace = JjWorkspace::open(root).await?;
            output(workspace.undo().await?, pretty)
        }
        Command::Redo { path } => {
            let root = discover_root(path)?;
            let mut workspace = JjWorkspace::open(root).await?;
            output(workspace.redo().await?, pretty)
        }
        command => {
            let (_store, session, mut api) = authenticated().await?;
            dispatch_authenticated(command, session, &mut api, pretty).await
        }
    }
}

async fn dispatch_authenticated(
    command: Command,
    session: Session,
    api: &mut OverleafApi,
    pretty: bool,
) -> Result<()> {
    match command {
        Command::Projects => output(api.list_projects().await?, pretty),
        Command::CreateProject { name } => output(api.create_project(&name).await?, pretty),
        Command::RenameProject {
            project_id,
            new_name,
        } => output(api.rename_project(&project_id, &new_name).await?, pretty),
        Command::Files { project_id } => output(api.entities(&project_id).await?, pretty),
        Command::Read {
            project_id,
            path,
            raw,
            meta,
        } => {
            let value = read_remote_document(&session, &project_id, &path).await?;
            if raw {
                print!("{}", value["content"].as_str().unwrap_or_default());
                Ok(())
            } else if meta {
                output(value, pretty)
            } else {
                output(json!({"path": path, "content": value["content"]}), pretty)
            }
        }
        Command::Locate {
            project_id,
            path,
            text,
        } => {
            let value = read_remote_document(&session, &project_id, &path).await?;
            let matches = locate_text(value["content"].as_str().unwrap_or_default(), &text)?;
            output(
                json!({"path": path, "text": text, "matchCount": matches.len(), "matches": matches}),
                pretty,
            )
        }
        Command::Edit(args) => edit_remote(EditorCommand::Edit, args, &session, api, pretty).await,
        Command::Suggest(args) => {
            edit_remote(EditorCommand::Suggest, args, &session, api, pretty).await
        }
        Command::Insert(args) => {
            edit_remote(EditorCommand::Insert, args, &session, api, pretty).await
        }
        Command::Delete(args) => {
            edit_remote(EditorCommand::Delete, args, &session, api, pretty).await
        }
        Command::Replace(args) => {
            edit_remote(EditorCommand::Replace, args, &session, api, pretty).await
        }
        Command::ApplyChanges(args) => {
            edit_remote(EditorCommand::ApplyChanges, args, &session, api, pretty).await
        }
        Command::ApplyOps(args) => {
            edit_remote(EditorCommand::ApplyOps, args, &session, api, pretty).await
        }
        Command::AcceptChanges {
            project_id,
            doc_id,
            change_ids,
        } => output(
            api.accept_changes(&project_id, &doc_id, &change_ids)
                .await?,
            pretty,
        ),
        Command::CreateDoc {
            project_id,
            name,
            parent,
        } => {
            let parent = match parent {
                Some(parent) => parent,
                None => root_folder(&session, &project_id).await?,
            };
            output(
                api.create_doc(&project_id, &name, Some(&parent)).await?,
                pretty,
            )
        }
        Command::DeleteDoc { project_id, doc_id } => {
            output(api.delete_doc(&project_id, &doc_id).await?, pretty)
        }
        Command::CreateFolder {
            project_id,
            name,
            parent,
        } => {
            let parent = match parent {
                Some(parent) => parent,
                None => root_folder(&session, &project_id).await?,
            };
            output(
                api.create_folder(&project_id, &name, Some(&parent)).await?,
                pretty,
            )
        }
        Command::DeleteFolder {
            project_id,
            folder_id,
        } => output(api.delete_folder(&project_id, &folder_id).await?, pretty),
        Command::Rename {
            project_id,
            entity_id,
            name,
            r#type,
        } => output(
            api.rename_entity(&project_id, r#type.as_str(), &entity_id, &name)
                .await?,
            pretty,
        ),
        Command::Move {
            project_id,
            entity_id,
            folder_id,
            r#type,
        } => output(
            api.move_entity(&project_id, r#type.as_str(), &entity_id, &folder_id)
                .await?,
            pretty,
        ),
        Command::Upload {
            project_id,
            local_path,
            name,
            parent,
        } => {
            let parent = match parent {
                Some(parent) => parent,
                None => root_folder(&session, &project_id).await?,
            };
            let name = name
                .or_else(|| {
                    local_path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .map(ToOwned::to_owned)
                })
                .ok_or_else(|| anyhow!("could not determine remote filename"))?;
            output(
                api.upload(&project_id, &parent, &local_path, &name).await?,
                pretty,
            )
        }
        Command::Download {
            project_id,
            path,
            output: output_path,
        } => {
            let value = read_remote_document(&session, &project_id, &path).await?;
            let output_path = output_path.unwrap_or_else(|| {
                Path::new(&path)
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("document.tex"))
            });
            let content = value["content"].as_str().unwrap_or_default();
            tokio::fs::write(&output_path, content).await?;
            output(
                json!({"success": true, "path": output_path, "bytes": content.len()}),
                pretty,
            )
        }
        Command::Compile { project_id, draft } => {
            output(api.compile(&project_id, draft).await?, pretty)
        }
        Command::Pdf {
            project_id,
            output: path,
        } => {
            let bytes = api.download_pdf(&project_id).await?;
            tokio::fs::write(&path, &bytes).await?;
            output(
                json!({"success": true, "path": path, "bytes": bytes.len()}),
                pretty,
            )
        }
        Command::Zip {
            project_id,
            output: path,
        } => {
            let bytes = api.download_zip(&project_id).await?;
            tokio::fs::write(&path, &bytes).await?;
            output(
                json!({"success": true, "path": path, "bytes": bytes.len()}),
                pretty,
            )
        }
        Command::Threads { project_id } => output(api.threads(&project_id).await?, pretty),
        Command::Comment {
            project_id,
            thread_id,
            text,
        } => output(
            api.send_message(&project_id, &thread_id, &text.join(" "))
                .await?,
            pretty,
        ),
        Command::AddComment {
            project_id,
            path,
            text,
            at_text,
            position,
            length,
            occurrence,
        } => {
            let (mut socket, project) = connect_project(&session, &project_id).await?;
            let document =
                find_document(&project, &path).ok_or_else(|| anyhow!("file not found: {path}"))?;
            let joined = socket.join_doc(&document.id).await?;
            let state = parse_document_snapshot(joined.snapshot, &joined.ot_type)?;
            let (position, selected) = if let Some(anchor) = at_text {
                let change = changes_for_text(
                    &state.content,
                    &anchor,
                    &anchor,
                    &TextSelector {
                        occurrence,
                        ..Default::default()
                    },
                )?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("comment anchor not found"))?;
                (change.from, anchor)
            } else {
                let position =
                    position.ok_or_else(|| anyhow!("provide --at-text or --position"))?;
                let end = (position + length.unwrap_or(20)).min(utf16_len(&state.content));
                (
                    position,
                    slice_utf16(&state.content, position, end)?.to_owned(),
                )
            };
            ensure!(!selected.is_empty(), "a comment anchor cannot be empty");
            let random: [u8; 12] = rand::random();
            let thread_id = hex::encode(random);
            let ops = if state.ot_type == HISTORY_OT {
                let from = visible_to_source_position(position, &state)?;
                let to = visible_to_source_position(position + utf16_len(&selected), &state)?;
                let ops = vec![json!({
                    "commentId": thread_id,
                    "ranges": [{"pos": from, "length": to - from}]
                })];
                validate_history_operations(&state, &ops)?;
                ops
            } else {
                vec![json!({"c": selected, "p": position, "t": thread_id})]
            };
            api.send_message(&project_id, &thread_id, &text.join(" "))
                .await?;
            let confirmation = socket
                .apply_update(
                    &document.id,
                    &ops,
                    joined.version,
                    None,
                    &UpdateOptions::default(),
                )
                .await?;
            socket.leave_doc(&document.id).await.ok();
            socket.close().await.ok();
            output(
                json!({
                    "success": true,
                    "confirmed": confirmation.confirmed,
                    "threadId": thread_id,
                    "path": path,
                    "position": position,
                    "selectedText": selected,
                    "otType": state.ot_type
                }),
                pretty,
            )
        }
        Command::ResolveThread {
            project_id,
            doc_id,
            thread_id,
        } => output(
            api.set_thread_resolved(&project_id, &doc_id, &thread_id, true)
                .await?,
            pretty,
        ),
        Command::ReopenThread {
            project_id,
            doc_id,
            thread_id,
        } => output(
            api.set_thread_resolved(&project_id, &doc_id, &thread_id, false)
                .await?,
            pretty,
        ),
        Command::DeleteThread {
            project_id,
            doc_id,
            thread_id,
        } => output(
            api.delete_thread(&project_id, &doc_id, &thread_id).await?,
            pretty,
        ),
        Command::EditComment {
            project_id,
            thread_id,
            message_id,
            text,
        } => output(
            api.edit_message(&project_id, &thread_id, &message_id, &text.join(" "))
                .await?,
            pretty,
        ),
        Command::DeleteComment {
            project_id,
            thread_id,
            message_id,
        } => output(
            api.delete_message(&project_id, &thread_id, &message_id)
                .await?,
            pretty,
        ),
        Command::Diff {
            project_id,
            path,
            from,
            to,
        } => {
            let to = match to {
                Some(to) => to,
                None => api
                    .updates(&project_id, 1)
                    .await?
                    .pointer("/updates/0/toV")
                    .and_then(Value::as_i64)
                    .unwrap_or(1),
            };
            output(
                api.diff(&project_id, path.trim_start_matches('/'), from, to)
                    .await?,
                pretty,
            )
        }
        Command::Search { project_id, query } => {
            let query = query.join(" ");
            let zip = api.download_zip(&project_id).await?;
            output(search_zip(&zip, &query)?, pretty)
        }
        Command::Watch { project_id } => {
            let (mut socket, project) = connect_project(&session, &project_id).await?;
            let documents = collect_documents(&project);
            let paths: HashMap<_, _> = documents
                .iter()
                .map(|document| (document.id.clone(), document.path.clone()))
                .collect();
            for document in &documents {
                socket.join_doc(&document.id).await?;
            }
            eprintln!(
                "Watching {} documents. Press Ctrl+C to stop.",
                documents.len()
            );
            loop {
                let event = socket.next_event().await?;
                let doc_id = event
                    .args
                    .first()
                    .and_then(|value| value.get("doc"))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        event
                            .args
                            .get(1)
                            .and_then(|value| value.get("doc_id"))
                            .and_then(Value::as_str)
                    });
                output(
                    json!({
                        "type": event.name,
                        "args": event.args,
                        "path": doc_id.and_then(|id| paths.get(id)),
                        "timestamp": chrono::Utc::now().to_rfc3339()
                    }),
                    false,
                )?;
            }
        }
        Command::History {
            project_id,
            min_count,
        } => output(api.updates(&project_id, min_count).await?, pretty),
        Command::Wordcount { project_id } => output(api.word_count(&project_id).await?, pretty),
        Command::Clone {
            project_id,
            destination,
        } => output(
            clone_project(api, &session, &project_id, &destination).await?,
            pretty,
        ),
        Command::Pull { path } => {
            let root = discover_root(path)?;
            output(pull_project(&root, &session).await?, pretty)
        }
        Command::Push { path, retry } => {
            let root = discover_root(path)?;
            output(
                push_project(&root, &session, &retry.options()?).await?,
                pretty,
            )
        }
        Command::Sync { path, retry } => {
            let root = discover_root(path)?;
            let pull = pull_project(&root, &session).await?;
            if !pull.conflicts.is_empty() {
                return output(json!({"success": false, "pull": pull}), pretty);
            }
            let push = push_project(&root, &session, &retry.options()?).await?;
            output(
                json!({"success": push.success, "pull": pull, "push": push}),
                pretty,
            )
        }
        Command::Login { .. }
        | Command::Status { .. }
        | Command::Checkpoint { .. }
        | Command::Undo { .. }
        | Command::Redo { .. } => unreachable!(),
    }
}

pub fn print_json_error(error: &anyhow::Error) {
    eprintln!("{}", json!({"error": format!("{error:#}")}));
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn editor_flags_parse_like_the_original_cli() {
        let cli = Cli::try_parse_from([
            "jujuleaf",
            "replace",
            "project",
            "main.tex",
            "--old",
            "cat",
            "--new",
            "fox",
            "--occurrence",
            "2",
        ])
        .unwrap();
        let Command::Replace(args) = cli.command else {
            panic!("wrong command");
        };
        assert_eq!(args.old.as_deref(), Some("cat"));
        assert_eq!(args.occurrence, Some(2));
    }
}
