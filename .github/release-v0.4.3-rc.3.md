<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.4.3-rc.3/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.4.3-rc.3</h1>

<p align="center"><strong>Reliable semantic Review ranges for repeated text</strong></p>

> This prerelease fixes a semantic-v4 Review planner failure discovered with
> multi-paragraph and list rewrites. Existing Review drafts are supported.

## Fixed: reversed UTF-16 Review ranges

Some repeated-text replacements could make the diff compactor move an insertion
inside a preceding deletion. JujuLeaf treated those operations as ordered,
non-overlapping atoms and later failed with an error such as:

~~~text
invalid UTF-16 range 4676..4673
~~~

JujuLeaf now validates that compact diff operations form complete,
non-overlapping partitions of both the old and new text. If compaction produces
crossing ranges, the planner safely falls back to the unshifted diff. A final
coarse replacement fallback prevents malformed dependency output from reaching
the UTF-16 planner.

The protection applies to:

- semantic-v4 grapheme planning for exact, word, sentence, and adaptive modes;
- LaTeX structural block alignment used for larger rewrites;
- Myers, Patience, and LCS diff algorithms.

Ordinary compact output is preserved whenever its ranges are well formed, so
small edits and readable semantic grouping keep their existing behavior.

## Validation

- Reproduced and fixed the preserved real-world Review draft without submitting
  or changing remote state.
- Added a content-independent minimal regression: `aab` to `bbb`.
- Added repeated-text, combining-character, emoji, and all-granularity coverage.
- Audited every compact slice-diff caller for the same range assumption.
- 144 unit and integration tests.
- Strict Clippy with warnings denied, formatting, and Git diff checks.

No user document text or project identifiers are included in the test suite.

## Install or upgrade

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf@0.4.3-rc.3
jujuleaf --version
~~~

The reported version should be `jujuleaf 0.4.3-rc.3`.

## Release targets

- Linux AMD64 and ARM64: fully static musl binaries
- macOS Apple Silicon and Intel: single binaries using Apple system libraries
- Windows AMD64: single executable with the static MSVC runtime

- [README](https://github.com/CsomePro/jujuleaf/blob/v0.4.3-rc.3/README.md)
- [Chinese README](https://github.com/CsomePro/jujuleaf/blob/v0.4.3-rc.3/README.zh.md)
- [Changes since v0.4.3-rc.2](https://github.com/CsomePro/jujuleaf/compare/v0.4.3-rc.2...v0.4.3-rc.3)
