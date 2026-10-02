<p align="center">
  <img src="assets/jujuleaf-logo.png" alt="JujuLeaf logo" width="176">
</p>

<h1 align="center">JujuLeaf</h1>

<p align="center">
  Work on Overleaf projects locally, with safe synchronization and native Jujutsu history.
</p>

<p align="center">
  <a href="README.zh.md">简体中文</a>
  · <a href="https://github.com/CsomePro/jujuleaf/releases">Releases</a>
  · <a href="docs/architecture.zh-CN.md">Architecture</a>
  · <a href="skill/SKILL.md">Agent skill</a>
</p>

<p align="center">
  <a href="https://github.com/CsomePro/jujuleaf/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/CsomePro/jujuleaf/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/CsomePro/jujuleaf/releases"><img alt="GitHub release" src="https://img.shields.io/github/v/release/CsomePro/jujuleaf"></a>
  <a href="https://crates.io/crates/jujuleaf"><img alt="crates.io" src="https://img.shields.io/crates/v/jujuleaf.svg"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/github/license/CsomePro/jujuleaf"></a>
</p>

JujuLeaf brings an Overleaf project into a real local working directory. Use any
editor or coding agent, keep recoverable local checkpoints with the embedded
Jujutsu engine, and synchronize changes deliberately. JujuLeaf translates text
edits back into Overleaf collaboration events instead of replacing whole files.

> [!IMPORTANT]
> JujuLeaf is pre-1.0 software built on private Overleaf APIs. Start with a
> non-critical project or keep a backup. v0.4.1 is the first stable
> cross-platform release, but private API behavior can still vary by instance.

## Why JujuLeaf?

- **Your editor, your tools.** Work in Neovim, VS Code, Zed, an IDE, or with a
  coding agent while collaborators remain in Overleaf.
- **Safe synchronization.** Local and remote changes are compared against a
  confirmed baseline; JujuLeaf stops on conflicts instead of silently
  overwriting work.
- **Built-in local history.** Checkpoint, undo, and redo through an embedded
  Jujutsu repository. A separate jj executable is not required.
- **Native collaboration.** Exact edits, tracked changes, comments, compilation,
  PDFs, project history, and live events use Overleaf collaboration features.
- **Human and machine friendly.** The default output is readable and colorful;
  compact JSON is available for scripts, while `jujuleaf bridge` exposes a
  stable, versioned process protocol for long-lived integrations.

## Install

### cargo-binstall (recommended)

With [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) installed,
install the current stable release:

~~~bash
cargo binstall --strategies crate-meta-data jujuleaf
~~~

Use `--force` to reinstall or upgrade an existing JujuLeaf installation. For
unattended environments, add `--no-confirm`:

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf
jujuleaf --version
~~~

cargo-binstall automatically selects the matching archive:

| Platform | Architecture | Rust target | Package |
| --- | --- | --- | --- |
| Linux | AMD64 | `x86_64-unknown-linux-musl` | Fully static `.tar.gz` |
| Linux | ARM64 | `aarch64-unknown-linux-musl` | Fully static `.tar.gz` |
| macOS | Apple Silicon | `aarch64-apple-darwin` | Single-binary `.tar.gz` |
| macOS | Intel | `x86_64-apple-darwin` | Single-binary `.tar.gz` |
| Windows | AMD64 | `x86_64-pc-windows-msvc` | Static-CRT `.zip` |

macOS binaries use Apple system libraries, and the Windows executable uses
Windows system libraries. No separate `jj`, Git, OpenSSL, or SQLite installation
is required.

### Manual download

