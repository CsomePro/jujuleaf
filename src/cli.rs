use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Cursor, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use clap::{
    ArgGroup, Args, ColorChoice, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::api::OverleafApi;
use crate::auth::{LoginPreset, ProfileStore, Session, SessionStore, interactive_login};
use crate::bridge::{self, BridgeCommand};
use crate::compile::{build_compile_report, has_output};
use crate::doctor;
use crate::jj::JjWorkspace;
use crate::operations::{
    BuildOptions, Change, HISTORY_OT, InputChange, LEGACY_OT, TextSelector,
    apply_legacy_operations, build_document_operations, changes_for_text, locate_text,
    parse_document_snapshot, slice_utf16, utf16_len, validate_history_operations,
    visible_to_source_position,
};
use crate::output::{OutputMode, notice, output, output_workspace_log};
use crate::project::{collect_documents, connect_project_with_api, find_document, root_folder_id};
use crate::review::{
    abort_review, begin_review, ensure_sync_allowed, finish_review_with_api, review_diff_with_api,
    review_status_with_api, submit_review_with_api,
};
use crate::skill::{self, SkillCommand};
use crate::socket::UpdateOptions;
use crate::sync::{
    ConflictResolution, ProjectBinding, WorkspaceOperationLock, clone_project,
    discover_project_context, discover_root, list_conflicts, local_status, pull_project_with_api,
    push_project_with_api, resolve_conflict, show_conflict,
};

#[derive(Parser)]
#[command(
    name = "jujuleaf",
    version,
    about = "Local-first Overleaf collaboration, powered by Jujutsu",
    long_about = "JujuLeaf translates editor changes into Overleaf OT events and gives every local project a native Jujutsu history.",
    arg_required_else_help = true,
    after_long_help = "Examples:\n  jujuleaf login --preset cstcloud\n  jujuleaf projects\n  jujuleaf files PROJECT_ID\n  jujuleaf read PROJECT_ID main.tex --content-only\n  jujuleaf replace PROJECT_ID main.tex --old 'before' --new 'after'\n  jujuleaf clone PROJECT_ID ./paper\n\nInside a JujuLeaf clone or child directory, omit PROJECT_ID (for example: `jujuleaf read main.tex`). Use `--project-id` to override the detected project.\n\nRun `jujuleaf <COMMAND> --help` for command-specific arguments and examples."
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
        help = "Disable ANSI colors in human-readable output"
    )]
    no_color: bool,

    #[arg(
        long = "project-id",
        global = true,
        value_name = "PROJECT_ID",
        help = "Override the project detected from the current JujuLeaf clone"
    )]
    project: Option<String>,

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

#[derive(Debug)]
struct PreparedCliArgs {
    arguments: Vec<OsString>,
    context_profile: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum ProjectArgumentShape {
    Fixed(usize),
    Variadic(usize),
}

fn project_argument_shape(command: &str) -> Option<ProjectArgumentShape> {
    use ProjectArgumentShape::{Fixed, Variadic};

    match command {
        "files" | "compile" | "pdf" | "zip" | "threads" | "watch" | "history" | "wordcount" => {
            Some(Fixed(0))
        }
        "rename-project" | "read" | "locate" | "edit" | "suggest" | "insert" | "delete"
        | "replace" | "apply-changes" | "apply-ops" | "create-doc" | "delete-doc"
        | "delete-file" | "create-folder" | "delete-folder" | "upload" | "download" | "diff" => {
            Some(Fixed(1))
        }
        "rename" | "move" | "resolve-thread" | "reopen-thread" | "delete-thread"
        | "delete-comment" => Some(Fixed(2)),
        "accept-changes" | "comment" | "add-comment" => Some(Variadic(2)),
        "edit-comment" => Some(Variadic(3)),
        "search" => Some(Variadic(1)),
        _ => None,
    }
}

fn command_index(arguments: &[OsString]) -> Option<(usize, &str)> {
    let mut index = 1;
    while index < arguments.len() {
        let argument = arguments[index].to_str()?;
        if matches!(argument, "--profile" | "--project-id") {
            index += 2;
        } else if argument.starts_with("--profile=")
            || argument.starts_with("--project-id=")
            || argument.starts_with('-')
        {
            index += 1;
        } else {
            return Some((index, argument));
        }
    }
    None
}

fn option_takes_separate_value(argument: &str) -> bool {
    matches!(
        argument,
        "--profile"
            | "--project-id"
            | "--cookie"
            | "--base-url"
            | "--preset"
            | "--content"
            | "--text"
            | "--old"
            | "--new"
            | "--from"
            | "--to"
            | "--position"
            | "--length"
            | "--occurrence"
            | "--changes"
            | "--ops"
            | "--timeout"
            | "--retry-after"
            | "--parent"
            | "--type"
            | "--name"
            | "--output"
            | "--log-output"
            | "--at-text"
            | "--min-count"
            | "-o"
    )
}

fn positional_indices(arguments: &[OsString], command_index: usize) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut index = command_index + 1;
    let mut positional_only = false;
    while index < arguments.len() {
        let Some(argument) = arguments[index].to_str() else {
            positions.push(index);
            index += 1;
            continue;
        };
        if positional_only {
            positions.push(index);
            index += 1;
        } else if argument == "--" {
            positional_only = true;
            index += 1;
        } else if argument.starts_with('-') {
            index += if !argument.contains('=') && option_takes_separate_value(argument) {
                2
            } else {
                1
            };
        } else {
            positions.push(index);
            index += 1;
        }
    }
    positions
}

