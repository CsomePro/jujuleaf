<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.4.2-rc.2/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.4.2-rc.2</h1>

<p align="center"><strong>LaTeX-aware reviews and focused comment-driven collaboration</strong></p>

> This is a cross-platform prerelease for validating semantic review blocks and
> the new comment workflow. Keep a backup when testing it with an important
> project.

## Reviews that understand LaTeX structure

- The adaptive review planner now scans LaTeX into semantic blocks before
  aligning changes.
- Prose follows sentence boundaries, while multiline and nested list items stay
  together.
- Figures separate images, captions, and subfigures while keeping labels with
  their owning component.
- Tables, display math, code listings, and TikZ/PGF drawings are protected as
  atomic review blocks.
- Unknown commands, custom environments, and malformed structures fall back to
  the conservative planner instead of guessing.
- Small isolated edits remain precise: `model` → `models` still marks only the
  added `s`.

Ordinary `sync` and `push` continue to use exact character-level transport
diffs; semantic grouping affects only review presentation.

## A low-noise comment inbox

~~~bash
jujuleaf comments inbox
jujuleaf comments get THREAD_ID
jujuleaf comments reply THREAD_ID "Updated: clarified the comparison."
~~~

- `comments inbox` keeps open comment threads with real messages and filters
  resolved, empty, and tracked-change-only review entries.
- Normal commands return normalized messages, anchors, paths, bounded context,
  and document snapshots; use `--raw` for agent-friendly JSON.
- A clone supplies its project and profile automatically. Outside a clone, use
  `--project-id` and, when needed, `--profile`.
- An open thread can receive multiple replies. Each reply is limited to 300
  Unicode characters and does not resolve the thread automatically.

Stable integrations receive the same capabilities through Bridge protocol v1:

~~~bash
jujuleaf bridge comments inbox --protocol 1
jujuleaf bridge comments reply THREAD_ID \
  --message "Updated the wording." --protocol 1
~~~

## Intent-aware Agent replies

The embedded Agent Skill now inspects each thread before acting:

- change requests are applied and verified before a concise reply;
- questions and explanation requests are answered without inventing edits;
- informational notes do not force document changes;
- detached or ambiguous location-dependent requests stop for clarification;
- threads remain open for human review unless the user explicitly asks to
  resolve them.

The Skill recommends replies of at most 120 characters even though the command
hard limit is 300.

## Install or upgrade

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf@0.4.2-rc.2
jujuleaf --version
~~~

The reported version should be `jujuleaf 0.4.2-rc.2`.

## Release targets

- Linux AMD64 and ARM64: fully static musl binaries
- macOS Apple Silicon and Intel: single binaries using Apple system libraries
- Windows AMD64: single executable with the static MSVC runtime

- [README](https://github.com/CsomePro/jujuleaf/blob/v0.4.2-rc.2/README.md)
- [中文 README](https://github.com/CsomePro/jujuleaf/blob/v0.4.2-rc.2/README.zh.md)
- [Changes since v0.4.2-rc.1](https://github.com/CsomePro/jujuleaf/compare/v0.4.2-rc.1...v0.4.2-rc.2)
