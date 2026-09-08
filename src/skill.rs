use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};

use anstyle::{AnsiColor, Style};
use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{SecondsFormat, Utc};
use clap::{Args, Subcommand, ValueEnum};
use dialoguer::{
    Input, MultiSelect, Select,
    console::{Key, Term},
    theme::{ColorfulTheme, SimpleTheme, Theme},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::auth::ProfileStore;
use crate::output::{OutputMode, output};

const EMBEDDED_SKILL: &str = include_str!("../skill/SKILL.md");
const EMBEDDED_MANIFEST: &str = include_str!("../skill/manifest.json");
const REGISTRY_SCHEMA_VERSION: u32 = 1;
const RENDERER_VERSION: u32 = 1;

#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// Detect supported agents and their local configuration directories.
    Detect,

    /// Validate the Skill and manifest embedded in this executable.
    Validate,

    /// Print the embedded standard SKILL.md.
    Show,

    /// Render the files for one agent, optionally writing an exact skill directory.
    Render(SkillRenderArgs),

    /// Interactively select agents and install the embedded Skill.
    Install(SkillInstallArgs),

    /// Show every installation managed by JujuLeaf.
    Status(SkillSelectionArgs),

    /// Update managed installations from the Skill embedded in this executable.
    Update(SkillMutationArgs),

    /// Remove files previously installed and still tracked by JujuLeaf.
    Uninstall(SkillMutationArgs),
}

#[derive(Debug, Clone, Args)]
pub struct SkillRenderArgs {
    /// Agent format to render.
    #[arg(long, value_enum, default_value_t = SkillAgent::Portable)]
    agent: SkillAgent,

    /// Exact destination directory, for example /tmp/jujuleaf.
    #[arg(long, value_name = "SKILL_DIR")]
    output: Option<PathBuf>,

    /// Back up and replace conflicting generated files.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Clone, Args)]
pub struct SkillInstallArgs {
    /// Agent target. Repeat the option or use a comma-separated list.
    #[arg(
        long = "agent",
        visible_alias = "target",
        value_enum,
        value_delimiter = ','
    )]
    agents: Vec<SkillAgent>,

    /// Installation scope. Omit it to choose interactively.
    #[arg(long, value_enum, conflicts_with = "to")]
    scope: Option<SkillScope>,

    /// Custom skills root. JujuLeaf creates a jujuleaf child directory.
    #[arg(
        long,
        value_name = "SKILLS_DIR",
        conflicts_with_all = ["scope", "project_root"]
    )]
    to: Option<PathBuf>,

    /// Override the discovered project root for project-scoped installation.
    #[arg(long, value_name = "DIRECTORY", requires = "scope")]
    project_root: Option<PathBuf>,

    /// Show the installation plan without writing files or the registry.
    #[arg(long)]
    dry_run: bool,

    /// Skip the final confirmation.
    #[arg(short = 'y', long)]
    yes: bool,

    /// Back up and replace files not matching JujuLeaf's registry.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Clone, Args, Default)]
pub struct SkillSelectionArgs {
    /// Select all registered installations.
    #[arg(long, conflicts_with = "path")]
    all: bool,

    /// Select one exact installed jujuleaf skill directory.
    #[arg(long, value_name = "SKILL_DIR", conflicts_with = "all")]
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct SkillMutationArgs {
    #[command(flatten)]
    selection: SkillSelectionArgs,

    /// Show the update or removal plan without changing files.
    #[arg(long)]
    dry_run: bool,

    /// Skip the final confirmation.
    #[arg(short = 'y', long)]
    yes: bool,

    /// Back up locally modified files before replacing or removing them.
    #[arg(long)]
    force: bool,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum SkillAgent {
    /// Portable Agent Skills location shared by compatible agents.
    Portable,
    Codex,
    ClaudeCode,
    KimiCode,
    Pi,
    GeminiCli,
    GithubCopilot,
    Cursor,
    Opencode,
}

impl SkillAgent {
    const ALL: [Self; 9] = [
        Self::Portable,
        Self::Codex,
        Self::ClaudeCode,
        Self::KimiCode,
        Self::Pi,
        Self::GeminiCli,
        Self::GithubCopilot,
        Self::Cursor,
        Self::Opencode,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Portable => "portable",
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::KimiCode => "kimi-code",
            Self::Pi => "pi",
            Self::GeminiCli => "gemini-cli",
            Self::GithubCopilot => "github-copilot",
            Self::Cursor => "cursor",
            Self::Opencode => "opencode",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Portable => "Portable / .agents",
            Self::Codex => "Codex",
            Self::ClaudeCode => "Claude Code",
            Self::KimiCode => "Kimi Code CLI",
            Self::Pi => "Pi",
            Self::GeminiCli => "Gemini CLI",
            Self::GithubCopilot => "GitHub Copilot",
            Self::Cursor => "Cursor",
            Self::Opencode => "OpenCode",
        }
    }

    fn executable_names(self) -> &'static [&'static str] {
        match self {
            Self::Portable => &[],
            Self::Codex => &["codex"],
            Self::ClaudeCode => &["claude"],
            Self::KimiCode => &["kimi"],
            Self::Pi => &["pi"],
            Self::GeminiCli => &["gemini"],
            Self::GithubCopilot => &["copilot"],
            Self::Cursor => &["cursor"],
            Self::Opencode => &["opencode"],
        }
    }

    fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
        Self::ALL
            .into_iter()
            .find(|agent| agent.as_str() == normalized)
    }
}