Download the archive for your target and `SHA256SUMS` from the
[v0.4.1 release](https://github.com/CsomePro/jujuleaf/releases/tag/v0.4.1).
Linux and macOS archives contain `jujuleaf`; the Windows ZIP contains
`jujuleaf.exe`. Put that executable in a directory on your `PATH`.

### Build from source

JujuLeaf requires Rust 1.92 or newer:

~~~bash
cargo install --locked jujuleaf
~~~

## Install the Agent Skill

The executable contains a versioned, self-describing Agent Skill bundle. Run the
guided terminal installer to detect local Agent commands and configuration
directories, choose a project, current-directory, user, or custom installation,
and then select one or more targets:

~~~bash
jujuleaf skill install
~~~

Use ↑/↓ to move, Space to toggle targets, Enter to confirm, and Esc to cancel.
For the final confirmation, use ←/→ to choose, Enter to accept, or `y`/`n`
directly.

JujuLeaf supports Codex, Claude Code, Kimi Code CLI, Pi, Gemini CLI, GitHub
Copilot, Cursor, OpenCode, and the portable `.agents/skills` convention. It
writes each Agent's native directory, while keeping one standard `SKILL.md`
source. Codex also receives its optional `agents/openai.yaml` interface file.

Every choice is available as an argument for scripts and unattended setup:

~~~bash
# Install for two Agents in the nearest project root.
jujuleaf skill install --agent codex,claude-code --scope project --yes

# Install in native Agent directories under the current directory.
jujuleaf skill install --agent kimi-code --scope current --yes

# Treat another directory as a shared skills root.
jujuleaf skill install --agent portable --to ./agent-skills --yes
~~~

Inspect detected Agents and manage every registered installation:

~~~bash
jujuleaf skill detect
jujuleaf skill status
jujuleaf skill update
jujuleaf skill uninstall
~~~

Updates compare file digests before writing. JujuLeaf refuses to overwrite local
changes unless `--force` is supplied, in which case it creates a timestamped
backup first. Use `--raw`, explicit target arguments, `--yes`, and optionally
`--dry-run` for non-interactive automation.

## Quick start

Sign in, find a project, and clone it into a new or empty directory:

~~~bash
jujuleaf login --preset overleaf
jujuleaf projects
jujuleaf clone PROJECT_ID paper
cd paper
jujuleaf status
jujuleaf
~~~

The login command opens Chrome or Chromium and saves the authenticated session
locally. After cloning, JujuLeaf records the project ID and profile in the
workspace. From that directory or any child directory, commands automatically
use the current project:

~~~bash
jujuleaf files
jujuleaf read main.tex --content-only
jujuleaf compile
jujuleaf compile --log-output output.log
~~~

Overleaf's compile endpoint returns only after the server-side build finishes.
JujuLeaf then downloads `output.log` and parses its diagnostics. The default
command does not create a local log file; use `--log-output PATH` to save one
or `--show-log` to include it in the command output.

Edit files with your usual tools, record a local checkpoint, then synchronize:

~~~bash
jujuleaf checkpoint -m "rewrite introduction"
jujuleaf sync
~~~

## Choose a workflow

### Direct synchronization

Use this for changes that should appear in Overleaf as ordinary edits:

~~~bash
jujuleaf sync
# Edit files locally.
jujuleaf status
jujuleaf checkpoint -m "update experiment results"
jujuleaf sync
~~~

A one-shot sync pulls remote changes and then pushes safe local changes.
Use jujuleaf sync --watch to keep synchronizing in the foreground.

### Review with tracked changes

Use this when collaborators should accept or reject your changes in Overleaf:

~~~bash
jujuleaf sync
jujuleaf begin -m "rewrite the introduction"
# Edit files locally.
jujuleaf review diff
jujuleaf review submit
jujuleaf review status
~~~

After collaborators resolve the tracked changes in Overleaf:

~~~bash
jujuleaf review finish
~~~

JujuLeaf keeps the review anchored to its synchronized baseline and reports
foreign or partially resolved changes before closing it. A successful finish
keeps the reconciled review as a stable parent change and leaves a fresh empty
working-copy change for the next edit. The next `begin` reuses that empty
change; it does not create another workspace or an extra empty layer.

New reviews use `adaptive` granularity: an isolated edit such as adding the
`s` in `model` → `models` remains a one-character tracked change, while a
dense rewrite inside one word or phrase is presented as one review hunk.
Ordinary `sync` and `push` always keep their exact character-level transport
diff so comment anchors and concurrent edits are not widened.

Choose a different review policy when starting the work:

~~~bash
jujuleaf begin -m "literal corrections" --granularity exact
jujuleaf begin -m "terminology cleanup" --granularity word
jujuleaf begin -m "rewrite prose" --granularity sentence
~~~

The selected policy and diff algorithm are stored with the review. `review diff`
reports both review hunks and underlying atomic edits, and interrupted
submissions verify the persisted plan before retrying.

## Everyday commands

| Goal | Command |
| --- | --- |
| List projects | <code>jujuleaf projects</code> |
| Inspect project files | <code>jujuleaf files</code> |
| Read a document | <code>jujuleaf read main.tex --content-only</code> |
| Search a project snapshot | <code>jujuleaf search "bibliography"</code> |
| Replace exact text | <code>jujuleaf replace main.tex --old "draft" --new "final"</code> |
| Submit one tracked edit | <code>jujuleaf suggest main.tex --old "draft" --new "final"</code> |
| View comment threads | <code>jujuleaf threads</code> |
| Integrate an external worker | <code>jujuleaf bridge describe</code>, <code>jujuleaf bridge comments watch</code> |
| Install or update the Agent Skill | <code>jujuleaf skill install</code>, <code>jujuleaf skill update</code> |
| Compile and inspect diagnostics | <code>jujuleaf compile</code> |
| Download the compiled PDF | <code>jujuleaf pdf -o paper.pdf</code> |
| Compare local and remote state | <code>jujuleaf status</code> |
| Pull, push, or synchronize | <code>jujuleaf pull</code>, <code>jujuleaf push</code>, <code>jujuleaf sync</code> |
| Inspect and resolve conflicts | <code>jujuleaf conflict list</code>, <code>jujuleaf conflict show PATH</code>, <code>jujuleaf conflict resolve PATH</code> |
| Show the current change graph | <code>jujuleaf log</code> or <code>jujuleaf</code> |
| Inspect the operation journal | <code>jujuleaf op log</code>, <code>jujuleaf op show REVISION</code>, <code>jujuleaf op diff</code> |
| Share through Git | <code>jujuleaf git remote add origin URL</code>, <code>jujuleaf git fetch</code>, <code>jujuleaf git push</code> |
| Check setup and authentication | <code>jujuleaf doctor</code>, <code>jujuleaf auth status</code> |
| Restore local history | <code>jujuleaf undo</code>, <code>jujuleaf redo</code> |

Run jujuleaf --help or jujuleaf COMMAND --help for the complete command list and
examples.

## Safer targeted edits

Exact-text commands reject ambiguous matches by default. Locate the text first,
preview the collaboration operations, and then apply the edit:

~~~bash
jujuleaf locate main.tex --text "Related work"
jujuleaf replace main.tex \
  --old "Related work" \
  --new "Background and related work" \
  --dry-run
jujuleaf replace main.tex \
  --old "Related work" \
  --new "Background and related work"
~~~

For repeated text, select a match with --occurrence or --position, or explicitly
apply the edit to every match with --all. Positions use CodeMirror UTF-16 units,
matching Overleaf.

## Profiles and self-hosted instances

Named profiles keep accounts and endpoints separate:

~~~bash
jujuleaf login --profile official --preset overleaf
jujuleaf login --profile lab --base-url https://overleaf.example.org
jujuleaf login --profile cstcloud --preset cstcloud
jujuleaf profile list
jujuleaf profile use official
jujuleaf --profile lab projects
~~~

A clone remembers the profile that created it, so normal commands do not need a
repeated --profile option. Authentication data stays in the user configuration
directory and is never written into the project.

## Output for people and agents

Human-readable output is the default. Colors are enabled only for an interactive
terminal.

~~~bash
jujuleaf status
jujuleaf status --no-color
NO_COLOR=1 jujuleaf status
jujuleaf projects --raw
jujuleaf projects --pretty
~~~

Both --raw and --pretty produce ANSI-free JSON, so an additional --no-color is
not needed. Agents can use the bundled [SKILL.md](skill/SKILL.md) as a compact
command guide.

### Stable process integration

`--raw` follows JujuLeaf's internal command output and may grow with the CLI.
External workers that need a compatibility contract should use the dedicated
bridge instead:

~~~bash
jujuleaf bridge describe
jujuleaf bridge comments list --protocol 1
jujuleaf bridge comments get THREAD_ID --protocol 1
jujuleaf bridge comments watch --protocol 1
~~~

Bridge commands always emit compact, ANSI-free JSON. `comments.watch` emits
NDJSON and starts with an authoritative comment snapshot before live events.
It periodically emits another full snapshot, reconnects automatically, and
uses events only as wake-up notifications. A worker should replace its known
state on `comments.snapshot` and fetch `comments.get` after a `comment.event`.

The bridge normalizes thread messages, multi-range anchors, detached anchors,
document paths, UTF-16 source and visible ranges, and content hashes. It does
not expose Overleaf's private event names or history-OT payloads. Every record
contains protocol identity and version; failures use stable error codes and a
non-zero process exit status. See the [bridge protocol](docs/bridge-protocol.md)
for the complete contract and stream lifecycle.

## Synchronization safety

For each document or uploaded file, JujuLeaf compares the local copy and the
current Overleaf copy with the last confirmed checkpoint:

| Local state | Remote state | Result |
| --- | --- | --- |
| Unchanged | Changed | Pull the remote version |
| Changed | Unchanged | Push the local version |
| Changed | Changed | Stop and report a conflict |
| Unchanged | Unchanged | Do nothing |

Remote versions and content hashes are checked again before updates are sent and
after confirmation. New files and destructive operations are handled
explicitly; conflicting remote content is preserved for manual recovery.

### Resolve synchronization conflicts

Pull and push record unresolved conflicts without overwriting the working copy.
Inspect both sides, merge if needed, and make the choice explicit:

~~~bash
jujuleaf conflict list
jujuleaf conflict show main.tex
jujuleaf conflict resolve main.tex --ours
jujuleaf conflict resolve main.tex --theirs
jujuleaf conflict resolve main.tex --merged ./main.merged.tex
~~~

`--ours` keeps the local file, `--theirs` accepts the preserved remote copy,
and `--merged` installs a file you prepared. Resolution creates a recoverable
Jujutsu checkpoint and advances the observed remote baseline; the next push
still verifies the live remote state before writing.

### Inspect changes and the operation journal

The change graph contains durable work boundaries such as completed reviews:

~~~bash
jujuleaf log
~~~

The operation journal contains lightweight checkpoints and recovery points:

~~~bash
jujuleaf op log
jujuleaf op show OPERATION_ID
jujuleaf op diff
jujuleaf op restore OPERATION_ID
~~~

`op show` accepts an operation or commit ID prefix, or `@` for the current
operation. `op restore` restores that tree as a new operation, so the restore
itself can be undone and does not erase later history. The previous `local`
commands remain accepted as compatibility aliases. JujuLeaf embeds jj-lib;
installing or invoking the separate `jj` executable is not required.

Inside a clone, running `jujuleaf` without a subcommand snapshots pending
working-copy changes and displays a compact, colored first-parent commit graph,
similar to `jujuleaf log` and the default `jj` view. It shows up to 10 changes;
use `op log` for operation history and `jujuleaf log --raw` for full structured
commit data.

The compact graph uses jj-style 8-character display IDs. Elsewhere,
human-readable output abbreviates Jujutsu operation, commit, and change IDs to
a 12-character prefix. These prefixes can be passed back to `op show` and
`op restore`; if one is ever ambiguous, JujuLeaf asks for a longer prefix.
`--raw` and `--pretty` JSON always retain the complete IDs.

### Share the same history through Git

Every JujuLeaf clone is already backed by a bare Git repository inside `.jj`.
Configure and use it through JujuLeaf without creating a second `.git` working
tree:

~~~bash
jujuleaf git root
jujuleaf git remote add origin git@github.com:OWNER/REPOSITORY.git
jujuleaf git push --branch main
jujuleaf git fetch --remote origin
jujuleaf git remote list
~~~

`git push` first snapshots every pending file, keeps that exact state as a
stable parent change, moves the same workspace to a fresh blank child, and
publishes the frozen parent under the selected branch (default: `main`). Later
edits therefore continue in the child without rewriting the Git backup. It uses
the last fetched remote position as a lease, so an unexpected remote update is
rejected instead of overwritten; the frozen local state remains recoverable
even when the network push fails. Run `git fetch` first when updating an
existing branch. Fetch imports remote branches and tags into Jujutsu history;
it does not replace or automatically merge the working copy yet.

Remote configuration also supports `remote remove`, `remote rename`, and
`remote set-url`. Use `-R PATH` when running outside the clone. Create the
workspace with `jujuleaf clone`, not `jujuleaf git clone` or `git init`. A
separate `jj` executable is unnecessary; network fetch and push use the system
Git executable and its normal SSH or credential-helper configuration.

### Ignore local-only files

Create `.jujuleafignore` at the clone root to prevent matching generated or
private files from being discovered for upload or first tracked by Jujutsu. It
uses gitignore syntax:

~~~gitignore
/build/
*.aux
*.log
.env
~~~

The rule does not untrack a file that was already checkpointed or an Overleaf
entity that is already synchronized.
`.jj`, `.git`, `.jujuleaf`, and `.jujuleafignore` are always private and are
never considered for upload.

## Diagnostics and sign-out

~~~bash
jujuleaf auth status
jujuleaf doctor
jujuleaf doctor --offline
jujuleaf auth logout
~~~

`doctor` checks the selected profile, credential permissions, browser,
endpoint, project binding, Jujutsu workspace, and pending sync state. The
offline form skips the network check. `auth status` never prints the cookie;
`auth logout` removes the selected local profile and its credentials.

## Current limitations

- Release binaries currently cover Linux AMD64/ARM64, macOS Apple
  Silicon/Intel, and Windows AMD64.
- Browser login requires Chrome, Chromium, or Edge, unless an existing session
  cookie is supplied.
- Synchronization is foreground-only; there is no background service.
- Overleaf private APIs may change without notice, especially on self-hosted
  installations.

## Development and documentation

~~~bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --locked
~~~

See the [architecture document](docs/architecture.zh-CN.md) for protocol,
storage, conflict, and review details.

## Acknowledgements

JujuLeaf is inspired by
[overleaf-cli](https://github.com/dylantmoore/overleaf-cli) and builds on ideas
from the wider Overleaf command-line community.

JujuLeaf is an independent, unofficial project and is not affiliated with or
endorsed by Overleaf or the Jujutsu project.

## License

[MIT](LICENSE)
