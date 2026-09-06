use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Cursor, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::api::OverleafApi;
use crate::auth::{LoginPreset, ProfileStore, Session, SessionStore, interactive_login};
use crate::compile::{build_compile_report, has_output};
use crate::jj::JjWorkspace;
use crate::operations::{
    BuildOptions, Change, HISTORY_OT, InputChange, LEGACY_OT, TextSelector,
    apply_legacy_operations, build_document_operations, changes_for_text, locate_text,
    parse_document_snapshot, slice_utf16, utf16_len, validate_history_operations,
    visible_to_source_position,
};
use crate::project::{collect_documents, connect_project_with_api, find_document, root_folder_id};
use crate::socket::UpdateOptions;
use crate::sync::{
    ProjectBinding, clone_project, discover_root, local_status, pull_project_with_api,
    push_project_with_api,
};

#[derive(Parser)]
#[command(
    name = "jujuleaf",
    version,
    about = "Local-first Overleaf collaboration, powered by Jujutsu",
    long_about = "JujuLeaf translates editor changes into Overleaf OT events and gives every local project a native Jujutsu history.",
    arg_required_else_help = true,
    after_long_help = "Examples:\n  jujuleaf login --preset cstcloud\n  jujuleaf projects\n  jujuleaf files PROJECT_ID\n  jujuleaf read PROJECT_ID main.tex --content-only\n  jujuleaf replace PROJECT_ID main.tex --old 'before' --new 'after'\n  jujuleaf clone PROJECT_ID ./paper\n\nRun `jujuleaf <COMMAND> --help` for command-specific arguments and examples."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        conflicts_with = "raw",
        help = "Output indented JSON"
    )]
    pretty: bool,

    #[arg(
        long,
        global = true,
        conflicts_with = "pretty",
        help = "Output compact JSON instead of the human-readable view"
    )]
    raw: bool,

    #[arg(
        long,
        global = true,
        value_name = "NAME",
        help = "Use a named Overleaf account/endpoint profile"
    )]
    profile: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in with Chrome/Chromium or an existing session cookie.
    Login {
        /// Use an existing Cookie header or a bare session-cookie value.
        #[arg(long)]
        cookie: Option<String>,
        /// Override the endpoint selected by the login preset.
        #[arg(long)]
        base_url: Option<String>,
        /// Select the browser entry point and accepted authentication cookies.
        #[arg(
            long,
            alias = "instance",
            value_enum,
            default_value_t = LoginPreset::Auto
        )]
        preset: LoginPreset,
    },
    /// List, inspect, select, or delete saved profiles.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// List Overleaf projects.
    #[command(visible_alias = "ls-projects")]
    Projects,
    /// Create a blank Overleaf project.
    CreateProject {
        /// Name of the new project.
        name: String,
    },
    /// Rename an Overleaf project.
    RenameProject {
        /// Overleaf project ID shown by `jujuleaf projects`.
        project_id: String,
        /// New project name.
        new_name: String,
    },
    /// List all documents, uploaded files, and folders in a project.
    Files {
        /// Overleaf project ID shown by `jujuleaf projects`.
        project_id: String,
    },
    /// Read a document through the live collaboration channel.
    Read {
        /// Overleaf project ID shown by `jujuleaf projects`.
        project_id: String,
        /// Remote document path, for example `main.tex` or `chapters/intro.tex`.
        path: String,
        /// Print only the document text, without labels or metadata.
        #[arg(long)]
        content_only: bool,
        /// Include document ID, OT type, version, ranges, and length metadata.
        #[arg(long)]
        meta: bool,
    },
    /// Find all overlapping exact-text matches in CodeMirror UTF-16 coordinates.
    Locate {
        /// Overleaf project ID shown by `jujuleaf projects`.
        project_id: String,
        /// Remote document path.
        path: String,
        /// Exact text to find; `--old` is accepted as an alias.
        #[arg(long, alias = "old")]
        text: String,
    },
    /// Make an exact-text or full-document edit.
    #[command(
        after_long_help = "Examples:\n  jujuleaf edit PROJECT_ID main.tex --old 'before' --new 'after'\n  jujuleaf edit PROJECT_ID main.tex --content 'complete replacement'\n  cat main.tex | jujuleaf edit PROJECT_ID main.tex"
    )]
    Edit(EditorArgs),
    /// Submit an edit as Overleaf tracked changes.
    #[command(
        after_long_help = "Example:\n  jujuleaf suggest PROJECT_ID main.tex --old 'original' --new 'suggested'"
    )]
    Suggest(EditorArgs),
    /// Insert text at a UTF-16 position.
    #[command(
        after_long_help = "Example:\n  jujuleaf insert PROJECT_ID main.tex --position 120 --text 'inserted text'"
    )]
    Insert(EditorArgs),
    /// Delete an exact match or UTF-16 range.
    #[command(
        after_long_help = "Examples:\n  jujuleaf delete PROJECT_ID main.tex --old 'unique text'\n  jujuleaf delete PROJECT_ID main.tex --from 120 --to 140 --old 'expected text'"
    )]
    Delete(EditorArgs),
    /// Replace an exact match or UTF-16 range.
    #[command(
        after_long_help = "Examples:\n  jujuleaf replace PROJECT_ID main.tex --old 'before' --new 'after'\n  jujuleaf replace PROJECT_ID main.tex --old 'term' --new 'word' --occurrence 2\n  jujuleaf replace PROJECT_ID main.tex --from 120 --to 126 --new 'replacement'"
    )]
    Replace(EditorArgs),
    /// Apply ordered CodeMirror-style changes.
    #[command(
        after_long_help = "Example:\n  jujuleaf apply-changes PROJECT_ID main.tex --changes '[{\"from\":10,\"to\":16,\"insert\":\"new\",\"expect\":\"oldest\"}]'"
    )]
    ApplyChanges(EditorArgs),
    /// Validate and submit raw OT wire operations.
    #[command(
        after_long_help = "Advanced command. Prefer `replace`, `insert`, `delete`, or `apply-changes` unless you already have operations encoded for the document's active OT format."
    )]
    ApplyOps(EditorArgs),
    /// Accept tracked changes by ID.
    AcceptChanges {
        /// Overleaf project ID.
        project_id: String,
        /// Document ID shown by `files` or `read --meta`.
        doc_id: String,
        /// One or more tracked-change IDs to accept.
        #[arg(required = true)]
        change_ids: Vec<String>,
    },
    /// Create an empty text document in the project root or a folder.
    CreateDoc {
        /// Overleaf project ID.
        project_id: String,
        /// New document name, including extension.
        name: String,
        /// Parent folder ID; defaults to the project root folder.
        #[arg(long)]
        parent: Option<String>,
    },
    /// Permanently delete a text document by document ID.
    DeleteDoc {
        /// Overleaf project ID.
        project_id: String,
        /// Document ID shown by `jujuleaf files PROJECT_ID`.
        doc_id: String,
    },
    /// Permanently delete an uploaded binary file by file ID.
    DeleteFile {
        /// Overleaf project ID.
        project_id: String,
        /// Uploaded-file ID shown by `jujuleaf files PROJECT_ID`.
        file_id: String,
    },
    /// Create a folder in the project root or another folder.
    CreateFolder {
        /// Overleaf project ID.
        project_id: String,
        /// New folder name.
        name: String,
        /// Parent folder ID; defaults to the project root folder.
        #[arg(long)]
        parent: Option<String>,
    },
    /// Permanently delete a folder by folder ID.
    DeleteFolder {
        /// Overleaf project ID.
        project_id: String,
        /// Folder ID shown by `jujuleaf files PROJECT_ID`.
        folder_id: String,
    },
    /// Rename a document, uploaded file, or folder by entity ID.
    Rename {
        /// Overleaf project ID.
        project_id: String,
        /// Document, uploaded-file, or folder ID.
        entity_id: String,
        /// New entity name.
        name: String,
        /// Entity kind associated with ENTITY_ID.
        #[arg(long, value_enum, default_value_t = EntityType::Doc)]
        r#type: EntityType,
    },
    /// Move a document, uploaded file, or folder into another folder.
    Move {
        /// Overleaf project ID.
        project_id: String,
        /// Document, uploaded-file, or folder ID to move.
        entity_id: String,
        /// Destination folder ID.
        folder_id: String,
        /// Entity kind associated with ENTITY_ID.
        #[arg(long, value_enum, default_value_t = EntityType::Doc)]
        r#type: EntityType,
    },
    /// Upload a local file to the project root or a folder.
    Upload {
        /// Overleaf project ID.
        project_id: String,
        /// Path to the local file to upload.
        local_path: PathBuf,
        /// Remote filename; defaults to the local filename.
        #[arg(long)]
        name: Option<String>,
        /// Destination folder ID; defaults to the project root folder.
        #[arg(long)]
        parent: Option<String>,
    },
    /// Download a remote text document to a local file.
    Download {
        /// Overleaf project ID.
        project_id: String,
        /// Remote document path.
        path: String,
        /// Local output path; defaults to the remote document filename.
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
    },
    /// Compile a project, wait for completion, and parse its log diagnostics.
    Compile {
        /// Overleaf project ID.
        project_id: String,
        /// Request Overleaf's faster draft compilation mode.
        #[arg(long)]
        draft: bool,
        /// Maximum time to wait for Overleaf, in seconds.
        #[arg(long, default_value_t = 720)]
        timeout: u64,
        /// Include the complete output.log text in command output.
        #[arg(long)]
        show_log: bool,
        /// Save the complete output.log to this local path.
        #[arg(long)]
        log_output: Option<PathBuf>,
    },
    /// Compile a project and download the resulting PDF.
    Pdf {
        /// Overleaf project ID.
        project_id: String,
        /// Local PDF path.
        #[arg(short = 'o', long, default_value = "output.pdf")]
        output: PathBuf,
    },
    /// Download the complete project source as a ZIP archive.
    Zip {
        /// Overleaf project ID.
        project_id: String,
        /// Local ZIP path.
        #[arg(short = 'o', long, default_value = "project.zip")]
        output: PathBuf,
    },
    /// List comment threads and their messages for a project.
    Threads {
        /// Overleaf project ID.
        project_id: String,
    },
    /// Add a message to an existing comment thread.
    Comment {
        /// Overleaf project ID.
        project_id: String,
        /// Existing thread ID shown by `jujuleaf threads PROJECT_ID`.
        thread_id: String,
        /// Message text; multiple words are joined with spaces.
        #[arg(required = true, trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// Create a comment thread anchored to exact text or a UTF-16 range.
    AddComment {
        /// Overleaf project ID.
        project_id: String,
        /// Remote document path.
        path: String,
        /// Comment text; put selector options before this trailing text.
        #[arg(required = true, trailing_var_arg = true)]
        text: Vec<String>,
        /// Anchor the comment to an exact text match.
        #[arg(long)]
        at_text: Option<String>,
        /// Anchor start in CodeMirror UTF-16 units.
        #[arg(long)]
        position: Option<usize>,
        /// Anchor length in UTF-16 units; defaults to 20 with `--position`.
        #[arg(long)]
        length: Option<usize>,
        /// Select a 1-based occurrence when `--at-text` is not unique.
        #[arg(long)]
        occurrence: Option<usize>,
    },
    /// Mark a comment thread as resolved.
    ResolveThread {
        /// Overleaf project ID.
        project_id: String,
        /// Document ID containing the thread.
        doc_id: String,
        /// Thread ID shown by `jujuleaf threads PROJECT_ID`.
        thread_id: String,
    },
    /// Reopen a resolved comment thread.
    ReopenThread {
        /// Overleaf project ID.
        project_id: String,
        /// Document ID containing the thread.
        doc_id: String,
        /// Thread ID shown by `jujuleaf threads PROJECT_ID`.
        thread_id: String,
    },
    /// Permanently delete a comment thread.
    DeleteThread {
        /// Overleaf project ID.
        project_id: String,
        /// Document ID containing the thread.
        doc_id: String,
        /// Thread ID shown by `jujuleaf threads PROJECT_ID`.
        thread_id: String,
    },
    /// Replace the text of an existing comment message.
    EditComment {
        /// Overleaf project ID.
        project_id: String,
        /// Thread ID containing the message.
        thread_id: String,
        /// Message ID shown by `jujuleaf threads PROJECT_ID`.
        message_id: String,
        /// Replacement message text; multiple words are joined with spaces.
        #[arg(required = true, trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// Permanently delete a comment message.
    DeleteComment {
        /// Overleaf project ID.
        project_id: String,
        /// Thread ID containing the message.
        thread_id: String,
        /// Message ID shown by `jujuleaf threads PROJECT_ID`.
        message_id: String,
    },
    /// Show remote document changes between two project-history versions.
    Diff {
        /// Overleaf project ID.
        project_id: String,
        /// Remote document path.
        path: String,
        /// Starting project-history version.
        #[arg(long, default_value_t = 0)]
        from: i64,
        /// Ending version; defaults to the latest reported project version.
        #[arg(long)]
        to: Option<i64>,
    },
    /// Search text-like files in a downloaded project snapshot.
    Search {
        /// Overleaf project ID.
        project_id: String,
        /// Literal, case-sensitive search query; words are joined with spaces.
        #[arg(required = true, trailing_var_arg = true)]
        query: Vec<String>,
    },
    /// Stream live collaboration events until interrupted.
    Watch {
        /// Overleaf project ID.
        project_id: String,
    },
    /// Show recent remote project update batches.
    History {
        /// Overleaf project ID.
        project_id: String,
        /// Minimum number of update batches to request.
        #[arg(long, default_value_t = 10)]
        min_count: usize,
    },
    /// Show Overleaf's word-count result for a project.
    Wordcount {
        /// Overleaf project ID.
        project_id: String,
    },

    /// Clone an Overleaf project into a native .jj workspace.
    #[command(visible_alias = "init")]
    Clone {
        /// Overleaf project ID shown by `jujuleaf projects`.
        project_id: String,
        /// Empty or new destination directory.
        #[arg(default_value = ".")]
        destination: PathBuf,
    },
    /// Pull remote documents without overwriting concurrent local edits.
    #[command(visible_alias = "fetch")]
    Pull {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Push local documents with version/hash conflict protection.
    #[command(visible_alias = "publish")]
    Push {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
        #[command(flatten)]
        retry: RetryArgs,
    },
    /// Pull then push once, or keep synchronizing in the foreground.
    Sync {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
        #[command(flatten)]
        retry: RetryArgs,
        /// Keep polling local and remote state until Ctrl+C or a conflict.
        #[arg(long)]
        watch: bool,
        /// Delay between foreground sync cycles, in milliseconds.
        #[arg(long, default_value_t = 2_000)]
        interval: u64,
    },
    /// Compare local files with the last confirmed remote checkpoint.
    Status {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Record the current local files as a Jujutsu checkpoint.
    Checkpoint {
        /// Description stored with the Jujutsu checkpoint.
        #[arg(short, long, default_value = "manual checkpoint")]
        message: String,
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Restore the previous Jujutsu operation.
    Undo {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Restore the operation most recently undone by JujuLeaf.
    Redo {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// List profiles without exposing cookies.
    List,
    /// Set the active profile used outside a bound local clone.
    Use {
        /// Profile name to make active.
        name: String,
    },
    /// Show profile metadata without exposing its cookie.
    #[command(visible_alias = "current")]
    Show {
        /// Profile name; defaults to the selected or active profile.
        name: Option<String>,
    },
    /// Delete a saved profile. Local clones and remote sessions are untouched.
    Delete {
        /// Profile name to remove from local configuration.
        name: String,
    },
}

#[derive(Args, Debug, Clone)]
struct EditorArgs {
    /// Overleaf project ID shown by `jujuleaf projects`.
    project_id: String,
    /// Remote document path.
    path: String,
    /// Full document, inserted, or replacement content depending on the command.
    #[arg(long)]
    content: Option<String>,
    /// Inserted or replacement text; an alternative to `--content`.
    #[arg(long)]
    text: Option<String>,
    /// Exact source text to replace/delete, or expected text for a range edit.
    #[arg(long)]
    old: Option<String>,
    /// Replacement text used with `--old`.
    #[arg(long)]
    new: Option<String>,
    /// Range start in CodeMirror UTF-16 units.
    #[arg(long)]
    from: Option<usize>,
    /// Exclusive range end in CodeMirror UTF-16 units.
    #[arg(long)]
    to: Option<usize>,
    /// Exact-match start or range start in CodeMirror UTF-16 units.
    #[arg(long)]
    position: Option<usize>,
    /// Range length in UTF-16 units.
    #[arg(long)]
    length: Option<usize>,
    /// Select a 1-based occurrence when `--old` is not unique.
    #[arg(long)]
    occurrence: Option<usize>,
    /// Apply the same exact-text operation to every occurrence.
    #[arg(long)]
    all: bool,
    /// JSON array of ordered `{from,to,insert,expect}` changes for `apply-changes`.
    #[arg(long)]
    changes: Option<String>,
    /// JSON array of advanced Overleaf OT wire operations for `apply-ops`.
    #[arg(long)]
    ops: Option<String>,
    /// Encode the operation as tracked changes when the OT format supports it.
    #[arg(long)]
    tracked: bool,
    /// Validate and display generated operations without sending them.
    #[arg(long)]
    dry_run: bool,
    #[command(flatten)]
    retry: RetryArgs,
}

#[derive(Args, Debug, Clone)]
struct RetryArgs {
    /// Overall confirmation timeout in milliseconds.
    #[arg(long, default_value_t = 45_000)]
    timeout: u64,
    /// Delay before checking an uncertain update again, in milliseconds.
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
    /// Collaborative text document.
    Doc,
    /// Uploaded file.
    File,
    /// Folder.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Human,
    RawJson,
    PrettyJson,
}

impl OutputMode {
    fn from_flags(raw: bool, pretty: bool) -> Self {
        if raw {
            Self::RawJson
        } else if pretty {
            Self::PrettyJson
        } else {
            Self::Human
        }
    }
}

fn human_label(key: &str) -> String {
    let key = key.trim_start_matches('_');
    let mut label = String::new();
    let mut previous_was_lowercase = false;
    for character in key.chars() {
        if character == '_' || character == '-' {
            if !label.ends_with(' ') {
                label.push(' ');
            }
            previous_was_lowercase = false;
        } else {
            if character.is_uppercase() && previous_was_lowercase {
                label.push(' ');
            }
            label.extend(character.to_uppercase());
            previous_was_lowercase = character.is_lowercase() || character.is_ascii_digit();
        }
    }
    label
}

fn human_scalar(value: &Value) -> String {
    match value {
        Value::Null => "-".to_owned(),
        Value::Bool(true) => "yes".to_owned(),
        Value::Bool(false) => "no".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(string) => string.clone(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| "-".to_owned())
        }
    }
}

fn table_columns(rows: &[Value]) -> Option<Vec<String>> {
    let mut columns = Vec::new();
    for row in rows {
        let object = row.as_object()?;
        if object.values().any(Value::is_array) || object.values().any(Value::is_object) {
            return None;
        }
        for key in object.keys() {
            if !columns.contains(key) {
                columns.push(key.clone());
            }
        }
    }
    (!columns.is_empty() && columns.len() <= 8).then_some(columns)
}

fn write_table(output: &mut String, rows: &[Value], indent: usize) {
    if rows.is_empty() {
        let _ = writeln!(output, "{}(none)", " ".repeat(indent));
        return;
    }
    let Some(columns) = table_columns(rows) else {
        for (index, row) in rows.iter().enumerate() {
            let _ = writeln!(output, "{}[{}]", " ".repeat(indent), index + 1);
            write_human_value(output, row, indent + 2);
        }
        return;
    };
    let headers: Vec<_> = columns.iter().map(|column| human_label(column)).collect();
    let cells: Vec<Vec<_>> = rows
        .iter()
        .map(|row| {
            let object = row.as_object().expect("table rows are objects");
            columns
                .iter()
                .map(|column| object.get(column).map(human_scalar).unwrap_or_default())
                .collect()
        })
        .collect();
    let widths: Vec<_> = (0..columns.len())
        .map(|index| {
            cells
                .iter()
                .map(|row| row[index].chars().count())
                .chain(std::iter::once(headers[index].chars().count()))
                .max()
                .unwrap_or_default()
        })
        .collect();
    let write_row = |output: &mut String, row: &[String]| {
        output.push_str(&" ".repeat(indent));
        for (index, cell) in row.iter().enumerate() {
            output.push_str(cell);
            if index + 1 < row.len() {
                output
                    .push_str(&" ".repeat(widths[index].saturating_sub(cell.chars().count()) + 2));
            }
        }
        output.push('\n');
    };
    write_row(output, &headers);
    write_row(
        output,
        &widths
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<_>>(),
    );
    for row in cells {
        write_row(output, &row);
    }
}

fn write_human_value(output: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Object(object) => {
            for (key, value) in object
                .iter()
                .filter(|(_, value)| !value.is_array() && !value.is_object())
            {
                let _ = writeln!(
                    output,
                    "{}{}: {}",
                    " ".repeat(indent),
                    human_label(key),
                    human_scalar(value)
                );
            }
            for (key, value) in object
                .iter()
                .filter(|(_, value)| value.is_array() || value.is_object())
            {
                if !output.is_empty() && !output.ends_with("\n\n") {
                    output.push('\n');
                }
                match value {
                    Value::Array(values) => {
                        let _ = writeln!(
                            output,
                            "{}{} ({})",
                            " ".repeat(indent),
                            human_label(key),
                            values.len()
                        );
                        if values.is_empty() {
                            let _ = writeln!(output, "{}(none)", " ".repeat(indent + 2));
                        } else {
                            write_table(output, values, indent + 2);
                        }
                    }
                    Value::Object(_) => {
                        let _ = writeln!(output, "{}{}", " ".repeat(indent), human_label(key));
                        write_human_value(output, value, indent + 2);
                    }
                    _ => unreachable!(),
                }
            }
        }
        Value::Array(values) => write_table(output, values, indent),
        _ => {
            let _ = writeln!(output, "{}{}", " ".repeat(indent), human_scalar(value));
        }
    }
}

fn render_human(value: &Value) -> String {
    let mut rendered = String::new();
    write_human_value(&mut rendered, value, 0);
    rendered.trim_end().to_owned()
}

fn output(value: impl Serialize, mode: OutputMode) -> Result<()> {
    let value = serde_json::to_value(value)?;
    match mode {
        OutputMode::Human => println!("{}", render_human(&value)),
        OutputMode::RawJson => println!("{}", serde_json::to_string(&value)?),
        OutputMode::PrettyJson => println!("{}", serde_json::to_string_pretty(&value)?),
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

async fn authenticated(
    profiles: &ProfileStore,
    profile: &str,
) -> Result<(SessionStore, Session, OverleafApi)> {
    let store = profiles.session_store(profile)?;
    let session = store.require().with_context(|| {
        format!("profile '{profile}' is not authenticated; run: jujuleaf login --profile {profile}")
    })?;
    let api = OverleafApi::new(&session, Some(store.clone()))?;
    Ok((store, session, api))
}

fn selected_profile(
    profiles: &ProfileStore,
    explicit: Option<&str>,
    command: &Command,
) -> Result<String> {
    if let Some(explicit) = explicit {
        return profiles.resolve(Some(explicit));
    }
    let path = match command {
        Command::Pull { path } | Command::Push { path, .. } | Command::Sync { path, .. } => {
            Some(path)
        }
        _ => None,
    };
    if let Some(path) = path {
        let root = discover_root(path)?;
        return Ok(ProjectBinding::load(&root)?.profile);
    }
    profiles.resolve(None)
}

async fn read_remote_document(
    api: &mut OverleafApi,
    project_id: &str,
    path: &str,
) -> Result<Value> {
    let (mut socket, project) = connect_project_with_api(api, project_id).await?;
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

async fn root_folder(api: &mut OverleafApi, project_id: &str) -> Result<String> {
    let (socket, project) = connect_project_with_api(api, project_id).await?;
    socket.close().await.ok();
    root_folder_id(&project).ok_or_else(|| anyhow!("could not determine root folder ID"))
}

async fn edit_remote(
    command: EditorCommand,
    args: EditorArgs,
    api: &mut OverleafApi,
    pretty: OutputMode,
) -> Result<()> {
    let (mut socket, project) = connect_project_with_api(api, &args.project_id).await?;
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
    let pretty = OutputMode::from_flags(cli.raw, cli.pretty);
    let explicit_profile = cli.profile;
    match cli.command {
        Command::Login {
            cookie,
            base_url,
            preset,
        } => {
            let profiles = ProfileStore::from_default_path()?;
            let profile = profiles.resolve(explicit_profile.as_deref())?;
            let base_url = base_url.unwrap_or_else(|| preset.default_base_url().to_owned());
            let cookie = match cookie {
                Some(cookie) => cookie,
                None => {
                    eprintln!("Opening Chrome for Overleaf sign-in…");
                    interactive_login(&base_url, preset).await?
                }
            };
            let session = Session::new_with_preset(cookie, base_url, preset);
            let mut api = OverleafApi::new(&session, None)?;
            let projects = api.list_projects().await?;
            let session = Session::new_with_preset(api.cookie(), api.base_url(), preset);
            profiles.session_store(&profile)?.save(&session)?;
            profiles.set_active(&profile)?;
            output(
                json!({
                    "success": true,
                    "profile": profile,
                    "baseUrl": session.base_url,
                    "preset": preset,
                    "projectCount": projects.get("projects").and_then(Value::as_array).map(Vec::len).unwrap_or(0)
                }),
                pretty,
            )
        }
        Command::Profile { command } => {
            let profiles = ProfileStore::from_default_path()?;
            match command {
                ProfileCommand::List => output(
                    json!({
                        "active": profiles.active_name()?,
                        "profiles": profiles.list()?
                    }),
                    pretty,
                ),
                ProfileCommand::Use { name } => {
                    profiles.set_active(&name)?;
                    output(
                        json!({"success": true, "active": name, "profile": profiles.get(&name)?}),
                        pretty,
                    )
                }
                ProfileCommand::Show { name } => {
                    let name = profiles.resolve(name.as_deref().or(explicit_profile.as_deref()))?;
                    output(profiles.get(&name)?, pretty)
                }
                ProfileCommand::Delete { name } => {
                    profiles.delete(&name)?;
                    output(
                        json!({"success": true, "deleted": name, "active": profiles.active_name()?}),
                        pretty,
                    )
                }
            }
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
            let profiles = ProfileStore::from_default_path()?;
            let profile = selected_profile(&profiles, explicit_profile.as_deref(), &command)?;
            let (_store, session, mut api) = authenticated(&profiles, &profile).await?;
            dispatch_authenticated(command, session, &profile, &mut api, pretty).await
        }
    }
}

async fn dispatch_authenticated(
    command: Command,
    session: Session,
    profile: &str,
    api: &mut OverleafApi,
    pretty: OutputMode,
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
            content_only,
            meta,
        } => {
            let value = read_remote_document(api, &project_id, &path).await?;
            if content_only {
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
            let value = read_remote_document(api, &project_id, &path).await?;
            let matches = locate_text(value["content"].as_str().unwrap_or_default(), &text)?;
            output(
                json!({"path": path, "text": text, "matchCount": matches.len(), "matches": matches}),
                pretty,
            )
        }
        Command::Edit(args) => edit_remote(EditorCommand::Edit, args, api, pretty).await,
        Command::Suggest(args) => edit_remote(EditorCommand::Suggest, args, api, pretty).await,
        Command::Insert(args) => edit_remote(EditorCommand::Insert, args, api, pretty).await,
        Command::Delete(args) => edit_remote(EditorCommand::Delete, args, api, pretty).await,
        Command::Replace(args) => edit_remote(EditorCommand::Replace, args, api, pretty).await,
        Command::ApplyChanges(args) => {
            edit_remote(EditorCommand::ApplyChanges, args, api, pretty).await
        }
        Command::ApplyOps(args) => edit_remote(EditorCommand::ApplyOps, args, api, pretty).await,
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
                None => root_folder(api, &project_id).await?,
            };
            output(
                api.create_doc(&project_id, &name, Some(&parent)).await?,
                pretty,
            )
        }
        Command::DeleteDoc { project_id, doc_id } => {
            output(api.delete_doc(&project_id, &doc_id).await?, pretty)
        }
        Command::DeleteFile {
            project_id,
            file_id,
        } => output(api.delete_file(&project_id, &file_id).await?, pretty),
        Command::CreateFolder {
            project_id,
            name,
            parent,
        } => {
            let parent = match parent {
                Some(parent) => parent,
                None => root_folder(api, &project_id).await?,
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
                None => root_folder(api, &project_id).await?,
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
            let value = read_remote_document(api, &project_id, &path).await?;
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
        Command::Compile {
            project_id,
            draft,
            timeout,
            show_log,
            log_output,
        } => {
            ensure!(timeout > 0, "--timeout must be positive");
            let result = match tokio::time::timeout(
                Duration::from_secs(timeout),
                api.compile_detailed(&project_id, draft),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    tokio::time::timeout(Duration::from_secs(10), api.stop_compile(&project_id))
                        .await
                        .ok();
                    return Err(anyhow!(
                        "compilation did not finish within {timeout} seconds and was stopped"
                    ));
                }
            };
            let log = if has_output(&result, "output.log") {
                let bytes = api.download_compile_output(&result, "output.log").await?;
                Some(String::from_utf8_lossy(&bytes).into_owned())
            } else {
                None
            };
            if let Some(path) = log_output {
                let content = log
                    .as_deref()
                    .ok_or_else(|| anyhow!("compile result did not include output.log"))?;
                tokio::fs::write(&path, content)
                    .await
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }
            output(build_compile_report(result, log, show_log), pretty)
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
            let (mut socket, project) = connect_project_with_api(api, &project_id).await?;
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
            let (mut socket, project) = connect_project_with_api(api, &project_id).await?;
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
                    pretty,
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
            clone_project(api, &session, profile, &project_id, &destination).await?,
            pretty,
        ),
        Command::Pull { path } => {
            let root = discover_root(path)?;
            output(
                pull_project_with_api(&root, &session, api, profile).await?,
                pretty,
            )
        }
        Command::Push { path, retry } => {
            let root = discover_root(path)?;
            output(
                push_project_with_api(&root, &session, api, profile, &retry.options()?).await?,
                pretty,
            )
        }
        Command::Sync {
            path,
            retry,
            watch,
            interval,
        } => {
            let root = discover_root(path)?;
            ensure!(interval > 0, "--interval must be positive");
            let options = retry.options()?;
            let mut cycle = 1_u64;
            loop {
                let pull = pull_project_with_api(&root, &session, api, profile).await?;
                if !pull.success {
                    let value = json!({
                        "success": false,
                        "watching": watch,
                        "cycle": cycle,
                        "stopped": watch,
                        "reason": "pull conflict",
                        "pull": pull
                    });
                    return output(value, pretty);
                }
                let push = push_project_with_api(&root, &session, api, profile, &options).await?;
                let success = push.success;
                let value = json!({
                    "success": success,
                    "watching": watch,
                    "cycle": cycle,
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "pull": pull,
                    "push": push
                });
                output(value, pretty)?;
                if !watch || !success {
                    return Ok(());
                }
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        result?;
                        return output(json!({
                            "success": true,
                            "watching": false,
                            "stopped": true,
                            "reason": "interrupt",
                            "completedCycles": cycle
                        }), pretty);
                    }
                    _ = tokio::time::sleep(Duration::from_millis(interval)) => {}
                }
                cycle += 1;
            }
        }
        Command::Login { .. }
        | Command::Profile { .. }
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
    fn every_visible_subcommand_and_argument_has_help() {
        fn check(command: &clap::Command, path: &str) {
            for subcommand in command
                .get_subcommands()
                .filter(|command| !command.is_hide_set())
            {
                if subcommand.get_name() == "help" {
                    continue;
                }
                let subcommand_path = format!("{path} {}", subcommand.get_name());
                assert!(
                    subcommand.get_about().is_some(),
                    "{subcommand_path} has no help summary"
                );
                for argument in subcommand
                    .get_arguments()
                    .filter(|argument| !argument.is_hide_set())
                {
                    assert!(
                        argument.get_help().is_some(),
                        "{subcommand_path} argument '{}' has no help",
                        argument.get_id()
                    );
                }
                check(subcommand, &subcommand_path);
            }
        }

        check(&Cli::command(), "jujuleaf");
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

    #[test]
    fn profile_is_global_and_management_commands_parse() {
        let cli = Cli::try_parse_from([
            "jujuleaf",
            "login",
            "--profile",
            "company",
            "--base-url",
            "https://latex.company.test",
            "--cookie",
            "secret",
        ])
        .unwrap();
        assert_eq!(cli.profile.as_deref(), Some("company"));
        assert!(matches!(cli.command, Command::Login { .. }));

        let cli = Cli::try_parse_from(["jujuleaf", "profile", "use", "company"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Profile {
                command: ProfileCommand::Use { ref name }
            } if name == "company"
        ));

        let cli = Cli::try_parse_from([
            "jujuleaf",
            "login",
            "--profile",
            "cstcloud",
            "--preset",
            "cstcloud",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Login {
                base_url: None,
                preset: LoginPreset::Cstcloud,
                ..
            }
        ));
    }

    #[test]
    fn output_defaults_to_human_and_raw_is_machine_json() {
        let cli = Cli::try_parse_from(["jujuleaf", "projects"]).unwrap();
        assert_eq!(
            OutputMode::from_flags(cli.raw, cli.pretty),
            OutputMode::Human
        );

        let cli = Cli::try_parse_from(["jujuleaf", "projects", "--raw"]).unwrap();
        assert_eq!(
            OutputMode::from_flags(cli.raw, cli.pretty),
            OutputMode::RawJson
        );
        assert!(Cli::try_parse_from(["jujuleaf", "projects", "--raw", "--pretty"]).is_err());

        let cli =
            Cli::try_parse_from(["jujuleaf", "read", "project", "main.tex", "--content-only"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Command::Read {
                content_only: true,
                ..
            }
        ));
    }

    #[test]
    fn compile_and_foreground_sync_options_parse() {
        let cli = Cli::try_parse_from([
            "jujuleaf",
            "compile",
            "project",
            "--timeout",
            "60",
            "--show-log",
            "--log-output",
            "build.log",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Compile {
                timeout: 60,
                show_log: true,
                log_output: Some(_),
                ..
            }
        ));

        let cli =
            Cli::try_parse_from(["jujuleaf", "sync", "paper", "--watch", "--interval", "750"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Command::Sync {
                watch: true,
                interval: 750,
                ..
            }
        ));
    }

    #[test]
    fn human_renderer_formats_project_lists_as_tables() {
        let rendered = render_human(&json!({
            "projects": [
                {"_id": "p1", "accessLevel": "owner", "name": "Paper One"},
                {"_id": "p2", "accessLevel": "readWrite", "name": "Paper Two"}
            ]
        }));
        assert!(rendered.contains("PROJECTS (2)"));
        assert!(rendered.contains("ID"));
        assert!(rendered.contains("ACCESS LEVEL"));
        assert!(rendered.contains("Paper One"));
        assert!(!rendered.contains('{'));
    }

    #[test]
    fn local_sync_uses_the_bound_profile_unless_explicitly_overridden() {
        let project = tempfile::tempdir().unwrap();
        ProjectBinding {
            project_id: "p1".into(),
            base_url: "https://latex.company.test".into(),
            profile: "company".into(),
        }
        .save(project.path())
        .unwrap();
        let config = tempfile::tempdir().unwrap();
        let profiles = ProfileStore::new(config.path()).unwrap();
        let command = Command::Pull {
            path: project.path().to_owned(),
        };

        assert_eq!(
            selected_profile(&profiles, None, &command).unwrap(),
            "company"
        );
        assert_eq!(
            selected_profile(&profiles, Some("official"), &command).unwrap(),
            "official"
        );
    }
}