impl fmt::Display for SkillAgent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SkillScope {
    /// Use the nearest project root and the agent's native project directory.
    Project,
    /// Use the current directory and the agent's native project directory.
    Current,
    /// Use the agent's native user-wide directory.
    User,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillManifest {
    schema: String,
    schema_version: u32,
    skill_version: String,
    minimum_jujuleaf_version: String,
    entrypoint: String,
    invocation: InvocationManifest,
    interface: InterfaceManifest,
    targets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InvocationManifest {
    implicit: bool,
    user_invocable: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InterfaceManifest {
    display_name: String,
    short_description: String,
    default_prompt: String,
}

#[derive(Debug, Clone)]
struct RenderedFile {
    relative_path: PathBuf,
    content: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registry {
    schema_version: u32,
    installations: Vec<Installation>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            installations: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Installation {
    target_dir: PathBuf,
    scope: InstalledScope,
    agents: Vec<SkillAgent>,
    skill_version: String,
    source_digest: String,
    renderer_version: u32,
    files: Vec<ManagedFile>,
    installed_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum InstalledScope {
    Project,
    Current,
    User,
    Custom,
}

impl InstalledScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Current => "current",
            Self::User => "user",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManagedFile {
    path: PathBuf,
    digest: String,
}

#[derive(Debug, Clone)]
struct Destination {
    target_dir: PathBuf,
    scope: InstalledScope,
    agents: Vec<SkillAgent>,
}

#[derive(Debug, Clone)]
struct PlannedInstall {
    destination: Destination,
    agents: Vec<SkillAgent>,
    artifacts: Vec<RenderedFile>,
    previous: Option<Installation>,
    action: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallationState {
    Current,
    Outdated,
    Modified,
    Missing,
}

impl InstallationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Outdated => "outdated",
            Self::Modified => "modified",
            Self::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone)]
struct AgentDetection {
    agent: SkillAgent,
    executable: Option<PathBuf>,
    project_dir: PathBuf,
    user_dir: PathBuf,
    config_markers: Vec<PathBuf>,
}

impl AgentDetection {
    fn detected(&self) -> bool {
        self.executable.is_some()
            || self.project_installed()
            || self.user_installed()
            || self.config_markers.iter().any(|path| path.exists())
    }

    fn project_installed(&self) -> bool {
        self.project_dir.join("jujuleaf/SKILL.md").is_file()
    }

    fn user_installed(&self) -> bool {
        self.user_dir.join("jujuleaf/SKILL.md").is_file()
    }

    fn reason(&self) -> String {
        let mut reasons = Vec::new();
        if self.project_installed() {
            reasons.push(format!(
                "Skill {}",
                self.project_dir.join("jujuleaf").display()
            ));
        }
        if self.user_installed() {
            reasons.push(format!(
                "Skill {}",
                self.user_dir.join("jujuleaf").display()
            ));
        }
        if let Some(executable) = &self.executable {
            reasons.push(format!("command {}", executable.display()));
        }
        for marker in &self.config_markers {
            if marker.exists() {
                reasons.push(format!("config {}", marker.display()));
            }
        }
        if reasons.is_empty() {
            "not detected".to_owned()
        } else {
            reasons.join(", ")
        }
    }

    fn menu_summary(&self) -> String {
        let mut signals = Vec::new();
        if self.project_installed() {
            signals.push("Skill installed in project");
        }
        if self.user_installed() {
            signals.push("Skill installed for user");
        }
        if self.executable.is_some() {
            signals.push("command found");
        }
        if self.config_markers.iter().any(|path| path.exists()) {
            signals.push("config found");
        }
        if signals.is_empty() {
            "not detected".to_owned()
        } else {
            signals.join(" · ")
        }
    }
}

pub(crate) fn run(command: SkillCommand, current_dir: &Path, mode: OutputMode) -> Result<()> {
    let manifest = validate_embedded_bundle()?;
    match command {
        SkillCommand::Detect => detect_command(current_dir, mode),
        SkillCommand::Validate => output(
            json!({
                "valid": true,
                "schema": manifest.schema,
                "schemaVersion": manifest.schema_version,
                "skillVersion": manifest.skill_version,
                "minimumJujuleafVersion": manifest.minimum_jujuleaf_version,
                "entrypoint": manifest.entrypoint,
                "targets": manifest.targets,
                "embeddedBytes": EMBEDDED_SKILL.len() + EMBEDDED_MANIFEST.len(),
            }),
            mode,
        ),
        SkillCommand::Show => show_command(&manifest, mode),
        SkillCommand::Render(arguments) => render_command(arguments, &manifest, current_dir, mode),
        SkillCommand::Install(arguments) => {
            install_command(arguments, &manifest, current_dir, mode)
        }
        SkillCommand::Status(selection) => status_command(selection, &manifest, current_dir, mode),
        SkillCommand::Update(arguments) => update_command(arguments, &manifest, current_dir, mode),
        SkillCommand::Uninstall(arguments) => uninstall_command(arguments, current_dir, mode),
    }
}

fn embedded_manifest() -> Result<SkillManifest> {
    serde_json::from_str(EMBEDDED_MANIFEST).context("embedded skill manifest is invalid JSON")
}

fn validate_embedded_bundle() -> Result<SkillManifest> {
    let manifest = embedded_manifest()?;
    ensure!(
        manifest.schema == "jujuleaf.skill",
        "embedded skill manifest has unsupported schema {}",
        manifest.schema
    );
    ensure!(
        manifest.schema_version == 1,
        "embedded skill manifest has unsupported schema version {}",
        manifest.schema_version
    );
    ensure!(
        manifest.entrypoint == "SKILL.md",
        "embedded skill entrypoint must be SKILL.md"
    );
    ensure!(
        !manifest.skill_version.trim().is_empty(),
        "embedded skill version cannot be empty"
    );
    ensure!(
        !manifest.minimum_jujuleaf_version.trim().is_empty(),
        "embedded minimum JujuLeaf version cannot be empty"
    );
    ensure!(
        manifest.invocation.implicit || manifest.invocation.user_invocable,
        "embedded skill must support implicit or explicit invocation"
    );
    ensure!(
        !manifest.interface.display_name.trim().is_empty()
            && !manifest.interface.short_description.trim().is_empty()
            && !manifest.interface.default_prompt.trim().is_empty(),
        "embedded skill interface fields cannot be empty"
    );

    let (frontmatter, body) = parse_frontmatter(EMBEDDED_SKILL)?;
    let name = required_field(&frontmatter, "name")?;
    ensure!(name == "jujuleaf", "embedded skill name must be jujuleaf");
    ensure!(
        name.len() <= 64
            && !name.starts_with('-')
            && !name.ends_with('-')
            && !name.contains("--")
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "embedded skill name does not follow the Agent Skills specification"
    );
    let description = required_field(&frontmatter, "description")?;
    ensure!(
        !description.is_empty() && description.len() <= 1024,
        "embedded skill description must contain 1 to 1024 bytes"
    );
    ensure!(
        frontmatter.len() == 2,
        "embedded skill frontmatter may contain only name and description"
    );
    ensure!(
        !body.trim().is_empty(),
        "embedded skill body cannot be empty"
    );
    ensure!(
        body.lines().count() <= 500,
        "embedded SKILL.md should remain below 500 lines"
    );

    let mut targets = BTreeSet::new();
    for target in &manifest.targets {
        let agent = SkillAgent::parse(target)
            .ok_or_else(|| anyhow!("embedded skill manifest has unknown target {target}"))?;
        ensure!(
            targets.insert(agent),
            "embedded skill target {target} is duplicated"
        );
    }
    ensure!(
        targets == SkillAgent::ALL.into_iter().collect(),
        "embedded skill manifest does not list every supported target"
    );

    Ok(manifest)
}

fn parse_frontmatter(source: &str) -> Result<(BTreeMap<String, Value>, &str)> {
    let source = source
        .strip_prefix("---\n")
        .ok_or_else(|| anyhow!("embedded SKILL.md must start with YAML frontmatter"))?;
    let (header, body) = source
        .split_once("\n---\n")
        .ok_or_else(|| anyhow!("embedded SKILL.md frontmatter is not terminated"))?;
    let mut fields = BTreeMap::new();
    for line in header.lines() {
        if line.trim().is_empty() {
            continue;
        }
        ensure!(
            line == line.trim_start(),
            "embedded skill frontmatter may contain only top-level fields"
        );

        let (key, value) = split_yaml_field(line)?;
        ensure!(
            matches!(key, "name" | "description"),
            "embedded skill frontmatter contains unsupported field {key}"
        );
        ensure!(
            fields
                .insert(key.to_owned(), Value::String(unquote_yaml(value)?))
                .is_none(),
            "embedded skill frontmatter contains duplicate field {key}"
        );
    }
    Ok((fields, body))
}

fn split_yaml_field(line: &str) -> Result<(&str, &str)> {
    let (key, value) = line
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid embedded YAML frontmatter line: {line}"))?;
    ensure!(
        !key.trim().is_empty(),
        "YAML frontmatter key cannot be empty"
    );
    Ok((key.trim(), value.trim()))
}

fn unquote_yaml(value: &str) -> Result<String> {
    if value.starts_with('"') {
        return serde_json::from_str(value)
            .with_context(|| format!("invalid quoted YAML value {value}"));
    }
    if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        return Ok(value[1..value.len() - 1].replace("''", "'"));
    }
    Ok(value.to_owned())
}

fn required_field<'a>(fields: &'a BTreeMap<String, Value>, key: &str) -> Result<&'a str> {
    fields
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("embedded skill frontmatter is missing {key}"))
}

fn source_digest() -> String {
    let mut digest = Sha256::new();
    digest.update(EMBEDDED_MANIFEST.as_bytes());
    digest.update([0]);
    digest.update(EMBEDDED_SKILL.as_bytes());
    hex::encode(digest.finalize())
}

fn bytes_digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn render_for_agents(agents: &[SkillAgent], manifest: &SkillManifest) -> Vec<RenderedFile> {
    let mut files = vec![
        RenderedFile {
            relative_path: PathBuf::from("SKILL.md"),
            content: EMBEDDED_SKILL.as_bytes().to_vec(),
        },
        RenderedFile {
            relative_path: PathBuf::from("manifest.json"),
            content: EMBEDDED_MANIFEST.as_bytes().to_vec(),
        },
    ];
    if agents.contains(&SkillAgent::Codex) {
        let yaml = format!(
            "interface:\n  display_name: {}\n  short_description: {}\n  default_prompt: {}\npolicy:\n  allow_implicit_invocation: {}\n",
            yaml_string(&manifest.interface.display_name),
            yaml_string(&manifest.interface.short_description),
            yaml_string(&manifest.interface.default_prompt),
            manifest.invocation.implicit,
        );
        files.push(RenderedFile {
            relative_path: PathBuf::from("agents/openai.yaml"),
            content: yaml.into_bytes(),
        });
    }
    files
}

fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).expect("JSON string serialization cannot fail")
}