fn project_override(arguments: &[OsString]) -> Result<Option<String>> {
    let mut project = None;
    let mut index = 1;
    while index < arguments.len() {
        let Some(argument) = arguments[index].to_str() else {
            index += 1;
            continue;
        };
        if argument == "--" {
            break;
        }
        let value = if argument == "--project-id" {
            let value = arguments
                .get(index + 1)
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow!("--project-id requires a UTF-8 value"))?;
            index += 2;
            Some(value)
        } else if let Some(value) = argument.strip_prefix("--project-id=") {
            index += 1;
            Some(value)
        } else {
            index += 1;
            None
        };
        if let Some(value) = value {
            ensure!(project.is_none(), "--project-id may only be specified once");
            ensure!(!value.is_empty(), "--project-id cannot be empty");
            project = Some(value.to_owned());
        }
    }
    Ok(project)
}

fn positional_text<'a>(arguments: &'a [OsString], positions: &[usize]) -> Option<&'a str> {
    positions
        .first()
        .and_then(|index| arguments[*index].to_str())
}

fn prepare_cli_args(
    arguments: impl IntoIterator<Item = OsString>,
    current_dir: &Path,
) -> Result<PreparedCliArgs> {
    let mut arguments: Vec<_> = arguments.into_iter().collect();
    if arguments
        .iter()
        .any(|argument| matches!(argument.to_str(), Some("-h" | "--help" | "--version")))
    {
        return Ok(PreparedCliArgs {
            arguments,
            context_profile: None,
        });
    }

    let Some((command_index, command)) = command_index(&arguments) else {
        let context = discover_project_context(current_dir)?;
        let context_profile = context.as_ref().and_then(|context| context.profile.clone());
        if context.is_some() {
            arguments.push(OsString::from("__workspace-log"));
        }
        return Ok(PreparedCliArgs {
            arguments,
            context_profile,
        });
    };
    let Some(shape) = project_argument_shape(command) else {
        let context_profile = if matches!(command, "bridge" | "skill") {
            None
        } else {
            discover_project_context(current_dir)?.and_then(|context| context.profile)
        };
        return Ok(PreparedCliArgs {
            arguments,
            context_profile,
        });
    };
    let positions = positional_indices(&arguments, command_index);
    let project_override = project_override(&arguments)?;

    let needs_project = match shape {
        ProjectArgumentShape::Fixed(other_count) => positions.len() == other_count,
        ProjectArgumentShape::Variadic(minimum_other_count) => {
            positions.len() >= minimum_other_count
        }
    };
    if !needs_project {
        if let (ProjectArgumentShape::Fixed(other_count), Some(project_override)) =
            (shape, project_override.as_deref())
            && positions.len() == other_count + 1
        {
            ensure!(
                positional_text(&arguments, &positions) == Some(project_override),
                "project ID was provided both positionally and with --project-id"
            );
        }
        return Ok(PreparedCliArgs {
            arguments,
            context_profile: None,
        });
    }

    let (project_id, context_profile, from_context) = if let Some(project_id) = project_override {
        (project_id, None, false)
    } else if let Some(context) = discover_project_context(current_dir)? {
        (context.project_id, context.profile, true)
    } else {
        return Ok(PreparedCliArgs {
            arguments,
            context_profile: None,
        });
    };

    let already_positional = match shape {
        ProjectArgumentShape::Fixed(_) => false,
        ProjectArgumentShape::Variadic(minimum_other_count) => {
            positions.len() > minimum_other_count
                && positional_text(&arguments, &positions) == Some(project_id.as_str())
        }
    };
    if !already_positional {
        arguments.insert(command_index + 1, OsString::from(project_id));
    }

    Ok(PreparedCliArgs {
        arguments,
        context_profile: from_context.then_some(context_profile).flatten(),
    })
}

fn parse_cli(arguments: Vec<OsString>) -> Cli {
    let no_color = std::env::var_os("NO_COLOR").is_some()
        || arguments
            .iter()
            .any(|argument| matches!(argument.to_str(), Some("--no-color" | "--raw" | "--pretty")));
    let matches = Cli::command()
        .color(if no_color {
            ColorChoice::Never
        } else {
            ColorChoice::Auto
        })
        .get_matches_from(arguments);
    Cli::from_arg_matches(&matches).expect("clap matches the derived CLI")
}

