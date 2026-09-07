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
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/github/license/CsomePro/jujuleaf"></a>
</p>

JujuLeaf brings an Overleaf project into a real local working directory. Use any
editor or coding agent, keep recoverable local checkpoints with the embedded
Jujutsu engine, and synchronize changes deliberately. JujuLeaf translates text
edits back into Overleaf collaboration events instead of replacing whole files.

> [!IMPORTANT]
> JujuLeaf is pre-1.0 software built on private Overleaf APIs. Start with a
> non-critical project or keep a backup. Version 0.1.1 currently provides a
> prebuilt binary for Linux AMD64 only.

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
  compact or pretty JSON is available for scripts and agents.

## Install

### Linux AMD64 binary

Download the archive and checksum from the
[latest release](https://github.com/CsomePro/jujuleaf/releases/latest), or
install version 0.1.1 directly:

~~~bash
curl -LO https://github.com/CsomePro/jujuleaf/releases/download/v0.1.1/jujuleaf-v0.1.1-x86_64-unknown-linux-gnu.tar.gz
curl -LO https://github.com/CsomePro/jujuleaf/releases/download/v0.1.1/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf jujuleaf-v0.1.1-x86_64-unknown-linux-gnu.tar.gz
install -Dm755 jujuleaf-v0.1.1-x86_64-unknown-linux-gnu/jujuleaf ~/.local/bin/jujuleaf
jujuleaf --version
~~~

Make sure ~/.local/bin is on your PATH.

### Build from source

JujuLeaf requires Rust 1.92 or newer:

~~~bash
cargo install --git https://github.com/CsomePro/jujuleaf --tag v0.1.1
~~~

## Quick start

Sign in, find a project, and clone it into a new or empty directory:

~~~bash
jujuleaf login --preset overleaf
jujuleaf projects
jujuleaf clone PROJECT_ID paper
cd paper
jujuleaf status
~~~

The login command opens Chrome or Chromium and saves the authenticated session
locally. After cloning, JujuLeaf records the project ID and profile in the
workspace. From that directory or any child directory, commands automatically
use the current project:

~~~bash
jujuleaf files
jujuleaf read main.tex --content-only
jujuleaf compile
~~~

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
foreign or partially resolved changes before closing it.

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
| Compile and inspect diagnostics | <code>jujuleaf compile</code> |
| Download the compiled PDF | <code>jujuleaf pdf -o paper.pdf</code> |
| Compare local and remote state | <code>jujuleaf status</code> |
| Pull, push, or synchronize | <code>jujuleaf pull</code>, <code>jujuleaf push</code>, <code>jujuleaf sync</code> |
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

## Current limitations

- The ready-made release currently targets Linux AMD64. Other platforms can
  build from source.
- Browser login requires Chrome or Chromium, unless an existing session cookie
  is supplied.
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