fn show_command(manifest: &SkillManifest, mode: OutputMode) -> Result<()> {
    match mode {
        OutputMode::Human { .. } => {
            print!("{EMBEDDED_SKILL}");
            io::stdout().flush().context("failed to write SKILL.md")
        }
        OutputMode::RawJson | OutputMode::PrettyJson => output(
            json!({
                "skillVersion": manifest.skill_version,
                "manifest": serde_json::from_str::<Value>(EMBEDDED_MANIFEST)?,
                "content": EMBEDDED_SKILL,
            }),
            mode,
        ),
    }
}

fn render_command(
    arguments: SkillRenderArgs,
    manifest: &SkillManifest,
    current_dir: &Path,
    mode: OutputMode,
) -> Result<()> {
    let files = render_for_agents(&[arguments.agent], manifest);
    let Some(destination) = arguments.output else {
        return output(
            json!({
                "agent": arguments.agent,
                "skillVersion": manifest.skill_version,
                "files": files.iter().map(|file| json!({
                    "path": path_text(&file.relative_path),
                    "content": String::from_utf8_lossy(&file.content),
                })).collect::<Vec<_>>(),
            }),
            mode,
        );
    };

    let destination = absolute_path(&destination, current_dir)?;
    preflight_unmanaged_files(&destination, &files, arguments.force)?;
    let backups = write_artifacts(&destination, &files, arguments.force)?;
    output(
        json!({
            "rendered": true,
            "agent": arguments.agent,
            "skillVersion": manifest.skill_version,
            "path": destination,
            "files": files.iter().map(|file| path_text(&file.relative_path)).collect::<Vec<_>>(),
            "backups": backups,
        }),
        mode,
    )
}

fn detect_command(current_dir: &Path, mode: OutputMode) -> Result<()> {
    let home = user_home()?;
    let project_root = discover_project_root(current_dir);
    let detections = detect_agents(&project_root, &home);
    output(
        json!({
            "projectRoot": project_root,
            "agents": detections.iter().map(|detection| json!({
                "agent": detection.agent,
                "name": detection.agent.display_name(),
                "detected": detection.detected(),
                "projectInstalled": detection.project_installed(),
                "userInstalled": detection.user_installed(),
                "executable": detection.executable,
                "projectSkillsDirectory": detection.project_dir,
                "userSkillsDirectory": detection.user_dir,
                "reason": detection.reason(),
            })).collect::<Vec<_>>(),
        }),
        mode,
    )
}

fn detect_agents(project_root: &Path, home: &Path) -> Vec<AgentDetection> {
    SkillAgent::ALL
        .into_iter()
        .map(|agent| AgentDetection {
            agent,
            executable: agent
                .executable_names()
                .iter()
                .find_map(|name| find_executable(name)),
            project_dir: native_skills_root(agent, SkillScope::Project, project_root, home),
            user_dir: native_skills_root(agent, SkillScope::User, project_root, home),
            config_markers: agent_config_markers(agent, project_root, home),
        })
        .collect()
}

fn agent_config_markers(agent: SkillAgent, project_root: &Path, home: &Path) -> Vec<PathBuf> {
    match agent {
        SkillAgent::Portable => vec![project_root.join(".agents"), home.join(".agents")],
        SkillAgent::Codex => vec![project_root.join(".codex"), home.join(".codex")],
        SkillAgent::ClaudeCode => vec![project_root.join(".claude"), home.join(".claude")],
        SkillAgent::KimiCode => vec![project_root.join(".kimi-code"), home.join(".kimi-code")],
        SkillAgent::Pi => vec![project_root.join(".pi"), home.join(".pi")],
        SkillAgent::GeminiCli => vec![project_root.join(".gemini"), home.join(".gemini")],
        SkillAgent::GithubCopilot => {
            vec![project_root.join(".github/skills"), home.join(".copilot")]
        }
        SkillAgent::Cursor => vec![project_root.join(".cursor"), home.join(".cursor")],
        SkillAgent::Opencode => vec![
            project_root.join(".opencode"),
            home.join(".config/opencode"),
        ],
    }
}

fn find_executable(name: &str) -> Option<PathBuf> {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .flat_map(|directory| {
            let plain = directory.join(name);
            #[cfg(windows)]
            let candidates = vec![
                plain.clone(),
                plain.with_extension("exe"),
                plain.with_extension("cmd"),
            ];
            #[cfg(not(windows))]
            let candidates = vec![plain];
            candidates
        })
        .find(|candidate| candidate.is_file())
}

fn discover_project_root(current_dir: &Path) -> PathBuf {
    current_dir
        .ancestors()
        .find(|directory| {
            [".git", ".jj", ".jujuleaf"]
                .iter()
                .any(|marker| directory.join(marker).exists())
        })
        .unwrap_or(current_dir)
        .to_owned()
}

fn user_home() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_owned())
        .ok_or_else(|| anyhow!("could not determine the user home directory"))
}

fn native_skills_root(agent: SkillAgent, scope: SkillScope, base: &Path, home: &Path) -> PathBuf {
    let user = scope == SkillScope::User;
    match agent {
        SkillAgent::Portable | SkillAgent::Codex => {
            if user {
                home.join(".agents/skills")
            } else {
                base.join(".agents/skills")
            }
        }
        SkillAgent::ClaudeCode => {
            if user {
                home.join(".claude/skills")
            } else {
                base.join(".claude/skills")
            }
        }
        SkillAgent::KimiCode => {
            if user {
                home.join(".kimi-code/skills")
            } else {
                base.join(".kimi-code/skills")
            }
        }
        SkillAgent::Pi => {
            if user {
                home.join(".pi/agent/skills")
            } else {
                base.join(".pi/skills")
            }
        }
        SkillAgent::GeminiCli => {
            if user {
                home.join(".gemini/skills")
            } else {
                base.join(".gemini/skills")
            }
        }
        SkillAgent::GithubCopilot => {
            if user {
                home.join(".copilot/skills")
            } else {
                base.join(".github/skills")
            }
        }
        SkillAgent::Cursor => {
            if user {
                home.join(".cursor/skills")
            } else {
                base.join(".cursor/skills")
            }
        }
        SkillAgent::Opencode => {
            if user {
                home.join(".config/opencode/skills")
            } else {
                base.join(".opencode/skills")
            }
        }
    }
}