#[derive(Subcommand)]
enum Command {
    /// Show the current workspace commit graph.
    #[command(name = "__workspace-log", hide = true)]
    WorkspaceLog,

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
    /// Inspect or remove stored authentication for a profile.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Diagnose authentication, endpoint, browser, and local workspace health.
    Doctor {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Skip the authenticated Overleaf endpoint check.
        #[arg(long)]
        offline: bool,
    },
    /// Expose a stable JSON/NDJSON protocol for external tools.
    Bridge {
        #[command(subcommand)]
        command: BridgeCommand,
    },
    /// Install, inspect, update, or remove the embedded Agent Skill.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
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
    /// Inspect and resolve saved synchronization conflicts.
    Conflict {
        #[command(subcommand)]
        command: ConflictCommand,
    },
    /// Browse, compare, and restore JujuLeaf's local Jujutsu history.
    Local {
        #[command(subcommand)]
        command: LocalCommand,
    },
    /// Interoperate with Git through the embedded Jujutsu repository.
    #[command(
        after_long_help = "JujuLeaf clones are already Git-backed. Use `remote`, `fetch`, and `push` here; create the workspace itself with `jujuleaf clone`. A separate `jj` executable is not required."
    )]
    Git {
        #[command(subcommand)]
        command: GitCommand,
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
    /// Start a described local work change from a clean synchronized baseline.
    Begin {
        /// Description attached to the new Jujutsu work change.
        #[arg(short, long)]
        message: String,
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Submit and reconcile one described work change through Overleaf review.
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
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
enum ReviewCommand {
    /// Preview the local text changes that would be submitted for review.
    Diff {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Submit the active work change as Overleaf tracked changes.
    Submit {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
        #[command(flatten)]
        retry: RetryArgs,
    },
    /// Inspect pending, foreign, and unsubmitted review changes.
    Status {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Abandon an unsubmitted review draft and restore its synchronized parent.
    Abort {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Reconcile a resolved complete or partial remote review and close it.
    Finish {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

impl ReviewCommand {
    fn path(&self) -> &Path {
        match self {
            Self::Diff { path }
            | Self::Submit { path, .. }
            | Self::Status { path }
            | Self::Abort { path }
            | Self::Finish { path } => path,
        }
    }
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Show whether a profile has locally stored credentials.
    Status,
    /// Remove the selected profile's stored credentials.
    Logout,
}

#[derive(Subcommand)]
enum ConflictCommand {
    /// List unresolved document and binary conflicts.
    List {
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Show hashes and a local-to-remote patch for one conflict.
    Show {
        /// Conflicted project path.
        conflict_path: String,
        /// Local JujuLeaf clone or a path inside it.
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Resolve one conflict with local, remote, or explicitly merged content.
    Resolve(ConflictResolveArgs),
}

impl ConflictCommand {
    fn root_path(&self) -> &Path {
        match self {
            Self::List { path } | Self::Show { path, .. } => path,
            Self::Resolve(args) => &args.path,
        }
    }
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("resolution")
        .required(true)
        .multiple(false)
        .args(["ours", "theirs", "merged"])
))]
struct ConflictResolveArgs {
    /// Conflicted project path.
    conflict_path: String,
    /// Keep the local file and use the observed remote version as its new base.
    #[arg(long)]
    ours: bool,
    /// Replace the local file with the observed remote version.
    #[arg(long)]
    theirs: bool,
    /// Use this file as manually merged content.
    #[arg(long, value_name = "FILE")]
    merged: Option<PathBuf>,
    /// Local JujuLeaf clone or a path inside it.
    #[arg(long, default_value = ".")]
    path: PathBuf,
}

impl ConflictResolveArgs {
    fn resolution(&self) -> ConflictResolution {
        if self.ours {
            ConflictResolution::Ours
        } else if self.theirs {
            ConflictResolution::Theirs
        } else {
            ConflictResolution::Merged(
                self.merged
                    .clone()
                    .expect("clap requires one conflict resolution"),
            )
        }
    }
}

#[derive(Subcommand)]
enum LocalCommand {
    /// List recent local Jujutsu operations, newest first.
    Log {
        /// Maximum number of operations to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Show the files and patch recorded by a local operation.
    Show {
        /// Operation or commit ID prefix; '@' means current.
        revision: String,
        /// Include JujuLeaf's versioned audit metadata.
        #[arg(long)]
        internal: bool,
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Snapshot and show uncheckpointed working-copy changes.
    Diff {
        /// Include JujuLeaf's versioned audit metadata.
        #[arg(long)]
        internal: bool,
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Restore files from a previous operation as a new recoverable operation.
    Restore {
        /// Operation or commit ID prefix shown by local log.
        revision: String,
        /// Local JujuLeaf clone or a path inside it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Args, Debug, Clone)]
struct GitRepositoryArgs {
    /// JujuLeaf clone or a path inside it.
    #[arg(
        short = 'R',
        long = "repository",
        visible_alias = "path",
        default_value = "."
    )]
    path: PathBuf,
}

#[derive(Subcommand)]
enum GitCommand {
    /// Show the workspace root and embedded bare Git repository.
    Root {
        #[command(flatten)]
        repository: GitRepositoryArgs,
    },
    /// List or configure Git remotes.
    Remote {
        #[command(subcommand)]
        command: GitRemoteCommand,
    },
    /// Fetch branches and tags into the embedded Jujutsu repository.
    Fetch {
        /// Git remote to fetch from.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Branch name or glob to fetch; repeat to fetch multiple branches.
        #[arg(short = 'b', long = "branch")]
        branches: Vec<String>,
        #[command(flatten)]
        repository: GitRepositoryArgs,
    },
    /// Push the current Jujutsu working-copy commit as a Git branch.
    Push {
        /// Git remote to push to.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Git branch to create or update.
        #[arg(
            short = 'b',
            long = "branch",
            visible_alias = "bookmark",
            default_value = "main"
        )]
        branch: String,
        #[command(flatten)]
        repository: GitRepositoryArgs,
    },
}

#[derive(Subcommand)]
enum GitRemoteCommand {
    /// List configured Git remotes and URLs.
    List {
        #[command(flatten)]
        repository: GitRepositoryArgs,
    },
    /// Add a Git remote.
    Add {
        /// Remote name, such as `origin`.
        name: String,
        /// Fetch URL.
        url: String,
        #[command(flatten)]
        repository: GitRepositoryArgs,
    },
    /// Remove a Git remote and its remote-tracking bookmarks.
    Remove {
        /// Remote name.
        name: String,
        #[command(flatten)]
        repository: GitRepositoryArgs,
    },
    /// Rename a Git remote.
    Rename {
        /// Existing remote name.
        old_name: String,
        /// New remote name.
        new_name: String,
        #[command(flatten)]
        repository: GitRepositoryArgs,
    },
    /// Change a Git remote URL.
    SetUrl {
        /// Remote name.
        name: String,
        /// New fetch URL.
        url: String,
        #[command(flatten)]
        repository: GitRepositoryArgs,
    },
}

impl GitCommand {
    fn path(&self) -> &Path {
        match self {
            Self::Root { repository }
            | Self::Fetch { repository, .. }
            | Self::Push { repository, .. } => &repository.path,
            Self::Remote { command } => command.path(),
        }
    }
}

impl GitRemoteCommand {
    fn path(&self) -> &Path {
        match self {
            Self::List { repository }
            | Self::Add { repository, .. }
            | Self::Remove { repository, .. }
            | Self::Rename { repository, .. }
            | Self::SetUrl { repository, .. } => &repository.path,
        }
    }
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
    context_profile: Option<&str>,
    command: &Command,
) -> Result<String> {
    if let Some(explicit) = explicit {
        return profiles.resolve(Some(explicit));
    }
    if let Some(context_profile) = context_profile {
        return profiles.resolve(Some(context_profile));
    }
    let path: Option<&Path> = match command {
        Command::Pull { path } | Command::Push { path, .. } | Command::Sync { path, .. } => {
            Some(path.as_path())
        }
        Command::Review { command } => Some(command.path()),
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

async fn dispatch_git(command: GitCommand, mode: OutputMode) -> Result<()> {
    let root = discover_root(command.path())?;
    let _lock = WorkspaceOperationLock::acquire(&root, "git")?;
    match command {
        GitCommand::Root { .. } => {
            let workspace = JjWorkspace::open(root).await?;
            output(workspace.git_root()?, mode)
        }
        GitCommand::Remote {
            command: GitRemoteCommand::List { .. },
        } => {
            let workspace = JjWorkspace::open(root).await?;
            output(workspace.git_remotes()?, mode)
        }
        GitCommand::Remote {
            command: GitRemoteCommand::Add { name, url, .. },
        } => {
            ensure_sync_allowed(&root, "git remote add")?;
            let mut workspace = JjWorkspace::open(root).await?;
            output(workspace.git_remote_add(&name, &url).await?, mode)
        }
        GitCommand::Remote {
            command: GitRemoteCommand::Remove { name, .. },
        } => {
            ensure_sync_allowed(&root, "git remote remove")?;
            let mut workspace = JjWorkspace::open(root).await?;
            output(workspace.git_remote_remove(&name).await?, mode)
        }
        GitCommand::Remote {
            command: GitRemoteCommand::Rename {
                old_name, new_name, ..
            },
        } => {
            ensure_sync_allowed(&root, "git remote rename")?;
            let mut workspace = JjWorkspace::open(root).await?;
            output(
                workspace.git_remote_rename(&old_name, &new_name).await?,
                mode,
            )
        }
        GitCommand::Remote {
            command: GitRemoteCommand::SetUrl { name, url, .. },
        } => {
            ensure_sync_allowed(&root, "git remote set-url")?;
            let mut workspace = JjWorkspace::open(root).await?;
            output(workspace.git_remote_set_url(&name, &url).await?, mode)
        }
        GitCommand::Fetch {
            remote, branches, ..
        } => {
            ensure_sync_allowed(&root, "git fetch")?;
            let mut workspace = JjWorkspace::open(root).await?;
            output(workspace.git_fetch(&remote, &branches).await?, mode)
        }
        GitCommand::Push { remote, branch, .. } => {
            ensure_sync_allowed(&root, "git push")?;
            let mut workspace = JjWorkspace::open(root).await?;
            output(workspace.git_push(&remote, &branch).await?, mode)
        }
    }
}

pub async fn run() -> Result<()> {
    let current_dir = std::env::current_dir()?;
    let prepared = prepare_cli_args(std::env::args_os(), &current_dir)?;
    let context_profile = prepared.context_profile;
    let cli = parse_cli(prepared.arguments);
    let bridge_raw = cli.raw;
    let bridge_pretty = cli.pretty;
    let pretty = OutputMode::from_flags(cli.raw, cli.pretty, cli.no_color);
    let explicit_profile = cli.profile;
    let project_override = cli.project;
    match cli.command {
        Command::Skill { command } => skill::run(command, &current_dir, pretty),
        Command::Bridge { command } => {
            bridge::run(
                command,
                explicit_profile,
                project_override,
                &current_dir,
                bridge_raw,
                bridge_pretty,
            )
            .await
        }
        Command::WorkspaceLog => {
            let root = discover_root(".")?;
            let _lock = WorkspaceOperationLock::acquire(&root, "workspace log")?;
            let mut workspace = JjWorkspace::open(root).await?;
            let summary = workspace.workspace_log(10).await?;
            output_workspace_log(&summary, pretty)
        }
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
                    notice("Opening Chrome for Overleaf sign-in…", pretty);
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
        Command::Auth { command } => {
            let profiles = ProfileStore::from_default_path()?;
            let profile =
                profiles.resolve(explicit_profile.as_deref().or(context_profile.as_deref()))?;
            match command {
                AuthCommand::Status => {
                    let session = profiles.session_store(&profile)?.load()?;
                    match session {
                        Some(session) if !session.cookie.is_empty() => output(
                            json!({
                                "authenticated": true,
                                "profile": profile,
                                "baseUrl": session.base_url,
                                "preset": session.login_preset,
                                "createdAtMs": session.created_at_ms,
                                "updatedAtMs": session.updated_at_ms
                            }),
                            pretty,
                        ),
                        _ => output(
                            json!({
                                "authenticated": false,
                                "profile": profile,
                                "hint": format!("run: jujuleaf login --profile {profile}")
                            }),
                            pretty,
                        ),
                    }
                }
                AuthCommand::Logout => {
                    profiles.delete(&profile)?;
                    let remaining = profiles.list()?;
                    output(
                        json!({
                            "success": true,
                            "loggedOut": profile,
                            "remainingProfiles": remaining
                        }),
                        pretty,
                    )
                }
            }
        }
        Command::Doctor { path, offline } => {
            let profiles = ProfileStore::from_default_path()?;
            output(
                doctor::inspect(&profiles, explicit_profile.as_deref(), &path, !offline).await?,
                pretty,
            )
        }
        Command::Conflict { command } => {
            let root = discover_root(command.root_path())?;
            match command {
                ConflictCommand::List { .. } => output(list_conflicts(&root)?, pretty),
                ConflictCommand::Show { conflict_path, .. } => {
                    output(show_conflict(&root, &conflict_path)?, pretty)
                }
                ConflictCommand::Resolve(args) => {
                    let _lock = WorkspaceOperationLock::acquire(&root, "conflict resolve")?;
                    ensure_sync_allowed(&root, "conflict resolve")?;
                    output(
                        resolve_conflict(&root, &args.conflict_path, args.resolution()).await?,
                        pretty,
                    )
                }
            }
        }
        Command::Local { command } => match command {
            LocalCommand::Log { limit, path } => {
                let root = discover_root(path)?;
                let _lock = WorkspaceOperationLock::acquire(&root, "local log")?;
                let workspace = JjWorkspace::open(root).await?;
                output(workspace.history(limit).await?, pretty)
            }
            LocalCommand::Show {
                revision,
                internal,
                path,
            } => {
                let root = discover_root(path)?;
                let _lock = WorkspaceOperationLock::acquire(&root, "local show")?;
                let workspace = JjWorkspace::open(root).await?;
                output(workspace.show(&revision, internal).await?, pretty)
            }
            LocalCommand::Diff { internal, path } => {
                let root = discover_root(path)?;
                let _lock = WorkspaceOperationLock::acquire(&root, "local diff")?;
                let mut workspace = JjWorkspace::open(root).await?;
                output(workspace.working_diff(internal).await?, pretty)
            }
            LocalCommand::Restore { revision, path } => {
                let root = discover_root(path)?;
                let _lock = WorkspaceOperationLock::acquire(&root, "local restore")?;
                ensure_sync_allowed(&root, "local restore")?;
                let mut workspace = JjWorkspace::open(root).await?;
                output(workspace.restore(&revision).await?, pretty)
            }
        },
        Command::Git { command } => dispatch_git(command, pretty).await,
        Command::Begin { message, path } => {
            let root = discover_root(path)?;
            let _lock = WorkspaceOperationLock::acquire(&root, "begin")?;
            output(begin_review(&root, &message).await?, pretty)
        }
        Command::Review {
            command: ReviewCommand::Abort { path },
        } => {
            let root = discover_root(path)?;
            let _lock = WorkspaceOperationLock::acquire(&root, "review abort")?;
            output(abort_review(&root).await?, pretty)
        }
        Command::Status { path } => {
            let root = discover_root(path)?;
            output(local_status(&root).await?, pretty)
        }
        Command::Checkpoint { message, path } => {
            let root = discover_root(path)?;
            let _lock = WorkspaceOperationLock::acquire(&root, "checkpoint")?;
            let mut workspace = JjWorkspace::open(root).await?;
            output(workspace.checkpoint(&message).await?, pretty)
        }
        Command::Undo { path } => {
            let root = discover_root(path)?;
            let _lock = WorkspaceOperationLock::acquire(&root, "undo")?;
            ensure_sync_allowed(&root, "undo")?;
            let mut workspace = JjWorkspace::open(root).await?;
            output(workspace.undo().await?, pretty)
        }
        Command::Redo { path } => {
            let root = discover_root(path)?;
            let _lock = WorkspaceOperationLock::acquire(&root, "redo")?;
            ensure_sync_allowed(&root, "redo")?;
            let mut workspace = JjWorkspace::open(root).await?;
            output(workspace.redo().await?, pretty)
        }
        command => {
            let profiles = ProfileStore::from_default_path()?;
            let profile = selected_profile(
                &profiles,
                explicit_profile.as_deref(),
                context_profile.as_deref(),
                &command,
            )?;
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
            notice(
                &format!(
                    "Watching {} documents. Press Ctrl+C to stop.",
                    documents.len()
                ),
                pretty,
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
        Command::Review { command } => {
            let root = discover_root(command.path())?;
            let _lock = WorkspaceOperationLock::acquire(&root, "review")?;
            match command {
                ReviewCommand::Diff { .. } => output(
                    review_diff_with_api(&root, &session, profile, api).await?,
                    pretty,
                ),
                ReviewCommand::Submit { retry, .. } => output(
                    submit_review_with_api(&root, &session, profile, api, &retry.options()?)
                        .await?,
                    pretty,
                ),
                ReviewCommand::Status { .. } => output(
                    review_status_with_api(&root, &session, profile, api).await?,
                    pretty,
                ),
                ReviewCommand::Abort { .. } => unreachable!(),
                ReviewCommand::Finish { .. } => output(
                    finish_review_with_api(&root, &session, profile, api).await?,
                    pretty,
                ),
            }
        }
        Command::Pull { path } => {
            let root = discover_root(path)?;
            let _lock = WorkspaceOperationLock::acquire(&root, "pull")?;
            ensure_sync_allowed(&root, "pull")?;
            output(
                pull_project_with_api(&root, &session, api, profile).await?,
                pretty,
            )
        }
        Command::Push { path, retry } => {
            let root = discover_root(path)?;
            let _lock = WorkspaceOperationLock::acquire(&root, "push")?;
            ensure_sync_allowed(&root, "push")?;
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
            let _lock = WorkspaceOperationLock::acquire(&root, "sync")?;
            ensure_sync_allowed(&root, "sync")?;
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
        Command::WorkspaceLog
        | Command::Login { .. }
        | Command::Profile { .. }
        | Command::Auth { .. }
        | Command::Doctor { .. }
        | Command::Bridge { .. }
        | Command::Skill { .. }
        | Command::Conflict { .. }
        | Command::Local { .. }
        | Command::Git { .. }
        | Command::Begin { .. }
        | Command::Status { .. }
        | Command::Checkpoint { .. }
        | Command::Undo { .. }
        | Command::Redo { .. } => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::BridgeCommentsCommand;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_remote_project_command_supports_context_discovery() {
        for command in Cli::command().get_subcommands() {
            let requires_project_id = command
                .get_arguments()
                .any(|argument| argument.get_id() == "project_id");
            if requires_project_id && command.get_name() != "clone" {
                assert!(
                    project_argument_shape(command.get_name()).is_some(),
                    "{} is missing project context support",
                    command.get_name()
                );
            }
        }
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
    fn diagnostics_conflicts_and_local_history_commands_parse() {
        let cli = Cli::try_parse_from(["jujuleaf", "auth", "status", "--profile", "work"]).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("work"));
        assert!(matches!(
            cli.command,
            Command::Auth {
                command: AuthCommand::Status
            }
        ));

        let cli = Cli::try_parse_from(["jujuleaf", "doctor", "--offline", "./paper"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Doctor {
                offline: true,
                ref path
            } if path == Path::new("./paper")
        ));

        let cli = Cli::try_parse_from([
            "jujuleaf", "conflict", "resolve", "main.tex", "--ours", "--raw",
        ])
        .unwrap();
        assert!(cli.raw);
        assert!(matches!(
            cli.command,
            Command::Conflict {
                command: ConflictCommand::Resolve(ConflictResolveArgs { ours: true, .. })
            }
        ));
        assert!(Cli::try_parse_from(["jujuleaf", "conflict", "resolve", "main.tex"]).is_err());
        assert!(
            Cli::try_parse_from([
                "jujuleaf", "conflict", "resolve", "main.tex", "--ours", "--theirs"
            ])
            .is_err()
        );

        let cli = Cli::try_parse_from(["jujuleaf", "local", "show", "@", "--internal"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Local {
                command: LocalCommand::Show { internal: true, .. }
            }
        ));
    }

    #[test]
    fn git_commands_parse_without_overleaf_credentials() {
        let cli = Cli::try_parse_from([
            "jujuleaf",
            "git",
            "remote",
            "add",
            "origin",
            "https://example.test/paper.git",
            "--repository",
            "./paper",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Git {
                command: GitCommand::Remote {
                    command: GitRemoteCommand::Add {
                        ref name,
                        ref repository,
                        ..
                    }
                }
            } if name == "origin" && repository.path == Path::new("./paper")
        ));
        assert!(Cli::try_parse_from(["jujuleaf", "git", "push", "--branch", "paper"]).is_ok());
    }

    #[test]
    fn output_defaults_to_human_and_raw_is_machine_json() {
        let cli = Cli::try_parse_from(["jujuleaf", "projects"]).unwrap();
        assert!(matches!(
            OutputMode::from_flags(cli.raw, cli.pretty, cli.no_color),
            OutputMode::Human { color: false }
        ));

        let cli = Cli::try_parse_from(["jujuleaf", "projects", "--raw"]).unwrap();
        assert_eq!(
            OutputMode::from_flags(cli.raw, cli.pretty, cli.no_color),
            OutputMode::RawJson
        );
        assert!(Cli::try_parse_from(["jujuleaf", "projects", "--raw", "--pretty"]).is_err());

        let cli = Cli::try_parse_from(["jujuleaf", "projects", "--no-color"]).unwrap();
        assert!(matches!(
            OutputMode::from_flags(cli.raw, cli.pretty, cli.no_color),
            OutputMode::Human { color: false }
        ));

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

    fn parse_in_project(arguments: &[&str], current_dir: &Path) -> (Cli, Option<String>) {
        let prepared = prepare_cli_args(
            arguments.iter().map(OsString::from).collect::<Vec<_>>(),
            current_dir,
        )
        .unwrap();
        let profile = prepared.context_profile.clone();
        (Cli::try_parse_from(prepared.arguments).unwrap(), profile)
    }

    #[test]
    fn project_commands_infer_the_bound_project_from_parent_directories() {
        let project = tempfile::tempdir().unwrap();
        ProjectBinding {
            project_id: "project-one".into(),
            base_url: "https://example.test".into(),
            profile: "work".into(),
        }
        .save(project.path())
        .unwrap();
        let nested = project.path().join("chapters/intro");
        std::fs::create_dir_all(&nested).unwrap();

        let (cli, profile) = parse_in_project(&["jujuleaf", "auth", "status"], &nested);
        assert_eq!(profile.as_deref(), Some("work"));
        assert!(matches!(
            cli.command,
            Command::Auth {
                command: AuthCommand::Status
            }
        ));

        let (cli, profile) =
            parse_in_project(&["jujuleaf", "read", "main.tex", "--content-only"], &nested);
        assert_eq!(profile.as_deref(), Some("work"));
        assert!(matches!(
            cli.command,
            Command::Read {
                ref project_id,
                ref path,
                content_only: true,
                ..
            } if project_id == "project-one" && path == "main.tex"
        ));

        let (cli, _) = parse_in_project(
            &["jujuleaf", "compile", "--timeout", "60", "--show-log"],
            &nested,
        );
        assert!(matches!(
            cli.command,
            Command::Compile {
                ref project_id,
                timeout: 60,
                show_log: true,
                ..
            } if project_id == "project-one"
        ));

        let (cli, _) = parse_in_project(&["jujuleaf", "search", "two", "words"], &nested);
        assert!(matches!(
            cli.command,
            Command::Search {
                ref project_id,
                ref query,
            } if project_id == "project-one" && query == &["two", "words"]
        ));

        let (cli, _) = parse_in_project(
            &[
                "jujuleaf",
                "accept-changes",
                "document-id",
                "change-one",
                "change-two",
            ],
            &nested,
        );
        assert!(matches!(
            cli.command,
            Command::AcceptChanges {
                ref project_id,
                ref doc_id,
                ref change_ids,
            } if project_id == "project-one"
                && doc_id == "document-id"
                && change_ids == &["change-one", "change-two"]
        ));
    }

    #[test]
    fn explicit_project_ids_remain_supported_and_can_override_context() {
        let project = tempfile::tempdir().unwrap();
        ProjectBinding {
            project_id: "project-one".into(),
            base_url: "https://example.test".into(),
            profile: "work".into(),
        }
        .save(project.path())
        .unwrap();

        let (cli, profile) = parse_in_project(
            &["jujuleaf", "read", "project-two", "main.tex"],
            project.path(),
        );
        assert!(profile.is_none());
        assert!(matches!(
            cli.command,
            Command::Read { ref project_id, .. } if project_id == "project-two"
        ));

        let (cli, profile) = parse_in_project(
            &[
                "jujuleaf",
                "--project-id",
                "project-two",
                "search",
                "two",
                "words",
            ],
            project.path(),
        );
        assert!(profile.is_none());
        assert_eq!(cli.project.as_deref(), Some("project-two"));
        assert!(matches!(
            cli.command,
            Command::Search {
                ref project_id,
                ref query,
            } if project_id == "project-two" && query == &["two", "words"]
        ));
    }

    #[test]
    fn missing_project_id_still_errors_outside_a_clone() {
        let outside = tempfile::tempdir().unwrap();
        let prepared = prepare_cli_args(
            ["jujuleaf", "read", "main.tex"].map(OsString::from),
            outside.path(),
        )
        .unwrap();
        assert!(Cli::try_parse_from(prepared.arguments).is_err());
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
    fn begin_and_review_workflow_commands_parse() {
        let cli = Cli::try_parse_from([
            "jujuleaf",
            "begin",
            "--message",
            "rewrite introduction",
            "paper",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Begin { message, path }
                if message == "rewrite introduction" && path.as_path() == Path::new("paper")
        ));

        for subcommand in ["diff", "status", "finish", "abort"] {
            Cli::try_parse_from(["jujuleaf", "review", subcommand, "paper"]).unwrap();
        }
        let cli = Cli::try_parse_from([
            "jujuleaf",
            "review",
            "submit",
            "paper",
            "--retry-after",
            "4",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Review {
                command: ReviewCommand::Submit { .. }
            }
        ));
    }

    #[test]
    fn bridge_commands_parse_as_a_dedicated_protocol_namespace() {
        let cli = Cli::try_parse_from([
            "jujuleaf",
            "bridge",
            "comments",
            "get",
            "thread-1",
            "--protocol",
            "1",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Bridge {
                command: BridgeCommand::Comments {
                    command: BridgeCommentsCommand::Get { ref thread_id, .. }
                }
            } if thread_id == "thread-1"
        ));

        Cli::try_parse_from(["jujuleaf", "bridge", "describe"]).unwrap();
        Cli::try_parse_from(["jujuleaf", "bridge", "comments", "list"]).unwrap();
        Cli::try_parse_from(["jujuleaf", "bridge", "comments", "watch"]).unwrap();
    }

    #[test]
    fn skill_commands_support_interactive_and_direct_installation() {
        Cli::try_parse_from(["jujuleaf", "skill", "install"]).unwrap();
        let cli = Cli::try_parse_from([
            "jujuleaf",
            "skill",
            "install",
            "--agent",
            "codex,kimi-code",
            "--scope",
            "project",
            "--yes",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Skill {
                command: SkillCommand::Install(_)
            }
        ));

        Cli::try_parse_from(["jujuleaf", "skill", "detect"]).unwrap();
        Cli::try_parse_from(["jujuleaf", "skill", "status", "--all"]).unwrap();
        Cli::try_parse_from(["jujuleaf", "skill", "update", "--all", "--yes"]).unwrap();
        Cli::try_parse_from(["jujuleaf", "skill", "uninstall", "--all", "--yes"]).unwrap();
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
            selected_profile(&profiles, None, None, &command).unwrap(),
            "company"
        );
        assert_eq!(
            selected_profile(&profiles, Some("official"), None, &command).unwrap(),
            "official"
        );
        assert_eq!(
            selected_profile(&profiles, None, Some("company"), &Command::Projects).unwrap(),
            "company"
        );
    }

    #[test]
    fn no_command_selects_workspace_log_only_inside_a_clone() {
        let project = tempfile::tempdir().unwrap();
        ProjectBinding {
            project_id: "p1".into(),
            base_url: "https://example.test".into(),
            profile: "work".into(),
        }
        .save(project.path())
        .unwrap();
        let prepared = prepare_cli_args([OsString::from("jujuleaf")], project.path()).unwrap();
        assert_eq!(prepared.context_profile.as_deref(), Some("work"));
        let cli = Cli::try_parse_from(prepared.arguments).unwrap();
        assert!(matches!(cli.command, Command::WorkspaceLog));

        let outside = tempfile::tempdir().unwrap();
        let prepared = prepare_cli_args([OsString::from("jujuleaf")], outside.path()).unwrap();
        assert_eq!(prepared.arguments.len(), 1);
        assert!(prepared.context_profile.is_none());
        assert!(Cli::try_parse_from(prepared.arguments).is_err());
    }
}
