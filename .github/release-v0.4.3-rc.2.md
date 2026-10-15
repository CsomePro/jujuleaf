<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.4.3-rc.2/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.4.3-rc.2</h1>

<p align="center"><strong>Review changes at the scale people actually read</strong></p>

> This is a cross-platform prerelease for validating the new semantic review
> planner. Keep a backup when testing it with an important project.

## Smarter Review ranges

JujuLeaf now plans tracked changes with the versioned semantic-v4 algorithm:

- Start from Unicode grapheme-safe edits, so combining characters and emoji are
  never split in the middle.
- Consider word, sentence, and line candidates, then use a deterministic cost
  model to select the clearest overall plan.
- Keep isolated changes such as model to models as a one-character insertion.
- Combine dense sentence rewrites into a single readable change.
- Combine dense table-cell edits at row scope without replacing the whole
  table.
- Keep physical lines as default hard boundaries.
- Use LaTeX-aware structural alignment when a paragraph is reshaped into a
  list or another multi-line construct.

Existing in-progress reviews remain reproducible: semantic-v1, semantic-v2,
and semantic-v3 are still supported.

## Safer around Overleaf comments

Existing comment anchors are normalized for both history-OT and legacy
projects. They act as hard boundaries when JujuLeaf considers widening a
Review range, preventing an unrelated stable comment span from being swallowed
by a larger replacement.

This is deliberately conservative: editing the commented text itself still
uses Overleaf's normal OT behavior; JujuLeaf does not silently recreate or move
comment threads.

## Root document context fix

compile --root and pdf --root now correctly infer the project ID and profile
from the current JujuLeaf clone. An explicit project ID remains supported but
is no longer required inside a clone.

~~~bash
jujuleaf compile --root main-cn.tex
jujuleaf pdf --root appendix.tex -o appendix.pdf
~~~

## Validation

- 141 unit and integration tests
- Strict Clippy with warnings denied
- Formatting and Git diff checks
- Synthetic coverage for character, word, sentence, line, table-row, LaTeX
  structure, Unicode grapheme, and comment-anchor cases

No user document text or project identifiers are included in the test suite.

## Install or upgrade

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf@0.4.3-rc.2
jujuleaf --version
~~~

The reported version should be jujuleaf 0.4.3-rc.2.

## Release targets

- Linux AMD64 and ARM64: fully static musl binaries
- macOS Apple Silicon and Intel: single binaries using Apple system libraries
- Windows AMD64: single executable with the static MSVC runtime

- [README](https://github.com/CsomePro/jujuleaf/blob/v0.4.3-rc.2/README.md)
- [Chinese README](https://github.com/CsomePro/jujuleaf/blob/v0.4.3-rc.2/README.zh.md)
- [Changes since v0.4.3-rc.1](https://github.com/CsomePro/jujuleaf/compare/v0.4.3-rc.1...v0.4.3-rc.2)