fn install_command(
    arguments: SkillInstallArgs,
    manifest: &SkillManifest,
    current_dir: &Path,
    mode: OutputMode,
) -> Result<()> {
    let interactive = interactive_terminal(mode);
    if !interactive && arguments.agents.is_empty() {
        bail!("non-interactive installation requires at least one --agent");
    }
    if !interactive && arguments.scope.is_none() && arguments.to.is_none() {
        bail!("non-interactive installation requires --scope or --to");
    }
    require_noninteractive_confirmation(mode, arguments.yes, arguments.dry_run)?;

    let home = user_home()?;
    let project_root = arguments
        .project_root
        .as_deref()
        .map(|path| absolute_path(path, current_dir))
        .transpose()?
        .unwrap_or_else(|| discover_project_root(current_dir));
    if arguments.project_root.is_some() && arguments.scope != Some(SkillScope::Project) {
        bail!("--project-root requires --scope project");
    }

    let (scope, custom_root) = if let Some(path) = arguments.to {
        (
            InstalledScope::Custom,
            Some(absolute_path(&path, current_dir)?),
        )
    } else {
        let selected_scope = match arguments.scope.map(ScopeChoice::Native) {
            Some(scope) => scope,
            None => prompt_scope(&project_root, current_dir, &home, mode)?,
        };
        match selected_scope {
            ScopeChoice::Native(SkillScope::Project) => (InstalledScope::Project, None),
            ScopeChoice::Native(SkillScope::Current) => (InstalledScope::Current, None),
            ScopeChoice::Native(SkillScope::User) => (InstalledScope::User, None),
            ScopeChoice::Custom => (InstalledScope::Custom, None),
        }
    };

    let custom_root = if scope == InstalledScope::Custom && custom_root.is_none() {
        ensure!(
            interactive,
            "non-interactive custom installation requires --to"
        );
        Some(prompt_custom_root(current_dir, mode)?)
    } else {
        custom_root
    };

    let detections = detect_agents(&project_root, &home);
    let agents = if arguments.agents.is_empty() {
        prompt_agents(
            &detections,
            scope,
            custom_root.as_deref(),
            &project_root,
            current_dir,
            &home,
            mode,
        )?
    } else {
        sorted_agents(arguments.agents)
    };
    ensure!(!agents.is_empty(), "select at least one agent");

    let destinations = resolve_destinations(
        &agents,
        scope,
        custom_root.as_deref(),
        &project_root,
        current_dir,
        &home,
    )?;

    let registry_path = registry_path()?;
    let mut registry = load_registry(&registry_path)?;
    let plans = plan_installations(destinations, &registry, manifest, arguments.force)?;

    print_install_plan(&plans, mode);
    let changes = plans
        .iter()
        .filter(|plan| plan.action != "unchanged")
        .count();
    if changes == 0 || arguments.dry_run {
        return output(
            install_summary(
                &plans,
                manifest,
                &registry_path,
                arguments.dry_run,
                Vec::new(),
            ),
            mode,
        );
    }
    confirm_mutation(
        "Install the JujuLeaf Skill at these paths?",
        arguments.yes,
        mode,
    )?;

    let mut backups = Vec::new();
    for plan in &plans {
        if plan.action == "unchanged" {
            continue;
        }
        backups.extend(write_artifacts(
            &plan.destination.target_dir,
            &plan.artifacts,
            arguments.force,
        )?);
        if let Some(previous) = &plan.previous {
            remove_stale_managed_files(previous, &plan.artifacts)?;
        }
        upsert_installation(&mut registry, installation_from_plan(plan, manifest));
        save_registry(&registry_path, &registry)?;
    }

    output(
        install_summary(&plans, manifest, &registry_path, false, backups),
        mode,
    )
}

fn resolve_destinations(
    agents: &[SkillAgent],
    scope: InstalledScope,
    custom_root: Option<&Path>,
    project_root: &Path,
    current_dir: &Path,
    home: &Path,
) -> Result<Vec<Destination>> {
    let mut grouped: BTreeMap<PathBuf, BTreeSet<SkillAgent>> = BTreeMap::new();
    for agent in agents {
        let target = skill_target_dir(*agent, scope, custom_root, project_root, current_dir, home)?;
        grouped.entry(target).or_default().insert(*agent);
    }
    Ok(grouped
        .into_iter()
        .map(|(target_dir, agents)| Destination {
            target_dir,
            scope,
            agents: agents.into_iter().collect(),
        })
        .collect())
}

fn skill_target_dir(
    agent: SkillAgent,
    scope: InstalledScope,
    custom_root: Option<&Path>,
    project_root: &Path,
    current_dir: &Path,
    home: &Path,
) -> Result<PathBuf> {
    let root = match scope {
        InstalledScope::Custom => custom_root
            .ok_or_else(|| anyhow!("custom installation requires a destination"))?
            .to_owned(),
        InstalledScope::Project => {
            native_skills_root(agent, SkillScope::Project, project_root, home)
        }
        InstalledScope::Current => {
            native_skills_root(agent, SkillScope::Current, current_dir, home)
        }
        InstalledScope::User => native_skills_root(agent, SkillScope::User, home, home),
    };
    let target = root.join("jujuleaf");
    ensure_safe_skill_target(&target)?;
    Ok(target)
}

fn plan_installations(
    destinations: Vec<Destination>,
    registry: &Registry,
    manifest: &SkillManifest,
    force: bool,
) -> Result<Vec<PlannedInstall>> {
    let mut plans = Vec::new();
    for destination in destinations {
        let previous = registry
            .installations
            .iter()
            .find(|installation| installation.target_dir == destination.target_dir)
            .cloned();
        let mut combined = destination.agents.iter().copied().collect::<BTreeSet<_>>();
        if let Some(previous) = &previous {
            combined.extend(previous.agents.iter().copied());
            if installation_state(previous, manifest)? == InstallationState::Modified && !force {
                bail!(
                    "{} contains locally modified managed files; inspect status or rerun with --force to back them up",
                    destination.target_dir.display()
                );
            }
        }
        let agents = combined.into_iter().collect::<Vec<_>>();
        let artifacts = render_for_agents(&agents, manifest);
        if let Some(previous) = &previous {
            preflight_new_artifacts(previous, &artifacts, force)?;
        } else {
            preflight_unmanaged_files(&destination.target_dir, &artifacts, force)?;
        }
        let action = if artifacts_match(&destination.target_dir, &artifacts)?
            && previous.as_ref().is_some_and(|installation| {
                installation.source_digest == source_digest()
                    && installation.renderer_version == RENDERER_VERSION
                    && sorted_agents(installation.agents.clone()) == agents
            }) {
            "unchanged"
        } else if previous.is_some() {
            "updated"
        } else if artifacts_match(&destination.target_dir, &artifacts)? {
            "registered"
        } else {
            "installed"
        };
        plans.push(PlannedInstall {
            destination,
            agents,
            artifacts,
            previous,
            action,
        });
    }
    Ok(plans)
}

