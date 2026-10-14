<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.4.2/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.4.2</h1>

<p align="center"><strong>Meaningful local history, LaTeX-aware reviews, and focused comment collaboration</strong></p>

> JujuLeaf is still pre-1.0 and uses private Overleaf APIs. Keep a backup when
> adopting it for an important project.

## Official Overleaf compatibility

- Cloning and reading projects on `www.overleaf.com` now omit absent optional
  `joinDoc` fields instead of sending values that the production validator
  rejects.
- Compile logs and PDFs use Overleaf's authenticated same-origin output route,
  while Community Edition and self-hosted instances keep their existing output
  URL behavior.
- Absolute cross-origin artifact downloads never receive the saved Overleaf
  session cookie.
- The fixes were exercised against the official service with clone, pull, read,
  compile, PDF download, comments, history, search, word count, and live event
  connections.

## Meaningful Jujutsu history

- A successful `review finish` freezes the reconciled result as a stable parent
  change and moves the same workspace to a fresh blank child.
- The next `begin` reuses that child instead of creating empty workspace
  layers.
- `jujuleaf log` shows semantic changes; `jujuleaf op log/show/diff/restore`
  exposes lightweight checkpoints and recovery operations separately.
- `jujuleaf git push` snapshots pending files, freezes the exact current state,
  and pushes that stable parent while later work continues in a child change.
- Interrupted finish and failed Git operations leave recoverable Jujutsu state.

## Reviews that understand LaTeX structure

- Adaptive review planning scans LaTeX into semantic blocks before aligning
  changes.
- Prose follows sentence boundaries, while multiline and nested list items stay
  together.
- Figures separate images, captions, and subfigures while keeping labels with
  their owning component.
- Tables, display math, code listings, and TikZ/PGF drawings remain atomic.
- Unknown commands, custom environments, and malformed structures fall back to
  the conservative planner.
- Small isolated edits remain precise: `model` → `models` still marks only
  the added `s`.

Ordinary `sync` and `push` continue to use exact character-level transport
diffs; semantic grouping affects only review presentation.

## A low-noise comment workflow

~~~bash
jujuleaf comments inbox
jujuleaf comments get THREAD_ID
jujuleaf comments reply THREAD_ID "Updated: clarified the comparison."
~~~

- `comments inbox` keeps open threads with real messages and filters resolved,
  empty, and tracked-change-only entries.
- Normal commands return normalized messages, anchors, paths, bounded context,
  and document snapshots; Bridge protocol v1 exposes the same capabilities to
  stable integrations.
- Change requests, questions, explanations, and informational notes are handled
  according to intent by the embedded Agent Skill.
- Threads can receive multiple replies. The command hard limit is 300 Unicode
  characters, while the Skill recommends concise replies of at most 120.

## Install or upgrade

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf
jujuleaf --version
~~~

The reported version should be `jujuleaf 0.4.2`.

## Release targets

- Linux AMD64 and ARM64: fully static musl binaries
- macOS Apple Silicon and Intel: single binaries using Apple system libraries
- Windows AMD64: single executable with the static MSVC runtime

- [README](https://github.com/CsomePro/jujuleaf/blob/v0.4.2/README.md)
- [中文 README](https://github.com/CsomePro/jujuleaf/blob/v0.4.2/README.zh.md)
- [Changes since v0.4.1](https://github.com/CsomePro/jujuleaf/compare/v0.4.1...v0.4.2)
- [Changes since v0.4.2-rc.2](https://github.com/CsomePro/jujuleaf/compare/v0.4.2-rc.2...v0.4.2)
