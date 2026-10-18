<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.4.3-rc.4/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.4.3-rc.4</h1>

<p align="center"><strong>Readable Review rewrites around Overleaf comments</strong></p>

> This prerelease keeps dense sentence rewrites readable when the source text
> contains Overleaf comment anchors, without sacrificing structural LaTeX
> boundaries or UTF-16 editing precision.

## Fixed: comment anchors no longer fragment sentence rewrites

Overleaf comments can cover an entire sentence or a small phrase inside it.
Previously, every comment boundary was treated as a mandatory Review boundary.
A substantial sentence rewrite could therefore appear as dozens of tiny
tracked ranges, even though it was one semantic edit.

JujuLeaf now lets the semantic planner group edits across comment anchors while
still respecting sentence, line, and existing LaTeX boundaries. Overleaf
rebases an affected comment to the replacement boundary, keeping it attached
to the rewritten sentence instead of losing it or moving it to the end of the
document.

The planner also distinguishes inline additions from block structure:

- a newly added inline `\ref{...}` can remain part of one readable sentence
  replacement;
- existing LaTeX commands remain protected merge boundaries;
- newly inserted lists, formulas, tables, comments, and multiline structures
  still use structural planning.

## Validation

- Tested full-sentence and inner-phrase comment anchors against an official
  Overleaf test project.
- Reduced a representative sentence rewrite from 44 visible Review fragments
  to one semantic hunk while retaining all 44 atomic UTF-16 edits.
- Verified that the remote comment remained attached at the rewritten
  sentence boundary.
- Added synthetic, content-independent regressions for comment anchors, inline
  references, existing LaTeX boundaries, and inserted block structures.
- 146 unit and integration tests.
- Strict Clippy with warnings denied, formatting, and Git diff checks.

No user document text or project identifiers are included in the test suite.

## Install or upgrade

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf@0.4.3-rc.4
jujuleaf --version
~~~

The reported version should be `jujuleaf 0.4.3-rc.4`.

## Release targets

- Linux AMD64 and ARM64: fully static musl binaries
- macOS Apple Silicon and Intel: single binaries using Apple system libraries
- Windows AMD64: single executable with the static MSVC runtime

