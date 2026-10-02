<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.4.1/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.4.1</h1>

<p align="center"><strong>Cross-platform local-first Overleaf collaboration, with reviews that read like reviews</strong></p>

> JujuLeaf is still pre-1.0 and uses private Overleaf APIs. Keep a backup when
> adopting it for an important project.

## Better tracked-change reviews

- New review work uses adaptive granularity by default. A single edit remains
  exact—adding the `s` in `model` → `models` is still a one-character change.
- Fragmented edits inside a word and dense phrase rewrites are combined into
  readable review hunks without crossing LaTeX commands, braces, math,
  comments, line breaks, or sentence boundaries.
- `jujuleaf begin --granularity exact|adaptive|word|sentence` makes the policy
  explicit. Existing active reviews created by an older version continue in
  exact mode.
- `jujuleaf review diff` now reports semantic hunks, underlying atomic edits,
  line/column positions, and compact change previews.
- Review state persists the granularity, algorithm version, and per-document
  plan hash so an interrupted submission cannot silently retry with a
  different tracked-change layout.
- Ordinary `sync` and `push` remain exact character-level operations to protect
  comment anchors and concurrent edits.

## Stable cross-platform packages

- Linux AMD64 and ARM64: fully static musl binaries with no glibc dependency.
- macOS Apple Silicon and Intel: single binaries using Apple system libraries.
- Windows AMD64: one executable using the static MSVC runtime.
- Every GitHub release includes five platform archives plus `SHA256SUMS`;
  cargo-binstall selects the matching package automatically.

## Other highlights since v0.1.3

- Embedded Jujutsu history, human-readable colored output, optional exact JSON,
  project discovery, conflict-safe synchronization, and Git backup remotes.
- A stable `jujuleaf bridge` protocol for comment events and thread context.
- An embedded, versioned Agent Skill with interactive installation, detection,
  scope selection, update checks, and support for major coding agents.
- Reliable CSTCloud compilation through a scoped HTTP/1.0 compatibility path,
  plus structured Underfull and Overfull TeX diagnostics.

## Install or upgrade

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf
jujuleaf --version
~~~

The reported version should be `jujuleaf 0.4.1`.

- [README](https://github.com/CsomePro/jujuleaf/blob/v0.4.1/README.md)
- [中文 README](https://github.com/CsomePro/jujuleaf/blob/v0.4.1/README.zh.md)
- [Changes since v0.1.3](https://github.com/CsomePro/jujuleaf/compare/v0.1.3...v0.4.1)
- [Changes since v0.4.1-rc.3](https://github.com/CsomePro/jujuleaf/compare/v0.4.1-rc.3...v0.4.1)