fn install_summary(
    plans: &[PlannedInstall],
    manifest: &SkillManifest,
    registry_path: &Path,
    dry_run: bool,
    backups: Vec<PathBuf>,
) -> Value {
    json!({
        "success": true,
        "dryRun": dry_run,
        "skillVersion": manifest.skill_version,
        "registryPath": registry_path,
        "installations": plans.iter().map(|plan| json!({
            "action": plan.action,
            "scope": plan.destination.scope.as_str(),
            "path": plan.destination.target_dir,
            "agents": plan.agents,
            "files": plan.artifacts.iter().map(|file| path_text(&file.relative_path)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "backups": backups,
    })
}

fn installation_from_plan(plan: &PlannedInstall, manifest: &SkillManifest) -> Installation {
    let timestamp = timestamp();
    Installation {
        target_dir: plan.destination.target_dir.clone(),
        scope: plan.destination.scope,
        agents: plan.agents.clone(),
        skill_version: manifest.skill_version.clone(),
        source_digest: source_digest(),
        renderer_version: RENDERER_VERSION,
        files: plan
            .artifacts
            .iter()
            .map(|file| ManagedFile {
                path: file.relative_path.clone(),
                digest: bytes_digest(&file.content),
            })
            .collect(),
        installed_at: plan
            .previous
            .as_ref()
            .map(|installation| installation.installed_at.clone())
            .unwrap_or_else(|| timestamp.clone()),
        updated_at: timestamp,
    }
}

fn upsert_installation(registry: &mut Registry, installation: Installation) {
    if let Some(existing) = registry
        .installations
        .iter_mut()
        .find(|existing| existing.target_dir == installation.target_dir)
    {
        *existing = installation;
    } else {
        registry.installations.push(installation);
        registry
            .installations
            .sort_by(|left, right| left.target_dir.cmp(&right.target_dir));
    }
}

fn status_command(
    selection: SkillSelectionArgs,
    manifest: &SkillManifest,
    current_dir: &Path,
    mode: OutputMode,
) -> Result<()> {
    let registry_path = registry_path()?;
    let registry = load_registry(&registry_path)?;
    let indices = select_installations(&registry, &selection, current_dir, mode, false)?;
    let installations = indices
        .iter()
        .map(|index| installation_json(&registry.installations[*index], manifest))
        .collect::<Result<Vec<_>>>()?;
    output(
        json!({
            "skillVersion": manifest.skill_version,
            "sourceDigest": source_digest(),
            "registryPath": registry_path,
            "installations": installations,
        }),
        mode,
    )
}

fn installation_json(installation: &Installation, manifest: &SkillManifest) -> Result<Value> {
    let state = installation_state(installation, manifest)?;
    let files = installation
        .files
        .iter()
        .map(|file| {
            let path = installation.target_dir.join(&file.path);
            let actual = file_digest(&path).ok();
            json!({
                "path": path,
                "expectedDigest": file.digest,
                "actualDigest": actual,
                "exists": path.exists(),
                "modified": actual.as_deref() != Some(file.digest.as_str()),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "status": state.as_str(),
        "path": installation.target_dir,
        "scope": installation.scope.as_str(),
        "agents": installation.agents,
        "installedSkillVersion": installation.skill_version,
        "availableSkillVersion": manifest.skill_version,
        "installedAt": installation.installed_at,
        "updatedAt": installation.updated_at,
        "files": files,
    }))
}

fn update_command(
    arguments: SkillMutationArgs,
    manifest: &SkillManifest,
    current_dir: &Path,
    mode: OutputMode,
) -> Result<()> {
    require_noninteractive_confirmation(mode, arguments.yes, arguments.dry_run)?;
    let registry_path = registry_path()?;
    let mut registry = load_registry(&registry_path)?;
    let indices = select_installations(&registry, &arguments.selection, current_dir, mode, true)?;
    let mut changes = Vec::new();
    for index in &indices {
        let installation = &registry.installations[*index];
        let state = installation_state(installation, manifest)?;
        if state == InstallationState::Modified && !arguments.force {
            bail!(
                "{} contains locally modified files; rerun with --force to back them up",
                installation.target_dir.display()
            );
        }
        let artifacts = render_for_agents(&installation.agents, manifest);
        preflight_new_artifacts(installation, &artifacts, arguments.force)?;
        if state != InstallationState::Current {
            changes.push(*index);
        }
    }

    print_mutation_plan("Update", &registry, &changes, mode);
    if changes.is_empty() || arguments.dry_run {
        return output(
            json!({
                "success": true,
                "dryRun": arguments.dry_run,
                "updated": [],
                "unchangedCount": indices.len() - changes.len(),
                "skillVersion": manifest.skill_version,
            }),
            mode,
        );
    }
    confirm_mutation(
        "Update these JujuLeaf Skill installations?",
        arguments.yes,
        mode,
    )?;

    let mut updated = Vec::new();
    let mut backups = Vec::new();
    for index in changes {
        let previous = registry.installations[index].clone();
        let agents = sorted_agents(previous.agents.clone());
        let artifacts = render_for_agents(&agents, manifest);
        backups.extend(write_artifacts(
            &previous.target_dir,
            &artifacts,
            arguments.force,
        )?);
        remove_stale_managed_files(&previous, &artifacts)?;
        let plan = PlannedInstall {
            destination: Destination {
                target_dir: previous.target_dir.clone(),
                scope: previous.scope,
                agents: agents.clone(),
            },
            agents,
            artifacts,
            previous: Some(previous.clone()),
            action: "updated",
        };
        registry.installations[index] = installation_from_plan(&plan, manifest);
        save_registry(&registry_path, &registry)?;
        updated.push(previous.target_dir);
    }
    output(
        json!({
            "success": true,
            "dryRun": false,
            "updated": updated,
            "backups": backups,
            "skillVersion": manifest.skill_version,
        }),
        mode,
    )
}

fn uninstall_command(
    arguments: SkillMutationArgs,
    current_dir: &Path,
    mode: OutputMode,
) -> Result<()> {
    require_noninteractive_confirmation(mode, arguments.yes, arguments.dry_run)?;
    let registry_path = registry_path()?;
    let mut registry = load_registry(&registry_path)?;
    let placeholder_manifest = embedded_manifest()?;
    let indices = select_installations(&registry, &arguments.selection, current_dir, mode, true)?;
    for index in &indices {
        let installation = &registry.installations[*index];
        if installation_state(installation, &placeholder_manifest)? == InstallationState::Modified
            && !arguments.force
        {
            bail!(
                "{} contains locally modified files; rerun with --force to back them up",
                installation.target_dir.display()
            );
        }
    }

    print_mutation_plan("Remove", &registry, &indices, mode);
    if indices.is_empty() || arguments.dry_run {
        return output(
            json!({
                "success": true,
                "dryRun": arguments.dry_run,
                "removed": [],
            }),
            mode,
        );
    }
    confirm_mutation(
        "Remove these managed JujuLeaf Skill files?",
        arguments.yes,
        mode,
    )?;

    let mut removed = Vec::new();
    let mut backups = Vec::new();
    let mut indices = indices;
    indices.sort_unstable_by(|left, right| right.cmp(left));
    for index in indices {
        let installation = registry.installations[index].clone();
        backups.extend(remove_installation_files(&installation, arguments.force)?);
        removed.push(installation.target_dir);
        registry.installations.remove(index);
        save_registry(&registry_path, &registry)?;
    }
    removed.sort();
    output(
        json!({
            "success": true,
            "dryRun": false,
            "removed": removed,
            "backups": backups,
        }),
        mode,
    )
}

fn select_installations(
    registry: &Registry,
    selection: &SkillSelectionArgs,
    current_dir: &Path,
    mode: OutputMode,
    prompt_if_ambiguous: bool,
) -> Result<Vec<usize>> {
    if let Some(path) = &selection.path {
        let path = absolute_path(path, current_dir)?;
        let index = registry
            .installations
            .iter()
            .position(|installation| installation.target_dir == path)
            .ok_or_else(|| anyhow!("{} is not a registered Skill installation", path.display()))?;
        return Ok(vec![index]);
    }
    if selection.all {
        return Ok((0..registry.installations.len()).collect());
    }
    if registry.installations.len() <= 1 || !prompt_if_ambiguous {
        return Ok((0..registry.installations.len()).collect());
    }
    if !interactive_terminal(mode) {
        bail!("multiple Skill installations exist; pass --all or --path");
    }
    prompt_installations(&registry.installations, mode)
}

fn installation_state(
    installation: &Installation,
    manifest: &SkillManifest,
) -> Result<InstallationState> {
    let mut missing = false;
    let mut modified = false;
    for file in &installation.files {
        validate_relative_path(&file.path)?;
        let path = installation.target_dir.join(&file.path);
        if !path.exists() {
            missing = true;
            continue;
        }
        if file_digest(&path).ok().as_deref() != Some(file.digest.as_str()) {
            modified = true;
        }
    }
    if modified {
        return Ok(InstallationState::Modified);
    }
    if missing {
        return Ok(InstallationState::Missing);
    }

    let expected = render_for_agents(&installation.agents, manifest);
    let expected_files = expected
        .iter()
        .map(|file| (file.relative_path.clone(), bytes_digest(&file.content)))
        .collect::<BTreeMap<_, _>>();
    let recorded_files = installation
        .files
        .iter()
        .map(|file| (file.path.clone(), file.digest.clone()))
        .collect::<BTreeMap<_, _>>();
    if installation.source_digest != source_digest()
        || installation.renderer_version != RENDERER_VERSION
        || installation.skill_version != manifest.skill_version
        || expected_files != recorded_files
    {
        Ok(InstallationState::Outdated)
    } else {
        Ok(InstallationState::Current)
    }
}

fn preflight_unmanaged_files(target: &Path, files: &[RenderedFile], force: bool) -> Result<()> {
    for file in files {
        validate_relative_path(&file.relative_path)?;
        let path = target.join(&file.relative_path);
        if !path.exists() {
            continue;
        }
        let expected = bytes_digest(&file.content);
        let matches = file_digest(&path).ok().as_deref() == Some(expected.as_str());
        if !matches && !force {
            bail!(
                "{} already exists and is not managed by JujuLeaf; use --force to back it up",
                path.display()
            );
        }
    }
    Ok(())
}

fn preflight_new_artifacts(
    installation: &Installation,
    files: &[RenderedFile],
    force: bool,
) -> Result<()> {
    let managed = installation
        .files
        .iter()
        .map(|file| file.path.as_path())
        .collect::<BTreeSet<_>>();
    for file in files {
        if !managed.contains(file.relative_path.as_path()) {
            preflight_unmanaged_files(&installation.target_dir, std::slice::from_ref(file), force)?;
        }
    }
    Ok(())
}

fn artifacts_match(target: &Path, files: &[RenderedFile]) -> Result<bool> {
    for file in files {
        validate_relative_path(&file.relative_path)?;
        let expected = bytes_digest(&file.content);
        if file_digest(&target.join(&file.relative_path))
            .ok()
            .as_deref()
            != Some(expected.as_str())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn write_artifacts(target: &Path, files: &[RenderedFile], force: bool) -> Result<Vec<PathBuf>> {
    ensure_safe_skill_target(target)?;
    let mut backups = Vec::new();
    for file in files {
        validate_relative_path(&file.relative_path)?;
        let path = target.join(&file.relative_path);
        if path.exists() {
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("failed to inspect {}", path.display()))?;
            ensure!(
                !metadata.file_type().is_symlink() && metadata.is_file(),
                "refusing to replace non-regular file {}",
                path.display()
            );
            if file_digest(&path)? != bytes_digest(&file.content) && force {
                backups.push(backup_file(&path)?);
            }
        }
        atomic_write(&path, &file.content)?;
    }
    Ok(backups)
}

fn remove_stale_managed_files(previous: &Installation, new_files: &[RenderedFile]) -> Result<()> {
    let retained = new_files
        .iter()
        .map(|file| file.relative_path.as_path())
        .collect::<BTreeSet<_>>();
    for file in &previous.files {
        validate_relative_path(&file.path)?;
        if retained.contains(file.path.as_path()) {
            continue;
        }
        let path = previous.target_dir.join(&file.path);
        if path.exists() && file_digest(&path).ok().as_deref() == Some(file.digest.as_str()) {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove stale file {}", path.display()))?;
        }
    }
    Ok(())
}

fn remove_installation_files(installation: &Installation, force: bool) -> Result<Vec<PathBuf>> {
    ensure_safe_skill_target(&installation.target_dir)?;
    let mut backups = Vec::new();
    for file in &installation.files {
        validate_relative_path(&file.path)?;
        let path = installation.target_dir.join(&file.path);
        if !path.exists() {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "refusing to remove non-regular file {}",
            path.display()
        );
        let modified = file_digest(&path)? != file.digest;
        if modified {
            ensure!(
                force,
                "{} was modified; use --force to back it up before removal",
                path.display()
            );
            backups.push(backup_file(&path)?);
        }
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
        remove_empty_parents(path.parent(), &installation.target_dir)?;
    }
    if installation.target_dir.is_dir() {
        let _ = fs::remove_dir(&installation.target_dir);
    }
    Ok(backups)
}

fn remove_empty_parents(mut directory: Option<&Path>, boundary: &Path) -> Result<()> {
    while let Some(path) = directory {
        if path == boundary {
            break;
        }
        match fs::remove_dir(path) {
            Ok(()) => directory = path.parent(),
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                directory = path.parent();
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to remove empty directory {}", path.display())
                });
            }
        }
    }
    Ok(())
}

fn backup_file(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("cannot back up path without a UTF-8 file name"))?;
    let suffix = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let backup = path.with_file_name(format!("{name}.jujuleaf-backup-{suffix}"));
    fs::copy(path, &backup).with_context(|| {
        format!(
            "failed to back up {} to {}",
            path.display(),
            backup.display()
        )
    })?;
    Ok(backup)
}

fn registry_path() -> Result<PathBuf> {
    Ok(ProfileStore::default_root()?.join("skills.json"))
}

fn load_registry(path: &Path) -> Result<Registry> {
    if !path.exists() {
        return Ok(Registry::default());
    }
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read Skill registry {}", path.display()))?;
    let registry: Registry = serde_json::from_slice(&bytes)
        .with_context(|| format!("Skill registry {} is invalid", path.display()))?;
    ensure!(
        registry.schema_version == REGISTRY_SCHEMA_VERSION,
        "Skill registry {} uses unsupported schema version {}",
        path.display(),
        registry.schema_version
    );
    let mut targets = BTreeSet::new();
    for installation in &registry.installations {
        ensure_safe_skill_target(&installation.target_dir)?;
        ensure!(
            targets.insert(&installation.target_dir),
            "Skill registry {} contains duplicate target {}",
            path.display(),
            installation.target_dir.display()
        );
        for file in &installation.files {
            validate_relative_path(&file.path)?;
        }
    }
    Ok(registry)
}

fn save_registry(path: &Path, registry: &Registry) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(registry)?;
    atomic_write(path, &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} does not have a parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create a temporary file in {}", parent.display()))?;
    temporary
        .write_all(bytes)
        .with_context(|| format!("failed to write temporary file for {}", path.display()))?;
    temporary
        .flush()
        .with_context(|| format!("failed to flush temporary file for {}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to atomically replace {}", path.display()))?;
    Ok(())
}

fn file_digest(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{} is not a regular file",
        path.display()
    );
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn validate_relative_path(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "managed file path {} must be relative and cannot traverse directories",
        path.display()
    );
    Ok(())
}

fn ensure_safe_skill_target(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "managed Skill directory must be absolute: {}",
        path.display()
    );
    ensure!(
        path.file_name().and_then(|name| name.to_str()) == Some("jujuleaf"),
        "managed Skill directory must end in jujuleaf: {}",
        path.display()
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    ensure!(
        parent.parent().is_some(),
        "refusing unsafe Skill target {}",
        path.display()
    );
    Ok(())
}

fn absolute_path(path: &Path, current_dir: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        current_dir.join(path)
    };
    std::path::absolute(&path).with_context(|| format!("failed to resolve path {}", path.display()))
}

fn sorted_agents(agents: Vec<SkillAgent>) -> Vec<SkillAgent> {
    agents
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn interactive_terminal(mode: OutputMode) -> bool {
    matches!(mode, OutputMode::Human { .. })
        && io::stdin().is_terminal()
        && io::stderr().is_terminal()
}

fn require_noninteractive_confirmation(mode: OutputMode, yes: bool, dry_run: bool) -> Result<()> {
    if !interactive_terminal(mode) && !yes && !dry_run {
        bail!("non-interactive mutation requires --yes");
    }
    Ok(())
}

fn prompt_agents(
    detections: &[AgentDetection],
    scope: InstalledScope,
    custom_root: Option<&Path>,
    project_root: &Path,
    current_dir: &Path,
    home: &Path,
    mode: OutputMode,
) -> Result<Vec<SkillAgent>> {
    let mut defaults = detections
        .iter()
        .map(AgentDetection::detected)
        .collect::<Vec<_>>();
    if !defaults.iter().any(|selected| *selected) {
        defaults[0] = true;
    }
    let items = detections
        .iter()
        .map(|detection| -> Result<String> {
            let target = skill_target_dir(
                detection.agent,
                scope,
                custom_root,
                project_root,
                current_dir,
                home,
            )?;
            Ok(format!(
                "{:<21} → {}  · {}",
                detection.agent.display_name(),
                compact_path(&target, home),
                detection.menu_summary()
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    loop {
        let theme = prompt_theme(mode);
        let selected = MultiSelect::with_theme(theme.as_ref())
            .with_prompt("Agent targets  ↑↓ move · Space toggle · Enter confirm · Esc cancel")
            .items(&items)
            .defaults(&defaults)
            .interact_opt()
            .context("failed to read Agent target selection")?
            .ok_or_else(|| anyhow!("cancelled"))?;
        if !selected.is_empty() {
            return Ok(selected
                .into_iter()
                .map(|index| detections[index].agent)
                .collect());
        }
        eprintln!("Select at least one Agent target.");
    }
}

fn compact_path(path: &Path, home: &Path) -> String {
    if path == home {
        return "~".to_owned();
    }
    match path.strip_prefix(home) {
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

enum ScopeChoice {
    Native(SkillScope),
    Custom,
}

fn prompt_scope(
    project_root: &Path,
    current_dir: &Path,
    home: &Path,
    mode: OutputMode,
) -> Result<ScopeChoice> {
    let items = [
        format!(
            "{:<19} e.g. {}",
            "Project root",
            compact_path(&project_root.join(".agents/skills/jujuleaf"), home)
        ),
        format!(
            "{:<19} e.g. {}",
            "Current directory",
            compact_path(&current_dir.join(".agents/skills/jujuleaf"), home)
        ),
        format!(
            "{:<19} e.g. {}",
            "User-wide",
            compact_path(&home.join(".agents/skills/jujuleaf"), home)
        ),
        format!("{:<19} <chosen-directory>/jujuleaf", "Custom directory"),
    ];
    let theme = prompt_theme(mode);
    let selected = Select::with_theme(theme.as_ref())
        .with_prompt("Installation scope  ↑↓ move · Enter confirm · Esc cancel")
        .items(items)
        .default(0)
        .interact_opt()
        .context("failed to read installation scope")?
        .ok_or_else(|| anyhow!("cancelled"))?;
    match selected {
        0 => Ok(ScopeChoice::Native(SkillScope::Project)),
        1 => Ok(ScopeChoice::Native(SkillScope::Current)),
        2 => Ok(ScopeChoice::Native(SkillScope::User)),
        3 => Ok(ScopeChoice::Custom),
        _ => unreachable!("dialoguer returned an invalid scope index"),
    }
}

fn prompt_custom_root(current_dir: &Path, mode: OutputMode) -> Result<PathBuf> {
    let theme = prompt_theme(mode);
    let value = Input::<String>::with_theme(theme.as_ref())
        .with_prompt("Custom skills directory")
        .interact_text()
        .context("failed to read custom skills directory")?;
    ensure!(!value.is_empty(), "custom skills directory cannot be empty");
    absolute_path(Path::new(&value), current_dir)
}

fn prompt_installations(installations: &[Installation], mode: OutputMode) -> Result<Vec<usize>> {
    let items = installations
        .iter()
        .map(|installation| {
            format!(
                "{:<10} {}",
                installation.scope.as_str(),
                installation.target_dir.display()
            )
        })
        .collect::<Vec<_>>();
    let defaults = vec![true; items.len()];
    loop {
        let theme = prompt_theme(mode);
        let selected = MultiSelect::with_theme(theme.as_ref())
            .with_prompt("Managed installations  ↑↓ move · Space toggle · Enter confirm")
            .items(&items)
            .defaults(&defaults)
            .interact_opt()
            .context("failed to read installation selection")?
            .ok_or_else(|| anyhow!("cancelled"))?;
        if !selected.is_empty() {
            return Ok(selected);
        }
        eprintln!("Select at least one installation.");
    }
}

fn confirm_mutation(question: &str, yes: bool, mode: OutputMode) -> Result<()> {
    if yes {
        return Ok(());
    }
    ensure!(
        interactive_terminal(mode),
        "confirmation requires a terminal; pass --yes"
    );
    let confirmed = prompt_confirmation(question, mode)?.unwrap_or(false);
    ensure!(confirmed, "cancelled");
    Ok(())
}

fn prompt_confirmation(question: &str, mode: OutputMode) -> Result<Option<bool>> {
    let term = Term::stderr();
    let color = mode_color(mode);
    let mut selected = false;

    term.hide_cursor()
        .context("failed to hide the terminal cursor")?;
    let interaction = (|| -> Result<Option<bool>> {
        render_confirmation(&term, question, selected, color)?;
        loop {
            match term
                .read_key()
                .context("failed to read confirmation input")?
            {
                Key::ArrowLeft => selected = false,
                Key::ArrowRight => selected = true,
                Key::Char(' ') | Key::Tab => selected = !selected,
                Key::Char('y') | Key::Char('Y') => return Ok(Some(true)),
                Key::Char('n') | Key::Char('N') => return Ok(Some(false)),
                Key::Enter => return Ok(Some(selected)),
                Key::Escape | Key::Char('q') | Key::CtrlC => return Ok(None),
                _ => continue,
            }
            render_confirmation(&term, question, selected, color)?;
        }
    })();
    let restore = term.show_cursor();
    let answer = interaction?;
    restore.context("failed to restore the terminal cursor")?;
    term.clear_line()
        .context("failed to clear the confirmation prompt")?;
    if let Some(answer) = answer {
        term.write_line(&completed_confirmation(question, answer, color))
            .context("failed to render the confirmation result")?;
    }
    Ok(answer)
}

fn render_confirmation(term: &Term, question: &str, selected: bool, color: bool) -> Result<()> {
    term.clear_line()?;
    term.write_str(&confirmation_line(question, selected, color))?;
    term.flush()?;
    Ok(())
}

fn confirmation_line(question: &str, selected: bool, color: bool) -> String {
    let active = Style::new().bold().fg_color(Some(AnsiColor::Cyan.into()));
    let no = if selected {
        "No".to_owned()
    } else {
        paint("[ No ]", active, color)
    };
    let yes = if selected {
        paint("[ Yes ]", active, color)
    } else {
        "Yes".to_owned()
    };
    format!(
        "{} {question}  {no}   {yes}    ←/→ choose · y/n · Enter confirm · Esc cancel",
        paint(
            "?",
            Style::new().fg_color(Some(AnsiColor::Yellow.into())),
            color
        )
    )
}

fn completed_confirmation(question: &str, answer: bool, color: bool) -> String {
    format!(
        "{} {question} · {}",
        paint(
            "✔",
            Style::new().fg_color(Some(AnsiColor::Green.into())),
            color
        ),
        paint(
            if answer { "yes" } else { "no" },
            Style::new().fg_color(Some(AnsiColor::Cyan.into())),
            color
        )
    )
}

fn prompt_theme(mode: OutputMode) -> Box<dyn Theme> {
    if mode_color(mode) {
        Box::new(ColorfulTheme::default())
    } else {
        Box::new(SimpleTheme)
    }
}

fn print_install_plan(plans: &[PlannedInstall], mode: OutputMode) {
    if !matches!(mode, OutputMode::Human { .. }) {
        return;
    }
    let color = mode_color(mode);
    eprintln!();
    eprintln!("{}", paint("Installation plan", Style::new().bold(), color));
    for plan in plans {
        eprintln!(
            "  {} {:<10} {}",
            paint(
                match plan.action {
                    "unchanged" => "·",
                    "updated" | "registered" => "↻",
                    _ => "+",
                },
                Style::new().fg_color(Some(AnsiColor::Green.into())),
                color
            ),
            plan.action,
            plan.destination.target_dir.display()
        );
        eprintln!(
            "      agents: {}",
            plan.agents
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

fn print_mutation_plan(verb: &str, registry: &Registry, indices: &[usize], mode: OutputMode) {
    if !matches!(mode, OutputMode::Human { .. }) || indices.is_empty() {
        return;
    }
    let color = mode_color(mode);
    eprintln!();
    eprintln!(
        "{}",
        paint(&format!("{verb} plan"), Style::new().bold(), color)
    );
    for index in indices {
        eprintln!(
            "  • {}",
            registry.installations[*index].target_dir.display()
        );
    }
}

fn mode_color(mode: OutputMode) -> bool {
    matches!(mode, OutputMode::Human { color: true })
}

fn paint(text: &str, style: Style, color: bool) -> String {
    if color {
        format!("{style}{text}{style:#}")
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_skill_is_valid_and_small() {
        let manifest = validate_embedded_bundle().unwrap();
        assert_eq!(manifest.skill_version, "0.1.0");
        assert!(EMBEDDED_SKILL.len() + EMBEDDED_MANIFEST.len() < 64 * 1024);
    }

    #[test]
    fn codex_render_adds_interface_metadata() {
        let manifest = embedded_manifest().unwrap();
        let portable = render_for_agents(&[SkillAgent::Portable], &manifest);
        let codex = render_for_agents(&[SkillAgent::Codex], &manifest);
        assert_eq!(portable.len(), 2);
        assert_eq!(codex.len(), 3);
        assert!(
            codex
                .iter()
                .any(|file| file.relative_path == Path::new("agents/openai.yaml"))
        );
        let openai = codex
            .iter()
            .find(|file| file.relative_path == Path::new("agents/openai.yaml"))
            .unwrap();
        let openai = String::from_utf8_lossy(&openai.content);
        assert!(openai.contains("$jujuleaf"));
        assert!(openai.contains("allow_implicit_invocation: true"));
    }

    #[test]
    fn native_paths_are_agent_specific() {
        let base = Path::new("/project");
        let home = Path::new("/home/person");
        assert_eq!(
            native_skills_root(SkillAgent::Codex, SkillScope::Project, base, home),
            Path::new("/project/.agents/skills")
        );
        assert_eq!(
            native_skills_root(SkillAgent::ClaudeCode, SkillScope::Project, base, home),
            Path::new("/project/.claude/skills")
        );
        assert_eq!(
            native_skills_root(SkillAgent::KimiCode, SkillScope::User, base, home),
            Path::new("/home/person/.kimi-code/skills")
        );
    }

    #[test]
    fn modified_installation_is_detected() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("skills/jujuleaf");
        let manifest = embedded_manifest().unwrap();
        let artifacts = render_for_agents(&[SkillAgent::Portable], &manifest);
        write_artifacts(&target, &artifacts, true).unwrap();
        let plan = PlannedInstall {
            destination: Destination {
                target_dir: target.clone(),
                scope: InstalledScope::Custom,
                agents: vec![SkillAgent::Portable],
            },
            agents: vec![SkillAgent::Portable],
            artifacts,
            previous: None,
            action: "installed",
        };
        let installation = installation_from_plan(&plan, &manifest);
        assert_eq!(
            installation_state(&installation, &manifest).unwrap(),
            InstallationState::Current
        );
        fs::write(target.join("SKILL.md"), "changed").unwrap();
        assert_eq!(
            installation_state(&installation, &manifest).unwrap(),
            InstallationState::Modified
        );
    }

    #[test]
    fn future_generated_files_do_not_overwrite_unmanaged_content() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("skills/jujuleaf");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SKILL.md"), EMBEDDED_SKILL).unwrap();
        fs::write(target.join("manifest.json"), "user-owned").unwrap();
        let installation = Installation {
            target_dir: target,
            scope: InstalledScope::Custom,
            agents: vec![SkillAgent::Portable],
            skill_version: "older".to_owned(),
            source_digest: "older".to_owned(),
            renderer_version: 0,
            files: vec![ManagedFile {
                path: PathBuf::from("SKILL.md"),
                digest: bytes_digest(EMBEDDED_SKILL.as_bytes()),
            }],
            installed_at: timestamp(),
            updated_at: timestamp(),
        };
        let manifest = embedded_manifest().unwrap();
        let artifacts = render_for_agents(&installation.agents, &manifest);
        assert!(preflight_new_artifacts(&installation, &artifacts, false).is_err());
        assert!(preflight_new_artifacts(&installation, &artifacts, true).is_ok());
    }
}
